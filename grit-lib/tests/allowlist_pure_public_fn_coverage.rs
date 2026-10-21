//! Execute allowlisted public helpers that need little or no repository setup.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::io::{Cursor, Write};
use std::num::NonZeroUsize;
use std::path::Path;
use std::process::{Command, Stdio};

use grit_lib::blame::build_fuzzy_line_map;
use grit_lib::command_runner::hooks::{
    resolve_hooks_dir, run_hook_capture, run_hook_with_env,
    run_reference_transaction_committed_for_head_update,
};
use grit_lib::command_runner::{run_program, CommandStdin, CommandStdio, RecordingRunner};
use grit_lib::commit::signing::{GpgConfig, SignatureCheck};
use grit_lib::config::config_file_display_for_error;
use grit_lib::config::dotfile::is_ntfs_dot_gitignore;
use grit_lib::config::environment::Environment;
use grit_lib::config::ident::SignatureTimestamp;
use grit_lib::config::ident::{fsck_commit_idents, timestamp_for_at_ct};
use grit_lib::config::ident_resolve::{peek_name_with, IdentRole};
use grit_lib::config::ConfigSet;
use grit_lib::diff::{
    detect_copies, format_stat_line, read_submodule_head_for_checkout,
    submodule_commit_subject_line, unified_diff, unified_diff_with_prefix,
};
use grit_lib::lockfile::{LockFile, LockWait, SystemLockJitter};
use grit_lib::objects::hash::try_par_hash_with_force;
use grit_lib::objects::pack_receive::copy_reader_to_writer;
use grit_lib::objects::promisor_remote::parse_advertisement;
use grit_lib::objects::{CommitData, HashAlgo, ObjectId, ObjectKind};
use grit_lib::porcelain::log::{
    collect_decorations, collect_decorations_inner, current_branch_decoration_index,
    normalize_glob_ref, peel_to_commit_hex, DecorationFilter,
};
use grit_lib::porcelain::revert::{committer_timestamp, revision_set_newest_first};
use grit_lib::refs::ref_exclusions::RefExclusions;
use grit_lib::refs::store::paths::{logs_refs_dir_path, logs_refs_subdir_path};
use grit_lib::refs::{ref_update_flags, verify_branch_commit_cas};
use grit_lib::remote::clone::{cleanup_failed_clone, derive_clone_dir};
use grit_lib::remote::pkt_line::{
    read_data_lines_until_flush, read_until_flush_or_delim, write_flush, write_line_to_vec, Packet,
};
use grit_lib::remote::push_cert::{cert_env_from_check, verify_push_cert};
use grit_lib::remote::transport::http::{
    discover_upload_pack, discover_upload_pack_exact, discover_upload_pack_for_list_refs,
    discover_upload_pack_for_list_refs_from_body, discover_upload_pack_from_body, HttpClient,
};
use grit_lib::repo::{InitOptions, Repository};
use grit_lib::worktree::attributes::builtin_objectmode_index;
use grit_lib::worktree::resolve_linked_worktree_add_path;
use grit_lib::worktree::worktree_cwd::cwd_would_be_removed_with_dir;
use tempfile::tempdir;

#[test]
fn lockfile_public_surface() {
    let dir = tempdir().expect("tempdir");
    let lock_path = dir.path().join("sample.lock");
    let wait = LockWait::files_ref_lock(None);
    let _ = LockWait::parse_files_ref_lock_timeout_ms(None);
    let _jitter = SystemLockJitter;
    let mut lock = LockFile::acquire(&lock_path, wait).expect("acquire");
    let _ = lock.as_file().expect("as_file");
    let _ = lock.as_file_mut().expect("as_file_mut");
    lock.rollback();
    assert!(!lock_path.exists());
}

fn init_repo(dir: &Path) -> Repository {
    Repository::init(
        dir,
        &InitOptions::new()
            .with_initial_branch("main".to_owned())
            .with_ref_storage(grit_lib::RefStorageFormat::Files),
    )
    .expect("init")
}

fn hermetic_git(cmd: &mut Command) {
    cmd.env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null");
}

fn git_head_hex(dir: &Path) -> String {
    let out = {
        let mut cmd = Command::new("git");
        cmd.current_dir(dir).args(["rev-parse", "HEAD"]);
        hermetic_git(&mut cmd);
        cmd.output().expect("git rev-parse")
    };
    assert!(
        out.status.success(),
        "git rev-parse: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).trim().to_owned()
}

#[test]
fn blame_diff_dotfile_ident_and_hash_helpers() {
    let old = ["line a", "line b"];
    let new = ["line a", "line c"];
    let exact = vec![Some(0), None];
    let _ = build_fuzzy_line_map(&old, &new, &exact);

    let _ = unified_diff("a\n", "b\n", "f", "f", 3, false, false);
    let _ = unified_diff_with_prefix("a\n", "b\n", "f", "f", 3, 0, "a/", "b/", false, false);
    let _ = format_stat_line("path", 1, 2, 80);

    let empty_tree = ObjectId::from_hex("4b825dc642cb6eb9a060e54bf8d69288fbee4904").expect("tree");
    let odb_dir = tempdir().expect("t").path().join("objects");
    let odb = grit_lib::objects::odb::Odb::new(&odb_dir);
    let _ = detect_copies(
        &odb,
        None,
        Vec::new(),
        50,
        false,
        &[(String::new(), String::new(), empty_tree)],
    )
    .expect("detect copies");

    assert!(is_ntfs_dot_gitignore(".gitignore"));

    let commit_bytes = b"tree 4b825dc642cb6eb9a060e54bf8d69288fbee4904\n\
author A <a@b> 0 +0000\ncommitter C <c@d> 0 +0000\n\nm\n";
    let _ = fsck_commit_idents(commit_bytes, HashAlgo::Sha1);
    let _ = timestamp_for_at_ct(SignatureTimestamp::Sentinel);
    let _ = timestamp_for_at_ct(SignatureTimestamp::Valid(1));

    let env = Environment::empty();
    let cfg = ConfigSet::new();
    let _ = peek_name_with(&env, &cfg, IdentRole::Author);

    let items = [1u8, 2, 3];
    let threads = NonZeroUsize::new(2).expect("nz");
    let _ = try_par_hash_with_force(&items, threads, |x| {
        Ok::<u32, std::convert::Infallible>(*x as u32)
    });

    let mut buf = Vec::new();
    let mut cursor = Cursor::new(b"hello");
    let _ = copy_reader_to_writer(&mut cursor, &mut buf);
}

#[test]
fn clone_config_command_runner_and_cwd_helpers() {
    assert!(!derive_clone_dir("https://example.com/foo.git")
        .expect("dir name")
        .is_empty());
    let dir = tempdir().expect("tempdir");
    let nested = dir.path().join("clone-fail");
    std::fs::create_dir_all(&nested).expect("mkdir");
    cleanup_failed_clone(&nested, false);

    let _ = config_file_display_for_error(Path::new("/tmp/no-such-config"));

    let runner = RecordingRunner::always_success();
    if cfg!(unix) {
        let out = run_program(
            runner.as_ref(),
            Path::new("/bin/echo"),
            &[std::ffi::OsString::from("ok")],
            None,
            CommandStdin::Null,
            CommandStdio::Null,
        );
        assert!(out.is_ok());
    }

    let wt = dir.path();
    let _ = cwd_would_be_removed_with_dir(wt, wt, ".");
    let _ = resolve_linked_worktree_add_path(wt, wt);
}

#[test]
fn pkt_line_promisor_push_cert_and_ref_helpers() {
    let mut pkt = Vec::new();
    write_line_to_vec(&mut pkt, "line").expect("line");
    write_flush(&mut pkt).expect("flush");
    let mut c = Cursor::new(&pkt);
    let (self_lines, self_term) = read_until_flush_or_delim(&mut c).expect("read");
    assert_eq!(self_lines, vec!["line".to_owned()]);
    assert_eq!(self_term, Some(Packet::Flush));
    let mut c2 = Cursor::new(&pkt);
    let self_flush_lines =
        read_data_lines_until_flush(&mut c2, "expected flush after self pkt-line").expect("data");
    assert_eq!(self_flush_lines, self_lines);

    let dir = tempdir().expect("tempdir");
    init_repo(dir.path());
    std::fs::write(dir.path().join("pkt.txt"), b"peer\n").expect("write");
    {
        let mut add = Command::new("git");
        add.current_dir(dir.path()).args(["add", "pkt.txt"]);
        hermetic_git(&mut add);
        let out = add.output().expect("git add");
        assert!(
            out.status.success(),
            "git add: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }
    {
        let mut commit = Command::new("git");
        commit
            .current_dir(dir.path())
            .args(["commit", "-qm", "pkt-line peer"]);
        commit
            .env("GIT_AUTHOR_NAME", "T")
            .env("GIT_AUTHOR_EMAIL", "t@e.com")
            .env("GIT_COMMITTER_NAME", "T")
            .env("GIT_COMMITTER_EMAIL", "t@e.com");
        hermetic_git(&mut commit);
        let out = commit.output().expect("git commit");
        assert!(
            out.status.success(),
            "git commit: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }
    let head_hex = git_head_hex(dir.path());

    let git_advert = {
        let mut cmd = Command::new("git");
        cmd.current_dir(dir.path())
            .args(["upload-pack", "--advertise-refs", "."]);
        hermetic_git(&mut cmd);
        cmd.output().expect("git upload-pack --advertise-refs")
    };
    assert!(
        git_advert.status.success(),
        "git upload-pack --advertise-refs: {}",
        String::from_utf8_lossy(&git_advert.stderr)
    );
    assert!(
        !git_advert.stdout.is_empty(),
        "git must emit pkt-line ref advertisement"
    );

    let mut advert = Cursor::new(git_advert.stdout.as_slice());
    let (git_ref_lines, git_term) =
        read_until_flush_or_delim(&mut advert).expect("read git ref pkt-lines");
    assert_eq!(git_term, Some(Packet::Flush));
    assert!(
        git_ref_lines
            .iter()
            .any(|line| line.contains(&head_hex) && line.contains("HEAD")),
        "git ref pkt-lines must advertise HEAD at {head_hex}: {git_ref_lines:?}"
    );

    let mut advert2 = Cursor::new(git_advert.stdout.as_slice());
    let git_data_lines =
        read_data_lines_until_flush(&mut advert2, "expected flush after git ref advertisement")
            .expect("read git pkt-lines until flush");
    assert_eq!(git_data_lines, git_ref_lines);

    let mut fetch_request = Vec::new();
    write_line_to_vec(
        &mut fetch_request,
        &format!("want {head_hex} multi_ack side-band-64k"),
    )
    .expect("want line");
    write_flush(&mut fetch_request).expect("request flush");
    write_line_to_vec(&mut fetch_request, "done").expect("done");
    write_flush(&mut fetch_request).expect("done flush");

    let mut upload = Command::new("git");
    upload
        .current_dir(dir.path())
        .args(["upload-pack", "--stateless-rpc", "."])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    hermetic_git(&mut upload);
    let mut child = upload.spawn().expect("spawn git upload-pack");
    child
        .stdin
        .take()
        .expect("stdin")
        .write_all(&fetch_request)
        .expect("write fetch pkt-lines");
    let pack_response = child.wait_with_output().expect("wait upload-pack");
    assert!(
        pack_response.status.success(),
        "git upload-pack must accept grit-written pkt-lines: {}",
        String::from_utf8_lossy(&pack_response.stderr)
    );
    assert!(
        pack_response.stdout.len() > 20,
        "git must respond with a non-trivial pack pkt-line stream"
    );
    let mut pack_reader = Cursor::new(pack_response.stdout.as_slice());
    let (_pack_lines, pack_term) =
        read_until_flush_or_delim(&mut pack_reader).expect("read git pack section");
    assert!(
        pack_term.is_some(),
        "git pack response must terminate a pkt-line section"
    );

    assert!(parse_advertisement("version https://git-scm.com/docs/pack-protocol/2\n").is_empty());

    let check = SignatureCheck::default_none();
    let _ = cert_env_from_check(&check, "oid".to_owned(), None);
    let gpg = GpgConfig::from_config(&ConfigSet::new()).expect("gpg cfg");
    let _ = verify_push_cert(&gpg, b"not a tag");

    let mut excl = RefExclusions::default();
    excl.clear();
    let _ = ref_update_flags("refs/heads/main");
    let oid = ObjectId::from_bytes(&[1u8; 20]).expect("oid");
    verify_branch_commit_cas(Some(oid), Some(oid), "refs/heads/main").expect("cas");

    let git_dir = tempdir().expect("t").path().join(".git");
    std::fs::create_dir_all(&git_dir).expect("mkdir");
    let _ = logs_refs_dir_path(&git_dir);
    let _ = logs_refs_subdir_path(&git_dir, "heads");
}

struct MockHttp {
    body: Vec<u8>,
}

impl HttpClient for MockHttp {
    fn get(&self, _url: &str, _git_protocol: Option<&str>) -> grit_lib::error::Result<Vec<u8>> {
        Ok(self.body.clone())
    }

    fn post(
        &self,
        _url: &str,
        _content_type: &str,
        _accept: &str,
        _body: &[u8],
        _git_protocol: Option<&str>,
    ) -> grit_lib::error::Result<Vec<u8>> {
        Ok(Vec::new())
    }

    fn get_with_final_url_exact(
        &self,
        _url: &str,
        _git_protocol: Option<&str>,
    ) -> grit_lib::error::Result<(Vec<u8>, Option<String>)> {
        Ok((self.body.clone(), None))
    }
}

fn sample_smart_advertisement() -> Vec<u8> {
    let oid = "0000000000000000000000000000000000000000";
    let mut body = String::new();
    body.push_str("001e# service=git-upload-pack\n");
    body.push_str("0000");
    body.push_str(&format!("0032{oid} capabilities^{}\n", "multi_ack"));
    body.push_str("0000");
    body.into_bytes()
}

#[test]
fn transport_http_discover_helpers() {
    let body = sample_smart_advertisement();
    let _ = discover_upload_pack_from_body(&body);
    let _ = discover_upload_pack_for_list_refs_from_body(&body);
    let client = MockHttp { body: body.clone() };
    let _ = discover_upload_pack(&client, "https://example.com/r.git", None);
    let _ = discover_upload_pack_exact(&client, "https://example.com/r.git", Some("version=2"));
    let _ = discover_upload_pack_for_list_refs(&client, "https://example.com/r.git", None);

    let dir = tempdir().expect("tempdir");
    init_repo(dir.path());
    std::fs::write(dir.path().join("f.txt"), b"x\n").expect("write");
    {
        let mut add = Command::new("git");
        add.current_dir(dir.path()).args(["add", "f.txt"]);
        hermetic_git(&mut add);
        let out = add.output().expect("git add");
        assert!(
            out.status.success(),
            "git add: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }
    {
        let mut commit = Command::new("git");
        commit.current_dir(dir.path()).args(["commit", "-qm", "c"]);
        commit
            .env("GIT_AUTHOR_NAME", "T")
            .env("GIT_AUTHOR_EMAIL", "t@e.com")
            .env("GIT_COMMITTER_NAME", "T")
            .env("GIT_COMMITTER_EMAIL", "t@e.com");
        hermetic_git(&mut commit);
        let out = commit.output().expect("git commit");
        assert!(
            out.status.success(),
            "git commit: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }
    let git_advert = {
        let mut cmd = Command::new("git");
        cmd.current_dir(dir.path())
            .args(["upload-pack", "--advertise-refs", "."]);
        hermetic_git(&mut cmd);
        cmd.output().expect("git upload-pack")
    };
    assert!(
        git_advert.status.success(),
        "git upload-pack: {}",
        String::from_utf8_lossy(&git_advert.stderr)
    );
    let (_version, refs, _caps, _agent) =
        discover_upload_pack_from_body(&git_advert.stdout).expect("parse git advertisement");
    assert!(
        refs.iter().any(|(name, _)| name == "refs/heads/main"),
        "parsed advertisement must list main: {refs:?}"
    );
    let (_version2, list_refs, _caps2, _agent2) =
        discover_upload_pack_for_list_refs_from_body(&git_advert.stdout)
            .expect("list refs from git");
    assert!(
        list_refs.iter().any(|(name, _)| name == "refs/heads/main"),
        "list-refs parse must include main: {list_refs:?}"
    );
}

#[test]
fn porcelain_log_revert_and_submodule_diff_helpers() {
    let dir = tempdir().expect("tempdir");
    let repo = init_repo(dir.path());
    let git_dir = dir.path().join(".git");
    let seed = ObjectId::from_hex("4b825dc642cb6eb9a060e54bf8d69288fbee4904").expect("tree");
    grit_lib::refs::write_ref(git_dir.as_path(), "refs/heads/main", &seed).expect("seed main");
    let _ = collect_decorations(&repo, false);
    let _ = collect_decorations_inner(&repo, true, true, &DecorationFilter::default());
    let _ = normalize_glob_ref("heads/*");
    let odb = repo.odb();
    let head = grit_lib::refs::resolve_ref(repo.git_dir(), "HEAD").expect("resolve");
    let _ = peel_to_commit_hex(odb, &head.to_hex());
    let head_state = grit_lib::state::resolve_head(repo.git_dir()).expect("head");
    let _ = current_branch_decoration_index(&[], &head_state);

    let c = CommitData {
        tree: ObjectId::from_hex("4b825dc642cb6eb9a060e54bf8d69288fbee4904").expect("tree"),
        parents: Vec::new(),
        author: String::new(),
        committer: "C <c@d> 1700000000 +0000".to_owned(),
        author_raw: Vec::new(),
        committer_raw: Vec::new(),
        encoding: None,
        message: "subject\n".to_owned(),
        raw_message: None,
        extra_headers: Vec::new(),
    };
    let _ = submodule_commit_subject_line(&c);
    let _ = read_submodule_head_for_checkout(dir.path());

    assert_eq!(committer_timestamp("N <e@x> 42 +0000"), 42);
    let tip = repo.odb().write(ObjectKind::Blob, b"x").expect("blob");
    let _ = revision_set_newest_first(&repo, &[tip], &[]);

    let index = repo.index_handle().load().expect("index");
    let _ = builtin_objectmode_index(&index, "README.md");
}

#[test]
fn hooks_helpers_with_repo() {
    let dir = tempdir().expect("tempdir");
    let repo = init_repo(dir.path());
    let _ = resolve_hooks_dir(&repo);
    let _ = repo.hooks().list();
    let _ = run_hook_with_env(&repo, "pre-commit", &[], None, &[]);
    let _ = run_hook_capture(&repo, "pre-commit", &[], None);
    let head = grit_lib::state::resolve_head(repo.git_dir()).expect("head");
    let zero = ObjectId::zero();
    let _ = run_reference_transaction_committed_for_head_update(&repo, &head, None, zero);
}

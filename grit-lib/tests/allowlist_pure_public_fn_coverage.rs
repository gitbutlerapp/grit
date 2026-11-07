//! Execute allowlisted public helpers that need little or no repository setup.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use crate::support;

use grit_lib::objects::FileMode;
use std::io::{Cursor, Write};
use std::num::NonZeroUsize;
use std::path::Path;
use std::process::{Command, Stdio};

use grit_lib::blame::build_fuzzy_line_map;
use grit_lib::command_runner::hooks::{resolve_hooks_dir, run_hook};
use grit_lib::command_runner::{run_program, CommandStdin, CommandStdio, RecordingRunner};
use grit_lib::config::config_file_display_for_error;
use grit_lib::config::dotfile::is_ntfs_dot_gitignore;
use grit_lib::diff::{
    detect_copies, read_submodule_head_for_checkout, submodule_commit_subject_line, unified_diff,
    unified_diff_with_prefix,
};
use grit_lib::ident::SignatureTimestamp;
use grit_lib::ident::{fsck_commit_idents, timestamp_for_at_ct};
use grit_lib::lockfile::{LockFile, LockWait, SystemLockJitter};
use grit_lib::objects::hash::try_par_hash_forced;
use grit_lib::objects::pack_receive::copy_reader_to_writer;
use grit_lib::objects::promisor_remote::parse_advertisement;
use grit_lib::objects::{CommitData, HashAlgo, ObjectId};
use grit_lib::porcelain::log::{
    current_branch_decoration_index, normalize_glob_ref, peel_to_commit_hex, DecorationFilter,
};
use grit_lib::porcelain::revert::commits_to_revert;
use grit_lib::refs::store::paths::{logs_refs_dir_path, logs_refs_subdir_path};
use grit_lib::refs::{ref_update_flags, verify_branch_commit_cas};
use grit_lib::remote::clone::{cleanup_failed_clone, derive_clone_dir};
use grit_lib::remote::pkt_line::{write_flush, write_line_to_vec};
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
            .with_ref_storage(grit_lib::refs::ref_storage::RefStorageFormat::Files),
    )
    .expect("init")
    .0
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
    let empty_tree = ObjectId::from_hex("4b825dc642cb6eb9a060e54bf8d69288fbee4904").expect("tree");
    let odb_dir = tempdir().expect("t").path().join("objects");
    let odb = grit_lib::objects::odb::Odb::new(&odb_dir);
    let _ = detect_copies(
        &odb,
        None,
        Vec::new(),
        50,
        false,
        &[(String::new(), FileMode::ABSENT, empty_tree)],
    )
    .expect("detect copies");

    assert!(is_ntfs_dot_gitignore(".gitignore"));

    let commit_bytes = b"tree 4b825dc642cb6eb9a060e54bf8d69288fbee4904\n\
author A <a@b> 0 +0000\ncommitter C <c@d> 0 +0000\n\nm\n";
    let _ = fsck_commit_idents(commit_bytes, HashAlgo::Sha1);
    let _ = timestamp_for_at_ct(SignatureTimestamp::Sentinel);
    let _ = timestamp_for_at_ct(SignatureTimestamp::Valid(1));

    let items = [1u8, 2, 3];
    let threads = NonZeroUsize::new(2).expect("nz");
    let _ = try_par_hash_forced(&items, threads, |x| {
        Ok::<u32, std::convert::Infallible>(*x as u32)
    });

    let mut buf = Vec::new();
    let mut cursor = Cursor::new(b"hello");
    let _ = copy_reader_to_writer(&mut cursor, &mut buf);
}

#[test]
fn clone_config_command_runner_and_cwd_helpers() {
    assert!(!derive_clone_dir("https://example.com/foo.git", false)
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
fn promisor_and_ref_store_helpers() {
    assert!(parse_advertisement("version https://git-scm.com/docs/pack-protocol/2\n").is_empty());

    let _ = ref_update_flags("refs/heads/main");
    let oid = ObjectId::from_bytes(&[1u8; 20]).expect("oid");
    verify_branch_commit_cas(Some(oid), Some(oid), "refs/heads/main").expect("cas");

    let git_dir = tempdir().expect("t").path().join(".git");
    std::fs::create_dir_all(&git_dir).expect("mkdir");
    let _ = logs_refs_dir_path(&git_dir);
    let _ = logs_refs_subdir_path(&git_dir, "heads");
}

#[test]
fn porcelain_log_revert_and_submodule_diff_helpers() {
    let dir = tempdir().expect("tempdir");
    let repo = init_repo(dir.path());
    let git_dir = dir.path().join(".git");
    let seed = support::stored_commit(&git_dir, 1);
    grit_test_support::refs::set_ref(
        &grit_test_support::refs::at(git_dir.as_path()),
        "refs/heads/main",
        &seed,
    )
    .expect("seed main");
    let _ = repo.revwalk().decorations(
        &grit_lib::porcelain::log::DecorationOptions::default().with_full_ref_names(false),
    );
    let _ = repo.revwalk().decorations(
        &grit_lib::porcelain::log::DecorationOptions::default()
            .with_full_ref_names(true)
            .with_clear_decorations(true)
            .with_filter((&DecorationFilter::default()).clone()),
    );
    let _ = normalize_glob_ref("heads/*");
    let odb = repo.odb();
    let head = repo.references().resolve("HEAD").expect("resolve");
    let _ = peel_to_commit_hex(odb, &head.to_hex());
    let head_state = repo.references().head().expect("head");
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

    assert!(commits_to_revert(&repo, &[]).expect("no specs").is_empty());

    let index = repo
        .index()
        .load(&grit_lib::index::IndexLoadOptions::default())
        .expect("index");
    let _ = builtin_objectmode_index(&index, "README.md");
}

#[test]
fn hooks_helpers_with_repo() {
    let dir = tempdir().expect("tempdir");
    let repo = init_repo(dir.path());
    let _ = resolve_hooks_dir(&repo);
    let _ = repo.hooks().list(None);
    let _ = run_hook(&repo, "pre-commit", &[], None);
    let _ = run_hook(
        &repo,
        "reference-transaction",
        &["committed"],
        Some(b"0000000000000000000000000000000000000000 0000000000000000000000000000000000000000 HEAD\n"),
    );
}

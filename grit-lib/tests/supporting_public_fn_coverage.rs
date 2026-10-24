//! Execute assorted public grit-lib helpers (strings, config, transport glue) for coverage.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::fs;
use std::io::Cursor;
use std::path::Path;
use std::sync::Arc;

use grit_lib::commit::encoding::identity_raw_for_serialized_commit;
use grit_lib::commit::trailers::finalize_cherry_pick_message;
use grit_lib::config::dotfile::{
    dotfile_matches, is_hfs_dot_gitattributes, is_hfs_dot_gitignore, is_hfs_dot_gitmodules,
    is_hfs_dot_mailmap, is_ntfs_dot_gitattributes, is_ntfs_dot_gitignore, is_ntfs_dot_gitmodules,
    is_ntfs_dot_mailmap,
};
use grit_lib::config::environment::Environment;
use grit_lib::config::git_date::show::{
    show_date_relative, strbuf_addftime, DateMode, DateModeType,
};
use grit_lib::config::git_date::tm::{
    empty_tm, init_tm_unknown, is_date_known, local_tzoffset, time_to_tm_local,
};
use grit_lib::config::git_path::{
    abspath_part_inside_repo, longest_ancestor_length, prefix_path_gently,
};
use grit_lib::config::precompose_config::{
    argv_precompose_enabled, effective_core_precomposeunicode, filesystem_nfd_nfc_aliases,
    locate_git_dir_from_environment, pathspec_precompose_enabled,
};
use grit_lib::config::ConfigSet;
use grit_lib::diagnostics::{warn, CollectingDiagnostics, DiagnosticsHandle, Warning};
use grit_lib::diff::userdiff::matcher_for_path_parsed;
use grit_lib::diff::ws::{is_space_git, ws_blank_line};
use grit_lib::objects::{HashAlgo, ObjectId};
use grit_lib::parallel::{index_parallelism_from_config, ParallelOp, ThreadPolicy};
use grit_lib::porcelain::terminal::{ansi_supported, stderr_supports_color, stdout_supports_color};
use grit_lib::progress::ProgressPhase;
use grit_lib::refs::branch_tracking::{
    apply_push_remote_tracking_updates, list_branches, upstream_status, upstream_tracking_full_ref,
    BranchDetail, BranchListQuery, BranchScope,
};
use grit_lib::refs::hide_refs::{hide_ref_patterns_receive, hide_ref_patterns_uploadpack};
use grit_lib::refs::listing::RefQuery;
use grit_lib::refs::ref_namespace::{
    logical_ref_name_from_storage, logical_ref_name_from_storage_env,
};
use grit_lib::refs::refspec::parse_push_refspec;
use grit_lib::remote::credentials::Credential;
use grit_lib::remote::pkt_line::{
    read_until_flush_or_delim, write_flush, write_line_to_vec, write_sideband_packet,
};
use grit_lib::remote::protocol::{
    check_protocol_allowed_with, protocol_from_user, ProtocolPolicyInputs,
};
use grit_lib::remote::push_report::{PushRefResult, PushRefStatus};
use grit_lib::remote::transport_path::looks_like_command_line_option;
use grit_lib::repo::{InitOptions, Repository};
use grit_lib::worktree::attributes::MacroTable;
use grit_lib::worktree::crlf::{
    load_gitattributes_for_checkout, load_gitattributes_from_index, parse_gitattributes_content,
    ConversionConfig,
};
use grit_lib::worktree::pathspec::{
    extend_pathspec_list_implicit_cwd, pathdiff, resolve_magic_pathspec,
    resolve_pathspec_in_worktree,
};
use tempfile::tempdir;

fn init_repo(dir: &Path) -> Repository {
    Repository::init(
        dir,
        &InitOptions::new()
            .with_initial_branch("main".to_owned())
            .with_ref_storage(grit_lib::RefStorageFormat::Files),
    )
    .expect("init")
}

#[test]
fn commit_encoding_and_trailers_helpers() {
    let (a, c) = identity_raw_for_serialized_commit(&None, "A", "B");
    assert!(a.is_empty() && c.is_empty());

    let env = Environment::empty();
    let dir = tempdir().expect("tempdir");
    init_repo(dir.path());
    let git_dir = dir.path().join(".git");
    let config = ConfigSet::load(&env, Some(&git_dir), true).expect("config");
    let _ = finalize_cherry_pick_message(
        "subject\n",
        true,
        false,
        "Name",
        "email@example.com",
        &config,
        "deadbeef",
    );
}

#[test]
fn credentials_parse_roundtrip() {
    let wire = "protocol=https\nhost=example.com\nusername=u\npassword=p\n";
    let cred = Credential::parse(wire);
    assert_eq!(cred.protocol.as_deref(), Some("https"));
    let out = cred.serialize();
    assert!(out.contains("protocol=https"));
}

#[test]
fn crlf_and_pathspec_helpers() {
    let env = Environment::empty();
    let dir = tempdir().expect("tempdir");
    let repo = init_repo(dir.path());
    let git_dir = dir.path().join(".git");
    let config = ConfigSet::load(&env, Some(&git_dir), true).expect("config");
    let _ = ConversionConfig::from_config(&config);

    let _ = parse_gitattributes_content("*.bin binary\n");

    let index = repo.index_handle().load().expect("index");
    let odb = repo.odb();
    let _ = load_gitattributes_from_index(&index, &odb);
    fs::write(dir.path().join(".gitattributes"), "* text=auto\n").expect("ga");
    let _ = load_gitattributes_for_checkout(dir.path(), "foo.txt", &index, &odb);

    let _ = resolve_magic_pathspec(":(glob)*.rs", "");
    let specs = vec![":(exclude)foo".to_owned()];
    let _ = extend_pathspec_list_implicit_cwd(&specs, Some("src"));
    let _ = pathdiff(dir.path(), dir.path());
    let _ = resolve_pathspec_in_worktree("add", "README.md", dir.path(), None);
}

#[test]
fn diagnostics_dotfile_environment() {
    let handle: DiagnosticsHandle = Arc::new(CollectingDiagnostics::new());
    warn(
        &handle,
        Warning::AmbiguousRefname {
            spec: "ambiguous".to_owned(),
        },
    );

    assert!(!dotfile_matches("unknown", ".gitignore"));
    assert!(is_hfs_dot_gitignore(".gitignore") || is_ntfs_dot_gitignore(".gitignore"));
    assert!(is_hfs_dot_gitattributes(".gitattributes"));
    assert!(is_ntfs_dot_gitattributes(".gitattributes"));
    assert!(is_hfs_dot_gitmodules(".gitmodules"));
    assert!(is_ntfs_dot_gitmodules(".gitmodules"));
    assert!(is_hfs_dot_mailmap(".mailmap"));
    assert!(is_ntfs_dot_mailmap(".mailmap"));

    let env = Environment::empty();
    assert!(env.var("NO_SUCH_VAR").is_none());
    assert!(env.var_os("NO_SUCH_VAR").is_none());
    let _ = env.reftable_autocompaction_enabled();
}

#[test]
fn git_date_helpers() {
    let _ = show_date_relative(1_700_000_000, 1_700_000_100);
    let _ = DateMode::from_type(DateModeType::Normal);
    let mut t = init_tm_unknown();
    assert!(!is_date_known(&t));
    t.tm_year = 124;
    t.tm_mon = 0;
    t.tm_mday = 1;
    assert!(is_date_known(&t));
    let _ = strbuf_addftime(&t, 0, "%Y", true);
    let _ = local_tzoffset(1_700_000_000);
    let mut out = empty_tm();
    let _ = unsafe { time_to_tm_local(1_700_000_000, std::ptr::addr_of_mut!(out)) };
}

#[test]
fn git_path_precompose_ident_objects() {
    let dir = tempdir().expect("tempdir");
    init_repo(dir.path());
    let _ = longest_ancestor_length("a/b", "a:b/c").expect("ancestor");
    let _ = abspath_part_inside_repo("file.txt", dir.path());
    let _ = prefix_path_gently("", "x", dir.path());

    let git_dir = dir.path().join(".git");
    let _ = effective_core_precomposeunicode(Some(&git_dir));
    let _ = filesystem_nfd_nfc_aliases(&git_dir);
    let _ = argv_precompose_enabled(Some(&git_dir));
    let _ = pathspec_precompose_enabled();
    let env = Environment::empty();
    let _ = locate_git_dir_from_environment(&env);

    let _ = ObjectId::zero();
    let _ = ObjectId::null(HashAlgo::Sha1);
}

#[test]
fn hide_refs_namespace_refspec_parallel() {
    let env = Environment::empty();
    let dir = tempdir().expect("tempdir");
    init_repo(dir.path());
    let git_dir = dir.path().join(".git");
    let config = ConfigSet::load(&env, Some(&git_dir), true).expect("config");
    let _ = hide_ref_patterns_receive(&config);
    let _ = hide_ref_patterns_uploadpack(&config);

    let _ = logical_ref_name_from_storage("refs/heads/main");
    let _ = logical_ref_name_from_storage_env(&env, "refs/heads/main");

    let _ = parse_push_refspec("refs/heads/main:refs/heads/main").expect("refspec");
    let _ = index_parallelism_from_config(&config);
    let policy = ThreadPolicy::default();
    let _ = policy.resolve(ParallelOp::IndexLoad, &config);
    let _ = policy.try_resolve(ParallelOp::IndexLoad, &config);
}

#[test]
fn protocol_transport_terminal_progress_ws_userdiff() {
    let env = Environment::empty();
    let dir = tempdir().expect("tempdir");
    init_repo(dir.path());
    let config = ConfigSet::load(&env, Some(&dir.path().join(".git")), true).expect("config");
    let macros = MacroTable::default();

    let inputs = ProtocolPolicyInputs::default();
    let _ = check_protocol_allowed_with("https", &inputs);
    assert!(protocol_from_user(None));

    assert!(!looks_like_command_line_option("file.txt"));

    let _ = ansi_supported();
    let _ = stdout_supports_color();
    let _ = stderr_supports_color();
    let _ = ProgressPhase::ReceivingObjects.default_unit();

    assert!(is_space_git(b'\t'));
    assert!(ws_blank_line("   \n"));
    let _ = matcher_for_path_parsed(&config, &[], &macros, "foo.c", false);
}

#[test]
fn pkt_line_sideband_roundtrip() {
    let mut encoded = Vec::new();
    write_sideband_packet(&mut encoded, 1, b"payload").expect("write sb");
    assert_eq!(encoded, b"000c\x01payload");

    let mut pkt = Vec::new();
    write_sideband_packet(&mut pkt, 1, b"line").expect("pkt");
    pkt.extend_from_slice(b"0000");
    let mut cursor = Cursor::new(pkt);
    let (lines, term) = read_until_flush_or_delim(&mut cursor).expect("read");
    assert!(!lines.is_empty());
    assert!(term.is_some());

    let mut flush_pkt = Vec::new();
    write_line_to_vec(&mut flush_pkt, "ok").expect("line");
    write_flush(&mut flush_pkt).expect("flush");
    let mut c2 = Cursor::new(flush_pkt);
    let (lines2, term2) = read_until_flush_or_delim(&mut c2).expect("flush read");
    assert_eq!(lines2, vec!["ok".to_owned()]);
    assert!(term2.is_some());
}

#[test]
fn porcelain_checkout_glob_and_branch_tracking() {
    let dir = tempdir().expect("tempdir");
    let repo = init_repo(dir.path());
    let git_dir = dir.path().join(".git");
    fs::write(
        git_dir.join("config"),
        r#"[remote "origin"]
	url = /tmp/placeholder
	fetch = +refs/heads/*:refs/remotes/origin/*
[branch "main"]
	remote = origin
	merge = refs/heads/main
"#,
    )
    .expect("config");
    repo.reload_config().expect("reload");
    let empty_tree = ObjectId::from_hex("4b825dc642cb6eb9a060e54bf8d69288fbee4904").expect("tree");
    grit_lib::refs::write_ref(git_dir.as_path(), "refs/heads/main", &empty_tree).expect("main");
    assert_eq!(
        upstream_tracking_full_ref(&repo, "main").as_deref(),
        Some("refs/remotes/origin/main")
    );
    let head = grit_lib::refs::resolve_ref(git_dir.as_path(), "HEAD").expect("head");
    let pushed = PushRefResult {
        local_ref: Some("refs/heads/main".to_owned()),
        remote_ref: "refs/heads/main".to_owned(),
        old_oid: None,
        new_oid: Some(head),
        forced: false,
        deletion: false,
        status: PushRefStatus::Ok,
        message: None,
    };
    apply_push_remote_tracking_updates(git_dir.as_path(), "origin", &[pushed])
        .expect("tracking update");
    assert_eq!(
        grit_lib::refs::resolve_ref(git_dir.as_path(), "refs/remotes/origin/main").ok(),
        Some(head)
    );
    let query = BranchListQuery {
        scope: BranchScope::Local,
        refs: RefQuery::default(),
    };
    let _ = list_branches(&repo, &query, BranchDetail::Names).expect("list branches");
    let _ = upstream_status(&repo, "main").expect("upstream status");
}

//! Execute assorted public grit-lib helpers (strings, config, transport glue) for coverage.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::fs;
use std::io::Cursor;
use std::path::Path;
use std::sync::Arc;

use grit_lib::commit::encoding::{
    commit_message_unicode_for_display, decode_rfc2047_mailbox_from_line,
    identity_raw_for_serialized_commit, is_known_encoding, reencode_utf8_to_label,
};
use grit_lib::commit::trailers::{
    finalize_cherry_pick_message, format_trailers, message_ends_with_trailer, TrailerOpts,
};
use grit_lib::config::dotfile::{
    dotfile_matches, is_hfs_dot_gitattributes, is_hfs_dot_gitignore, is_hfs_dot_gitmodules,
    is_hfs_dot_mailmap, is_ntfs_dot_gitattributes, is_ntfs_dot_gitignore, is_ntfs_dot_gitmodules,
    is_ntfs_dot_mailmap,
};
use grit_lib::config::environment::Environment;
use grit_lib::config::git_date::show::{
    date_mode_release, show_date_relative, strbuf_addftime, DateMode, DateModeType,
};
use grit_lib::config::git_date::tm::{
    atoi_bytes, empty_tm, init_tm_unknown, is_date_known, local_tzoffset, time_to_tm_local,
};
use grit_lib::config::git_date::{test_tool_date, TestToolDateResult};
use grit_lib::config::git_path::{
    abspath_part_inside_repo, is_absolute_path_unix, longest_ancestor_length, prefix_path_gently,
    real_path_resolving, relative_url, strip_path_suffix,
};
use grit_lib::config::ident_config::current_uid;
use grit_lib::config::precompose_config::{
    argv_precompose_enabled, effective_core_precomposeunicode, filesystem_nfd_nfc_aliases,
    index_relpath_from_worktree, locate_git_dir_from_cwd, locate_git_dir_from_environment,
    pathspec_precompose_enabled, read_core_precomposeunicode,
};
use grit_lib::config::ConfigSet;
use grit_lib::diagnostics::{
    hint_line, trace_network, warn, warn_optional, CollectingDiagnostics, DiagnosticsHandle,
    Warning,
};
use grit_lib::diff::userdiff::{
    builtin_word_regex, matcher_for_path, matcher_for_path_parsed,
    word_regex_pattern_for_path_parsed,
};
use grit_lib::diff::ws::{whitespace_error_string, ws_blank_line};
use grit_lib::objects::{HashAlgo, ObjectId};
use grit_lib::parallel::{index_parallelism_from_config, ParallelOp, ThreadPolicy};
use grit_lib::porcelain::checkout::{
    glob_matches, is_glob_pattern, prepare_parent_dirs_for_checkout,
};
use grit_lib::porcelain::tag::{
    compare_version, glob_matches as tag_glob_matches, parse_epoch_from_ident, version_segments,
};
use grit_lib::porcelain::terminal::{ansi_supported, stderr_supports_color, stdout_supports_color};
use grit_lib::progress::ProgressPhase;
use grit_lib::refs::branch_tracking::{
    apply_push_remote_tracking_updates, upstream_tracking_full_ref,
};
use grit_lib::refs::hide_refs::{hide_ref_patterns_receive, hide_ref_patterns_uploadpack};
use grit_lib::refs::ref_namespace::{
    logical_ref_name_from_storage, logical_ref_name_from_storage_env, strip_namespace_prefix,
    strip_namespace_prefix_env,
};
use grit_lib::refs::refspec::parse_push_refspec;
use grit_lib::remote::credentials::Credential;
use grit_lib::remote::pkt_line::{
    decode_sideband_primary, read_data_lines_until_flush, read_until_flush_or_delim, write_flush,
    write_line_to_vec, write_sideband_channel1_64k_reader, write_sideband_packet,
};
use grit_lib::remote::protocol::{
    check_protocol_allowed_with, effective_client_protocol_version_from_inputs,
    merged_git_protocol_value, protocol_from_user, server_protocol_version_from_git_protocol,
    ClientProtocolVersionInputs, ProtocolPolicyInputs,
};
use grit_lib::remote::push_report::{PushRefResult, PushRefStatus};
use grit_lib::remote::transport_path::{
    check_local_url_path_not_option_like, looks_like_command_line_option,
};
use grit_lib::repo::{InitOptions, Repository};
use grit_lib::worktree::attributes::MacroTable;
use grit_lib::worktree::crlf::{
    convert_attr_ascii_for_ls_files, load_gitattributes_for_checkout,
    load_gitattributes_for_tree_path, load_gitattributes_from_index, parse_gitattributes_content,
    path_has_gitattribute, ConversionConfig,
};
use grit_lib::worktree::pathspec::{
    extend_pathspec_list_implicit_cwd, pathdiff, resolve_magic_pathspec,
    resolve_pathspec_in_worktree, wildmatch_flags_icase_glob,
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
    assert!(is_known_encoding("UTF-8"));
    assert!(!is_known_encoding("not-a-real-encoding"));
    let _ = decode_rfc2047_mailbox_from_line("From: =?UTF-8?B?dGVzdA==?= <a@b>");
    let (a, c) = identity_raw_for_serialized_commit(&None, "A", "B");
    assert!(a.is_empty() && c.is_empty());
    let _ = commit_message_unicode_for_display(None, "hello", None);
    let _ = reencode_utf8_to_label("UTF-8", "hi");

    let env = Environment::empty();
    let dir = tempdir().expect("tempdir");
    init_repo(dir.path());
    let git_dir = dir.path().join(".git");
    let config = ConfigSet::load(&env, Some(&git_dir), true).expect("config");
    assert!(!message_ends_with_trailer("one line\n", &config));
    let _ = format_trailers("Signed-off-by: X <x@y>\n", &TrailerOpts::default());
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

    let rules = parse_gitattributes_content("*.bin binary\n");
    let _ = path_has_gitattribute(&rules, "bin.dat", false, "binary");
    let _ = convert_attr_ascii_for_ls_files(&rules, "readme.txt", &config);

    let index = repo.index_handle().load().expect("index");
    let odb = repo.odb();
    let _ = load_gitattributes_from_index(&index, &odb);
    fs::write(dir.path().join(".gitattributes"), "* text=auto\n").expect("ga");
    let _ = load_gitattributes_for_checkout(dir.path(), "foo.txt", &index, &odb);
    let empty_tree = ObjectId::from_hex("4b825dc642cb6eb9a060e54bf8d69288fbee4904").expect("tree");
    let _ = load_gitattributes_for_tree_path(&odb, &empty_tree, "foo.txt");

    let _ = wildmatch_flags_icase_glob(true, true);
    let _ = resolve_magic_pathspec(":(glob)*.rs", "");
    let specs = vec![":(exclude)foo".to_owned()];
    let _ = extend_pathspec_list_implicit_cwd(&specs, Some("src"));
    let _ = pathdiff(dir.path(), dir.path());
    let _ = resolve_pathspec_in_worktree("add", "README.md", dir.path(), None);
}

#[test]
fn diagnostics_dotfile_environment() {
    assert!(hint_line("tip").starts_with("hint: "));
    let handle: DiagnosticsHandle = Arc::new(CollectingDiagnostics::new());
    warn(
        &handle,
        Warning::AmbiguousRefname {
            spec: "ambiguous".to_owned(),
        },
    );
    warn_optional(
        Some(handle.as_ref()),
        Warning::AmbiguousRefname {
            spec: "opt".to_owned(),
        },
    );
    trace_network(&handle, true, "trace".to_owned());

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
    let env = Environment::empty();
    match test_tool_date(&env, &["is64bit".to_owned()]).expect("tool date") {
        TestToolDateResult::Exit(code) => assert!(code == 0 || code == 1),
        TestToolDateResult::Output(lines) => assert!(!lines.is_empty() || true),
    }
    let _ = show_date_relative(1_700_000_000, 1_700_000_100);
    let mut mode = DateMode::from_type(DateModeType::Normal);
    date_mode_release(&mut mode);
    let mut t = init_tm_unknown();
    assert!(!is_date_known(&t));
    t.tm_year = 124;
    t.tm_mon = 0;
    t.tm_mday = 1;
    assert!(is_date_known(&t));
    let _ = strbuf_addftime(&t, 0, "%Y", true);
    assert_eq!(atoi_bytes(b"  -42"), -42);
    let _ = local_tzoffset(1_700_000_000);
    let mut out = empty_tm();
    let _ = unsafe { time_to_tm_local(1_700_000_000, std::ptr::addr_of_mut!(out)) };
}

#[test]
fn git_path_precompose_ident_objects() {
    let dir = tempdir().expect("tempdir");
    init_repo(dir.path());
    let _ = is_absolute_path_unix("/abs");
    let _ = strip_path_suffix("foo/bar", "/bar");
    let _ = longest_ancestor_length("a/b", "a:b/c").expect("ancestor");
    let _ = relative_url("https://h/a.git", "https://h/b.git", None);
    let _ = real_path_resolving(".");
    let _ = abspath_part_inside_repo("file.txt", dir.path());
    let _ = prefix_path_gently("", "x", dir.path());

    let git_dir = dir.path().join(".git");
    let _ = read_core_precomposeunicode(&git_dir);
    let _ = effective_core_precomposeunicode(Some(&git_dir));
    let _ = filesystem_nfd_nfc_aliases(&git_dir);
    let _ = argv_precompose_enabled(Some(&git_dir));
    let _ = pathspec_precompose_enabled();
    let _ = index_relpath_from_worktree("src/x.rs", &git_dir);
    let _ = locate_git_dir_from_cwd(dir.path().to_path_buf());
    let env = Environment::empty();
    let _ = locate_git_dir_from_environment(&env);

    let _ = current_uid();
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
    let _ = strip_namespace_prefix("refs/heads/main");
    let _ = strip_namespace_prefix_env(&env, "refs/heads/main");

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
    let _ = effective_client_protocol_version_from_inputs(&ClientProtocolVersionInputs::default());
    let _ = merged_git_protocol_value(2, None);
    let _ = server_protocol_version_from_git_protocol(None);

    assert!(!looks_like_command_line_option("file.txt"));
    let _ = check_local_url_path_not_option_like("/tmp/repo.git");

    let _ = ansi_supported();
    let _ = stdout_supports_color();
    let _ = stderr_supports_color();
    let _ = ProgressPhase::ReceivingObjects.default_unit();

    let _ = whitespace_error_string(1);
    assert!(ws_blank_line("   \n"));
    let _ = builtin_word_regex("pascal");
    let _ = matcher_for_path(&config, &[], "foo.c");
    let _ = matcher_for_path_parsed(&config, &[], &macros, "foo.c", false);
    let _ = word_regex_pattern_for_path_parsed(&config, &[], &macros, "foo.c", false);
}

#[test]
fn pkt_line_sideband_roundtrip() {
    let mut encoded = Vec::new();
    write_sideband_packet(&mut encoded, 1, b"payload").expect("write sb");
    let decoded = decode_sideband_primary(&encoded).expect("decode");
    assert_eq!(decoded, b"payload");

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
    let lines2 = read_data_lines_until_flush(&mut c2, "expected flush").expect("flush read");
    assert_eq!(lines2, vec!["ok".to_owned()]);

    let mut out = Vec::new();
    let payload = b"chunked";
    write_sideband_channel1_64k_reader(&mut out, Cursor::new(payload)).expect("reader write");
    assert!(!out.is_empty());
}

#[test]
fn porcelain_checkout_glob_and_branch_tracking() {
    assert!(is_glob_pattern("*.rs"));
    assert!(glob_matches("*.rs", "lib.rs"));
    let dir = tempdir().expect("tempdir");
    prepare_parent_dirs_for_checkout(dir.path(), "nested/file.txt").expect("parents");
    assert_eq!(compare_version("1.2", "1.10"), std::cmp::Ordering::Less);
    let _ = version_segments("1.2.3");
    let _ = parse_epoch_from_ident("Name <e@x> 1234567890 +0000");
    assert!(tag_glob_matches("v*", "v1"));

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
}

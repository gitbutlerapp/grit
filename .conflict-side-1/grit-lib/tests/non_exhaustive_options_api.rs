//! Coverage for public option builders added for `#[non_exhaustive]` init/maintenance types.

use grit_lib::config::environment::{Environment, RepositoryOptions};
use grit_lib::diagnostics::NullDiagnostics;
use grit_lib::maintenance::{fsck, repack_all, FsckOptions, GcOptions, RepackOptions};
use grit_lib::repo::{InitOptions, Repository};
use grit_lib::RefStorageFormat;
use std::sync::Arc;
use tempfile::TempDir;

#[test]
fn init_options_new_matches_default_branch_and_storage() {
    let opts = InitOptions::new();
    assert!(!opts.bare);
    assert_eq!(opts.initial_branch, "main");
    assert_eq!(opts.ref_storage, RefStorageFormat::Files);
}

#[test]
fn init_options_with_bare() {
    let opts = InitOptions::new().with_bare(true);
    assert!(opts.bare);
}

#[test]
fn init_options_with_initial_branch() {
    let opts = InitOptions::new().with_initial_branch("develop");
    assert_eq!(opts.initial_branch, "develop");
}

#[test]
fn init_options_with_ref_storage_reftable() {
    let opts = InitOptions::new().with_ref_storage(RefStorageFormat::Reftable);
    assert_eq!(opts.ref_storage, RefStorageFormat::Reftable);
}

#[test]
fn init_options_with_template_dir() {
    let dir = std::path::PathBuf::from("/tmp/template");
    let opts = InitOptions::new().with_template_dir(Some(dir.clone()));
    assert_eq!(opts.template_dir.as_ref(), Some(&dir));
}

#[test]
fn init_options_with_separate_git_dir() {
    let sep = std::path::PathBuf::from("/tmp/external.git");
    let opts = InitOptions::new().with_separate_git_dir(Some(sep.clone()));
    assert_eq!(opts.separate_git_dir.as_ref(), Some(&sep));
}

#[test]
fn init_options_with_bare_with_worktree_at_path() {
    let opts = InitOptions::new().with_bare_with_worktree_at_path(true);
    assert!(opts.bare_with_worktree_at_path);
}

#[test]
fn init_options_builder_init_repository() {
    let tmp = TempDir::new().expect("tempdir");
    let opts = InitOptions::new()
        .with_initial_branch("main")
        .with_ref_storage(RefStorageFormat::Files);
    Repository::init(tmp.path(), &opts).expect("init");
    assert!(tmp.path().join(".git").join("HEAD").is_file());
}

#[test]
fn repack_options_with_dry_run() {
    let opts = RepackOptions::with_dry_run(true);
    assert!(opts.dry_run);
}

#[test]
fn gc_options_with_repack_dry_run() {
    let opts = GcOptions::with_repack_dry_run(true, 1_700_000_000);
    assert!(opts.repack.dry_run);
    assert!(opts.expire_reflog);
    assert_eq!(opts.now_unix, 1_700_000_000);
}

#[test]
fn gc_options_with_reflog_policy() {
    let opts = GcOptions::with_reflog_policy(42, false);
    assert!(!opts.expire_reflog);
    assert_eq!(opts.now_unix, 42);
}

#[test]
fn fsck_options_full_repository() {
    let opts = FsckOptions::full_repository(true);
    assert!(opts.strict);
    assert!(opts.check_objects && opts.check_refs && opts.check_midx);
}

#[test]
fn fsck_options_objects_and_refs() {
    let opts = FsckOptions::objects_and_refs(false);
    assert!(!opts.strict);
    assert!(opts.check_objects && opts.check_refs);
    assert!(!opts.check_midx);
}

#[test]
fn repository_options_with_environment() {
    let mut env = Environment::empty();
    env.cwd = std::path::PathBuf::from("/tmp");
    let opts = RepositoryOptions::with_environment(env);
    assert_eq!(opts.environment.cwd, std::path::PathBuf::from("/tmp"));
}

#[test]
fn repository_options_with_diagnostics_and_network_trace() {
    let sink = Arc::new(NullDiagnostics);
    let opts = RepositoryOptions::empty()
        .with_diagnostics(sink)
        .with_network_trace(true);
    assert!(opts.network_trace);
}

#[test]
fn repository_options_without_format_validation() {
    let opts = RepositoryOptions::empty().without_format_validation();
    assert!(!opts.validate_repository_format);
}

#[test]
fn repository_options_for_serving() {
    let opts = RepositoryOptions::empty().for_serving();
    assert!(opts.open_for_serving);
}

#[test]
fn maintenance_constructors_run_on_empty_repo() {
    let tmp = TempDir::new().expect("tempdir");
    let repo = Repository::init(tmp.path(), &InitOptions::new()).expect("init");
    let report = fsck(&repo, &FsckOptions::full_repository(false)).expect("fsck");
    assert!(report.ok);
    let stats = repack_all(&repo, &RepackOptions::with_dry_run(true)).expect("repack dry-run");
    assert!(stats.objects_packed.is_none());
}

#[test]
fn diff_ws_parse_whitespace_rule_default() {
    let rule = grit_lib::diff::ws::parse_whitespace_rule("").expect("parse");
    assert_eq!(rule, grit_lib::diff::ws::WS_DEFAULT_RULE);
}

#[test]
fn diff_ws_blank_at_eol_rule() {
    let rule = grit_lib::diff::ws::parse_whitespace_rule("blank-at-eol").expect("parse");
    assert_ne!(rule & grit_lib::diff::ws::WS_BLANK_AT_EOL, 0);
}

#[test]
fn diff_ws_negated_blank_at_eol() {
    let rule = grit_lib::diff::ws::parse_whitespace_rule("-blank-at-eol").expect("parse");
    assert_eq!(rule & grit_lib::diff::ws::WS_BLANK_AT_EOL, 0);
}

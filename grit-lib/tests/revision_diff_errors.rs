//! Typed [`RevisionError`] and [`DiffError`] variants (factory step 7106).

use grit_lib::apply::parse_patch;
use grit_lib::blame::{IgnoreRevs, IgnoreRevsSources};
use grit_lib::config::environment::RepositoryOptions;
use grit_lib::config::{ConfigFile, ConfigScope, ConfigSet};
use grit_lib::diff::error::DiffError;
use grit_lib::diff::userdiff::matcher_for_driver;
use grit_lib::error::{ApplyError, Error};
use grit_lib::objects::ObjectId;
use grit_lib::porcelain::merge::{write_merge_tree, MergePredictionOptions};
use grit_lib::repo::{InitOptions, Repository};
use grit_lib::revision_error::{ObjectFilterParseError, RevisionError};
use grit_lib::revwalk::rev_list::{expand_object_filter_for_protocol, ObjectFilter};
use grit_lib::revwalk::rev_parse::resolve_revision;
use grit_test_support::{git, git_cmd};
use std::fs;
use std::path::Path;
use tempfile::tempdir;

#[test]
fn revision_error_not_found_for_unknown_rev() {
    let dir = tempdir().unwrap();
    git_cmd(&["init"]).in_dir(dir.path()).exec();
    git(dir.path(), &["commit", "--allow-empty", "-m", "seed"]);
    let repo = Repository::open(
        &dir.path().join(".git"),
        &RepositoryOptions::empty().with_work_tree(dir.path()),
    )
    .unwrap();
    let err = resolve_revision(&repo, "not-a-real-rev").unwrap_err();
    assert!(matches!(err, Error::Revision(RevisionError::RevParse(_))));
    assert_eq!(err.kind_name(), "rev_parse");
}

#[test]
fn unrelated_histories_from_git_built_repo() {
    let dir = tempdir().unwrap();
    git_cmd(&["init"]).in_dir(dir.path()).exec();
    fs::write(dir.path().join("a"), "1").unwrap();
    git(dir.path(), &["add", "a"]);
    git(dir.path(), &["commit", "-m", "first"]);
    let first = ObjectId::from_hex(git(dir.path(), &["rev-parse", "HEAD"]).trim()).unwrap();
    git(dir.path(), &["checkout", "--orphan", "orphan"]);
    git(dir.path(), &["commit", "--allow-empty", "-m", "second"]);
    let second = ObjectId::from_hex(git(dir.path(), &["rev-parse", "HEAD"]).trim()).unwrap();
    let repo = Repository::open(
        &dir.path().join(".git"),
        &RepositoryOptions::empty().with_work_tree(dir.path()),
    )
    .unwrap();
    let err =
        write_merge_tree(&repo, first, second, &MergePredictionOptions::default()).unwrap_err();
    assert!(matches!(
        err,
        Error::Revision(RevisionError::UnrelatedHistories { .. })
    ));
    assert_eq!(err.kind_name(), "unrelated_histories");
}

#[test]
fn object_filter_parse_errors() {
    assert!(matches!(
        ObjectFilter::parse("not-a-filter"),
        Err(ObjectFilterParseError::InvalidFilterSpec { .. })
    ));
    assert!(expand_object_filter_for_protocol("!!!").is_err());
}

#[test]
fn apply_error_corrupt_patch() {
    let bad = "--- a/f\n+++ b/f\n@@ -1,3 +1,3 @@\n one\n";
    let err = parse_patch(bad, 1, "stdin", false, None).unwrap_err();
    assert!(matches!(
        err,
        Error::Diff(DiffError::Apply(ApplyError::CorruptPatch { .. }))
    ));
}

#[test]
fn userdiff_invalid_funcname() {
    let snippet = "[diff \"probe\"]\n\txfuncname = [unclosed\n";
    let file = ConfigFile::parse(Path::new(".git/config"), snippet, ConfigScope::Local).unwrap();
    let mut cfg = ConfigSet::new();
    cfg.merge(&file);
    let err = matcher_for_driver(&cfg, "probe").unwrap_err();
    assert!(matches!(err, DiffError::UserdiffInvalidFuncname { .. }));
}

#[test]
fn blame_ignore_revs_errors() {
    let dir = tempdir().unwrap();
    let repo = Repository::init(dir.path(), &InitOptions::default()).unwrap();
    git_cmd(&["init"]).in_dir(dir.path()).exec();
    git(dir.path(), &["commit", "--allow-empty", "-m", "c"]);
    let err = IgnoreRevs::load(
        &repo,
        &IgnoreRevsSources {
            revs: vec!["not-a-commit".into()],
            ..Default::default()
        },
    )
    .unwrap_err();
    assert!(matches!(
        err,
        Error::Diff(DiffError::BlameIgnoreRevUnknown { .. })
    ));
}

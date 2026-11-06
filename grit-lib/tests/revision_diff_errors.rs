//! Typed [`RevisionError`] and [`DiffError`] variants (factory step 7106).

use grit_lib::apply::{apply_patch_to_index, parse_patch, IndexApplyOptions};
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
use grit_lib::revwalk::RevWalkCommitStreamItem;
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
    assert!(matches!(err, RevisionError::RevParse(_)));
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
    assert!(matches!(
        err,
        DiffError::InvalidFuncname {
            source: grit_lib::diff::error::UserdiffFuncnameSourceError::InvalidRegexp { .. },
            ..
        }
    ));
}

#[test]
fn rev_walk_public_api_returns_revision_error() {
    let dir = tempdir().unwrap();
    let _ = git_cmd(&["init"]).in_dir(dir.path()).exec();
    git(dir.path(), &["commit", "--allow-empty", "-m", "c"]);
    let repo = Repository::open(
        &dir.path().join(".git"),
        &RepositoryOptions::empty().with_work_tree(dir.path()),
    )
    .unwrap();
    let err = repo.rev_walk().resolve("no-such-revision").unwrap_err();
    assert!(matches!(err, RevisionError::RevParse(_)));
}

#[test]
fn rev_walk_commits_stream_public_api_returns_revision_error() {
    let dir = tempdir().unwrap();
    let repo = Repository::init(dir.path(), &InitOptions::default()).unwrap();
    let _ = git_cmd(&["init"]).in_dir(dir.path()).exec();
    git(dir.path(), &["commit", "--allow-empty", "-m", "c"]);
    match repo
        .rev_walk()
        .commits_stream(&["no-such-revision"], &Default::default())
    {
        Err(e) => assert!(matches!(e, RevisionError::RevParse(_))),
        Ok(_) => panic!("expected revision error from commits_stream"),
    }
}

fn assert_rev_walk_stream_item_ty(
    _: std::result::Result<RevWalkCommitStreamItem, RevisionError>,
) {
}

#[test]
fn rev_walk_commits_stream_iter_item_is_revision_error() {
    let dir = tempdir().unwrap();
    let repo = Repository::init(dir.path(), &InitOptions::default()).unwrap();
    let _ = git_cmd(&["init"]).in_dir(dir.path()).exec();
    git(dir.path(), &["commit", "--allow-empty", "-m", "c"]);
    let mut stream = repo
        .rev_walk()
        .commits_stream(&["HEAD"], &Default::default())
        .unwrap()
        .expect("stream");
    if let Some(item) = stream.next() {
        assert_rev_walk_stream_item_ty(item);
    }
}

#[test]
fn log_walk_query_from_rev_walk_returns_revision_error() {
    let dir = tempdir().unwrap();
    let repo = Repository::init(dir.path(), &InitOptions::default()).unwrap();
    let _ = git_cmd(&["init"]).in_dir(dir.path()).exec();
    git(dir.path(), &["commit", "--allow-empty", "-m", "c"]);
    let err = grit_lib::revwalk::log_walk::LogWalkQuery::from_rev_walk(
        &repo,
        &["no-such-rev"],
        &Default::default(),
    )
    .unwrap_err();
    assert!(matches!(err, RevisionError::RevParse(_)));
}

#[test]
fn peel_bad_object_returns_revision_error() {
    let dir = tempdir().unwrap();
    let repo = Repository::init(dir.path(), &InitOptions::default()).unwrap();
    let _ = git_cmd(&["init"]).in_dir(dir.path()).exec();
    git(dir.path(), &["commit", "--allow-empty", "-m", "c"]);
    let tree_hex = git(dir.path(), &["rev-parse", "HEAD^{tree}"]);
    let tree_oid = ObjectId::from_hex(tree_hex.trim()).unwrap();
    let err =
        grit_lib::revwalk::rev_parse::peel_to_commit_for_merge_base(&repo, tree_oid).unwrap_err();
    assert!(matches!(err, RevisionError::RevParse(_)));
}

#[test]
fn apply_patch_that_does_not_apply() {
    let dir = tempdir().unwrap();
    let repo = Repository::init(dir.path(), &InitOptions::default()).unwrap();
    let _ = git_cmd(&["init"]).in_dir(dir.path()).exec();
    fs::write(dir.path().join("f"), "hello\n").unwrap();
    git(dir.path(), &["add", "f"]);
    git(dir.path(), &["commit", "-m", "init"]);
    let patch_text = "--- a/f\n+++ b/f\n@@ -1 +1 @@\n-other\n+hello\n";
    let patches = parse_patch(patch_text, 1, "stdin", false, None).unwrap();
    let mut index = repo.index_handle().load().unwrap();
    let err = apply_patch_to_index(&repo, &mut index, &patches, &IndexApplyOptions::default())
        .unwrap_err();
    assert!(
        matches!(
            err,
            ApplyError::ContextMismatch { .. } | ApplyError::PatchFailed { .. }
        ),
        "{err:?}"
    );
}

#[test]
fn blame_ignore_revs_file_io_error() {
    let dir = tempdir().unwrap();
    let repo = Repository::init(dir.path(), &InitOptions::default()).unwrap();
    let _ = git_cmd(&["init"]).in_dir(dir.path()).exec();
    git(dir.path(), &["commit", "--allow-empty", "-m", "c"]);
    let err = IgnoreRevs::load(
        &repo,
        &IgnoreRevsSources {
            files: vec!["/no/such/ignore-revs-file".into()],
            ..Default::default()
        },
    )
    .unwrap_err();
    assert!(matches!(
        err,
        Error::Diff(DiffError::BlameIgnoreRevsFileOpen { io, .. })
            if io.path.is_some()
    ));
}

#[test]
fn blame_ignore_revs_errors() {
    let dir = tempdir().unwrap();
    let repo = Repository::init(dir.path(), &InitOptions::default()).unwrap();
    let _ = git_cmd(&["init"]).in_dir(dir.path()).exec();
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

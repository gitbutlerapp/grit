//! `resolve_merge_source`: merge arguments name branches, tags, commit ids or any revision.

use grit_lib::error::Error;
use grit_lib::objects::{ObjectId, ObjectKind};
use grit_lib::porcelain::merge::{resolve_merge_source, MergeSource, MergeSourceKind};
use grit_lib::repo::Repository;
use grit_test_support::git;
use tempfile::TempDir;

fn open_repo(path: &std::path::Path) -> Repository {
    Repository::discover(
        Some(path),
        &grit_lib::config::environment::RepositoryOptions::empty(),
    )
    .expect("repo")
}

fn rev(dir: &TempDir, spec: &str) -> ObjectId {
    git(dir.path(), &["rev-parse", spec])
        .trim()
        .parse()
        .expect("oid")
}

fn commit(dir: &TempDir, file: &str, msg: &str) -> ObjectId {
    std::fs::write(dir.path().join(file), msg).expect("write");
    git(dir.path(), &["add", file]);
    git(
        dir.path(),
        &[
            "-c",
            "user.name=Test",
            "-c",
            "user.email=t@example.com",
            "commit",
            "-q",
            "-m",
            msg,
        ],
    );
    rev(dir, "HEAD")
}

/// main: c1..c4, tags `keep` (lightweight, c4) and `v1` (annotated, c3), branch `side` off c2,
/// a fake remote-tracking ref `origin/topic` at c2, and `ORIG_HEAD` at c1.
fn fixture() -> (TempDir, [ObjectId; 4]) {
    let dir = TempDir::new().expect("tempdir");
    git(dir.path(), &["init", "-q", "-b", "main"]);
    let c = [
        commit(&dir, "f1", "c1"),
        commit(&dir, "f2", "c2"),
        commit(&dir, "f3", "c3"),
        commit(&dir, "f4", "c4"),
    ];
    git(dir.path(), &["tag", "keep"]);
    git(
        dir.path(),
        &[
            "-c",
            "user.name=Test",
            "-c",
            "user.email=t@example.com",
            "tag",
            "-a",
            "-m",
            "release",
            "v1",
            &c[2].to_hex(),
        ],
    );
    git(dir.path(), &["branch", "side", &c[1].to_hex()]);
    git(
        dir.path(),
        &["update-ref", "refs/remotes/origin/topic", &c[1].to_hex()],
    );
    git(dir.path(), &["update-ref", "ORIG_HEAD", &c[0].to_hex()]);
    (dir, c)
}

fn resolved(repo: &Repository, spec: &str) -> MergeSource {
    resolve_merge_source(repo, spec).unwrap_or_else(|e| panic!("{spec}: {e}"))
}

#[test]
fn resolves_branches_by_namespace() {
    let (dir, c) = fixture();
    let repo = open_repo(dir.path());
    assert_eq!(
        resolved(&repo, "side"),
        MergeSource {
            oid: c[1],
            kind: MergeSourceKind::LocalBranch
        }
    );
    assert_eq!(
        resolved(&repo, "origin/topic"),
        MergeSource {
            oid: c[1],
            kind: MergeSourceKind::RemoteBranch
        }
    );
}

#[test]
fn resolves_lightweight_and_annotated_tags() {
    let (dir, c) = fixture();
    let repo = open_repo(dir.path());
    assert_eq!(
        resolved(&repo, "keep"),
        MergeSource {
            oid: c[3],
            kind: MergeSourceKind::Tag
        }
    );
    let annotated = resolved(&repo, "v1");
    assert_ne!(
        annotated.oid,
        rev(&dir, "refs/tags/v1"),
        "tag object must be peeled"
    );
    assert_eq!(
        annotated,
        MergeSource {
            oid: c[2],
            kind: MergeSourceKind::Tag
        }
    );
}

#[test]
fn resolves_commit_ids_and_revision_expressions() {
    let (dir, c) = fixture();
    let repo = open_repo(dir.path());
    let full = c[2].to_hex();
    for (spec, want) in [
        (full.as_str(), c[2]),
        (&full[..8], c[2]),
        ("keep~1", c[2]),
        ("v1^", c[1]),
        ("ORIG_HEAD", c[0]),
        ("origin/topic~1", c[0]),
        ("refs/heads/side", c[1]),
        ("HEAD~3", c[0]),
    ] {
        let got = resolved(&repo, spec);
        assert_eq!(got.kind, MergeSourceKind::Commit, "{spec}");
        assert_eq!(got.oid, want, "{spec}");
        assert_eq!(got.oid, rev(&dir, &format!("{spec}^{{commit}}")), "{spec}");
    }
}

#[test]
fn branch_name_wins_over_tag_of_same_name() {
    let (dir, c) = fixture();
    git(dir.path(), &["tag", "side", &c[3].to_hex()]);
    let repo = open_repo(dir.path());
    assert_eq!(
        resolved(&repo, "side"),
        MergeSource {
            oid: c[1],
            kind: MergeSourceKind::LocalBranch
        }
    );
}

#[test]
fn missing_revision_is_typed_not_found() {
    let (dir, _) = fixture();
    let repo = open_repo(dir.path());
    for spec in [
        "nonexist",
        "keep~10",
        "side..main",
        "side...main",
        "...main",
        "0123456789abcdef0123456789abcdef01234567",
    ] {
        match resolve_merge_source(&repo, spec) {
            Err(Error::RevisionNotFound(s)) => assert_eq!(s, spec),
            other => panic!("{spec}: expected RevisionNotFound, got {other:?}"),
        }
    }
}

#[test]
fn tree_or_blob_is_rejected_with_object_kind() {
    let (dir, c) = fixture();
    let tree = rev(&dir, "HEAD^{tree}");
    let blob = rev(&dir, "HEAD:f1");
    git(dir.path(), &["tag", "blobtag", &blob.to_hex()]);
    let repo = open_repo(dir.path());
    for (spec, oid, kind) in [
        (tree.to_hex(), tree, ObjectKind::Tree),
        ("HEAD:f1".to_owned(), blob, ObjectKind::Blob),
        ("blobtag".to_owned(), blob, ObjectKind::Blob),
    ] {
        match resolve_merge_source(&repo, &spec) {
            Err(Error::UnexpectedObjectKind {
                oid: got,
                expected: ObjectKind::Commit,
                actual,
            }) => {
                assert_eq!(got, oid, "{spec}");
                assert_eq!(actual, kind, "{spec}");
            }
            other => panic!("{spec}: expected UnexpectedObjectKind, got {other:?}"),
        }
    }
    assert_eq!(resolved(&repo, "main").oid, c[3]);
}

#[test]
fn dotted_message_selector_is_a_commit_not_a_range() {
    let (dir, _) = fixture();
    let candidate = commit(&dir, "f5", "release..candidate");
    commit(&dir, "f6", "after");
    let repo = open_repo(dir.path());
    for spec in ["main^{/release..candidate}", ":/release..candidate"] {
        let got = resolved(&repo, spec);
        assert_eq!(got.kind, MergeSourceKind::Commit, "{spec}");
        assert_eq!(got.oid, candidate, "{spec}");
        assert_eq!(got.oid, rev(&dir, spec), "{spec}");
    }
}

#[test]
fn ranges_over_brace_containing_branch_are_not_found() {
    let (dir, c) = fixture();
    git(dir.path(), &["branch", "foo{bar", &c[1].to_hex()]);
    git(dir.path(), &["branch", "a{b}c", &c[1].to_hex()]);
    let repo = open_repo(dir.path());
    assert_eq!(
        resolved(&repo, "foo{bar"),
        MergeSource {
            oid: c[1],
            kind: MergeSourceKind::LocalBranch
        }
    );
    let got = resolved(&repo, "foo{bar~1");
    assert_eq!((got.oid, got.kind), (c[0], MergeSourceKind::Commit));
    assert_eq!(got.oid, rev(&dir, "foo{bar~1"));
    for spec in [
        "foo{bar...main",
        "foo{bar..main",
        "main...foo{bar",
        "a{b}c...main",
        "foo{bar@{0}...main",
    ] {
        match resolve_merge_source(&repo, spec) {
            Err(Error::RevisionNotFound(s)) => assert_eq!(s, spec),
            other => panic!("{spec}: expected RevisionNotFound, got {other:?}"),
        }
    }
}

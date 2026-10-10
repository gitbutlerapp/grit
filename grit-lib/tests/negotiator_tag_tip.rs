//! Regression test: the fetch negotiator must accept an **annotated tag** as a
//! negotiation tip.
//!
//! `SkippingNegotiator::add_tip` / `known_common` receive raw ref-tip object ids.
//! An annotated tag ref (`refs/tags/v1`) points at a *tag* object, which has no
//! `author` header — feeding it straight to the commit parser failed with
//! "corrupt object: commit missing author header" and aborted the whole fetch
//! (observed over smart HTTP, whose negotiation did not pre-peel tips). The
//! negotiator now peels tag tips to their commit, so this must succeed.

use grit_lib::fetch_negotiator::SkippingNegotiator;
use grit_lib::objects::{ObjectId, ObjectKind};
use grit_lib::repo::{init_repository, Repository};

/// Write a raw object of `kind` and return its id.
fn write(repo: &Repository, kind: ObjectKind, data: &[u8]) -> ObjectId {
    repo.odb.write(kind, data).expect("write object")
}

#[test]
fn add_tip_accepts_annotated_tag() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let repo = init_repository(
        tmp.path(),
        true,
        "main",
        None,
        grit_lib::RefStorageFormat::Files,
    )
    .expect("init");

    // empty tree -> a commit -> an annotated tag pointing at the commit.
    let tree = write(&repo, ObjectKind::Tree, &[]);
    let commit_bytes = format!(
        "tree {tree}\n\
         author A U Thor <a@example.com> 0 +0000\n\
         committer A U Thor <a@example.com> 0 +0000\n\
         \n\
         a commit\n"
    );
    let commit = write(&repo, ObjectKind::Commit, commit_bytes.as_bytes());
    let tag_bytes = format!(
        "object {commit}\n\
         type commit\n\
         tag v1\n\
         tagger A U Thor <a@example.com> 0 +0000\n\
         \n\
         annotated v1\n"
    );
    let tag = write(&repo, ObjectKind::Tag, tag_bytes.as_bytes());

    // Re-open the repo so the negotiator reads through a fresh odb.
    let repo = Repository::open(&repo.git_dir, None).expect("open");
    let mut neg = SkippingNegotiator::new(repo);

    // Both entry points previously panicked the fetch on a tag tip.
    neg.add_tip(tag)
        .expect("add_tip must accept an annotated tag");
    neg.known_common(tag)
        .expect("known_common must accept an annotated tag");

    // The peeled commit is what the negotiator offers as a `have`.
    let have = neg.next_have().expect("next_have").expect("a have");
    assert_eq!(have, commit, "tag tip should peel to its target commit");

    // A plain commit tip still works, and a ref to a non-commit (a tree) is
    // skipped rather than erroring.
    let mut neg = SkippingNegotiator::new(Repository::open(&tmp.path(), None).expect("open"));
    neg.add_tip(commit).expect("commit tip ok");
    neg.add_tip(tree)
        .expect("tree tip must be skipped, not error");
}

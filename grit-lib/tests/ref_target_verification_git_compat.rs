//! Ref updates `git update-ref` refuses (a missing object, a branch or `HEAD` at a
//! non-commit) are refused by grit too, and leave a repository `git fsck` reads cleanly
//! (#2176). Updates git accepts round-trip in both directions.

use crate::support;

use grit_lib::config::environment::RepositoryOptions;
use grit_lib::error::Error;
use grit_lib::ident::Signature;
use grit_lib::objects::{NewCommit, ObjectId, ObjectKind, TreeEntry};
use grit_lib::refs::store::{
    Expected, RawRef, RefName, RefStore, RefStoreError, RefTransaction, RefUpdate, RefUpdateFlags,
};
use grit_lib::repo::Repository;

use support::{
    assert_fsck_when_git_interop, each_backend, empty_commit_oid, git, git_for_each_ref,
    git_interop_available, git_ok, TestRepo,
};

const BOGUS: &str = "1111111111111111111111111111111111111111";

fn open(repo: &TestRepo) -> Repository {
    Repository::discover(Some(repo.worktree()), &RepositoryOptions::empty()).expect("open")
}

fn bogus() -> ObjectId {
    ObjectId::from_hex(BOGUS).expect("oid")
}

fn put(name: &str, oid: ObjectId) -> RefUpdate {
    RefUpdate {
        name: name.to_owned(),
        new_value: Some(RawRef::Direct(oid)),
        expected: Expected::Any,
        reflog: None,
        flags: RefUpdateFlags::default(),
    }
}

fn commit_one(store: &dyn RefStore, update: RefUpdate) -> Result<(), RefStoreError> {
    store
        .prepare(RefTransaction::new().update(update)?)?
        .commit()
}

fn symref(name: &str, target: &str) -> RefUpdate {
    RefUpdate {
        name: name.to_owned(),
        new_value: Some(RawRef::Symbolic(target.to_owned())),
        expected: Expected::Any,
        reflog: None,
        flags: RefUpdateFlags::default(),
    }
}

fn head_tree(repo: &TestRepo) -> ObjectId {
    git(repo.worktree(), &["rev-parse", "HEAD^{tree}"])
        .trim()
        .parse()
        .expect("tree oid")
}

fn signature() -> Signature {
    "Ref Verify <verify@example.com> 1700000000 +0000"
        .parse()
        .expect("signature")
}

#[test]
fn issue_2176_reproduction_is_refused_and_git_fsck_stays_clean() {
    each_backend(|backend, repo| {
        let commit = empty_commit_oid(repo);
        if !git_interop_available(backend) {
            return;
        }
        let tree = head_tree(repo);
        let grit = open(repo);

        assert_eq!(
            commit_one(grit.refs(), put("refs/heads/bogus", bogus())),
            Err(RefStoreError::MissingObject {
                name: "refs/heads/bogus".to_owned(),
                oid: bogus(),
            }),
            "{backend:?}"
        );
        let to_tree = grit.references().update(
            &RefName::try_from("refs/heads/totree").expect("name"),
            &tree,
        );
        assert!(
            matches!(
                &to_tree,
                Err(Error::RefStore(RefStoreError::NotACommit {
                    kind: ObjectKind::Tree,
                    ..
                }))
            ),
            "{backend:?}: {to_tree:?}"
        );
        let missing = grit.references().update(
            &RefName::try_from("refs/heads/bogus2").expect("name"),
            &bogus(),
        );
        assert!(
            matches!(
                &missing,
                Err(Error::RefStore(RefStoreError::MissingObject { .. }))
            ),
            "{backend:?}: {missing:?}"
        );

        // Git refuses the same two updates.
        assert!(!git_ok(
            repo.worktree(),
            &["update-ref", "refs/heads/gitbogus", BOGUS]
        ));
        assert!(!git_ok(
            repo.worktree(),
            &["update-ref", "refs/heads/gt", &tree.to_hex()]
        ));

        assert_eq!(
            git_for_each_ref(repo.worktree(), "refs/"),
            vec![("refs/heads/main".to_owned(), commit)],
            "{backend:?}"
        );
        let fsck = git(repo.worktree(), &["fsck", "--no-progress"]);
        assert!(!fsck.contains("invalid sha1 pointer"), "{fsck}");
        assert!(!fsck.contains("not a commit"), "{fsck}");
        assert_fsck_when_git_interop(repo);
    });
}

#[test]
fn non_branch_refs_at_a_tree_round_trip_with_git() {
    each_backend(|backend, repo| {
        let commit = empty_commit_oid(repo);
        if !git_interop_available(backend) {
            return;
        }
        let tree = head_tree(repo);
        let grit = open(repo);
        for name in ["refs/tags/grit-tree", "refs/remotes/origin/grit-tree"] {
            commit_one(grit.refs(), put(name, tree))
                .unwrap_or_else(|err| panic!("{backend:?} {name}: {err}"));
        }
        for name in ["refs/tags/git-tree", "refs/remotes/origin/git-tree"] {
            git(repo.worktree(), &["update-ref", name, &tree.to_hex()]);
            assert_eq!(
                grit.refs().read_raw(name).expect("read"),
                Some(RawRef::Direct(tree)),
                "{backend:?} {name}"
            );
        }
        // A detached HEAD at a commit is accepted by both.
        let mut detach = put("HEAD", commit);
        detach.flags.no_deref = true;
        commit_one(grit.refs(), detach).expect("detach HEAD at a commit");
        assert_eq!(
            git(repo.worktree(), &["rev-parse", "HEAD"]).trim(),
            commit.to_hex()
        );

        assert_eq!(
            git_for_each_ref(repo.worktree(), "refs/"),
            vec![
                ("refs/heads/main".to_owned(), commit),
                ("refs/remotes/origin/git-tree".to_owned(), tree),
                ("refs/remotes/origin/grit-tree".to_owned(), tree),
                ("refs/tags/git-tree".to_owned(), tree),
                ("refs/tags/grit-tree".to_owned(), tree),
            ],
            "{backend:?}"
        );
        assert_fsck_when_git_interop(repo);
    });
}

#[test]
fn ref_update_after_git_prune_removed_the_commit_is_refused() {
    each_backend(|backend, repo| {
        let base = empty_commit_oid(repo);
        if !git_interop_available(backend) {
            return;
        }
        let grit = open(repo);
        let blob = grit
            .objects()
            .write(ObjectKind::Blob, b"racing writer\n")
            .expect("blob");
        let tree = grit
            .objects()
            .write_tree([TreeEntry {
                mode: 0o100644,
                name: b"f".to_vec(),
                oid: blob,
            }])
            .expect("tree");
        let next = grit
            .objects()
            .write_commit(
                &NewCommit::new(tree, signature(), signature(), "next").with_parents(vec![base]),
            )
            .expect("commit");

        // A concurrent prune removes the not-yet-referenced commit before the ref update.
        git(repo.worktree(), &["prune", "--expire=now"]);
        assert!(!git_ok(
            repo.worktree(),
            &["cat-file", "-e", &next.to_hex()]
        ));

        let mut cas = put("refs/heads/main", next);
        cas.expected = Expected::Oid(base);
        assert_eq!(
            commit_one(grit.refs(), cas),
            Err(RefStoreError::MissingObject {
                name: "refs/heads/main".to_owned(),
                oid: next,
            }),
            "{backend:?}"
        );
        assert_eq!(
            git(repo.worktree(), &["rev-parse", "refs/heads/main"]).trim(),
            base.to_hex()
        );
        assert_fsck_when_git_interop(repo);
    });
}

#[test]
fn skip_oid_verification_writes_the_dangling_ref_git_fsck_reports() {
    each_backend(|backend, repo| {
        empty_commit_oid(repo);
        if !git_interop_available(backend) {
            return;
        }
        let grit = open(repo);
        let mut dangling = put("refs/heads/dangling", bogus());
        dangling.flags.skip_oid_verification = true;
        commit_one(grit.refs(), dangling).expect("deliberately dangling ref");
        assert!(
            !git_ok(repo.worktree(), &["fsck", "--no-progress"]),
            "{backend:?}: git fsck must flag the dangling ref"
        );
    });
}

#[test]
fn issue_2258_invalid_symref_targets_leave_git_status_and_fsck_clean() {
    each_backend(|backend, repo| {
        empty_commit_oid(repo);
        if !git_interop_available(backend) {
            return;
        }
        let grit = open(repo);
        for (name, target) in [
            ("HEAD", "foo"),
            ("refs/heads/sym1", "foo"),
            ("refs/heads/loop", "refs/heads/loop"),
            ("HEAD", "refs/heads/a..b"),
        ] {
            assert!(
                commit_one(grit.refs(), symref(name, target)).is_err(),
                "{backend:?} {name} -> {target}"
            );
        }
        assert!(git_ok(repo.worktree(), &["status", "--porcelain"]));
        assert!(git_ok(repo.worktree(), &["fsck", "--no-progress"]));
        assert_fsck_when_git_interop(repo);
    });
}

#[test]
fn branch_commit_writer_to_a_missing_object_is_refused_and_git_fsck_stays_clean() {
    each_backend(|backend, repo| {
        let commit = empty_commit_oid(repo);
        if !git_interop_available(backend) {
            return;
        }
        let grit = open(repo);
        let err = grit_lib::refs::update_branch_for_commit_on_store(
            grit.refs(),
            &grit_lib::refs::BranchCommitRefUpdate {
                branch_ref: "refs/heads/main",
                expected_old: Some(commit),
                new_oid: bogus(),
                identity: "Ref Verify <verify@example.com> 1700000000 +0000",
                reflog_message: "commit: bogus",
            },
            &grit_lib::config::ConfigSet::new(),
        )
        .expect_err("missing tip");
        assert!(
            matches!(&err, Error::RefStore(RefStoreError::MissingObject { .. })),
            "{backend:?}: {err:?}"
        );
        // Git refuses the equivalent update too.
        assert!(!git_ok(
            repo.worktree(),
            &["update-ref", "refs/heads/main", BOGUS]
        ));
        assert_eq!(
            git(repo.worktree(), &["rev-parse", "HEAD"]).trim(),
            commit.to_hex()
        );
        assert!(git_ok(repo.worktree(), &["fsck", "--no-progress"]));
        assert_fsck_when_git_interop(repo);
    });
}

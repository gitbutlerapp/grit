//! Ref writes through a [`Repository`] refuse direct values that name a missing object, and
//! branches (or `HEAD`) that would point at anything but a commit (#2176). The checks run in
//! `prepare` before any lock, so a refused batch leaves every ref unchanged.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use grit_lib::config::environment::RepositoryOptions;
use grit_lib::config::ConfigSet;
use grit_lib::error::Error;
use grit_lib::ident::Signature;
use grit_lib::objects::{NewCommit, ObjectId, ObjectKind, TreeEntry};
use grit_lib::refs::ref_storage::RefStorageFormat;
use grit_lib::refs::reflog::read_reflog;
use grit_lib::refs::store::{
    Expected, MemoryRefStore, RawRef, RefName, RefStore, RefStoreError, RefTransaction, RefUpdate,
    RefUpdateFlags, ReflogUpdate,
};
use grit_lib::refs::{
    commit_ref_update_on_store, update_branch_for_commit, update_branch_for_commit_on_store,
    BranchCommitRefUpdate,
};
use grit_lib::repo::{InitOptions, Repository, RepositoryBuilder};

const BOGUS: &str = "1111111111111111111111111111111111111111";

struct Fixture {
    _root: tempfile::TempDir,
    worktree: PathBuf,
    repo: Repository,
    commit: ObjectId,
    tree: ObjectId,
}

impl Fixture {
    fn new(format: RefStorageFormat) -> Self {
        let root = tempfile::tempdir().expect("tempdir");
        let worktree = root.path().to_path_buf();
        Repository::init(
            &worktree,
            &InitOptions::new()
                .with_initial_branch("main".to_owned())
                .with_ref_storage(format),
        )
        .expect("init");
        let repo = open(&worktree);
        let (commit, tree) = write_commit(&repo, "seed", b"seed\n");
        repo.references()
            .update(&name("refs/heads/main"), &commit)
            .expect("seed main");
        Self {
            _root: root,
            worktree,
            repo,
            commit,
            tree,
        }
    }

    fn raw(&self, refname: &str) -> Option<RawRef> {
        self.repo.refs().read_raw(refname).expect("read ref")
    }
}

fn open(worktree: &Path) -> Repository {
    Repository::discover(Some(worktree), &RepositoryOptions::empty()).expect("open repository")
}

fn each_format(f: impl Fn(RefStorageFormat, &Fixture)) {
    for format in [RefStorageFormat::Files, RefStorageFormat::Reftable] {
        f(format, &Fixture::new(format));
    }
}

fn name(refname: &str) -> RefName {
    RefName::try_from(refname).expect("valid ref name")
}

fn bogus() -> ObjectId {
    ObjectId::from_hex(BOGUS).expect("oid")
}

fn signature() -> Signature {
    "Ref Verify <verify@example.com> 1700000000 +0000"
        .parse()
        .expect("signature")
}

/// Write a blob, a one-entry tree holding it, and a root commit of that tree.
fn write_commit(repo: &Repository, message: &str, content: &[u8]) -> (ObjectId, ObjectId) {
    let blob = repo
        .objects()
        .write(ObjectKind::Blob, content)
        .expect("blob");
    let tree = repo
        .objects()
        .write_tree([TreeEntry {
            mode: 0o100644,
            name: b"file".to_vec(),
            oid: blob,
        }])
        .expect("tree");
    let commit = repo
        .objects()
        .write_commit(&NewCommit::new(tree, signature(), signature(), message))
        .expect("commit");
    (commit, tree)
}

fn put(refname: &str, oid: ObjectId) -> RefUpdate {
    RefUpdate {
        name: refname.to_owned(),
        new_value: Some(RawRef::Direct(oid)),
        expected: Expected::Any,
        reflog: None,
        flags: RefUpdateFlags::default(),
    }
}

fn commit_updates(store: &dyn RefStore, updates: Vec<RefUpdate>) -> Result<(), RefStoreError> {
    let txn = updates
        .into_iter()
        .try_fold(RefTransaction::new(), RefTransaction::update)?;
    store.prepare(txn)?.commit()
}

fn missing(refname: &str, oid: ObjectId) -> RefStoreError {
    RefStoreError::MissingObject {
        name: refname.to_owned(),
        oid,
    }
}

fn not_a_commit(refname: &str, oid: ObjectId, kind: ObjectKind) -> RefStoreError {
    RefStoreError::NotACommit {
        name: refname.to_owned(),
        oid,
        kind,
    }
}

fn loose_path(repo: &Repository, oid: &ObjectId) -> PathBuf {
    let hex = oid.to_hex();
    repo.git_dir()
        .join("objects")
        .join(&hex[..2])
        .join(&hex[2..])
}

#[test]
fn transaction_refuses_missing_objects_on_every_kind_of_ref() {
    each_format(|format, fx| {
        for refname in [
            "refs/heads/bogus",
            "refs/tags/bogus",
            "refs/remotes/o/bogus",
        ] {
            assert_eq!(
                commit_updates(fx.repo.refs(), vec![put(refname, bogus())]),
                Err(missing(refname, bogus())),
                "{format:?} {refname}"
            );
            assert_eq!(fx.raw(refname), None, "{format:?} {refname}");
        }

        let mut detach = put("HEAD", bogus());
        detach.flags.no_deref = true;
        assert_eq!(
            commit_updates(fx.repo.refs(), vec![detach]),
            Err(missing("HEAD", bogus())),
            "{format:?}"
        );
        assert_eq!(
            fx.raw("HEAD"),
            Some(RawRef::Symbolic("refs/heads/main".to_owned())),
            "{format:?}"
        );
        assert_eq!(fx.raw("refs/heads/main"), Some(RawRef::Direct(fx.commit)));
    });
}

#[test]
fn branches_and_head_must_point_at_commits() {
    each_format(|format, fx| {
        assert_eq!(
            commit_updates(fx.repo.refs(), vec![put("refs/heads/totree", fx.tree)]),
            Err(not_a_commit("refs/heads/totree", fx.tree, ObjectKind::Tree)),
            "{format:?}"
        );
        assert_eq!(fx.raw("refs/heads/totree"), None, "{format:?}");

        let mut detach = put("HEAD", fx.tree);
        detach.flags.no_deref = true;
        assert_eq!(
            commit_updates(fx.repo.refs(), vec![detach]),
            Err(not_a_commit("HEAD", fx.tree, ObjectKind::Tree)),
            "{format:?}"
        );

        // Through the symbolic HEAD the write would land on refs/heads/main.
        assert_eq!(
            commit_updates(fx.repo.refs(), vec![put("HEAD", fx.tree)]),
            Err(not_a_commit("HEAD", fx.tree, ObjectKind::Tree)),
            "{format:?}"
        );
        assert_eq!(
            fx.raw("refs/heads/main"),
            Some(RawRef::Direct(fx.commit)),
            "{format:?}"
        );
    });
}

#[test]
fn tags_and_remote_tracking_refs_may_point_at_any_object() {
    each_format(|format, fx| {
        commit_updates(
            fx.repo.refs(),
            vec![
                put("refs/tags/tree", fx.tree),
                put("refs/remotes/origin/tree", fx.tree),
                put("refs/notes-like/tree", fx.tree),
            ],
        )
        .unwrap_or_else(|err| panic!("{format:?}: {err}"));
        for refname in [
            "refs/tags/tree",
            "refs/remotes/origin/tree",
            "refs/notes-like/tree",
        ] {
            assert_eq!(fx.raw(refname), Some(RawRef::Direct(fx.tree)), "{format:?}");
        }
    });
}

#[test]
fn deletes_and_well_formed_symbolic_updates_commit_without_object_lookup() {
    each_format(|format, fx| {
        commit_updates(fx.repo.refs(), vec![put("refs/heads/gone", fx.commit)]).expect("seed");
        let delete = RefUpdate {
            name: "refs/heads/gone".to_owned(),
            new_value: None,
            expected: Expected::Oid(fx.commit),
            reflog: None,
            flags: RefUpdateFlags::default(),
        };
        let symbolic = RefUpdate {
            name: "refs/heads/alias".to_owned(),
            new_value: Some(RawRef::Symbolic("refs/heads/not-yet".to_owned())),
            expected: Expected::Any,
            reflog: None,
            flags: RefUpdateFlags {
                no_deref: true,
                ..RefUpdateFlags::default()
            },
        };
        commit_updates(fx.repo.refs(), vec![delete, symbolic])
            .unwrap_or_else(|err| panic!("{format:?}: {err}"));
        assert_eq!(fx.raw("refs/heads/gone"), None, "{format:?}");
        assert_eq!(
            fx.raw("refs/heads/alias"),
            Some(RawRef::Symbolic("refs/heads/not-yet".to_owned())),
            "{format:?}"
        );
    });
}

#[test]
fn log_only_updates_with_missing_reflog_objects_are_refused() {
    each_format(|format, fx| {
        let mut log_only = put("refs/heads/main", fx.commit);
        log_only.flags.log_only = true;
        log_only.reflog = Some(ReflogUpdate {
            identity: "Ref Verify <verify@example.com>".to_owned(),
            message: "log only".to_owned(),
            time: time::OffsetDateTime::UNIX_EPOCH,
            old_oid: Some(fx.commit),
            new_oid: Some(bogus()),
        });
        assert_eq!(
            commit_updates(fx.repo.refs(), vec![log_only]),
            Err(missing("refs/heads/main", bogus())),
            "{format:?}"
        );
        assert_eq!(
            fx.raw("refs/heads/main"),
            Some(RawRef::Direct(fx.commit)),
            "{format:?}"
        );
    });
}

#[test]
fn log_only_with_no_new_value_still_checks_explicit_reflog_objects() {
    each_format(|format, fx| {
        let git_dir = fx.repo.git_dir();
        let log_len = || read_reflog(git_dir, "refs/heads/main").map_or(0, |e| e.len());
        let before = log_len();
        let log_only = RefUpdate {
            name: "refs/heads/main".to_owned(),
            new_value: None,
            expected: Expected::Any,
            reflog: Some(ReflogUpdate {
                identity: "Ref Verify <verify@example.com>".to_owned(),
                message: "log only".to_owned(),
                time: time::OffsetDateTime::UNIX_EPOCH,
                old_oid: Some(fx.commit),
                new_oid: Some(bogus()),
            }),
            flags: RefUpdateFlags {
                log_only: true,
                ..RefUpdateFlags::default()
            },
        };
        let err = commit_ref_update_on_store(fx.repo.refs(), log_only).expect_err("{format:?}");
        assert!(
            matches!(&err, Error::RefStore(e) if *e == missing("refs/heads/main", bogus())),
            "{format:?}: {err:?}"
        );
        assert_eq!(
            fx.raw("refs/heads/main"),
            Some(RawRef::Direct(fx.commit)),
            "{format:?}"
        );
        assert_eq!(log_len(), before, "{format:?}: reflog must be unchanged");
    });
}

fn invalid_symref(name: &str, target: &str) -> RefStoreError {
    RefStoreError::InvalidSymrefTarget {
        name: name.to_owned(),
        target: target.to_owned(),
    }
}

fn symref_update(name: &str, target: &str) -> RefUpdate {
    RefUpdate {
        name: name.to_owned(),
        new_value: Some(RawRef::Symbolic(target.to_owned())),
        expected: Expected::Any,
        reflog: None,
        flags: RefUpdateFlags::default(),
    }
}

#[test]
fn issue_2258_invalid_symref_targets_are_refused_and_store_is_unchanged() {
    each_format(|format, fx| {
        let head_before = fx.raw("HEAD");
        let main_before = fx.raw("refs/heads/main");
        for (name, target) in [
            ("HEAD", "foo"),
            ("refs/heads/sym1", "foo"),
            ("refs/heads/loop", "refs/heads/loop"),
            ("HEAD", "refs/heads/a..b"),
        ] {
            let err = commit_ref_update_on_store(fx.repo.refs(), symref_update(name, target))
                .expect_err("{format:?} {name} -> {target}");
            assert!(
                matches!(&err, Error::RefStore(e) if *e == invalid_symref(name, target)),
                "{format:?} {name} -> {target}: {err:?}"
            );
            assert_eq!(fx.raw("HEAD"), head_before, "{format:?} {name}");
            assert_eq!(fx.raw("refs/heads/main"), main_before, "{format:?} {name}");
            if name != "HEAD" {
                assert_eq!(fx.raw(name), None, "{format:?} {name}");
            }
        }
    });
}

#[test]
fn null_id_is_still_reported_as_null_oid() {
    each_format(|format, fx| {
        assert_eq!(
            commit_updates(fx.repo.refs(), vec![put("refs/heads/z", ObjectId::zero())]),
            Err(RefStoreError::NullOid {
                name: "refs/heads/z".to_owned()
            }),
            "{format:?}"
        );
    });
}

#[test]
fn one_bad_update_writes_nothing_from_the_batch() {
    each_format(|format, fx| {
        let err = commit_updates(
            fx.repo.refs(),
            vec![
                put("refs/heads/a", fx.commit),
                put("refs/tags/t", fx.tree),
                put("refs/heads/b", bogus()),
                put("refs/heads/c", fx.commit),
            ],
        )
        .expect_err("batch with a missing object");
        assert_eq!(err, missing("refs/heads/b", bogus()), "{format:?}");
        for refname in [
            "refs/heads/a",
            "refs/tags/t",
            "refs/heads/b",
            "refs/heads/c",
        ] {
            assert_eq!(fx.raw(refname), None, "{format:?} {refname}");
        }
    });
}

#[test]
fn skip_oid_verification_writes_a_dangling_ref_on_purpose() {
    each_format(|format, fx| {
        let mut dangling = put("refs/heads/dangling", bogus());
        dangling.flags.skip_oid_verification = true;
        let mut to_tree = put("refs/heads/to-tree", fx.tree);
        to_tree.flags.skip_oid_verification = true;
        commit_updates(fx.repo.refs(), vec![dangling, to_tree])
            .unwrap_or_else(|err| panic!("{format:?}: {err}"));
        assert_eq!(
            fx.raw("refs/heads/dangling"),
            Some(RawRef::Direct(bogus())),
            "{format:?}"
        );
        assert_eq!(
            fx.raw("refs/heads/to-tree"),
            Some(RawRef::Direct(fx.tree)),
            "{format:?}"
        );
    });
}

#[test]
fn compare_and_swap_after_the_commit_was_pruned_is_refused() {
    each_format(|format, fx| {
        let (next, _) = write_commit(&fx.repo, "next", b"next\n");
        let loose = loose_path(&fx.repo, &next);
        assert!(loose.is_file(), "{format:?}: commit is loose");
        std::fs::remove_file(&loose).expect("prune the loose commit");

        let mut cas = put("refs/heads/main", next);
        cas.expected = Expected::Oid(fx.commit);
        assert_eq!(
            commit_updates(fx.repo.refs(), vec![cas]),
            Err(missing("refs/heads/main", next)),
            "{format:?}"
        );
        assert_eq!(
            fx.raw("refs/heads/main"),
            Some(RawRef::Direct(fx.commit)),
            "{format:?}"
        );
    });
}

#[test]
fn references_update_refuses_missing_objects_and_non_commit_branches() {
    each_format(|format, fx| {
        let refs = fx.repo.references();
        let err = refs
            .update(&name("refs/heads/totree"), &fx.tree)
            .expect_err("branch at a tree");
        assert!(
            matches!(
                &err,
                Error::RefStore(e) if *e == not_a_commit("refs/heads/totree", fx.tree, ObjectKind::Tree)
            ),
            "{format:?}: {err:?}"
        );
        let err = refs
            .update(&name("refs/heads/bogus2"), &bogus())
            .expect_err("branch at a missing object");
        assert!(
            matches!(&err, Error::RefStore(e) if *e == missing("refs/heads/bogus2", bogus())),
            "{format:?}: {err:?}"
        );
        refs.update(&name("refs/tags/tree"), &fx.tree)
            .expect("tag at a tree");
        assert_eq!(fx.raw("refs/heads/totree"), None, "{format:?}");
        assert_eq!(fx.raw("refs/heads/bogus2"), None, "{format:?}");
    });
}

#[test]
fn git_dir_writers_refuse_missing_objects() {
    each_format(|format, fx| {
        let git_dir = fx.repo.git_dir();
        let err = grit_lib::refs::write_ref(git_dir, "refs/heads/w", &bogus())
            .expect_err("write_ref to a missing object");
        assert!(
            matches!(&err, Error::RefStore(e) if *e == missing("refs/heads/w", bogus())),
            "{format:?}: {err:?}"
        );
        let err = grit_lib::refs::write_ref_cas(git_dir, "refs/heads/main", &fx.tree, fx.commit)
            .expect_err("write_ref_cas to a tree");
        assert!(
            matches!(
                &err,
                Error::RefStore(e) if *e == not_a_commit("refs/heads/main", fx.tree, ObjectKind::Tree)
            ),
            "{format:?}: {err:?}"
        );
        let err = grit_lib::refs::commit_ref_store_batch(
            git_dir,
            &[put("refs/heads/x", fx.commit), put("refs/heads/y", bogus())],
        )
        .expect_err("batch with a missing object");
        assert!(
            matches!(&err, Error::RefStore(e) if *e == missing("refs/heads/y", bogus())),
            "{format:?}: {err:?}"
        );
        for refname in ["refs/heads/w", "refs/heads/x", "refs/heads/y"] {
            assert_eq!(fx.raw(refname), None, "{format:?} {refname}");
        }
        assert_eq!(fx.raw("refs/heads/main"), Some(RawRef::Direct(fx.commit)));

        grit_lib::refs::write_ref(git_dir, "refs/tags/tree", &fx.tree).expect("tag at a tree");
        grit_lib::refs::commit_ref_store_batch(git_dir, &[put("refs/heads/x", fx.commit)])
            .expect("valid batch");
        assert_eq!(fx.raw("refs/heads/x"), Some(RawRef::Direct(fx.commit)));
    });
}

#[test]
fn empty_tree_must_be_stored_to_be_a_ref_target() {
    each_format(|format, fx| {
        let empty = ObjectId::from_hex("4b825dc642cb6eb9a060e54bf8d69288fbee4904").expect("oid");
        assert!(!loose_path(&fx.repo, &empty).exists(), "{format:?}");
        assert_eq!(
            commit_updates(fx.repo.refs(), vec![put("refs/tags/empty", empty)]),
            Err(missing("refs/tags/empty", empty)),
            "{format:?}"
        );

        fx.repo
            .objects()
            .write_literally(ObjectKind::Tree, b"")
            .expect("store the empty tree");
        assert!(loose_path(&fx.repo, &empty).is_file(), "{format:?}");
        commit_updates(fx.repo.refs(), vec![put("refs/tags/empty", empty)])
            .unwrap_or_else(|err| panic!("{format:?}: {err}"));
        assert_eq!(
            commit_updates(fx.repo.refs(), vec![put("refs/heads/empty", empty)]),
            Err(not_a_commit("refs/heads/empty", empty, ObjectKind::Tree)),
            "{format:?}"
        );
    });
}

#[test]
fn injected_ref_store_is_checked_against_the_repository_objects() {
    let fx = Fixture::new(RefStorageFormat::Files);
    let store = Arc::new(MemoryRefStore::new());
    let repo = RepositoryBuilder::new(RepositoryOptions::empty())
        .ref_store(store.clone())
        .open(&fx.worktree.join(".git"), Some(&fx.worktree))
        .expect("open with an injected store");
    assert_eq!(
        commit_updates(repo.refs(), vec![put("refs/heads/bogus", bogus())]),
        Err(missing("refs/heads/bogus", bogus()))
    );
    let err = repo
        .references()
        .update(&name("refs/heads/tree"), &fx.tree)
        .expect_err("branch at a tree");
    assert!(
        matches!(&err, Error::RefStore(e) if *e == not_a_commit("refs/heads/tree", fx.tree, ObjectKind::Tree)),
        "{err:?}"
    );
    repo.references()
        .update(&name("refs/heads/ok"), &fx.commit)
        .expect("branch at a commit");
    assert_eq!(store.read_raw("refs/heads/bogus").expect("read"), None);
    assert_eq!(store.read_raw("refs/heads/tree").expect("read"), None);
    assert_eq!(
        store.read_raw("refs/heads/ok").expect("read"),
        Some(RawRef::Direct(fx.commit))
    );

    // The raw store itself is the unchecked backend layer.
    commit_updates(store.as_ref(), vec![put("refs/heads/raw", bogus())])
        .expect("raw stores take synthetic ids");
}

fn branch_commit<'a>(expected_old: ObjectId, new_oid: ObjectId) -> BranchCommitRefUpdate<'a> {
    BranchCommitRefUpdate {
        branch_ref: "refs/heads/main",
        expected_old: Some(expected_old),
        new_oid,
        identity: "Ref Verify <verify@example.com> 1700000000 +0000",
        reflog_message: "commit: next",
    }
}

#[test]
fn branch_commit_writer_refuses_missing_and_non_commit_tips() {
    each_format(|format, fx| {
        let config = ConfigSet::new();
        let err = update_branch_for_commit_on_store(
            fx.repo.refs(),
            &branch_commit(fx.commit, bogus()),
            &config,
        )
        .expect_err("missing tip");
        assert!(
            matches!(&err, Error::RefStore(e) if *e == missing("refs/heads/main", bogus())),
            "{format:?}: {err:?}"
        );
        let err = update_branch_for_commit_on_store(
            fx.repo.refs(),
            &branch_commit(fx.commit, fx.tree),
            &config,
        )
        .expect_err("tree tip");
        assert!(
            matches!(
                &err,
                Error::RefStore(e) if *e == not_a_commit("refs/heads/main", fx.tree, ObjectKind::Tree)
            ),
            "{format:?}: {err:?}"
        );
        let err = update_branch_for_commit(fx.repo.git_dir(), &branch_commit(fx.commit, bogus()))
            .expect_err("path-based writer, missing tip");
        assert!(
            matches!(&err, Error::RefStore(e) if *e == missing("refs/heads/main", bogus())),
            "{format:?}: {err:?}"
        );
        assert_eq!(
            fx.raw("refs/heads/main"),
            Some(RawRef::Direct(fx.commit)),
            "{format:?}"
        );
        assert_eq!(
            fx.raw("HEAD"),
            Some(RawRef::Symbolic("refs/heads/main".to_owned())),
            "{format:?}"
        );
        assert!(
            !fx.repo.git_dir().join("refs/heads/main.lock").exists(),
            "{format:?}: the check runs before the lock"
        );

        let (next, _) = write_commit(&fx.repo, "next", b"next\n");
        update_branch_for_commit_on_store(fx.repo.refs(), &branch_commit(fx.commit, next), &config)
            .unwrap_or_else(|err| panic!("{format:?}: {err}"));
        assert_eq!(
            fx.raw("refs/heads/main"),
            Some(RawRef::Direct(next)),
            "{format:?}"
        );
    });
}

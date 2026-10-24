//! Updates every ref backend must refuse at `prepare` leave a repository system `git` reads
//! cleanly (#1738): unsafe ref names and the null object id as a direct value.

mod support;

use grit_lib::objects::ObjectId;
use grit_lib::refs::store::{
    open_ref_store, Expected, RawRef, RefStore, RefStoreError, RefTransaction, RefUpdate,
    RefUpdateFlags,
};

use support::{
    assert_fsck_when_git_interop, each_backend, empty_commit_oid, git, git_for_each_ref,
    git_interop_available,
};

fn put(store: &dyn RefStore, name: &str, value: RawRef) -> Result<(), RefStoreError> {
    let txn = RefTransaction::new().update(RefUpdate {
        name: name.to_owned(),
        new_value: Some(value),
        expected: Expected::Any,
        reflog: None,
        flags: RefUpdateFlags::default(),
    })?;
    store.prepare(txn)?.commit()
}

#[test]
fn rejected_updates_keep_repository_clean_for_git() {
    each_backend(|backend, repo| {
        let commit = empty_commit_oid(repo);
        let git_dir = repo.worktree().join(".git");
        let store = open_ref_store(&git_dir).expect("open ref store");

        assert_eq!(
            put(
                store.as_ref(),
                "refs/heads/z",
                RawRef::Direct(ObjectId::zero())
            ),
            Err(RefStoreError::NullOid {
                name: "refs/heads/z".to_owned()
            }),
            "{backend:?}"
        );
        for name in [
            "refs/heads/../x",
            "refs/heads/a b",
            "refs/heads/x.lock",
            "refs/heads/x/",
            "refs/heads//x",
            "",
            "heads/x",
        ] {
            assert_eq!(
                put(store.as_ref(), name, RawRef::Direct(commit)),
                Err(RefStoreError::InvalidRefName {
                    name: name.to_owned()
                }),
                "{backend:?} {name:?}"
            );
        }

        put(store.as_ref(), "refs/heads/kept", RawRef::Direct(commit)).expect("valid update");
        assert_eq!(
            store.read_raw("refs/heads/z").expect("read z"),
            None,
            "{backend:?}"
        );

        if !git_interop_available(backend) {
            return;
        }
        assert_eq!(
            git_for_each_ref(repo.worktree(), "refs/"),
            vec![
                ("refs/heads/kept".to_owned(), commit),
                ("refs/heads/main".to_owned(), commit),
            ],
            "{backend:?}"
        );
        let fsck = git(repo.worktree(), &["fsck", "--strict", "--no-progress"]);
        assert!(!fsck.contains("invalid sha1 pointer"), "{fsck}");
        assert_fsck_when_git_interop(repo);
    });
}

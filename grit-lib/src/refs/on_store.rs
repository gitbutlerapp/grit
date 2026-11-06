//! Ref and reflog writes through a caller-selected [`RefStore`].
//!
//! Porcelain receives its store from [`crate::repo::Repository::refs`], which may be an
//! injected backend such as [`crate::refs::store::MemoryRefStore`]. These helpers route every write through
//! that store. The files backend keeps its lock-and-append implementation (the generic
//! transaction rewrites whole reflog files); every other backend gets one atomic
//! [`crate::refs::store::RefTransaction`] with compare-and-swap expectations and reflog entries.

use crate::config::ConfigSet;
use crate::error::{Error, Result};
use crate::lockfile::LockWait;
use crate::objects::ObjectId;

use super::store::reference_transaction::{
    branch_commit_reference_transaction_lines, detached_head_commit_reference_transaction_lines,
};
use super::store::{Expected, RawRef, RefStore, RefUpdate, RefUpdateFlags, ReflogUpdate};
use super::{
    append_reflog_with_config, commit_ref_update_on_store, commit_ref_update_on_store_with_wait,
    commit_ref_updates_on_store_with_wait, ensure_refname_safe_for_storage,
    should_autocreate_reflog_for_mode, store_write_value_on_store, BranchCommitRefUpdate,
};

/// Whether an update to `refname` should carry a reflog entry: auto-created under
/// `core.logAllRefUpdates`, or appended because the ref already has a reflog.
pub(crate) fn reflog_wanted(
    store: &dyn RefStore,
    config: &ConfigSet,
    refname: &str,
) -> Result<bool> {
    if should_autocreate_reflog_for_mode(refname, config.effective_log_refs_config()) {
        return Ok(true);
    }
    store.reflog_exists(refname)
}

/// Reflog entry with explicit old/new ids. `identity` is a full `Name <email> <epoch> <tz>`
/// line, so the entry's `time` is never consulted.
fn explicit_reflog(identity: &str, message: &str, old: ObjectId, new: ObjectId) -> ReflogUpdate {
    ReflogUpdate {
        identity: identity.to_owned(),
        message: message.to_owned(),
        time: time::OffsetDateTime::UNIX_EPOCH,
        old_oid: Some(old),
        new_oid: Some(new),
    }
}

fn expected_tip(expected_old: Option<ObjectId>) -> Expected {
    expected_old.map_or(Expected::Missing, Expected::Oid)
}

/// Run `store`'s object check ([`RefStore::verify_update_targets`]) for pointing `refname` at
/// `new_oid`, for the files-backend paths that write without a transaction.
pub(crate) fn verify_new_tip(store: &dyn RefStore, refname: &str, new_oid: ObjectId) -> Result<()> {
    store.verify_update_targets(&[RefUpdate {
        name: refname.to_owned(),
        new_value: Some(RawRef::Direct(new_oid)),
        expected: Expected::Any,
        reflog: None,
        flags: RefUpdateFlags::default(),
    }])?;
    Ok(())
}

/// Move the branch `HEAD` points at to a new commit and append the branch and `HEAD` reflogs,
/// all through `store`.
///
/// `update.identity` must be a full `Name <email> <epoch> <tz>` line. The branch moves only
/// when it still has `update.expected_old` (`None`: it must not exist yet). Reflog entries are
/// written when `core.logAllRefUpdates` (read from `config`) allows auto-creating them or the
/// ref already has a reflog. On the files backend the branch lock is held from the
/// compare-and-swap through the reflog appends and the ref write; on every other backend the
/// branch update and both reflog entries commit as one transaction, waiting up to
/// `core.filesRefLockTimeout` (from `config`) while another writer holds the store's lock.
///
/// # Errors
///
/// - [`Error::Message`] when `HEAD` is not a symbolic ref to `update.branch_ref`.
/// - [`Error::RefStore`] with [`crate::refs::store::RefStoreError::ExpectedMismatch`] when the branch no
///   longer has `update.expected_old`.
/// - [`Error::RefStore`] with [`crate::refs::store::RefStoreError::MissingObject`] or
///   [`crate::refs::store::RefStoreError::NotACommit`] when `store` is the store of
///   [`crate::repo::Repository::refs`] and `update.new_oid` is not a commit the repository
///   stores (checked before any lock, on every backend).
/// - Ref, reflog and I/O failures from the backend; the branch is not advanced when they occur.
pub fn update_branch_for_commit_on_store(
    store: &dyn RefStore,
    update: &BranchCommitRefUpdate<'_>,
    config: &ConfigSet,
) -> Result<()> {
    match store.read_raw("HEAD")? {
        Some(RawRef::Symbolic(target)) if target == update.branch_ref => {}
        _ => {
            return Err(Error::Message(format!(
                "HEAD does not point at branch '{}'",
                update.branch_ref
            )))
        }
    }
    let null_oid = store.reference_transaction_null_oid();
    let reflog_old = update.expected_old.unwrap_or(null_oid);
    if let Some(files) = store.as_files() {
        verify_new_tip(store, update.branch_ref, update.new_oid)?;
        if let Some(ctx) = store.ref_hook_context() {
            let hook_old = update.expected_old.unwrap_or_else(|| ctx.null_oid());
            let lines = branch_commit_reference_transaction_lines(
                update.branch_ref,
                hook_old,
                update.new_oid,
            );
            return super::update_branch_for_commit_files_with_reference_transaction_hooks(
                files.git_dir(),
                update,
                &reflog_old,
                config,
                ctx,
                &lines,
            );
        }
        return super::update_branch_for_commit_files(files.git_dir(), update, &reflog_old, config);
    }

    ensure_refname_safe_for_storage(update.branch_ref)?;
    let reflog = || {
        explicit_reflog(
            update.identity,
            update.reflog_message,
            reflog_old,
            update.new_oid,
        )
    };
    let mut updates = Vec::with_capacity(2);
    updates.push(RefUpdate {
        name: update.branch_ref.to_owned(),
        new_value: Some(RawRef::Direct(update.new_oid)),
        expected: expected_tip(update.expected_old),
        reflog: reflog_wanted(store, config, update.branch_ref)?.then(reflog),
        flags: RefUpdateFlags::default(),
    });
    if reflog_wanted(store, config, "HEAD")? {
        updates.push(RefUpdate {
            name: "HEAD".to_owned(),
            new_value: Some(RawRef::Direct(update.new_oid)),
            expected: Expected::Any,
            reflog: Some(reflog()),
            flags: RefUpdateFlags {
                no_deref: true,
                log_only: true,
                ..Default::default()
            },
        });
    }
    commit_ref_updates_on_store_with_wait(store, &updates, || {
        LockWait::files_ref_lock(Some(config))
    })
}

/// Advance detached `HEAD` to `new_oid` through `store`, with compare-and-swap on
/// `expected_old` and an optional `HEAD` reflog entry (see
/// [`update_branch_for_commit_on_store`] for the reflog policy and `identity` format).
///
/// # Errors
///
/// Returns [`Error::Message`] when `HEAD` is symbolic,
/// [`crate::refs::store::RefStoreError::ExpectedMismatch`] when `HEAD` moved, or a ref/reflog/I/O failure.
pub(crate) fn update_detached_head_for_commit_on_store(
    store: &dyn RefStore,
    expected_old: Option<ObjectId>,
    new_oid: ObjectId,
    identity: &str,
    reflog_message: &str,
    config: &ConfigSet,
) -> Result<()> {
    if matches!(store.read_raw("HEAD")?, Some(RawRef::Symbolic(_))) {
        return Err(Error::Message(
            "HEAD points at a branch; use update_branch_for_commit".into(),
        ));
    }
    if let Some(files) = store.as_files() {
        verify_new_tip(store, "HEAD", new_oid)?;
        if let Some(ctx) = store.ref_hook_context() {
            let hook_old = expected_old.unwrap_or_else(|| ctx.null_oid());
            let lines = detached_head_commit_reference_transaction_lines(hook_old, new_oid);
            return super::update_detached_head_for_commit_files_with_reference_transaction_hooks(
                files.git_dir(),
                expected_old,
                new_oid,
                identity,
                reflog_message,
                config,
                ctx,
                &lines,
            );
        }
        return super::update_detached_head_for_commit_files(
            files.git_dir(),
            expected_old,
            new_oid,
            identity,
            reflog_message,
            config,
        );
    }
    let reflog_old = expected_old.unwrap_or_else(|| store.reference_transaction_null_oid());
    let reflog = reflog_wanted(store, config, "HEAD")?
        .then(|| explicit_reflog(identity, reflog_message, reflog_old, new_oid));
    commit_ref_update_on_store_with_wait(
        store,
        RefUpdate {
            name: "HEAD".to_owned(),
            new_value: Some(RawRef::Direct(new_oid)),
            expected: expected_tip(expected_old),
            reflog,
            flags: RefUpdateFlags {
                no_deref: true,
                log_only: false,
                ..Default::default()
            },
        },
        || LockWait::files_ref_lock(Some(config)),
    )
}

/// Append one `old -> new` entry to the reflog of `refname` in `store` without moving the ref.
///
/// The entry is written when `force_create` is set, when `core.logAllRefUpdates` (from
/// `config`) allows auto-creating the reflog, or when the reflog already exists.
/// `identity` is a full `Name <email> <epoch> <tz>` line.
///
/// # Errors
///
/// Propagates invalid refname, backend and I/O failures.
#[allow(clippy::too_many_arguments)]
pub(crate) fn append_reflog_on_store(
    store: &dyn RefStore,
    refname: &str,
    old_oid: &ObjectId,
    new_oid: &ObjectId,
    identity: &str,
    message: &str,
    force_create: bool,
    config: &ConfigSet,
) -> Result<()> {
    if let Some(files) = store.as_files() {
        return append_reflog_with_config(
            files.git_dir(),
            refname,
            old_oid,
            new_oid,
            identity,
            message,
            force_create,
            Some(config),
        );
    }
    ensure_refname_safe_for_storage(refname)?;
    if !force_create && !reflog_wanted(store, config, refname)? {
        return Ok(());
    }
    commit_ref_update_on_store_with_wait(
        store,
        RefUpdate {
            name: refname.to_owned(),
            new_value: Some(RawRef::Direct(*new_oid)),
            expected: Expected::Any,
            reflog: Some(explicit_reflog(identity, message, *old_oid, *new_oid)),
            flags: RefUpdateFlags {
                no_deref: true,
                log_only: true,
                ..Default::default()
            },
        },
        || LockWait::files_ref_lock(Some(config)),
    )
}

/// Point `refname` (usually `HEAD`) symbolically at `target` through `store`.
///
/// `git_dir` is only used for the files backend's loose-ref checks.
///
/// # Errors
///
/// Propagates invalid refname, backend and I/O failures.
pub(crate) fn write_symbolic_ref_on_store(
    store: &dyn RefStore,
    git_dir: &std::path::Path,
    refname: &str,
    target: &str,
) -> Result<()> {
    store_write_value_on_store(
        store,
        git_dir,
        refname,
        Some(RawRef::Symbolic(target.to_owned())),
        Expected::Any,
        None,
    )
}

/// Delete `refname` itself (not a symbolic ref's target) and its reflog from `store`.
///
/// # Errors
///
/// Fails when the ref does not exist; propagates invalid refname, backend and I/O failures.
pub(crate) fn delete_ref_on_store(store: &dyn RefStore, refname: &str) -> Result<()> {
    if let Some(files) = store.as_files() {
        if store.ref_hook_context().is_none() {
            return super::delete_ref(files.git_dir(), refname);
        }
    }
    ensure_refname_safe_for_storage(refname)?;
    commit_ref_update_on_store(
        store,
        RefUpdate {
            name: refname.to_owned(),
            new_value: None,
            expected: Expected::Exists,
            reflog: None,
            flags: RefUpdateFlags {
                no_deref: true,
                log_only: false,
                ..Default::default()
            },
        },
    )?;
    store.delete_reflog(refname)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used, clippy::unwrap_used)]

    use std::ops::ControlFlow;
    use std::path::Path;

    use super::*;
    use crate::config::{ConfigFile, ConfigScope};
    use crate::diff::zero_oid;
    use crate::refs::store::{MemoryRefStore, RefStoreError};

    const IDENT: &str = "T <t@e.x> 1700000000 +0000";

    fn oid(byte: u8) -> ObjectId {
        let mut bytes = [0u8; 20];
        bytes[19] = byte;
        ObjectId::from_bytes(&bytes).expect("oid")
    }

    fn config(log_all_ref_updates: Option<&str>) -> ConfigSet {
        let text = log_all_ref_updates
            .map(|value| format!("[core]\n\tlogAllRefUpdates = {value}\n"))
            .unwrap_or_default();
        let file =
            ConfigFile::parse(Path::new("config"), &text, ConfigScope::Local).expect("parse");
        let mut set = ConfigSet::new();
        set.merge(&file);
        set
    }

    fn store_on(branch: &str, tip: Option<ObjectId>) -> MemoryRefStore {
        let store = MemoryRefStore::new();
        store
            .set_ref("HEAD", RawRef::Symbolic(branch.to_owned()))
            .expect("HEAD");
        if let Some(tip) = tip {
            store.set_ref(branch, RawRef::Direct(tip)).expect("tip");
        }
        store
    }

    fn log(store: &dyn RefStore, name: &str) -> Vec<(ObjectId, ObjectId, String, String)> {
        let mut out = Vec::new();
        store
            .for_each_reflog_entry(name, false, &mut |e| {
                out.push((e.old_oid, e.new_oid, e.identity.clone(), e.message.clone()));
                ControlFlow::Continue(())
            })
            .expect("reflog");
        out
    }

    fn update<'a>(expected_old: Option<ObjectId>, new_oid: ObjectId) -> BranchCommitRefUpdate<'a> {
        BranchCommitRefUpdate {
            branch_ref: "refs/heads/main",
            expected_old,
            new_oid,
            identity: IDENT,
            reflog_message: "commit: two",
        }
    }

    #[test]
    fn branch_commit_moves_branch_and_logs_branch_and_head_in_the_store() {
        let store = store_on("refs/heads/main", Some(oid(1)));
        update_branch_for_commit_on_store(&store, &update(Some(oid(1)), oid(2)), &config(None))
            .expect("update");

        assert_eq!(
            store.read_raw("refs/heads/main").expect("read"),
            Some(RawRef::Direct(oid(2)))
        );
        assert_eq!(
            store.read_raw("HEAD").expect("read"),
            Some(RawRef::Symbolic("refs/heads/main".into()))
        );
        let expected = vec![(oid(1), oid(2), IDENT.to_owned(), "commit: two".to_owned())];
        assert_eq!(log(&store, "refs/heads/main"), expected);
        assert_eq!(log(&store, "HEAD"), expected);
    }

    #[test]
    fn unborn_branch_commit_creates_the_branch_with_a_null_old_id() {
        let store = store_on("refs/heads/main", None);
        update_branch_for_commit_on_store(&store, &update(None, oid(1)), &config(None))
            .expect("update");
        assert_eq!(store.resolve("HEAD").expect("HEAD"), oid(1));
        assert_eq!(log(&store, "refs/heads/main")[0].0, zero_oid());
    }

    #[test]
    fn stale_expected_tip_is_a_typed_mismatch_and_changes_nothing() {
        let store = store_on("refs/heads/main", Some(oid(3)));
        let err =
            update_branch_for_commit_on_store(&store, &update(Some(oid(1)), oid(2)), &config(None))
                .expect_err("stale tip");
        assert!(matches!(
            err,
            Error::RefStore(RefStoreError::ExpectedMismatch { ref name, .. }) if name == "refs/heads/main"
        ));
        assert_eq!(store.resolve("refs/heads/main").expect("tip"), oid(3));
        assert!(log(&store, "HEAD").is_empty());

        let err = update_branch_for_commit_on_store(&store, &update(None, oid(2)), &config(None))
            .expect_err("branch exists");
        assert!(matches!(
            err,
            Error::RefStore(RefStoreError::ExpectedMismatch { .. })
        ));
    }

    #[test]
    fn branch_commit_refuses_when_head_is_elsewhere() {
        let store = store_on("refs/heads/other", Some(oid(1)));
        store
            .set_ref("refs/heads/main", RawRef::Direct(oid(1)))
            .expect("main");
        let err =
            update_branch_for_commit_on_store(&store, &update(Some(oid(1)), oid(2)), &config(None))
                .expect_err("HEAD elsewhere");
        assert!(matches!(err, Error::Message(_)));
        assert_eq!(store.resolve("refs/heads/main").expect("tip"), oid(1));
    }

    #[test]
    fn disabled_ref_logging_skips_new_reflogs_but_appends_to_existing_ones() {
        let store = store_on("refs/heads/main", Some(oid(1)));
        update_branch_for_commit_on_store(
            &store,
            &update(Some(oid(1)), oid(2)),
            &config(Some("false")),
        )
        .expect("update");
        assert!(!store.reflog_exists("refs/heads/main").expect("exists"));
        assert!(!store.reflog_exists("HEAD").expect("exists"));

        store.create_reflog("HEAD").expect("create");
        update_branch_for_commit_on_store(
            &store,
            &update(Some(oid(2)), oid(3)),
            &config(Some("false")),
        )
        .expect("update");
        assert_eq!(log(&store, "HEAD").len(), 1);
        assert!(!store.reflog_exists("refs/heads/main").expect("exists"));
    }

    #[test]
    fn files_fast_paths_check_the_new_tip_against_the_repository_objects() {
        let dir = tempfile::tempdir().expect("tempdir");
        let repo = crate::repo::init_repository(
            dir.path(),
            false,
            "main",
            None,
            crate::RefStorageFormat::Files,
        )
        .expect("init");
        let git_dir = repo.git_dir().to_path_buf();
        let base = crate::refs::test_commit(&git_dir, "base");
        let tree = repo
            .odb()
            .write(
                crate::objects::ObjectKind::Tree,
                b"100644 f\0aaaaaaaaaaaaaaaaaaaa",
            )
            .expect("tree");
        crate::refs::write_ref(&git_dir, "refs/heads/main", &base).expect("main");
        assert!(
            repo.refs().as_files().is_some(),
            "exercises the files fast path"
        );

        let branch = |new_oid| BranchCommitRefUpdate {
            branch_ref: "refs/heads/main",
            expected_old: Some(base),
            new_oid,
            identity: IDENT,
            reflog_message: "commit: x",
        };
        let err = update_branch_for_commit_on_store(repo.refs(), &branch(oid(9)), &config(None))
            .expect_err("missing tip");
        assert!(
            matches!(&err, Error::RefStore(RefStoreError::MissingObject { name, oid: o })
                if name == "refs/heads/main" && *o == oid(9)),
            "{err:?}"
        );
        let err = update_branch_for_commit_on_store(repo.refs(), &branch(tree), &config(None))
            .expect_err("tree tip");
        assert!(
            matches!(&err, Error::RefStore(RefStoreError::NotACommit { name, .. })
                if name == "refs/heads/main"),
            "{err:?}"
        );
        assert_eq!(
            crate::refs::resolve_ref(&git_dir, "HEAD").expect("HEAD"),
            base
        );
        assert!(!git_dir.join("refs/heads/main.lock").exists());

        crate::refs::write_ref(&git_dir, "HEAD", &base).expect("detach");
        for (new_oid, missing) in [(oid(9), true), (tree, false)] {
            let err = update_detached_head_for_commit_on_store(
                repo.refs(),
                Some(base),
                new_oid,
                IDENT,
                "commit: x",
                &config(None),
            )
            .expect_err("bad detached tip");
            let expected = if missing {
                matches!(&err, Error::RefStore(RefStoreError::MissingObject { name, .. }) if name == "HEAD")
            } else {
                matches!(&err, Error::RefStore(RefStoreError::NotACommit { name, .. }) if name == "HEAD")
            };
            assert!(expected, "{err:?}");
        }
        assert_eq!(
            repo.refs().read_raw("HEAD").expect("HEAD"),
            Some(RawRef::Direct(base))
        );

        let next = crate::refs::test_commit(&git_dir, "next");
        update_detached_head_for_commit_on_store(
            repo.refs(),
            Some(base),
            next,
            IDENT,
            "commit: next",
            &config(None),
        )
        .expect("stored commit");
        assert_eq!(
            crate::refs::resolve_ref(&git_dir, "HEAD").expect("HEAD"),
            next
        );
    }

    #[test]
    fn detached_commit_moves_head_with_cas_and_logs_only_head() {
        let store = MemoryRefStore::new();
        store.set_ref("HEAD", RawRef::Direct(oid(1))).expect("HEAD");
        update_detached_head_for_commit_on_store(
            &store,
            Some(oid(1)),
            oid(2),
            IDENT,
            "commit: d",
            &config(None),
        )
        .expect("detached");
        assert_eq!(
            store.read_raw("HEAD").expect("HEAD"),
            Some(RawRef::Direct(oid(2)))
        );
        assert_eq!(log(&store, "HEAD").len(), 1);

        let err = update_detached_head_for_commit_on_store(
            &store,
            Some(oid(1)),
            oid(3),
            IDENT,
            "commit: d",
            &config(None),
        )
        .expect_err("stale");
        assert!(matches!(
            err,
            Error::RefStore(RefStoreError::ExpectedMismatch { .. })
        ));

        let attached = store_on("refs/heads/main", Some(oid(1)));
        assert!(matches!(
            update_detached_head_for_commit_on_store(
                &attached,
                Some(oid(1)),
                oid(2),
                IDENT,
                "commit: d",
                &config(None),
            ),
            Err(Error::Message(_))
        ));
    }

    #[test]
    fn append_reflog_honours_policy_and_force_without_moving_the_ref() {
        let store = store_on("refs/heads/main", Some(oid(1)));
        append_reflog_on_store(
            &store,
            "refs/tags/v1",
            &oid(0),
            &oid(1),
            IDENT,
            "tag",
            false,
            &config(None),
        )
        .expect("skip");
        assert!(!store.reflog_exists("refs/tags/v1").expect("exists"));

        append_reflog_on_store(
            &store,
            "refs/stash",
            &zero_oid(),
            &oid(4),
            IDENT,
            "WIP",
            true,
            &config(None),
        )
        .expect("forced");
        assert_eq!(
            log(&store, "refs/stash"),
            vec![(zero_oid(), oid(4), IDENT.to_owned(), "WIP".to_owned())]
        );
        assert_eq!(store.read_raw("refs/stash").expect("read"), None);

        append_reflog_on_store(
            &store,
            "HEAD",
            &oid(1),
            &oid(1),
            IDENT,
            "checkout: moving from main to main",
            false,
            &config(None),
        )
        .expect("HEAD");
        assert_eq!(log(&store, "HEAD").len(), 1);
        assert!(log(&store, "refs/heads/main").is_empty());
        assert_eq!(store.resolve("refs/heads/main").expect("tip"), oid(1));
    }

    #[test]
    fn symbolic_write_and_delete_go_through_the_store() {
        let store = store_on("refs/heads/main", Some(oid(1)));
        write_symbolic_ref_on_store(&store, Path::new("unused"), "HEAD", "refs/heads/topic")
            .expect("symref");
        assert_eq!(
            store.read_raw("HEAD").expect("HEAD"),
            Some(RawRef::Symbolic("refs/heads/topic".into()))
        );

        store
            .set_ref("refs/stash", RawRef::Direct(oid(5)))
            .expect("stash");
        store.create_reflog("refs/stash").expect("log");
        delete_ref_on_store(&store, "refs/stash").expect("delete");
        assert_eq!(store.read_raw("refs/stash").expect("read"), None);
        assert!(!store.reflog_exists("refs/stash").expect("exists"));
        assert!(delete_ref_on_store(&store, "refs/stash").is_err());
    }
}

//! Ref moves made for snapshot checkouts, by the server and by a refresh: one
//! compare-and-swap each, with a reflog entry where the repository keeps
//! reflogs.

use crate::error::Error;
use crate::ident::Signature;
use crate::objects::ObjectId;
use crate::refs::store::{Expected, RawRef, RefStoreError, ReflogUpdate};
use crate::repo::Repository;

use super::Result;

/// What [`swap_ref`] did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Swap {
    /// The ref moved.
    Done,
    /// The ref was not where the caller expected it; nothing changed.
    Moved,
}

/// Move `refname` from `expected` (`None`: it must not exist yet) to `new`,
/// as one compare-and-swap under the ref's lock.
///
/// The move is logged by `who` with `message` when `core.logAllRefUpdates`
/// asks for a reflog of `refname` (Git's default in a repository with a
/// worktree) or the ref already has one, as `git update-ref` would.
///
/// # Errors
///
/// Config, ref and reflog failures other than the compare-and-swap check,
/// which is [`Swap::Moved`].
pub(crate) fn swap_ref(
    repo: &Repository,
    refname: &str,
    expected: Option<ObjectId>,
    new: ObjectId,
    who: &Signature,
    message: &str,
) -> Result<Swap> {
    swap_ref_as(repo, refname, expected, new, &who.header_line(), message)
}

/// Log a move of the branch `HEAD` names from `old` to `new` in `HEAD`'s
/// reflog as well, as Git does for a branch that is checked out, where
/// `core.logAllRefUpdates` asks for it or `HEAD` already has a reflog.
///
/// # Errors
///
/// Config and reflog failures.
pub(crate) fn log_head_move(
    repo: &Repository,
    old: ObjectId,
    new: ObjectId,
    who: &Signature,
    message: &str,
) -> Result<()> {
    log_head_move_as(repo, old, new, &who.header_line(), message)
}

/// [`log_head_move`], logged as `identity`: a whole reflog identity line,
/// `Name <email> <seconds> <zone>`.
///
/// # Errors
///
/// As [`log_head_move`].
pub(crate) fn log_head_move_as(
    repo: &Repository,
    old: ObjectId,
    new: ObjectId,
    identity: &str,
    message: &str,
) -> Result<()> {
    let config = repo.config()?;
    if !crate::refs::should_autocreate_reflog_with_config(&config, repo.git_dir(), "HEAD")
        && !repo.refs().reflog_exists("HEAD")?
    {
        return Ok(());
    }
    crate::refs::append_reflog_on_store(
        repo.refs(),
        "HEAD",
        &old,
        &new,
        identity,
        message,
        false,
        &config,
    )?;
    Ok(())
}

/// [`swap_ref`], logged as `identity`: a whole reflog identity line,
/// `Name <email> <seconds> <zone>`.
///
/// # Errors
///
/// As [`swap_ref`].
pub(crate) fn swap_ref_as(
    repo: &Repository,
    refname: &str,
    expected: Option<ObjectId>,
    new: ObjectId,
    identity: &str,
    message: &str,
) -> Result<Swap> {
    let config = repo.config()?;
    let logged =
        crate::refs::should_autocreate_reflog_with_config(&config, repo.git_dir(), refname)
            || repo.refs().reflog_exists(refname)?;
    let reflog = logged.then(|| ReflogUpdate {
        identity: identity.to_owned(),
        message: message.to_owned(),
        // The identity carries its own time, so this one is never read.
        time: time::OffsetDateTime::UNIX_EPOCH,
        old_oid: None,
        new_oid: None,
    });
    match crate::refs::store_write_value_on_store(
        repo.refs(),
        repo.git_dir(),
        refname,
        Some(RawRef::Direct(new)),
        expected.map_or(Expected::Missing, Expected::Oid),
        reflog,
    ) {
        Ok(()) => Ok(Swap::Done),
        Err(Error::RefStore(RefStoreError::ExpectedMismatch { .. })) => Ok(Swap::Moved),
        Err(e) => Err(e.into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::objects::ObjectKind;
    use crate::repo::InitOptions;

    fn who() -> Signature {
        let time = time::OffsetDateTime::from_unix_timestamp(1_700_000_000)
            .unwrap_or(time::OffsetDateTime::UNIX_EPOCH);
        Signature::cleaned("Server", "server@example.com", time).expect("signature")
    }

    #[test]
    fn swaps_are_create_only_or_compare_and_swap_and_logged() {
        let dir = tempfile::tempdir().expect("tempdir");
        let repo = Repository::init(dir.path(), &InitOptions::default()).expect("init");
        let a = repo.odb().write(ObjectKind::Blob, b"a").expect("blob");
        let b = repo.odb().write(ObjectKind::Blob, b"b").expect("blob");
        let name = "refs/heads/topic";

        assert_eq!(
            swap_ref(&repo, name, None, a, &who(), "first").expect("create"),
            Swap::Done
        );
        // A create of a ref that exists, or an update from a stale tip, changes nothing.
        assert_eq!(
            swap_ref(&repo, name, None, b, &who(), "again").expect("create"),
            Swap::Moved
        );
        assert_eq!(
            swap_ref(&repo, name, Some(b), a, &who(), "stale").expect("update"),
            Swap::Moved
        );
        assert_eq!(crate::refs::resolve_ref(repo.git_dir(), name).ok(), Some(a));
        assert_eq!(
            swap_ref(&repo, name, Some(a), b, &who(), "second").expect("update"),
            Swap::Done
        );
        assert_eq!(crate::refs::resolve_ref(repo.git_dir(), name).ok(), Some(b));

        // A repository with a worktree logs ref moves, as Git does by default.
        let log = crate::reflog::read_reflog(repo.git_dir(), name).expect("reflog");
        let messages: Vec<&str> = log.iter().map(|e| e.message.as_str()).collect();
        assert_eq!(messages.len(), 2, "{messages:?}");
        assert!(messages.contains(&"first") && messages.contains(&"second"));
        assert!(log
            .iter()
            .all(|e| e.identity == "Server <server@example.com> 1700000000 +0000"));
    }
}

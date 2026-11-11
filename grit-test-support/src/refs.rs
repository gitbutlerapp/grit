//! Ref fixture helpers on top of [`grit_lib::refs::References`].

use std::path::Path;

use grit_lib::config::environment::RepositoryOptions;
use grit_lib::error::Result;
use grit_lib::objects::ObjectId;
use grit_lib::refs::update_direct_ref_on_store;
use grit_lib::repo::Repository;
use grit_lib::test_support::refs::unchecked_ref_name;

/// Open the repository whose git directory is `git_dir` (a bare repository, a `.git`
/// directory or a linked worktree's admin directory).
///
/// # Panics
///
/// When `git_dir` is not a repository.
#[must_use]
pub fn at(git_dir: impl AsRef<Path>) -> Repository {
    let git_dir = git_dir.as_ref();
    Repository::open(git_dir, &RepositoryOptions::default())
        .unwrap_or_else(|err| panic!("open {}: {err}", git_dir.display()))
}

/// Point `name` at `oid` without writing a reflog entry, the way fixtures set up refs.
///
/// `HEAD` is written itself (detaching it) rather than the branch it points at; any other
/// symbolic ref is followed. `oid` must be stored in the repository, and be a commit for a
/// branch or `HEAD`.
///
/// `name` is a plain string so fixtures can pass literals; the ref store validates it
/// before writing, exactly as it would a [`RefName`](grit_lib::refs::store::RefName).
///
/// # Errors
///
/// The errors of [`update_direct_ref_on_store`], including [`grit_lib::error::RefError::Invalid`]
/// for a name that is not safe to store.
pub fn set_ref(repo: &Repository, name: impl AsRef<str>, oid: &ObjectId) -> Result<()> {
    update_direct_ref_on_store(
        repo.ref_store(),
        repo.git_dir(),
        unchecked_ref_name(name.as_ref()),
        oid,
    )
}

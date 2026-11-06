//! Path-oriented view of an open repository's git directory and work tree.

use std::path::{Path, PathBuf};

use crate::repo::{local_config_write_path, Repository};

/// Where a repository lives on disk: its git directory and, when it has one, its work tree.
///
/// Build this from an open [`Repository`] with [`From`]: it copies
/// [`Repository::git_dir`](crate::repo::Repository::git_dir) and
/// [`Repository::work_tree`](crate::repo::Repository::work_tree). Discovery metadata lives on
/// [`DiscoveryInfo`](crate::repo::DiscoveryInfo) via [`Repository::discovery_info`](crate::repo::Repository::discovery_info).
///
/// # Examples
///
/// ```
/// use grit_lib::config::environment::RepositoryOptions;
/// use grit_lib::repo::{InitOptions, Repository, RepositoryLocation};
///
/// # fn main() -> grit_lib::error::Result<()> {
/// let dir = tempfile::tempdir()?;
/// let (repo, _) = Repository::init(dir.path(), &InitOptions::default())?;
/// let discovered = Repository::discover(Some(dir.path()), &RepositoryOptions::empty())?;
/// let location = RepositoryLocation::from(&discovered);
/// assert!(location.git_dir().ends_with(".git"));
/// assert!(location.local_config_path().ends_with(".git/config"));
/// assert_eq!(location, RepositoryLocation::from(&repo));
/// # Ok(())
/// # }
/// ```
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RepositoryLocation {
    git_dir: PathBuf,
    work_tree: Option<PathBuf>,
}

impl RepositoryLocation {
    /// Absolute path of the git directory (`.git`, a linked worktree's admin directory, or the
    /// root of a bare repository).
    #[must_use]
    pub fn git_dir(&self) -> &Path {
        &self.git_dir
    }

    /// Absolute path of the work tree, or `None` for a bare repository.
    #[must_use]
    pub fn work_tree(&self) -> Option<&Path> {
        self.work_tree.as_deref()
    }

    /// The repository-local config file that `--local` reads and writes (`.git/config`, or the
    /// common directory's `config` for a linked worktree).
    #[must_use]
    pub fn local_config_path(&self) -> PathBuf {
        local_config_write_path(&self.git_dir)
    }
}

impl From<&Repository> for RepositoryLocation {
    fn from(repo: &Repository) -> Self {
        Self {
            git_dir: repo.git_dir().to_path_buf(),
            work_tree: repo.work_tree().map(Path::to_path_buf),
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;
    use crate::environment::{Environment, RepositoryOptions};
    use crate::repo::InitOptions;

    fn options_at(cwd: &Path, vars: &[(&str, &str)]) -> RepositoryOptions {
        use std::ffi::OsString;
        let vars: Vec<(OsString, OsString)> = vars
            .iter()
            .map(|(k, v)| (OsString::from(k), OsString::from(v)))
            .collect();
        RepositoryOptions {
            environment: Environment::from_vars(vars, cwd.to_path_buf()),
            ..RepositoryOptions::default()
        }
    }

    #[test]
    fn location_from_repository_copies_paths() {
        let tmp = tempfile::tempdir().unwrap();
        Repository::init(tmp.path(), &InitOptions::default()).unwrap();
        let repo = Repository::discover(None, &options_at(tmp.path(), &[])).unwrap();
        let location = RepositoryLocation::from(&repo);
        assert_eq!(location.git_dir(), repo.git_dir());
        assert_eq!(location.work_tree(), repo.work_tree());
    }
}

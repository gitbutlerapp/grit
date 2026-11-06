//! Injectable ownership checks for repository discovery (`safe.directory`).
//!
//! Grit refuses to use repositories whose git directory or work tree is not owned by the
//! effective user unless `safe.directory` allows the path. Embedders can replace the default
//! [`SystemOwnershipCheck`] to model sandboxes, containers, or test harnesses.
//!
//! ```rust
//! use std::path::Path;
//! use std::sync::Arc;
//!
//! use grit_lib::config::environment::{Environment, RepositoryOptions};
//! use grit_lib::ownership_check::{
//!     OwnershipCheck, OwnershipContext, OwnershipStatus, SystemOwnershipCheck,
//! };
//! use grit_lib::repo::{InitOptions, Repository};
//!
//! /// Treat every path as foreign so `safe.directory` always applies (tests/sandboxes).
//! struct SandboxForeign;
//!
//! impl OwnershipCheck for SandboxForeign {
//!     fn always_apply_safe_directory(&self) -> bool {
//!         true
//!     }
//!     fn status(
//!         &self,
//!         _path: &Path,
//!         _context: &OwnershipContext,
//!     ) -> Result<OwnershipStatus, grit_lib::ownership_check::OwnershipError> {
//!         Ok(OwnershipStatus::NotOwned)
//!     }
//! }
//!
//! # fn main() -> grit_lib::error::Result<()> {
//! let dir = tempfile::tempdir()?;
//! let (repo, _) = Repository::init(dir.path(), &InitOptions::default())?;
//!
//! let mut env = Environment::empty();
//! env.cwd = dir.path().to_path_buf();
//! env.git_config_nosystem = Some("true".into());
//! let options = RepositoryOptions::with_environment(env)
//!     .with_ownership_check(Arc::new(SandboxForeign));
//! let err = Repository::discover(Some(dir.path()), &options).unwrap_err();
//! assert!(matches!(err, grit_lib::error::Error::Repository(grit_lib::error::RepositoryError::DubiousOwnership { .. })));
//!
//! let mut owned_env = Environment::empty();
//! owned_env.cwd = dir.path().to_path_buf();
//! let owned = RepositoryOptions::with_environment(owned_env)
//!     .with_ownership_check(Arc::new(SystemOwnershipCheck));
//! let _repo = Repository::discover(Some(dir.path()), &owned)?;
//! # Ok(())
//! # }
//! ```

use std::path::Path;
use std::sync::{Arc, LazyLock};

use thiserror::Error;

/// Whether a path on disk is owned by the effective user.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OwnershipStatus {
    /// The effective user owns the path (or ownership checks are skipped on this platform).
    Owned,
    /// Another user owns the path; `safe.directory` must allow it.
    NotOwned,
}

/// Failure modes for ownership inspection.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum OwnershipError {
    /// Could not read metadata for the path.
    #[error("failed to inspect ownership of {}: {source}", path.display())]
    Io {
        /// Path that could not be inspected.
        path: std::path::PathBuf,
        /// Underlying I/O error.
        #[source]
        source: std::io::Error,
    },
}

/// Context passed to [`OwnershipCheck::status`].
#[derive(Debug, Clone, Copy, Default)]
pub struct OwnershipContext {
    /// When the effective user is root, treat this uid as the real user (`SUDO_UID`).
    pub sudo_uid: Option<u32>,
}

impl OwnershipContext {
    /// Build context from Git discovery environment fields.
    #[must_use]
    pub fn from_environment(environment: &crate::environment::Environment) -> Self {
        Self {
            sudo_uid: environment
                .sudo_uid
                .as_deref()
                .and_then(|s| s.parse::<u32>().ok()),
        }
    }
}

/// Answers whether the current user owns a repository path.
pub trait OwnershipCheck: Send + Sync {
    /// When `true`, skip uid short-circuiting and always apply `safe.directory` rules.
    ///
    /// Used by sandboxes that treat every repository as foreign even when uid matches.
    fn always_apply_safe_directory(&self) -> bool {
        false
    }

    /// Inspect ownership of `path` using symlink metadata (`lstat` semantics).
    ///
    /// # Errors
    ///
    /// Returns [`OwnershipError::Io`] when metadata cannot be read, except that a missing
    /// work tree path is treated as [`OwnershipStatus::Owned`] by callers (Git `t1510`).
    fn status(
        &self,
        path: &Path,
        context: &OwnershipContext,
    ) -> Result<OwnershipStatus, OwnershipError>;
}

/// Default ownership check using the effective Unix uid and `SUDO_UID`.
#[derive(Debug, Clone, Copy, Default)]
pub struct SystemOwnershipCheck;

impl OwnershipCheck for SystemOwnershipCheck {
    fn status(
        &self,
        path: &Path,
        context: &OwnershipContext,
    ) -> Result<OwnershipStatus, OwnershipError> {
        #[cfg(unix)]
        {
            let owned =
                system_owned_by_effective_user(path, context.sudo_uid).map_err(|source| {
                    OwnershipError::Io {
                        path: path.to_path_buf(),
                        source,
                    }
                })?;
            Ok(if owned {
                OwnershipStatus::Owned
            } else {
                OwnershipStatus::NotOwned
            })
        }
        #[cfg(not(unix))]
        {
            let _ = (path, context);
            Ok(OwnershipStatus::Owned)
        }
    }
}

/// Treat every path as not owned so `safe.directory` always applies.
///
/// Used in crate tests; embedders typically implement [`OwnershipCheck`] directly.
#[cfg(test)]
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct ForeignOwnershipCheck;

#[cfg(test)]
impl OwnershipCheck for ForeignOwnershipCheck {
    fn always_apply_safe_directory(&self) -> bool {
        true
    }

    fn status(
        &self,
        _path: &Path,
        _context: &OwnershipContext,
    ) -> Result<OwnershipStatus, OwnershipError> {
        Ok(OwnershipStatus::NotOwned)
    }
}

static DEFAULT_OWNERSHIP_CHECK: LazyLock<Arc<dyn OwnershipCheck>> =
    LazyLock::new(|| Arc::new(SystemOwnershipCheck));

/// Shared [`SystemOwnershipCheck`] used by [`RepositoryOptions`](crate::config::environment::RepositoryOptions) defaults.
#[must_use]
pub(crate) fn default_ownership_check() -> Arc<dyn OwnershipCheck> {
    Arc::clone(&DEFAULT_OWNERSHIP_CHECK)
}

/// Whether `check` is the shared default [`SystemOwnershipCheck`] instance.
#[must_use]
pub(crate) fn is_default_ownership_check(check: &Arc<dyn OwnershipCheck>) -> bool {
    Arc::ptr_eq(check, &DEFAULT_OWNERSHIP_CHECK)
}

#[cfg(unix)]
fn system_owned_by_effective_user(path: &Path, sudo_uid: Option<u32>) -> std::io::Result<bool> {
    use std::fs;
    use std::os::unix::fs::MetadataExt;

    const ROOT_UID: u32 = 0;
    let meta = fs::symlink_metadata(path)?;
    let st_uid = meta.uid();
    let mut euid = nix::unistd::geteuid().as_raw();
    if euid == ROOT_UID {
        if st_uid == ROOT_UID {
            return Ok(true);
        }
        if let Some(sudo_uid) = sudo_uid {
            euid = sudo_uid;
        }
    }
    Ok(st_uid == euid)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;
    use crate::config::environment::RepositoryOptions;
    use crate::repo::{InitOptions, Repository, RepositoryError};

    #[test]
    fn default_ownership_check_is_shared() {
        let a = default_ownership_check();
        let b = default_ownership_check();
        assert!(is_default_ownership_check(&a));
        assert!(Arc::ptr_eq(&a, &b));
    }

    #[test]
    fn foreign_check_always_applies_safe_directory() {
        let check = ForeignOwnershipCheck;
        assert!(check.always_apply_safe_directory());
        let ctx = OwnershipContext::default();
        assert_eq!(
            check.status(Path::new("."), &ctx).unwrap(),
            OwnershipStatus::NotOwned
        );
    }

    #[test]
    fn system_check_owned_repo_in_tempdir() {
        let dir = tempfile::tempdir().unwrap();
        Repository::init(dir.path(), &InitOptions::default())
            .unwrap()
            .0;
        let ctx = OwnershipContext::default();
        assert_eq!(
            SystemOwnershipCheck.status(dir.path(), &ctx).unwrap(),
            OwnershipStatus::Owned
        );
        let dot_git = dir.path().join(".git");
        assert_eq!(
            SystemOwnershipCheck.status(&dot_git, &ctx).unwrap(),
            OwnershipStatus::Owned
        );
    }

    #[test]
    fn with_ownership_check_replaces_default_arc() {
        let options =
            RepositoryOptions::empty().with_ownership_check(Arc::new(ForeignOwnershipCheck));
        assert!(!options.uses_default_ownership_check());
    }

    #[test]
    fn ownership_check_default_allows_owned_repo() {
        let td = tempfile::tempdir().unwrap();
        Repository::init(td.path(), &InitOptions::default())
            .unwrap()
            .0;
        let mut env = crate::environment::Environment::empty();
        env.cwd = td.path().to_path_buf();
        let options = RepositoryOptions::with_environment(env)
            .with_ownership_check(Arc::new(SystemOwnershipCheck));
        Repository::discover(Some(td.path()), &options).expect("owned repo opens");
    }

    #[test]
    fn ownership_check_foreign_repo_requires_safe_directory() {
        let td = tempfile::tempdir().unwrap();
        Repository::init(td.path(), &InitOptions::default())
            .unwrap()
            .0;
        let mut env = crate::environment::Environment::empty();
        env.cwd = td.path().to_path_buf();
        env.git_config_nosystem = Some("true".into());
        let options = RepositoryOptions::with_environment(env)
            .with_ownership_check(Arc::new(ForeignOwnershipCheck));
        let err = Repository::discover(Some(td.path()), &options).unwrap_err();
        assert!(matches!(
            err,
            crate::error::Error::Repository(RepositoryError::DubiousOwnership { .. })
        ));
    }
}

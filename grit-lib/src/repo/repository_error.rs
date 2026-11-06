//! Repository layout, discovery, init, and embedding failures.

use std::path::PathBuf;

use thiserror::Error;

use std::fmt;

use crate::error::{NotARepositoryReason, PathIoError};

/// One or more unknown `extensions.*` keys (repository format version ≥ 1).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnknownRepositoryExtensions {
    /// Extension names from the config file, in sorted order.
    pub extensions: Vec<String>,
}

impl fmt::Display for UnknownRepositoryExtensions {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let lead = if self.extensions.len() == 1 {
            "unknown repository extension found:"
        } else {
            "unknown repository extensions found:"
        };
        write!(f, "{lead}")?;
        for ext in &self.extensions {
            write!(f, "\n\t{ext}")?;
        }
        Ok(())
    }
}

/// Format version 0 with `extensions.*` keys that require format version 1.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct V1OnlyExtensionsInFormatZero {
    /// Extension names from the config file, in sorted order.
    pub extensions: Vec<String>,
}

impl fmt::Display for V1OnlyExtensionsInFormatZero {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let lead = if self.extensions.len() == 1 {
            "repo version is 0, but v1-only extension found:"
        } else {
            "repo version is 0, but v1-only extensions found:"
        };
        write!(f, "{lead}")?;
        for ext in &self.extensions {
            write!(f, "\n\t{ext}")?;
        }
        Ok(())
    }
}
use crate::git_dir::GitDirPathOperation;
use crate::objects::HashAlgo;

/// Failures opening, discovering, initializing, or validating a repository layout.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum RepositoryError {
    /// A repository could not be found or is structurally invalid.
    #[error("not a git repository (or any of the parent directories): {0}")]
    NotARepository(NotARepositoryReason),

    /// A bare repository was found but access is forbidden by `safe.bareRepository`.
    #[error("cannot use bare repository '{path}' (safe.bareRepository is 'explicit')")]
    ForbiddenBareRepository {
        /// Display path of the bare repository that was rejected.
        path: String,
    },

    /// The repository is owned by a different user (`safe.directory`).
    #[error("detected dubious ownership in repository at '{path}'")]
    DubiousOwnership {
        /// Display path that failed the ownership check.
        path: String,
        /// Owner uid of the path when known.
        owner_uid: Option<u32>,
        /// Effective uid of the current process when known.
        effective_uid: Option<u32>,
    },

    /// Repository format version is not supported by this implementation.
    #[error("unsupported repository format version '{version}'")]
    UnsupportedRepositoryFormatVersion {
        /// Parsed `core.repositoryformatversion`.
        version: u32,
    },

    /// `extensions.refStorage` names an unsupported backend.
    #[error("invalid value for 'extensions.refstorage': '{value}'")]
    InvalidRefStorageFormat {
        /// Raw `extensions.refStorage` value from the repository config.
        value: String,
    },

    /// [`super::Repository::init`] refused to change object format on reinit.
    #[error("cannot change object format in an existing repository")]
    InitObjectFormatChange,

    /// [`super::Repository::init`] with incompatible `--bare` and separate git dir.
    #[error("cannot combine bare repository with separate git directory")]
    InitBareWithSeparateGitDir,

    /// Command requires a working tree but the repository is bare.
    #[error("this operation needs a working tree")]
    RequiresWorkTree,

    /// [`super::Repository::prefetch_promised`] was called without a promisor fetcher on the handle.
    #[error("promisor prefetch requires a PromisorFetcher on RepositoryOptions")]
    PromisorFetcherMissing,

    /// Repository format version 1+ declares an extension this implementation does not recognize.
    #[error("{0}")]
    UnknownRepositoryExtensions(UnknownRepositoryExtensions),

    /// Format version 0 declares an extension that requires format version 1 or newer.
    #[error("{0}")]
    V1OnlyExtensionsInFormatZero(V1OnlyExtensionsInFormatZero),

    /// Filesystem I/O failed on a path under the repository git directory.
    #[error("{0}")]
    GitDirPathIo(PathIoError),

    /// The operation needs the files-backed `objects/` directory, but the repository uses a custom primary store.
    #[error("{operation} requires the files-backed object store; it is not supported with a custom primary")]
    UnsupportedObjectStore {
        /// Operation that was refused (for example `"fetch"`, `"push"`, `"gc"`).
        operation: &'static str,
    },

    /// The operation would delete objects, and `extensions.preciousObjects` forbids it.
    #[error("{operation} deletes objects, which extensions.preciousObjects forbids here")]
    PreciousObjectsRepository {
        /// Operation that was refused (for example `"drop-large-blobs"`).
        operation: &'static str,
    },

    /// [`super::Repository::open`] was given an [`OdbBuilder`](crate::odb::OdbBuilder) rooted outside `objects/`.
    #[error(
        "object database {} is not the repository's objects directory {}",
        builder.display(),
        repository.display()
    )]
    OdbObjectsDirMismatch {
        /// The builder's objects directory, canonicalized when it exists.
        builder: PathBuf,
        /// The repository's canonical objects directory.
        repository: PathBuf,
    },

    /// Injected object store hash algorithm differs from the repository's `extensions.objectformat`.
    #[error(
        "object store hashes with {} but the repository's object format is {}",
        store.name(),
        repository.name()
    )]
    ObjectFormatMismatch {
        /// The repository's object format from its config.
        repository: HashAlgo,
        /// The first injected store's algorithm that differs from `repository`.
        store: HashAlgo,
    },
}

impl RepositoryError {
    /// Stable snake_case name (same slugs as the legacy umbrella variants for these failures).
    #[must_use]
    pub fn kind_name(&self) -> &'static str {
        match self {
            Self::NotARepository(..) => "not_a_repository",
            Self::ForbiddenBareRepository { .. } => "forbidden_bare_repository",
            Self::DubiousOwnership { .. } => "dubious_ownership",
            Self::UnsupportedRepositoryFormatVersion { .. } => {
                "unsupported_repository_format_version"
            }
            Self::InvalidRefStorageFormat { .. } => "invalid_ref_storage_format",
            Self::InitObjectFormatChange => "init_object_format_change",
            Self::InitBareWithSeparateGitDir => "init_bare_with_separate_git_dir",
            Self::RequiresWorkTree => "requires_work_tree",
            Self::PromisorFetcherMissing => "message",
            Self::UnknownRepositoryExtensions(_) | Self::V1OnlyExtensionsInFormatZero(_) => {
                "message"
            }
            Self::GitDirPathIo(io) => match io.operation {
                crate::error::PathIoOperation::GitDir(_) => "git_dir_path_io",
                crate::error::PathIoOperation::Worktree(_) => "worktree_path_io",
            },
            Self::UnsupportedObjectStore { .. } => "unsupported_object_store",
            Self::PreciousObjectsRepository { .. } => "precious_objects_repository",
            Self::OdbObjectsDirMismatch { .. } => "odb_objects_dir_mismatch",
            Self::ObjectFormatMismatch { .. } => "object_format_mismatch",
        }
    }

    /// Build [`Self::GitDirPathIo`] from git-dir path context (legacy call sites).
    #[must_use]
    pub fn git_dir_path_io(
        path: impl Into<String>,
        operation: GitDirPathOperation,
        source: std::io::Error,
    ) -> Self {
        Self::GitDirPathIo(PathIoError::git_dir(path, operation, source))
    }
}

impl From<PathIoError> for RepositoryError {
    fn from(value: PathIoError) -> Self {
        Self::GitDirPathIo(value)
    }
}

/// Convenience alias for repository open/discover/init APIs.
pub type RepositoryResult<T> = std::result::Result<T, RepositoryError>;

//! Repository layout, discovery, init, and embedding failures.

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

/// An operation that only the files-backed object store supports.
///
/// Carried by [`RepositoryError::UnsupportedObjectStore`] when a repository whose primary
/// store is custom (for example a [`MemoryStore`](crate::objects::odb::store::MemoryStore))
/// is asked to do something that reads or writes the on-disk `objects/` layout: loose
/// files, packs, `.idx`, multi-pack-index or commit-graph files. Each variant names the
/// APIs that refuse with it.
///
/// [`Self::as_str`] gives a stable snake_case name for scripts (the `operation` field of
/// `grit --json` errors); [`Display`](fmt::Display) gives a short human phrase.
///
/// # Examples
///
/// ```
/// use grit_lib::repo::error::FilesOnlyOperation;
///
/// assert_eq!(FilesOnlyOperation::CommitGraphWrite.as_str(), "commit_graph_write");
/// assert_eq!(FilesOnlyOperation::Gc.to_string(), "garbage collection");
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum FilesOnlyOperation {
    /// Garbage collection without a repository: [`Odb::gc`](crate::odb::Odb::gc) on an `Odb`
    /// built standalone, with no git directory to find reachability roots in.
    Gc,
    /// Deleting unreachable objects from a custom primary that cannot delete
    /// ([`WritableObjectStore::as_prunable`](crate::odb::store::WritableObjectStore::as_prunable)
    /// is `None`): [`maintenance::prune_loose_objects`](crate::maintenance::prune_loose_objects),
    /// [`refs::gc::prune_loose_unreachable`](crate::gc::prune_loose_unreachable),
    /// [`Odb::gc`](crate::odb::Odb::gc), [`maintenance::gc`](crate::maintenance::gc) and
    /// [`maintenance::gc_with_cancel`](crate::maintenance::gc_with_cancel). Implement
    /// [`PrunableObjectStore`](crate::odb::store::PrunableObjectStore) to support them.
    Prune,
    /// Repacking into `objects/pack/`: [`maintenance::repack`](crate::maintenance::repack), [`maintenance::repack_all`](crate::maintenance::repack_all)
    /// and [`maintenance::repack_all_with_cancel`](crate::maintenance::repack_all_with_cancel).
    Repack,
    /// Writing `objects/info/commit-graph`: [`Odb::write_commit_graph`](crate::odb::Odb::write_commit_graph) and
    /// [`maintenance::write_commit_graph`](crate::maintenance::write_commit_graph). A custom store builds its graph in memory with
    /// [`maintenance::build_commit_graph`](crate::maintenance::build_commit_graph) instead.
    CommitGraphWrite,
    /// Writing `objects/pack/multi-pack-index`: [`maintenance::write_midx`](crate::maintenance::write_midx).
    MidxWrite,
    /// The files backend's pack and MIDX read cache: [`Odb::pack_store`](crate::odb::Odb::pack_store).
    PackStore,
    /// Installing a pack file and its `.idx` under `objects/pack/`:
    /// [`index_pack::install_pack_bytes`](crate::index_pack::install_pack_bytes) and [`index_pack::install_pack_path`](crate::index_pack::install_pack_path). Use
    /// [`index_pack::ingest_received_pack`](crate::index_pack::ingest_received_pack) to write a pack into any primary.
    PackInstall,
    /// Finding the largest pack index in `objects/pack/`: [`Odb::largest_pack_index_path`](crate::odb::Odb::largest_pack_index_path).
    LargestPackIndex,
    /// Writing loose object files into `objects/`: [`Odb::write_local`](crate::odb::Odb::write_local),
    /// [`Odb::write_raw_local`](crate::odb::Odb::write_raw_local) and [`Odb::write_loose_materialize`](crate::odb::Odb::write_loose_materialize). [`Odb::write`](crate::odb::Odb::write) works with
    /// any primary.
    LooseWrite,
    /// The files-backed primary source itself: [`Odb::primary`](crate::odb::Odb::primary). Use [`Odb::primary_writable`](crate::odb::Odb::primary_writable)
    /// for the primary of any kind.
    FilesSource,
    /// Moving blobs to a large-object promisor: [`maintenance::offload_blobs`](crate::maintenance::offload_blobs), for the main
    /// repository and for the promisor repository.
    Offload,
    /// Rewriting packs to leave out promised objects, as offload and dropping promised
    /// blobs do.
    PromisorRewrite,
    /// Fetching or cloning with an object filter (a partial clone) into a custom primary.
    PartialFetch,
    /// The receive-pack quarantine directory under `objects/`. A custom primary receives
    /// pushes through an in-memory overlay instead.
    ReceiveQuarantine,
    /// Dropping promised blobs from promisor packs: [`maintenance::drop_promised_blobs`](crate::maintenance::drop_promised_blobs).
    DropLargeBlobs,
}

impl FilesOnlyOperation {
    /// Stable snake_case name (`"gc"`, `"commit_graph_write"`, …) for scripts and JSON.
    ///
    /// Names never change once released; new variants add new names.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Gc => "gc",
            Self::Prune => "prune",
            Self::Repack => "repack",
            Self::CommitGraphWrite => "commit_graph_write",
            Self::MidxWrite => "midx_write",
            Self::PackStore => "pack_store",
            Self::PackInstall => "pack_install",
            Self::LargestPackIndex => "largest_pack_index",
            Self::LooseWrite => "loose_write",
            Self::FilesSource => "files_source",
            Self::Offload => "offload",
            Self::PromisorRewrite => "promisor_rewrite",
            Self::PartialFetch => "partial_fetch",
            Self::ReceiveQuarantine => "receive_quarantine",
            Self::DropLargeBlobs => "drop_large_blobs",
        }
    }
}

impl fmt::Display for FilesOnlyOperation {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Gc => "garbage collection",
            Self::Prune => "pruning unreachable objects",
            Self::Repack => "repacking",
            Self::CommitGraphWrite => "writing a commit-graph file",
            Self::MidxWrite => "writing a multi-pack-index",
            Self::PackStore => "the pack read cache",
            Self::PackInstall => "installing a pack file",
            Self::LargestPackIndex => "finding the largest pack index",
            Self::LooseWrite => "writing loose object files",
            Self::FilesSource => "opening the files-backed primary",
            Self::Offload => "offloading blobs",
            Self::PromisorRewrite => "rewriting promisor packs",
            Self::PartialFetch => "a partial fetch",
            Self::ReceiveQuarantine => "the receive quarantine directory",
            Self::DropLargeBlobs => "dropping promised blobs",
        })
    }
}

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

    /// [`super::Repository::init`] was asked for an on-disk ref storage format while
    /// [`RepositoryOptions::with_ref_store`](crate::config::environment::RepositoryOptions::with_ref_store)
    /// injects the ref backend.
    #[error("cannot create {requested} ref storage: the ref store is injected")]
    InitRefStorageWithInjectedRefStore {
        /// The ref storage format [`InitOptions::ref_storage`](super::InitOptions::ref_storage) requested.
        requested: crate::ref_storage::RefStorageFormat,
    },

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
    #[error(
        "{operation} needs the files-backed object store, but this repository's primary store is custom"
    )]
    UnsupportedObjectStore {
        /// The operation that was refused.
        operation: FilesOnlyOperation,
    },

    /// The operation would delete objects, and `extensions.preciousObjects` forbids it.
    #[error("{operation} deletes objects, which extensions.preciousObjects forbids here")]
    PreciousObjectsRepository {
        /// Operation that was refused (for example `"drop-large-blobs"`).
        operation: &'static str,
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
            Self::InitRefStorageWithInjectedRefStore { .. } => {
                "init_ref_storage_with_injected_ref_store"
            }
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
            Self::ObjectFormatMismatch { .. } => "object_format_mismatch",
        }
    }

    /// Functional area for analytics and JSON (`area`), matching [`crate::error::Error::area`] when wrapped.
    #[must_use]
    pub fn area(&self) -> &'static str {
        match self {
            Self::GitDirPathIo(io) => match io.operation {
                crate::error::PathIoOperation::GitDir(_) => "repository",
                crate::error::PathIoOperation::Worktree(_) => "worktree",
            },
            Self::RequiresWorkTree
            | Self::PromisorFetcherMissing
            | Self::UnknownRepositoryExtensions(_)
            | Self::V1OnlyExtensionsInFormatZero(_) => "repository",
            _ => "repository",
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

#[cfg(test)]
#[path = "files_only_operation_tests.rs"]
mod files_only_operation_tests;

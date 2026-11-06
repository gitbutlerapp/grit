//! Typed failures for index read, write, extensions, and write-tree.

use std::path::{Path, PathBuf};

use crate::error::{GitDirPathOperation, LockHeldError, PathIoError, PathIoOperation};
use crate::objects::HashAlgo;
use thiserror::Error;

/// Result type for index operations ([`IndexError`] only).
pub type IndexResult<T> = std::result::Result<T, IndexError>;

/// Why index bytes or an extension payload could not be parsed.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum IndexCorruptReason {
    /// Header magic is not `DIRC`.
    BadSignature,
    /// On-disk version is not supported (2, 3, or 4).
    UnsupportedVersion {
        /// Version field read from the header.
        version: u32,
    },
    /// Unexpected end of file while reading required fields.
    Truncated,
    /// Trailing checksum does not match file contents.
    ChecksumMismatch {
        /// Hash algorithm used for the trailer.
        algo: HashAlgo,
    },
    /// A fixed field or entry could not be decoded.
    BadEntry,
    /// File is shorter than header plus checksum.
    FileTooShortForHeader,
    /// Version field could not be read.
    CannotReadVersion,
    /// Entry count field could not be read.
    CannotReadEntryCount,
    /// Parallel load expected EOIE but none was present.
    ParallelLoadMissingEoie,
    /// Extension length extends past the index body.
    ExtensionOverrunsBody,
    /// Extension signature truncated.
    ExtensionTruncatedSig,
    /// Extension size field truncated.
    ExtensionTruncatedSize,
    /// Bytes remain after the extension region.
    JunkAfterExtensions,
    /// Extension fourcc is unsupported (lowercase signature).
    UnsupportedExtension {
        /// Extension signature dword.
        sig: u32,
    },
    /// Entry region starts beyond the file end.
    EntryRegionStartsPastEnd,
    /// Declared entry count exceeds what fits in the file.
    EntryCountMismatch {
        /// Count from the header.
        declared: u32,
        /// Maximum entries that fit in remaining bytes.
        max_fit: usize,
        /// Remaining byte length for entries.
        remaining: usize,
    },
    /// Entry consumed size overflowed `usize`.
    EntrySizeOverflow,
    /// An index entry extends past the body.
    EntryOverrunsBody,
    /// Entry fixed portion is too short.
    EntryTooShort,
    /// Truncated u32 while reading an entry.
    TruncatedU32,
    /// Truncated standard flags.
    TruncatedFlags,
    /// Truncated extended flags.
    TruncatedExtendedFlags,
    /// Version 4 entry path has no NUL delimiter.
    V4PathMissingNul,
    /// Entry path has no NUL terminator.
    PathMissingNulTerminator,
    /// Index entry truncated mid-record.
    TruncatedIndexEntry,
    /// IEOT block slice is out of range.
    IeotBlockRangeOutOfBounds,
    /// IEOT per-block entry count overflow.
    IeotEntryCountOverflow,
    /// IEOT block offset is past the index body.
    IeotBlockOffsetPastBody {
        /// Block index in the IEOT table.
        block: usize,
        /// Byte offset recorded for the block.
        offset: usize,
    },
    /// IEOT block entry overruns the body.
    IeotBlockEntryOverruns {
        /// Block index in the IEOT table.
        block: usize,
        /// Position within the index body.
        pos: usize,
    },
    /// IEOT table has no blocks.
    EmptyIeotTable,
    /// Parallel IEOT load entry count does not match header.
    IeotEntryCountMismatch {
        /// Count from the index header.
        expected: u32,
        /// Entries assembled from IEOT blocks.
        got: usize,
    },
    /// Cache-tree update during write-tree failed.
    CacheTreeUpdateFailed,
    /// write-tree: cache-tree missing after update.
    WriteTreeMissingCacheTree,
    /// write-tree: cache-tree root has no oid.
    WriteTreeCacheTreeRootMissingOid,
    /// write-tree: invalid path prefix.
    WriteTreeInvalidPrefix {
        /// Prefix argument that failed validation.
        prefix: String,
    },
    /// write-tree: cache-tree child metadata missing.
    WriteTreeCacheTreeChildMissing,
    /// write-tree: expected a tree object at an oid.
    WriteTreeExpectedTree {
        /// Object id that was not a tree.
        oid: crate::objects::ObjectId,
    },
    /// write-tree: missing index entry during traversal.
    WriteTreeMissingIndexEntry,
    /// Resolve-undo extension payload truncated (path).
    ResolveUndoTruncatedPath,
    /// Resolve-undo mode field truncated.
    ResolveUndoTruncatedMode,
    /// Resolve-undo mode is not valid UTF-8.
    ResolveUndoInvalidModeUtf8,
    /// Resolve-undo mode is not a valid octal file mode.
    ResolveUndoInvalidMode {
        /// Raw mode bytes as lossy UTF-8 for display.
        mode: String,
    },
    /// Resolve-undo path is not valid UTF-8.
    ResolveUndoInvalidPathUtf8,
    /// Resolve-undo parse failed for another reason.
    ResolveUndoCorrupt,
    /// Resolve-undo object id field truncated.
    ResolveUndoTruncatedOid,
    /// Untracked-cache (`UNTR`) payload ends before required fields.
    UntrackedCacheTruncated,
    /// Untracked-cache varint could not be decoded.
    UntrackedCacheInvalidVarint,
    /// Untracked-cache payload missing trailing NUL.
    UntrackedCacheMissingNul,
    /// Untracked-cache string field is not valid UTF-8.
    UntrackedCacheInvalidUtf8,
    /// Untracked-cache directory tree could not be parsed.
    UntrackedCacheDirCorrupt,
    /// Untracked-cache extension has no root directory.
    UntrackedCacheNoRoot,
    /// Split index link or merge failure (detail in message via split variant).
    SplitIndexCorrupt,
    /// Index file shorter than minimum for split index.
    SplitIndexTooShort,
    /// Checksum worker thread panicked during parallel verify.
    ChecksumWorkerPanicked,
    /// Missing index entry during diff tree walk.
    MissingIndexEntryDuringTreeWalk,
    /// Missing index entry during hard reset alignment.
    MissingIndexEntryDuringHardReset,
    /// Hard-reset cache-tree rebuild: header entry count mismatch.
    HardResetCacheTreeEntryCountMismatch,
    /// Hard-reset cache-tree rebuild: index and tree out of sync.
    HardResetCacheTreeOutOfSync,
    /// Hard-reset cache-tree rebuild: tree vs index entry kind mismatch.
    HardResetCacheTreeTypeMismatch,
}

/// Index file or extension corruption at an optional byte offset.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IndexCorrupt {
    /// Repository-relative path when the failure came from a file on disk.
    pub path: Option<PathBuf>,
    /// Byte offset into the index file when known.
    pub offset: Option<usize>,
    /// Structured reason for the failure.
    pub reason: IndexCorruptReason,
}

/// Why [`IndexError::Chmod`] rejected a mode change.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum IndexChmodReason {
    /// Conflict stages exist for the path.
    UnmergedPath,
    /// Entry is not a regular file (symlink, gitlink, directory, …).
    NotChmodEligible,
    /// Target mode is not `100644` or `100755`.
    InvalidTargetMode {
        /// Mode the caller requested.
        mode: u32,
    },
}

impl IndexCorrupt {
    /// Build a corruption error without path or offset.
    #[must_use]
    pub fn new(reason: IndexCorruptReason) -> Self {
        Self {
            path: None,
            offset: None,
            reason,
        }
    }

    /// Attach a byte offset.
    #[must_use]
    pub fn at_offset(mut self, offset: usize) -> Self {
        self.offset = Some(offset);
        self
    }

    /// Attach the index path being read or written.
    #[must_use]
    pub fn at_path(mut self, path: impl Into<PathBuf>) -> Self {
        self.path = Some(path.into());
        self
    }
}

/// Failures while reading, writing, or deriving trees from the Git index.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum IndexError {
    /// On-disk index bytes or an extension could not be parsed.
    #[error("index error: {detail}")]
    Corrupt {
        /// Structured corruption site.
        corrupt: IndexCorrupt,
        /// Human detail matching historical `Error::IndexError` messages where needed.
        detail: String,
    },

    /// Stage 1/2/3 entries are present; a stage-0 tree cannot be built.
    #[error("index contains unmerged entries")]
    Unmerged {
        /// Repository-relative paths with conflict stages (may be empty when unknown).
        paths: Vec<String>,
    },

    /// A file path is a prefix of another index path at the same tree level.
    #[error(
        "index path prefix conflict between '{}' and '{}'",
        directory.display(),
        file.display()
    )]
    PathPrefixConflict {
        /// Shorter path (treated as the ambiguous directory prefix).
        directory: PathBuf,
        /// Longer path that extends the prefix with an additional component.
        file: PathBuf,
    },

    /// The `FSMN` index extension ends before its header, token, or EWAH payload.
    #[error("index fsmonitor extension payload is truncated")]
    FsmonitorExtensionTruncated,

    /// The `FSMN` extension header version is not supported.
    #[error("index fsmonitor extension has unsupported header version {0}")]
    FsmonitorExtensionBadVersion(u32),

    /// The EWAH bitmap embedded in `FSMN` could not be decoded.
    #[error("index fsmonitor extension EWAH bitmap is invalid")]
    FsmonitorExtensionEwahInvalid,

    /// The cache-tree extension references more entries than the index contains.
    #[error("corrupted cache-tree has entries not present in index")]
    CacheTreeCorrupt,

    /// `.git/index.lock` (or `GIT_INDEX_FILE.lock`) is held by another process.
    #[error(transparent)]
    LockHeld(#[from] LockHeldError),

    /// Filesystem I/O on the index or a related git-dir path.
    #[error(transparent)]
    PathIo(#[from] PathIoError),

    /// `update-index --chmod` / staging hint rejected the path or mode.
    #[error("cannot change mode of index path '{path}': {reason:?}")]
    Chmod {
        /// Repository-relative path (UTF-8 lossy when stored as bytes).
        path: String,
        /// Why the mode change was rejected.
        reason: IndexChmodReason,
    },

    /// Failure from another subsystem while building or persisting the index (for example ODB I/O
    /// during write-tree) that is not mapped to a dedicated [`IndexError`] leaf.
    #[error("{0}")]
    Nested(#[from] Box<crate::error::Error>),
}

impl IndexError {
    /// Stable snake_case kind name matching legacy [`crate::error::Error`] slugs.
    #[must_use]
    pub fn kind_name(&self) -> &'static str {
        match self {
            Self::Corrupt { .. } => "index_error",
            Self::Unmerged { .. } => "index_unmerged",
            Self::PathPrefixConflict { .. } => "index_path_prefix_conflict",
            Self::FsmonitorExtensionTruncated => "index_fsmonitor_extension_truncated",
            Self::FsmonitorExtensionBadVersion(_) => "index_fsmonitor_extension_bad_version",
            Self::FsmonitorExtensionEwahInvalid => "index_fsmonitor_extension_ewah_invalid",
            Self::CacheTreeCorrupt => "cache_tree_corrupt",
            Self::LockHeld(_) => "lock_held",
            Self::PathIo(_) => "git_dir_path_io",
            Self::Chmod { .. } => "message",
            Self::Nested(inner) => inner.kind_name(),
        }
    }

    /// Build a corrupt index error with a generated detail string.
    pub(crate) fn corrupt(corrupt: IndexCorrupt) -> Self {
        let detail = corrupt_detail(&corrupt);
        Self::Corrupt { corrupt, detail }
    }

    /// Build a corrupt index error with optional file path and byte offset.
    pub(crate) fn corrupt_at(
        path: Option<&Path>,
        offset: Option<usize>,
        reason: IndexCorruptReason,
    ) -> Self {
        let mut corrupt = IndexCorrupt::new(reason);
        if let Some(p) = path {
            corrupt = corrupt.at_path(p);
        }
        if let Some(o) = offset {
            corrupt = corrupt.at_offset(o);
        }
        Self::corrupt(corrupt)
    }

    /// Unmerged index with no path list (legacy `IndexUnmerged` shape).
    #[must_use]
    pub fn unmerged() -> Self {
        Self::Unmerged { paths: Vec::new() }
    }
}

fn corrupt_detail(corrupt: &IndexCorrupt) -> String {
    match corrupt.reason {
        IndexCorruptReason::BadSignature => "bad magic: expected DIRC".to_owned(),
        IndexCorruptReason::UnsupportedVersion { version } => {
            format!("unsupported index version {version}")
        }
        IndexCorruptReason::Truncated => "truncated".to_owned(),
        IndexCorruptReason::ChecksumMismatch { algo } => {
            format!("{} checksum mismatch", algo.name().to_uppercase())
        }
        IndexCorruptReason::BadEntry => "bad index entry".to_owned(),
        IndexCorruptReason::FileTooShortForHeader => {
            "file too short for index header and checksum".to_owned()
        }
        IndexCorruptReason::CannotReadVersion => "cannot read version".to_owned(),
        IndexCorruptReason::CannotReadEntryCount => "cannot read entry count".to_owned(),
        IndexCorruptReason::ParallelLoadMissingEoie => {
            "parallel index load missing EOIE offset".to_owned()
        }
        IndexCorruptReason::ExtensionOverrunsBody => "extension overruns index body".to_owned(),
        IndexCorruptReason::ExtensionTruncatedSig => "truncated extension sig".to_owned(),
        IndexCorruptReason::ExtensionTruncatedSize => "truncated extension size".to_owned(),
        IndexCorruptReason::JunkAfterExtensions => "junk after index extensions".to_owned(),
        IndexCorruptReason::UnsupportedExtension { sig } => {
            format!("index uses unsupported extension {sig:#010x}")
        }
        IndexCorruptReason::EntryRegionStartsPastEnd => {
            "index entry region starts past end of file".to_owned()
        }
        IndexCorruptReason::EntryCountMismatch {
            declared,
            max_fit,
            remaining,
        } => {
            format!("index declares {declared} entries but only {max_fit} fit in {remaining} bytes")
        }
        IndexCorruptReason::EntrySizeOverflow => "index entry size overflow".to_owned(),
        IndexCorruptReason::EntryOverrunsBody => "index entry overruns index body".to_owned(),
        IndexCorruptReason::EntryTooShort => "entry too short".to_owned(),
        IndexCorruptReason::TruncatedU32 => "truncated u32".to_owned(),
        IndexCorruptReason::TruncatedFlags => "truncated flags".to_owned(),
        IndexCorruptReason::TruncatedExtendedFlags => "truncated extended flags".to_owned(),
        IndexCorruptReason::V4PathMissingNul => "v4 entry path missing NUL".to_owned(),
        IndexCorruptReason::PathMissingNulTerminator => {
            "entry path missing NUL terminator".to_owned()
        }
        IndexCorruptReason::TruncatedIndexEntry => "truncated index entry".to_owned(),
        IndexCorruptReason::IeotBlockRangeOutOfBounds => {
            "IEOT block range out of bounds".to_owned()
        }
        IndexCorruptReason::IeotEntryCountOverflow => "IEOT entry count overflow".to_owned(),
        IndexCorruptReason::IeotBlockOffsetPastBody { block, offset } => {
            format!("IEOT block {block} offset {offset} past index body")
        }
        IndexCorruptReason::IeotBlockEntryOverruns { block, pos } => {
            format!("IEOT block {block} entry overruns index body at {pos}")
        }
        IndexCorruptReason::EmptyIeotTable => "empty IEOT table".to_owned(),
        IndexCorruptReason::IeotEntryCountMismatch { expected, got } => {
            format!("IEOT entry count mismatch: expected {expected}, got {got}")
        }
        IndexCorruptReason::CacheTreeUpdateFailed => "cache-tree update failed".to_owned(),
        IndexCorruptReason::WriteTreeMissingCacheTree => {
            "write-tree: missing cache-tree after update".to_owned()
        }
        IndexCorruptReason::WriteTreeCacheTreeRootMissingOid => {
            "write-tree: cache-tree root has no oid".to_owned()
        }
        IndexCorruptReason::WriteTreeInvalidPrefix { ref prefix } => {
            format!("write-tree: invalid prefix '{prefix}'")
        }
        IndexCorruptReason::WriteTreeCacheTreeChildMissing => {
            "cache-tree child missing oid".to_owned()
        }
        IndexCorruptReason::WriteTreeExpectedTree { oid } => {
            format!("expected tree at {oid}")
        }
        IndexCorruptReason::WriteTreeMissingIndexEntry => {
            "missing index entry during tree walk".to_owned()
        }
        IndexCorruptReason::ResolveUndoTruncatedPath => "truncated resolve-undo path".to_owned(),
        IndexCorruptReason::ResolveUndoTruncatedMode => "truncated resolve-undo mode".to_owned(),
        IndexCorruptReason::ResolveUndoInvalidModeUtf8 => {
            "invalid UTF-8 in resolve-undo mode".to_owned()
        }
        IndexCorruptReason::ResolveUndoInvalidMode { ref mode } => {
            format!("invalid resolve-undo mode '{mode}'")
        }
        IndexCorruptReason::ResolveUndoInvalidPathUtf8 => {
            "invalid UTF-8 in resolve-undo path".to_owned()
        }
        IndexCorruptReason::ResolveUndoCorrupt => "corrupt resolve-undo extension".to_owned(),
        IndexCorruptReason::ResolveUndoTruncatedOid => {
            "truncated resolve-undo object id".to_owned()
        }
        IndexCorruptReason::UntrackedCacheTruncated => {
            "untracked-cache extension truncated".to_owned()
        }
        IndexCorruptReason::UntrackedCacheInvalidVarint => {
            "untracked-cache invalid varint".to_owned()
        }
        IndexCorruptReason::UntrackedCacheMissingNul => {
            "untracked-cache payload missing NUL terminator".to_owned()
        }
        IndexCorruptReason::UntrackedCacheInvalidUtf8 => "untracked-cache invalid UTF-8".to_owned(),
        IndexCorruptReason::UntrackedCacheDirCorrupt => {
            "untracked-cache directory tree corrupt".to_owned()
        }
        IndexCorruptReason::UntrackedCacheNoRoot => "no uc root".to_owned(),
        IndexCorruptReason::SplitIndexCorrupt => "split index corrupt".to_owned(),
        IndexCorruptReason::SplitIndexTooShort => "index too short".to_owned(),
        IndexCorruptReason::ChecksumWorkerPanicked => {
            "index checksum worker thread panicked".to_owned()
        }
        IndexCorruptReason::MissingIndexEntryDuringTreeWalk => {
            "missing index entry during tree walk".to_owned()
        }
        IndexCorruptReason::MissingIndexEntryDuringHardReset => {
            "missing index entry during hard reset align".to_owned()
        }
        IndexCorruptReason::HardResetCacheTreeEntryCountMismatch => {
            "hard reset cache-tree entry count does not match index".to_owned()
        }
        IndexCorruptReason::HardResetCacheTreeOutOfSync => {
            "hard reset cache-tree rebuild: index out of sync with target tree".to_owned()
        }
        IndexCorruptReason::HardResetCacheTreeTypeMismatch => {
            "hard reset cache-tree rebuild: tree/index type mismatch".to_owned()
        }
    }
}

impl From<crate::lockfile::LockError> for IndexError {
    fn from(err: crate::lockfile::LockError) -> Self {
        use crate::lockfile::LockError;
        match err {
            LockError::Held { path } => Self::LockHeld(LockHeldError {
                holder: crate::lockfile::LockHolder::read(&path),
                path,
            }),
            LockError::Io { path, source } => Self::PathIo(PathIoError {
                operation: PathIoOperation::GitDir(GitDirPathOperation::WriteIndex),
                path: Some(path),
                source,
            }),
        }
    }
}

impl From<crate::error::ConfigError> for IndexError {
    fn from(err: crate::error::ConfigError) -> Self {
        Self::from(crate::error::Error::Config(err))
    }
}

impl From<IndexError> for crate::error::Error {
    fn from(err: IndexError) -> Self {
        match err {
            IndexError::LockHeld(h) => Self::LockHeld(h),
            IndexError::PathIo(p) => p.into(),
            IndexError::Nested(inner) => *inner,
            IndexError::Unmerged { .. } => Self::Index(err),
            IndexError::PathPrefixConflict { .. } => Self::Index(err),
            IndexError::FsmonitorExtensionTruncated => Self::Index(err),
            IndexError::FsmonitorExtensionBadVersion(_) => Self::Index(err),
            IndexError::FsmonitorExtensionEwahInvalid => Self::Index(err),
            IndexError::CacheTreeCorrupt => Self::Index(err),
            IndexError::Corrupt { .. } | IndexError::Chmod { .. } => Self::Index(err),
        }
    }
}

impl From<std::io::Error> for IndexError {
    fn from(source: std::io::Error) -> Self {
        Self::PathIo(PathIoError {
            operation: crate::error::PathIoOperation::GitDir(
                crate::error::GitDirPathOperation::WriteIndex,
            ),
            path: None,
            source,
        })
    }
}

impl From<crate::error::Error> for IndexError {
    #[allow(deprecated)]
    fn from(err: crate::error::Error) -> Self {
        match err {
            crate::error::Error::IndexError(detail) => Self::Corrupt {
                corrupt: IndexCorrupt::new(IndexCorruptReason::Truncated),
                detail,
            },
            crate::error::Error::IndexUnmerged => Self::unmerged(),
            crate::error::Error::IndexPathPrefixConflict { directory, file } => {
                Self::PathPrefixConflict { directory, file }
            }
            crate::error::Error::IndexFsmonitorExtensionTruncated => {
                Self::FsmonitorExtensionTruncated
            }
            crate::error::Error::IndexFsmonitorExtensionBadVersion(v) => {
                Self::FsmonitorExtensionBadVersion(v)
            }
            crate::error::Error::IndexFsmonitorExtensionEwahInvalid => {
                Self::FsmonitorExtensionEwahInvalid
            }
            crate::error::Error::CacheTreeCorrupt => Self::CacheTreeCorrupt,
            crate::error::Error::Index(index) => index,
            crate::error::Error::LockHeld(h) => Self::LockHeld(h),
            crate::error::Error::Io(source) => Self::PathIo(PathIoError {
                operation: PathIoOperation::GitDir(GitDirPathOperation::WriteIndex),
                path: None,
                source,
            }),
            crate::error::Error::Repository(crate::repo::RepositoryError::GitDirPathIo(source)) => {
                Self::PathIo(source)
            }
            crate::error::Error::WorktreePathIo {
                operation,
                path,
                source,
            } => Self::PathIo(PathIoError {
                operation: PathIoOperation::Worktree(operation),
                path: Some(PathBuf::from(path)),
                source,
            }),
            other => IndexError::Nested(Box::new(other)),
        }
    }
}

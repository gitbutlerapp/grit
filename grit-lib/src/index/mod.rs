//! Git index (staging area) read/write and related cache extensions.
//!
//! Library guide: [mod](https://grit-scm.com/docs/library/staging)
//!
//! ```rust
//! use grit_lib::repo::{InitOptions, Repository};
//! # fn main() -> grit_lib::error::Result<()> {
//! let dir = tempfile::tempdir()?;
//! let (_repo, _) = Repository::init(dir.path(), &InitOptions::default())?;
//! let _ = _repo.index().load(&grit_lib::index::IndexLoadOptions::default())?;
//!
//! # Ok(())
//! # }
//! ```
mod core;
pub mod error;
pub(crate) mod handle;
mod mode_policy;

#[cfg(test)]
pub(crate) use core::index_path_cmp;
#[cfg(windows)]
pub(crate) use core::metadata_times;
pub use core::CacheTreeNode;
pub(crate) use core::{
    advance_index_file_mtime_past_entries, apply_fsmonitor_dirty_to_entries, canon_mode,
    index_path_outcome_key, index_skip_hash_for_write, is_tree_mode, max_stage0_mtime_pair,
    opportunistic_index_write_still_current, parse_cache_tree, parse_entry,
    parse_fsmonitor_extension, read_index_on_disk_identity, stage0_mtime_max, IndexStatFields,
    INDEX_EXT_CACHE_TREE, INDEX_EXT_FSMONITOR, INDEX_EXT_LINK, INDEX_EXT_RESOLVE_UNDO,
    INDEX_EXT_SPARSE_DIRECTORIES, INDEX_EXT_UNTRACKED,
};
pub use core::{
    entry_from_metadata, entry_from_stat, index_entries_stat_equivalent, index_file_mtime,
    munge_index_size, normalize_mode, worktree_path_from_index_rel, Index, IndexEntry, IndexLock,
    IndexParseContext, IndexParseOptions, StatMatchPolicy, MODE_EXECUTABLE, MODE_GITLINK,
    MODE_REGULAR, MODE_SYMLINK, MODE_TREE,
};
pub use error::{IndexChmodReason, IndexCorrupt, IndexCorruptReason, IndexError};
pub use handle::{IndexFile, IndexLoadOptions, IndexWriteOptions, SparseIndexLoad};

/// Legacy name for [`IndexFile`].
pub type IndexHandle<'repo> = IndexFile<'repo>;
pub use mode_policy::WorktreeModePolicy;

pub mod sparse_checkout {
    //! sparse_checkout API (re-exported at this path).
    pub use crate::sparse_checkout::SparseCheckout;
}
pub mod split_index {
    //! split_index API (re-exported at this path).

    pub use crate::split_index::{
        format_dump_split_index_file, resolve_split_index_if_needed, WriteSplitIndexRequest,
    };
}
pub mod untracked_cache {
    //! untracked_cache API (re-exported at this path).

    pub use crate::untracked_cache::OidStat;
    pub use crate::untracked_cache::StatDataDisk;
    pub use crate::untracked_cache::{
        write_untracked_extension, UntrackedCache, UntrackedCacheDir, UntrackedIgnoredMode,
        AUTO_UNTRACKED_CACHE_MIN_ENTRIES, DIR_HIDE_EMPTY_DIRECTORIES, DIR_SHOW_OTHER_DIRECTORIES,
    };
}
pub mod resolve_undo {
    //! resolve_undo API (re-exported at this path).

    pub use crate::resolve_undo::{
        parse_resolve_undo_payload, write_resolve_undo_payload, ResolveUndoRecord,
    };
}
pub mod index_name_hash_lazy {
    //! index_name_hash_lazy API (re-exported at this path).
    pub use crate::index_name_hash_lazy::memihash;
}
pub mod write_tree {
    //! write_tree API (re-exported at this path).
    pub(crate) use crate::write_tree::cache_tree_extension_valid;
    pub use crate::write_tree::WriteTreePersistence;
    pub use crate::write_tree::{
        build_cache_tree_from_tree, write_tree_from_index, write_tree_update_index, WriteTreeFlags,
    };
}

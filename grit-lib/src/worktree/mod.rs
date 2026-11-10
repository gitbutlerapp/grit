//! Working tree, ignore/attributes, and checkout path helpers.
//!
//! Library guide: [mod](https://grit-scm.com/docs/library/worktree)
//!
//! ```rust
//! use grit_lib::repo::{InitOptions, Repository};
//! # fn main() -> grit_lib::error::Result<()> {
//! let dir = tempfile::tempdir()?;
//! let (_repo, _) = Repository::init(dir.path(), &InitOptions::default())?;
//! let _wt = _repo.work_tree();
//!
//! # Ok(())
//! # }
//! ```
mod core;
mod error;
pub(crate) mod io_error;
pub(crate) mod linked_worktree_error;
mod move_repair;
pub(crate) mod symlink;

#[cfg(feature = "test-support")]
pub(crate) use core::copy_filtered_worktree_config;
pub(crate) use core::{
    add_linked_worktree, add_linked_worktree_with_cancel, admin_dirs_match,
    ensure_branch_not_checked_out_elsewhere, ensure_branch_not_checked_out_in_any_worktree,
    find_worktree_with_branch_checked_out, list_worktrees, lock_linked_worktree,
    prune_linked_worktrees, read_worktree_path, remove_linked_worktree, unlock_linked_worktree,
    worktree_admin_dir,
};
pub use core::{
    resolve_linked_worktree_add_path, AddLinkedWorktreeOptions, LockWorktreeOptions,
    PrunableReason, PruneWorktreesOptions, PrunedWorktreeEntry, WorktreeBranch, WorktreeEntry,
};
pub use error::{LinkedWorktreeError, LinkedWorktreeOperation, WorktreeError};
pub(crate) use io_error::{io_with_path, map_worktree_io, WorktreePathOperation};
pub(crate) use move_repair::{move_linked_worktree, repair_worktrees};
pub use move_repair::{
    resolve_move_destination, MoveWorktreeOptions, WorktreeRepair, WorktreeRepairKind,
};
pub(crate) use symlink::{
    create_worktree_symlink, create_worktree_symlink_from_blob, symlink_target_path_from_bytes,
    write_index_symlink_to_worktree,
};

pub mod attributes {
    //! attributes API (re-exported at this path).

    pub use crate::attributes::AttrRule;
    pub use crate::attributes::{
        attribute_matching_ignore_case, builtin_objectmode_index, collect_attrs_for_path,
        load_gitattributes_for_check_attr_cached, load_gitattributes_for_check_attr_source,
        load_gitattributes_from_index, load_gitattributes_stack, normalize_rel_path,
        path_relative_to_worktree, resolve_tree_oid, AttrValue, MacroTable, ParsedGitAttributes,
        MAX_ATTR_FILE_BYTES, MAX_ATTR_LINE_BYTES,
    };
}
pub mod crlf {
    //! crlf API (re-exported at this path).

    pub use crate::crlf::AutoCrlf;
    pub use crate::crlf::CoreEol;
    pub use crate::crlf::CrlfLegacyAttr;
    pub use crate::crlf::DiffAttr;
    pub use crate::crlf::EolAttr;
    pub use crate::crlf::MergeAttr;
    pub use crate::crlf::SafeCrlf;
    pub use crate::crlf::TextAttr;
    pub use crate::crlf::{
        clean_uses_autocrlf_index_guard, collapse_ident, convert_to_git, convert_to_worktree_eager,
        crlf_to_lf, gather_convert_stats_ascii, has_crlf, has_crlf_in_index_blob, has_lone_lf,
        is_all_crlf, is_all_lf, is_binary, lf_to_crlf, load_gitattributes,
        load_gitattributes_for_checkout, load_gitattributes_from_index,
        parse_gitattributes_content, run_filter, should_convert_to_crlf, ConversionConfig,
        ConvertToGitOpts, FileAttrs,
    };
}
pub mod ignore {
    //! ignore API (re-exported at this path).
    pub use crate::ignore::IgnorePatternMatch;
    pub use crate::ignore::{
        normalize_repo_relative, parse_sparse_patterns_from_blob, path_matches_sparse_pattern_list,
        IgnoreCheck, IgnoreMatch, IgnoreMatcher, IgnorePatternList, IgnoreSource,
        RepoPathNormalizer,
    };
}
pub mod fsmonitor {
    //! fsmonitor API (re-exported at this path).
    pub use crate::fsmonitor_root::FsmonitorQueryBoundary;
    pub(crate) use crate::fsmonitor_root::{
        entry_skips_worktree_stat, fsmonitor_enabled, mark_fsmonitor_valid_proven_clean,
        mark_fsmonitor_valid_proven_clean_path, refresh_index, FsmonitorRefreshOptions,
    };
}
pub mod worktree_cwd {
    //! worktree_cwd API (re-exported at this path).
    pub use crate::worktree_cwd::cwd_would_be_removed_with_dir;
}
pub mod worktree_ref {
    //! worktree_ref API (re-exported at this path).
    pub use crate::worktree_ref::RefWorktreeType;
}
pub mod worktree_rules {
    //! worktree_rules API (re-exported at this path).
    #[cfg(test)]
    pub(crate) use crate::worktree_rules::file_load_counters;

    pub use crate::worktree_rules::WorktreeRules;
}
pub mod pathspec {
    //! pathspec API (re-exported at this path).
    pub use crate::error::{PathspecFromFileParseError, PathspecResolveError};
    pub use crate::pathspec::{
        bloom_lookup_prefix, bloom_lookup_prefix_with_cwd, has_glob_chars,
        is_long_magic_pathspec_token, parse_pathspecs_from_source, path_allowed_by_pathspec_list,
        pathdiff, pathspec_contributes_match, pathspec_has_top, pathspec_is_exclude,
        pathspec_wants_descent_into_tree, precompose_argv_pathspec,
        resolve_pathspec_with_argv_precompose, simple_length, validate_attr_pathspecs, Pathspec,
        PathspecFile, PathspecGlobalFlags, PathspecGlobalsError, PathspecOptions,
        PathspecParseError,
    };
}
pub mod wildmatch {
    //! wildmatch API (re-exported at this path).
    pub use crate::wildmatch::{wildmatch, WM_CASEFOLD, WM_PATHNAME};
}

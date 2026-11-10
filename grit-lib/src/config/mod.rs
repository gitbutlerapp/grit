//! Configuration cascade, identity, and repository bootstrap helpers.
//!
//! Library guide: [mod](https://grit-scm.com/docs/library/config)
//!
//! ```rust
//! use grit_lib::repo::{InitOptions, Repository};
//! # fn main() -> grit_lib::error::Result<()> {
//! let dir = tempfile::tempdir()?;
//! let (_repo, _) = Repository::init(dir.path(), &InitOptions::default())?;
//! let _cfg = _repo.config().snapshot()?;
//!
//! # Ok(())
//! # }
//! ```
mod core;
pub(crate) mod handle;
mod location;
mod typed;
mod write;

#[cfg(test)]
pub(crate) use core::cascade_load_counters;
pub use core::{
    canonical_key, config_entry_origin_file, config_file_display_for_error, config_lock_path,
    get_urlmatch_all_in_section, get_urlmatch_entries, git_config_parameters_last_value,
    global_config_paths_pub, parse_bool, parse_color, parse_config_parameters,
    parse_git_config_int, parse_git_config_int_strict, parse_git_config_maybe_bool, parse_i64,
    parse_path, parse_path_optional, resolve_diff_context_lines, system_config_path, url_matches,
    ConfigEntry, ConfigFile, ConfigIncludeOrigin, ConfigScope, ConfigSet, ConfigValueOrigin,
    GitConfigIntStrictError, IncludeContext, IncludeMode, IncludeOptions, LoadConfigOptions,
};
pub(crate) use core::{
    config_cache_lookup, config_cache_store, config_file_stamps, resolve_config_override_path,
    stamp_for_path, stamp_paths, ConfigCacheEntry, ConfigCacheEntryStamps, ConfigCacheKey,
};
pub use handle::Config;
pub use location::{global_config_write_path, ConfigLocation, ConfigWritePreview};
pub use typed::{
    canonicalize_config_write_value, parse_typed_value, ConfigValueType, TypedConfigValue,
};
pub use write::{ConfigEditor, ConfigWriteOptions};

pub mod dotfile {
    //! dotfile API (re-exported at this path).
    pub use crate::dotfile::{
        dotfile_matches, is_hfs_dot_gitattributes, is_hfs_dot_gitignore, is_hfs_dot_gitmodules,
        is_hfs_dot_mailmap, is_ntfs_dot_gitattributes, is_ntfs_dot_gitignore,
        is_ntfs_dot_gitmodules, is_ntfs_dot_mailmap,
    };
}
pub mod environment {
    //! environment API (re-exported at this path).
    pub use crate::environment::{
        offset_from_epoch_and_tz, ConfigOverride, Environment, RepositoryOptions,
    };
}
pub mod gitmodules {
    //! gitmodules API (re-exported at this path).
    pub use crate::gitmodules::{
        check_submodule_name, check_submodule_url, looks_like_command_line_option,
        tree_entry_is_gitattributes_blob, tree_entry_is_gitmodules_blob,
        validate_gitmodules_blob_line, DotFsckIssue,
    };

    /// Submodule `.git` file resolution.
    pub mod gitdir {
        pub use crate::submodule_gitdir::{
            compute_default_submodule_gitdir, connect_submodule_work_tree_and_git_dir,
            ensure_submodule_gitdir_config, hash_blob_sha1_hex, init_submodule_head_from_gitlink,
            is_git_directory, migrate_gitdir_configs, path_inside_indexed_submodule,
            path_inside_registered_submodule, path_inside_registered_submodule_name,
            set_submodule_repo_worktree, submodule_gitdir_filesystem_path,
            validate_encoded_submodule_git_dir, validate_submodule_path, write_submodule_gitfile,
        };
    }
}
pub mod ident_resolve {
    //! ident_resolve API (re-exported at this path).

    pub use crate::ident_resolve::IdentityEnv;
    pub use crate::ident_resolve::{
        resolve_email_with, resolve_identity_with, resolve_loose_committer_identity_with,
        resolve_signature_with, IdentRole, IdentityError,
    };
}
pub mod init_filesystem {
    //! init_filesystem API (re-exported at this path).

    pub use crate::init_filesystem::{
        FilesystemCapabilities, FilesystemCapabilitiesError, InitFilesystemConfigOptions,
        PrecomposeDetection,
    };
}
pub mod precompose_config {
    //! precompose_config API (re-exported at this path).
    pub use crate::precompose_config::{
        argv_precompose_enabled, effective_core_precomposeunicode, locate_git_dir_from_environment,
        pathspec_precompose_enabled,
    };
}
pub mod git_date {
    //! git_date API (re-exported at this path).
    pub use crate::git_date::DateModeType;
    pub use crate::git_date::DateSpecError;
    pub use crate::git_date::{
        approx, parse_date_cutoff, parse_date_format, refresh_process_tz, show_date, DateMode,
    };
}
pub mod git_path {
    //! git_path API (re-exported at this path).
    pub use crate::git_path::GitPathError;
    pub use crate::git_path::{
        absolute_path_for_gitfile_stored_path, abspath_part_inside_repo, cleanup_path,
        git_path_relative_component, is_common_git_path, longest_ancestor_length,
        normalize_path_copy, path_for_disk_compare, path_for_error_message,
        path_lexical_for_disk_compare, prefix_path_gently, relative_path,
        relative_path_for_display, user_display_path,
    };
}
pub mod mailmap {
    //! mailmap API (re-exported at this path).

    pub use crate::mailmap::MailmapEntry;
    pub use crate::mailmap::{
        map_contact_table, parse_contact, parse_mailmap, render_contact, table_from_entries,
        MailmapTable,
    };
}

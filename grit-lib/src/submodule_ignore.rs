//! Submodule ignore policy (`diff.ignoreSubmodules`, `submodule.<name>.ignore`, `.gitmodules`).
//!
//! Matches Git's `submodule-config` precedence for worktree diffs used by `status` and `diff`.

use std::path::Path;

use crate::config::{ConfigFile, ConfigScope, ConfigSet};

/// Submodule ignore mode for worktree gitlink comparisons.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SubmoduleIgnoreMode {
    /// Report new submodule commits and dirty tracked content inside the checkout.
    #[default]
    None,
    /// Ignore untracked paths inside the submodule only.
    Untracked,
    /// Ignore dirty worktrees when the recorded gitlink commit matches the checkout HEAD.
    Dirty,
    /// Omit gitlink worktree drift when the checkout HEAD matches the recorded gitlink, and also
    /// omit a moved submodule HEAD (Git `ignore=all`).
    All,
}

/// Parse `none|untracked|dirty|all`. Unknown values fall back to [`SubmoduleIgnoreMode::None`].
#[must_use]
pub fn parse_submodule_ignore_value(raw: &str) -> SubmoduleIgnoreMode {
    match raw.trim().to_ascii_lowercase().as_str() {
        "untracked" => SubmoduleIgnoreMode::Untracked,
        "dirty" => SubmoduleIgnoreMode::Dirty,
        "all" => SubmoduleIgnoreMode::All,
        _ => SubmoduleIgnoreMode::None,
    }
}

/// Submodule logical name for a gitlink path (defaults to the path when unregistered).
#[must_use]
pub fn submodule_name_for_gitlink_path(cfg: &ConfigSet, gitlink_path: &str) -> String {
    let path_norm = gitlink_path.replace('\\', "/");
    submodule_name_from_config_paths(cfg, &path_norm).unwrap_or(path_norm)
}

fn submodule_name_from_config_paths(cfg: &ConfigSet, path: &str) -> Option<String> {
    for e in cfg.entries() {
        let key = &e.key;
        if let Some(rest) = key.strip_prefix("submodule.") {
            if let Some(name) = rest.strip_suffix(".path") {
                if e.value
                    .as_deref()
                    .is_some_and(|p| p.replace('\\', "/") == path)
                {
                    return Some(name.to_owned());
                }
            }
        }
    }
    None
}

fn gitmodules_submodule_value(work_tree: &Path, name: &str, field: &str) -> Option<String> {
    let gitmodules = work_tree.join(".gitmodules");
    let content = std::fs::read_to_string(&gitmodules).ok()?;
    let (entries, _) =
        ConfigFile::parse_gitmodules_best_effort(&gitmodules, &content, ConfigScope::Local);
    let suffix = format!(".{field}");
    let prefix = format!("submodule.{name}");
    entries
        .into_iter()
        .find(|e| e.key.starts_with(&prefix) && e.key.ends_with(&suffix))
        .and_then(|e| e.value)
}

/// Effective ignore mode for `gitlink_path` in the superproject work tree.
///
/// Precedence: `submodule.<name>.ignore` in the loaded [`ConfigSet`], then the same key from
/// `.gitmodules`, then `diff.ignoreSubmodules`, default [`SubmoduleIgnoreMode::None`].
#[must_use]
pub fn submodule_ignore_for_path(
    cfg: &ConfigSet,
    work_tree: &Path,
    gitlink_path: &str,
) -> SubmoduleIgnoreMode {
    let name = submodule_name_for_gitlink_path(cfg, gitlink_path);
    let per_name_key = format!("submodule.{name}.ignore");
    if let Some(v) = cfg.get(&per_name_key) {
        return parse_submodule_ignore_value(&v);
    }
    if let Some(v) = gitmodules_submodule_value(work_tree, &name, "ignore") {
        return parse_submodule_ignore_value(&v);
    }
    if let Some(v) = cfg.get("diff.ignoreSubmodules") {
        return parse_submodule_ignore_value(&v);
    }
    SubmoduleIgnoreMode::None
}

/// Whether a gitlink worktree diff entry should be omitted given ignore policy.
///
/// `ref_matches` is true when the submodule checkout HEAD equals the parent index gitlink OID.
#[must_use]
pub fn gitlink_worktree_change_suppressed(
    ignore: SubmoduleIgnoreMode,
    ref_matches: bool,
    flags: crate::diff::SubmodulePorcelainFlags,
) -> bool {
    match ignore {
        SubmoduleIgnoreMode::All => true,
        _ if !ref_matches => false,
        SubmoduleIgnoreMode::None => false,
        SubmoduleIgnoreMode::Untracked => !flags.modified && flags.untracked,
        SubmoduleIgnoreMode::Dirty => flags.modified || flags.untracked,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::diff::SubmodulePorcelainFlags;

    #[test]
    fn parse_submodule_ignore_rejects_unknown() {
        assert_eq!(
            parse_submodule_ignore_value("bogus"),
            SubmoduleIgnoreMode::None
        );
        assert_eq!(
            parse_submodule_ignore_value("all"),
            SubmoduleIgnoreMode::All
        );
    }

    #[test]
    fn gitlink_suppressed_for_dirty_and_all_when_ref_matches() {
        let flags = SubmodulePorcelainFlags {
            modified: true,
            ..Default::default()
        };
        assert!(gitlink_worktree_change_suppressed(
            SubmoduleIgnoreMode::Dirty,
            true,
            flags
        ));
        assert!(gitlink_worktree_change_suppressed(
            SubmoduleIgnoreMode::All,
            true,
            flags
        ));
        assert!(!gitlink_worktree_change_suppressed(
            SubmoduleIgnoreMode::Dirty,
            false,
            flags
        ));
        assert!(gitlink_worktree_change_suppressed(
            SubmoduleIgnoreMode::All,
            false,
            flags
        ));
    }

    #[test]
    fn gitlink_untracked_only_honors_untracked_mode() {
        let flags = SubmodulePorcelainFlags {
            untracked: true,
            ..Default::default()
        };
        assert!(gitlink_worktree_change_suppressed(
            SubmoduleIgnoreMode::Untracked,
            true,
            flags
        ));
        assert!(!gitlink_worktree_change_suppressed(
            SubmoduleIgnoreMode::None,
            true,
            flags
        ));
    }
}

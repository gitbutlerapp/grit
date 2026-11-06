//! Submodule ignore policy (`diff.ignoreSubmodules`, `submodule.<name>.ignore`, `.gitmodules`).
//!
//! Matches Git's `submodule-config` precedence for worktree diffs used by `status` and `diff`.

use std::path::Path;

use crate::config::{ConfigEntry, ConfigFile, ConfigScope, ConfigSet};

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

/// Parsed `.gitmodules` entries for path→name mapping and per-name fields.
struct GitmodulesFile {
    entries: Vec<ConfigEntry>,
}

impl GitmodulesFile {
    fn load(work_tree: &Path) -> Option<Self> {
        let gitmodules = work_tree.join(".gitmodules");
        let content = std::fs::read_to_string(&gitmodules).ok()?;
        let (entries, _) =
            ConfigFile::parse_gitmodules_best_effort(&gitmodules, &content, ConfigScope::Local);
        Some(Self { entries })
    }

    fn name_for_path(&self, path: &str) -> Option<String> {
        for e in &self.entries {
            let key = &e.key;
            let Some(rest) = key.strip_prefix("submodule.") else {
                continue;
            };
            let Some(name) = rest.strip_suffix(".path") else {
                continue;
            };
            if e.value
                .as_deref()
                .is_some_and(|p| p.replace('\\', "/") == path)
            {
                return Some(name.to_owned());
            }
        }
        None
    }

    fn field_value(&self, name: &str, field: &str) -> Option<String> {
        let key = format!("submodule.{name}.{field}");
        // Git config uses last value wins for duplicate keys in one file.
        self.entries
            .iter()
            .rev()
            .find(|e| e.key == key)
            .and_then(|e| e.value.clone())
    }
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

/// Submodule logical name for a gitlink path (defaults to the path when unregistered).
#[must_use]
pub fn submodule_name_for_gitlink_path(
    cfg: &ConfigSet,
    work_tree: &Path,
    gitlink_path: &str,
) -> String {
    let path_norm = gitlink_path.replace('\\', "/");
    submodule_name_from_config_paths(cfg, &path_norm)
        .or_else(|| GitmodulesFile::load(work_tree).and_then(|gm| gm.name_for_path(&path_norm)))
        .unwrap_or(path_norm)
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
    let name = submodule_name_for_gitlink_path(cfg, work_tree, gitlink_path);
    let per_name_key = format!("submodule.{name}.ignore");
    if let Some(v) = cfg.value(&per_name_key) {
        return parse_submodule_ignore_value(&v);
    }
    if let Some(gm) = GitmodulesFile::load(work_tree) {
        if let Some(v) = gm.field_value(&name, "ignore") {
            return parse_submodule_ignore_value(&v);
        }
    }
    if let Some(v) = cfg.value("diff.ignoreSubmodules") {
        return parse_submodule_ignore_value(&v);
    }
    SubmoduleIgnoreMode::None
}

/// Whether staged index-vs-`HEAD` diffs should omit this gitlink (`git diff --cached` with ignore=all).
#[must_use]
pub fn staged_gitlink_omitted_in_diff(
    cfg: &ConfigSet,
    work_tree: &Path,
    gitlink_path: &str,
) -> bool {
    submodule_ignore_for_path(cfg, work_tree, gitlink_path) == SubmoduleIgnoreMode::All
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
    use std::fs;
    use tempfile::tempdir;

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

    #[test]
    fn gitmodules_logical_name_resolves_ignore_key() {
        let dir = tempdir().expect("tempdir");
        fs::write(
            dir.path().join(".gitmodules"),
            "[submodule \"logical\"]\n\tpath = vendor/lib\n\tignore = all\n",
        )
        .expect("gitmodules");
        let cfg = ConfigSet::new();
        assert_eq!(
            submodule_name_for_gitlink_path(&cfg, dir.path(), "vendor/lib"),
            "logical"
        );
        assert_eq!(
            submodule_ignore_for_path(&cfg, dir.path(), "vendor/lib"),
            SubmoduleIgnoreMode::All
        );
    }

    #[test]
    fn gitmodules_field_keys_do_not_prefix_match_longer_names() {
        let dir = tempdir().expect("tempdir");
        fs::write(
            dir.path().join(".gitmodules"),
            "[submodule \"foo\"]\n\tpath = a\n\tignore = dirty\n[submodule \"foobar\"]\n\tpath = b\n\tignore = all\n",
        )
        .expect("gitmodules");
        let cfg = ConfigSet::new();
        assert_eq!(
            submodule_ignore_for_path(&cfg, dir.path(), "a"),
            SubmoduleIgnoreMode::Dirty
        );
        assert_eq!(
            submodule_ignore_for_path(&cfg, dir.path(), "b"),
            SubmoduleIgnoreMode::All
        );
    }

    #[test]
    fn gitmodules_duplicate_ignore_uses_last_value() {
        let dir = tempdir().expect("tempdir");
        fs::write(
            dir.path().join(".gitmodules"),
            "[submodule \"logical\"]\n\tpath = vendor/lib\n\tignore = all\n\tignore = none\n",
        )
        .expect("gitmodules");
        let cfg = ConfigSet::new();
        assert_eq!(
            submodule_ignore_for_path(&cfg, dir.path(), "vendor/lib"),
            SubmoduleIgnoreMode::None
        );
    }
}

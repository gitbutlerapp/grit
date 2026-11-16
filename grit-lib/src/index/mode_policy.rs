//! Mapping worktree file modes onto index modes (`core.filemode`, `core.symlinks`).

use crate::config::ConfigSet;

use crate::objects::FileMode;

/// What the worktree filesystem can express about a tracked path's mode.
///
/// Git records these capabilities per repository as `core.filemode` (is the executable bit
/// meaningful?) and `core.symlinks` (can the checkout hold symbolic links?). Both default to
/// `true`; `git init`/`git clone` write `false` when probing the filesystem fails, which is the
/// norm on Windows. When a capability is missing, the worktree cannot show that aspect of a
/// path, so the mode already recorded in the index wins over what `lstat` reports.
///
/// Use [`WorktreeModePolicy::index_mode_for_worktree`] whenever a worktree mode is compared
/// with or written over an index entry: staging, status and worktree diffs all share it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub struct WorktreeModePolicy {
    /// `core.filemode`: the worktree's executable bit is meaningful.
    pub trust_filemode: bool,
    /// `core.symlinks`: the worktree can hold symbolic links.
    pub has_symlinks: bool,
}

impl Default for WorktreeModePolicy {
    fn default() -> Self {
        Self {
            trust_filemode: true,
            has_symlinks: true,
        }
    }
}

impl WorktreeModePolicy {
    /// Read `core.filemode` and `core.symlinks` from `config`, defaulting each to `true` when
    /// unset or not a boolean.
    #[must_use]
    pub fn from_config(config: &ConfigSet) -> Self {
        let flag = |key: &str| config.bool_with_default(key, true);
        Self {
            trust_filemode: flag("core.filemode"),
            has_symlinks: flag("core.symlinks"),
        }
    }

    /// The mode to record for a path given its worktree mode and the mode currently recorded.
    ///
    /// Rules, in order:
    ///
    /// 1. Without symlink support, a regular file standing in for a recorded symlink
    ///    ([`FileMode::SYMLINK`]) stays a symlink: the file holds the link target.
    /// 2. Without a trusted executable bit, a regular file over a recorded regular file keeps the
    ///    recorded `100644`/`100755`.
    /// 3. Otherwise the worktree mode wins (including real type changes such as a directory, a
    ///    symlink on a symlink-capable checkout, or a gitlink).
    ///
    /// # Parameters
    ///
    /// - `worktree_mode` — Git mode derived from `lstat` (same mapping as the diff worktree helpers).
    /// - `recorded_mode` — the index entry's mode, or `None` for a path not yet tracked (the
    ///   worktree mode is then returned unchanged).
    ///
    /// # Returns
    ///
    /// The Git mode to compare against or store in the index.
    #[must_use]
    pub fn index_mode_for_worktree(
        self,
        worktree_mode: FileMode,
        recorded_mode: Option<FileMode>,
    ) -> FileMode {
        let Some(recorded) = recorded_mode else {
            return worktree_mode;
        };
        if !is_regular_file_mode(worktree_mode) {
            return worktree_mode;
        }
        if !self.has_symlinks && recorded == FileMode::SYMLINK {
            return FileMode::SYMLINK;
        }
        if !self.trust_filemode && is_regular_file_mode(recorded) {
            return recorded;
        }
        worktree_mode
    }

    /// Whether a worktree mode is indistinguishable from `recorded_mode` under this policy.
    ///
    /// Equivalent to `index_mode_for_worktree(worktree_mode, Some(recorded_mode)) == recorded_mode`.
    #[must_use]
    pub fn worktree_mode_matches(self, worktree_mode: FileMode, recorded_mode: FileMode) -> bool {
        self.index_mode_for_worktree(worktree_mode, Some(recorded_mode)) == recorded_mode
    }

    /// Sets [`trust_filemode`](Self::trust_filemode).
    #[must_use]
    pub const fn with_trust_filemode(mut self, trust_filemode: bool) -> Self {
        self.trust_filemode = trust_filemode;
        self
    }

    /// Sets [`has_symlinks`](Self::has_symlinks).
    #[must_use]
    pub const fn with_has_symlinks(mut self, has_symlinks: bool) -> Self {
        self.has_symlinks = has_symlinks;
        self
    }
}

/// Exactly [`FileMode::REGULAR`] or [`FileMode::EXECUTABLE`] (index modes are canonical).
const fn is_regular_file_mode(mode: FileMode) -> bool {
    matches!(mode, FileMode::REGULAR | FileMode::EXECUTABLE)
}

#[cfg(test)]
mod tests {
    use super::*;

    const NO_SYMLINKS: WorktreeModePolicy = WorktreeModePolicy {
        trust_filemode: true,
        has_symlinks: false,
    };
    const NO_FILEMODE: WorktreeModePolicy = WorktreeModePolicy {
        trust_filemode: false,
        has_symlinks: true,
    };

    #[test]
    fn default_trusts_worktree() {
        let p = WorktreeModePolicy::default();
        assert_eq!(
            p.index_mode_for_worktree(FileMode::REGULAR, Some(FileMode::SYMLINK)),
            FileMode::REGULAR
        );
        assert_eq!(
            p.index_mode_for_worktree(FileMode::EXECUTABLE, Some(FileMode::REGULAR)),
            FileMode::EXECUTABLE
        );
        assert!(!p.worktree_mode_matches(FileMode::REGULAR, FileMode::SYMLINK));
    }

    #[test]
    fn no_symlinks_keeps_recorded_symlink_for_regular_file() {
        for wt in [FileMode::REGULAR, FileMode::EXECUTABLE] {
            assert_eq!(
                NO_SYMLINKS.index_mode_for_worktree(wt, Some(FileMode::SYMLINK)),
                FileMode::SYMLINK
            );
            assert!(NO_SYMLINKS.worktree_mode_matches(wt, FileMode::SYMLINK));
        }
        // The executable bit is still trusted for real regular files.
        assert_eq!(
            NO_SYMLINKS.index_mode_for_worktree(FileMode::EXECUTABLE, Some(FileMode::REGULAR)),
            FileMode::EXECUTABLE
        );
        // A real symlink on disk is still a symlink.
        assert_eq!(
            NO_SYMLINKS.index_mode_for_worktree(FileMode::SYMLINK, Some(FileMode::REGULAR)),
            FileMode::SYMLINK
        );
    }

    #[test]
    fn no_filemode_keeps_recorded_regular_mode() {
        assert_eq!(
            NO_FILEMODE.index_mode_for_worktree(FileMode::EXECUTABLE, Some(FileMode::REGULAR)),
            FileMode::REGULAR
        );
        assert_eq!(
            NO_FILEMODE.index_mode_for_worktree(FileMode::REGULAR, Some(FileMode::EXECUTABLE)),
            FileMode::EXECUTABLE
        );
        // Symlink support is trusted: a file over a symlink entry is a type change.
        assert_eq!(
            NO_FILEMODE.index_mode_for_worktree(FileMode::REGULAR, Some(FileMode::SYMLINK)),
            FileMode::REGULAR
        );
        assert_eq!(
            NO_FILEMODE.index_mode_for_worktree(FileMode::REGULAR, Some(FileMode::GITLINK)),
            FileMode::REGULAR
        );
    }

    #[test]
    fn untracked_and_non_regular_worktree_modes_pass_through() {
        let none = WorktreeModePolicy {
            trust_filemode: false,
            has_symlinks: false,
        };
        assert_eq!(
            none.index_mode_for_worktree(FileMode::EXECUTABLE, None),
            FileMode::EXECUTABLE
        );
        assert_eq!(
            none.index_mode_for_worktree(FileMode::TREE, Some(FileMode::SYMLINK)),
            FileMode::TREE
        );
        assert_eq!(
            none.index_mode_for_worktree(FileMode::GITLINK, Some(FileMode::REGULAR)),
            FileMode::GITLINK
        );
    }

    #[test]
    fn from_config_reads_both_keys() {
        let mut cfg = ConfigSet::new();
        assert_eq!(
            WorktreeModePolicy::from_config(&cfg),
            WorktreeModePolicy::default()
        );
        cfg.add_command_override("core.symlinks", "false")
            .expect("set symlinks");
        cfg.add_command_override("core.filemode", "false")
            .expect("set filemode");
        assert_eq!(
            WorktreeModePolicy::from_config(&cfg),
            WorktreeModePolicy {
                trust_filemode: false,
                has_symlinks: false,
            }
        );
    }
}

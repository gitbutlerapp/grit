//! Git ref-related paths under a repository directory (common dir or git dir).

use std::path::{Path, PathBuf};

/// Relative path to `packed-refs` under a common directory.
pub const PACKED_REFS: &str = "packed-refs";

/// Relative path to `packed-refs` under a common directory.
#[must_use]
pub const fn packed_refs_rel() -> &'static str {
    PACKED_REFS
}

/// Absolute path to `packed-refs` under `common_dir`.
#[must_use]
pub fn packed_refs_path(common_dir: &Path) -> PathBuf {
    common_dir.join(packed_refs_rel())
}

/// Relative path prefix for reflogs under `refs/`.
#[must_use]
pub fn logs_refs_rel() -> &'static str {
    "logs/refs"
}

/// Relative path to the bisect reflog directory.
pub const LOGS_REFS_BISECT: &str = "logs/refs/bisect";

/// Relative path to the rewritten reflog directory.
pub const LOGS_REFS_REWRITTEN: &str = "logs/refs/rewritten";

/// Relative path to the worktree reflog directory.
pub const LOGS_REFS_WORKTREE: &str = "logs/refs/worktree";

/// Relative path to a branch reflog under `logs/refs/heads/`.
pub const LOGS_REFS_HEADS_MAIN: &str = "logs/refs/heads/main";

/// Build `logs/refs/<suffix>` for classification tests and tooling.
#[must_use]
pub fn logs_refs_child(suffix: &str) -> String {
    format!("{}/{}", logs_refs_rel(), suffix)
}

/// Path to the top-level `logs` directory under a git or common directory.
#[must_use]
pub fn logs_dir_path(git_or_common: &Path) -> PathBuf {
    git_or_common.join("logs")
}

/// Path to `logs/refs` under a git or common directory.
#[must_use]
pub fn logs_refs_dir_path(git_or_common: &Path) -> PathBuf {
    git_or_common.join(logs_refs_rel())
}

/// Path to `logs/refs/<suffix>` (suffix without leading `refs/`).
#[must_use]
pub fn logs_refs_subdir_path(git_or_common: &Path, suffix: &str) -> PathBuf {
    logs_refs_dir_path(git_or_common).join(suffix)
}

/// Path to `refs` under an existing `logs/` directory (`logs_root/refs`).
#[must_use]
pub fn logs_refs_from_logs_root(logs_root: &Path) -> PathBuf {
    logs_root.join("refs")
}

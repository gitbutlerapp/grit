//! Shared per-commit JSON rows for `log`, `revs`, and `shortlog`.

use schemars::JsonSchema;
use serde::Serialize;

use crate::commands::diff::DiffTotalsJson;
use crate::output::CommitJson;
use crate::render::diff::{DiffStat, FileDiff};

/// One commit in `grit log` JSON output.
#[derive(Serialize, JsonSchema)]
pub struct LogCommit {
    #[serde(flatten)]
    pub commit: CommitJson,
    /// Per-file diff detail when `-p`, `--stat`, `--name-only`, or `--name-status` is set.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub files: Option<Vec<FileDiff>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub totals: Option<DiffTotalsJson>,
    /// The files the commit changed, only with `--stat` (same shape as [`grit show`](../show/)).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stat: Option<DiffStat>,
}

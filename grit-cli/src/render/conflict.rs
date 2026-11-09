//! Conflict rows shared by merge-like command outcomes.

use schemars::JsonSchema;
use serde::Serialize;

/// One conflicted path in `--json` output (pull v1 `conflict_details` and merge v2 items).
#[derive(Clone, Serialize, JsonSchema)]
pub struct MergeConflictJson {
    pub path: String,
    pub kind: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schemars(skip_serializing_if = "Option::is_none")]
    pub summary: Option<String>,
}

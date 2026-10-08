//! CLI rendering for [`grit_lib::diagnostics`] warnings and network traces.

use std::collections::HashSet;
use std::sync::{Arc, Mutex};

use grit_lib::diagnostics::{DiagnosticSink, Trace, Warning};
use serde::Serialize;

/// Delivers library warnings to stderr and retains them for JSON output.
#[derive(Debug)]
pub struct CliDiagnosticSink {
    warnings: Mutex<Vec<WarningRecord>>,
    dedupe: Mutex<HashSet<Warning>>,
}

#[derive(Debug, Clone, Serialize)]
pub struct WarningRecord {
    pub kind: String,
    pub message: String,
}

impl Default for CliDiagnosticSink {
    fn default() -> Self {
        Self::new()
    }
}

impl CliDiagnosticSink {
    /// Create a sink that deduplicates identical warnings before stderr/JSON retention.
    #[must_use]
    pub fn new() -> Self {
        Self {
            warnings: Mutex::new(Vec::new()),
            dedupe: Mutex::new(HashSet::new()),
        }
    }

    /// Snapshot warnings collected so far.
    #[must_use]
    pub fn warnings(&self) -> Vec<WarningRecord> {
        self.warnings
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }
}

impl DiagnosticSink for CliDiagnosticSink {
    fn warn(&self, warning: Warning) {
        let mut dedupe = self.dedupe.lock().unwrap_or_else(|e| e.into_inner());
        if !dedupe.insert(warning.clone()) {
            return;
        }
        let message = format_warning_message(&warning);
        eprintln!("warning: {message}");
        self.warnings
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push(WarningRecord {
                kind: warning_kind(&warning),
                message,
            });
    }

    fn trace(&self, event: Trace) {
        if let Trace::Network { message } = event {
            eprintln!("[grit-net] {message}");
        }
    }
}

/// Human-readable warning text for stderr and JSON (not used inside `grit-lib`).
#[must_use]
pub fn format_warning_message(w: &Warning) -> String {
    match w {
        Warning::AmbiguousRefname { spec } => {
            format!("'{spec}' matches more than one ref or object id")
        }
        Warning::DanglingSymref { name } => {
            format!("symbolic ref `{name}` has no valid target; skipping")
        }
        Warning::CommitGraphChunkTooSmall { layer, chunk } => format!(
            "commit-graph layer '{layer}': {chunk} chunk is smaller than required"
        ),
        Warning::CommitGraphBloomDisabled { layer } => format!(
            "Bloom filters disabled for commit-graph layer '{layer}' (incompatible settings)"
        ),
        Warning::CommitGraphChangedPathIndexTooSmall => {
            "commit-graph changed-path index chunk is smaller than required".into()
        }
        Warning::CommitGraphChangedPathChunkTooSmall { actual, minimum } => format!(
            "commit-graph changed-path chunk too small ({actual} bytes; need at least {minimum})"
        ),
        Warning::CommitGraphChangedPathOffsetOutOfRange {
            offset,
            position,
            graph,
            chunk_size,
        } => format!(
            "changed-path filter offset {offset} out of range at index {position} in {graph} (chunk size {chunk_size})"
        ),
        Warning::CommitGraphChangedPathOffsetsDecreasing {
            start,
            end,
            position_start,
            position_end,
            graph,
        } => format!(
            "changed-path index offsets decrease ({start} > {end}) between positions {position_start} and {position_end} in {graph}"
        ),
        Warning::IgnoredGitDir { path, reason } => {
            format!("skipped config from git dir '{}': {reason}", path.display())
        }
        Warning::CoreBareWithWorktree => {
            "`core.bare` is true while `core.worktree` is set; these options conflict".into()
        }
        Warning::BadBooleanConfig { key, value } => {
            format!("cannot parse `{value}` as a boolean for config key `{key}`")
        }
        Warning::SuspiciousSubmoduleName { name } => {
            format!("submodule name `{name}` looks invalid; skipping")
        }
        Warning::SubmoduleConfigLooksLikeOption { key, value } => format!(
            "submodule config `{key}` value `{value}` looks like a command-line flag; skipping"
        ),
        Warning::SubmoduleMultipleConfigs {
            commit,
            name,
            option,
        } => format!(
            "{commit}: duplicate submodule.{name}.{option} in .gitmodules; keeping the first value"
        ),
        Warning::GitmodulesBadConfig { message } => message.clone(),
        Warning::SplitIndexDisabledWhileConfigEnabled => {
            "split index disabled on the command line but `core.splitIndex` is true in config".into()
        }
        Warning::SplitIndexEnabledWhileConfigDisabled => {
            "split index enabled on the command line but `core.splitIndex` is false in config".into()
        }
        Warning::IndexVersionEnvInvalid { fallback } => format!(
            "`GIT_INDEX_VERSION` is not a valid index format number (using format {fallback})"
        ),
        Warning::IndexVersionConfigInvalid { fallback } => format!(
            "`index.version` is not a valid index format number (using format {fallback})"
        ),
        Warning::MailmapUnreadable { detail } => detail.clone(),
        Warning::MidxChecksumMismatch => {
            "multi-pack-index checksum mismatch; ignoring index for object reads".into()
        }
        Warning::MidxIgnoringExistingChecksumMismatch => {
            "ignoring existing multi-pack-index because its checksum does not validate".into()
        }
        Warning::MidxChunkTableError { detail } => detail.clone(),
        Warning::MidxRevIndexWrongSize => {
            "multi-pack-index reverse-index chunk is the wrong size".into()
        }
        Warning::MidxBitmapMissingReverseIndex => {
            "multi-pack bitmap is missing required reverse index".into()
        }
        Warning::MidxPackIndexUnavailable { pack } => {
            format!("packfile {pack} index unavailable")
        }
        _ => format!("{w:?}"),
    }
}

fn warning_kind(w: &Warning) -> String {
    match w {
        Warning::AmbiguousRefname { .. } => "ambiguous_refname".into(),
        Warning::DanglingSymref { .. } => "dangling_symref".into(),
        Warning::CommitGraphChunkTooSmall { .. } => "commit_graph_chunk_too_small".into(),
        Warning::CommitGraphBloomDisabled { .. } => "commit_graph_bloom_disabled".into(),
        Warning::CommitGraphChangedPathIndexTooSmall => "commit_graph_changed_path_index".into(),
        Warning::CommitGraphChangedPathChunkTooSmall { .. } => {
            "commit_graph_changed_path_chunk".into()
        }
        Warning::CommitGraphChangedPathOffsetOutOfRange { .. } => {
            "commit_graph_changed_path_offset".into()
        }
        Warning::CommitGraphChangedPathOffsetsDecreasing { .. } => {
            "commit_graph_changed_path_offsets".into()
        }
        Warning::IgnoredGitDir { .. } => "ignored_git_dir".into(),
        Warning::CoreBareWithWorktree => "core_bare_with_worktree".into(),
        Warning::BadBooleanConfig { .. } => "bad_boolean_config".into(),
        Warning::SuspiciousSubmoduleName { .. } => "suspicious_submodule_name".into(),
        Warning::SubmoduleConfigLooksLikeOption { .. } => "submodule_config_option".into(),
        Warning::SubmoduleMultipleConfigs { .. } => "submodule_multiple_configs".into(),
        Warning::GitmodulesBadConfig { .. } => "gitmodules_bad_config".into(),
        Warning::SplitIndexDisabledWhileConfigEnabled => "split_index_disabled".into(),
        Warning::SplitIndexEnabledWhileConfigDisabled => "split_index_enabled".into(),
        Warning::IndexVersionEnvInvalid { .. } => "index_version_env_invalid".into(),
        Warning::IndexVersionConfigInvalid { .. } => "index_version_config_invalid".into(),
        Warning::MailmapUnreadable { .. } => "mailmap_unreadable".into(),
        Warning::MidxChecksumMismatch => "midx_checksum_mismatch".into(),
        Warning::MidxIgnoringExistingChecksumMismatch => {
            "midx_ignoring_existing_checksum_mismatch".into()
        }
        Warning::MidxChunkTableError { .. } => "midx_chunk_table_error".into(),
        Warning::MidxRevIndexWrongSize => "midx_rev_index_wrong_size".into(),
        Warning::MidxBitmapMissingReverseIndex => "midx_bitmap_missing_reverse_index".into(),
        Warning::MidxPackIndexUnavailable { .. } => "midx_pack_index_unavailable".into(),
        _ => "other".into(),
    }
}

/// Build [`grit_lib::repo::RepositoryOptions`] with a fresh CLI diagnostic sink.
pub fn repository_options(
    network_trace: bool,
) -> (grit_lib::repo::RepositoryOptions, Arc<CliDiagnosticSink>) {
    let sink = Arc::new(CliDiagnosticSink::new());
    let options = grit_lib::repo::RepositoryOptions {
        diagnostics: sink.clone(),
        network_trace,
    };
    (options, sink)
}

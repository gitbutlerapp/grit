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
        let message = warning.to_string();
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

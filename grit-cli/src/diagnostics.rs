//! CLI rendering for [`grit_lib::diagnostics`] warnings and network traces.

use std::collections::HashSet;
use std::path::Path;
use std::sync::{Arc, Mutex};

use grit_lib::diagnostics::{DiagnosticSink, Trace, Warning};
use schemars::JsonSchema;
use serde::Serialize;

/// Delivers library warnings to stderr and retains them for JSON output.
#[derive(Debug)]
pub struct CliDiagnosticSink {
    warnings: Mutex<Vec<WarningRecord>>,
    dedupe: Mutex<HashSet<Warning>>,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
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
        Warning::AmbiguousRefname { spec } => format!("refname '{spec}' is ambiguous"),
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
        Warning::MailmapUnreadable { spec, kind } => {
            format!("mailmap blob `{spec}` is a {kind}, not a blob; skipping")
        }
        Warning::ReflogNotWritten { refname, detail } => {
            format!("{refname} was updated, but its reflog could not be written: {detail}")
        }
        Warning::ReflogDateBeforeOldestEntry {
            ref_display,
            cutoff_sec,
        } => format!(
            "reflog date for '{ref_display}' is older than every recorded entry (cutoff {cutoff_sec}); using the oldest entry"
        ),
        Warning::NonExecutableHookIgnored {
            hook_name,
            hook_path,
        } => {
            let silence = "see `grit config advice.ignoredHook false` to silence";
            match hook_path {
                Some(path) => format!(
                    "the '{hook_name}' hook was ignored because it is not executable (run `chmod +x {}` to enable it, or {silence})",
                    shell_quote_path(path.as_path())
                ),
                None => format!(
                    "the '{hook_name}' hook was ignored because it is not executable ({silence})"
                ),
            }
        }
        Warning::HookNotRun { hook_name, detail } => {
            format!("the '{hook_name}' hook did not run: {detail}")
        }
        Warning::MidxChecksumMismatch => {
            "multi-pack-index checksum does not match; ignoring index".into()
        }
        Warning::MidxIgnoringExistingChecksumMismatch => {
            "existing multi-pack-index checksum failed verification; replacing index".into()
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
        Warning::MidxUnknownPreferredPack { name } => format!(
            "multi-pack-index preferred pack '{name}' not found in pack directory"
        ),
        Warning::PackBitmapIgnored { path, reason } => format!(
            "pack bitmap ignored at {}: {reason}",
            path.display()
        ),
        Warning::WorktreeAccessFailed { path, detail } => {
            format!("cannot access worktree path '{path}': {detail}")
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
        Warning::ReflogNotWritten { .. } => "reflog_not_written".into(),
        Warning::ReflogDateBeforeOldestEntry { .. } => "reflog_date_before_oldest".into(),
        Warning::NonExecutableHookIgnored { .. } => "non_executable_hook_ignored".into(),
        Warning::HookNotRun { .. } => "hook_not_run".into(),
        Warning::MidxChecksumMismatch => "midx_checksum_mismatch".into(),
        Warning::MidxIgnoringExistingChecksumMismatch => {
            "midx_ignoring_existing_checksum_mismatch".into()
        }
        Warning::MidxChunkTableError { .. } => "midx_chunk_table_error".into(),
        Warning::MidxRevIndexWrongSize => "midx_rev_index_wrong_size".into(),
        Warning::MidxBitmapMissingReverseIndex => "midx_bitmap_missing_reverse_index".into(),
        Warning::MidxPackIndexUnavailable { .. } => "midx_pack_index_unavailable".into(),
        Warning::MidxUnknownPreferredPack { .. } => "midx_unknown_preferred_pack".into(),
        Warning::PackBitmapIgnored { .. } => "pack_bitmap_ignored".into(),
        Warning::WorktreeAccessFailed { .. } => "worktree_access_failed".into(),
        _ => "other".into(),
    }
}

/// Build [`grit_lib::config::environment::RepositoryOptions`] with a fresh CLI diagnostic sink.
pub fn repository_options(
    network_trace: bool,
) -> (
    grit_lib::config::environment::RepositoryOptions,
    Arc<CliDiagnosticSink>,
) {
    let sink = Arc::new(CliDiagnosticSink::new());
    let mut options = grit_lib::config::environment::RepositoryOptions::default();
    options.environment = crate::context::environment();
    options.thread_policy = crate::context::thread_policy_for_repo();
    options.hook_stdio = crate::context::hook_stdio_for_repo();
    options.diagnostics = sink.clone();
    options.network_trace = network_trace;
    options.reference_unix_time = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .ok()
        .map(|d| d.as_secs() as i64);
    options.promisor_fetcher = Some(std::sync::Arc::new(
        grit_lib::objects::promisor::DefaultPromisorFetcher::new(std::sync::Arc::new(
            crate::context::http_client_factory(),
        )),
    ));
    (options, sink)
}

/// Single-quoted path safe to paste into a POSIX `sh -c` one-liner (e.g. `chmod +x …`).
fn shell_quote_path(path: &Path) -> String {
    let s = path.to_string_lossy();
    if s.contains('\'') {
        format!("'{}'", s.replace('\'', "'\"'\"'"))
    } else {
        format!("'{s}'")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use std::process::Command;

    fn chmod_hint_from_message(message: &str) -> &str {
        const PREFIX: &str = "`chmod +x ";
        let start = message.find(PREFIX).expect("chmod hint").saturating_add(1);
        let rest = &message[start..];
        let end = rest.find('`').expect("closing backtick");
        &rest[..end]
    }

    #[test]
    fn non_executable_hook_warning_names_valid_config_command() {
        let warning = Warning::NonExecutableHookIgnored {
            hook_name: "pre-commit".into(),
            hook_path: Some(PathBuf::from(".git/hooks/pre-commit")),
        };
        let message = format_warning_message(&warning);
        assert!(message.contains("grit config advice.ignoredHook false"));
        assert!(!message.contains("config set"));
        assert!(message.contains("chmod +x '.git/hooks/pre-commit'"));
    }

    #[test]
    fn non_executable_hook_warning_chmod_hint_is_shell_safe_with_spaces() {
        let hook = PathBuf::from("/tmp/repo with spaces/.git/hooks/pre-commit");
        let message = format_warning_message(&Warning::NonExecutableHookIgnored {
            hook_name: "pre-commit".into(),
            hook_path: Some(hook.clone()),
        });
        assert!(message.contains("chmod +x '/tmp/repo with spaces/.git/hooks/pre-commit'"));

        let dir = tempfile::tempdir().expect("tempdir");
        let repo = dir.path().join("repo with spaces");
        let hook_path = repo.join(".git/hooks/pre-commit");
        std::fs::create_dir_all(hook_path.parent().expect("parent")).expect("mkdir");
        std::fs::write(&hook_path, "#!/bin/sh\n").expect("hook");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut perms = std::fs::metadata(&hook_path).expect("meta").permissions();
            perms.set_mode(0o644);
            std::fs::set_permissions(&hook_path, perms).expect("perms");
        }

        let warning = Warning::NonExecutableHookIgnored {
            hook_name: "pre-commit".into(),
            hook_path: Some(hook_path.clone()),
        };
        let message = format_warning_message(&warning);
        let cmd = chmod_hint_from_message(&message);
        Command::new("sh")
            .arg("-c")
            .arg(cmd)
            .status()
            .expect("chmod sh -c");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&hook_path)
                .expect("meta")
                .permissions()
                .mode();
            assert_ne!(
                mode & 0o111,
                0,
                "hook should be executable after hinted chmod"
            );
        }
    }

    #[test]
    fn non_executable_hook_warning_chmod_hint_quotes_shell_metacharacters() {
        let hook = PathBuf::from("/tmp/a'b $(rm -rf /)/.git/hooks/pre-commit");
        let message = format_warning_message(&Warning::NonExecutableHookIgnored {
            hook_name: "pre-commit".into(),
            hook_path: Some(hook),
        });
        assert!(message.contains("chmod +x '/tmp/a'\"'\"'b $(rm -rf /)/.git/hooks/pre-commit'"));
        let cmd = chmod_hint_from_message(&message);
        assert_eq!(cmd.matches("rm -rf").count(), 1);
    }
}

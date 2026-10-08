//! Typed warnings and optional trace events for embedders and the `grit` CLI.
//!
//! Library code reports [`Warning`] values through a [`DiagnosticSink`] instead of
//! printing to stderr. The default [`NullDiagnostics`] discards them; tests and
//! the CLI use [`CollectingDiagnostics`] or a custom sink.

use std::collections::HashSet;
use std::fmt;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

/// A non-fatal condition worth surfacing to the user or an embedder.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum Warning {
    /// A ref name could mean both a branch/tag ref and an object id prefix.
    AmbiguousRefname { spec: String },
    /// A symbolic ref pointed at a missing or invalid target (except bare `HEAD`).
    DanglingSymref { name: String },
    /// A commit-graph chunk was smaller than required.
    CommitGraphChunkTooSmall { layer: String, chunk: String },
    /// Bloom filters were disabled for a commit-graph layer due to incompatible settings.
    CommitGraphBloomDisabled { layer: String },
    /// Changed-path Bloom index chunk does not cover all commits in the layer.
    CommitGraphChangedPathIndexTooSmall,
    /// Changed-path Bloom data chunk header is too small.
    CommitGraphChangedPathChunkTooSmall { actual: usize, minimum: usize },
    /// Changed-path Bloom index entry points past the chunk.
    CommitGraphChangedPathOffsetOutOfRange {
        offset: usize,
        position: usize,
        graph: String,
        chunk_size: usize,
    },
    /// Changed-path Bloom index offsets decrease between adjacent entries.
    CommitGraphChangedPathOffsetsDecreasing {
        start: usize,
        end: usize,
        position_start: usize,
        position_end: usize,
        graph: String,
    },
    /// Repository config was skipped because the git directory uses an unsupported format.
    IgnoredGitDir { path: PathBuf, reason: String },
    /// `core.bare=true` while `core.worktree` is also set.
    CoreBareWithWorktree,
    /// A config value looked boolean but could not be parsed.
    BadBooleanConfig { key: String, value: String },
    /// Submodule name failed validation.
    SuspiciousSubmoduleName { name: String },
    /// Submodule config value looked like a command-line flag.
    SubmoduleConfigLooksLikeOption { key: String, value: String },
    /// Duplicate submodule keys in `.gitmodules` for one commit tree.
    SubmoduleMultipleConfigs {
        commit: String,
        name: String,
        option: String,
    },
    /// `.gitmodules` could not be parsed at a given line.
    GitmodulesBadConfig { message: String },
    /// Split index was requested while `core.splitIndex` disables it.
    SplitIndexDisabledWhileConfigEnabled,
    /// Split index was disabled on the command line while config enables it.
    SplitIndexEnabledWhileConfigDisabled,
    /// `GIT_INDEX_VERSION` was set but invalid.
    IndexVersionEnvInvalid { fallback: u32 },
    /// `index.version` in config was invalid.
    IndexVersionConfigInvalid { fallback: u32 },
    /// Mailmap blob/object could not be read as a blob.
    MailmapUnreadable { detail: String },
}

/// Optional trace events (network debugging, etc.), separate from [`Warning`].
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Trace {
    Network { message: String },
}

/// Receives warnings and optional trace lines from `grit-lib`.
pub trait DiagnosticSink: Send + Sync {
    /// Record a non-fatal warning.
    fn warn(&self, warning: Warning);
    /// Record a trace event (no-op by default).
    fn trace(&self, event: Trace) {
        let _ = event;
    }
}

/// Discards all diagnostics (default for [`crate::repo::Repository`]).
#[derive(Debug, Default, Clone, Copy)]
pub struct NullDiagnostics;

impl DiagnosticSink for NullDiagnostics {
    fn warn(&self, _warning: Warning) {}
}

/// Collects warnings (and traces) for tests and the CLI.
#[derive(Debug)]
pub struct CollectingDiagnostics {
    warnings: Mutex<Vec<Warning>>,
    traces: Mutex<Vec<Trace>>,
    dedupe: bool,
    seen: Mutex<HashSet<Warning>>,
}

impl Default for CollectingDiagnostics {
    fn default() -> Self {
        Self::new()
    }
}

impl CollectingDiagnostics {
    /// Create a collector that records every warning.
    #[must_use]
    pub fn new() -> Self {
        Self {
            warnings: Mutex::new(Vec::new()),
            traces: Mutex::new(Vec::new()),
            dedupe: false,
            seen: Mutex::new(HashSet::new()),
        }
    }

    /// When `dedupe` is true, identical [`Warning`] values are stored at most once.
    #[must_use]
    pub fn with_dedupe(dedupe: bool) -> Self {
        Self {
            dedupe,
            ..Self::new()
        }
    }

    /// Snapshot collected warnings (clone of each entry).
    #[must_use]
    pub fn warnings(&self) -> Vec<Warning> {
        self.warnings
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    /// Snapshot collected trace events.
    #[must_use]
    pub fn traces(&self) -> Vec<Trace> {
        self.traces
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    /// Drop all collected warnings and traces.
    pub fn clear(&self) {
        self.warnings
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clear();
        self.traces
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clear();
        self.seen.lock().unwrap_or_else(|e| e.into_inner()).clear();
    }
}

impl DiagnosticSink for CollectingDiagnostics {
    fn warn(&self, warning: Warning) {
        if self.dedupe {
            let mut seen = self.seen.lock().unwrap_or_else(|e| e.into_inner());
            if !seen.insert(warning.clone()) {
                return;
            }
        }
        self.warnings
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push(warning);
    }

    fn trace(&self, event: Trace) {
        self.traces
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push(event);
    }
}

/// Shared handle used when threading diagnostics through load paths.
pub type DiagnosticsHandle = Arc<dyn DiagnosticSink + Send + Sync>;

/// Emit `warning` when `sink` is `Some`, otherwise discard.
pub fn warn_optional(sink: Option<&dyn DiagnosticSink>, warning: Warning) {
    if let Some(s) = sink {
        s.warn(warning);
    }
}

/// Emit `warning` through `handle`.
pub fn warn(handle: &DiagnosticsHandle, warning: Warning) {
    handle.warn(warning);
}

/// Emit a network trace when `handle` is set and `enabled` is true.
pub fn trace_network(handle: &DiagnosticsHandle, enabled: bool, message: String) {
    if enabled {
        handle.trace(Trace::Network { message });
    }
}

impl fmt::Display for Warning {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Warning::AmbiguousRefname { spec } => {
                write!(f, "refname '{spec}' is ambiguous")
            }
            Warning::DanglingSymref { name } => {
                write!(f, "ignoring dangling symref {name}")
            }
            Warning::CommitGraphChunkTooSmall { layer, chunk } => {
                write!(
                    f,
                    "commit-graph layer '{layer}': {chunk} chunk is too small"
                )
            }
            Warning::CommitGraphBloomDisabled { layer } => write!(
                f,
                "disabling Bloom filters for commit-graph layer '{layer}' due to incompatible settings"
            ),
            Warning::CommitGraphChangedPathIndexTooSmall => {
                write!(f, "commit-graph changed-path index chunk is too small")
            }
            Warning::CommitGraphChangedPathChunkTooSmall { actual, minimum } => write!(
                f,
                "ignoring too-small changed-path chunk ({actual} < {minimum}) in commit-graph file"
            ),
            Warning::CommitGraphChangedPathOffsetOutOfRange {
                offset,
                position,
                graph,
                chunk_size,
            } => write!(
                f,
                "ignoring out-of-range offset ({offset}) for changed-path filter at pos {position} of {graph} (chunk size: {chunk_size})"
            ),
            Warning::CommitGraphChangedPathOffsetsDecreasing {
                start,
                end,
                position_start,
                position_end,
                graph,
            } => write!(
                f,
                "ignoring decreasing changed-path index offsets ({start} > {end}) for positions {position_start} and {position_end} of {graph}"
            ),
            Warning::IgnoredGitDir { path, reason } => {
                write!(f, "ignoring git dir '{}': {reason}", path.display())
            }
            Warning::CoreBareWithWorktree => {
                write!(f, "core.bare and core.worktree do not make sense together")
            }
            Warning::BadBooleanConfig { key, value } => write!(
                f,
                "bad boolean config value '{value}' for option '{key}'"
            ),
            Warning::SuspiciousSubmoduleName { name } => {
                write!(f, "ignoring suspicious submodule name: {name}")
            }
            Warning::SubmoduleConfigLooksLikeOption { key, value } => write!(
                f,
                "ignoring '{key}' which may be interpreted as a command-line option: {value}"
            ),
            Warning::SubmoduleMultipleConfigs {
                commit,
                name,
                option,
            } => write!(
                f,
                "{commit}:.gitmodules, multiple configurations found for 'submodule.{name}.{option}'. Skipping second one!"
            ),
            Warning::GitmodulesBadConfig { message } => f.write_str(message),
            Warning::SplitIndexDisabledWhileConfigEnabled => write!(
                f,
                "core.splitIndex is set to true; remove or change it if you really want to disable split index"
            ),
            Warning::SplitIndexEnabledWhileConfigDisabled => write!(
                f,
                "core.splitIndex is set to false; remove or change it if you really want to enable split index"
            ),
            Warning::IndexVersionEnvInvalid { fallback } => write!(
                f,
                "GIT_INDEX_VERSION set, but the value is invalid (using version {fallback})"
            ),
            Warning::IndexVersionConfigInvalid { fallback } => write!(
                f,
                "index.version set, but the value is invalid (using version {fallback})"
            ),
            Warning::MailmapUnreadable { detail } => f.write_str(detail),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn collecting_diagnostics_records_warnings() {
        let sink = CollectingDiagnostics::new();
        sink.warn(Warning::AmbiguousRefname { spec: "abc".into() });
        assert_eq!(sink.warnings().len(), 1);
    }

    #[test]
    fn collecting_diagnostics_dedupes_identical_warnings() {
        let sink = CollectingDiagnostics::with_dedupe(true);
        let w = Warning::CommitGraphChunkTooSmall {
            layer: "layer-a".into(),
            chunk: "base graphs".into(),
        };
        sink.warn(w.clone());
        sink.warn(w);
        assert_eq!(sink.warnings().len(), 1);
    }
}

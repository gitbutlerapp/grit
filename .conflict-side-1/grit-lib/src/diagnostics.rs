//! Git-style diagnostic prefixes and typed warning sinks for embedders and the CLI.
//!
//! Prefix helpers keep `fatal:` / `warning:` literals out of call sites. Structured
//! [`Warning`] values flow through [`DiagnosticSink`] implementations; human-readable
//! text is formatted by embedders (for example `grit-cli`), not by `grit-lib`.
//!
//! Library guide: [diagnostics](https://grit-scm.com/docs/library/embedding)
//!
//! ```rust
//! use grit_lib::repo::{InitOptions, Repository};
//! # fn main() -> grit_lib::error::Result<()> {
//! let dir = tempfile::tempdir()?;
//! let _repo = Repository::init(dir.path(), &InitOptions::default())?;
//! # Ok(())
//! # }
//! ```

use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

/// Prefix a body with Git's `warning: ` diagnostic marker.
#[must_use]
pub fn warning_line(body: &str) -> String {
    ["warning", ": ", body].concat()
}

/// Prefix a body with Git's `hint: ` diagnostic marker.
#[must_use]
pub fn hint_line(body: &str) -> String {
    ["hint", ": ", body].concat()
}

/// Prefix a body with Git's `error: ` diagnostic marker.
#[must_use]
pub fn error_line(body: &str) -> String {
    ["error", ": ", body].concat()
}

/// Prefix a body with Git's `fatal: ` diagnostic marker.
#[must_use]
pub fn fatal_line(body: &str) -> String {
    ["fatal", ": ", body].concat()
}

/// A non-fatal condition worth surfacing to the user or an embedder.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum Warning {
    /// A ref name could mean both a branch/tag ref and an object id prefix.
    AmbiguousRefname {
        /// User-supplied revision or pathspec fragment.
        spec: String,
    },
    /// A symbolic ref pointed at a missing or invalid target (except bare `HEAD`).
    DanglingSymref {
        /// Pack file name or config identifier.
        name: String,
    },
    /// A commit-graph chunk was smaller than required.
    CommitGraphChunkTooSmall {
        /// Commit-graph layer identifier in the warning.
        layer: String,
        /// Commit-graph chunk name that failed validation.
        chunk: String,
    },
    /// Bloom filters were disabled for a commit-graph layer due to incompatible settings.
    CommitGraphBloomDisabled {
        /// Commit-graph layer identifier in the warning.
        layer: String,
    },
    /// Changed-path Bloom index chunk does not cover all commits in the layer.
    CommitGraphChangedPathIndexTooSmall,
    /// Changed-path Bloom data chunk header is too small.
    CommitGraphChangedPathChunkTooSmall {
        /// Observed size or offset in bytes.
        actual: usize,
        /// Minimum required size in bytes.
        minimum: usize,
    },
    /// Changed-path Bloom index entry points past the chunk.
    CommitGraphChangedPathOffsetOutOfRange {
        /// Byte offset into a commit-graph chunk.
        offset: usize,
        /// Index of the commit-graph entry being validated.
        position: usize,
        /// Path or label of the commit-graph file.
        graph: String,
        /// Total size of the commit-graph chunk in bytes.
        chunk_size: usize,
    },
    /// Changed-path Bloom index offsets decrease between adjacent entries.
    CommitGraphChangedPathOffsetsDecreasing {
        /// Starting offset for an adjacent pair of entries.
        start: usize,
        /// Ending offset for an adjacent pair of entries.
        end: usize,
        /// Commit index for the start offset.
        position_start: usize,
        /// Commit index for the end offset.
        position_end: usize,
        /// Path or label of the commit-graph file.
        graph: String,
    },
    /// Repository config was skipped because the git directory uses an unsupported format.
    IgnoredGitDir {
        /// Repository-relative or filesystem path related to the failure.
        path: PathBuf,
        /// Why the value or operation was rejected.
        reason: String,
    },
    /// `core.bare=true` while `core.worktree` is also set.
    CoreBareWithWorktree,
    /// A config value looked boolean but could not be parsed.
    BadBooleanConfig {
        /// Config key name that failed validation.
        key: String,
        /// Raw config value that could not be parsed.
        value: String,
    },
    /// Submodule name failed validation.
    SuspiciousSubmoduleName {
        /// Pack file name or config identifier.
        name: String,
    },
    /// Submodule config value looked like a command-line flag.
    SubmoduleConfigLooksLikeOption {
        /// Config key name that failed validation.
        key: String,
        /// Raw config value that could not be parsed.
        value: String,
    },
    /// Duplicate submodule keys in `.gitmodules` for one commit tree.
    SubmoduleMultipleConfigs {
        /// Commit object id or label tied to the warning.
        commit: String,
        /// Pack file name or config identifier.
        name: String,
        /// Config option or submodule key name.
        option: String,
    },
    /// `.gitmodules` could not be parsed at a given line.
    GitmodulesBadConfig {
        /// Free-form diagnostic message body.
        message: String,
    },
    /// Split index was requested while `core.splitIndex` disables it.
    SplitIndexDisabledWhileConfigEnabled,
    /// Split index was disabled on the command line while config enables it.
    SplitIndexEnabledWhileConfigDisabled,
    /// `GIT_INDEX_VERSION` was set but invalid.
    IndexVersionEnvInvalid {
        /// Index version used after the invalid setting was ignored.
        fallback: u32,
    },
    /// `index.version` in config was invalid.
    IndexVersionConfigInvalid {
        /// Index version used after the invalid setting was ignored.
        fallback: u32,
    },
    /// Mailmap blob/object could not be read as a blob.
    MailmapUnreadable {
        /// Additional human-readable detail for the failure.
        detail: String,
    },
    /// A traditional hook script exists but is not executable.
    NonExecutableHookIgnored {
        /// Hook script basename that was skipped.
        hook_name: String,
    },
    /// Returned when midx checksum mismatch.
    MidxChecksumMismatch,
    /// Returned when midx ignoring existing checksum mismatch.
    MidxIgnoringExistingChecksumMismatch,
    /// Returned when midx chunk table error.
    MidxChunkTableError {
        /// Additional human-readable detail for the failure.
        detail: String,
    },
    /// Returned when midx rev index wrong size.
    MidxRevIndexWrongSize,
    /// Returned when midx bitmap missing reverse index.
    MidxBitmapMissingReverseIndex,
    /// Returned when midx pack index unavailable.
    MidxPackIndexUnavailable {
        /// Pack file basename (`.pack` suffix).
        pack: String,
    },
    /// Returned when midx unknown preferred pack.
    MidxUnknownPreferredPack {
        /// Pack file name or config identifier.
        name: String,
    },
    /// A `.bitmap` sidecar was present but could not be loaded; reachability falls back without it.
    PackBitmapIgnored {
        /// Repository-relative or filesystem path related to the failure.
        path: PathBuf,
        /// Why the value or operation was rejected.
        reason: String,
    },
}

/// Optional trace events (network debugging, etc.), separate from [`Warning`].
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Trace {
    /// Returned when network.
    Network {
        /// Human-readable message body.
        message: String,
    },
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

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
//! let (_repo, _) = Repository::init(dir.path(), &InitOptions::default())?;
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
    /// `mailmap.blob` (or the bare-repository `HEAD:.mailmap` default) names an object that
    /// is not a blob; it was skipped.
    MailmapUnreadable {
        /// The configured blob revision, as written.
        spec: String,
        /// Kind of the object the revision resolved to.
        kind: crate::objects::ObjectKind,
    },
    /// A ref was updated but its reflog line could not be appended; the update stands.
    ReflogNotWritten {
        /// Ref whose reflog was not updated (for example `refs/heads/main` or `HEAD`).
        refname: String,
        /// Why the append failed.
        detail: String,
    },
    /// Reflog `@{date}` cutoff is older than every reflog entry; the oldest entry was used.
    ReflogDateBeforeOldestEntry {
        /// Human-readable ref label from the revision spec.
        ref_display: String,
        /// Parsed cutoff instant (Unix seconds).
        cutoff_sec: i64,
    },
    /// A traditional hook script exists but is not executable.
    NonExecutableHookIgnored {
        /// Hook script basename that was skipped.
        hook_name: String,
        /// Resolved path to the hook script, when known.
        hook_path: Option<PathBuf>,
    },
    /// A hook whose outcome does not affect the operation (such as `post-commit`) could not be
    /// run; the operation already completed.
    HookNotRun {
        /// Hook event name (`post-commit`, …).
        hook_name: String,
        /// Why it did not run, including the underlying OS error.
        detail: String,
    },
    /// Multi-pack-index checksum does not match file contents.
    MidxChecksumMismatch,
    /// Existing on-disk MIDX had a checksum mismatch; grit ignored it and continued.
    MidxIgnoringExistingChecksumMismatch,
    /// Multi-pack-index chunk table is invalid or inconsistent.
    MidxChunkTableError {
        /// Additional human-readable detail for the failure.
        detail: String,
    },
    /// Reverse index chunk size does not match the MIDX layout.
    MidxRevIndexWrongSize,
    /// Pack bitmap requires a reverse index that is missing from the MIDX.
    MidxBitmapMissingReverseIndex,
    /// A pack named in the MIDX has no readable `.idx` sidecar.
    MidxPackIndexUnavailable {
        /// Pack file basename (`.pack` suffix).
        pack: String,
    },
    /// `core.midx` preferred pack name does not match any pack in the MIDX.
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
    /// A work-tree path could not be accessed (for example permission denied); the scan continued.
    WorktreeAccessFailed {
        /// Repository-relative path (directories often end with `/`).
        path: String,
        /// OS error detail.
        detail: String,
    },
}

/// Optional trace events (network debugging, etc.), separate from [`Warning`].
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Trace {
    /// Low-level network trace line (when trace diagnostics are enabled).
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

/// Emit `warning` through `handle`.
pub fn warn(handle: &DiagnosticsHandle, warning: Warning) {
    handle.warn(warning);
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
        let w = Warning::DanglingSymref {
            name: "refs/remotes/origin/HEAD".into(),
        };
        sink.warn(w.clone());
        sink.warn(w);
        assert_eq!(sink.warnings().len(), 1);
    }
}

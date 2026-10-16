//! stderr progress rendering for long-running library operations.

use std::io::{IsTerminal, Write as _};

use grit_lib::progress::{ProgressPhase, ProgressSink};

use crate::stdio;

/// Draws typed [`ProgressPhase`] updates to stderr when enabled.
///
/// When `enabled` is false (non-TTY stderr, `--json`, or `--markdown`), every
/// [`ProgressSink`] method is a no-op so stdout stays clean for machine output.
pub struct TerminalProgress {
    enabled: bool,
    phase: Option<ProgressPhase>,
    current: u64,
    total: Option<u64>,
}

impl TerminalProgress {
    /// Create a progress sink. Pass `enabled: true` only for interactive human
    /// mode on a tty stderr stream.
    #[must_use]
    pub fn new(enabled: bool) -> Self {
        Self {
            enabled,
            phase: None,
            current: 0,
            total: None,
        }
    }

    fn phase_label(phase: ProgressPhase) -> &'static str {
        match phase {
            ProgressPhase::ReceivingObjects => "Receiving objects",
            ProgressPhase::IndexingObjects => "Indexing objects",
            ProgressPhase::ResolvingDeltas => "Resolving deltas",
            ProgressPhase::CopyingObjects => "Copying objects",
            ProgressPhase::CheckingOutFiles => "Checking out files",
            _ => "Working",
        }
    }

    fn render(&self) {
        if !self.enabled {
            return;
        }
        let Some(phase) = self.phase else {
            return;
        };
        let label = Self::phase_label(phase);
        let line = match self.total {
            Some(total) => format!("{label}: {}/{}", self.current, total),
            None => format!("{label}: {}", self.current),
        };
        let mut err = std::io::stderr();
        let _ = stdio::io_result(write!(err, "\r{line}"));
        let _ = stdio::io_result(err.flush());
    }
}

impl ProgressSink for TerminalProgress {
    fn start(&mut self, phase: ProgressPhase, total: Option<u64>) {
        if !self.enabled {
            return;
        }
        self.phase = Some(phase);
        self.current = 0;
        self.total = total;
        self.render();
    }

    fn inc(&mut self, units: u64) {
        if !self.enabled || self.phase.is_none() {
            return;
        }
        self.current = self.current.saturating_add(units);
        self.render();
    }

    fn set(&mut self, current: u64) {
        if !self.enabled || self.phase.is_none() {
            return;
        }
        self.current = current;
        self.render();
    }

    fn finish(&mut self) {
        if !self.enabled {
            return;
        }
        if self.phase.is_some() {
            let _ = stdio::io_result(writeln!(std::io::stderr()));
        }
        self.phase = None;
        self.current = 0;
        self.total = None;
    }
}

/// Whether stderr can show live progress for human-oriented commands.
#[must_use]
pub fn stderr_progress_enabled() -> bool {
    std::io::stderr().is_terminal()
        && grit_lib::porcelain::terminal::stderr_supports_color()
        && std::env::var_os("NO_COLOR").is_none()
}

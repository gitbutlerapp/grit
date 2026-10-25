//! Shared `-q` / `--quiet` argument group (fetch, pull, clone, push, commit).

use clap::Args;

use crate::output::OutputMode;
use crate::progress::TerminalProgress;

/// Silence progress and the human summary; `--json` and `--markdown` output is unchanged.
#[derive(Args, Clone, Debug, Default)]
pub struct QuietArgs {
    /// Suppress progress and human output.
    #[arg(short = 'q', long = "quiet")]
    pub quiet: bool,
}

impl QuietArgs {
    /// Progress sink for this command: never draws when `--quiet` is set.
    #[must_use]
    pub fn progress(&self, mode: OutputMode) -> TerminalProgress {
        if self.quiet {
            TerminalProgress::quiet(mode)
        } else {
            TerminalProgress::new(mode)
        }
    }

    /// Whether the command's outcome should be rendered in `mode`.
    #[must_use]
    pub fn shows_outcome(&self, mode: OutputMode) -> bool {
        !(self.quiet && mode == OutputMode::Human)
    }
}

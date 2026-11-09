//! Progress and plumbing inputs derived from output mode at the dispatch boundary.

/// Whether human-oriented progress and side output is allowed for this command run.
#[derive(Clone, Copy, Debug, Default)]
pub struct CommandInteraction {
    /// Draw live progress and inherit subprocess stdout (human mode only).
    pub human_progress: bool,
}

impl CommandInteraction {
    /// Derive interaction from the invocation's selected stdout format.
    #[must_use]
    pub fn from_mode(mode: crate::output::OutputMode) -> Self {
        Self {
            human_progress: mode == crate::output::OutputMode::Human,
        }
    }

    /// Map to [`crate::output::OutputMode`] for code that still keys off it internally.
    #[must_use]
    pub fn output_mode_for_progress(self) -> crate::output::OutputMode {
        if self.human_progress {
            crate::output::OutputMode::Human
        } else {
            crate::output::OutputMode::Json
        }
    }
}

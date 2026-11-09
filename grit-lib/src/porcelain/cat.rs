//! Presentation planning for `grit cat` blob output.

/// Maximum blob size whose bytes are embedded in `--json` output.
pub const JSON_INLINE_BLOB_MAX: u64 = 1024 * 1024;

/// stdout format for porcelain cat (mirrors CLI modes without depending on the binary).
#[non_exhaustive]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum CatStdoutFormat {
    /// Human-readable terminal output.
    #[default]
    Human,
    /// Machine-readable JSON on stdout.
    Json,
    /// Agent-oriented Markdown on stdout.
    Markdown,
}

/// Presentation inputs for [`CatBlobDisplayPlan::for_blob`].
#[non_exhaustive]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct CatPresentationOptions {
    /// Selected stdout format.
    pub format: CatStdoutFormat,
}

impl CatPresentationOptions {
    /// Set [`Self::format`].
    #[must_use]
    pub fn with_format(mut self, format: CatStdoutFormat) -> Self {
        self.format = format;
        self
    }
}

/// How a blob's bytes are shown for a given size and output format.
#[non_exhaustive]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CatBlobDisposition {
    /// Stream raw bytes to stdout in human mode (large blob).
    StreamHumanStdout,
    /// Omit inline payload from JSON/Markdown (large blob).
    OmitInline,
    /// Embed or print inline (fits under [`JSON_INLINE_BLOB_MAX`]).
    Inline,
}

/// Resolved blob display plan for one object size.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CatBlobDisplayPlan {
    /// How bytes should be delivered for this size and format.
    pub disposition: CatBlobDisposition,
    /// Object size in bytes (from ODB metadata).
    pub size: u64,
}

impl CatBlobDisplayPlan {
    /// Build a plan from object size (from [`Repository::read_info`](crate::repo::Repository) or resolve).
    #[must_use]
    pub fn for_blob(size: u64, presentation: CatPresentationOptions) -> Self {
        let large = size >= JSON_INLINE_BLOB_MAX;
        let disposition = match (large, presentation.format) {
            (true, CatStdoutFormat::Human) => CatBlobDisposition::StreamHumanStdout,
            (true, CatStdoutFormat::Json | CatStdoutFormat::Markdown) => {
                CatBlobDisposition::OmitInline
            }
            (false, _) => CatBlobDisposition::Inline,
        };
        Self { disposition, size }
    }

    /// Whether human mode should stream this blob to stdout without buffering.
    #[must_use]
    pub fn streams_on_human_stdout(self) -> bool {
        matches!(self.disposition, CatBlobDisposition::StreamHumanStdout)
    }

    /// Whether JSON/Markdown should omit inline blob bytes.
    #[must_use]
    pub fn omits_json_inline(self) -> bool {
        matches!(self.disposition, CatBlobDisposition::OmitInline)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn small_blob_inlines_everywhere() {
        let plan = CatBlobDisplayPlan::for_blob(
            100,
            CatPresentationOptions {
                format: CatStdoutFormat::Json,
            },
        );
        assert_eq!(plan.disposition, CatBlobDisposition::Inline);
    }

    #[test]
    fn large_blob_streams_human_only() {
        let human = CatBlobDisplayPlan::for_blob(
            JSON_INLINE_BLOB_MAX,
            CatPresentationOptions {
                format: CatStdoutFormat::Human,
            },
        );
        assert!(human.streams_on_human_stdout());
        let json = CatBlobDisplayPlan::for_blob(
            JSON_INLINE_BLOB_MAX,
            CatPresentationOptions {
                format: CatStdoutFormat::Json,
            },
        );
        assert!(json.omits_json_inline());
    }
}

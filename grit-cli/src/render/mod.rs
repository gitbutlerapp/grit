//! Presentation layer: format typed command outcomes for humans, agents, and scripts.

pub mod diff;
pub mod list;
pub mod stream;
pub mod style;
pub mod table;

use anyhow::Result;
use serde::Serialize;
use std::io::Write;
use time::OffsetDateTime;

/// Wall-clock instant captured once per process invocation for relative dates.
#[derive(Clone, Copy, Debug)]
pub struct RenderContext {
    /// Current time with repository timezone semantics applied at capture.
    pub now: OffsetDateTime,
    /// Selected stdout format for this invocation.
    pub mode: OutputMode,
}

/// How a command's outcome should be rendered.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum OutputMode {
    /// Human-readable text (the default).
    Human,
    /// A single machine-readable JSON object on stdout.
    Json,
    /// Agent-friendly Markdown on stdout.
    Markdown,
}

/// Render a value as the command's human-readable output.
pub trait HumanRender {
    /// Write human-oriented text to `w`.
    fn render_human(&self, w: &mut dyn Write, ctx: &RenderContext) -> Result<()>;
}

/// Render a command outcome as agent-friendly Markdown on stdout.
pub trait MarkdownRender: Serialize {
    fn render_markdown(&self, w: &mut dyn Write, ctx: &RenderContext) -> Result<()>;
}

#[cfg(test)]
mod purity {
    use std::fs;
    use std::path::PathBuf;

    const FORBIDDEN_USE_MARKERS: &[&str] = &[
        "grit_lib::repo",
        "::Repository",
        "crate::context",
        "std::fs",
        "std::env",
        "crate::commands::",
    ];

    const FORBIDDEN_HANDLE_CALLS: &[&str] = &[".objects()", ".references()", ".status()"];

    fn line_has_forbidden_use(line: &str) -> Option<&'static str> {
        let trimmed = line.trim();
        if !trimmed.starts_with("use ") && !trimmed.starts_with("pub use ") {
            return None;
        }
        FORBIDDEN_USE_MARKERS
            .iter()
            .find(|marker| trimmed.contains(**marker))
            .copied()
    }

    fn render_rs_files() -> Vec<PathBuf> {
        let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("src/render");
        let mut files = Vec::new();
        let mut stack = vec![root];
        while let Some(dir) = stack.pop() {
            let entries = fs::read_dir(&dir).unwrap_or_else(|e| panic!("read {dir:?}: {e}"));
            for entry in entries {
                let path = entry.unwrap().path();
                if path.is_dir() {
                    stack.push(path);
                } else if path.extension().is_some_and(|ext| ext == "rs") {
                    files.push(path);
                }
            }
        }
        files.sort();
        files
    }

    #[test]
    fn render_layer_is_repository_free() {
        let mut violations = Vec::new();
        for path in render_rs_files() {
            let rel = path
                .strip_prefix(PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("src"))
                .unwrap()
                .display();
            let source = fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {path:?}: {e}"));
            let mut in_cfg_test = false;
            for line in source.lines() {
                let trimmed = line.trim();
                if trimmed.starts_with("#[cfg(test)]") {
                    in_cfg_test = true;
                    continue;
                }
                if in_cfg_test {
                    continue;
                }
                if let Some(marker) = line_has_forbidden_use(line) {
                    violations.push(format!("{rel}: forbidden import `{marker}` in `{line}`"));
                }
                for call in FORBIDDEN_HANDLE_CALLS {
                    if line.contains(call) {
                        violations.push(format!("{rel}: must not call `{call}` in `{line}`"));
                    }
                }
            }
        }
        assert!(
            violations.is_empty(),
            "render layer purity violations:\n{}",
            violations.join("\n")
        );
    }
}

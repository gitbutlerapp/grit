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
    /// Whether ANSI styling is enabled for this invocation (captured outside the render layer).
    pub color: bool,
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

    const FORBIDDEN_SOURCE_MARKERS: &[&str] = &[
        "std::env",
        "::env::",
        "grit_lib::repo",
        "crate::context",
        "std::fs",
        "crate::commands::",
    ];

    const FORBIDDEN_TYPE_MARKERS: &[&str] = &["Repository"];

    const FORBIDDEN_HANDLE_CALLS: &[&str] = &[".objects()", ".references()", ".status()"];

    const ENV_ALIAS_USE_SUFFIXES: &[&str] = &["::var_os", "::var(", "::args", "::current"];

    #[derive(Clone, Copy, PartialEq, Eq, Debug)]
    enum ScanState {
        Production,
        SkippingCfgTestItem { brace_depth: i32 },
    }

    #[derive(Clone, Copy, Debug, Default)]
    struct BraceLexerState {
        /// When set, the scanner is inside a raw string continued from a previous line.
        raw_string_hashes: Option<usize>,
        /// Block comment open across the previous line boundary (`/*` without a same-line `*/`).
        in_block_comment: bool,
    }

    fn skip_to_block_comment_end(bytes: &[u8], start: usize) -> Option<usize> {
        let mut i = start;
        while i + 1 < bytes.len() {
            if bytes[i] == b'*' && bytes[i + 1] == b'/' {
                return Some(i + 2);
            }
            i += 1;
        }
        None
    }

    /// Resume scanning after a multiline block comment; `None` if the line is still comment-only.
    fn resume_block_comment(bytes: &[u8], lex: &mut BraceLexerState) -> Option<usize> {
        if !lex.in_block_comment {
            return Some(0);
        }
        match skip_to_block_comment_end(bytes, 0) {
            Some(end) => {
                lex.in_block_comment = false;
                Some(end)
            }
            None => None,
        }
    }

    fn enter_block_comment(bytes: &[u8], start: usize, lex: &mut BraceLexerState) -> Option<usize> {
        lex.in_block_comment = true;
        match skip_to_block_comment_end(bytes, start) {
            Some(end) => {
                lex.in_block_comment = false;
                Some(end)
            }
            None => None,
        }
    }

    fn line_is_ignorable(line: &str) -> bool {
        let trimmed = line.trim();
        trimmed.is_empty()
            || trimmed.starts_with("//")
            || trimmed.starts_with("///")
            || trimmed.starts_with('*')
            || trimmed.starts_with("/*")
            || trimmed.starts_with("*/")
    }

    fn close_raw_string_on_line(bytes: &[u8], start: usize, hashes: usize) -> (usize, bool) {
        let mut i = start;
        while i < bytes.len() {
            if bytes[i] == b'"' {
                let close = i + 1;
                if close + hashes <= bytes.len()
                    && bytes[close..close + hashes].iter().all(|&b| b == b'#')
                {
                    return (close + hashes, true);
                }
            }
            i += 1;
        }
        (bytes.len(), false)
    }

    /// Brace depth change outside strings, char literals, and comments.
    fn brace_delta(line: &str, lex: &mut BraceLexerState) -> i32 {
        let bytes = line.as_bytes();
        let mut delta = 0i32;
        let mut i = 0usize;
        if let Some(hashes) = lex.raw_string_hashes {
            let (next, closed) = close_raw_string_on_line(bytes, 0, hashes);
            if !closed {
                return 0;
            }
            lex.raw_string_hashes = None;
            i = next;
        }
        match resume_block_comment(bytes, lex) {
            None => return 0,
            Some(start) => i = start,
        }
        while i < bytes.len() {
            let b = bytes[i];
            if b == b'/' && i + 1 < bytes.len() {
                if bytes[i + 1] == b'/' {
                    break;
                }
                if bytes[i + 1] == b'*' {
                    i += 2;
                    match enter_block_comment(bytes, i, lex) {
                        Some(next) => {
                            i = next;
                            continue;
                        }
                        None => break,
                    }
                }
            }
            if b == b'"' || b == b'\'' {
                i = skip_string_or_char(bytes, i);
                continue;
            }
            if b == b'r' && i + 1 < bytes.len() && (bytes[i + 1] == b'"' || bytes[i + 1] == b'#') {
                let (next, closed, hashes) = scan_raw_string(bytes, i);
                if !closed {
                    lex.raw_string_hashes = Some(hashes);
                    break;
                }
                i = next;
                continue;
            }
            if b == b'b' && i + 1 < bytes.len() && matches!(bytes[i + 1], b'"' | b'\'') {
                i = skip_string_or_char(bytes, i + 1);
                continue;
            }
            match b {
                b'{' => delta += 1,
                b'}' => delta -= 1,
                _ => {}
            }
            i += 1;
        }
        delta
    }

    fn skip_string_or_char(bytes: &[u8], start: usize) -> usize {
        let quote = bytes[start];
        let mut i = start + 1;
        while i < bytes.len() {
            let b = bytes[i];
            if b == b'\\' && i + 1 < bytes.len() {
                i += 2;
                continue;
            }
            if b == quote {
                return i + 1;
            }
            i += 1;
        }
        bytes.len()
    }

    /// Returns `(next_index, closed, hash_delimiter_count)`.
    fn scan_raw_string(bytes: &[u8], start: usize) -> (usize, bool, usize) {
        let mut i = start + 1;
        let mut hashes = 0usize;
        while i < bytes.len() && bytes[i] == b'#' {
            hashes += 1;
            i += 1;
        }
        if i >= bytes.len() || bytes[i] != b'"' {
            return (start + 1, true, hashes);
        }
        i += 1;
        let (next, closed) = close_raw_string_on_line(bytes, i, hashes);
        (next, closed, hashes)
    }

    fn line_has_code_brace(line: &str, lex: &mut BraceLexerState) -> bool {
        let bytes = line.as_bytes();
        let mut i = 0usize;
        if let Some(hashes) = lex.raw_string_hashes {
            let (next, closed) = close_raw_string_on_line(bytes, 0, hashes);
            if !closed {
                return false;
            }
            lex.raw_string_hashes = None;
            i = next;
        }
        match resume_block_comment(bytes, lex) {
            None => return false,
            Some(start) => i = start,
        }
        while i < bytes.len() {
            let b = bytes[i];
            if b == b'/' && i + 1 < bytes.len() {
                if bytes[i + 1] == b'/' {
                    break;
                }
                if bytes[i + 1] == b'*' {
                    i += 2;
                    match enter_block_comment(bytes, i, lex) {
                        Some(next) => {
                            i = next;
                            continue;
                        }
                        None => return false,
                    }
                }
            }
            if b == b'"' || b == b'\'' {
                i = skip_string_or_char(bytes, i);
                continue;
            }
            if b == b'r' && i + 1 < bytes.len() && (bytes[i + 1] == b'"' || bytes[i + 1] == b'#') {
                let (next, closed, hashes) = scan_raw_string(bytes, i);
                if !closed {
                    lex.raw_string_hashes = Some(hashes);
                    return false;
                }
                i = next;
                continue;
            }
            if b == b'b' && i + 1 < bytes.len() && matches!(bytes[i + 1], b'"' | b'\'') {
                i = skip_string_or_char(bytes, i + 1);
                continue;
            }
            if b == b'{' {
                return true;
            }
            i += 1;
        }
        false
    }

    fn is_use_line(line: &str) -> bool {
        let t = line.trim();
        t.starts_with("use ") || t.starts_with("pub use ")
    }

    fn use_line_imports_env(line: &str) -> bool {
        if !is_use_line(line) {
            return false;
        }
        let t = line.trim();
        t.contains("std::env")
            || t.contains("::env::")
            || t.contains("env as ")
            || t.contains("{env")
            || t.contains(", env")
            || t.contains(" env,")
            || t.contains(" env }")
    }

    fn env_aliases_from_use_line(line: &str) -> Vec<String> {
        let mut aliases = Vec::new();
        let mut rest = line;
        while let Some(pos) = rest.find("env") {
            let tail = &rest[pos + 3..];
            let after = tail.trim_start();
            if let Some(stripped) = after.strip_prefix("as") {
                let name = stripped.trim_start().trim_start_matches(':').trim_start();
                let ident: String = name
                    .chars()
                    .take_while(|c| c.is_ascii_alphanumeric() || *c == '_')
                    .collect();
                if !ident.is_empty() {
                    aliases.push(ident);
                }
            }
            rest = &rest[pos + 3..];
        }
        aliases
    }

    fn line_uses_env_alias(line: &str, aliases: &[String]) -> Option<String> {
        for alias in aliases {
            for suffix in ENV_ALIAS_USE_SUFFIXES {
                let needle = format!("{alias}{suffix}");
                if line.contains(&needle) {
                    return Some(format!("{alias}…{suffix}"));
                }
            }
            let prefix = format!("{alias}::");
            if line.contains(&prefix) {
                return Some(prefix);
            }
        }
        None
    }

    fn scan_render_source(rel: &str, source: &str) -> Vec<String> {
        let mut violations = Vec::new();
        let mut state = ScanState::Production;
        let mut pending_cfg_test = false;
        let mut env_aliases: Vec<String> = Vec::new();
        let mut brace_lex = BraceLexerState::default();

        for line in source.lines() {
            match state {
                ScanState::SkippingCfgTestItem {
                    ref mut brace_depth,
                } => {
                    *brace_depth += brace_delta(line, &mut brace_lex);
                    if *brace_depth <= 0 {
                        state = ScanState::Production;
                    }
                    continue;
                }
                ScanState::Production => {}
            }

            let trimmed = line.trim();
            if trimmed == "#[cfg(test)]" {
                pending_cfg_test = true;
                continue;
            }

            if pending_cfg_test {
                pending_cfg_test = false;
                if line_has_code_brace(line, &mut brace_lex) {
                    let open = brace_delta(line, &mut brace_lex);
                    if open > 0 {
                        state = ScanState::SkippingCfgTestItem { brace_depth: open };
                    }
                    continue;
                }
            }

            if line_is_ignorable(line) {
                continue;
            }

            if use_line_imports_env(line) {
                violations.push(format!(
                    "{rel}: forbidden environment import in production line `{line}`"
                ));
            }
            for alias in env_aliases_from_use_line(line) {
                if !env_aliases.contains(&alias) {
                    env_aliases.push(alias);
                }
            }
            if let Some(alias_use) = line_uses_env_alias(line, &env_aliases) {
                violations.push(format!(
                    "{rel}: forbidden aliased environment access `{alias_use}` in `{line}`"
                ));
            }

            for marker in FORBIDDEN_SOURCE_MARKERS {
                if line.contains(marker) {
                    violations.push(format!(
                        "{rel}: forbidden `{marker}` in production line `{line}`"
                    ));
                }
            }
            for marker in FORBIDDEN_TYPE_MARKERS {
                if line.contains(marker) {
                    violations.push(format!(
                        "{rel}: forbidden type `{marker}` in production line `{line}`"
                    ));
                }
            }
            for call in FORBIDDEN_HANDLE_CALLS {
                if line.contains(call) {
                    violations.push(format!("{rel}: must not call `{call}` in `{line}`"));
                }
            }
        }

        violations
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
            violations.extend(scan_render_source(&rel.to_string(), &source));
        }
        assert!(
            violations.is_empty(),
            "render layer purity violations:\n{}",
            violations.join("\n")
        );
    }

    #[test]
    fn purity_regression_rejects_direct_and_qualified_env() {
        let src = "pub fn bad() { let _ = std::env::var_os(\"NO_COLOR\"); }";
        let v = scan_render_source("render/fixture.rs", src);
        assert!(
            v.iter().any(|line| line.contains("std::env")),
            "expected std::env violation, got {v:?}"
        );
    }

    #[test]
    fn purity_regression_rejects_grouped_env_alias() {
        let src = r#"
use std::{env as ambient};
pub fn bad() {
    let _ = ambient::var_os("NO_COLOR");
}
"#;
        let v = scan_render_source("render/fixture.rs", src);
        assert!(
            v.len() >= 2,
            "expected import and alias-use violations, got {v:?}"
        );
        assert!(v.iter().any(|line| line.contains("environment import")));
        assert!(v.iter().any(|line| line.contains("aliased environment")));
    }

    #[test]
    fn purity_regression_scans_production_after_cfg_test_module() {
        let src = r#"
#[cfg(test)]
mod tests {
    fn allowed() {
        let _ = std::env::var_os("X");
    }
}

pub fn still_production() {
    let _ = std::env::var_os("Y");
}
"#;
        let v = scan_render_source("render/fixture.rs", src);
        assert!(
            v.iter()
                .any(|line| line.contains("std::env") && line.contains('Y')),
            "production code after #[cfg(test)] must be scanned: {v:?}"
        );
        assert!(
            !v.iter().any(|line| line.contains('X')),
            "cfg(test) body must stay ignored: {v:?}"
        );
    }

    #[test]
    fn purity_regression_scans_production_after_cfg_test_with_brace_in_string() {
        let src = r#"
#[cfg(test)]
mod tests {
    const OPEN: &str = "{";
}

pub fn still_production() {
    let _ = std::env::var_os("Y");
}
"#;
        let v = scan_render_source("render/fixture.rs", src);
        assert!(
            v.iter()
                .any(|line| line.contains("std::env") && line.contains('Y')),
            "production code after #[cfg(test)] with brace literal in test body must be scanned: {v:?}"
        );
        assert!(
            !v.iter().any(|line| line.contains("OPEN")),
            "cfg(test) body must stay ignored: {v:?}"
        );
    }

    #[test]
    fn purity_regression_scans_production_after_cfg_test_with_brace_in_block_comment() {
        let src = r#"
#[cfg(test)]
mod tests { /*
{
*/
}

pub fn still_production() {
    let _ = std::env::var_os("Y");
}
"#;
        let v = scan_render_source("render/fixture.rs", src);
        assert!(
            v.iter()
                .any(|line| line.contains("std::env") && line.contains('Y')),
            "production code after #[cfg(test)] with brace in multiline block comment must be scanned: {v:?}"
        );
    }

    #[test]
    fn purity_regression_ignores_cfg_test_bodies_only() {
        let src = r#"
pub fn ok() -> bool {
    true
}

#[cfg(test)]
mod inner {
    fn uses_env() {
        let _ = std::env::var_os("NO_COLOR");
    }
}
"#;
        let v = scan_render_source("render/fixture.rs", src);
        assert!(v.is_empty(), "test module env use must be ignored: {v:?}");
    }
}

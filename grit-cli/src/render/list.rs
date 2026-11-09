//! Markdown list and inline-code helpers.

use std::io::Write;

use crate::output::outln;
use crate::stdio;

/// Pick a fenced-code delimiter longer than any run of backticks in `body`.
#[doc(hidden)]
pub fn code_fence(body: &str, lang: &str) -> (String, String) {
    let mut ticks = 3usize;
    loop {
        let marker = "`".repeat(ticks);
        if !body.contains(&marker) {
            return (format!("{marker}{lang}"), marker);
        }
        ticks += 1;
    }
}

/// Fenced `diff` block delimiters that cannot be broken by backticks inside the patch.
#[must_use]
pub fn diff_code_fence(body: &str) -> (String, String) {
    code_fence(body, "diff")
}

/// Write `body` inside a closed `text` fence whose delimiter is longer than any run of
/// backticks in `body`, so untrusted text cannot break out into headings or tables.
pub fn print_fenced_text(w: &mut dyn Write, body: &str) -> anyhow::Result<()> {
    let body = body.strip_suffix('\n').unwrap_or(body);
    let (open, close) = code_fence(body, "text");
    outln!(w, "{open}");
    stdio::io_result(writeln!(w, "{body}")).map_err(|e| anyhow::anyhow!("writing output: {e}"))?;
    outln!(w, "{close}");
    Ok(())
}

/// Render captured hook output as labelled `text` code blocks; empty streams are skipped.
pub fn render_hook_output(w: &mut dyn Write, stdout: &str, stderr: &str) -> anyhow::Result<()> {
    for (label, body) in [("stdout", stdout), ("stderr", stderr)] {
        if body.is_empty() {
            continue;
        }
        outln!(w, "\n**Hook {label}:**\n");
        print_fenced_text(w, body)?;
    }
    Ok(())
}

/// Wrap `content` in a Markdown inline code span.
#[must_use]
pub fn format_inline_code(content: &str) -> String {
    let mut ticks = 1usize;
    loop {
        let marker = "`".repeat(ticks);
        if !content.contains(&marker) {
            let pad = content.starts_with(' ')
                || content.ends_with(' ')
                || content.starts_with('`')
                || content.ends_with('`');
            if pad {
                return format!("{marker} {content} {marker}");
            }
            return format!("{marker}{content}{marker}");
        }
        ticks += 1;
    }
}

#[cfg(test)]
mod tests {
    use super::{format_inline_code, print_fenced_text};
    #[test]
    fn print_fenced_text_closes_fence_when_body_contains_triple_backticks() {
        let body = "````\nnot a fence\n````";
        let mut buf = Vec::new();
        print_fenced_text(&mut buf, body).unwrap();
        let s = String::from_utf8(buf).unwrap();
        assert!(s.contains("`````text"));
        assert!(s.contains(body));
    }

    #[test]
    fn print_fenced_text_fences_forged_changes_heading() {
        let body = "## Forged\n\nnot a heading";
        let mut buf = Vec::new();
        print_fenced_text(&mut buf, body).unwrap();
        let s = String::from_utf8(buf).unwrap();
        assert!(s.starts_with("```text\n"));
    }

    #[test]
    fn format_inline_code_uses_longer_delimiter_for_embedded_backtick() {
        assert_eq!(format_inline_code("tick`name.txt"), "``tick`name.txt``");
        assert_eq!(format_inline_code("plain.txt"), "`plain.txt`");
    }

    #[test]
    fn format_inline_code_pads_when_content_touches_delimiter() {
        assert_eq!(format_inline_code("`edge"), "`` `edge ``");
    }
}

/// First seven characters of a change id.
#[must_use]
pub fn short_change_id(id: &str) -> &str {
    id.get(..7).unwrap_or(id)
}

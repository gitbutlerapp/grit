//! Terminal styling: color, width, and truncation helpers.

use std::io::IsTerminal;

/// ANSI reset sequence.
pub(crate) const RESET: &str = "\x1b[0m";

/// Whether to emit ANSI color: only when stdout is a TTY and `NO_COLOR` is unset
/// (the de-facto standard for opting out — <https://no-color.org>).
#[must_use]
pub fn use_color() -> bool {
    std::io::stdout().is_terminal()
        && grit_lib::porcelain::terminal::ansi_supported()
        && std::env::var_os("NO_COLOR").is_none()
}

/// Wrap `text` in the SGR `code` (e.g. `"32"`) when `color` is enabled.
#[must_use]
pub fn paint(color: bool, code: &str, text: &str) -> String {
    if color {
        format!("\x1b[{code}m{text}{RESET}")
    } else {
        text.to_owned()
    }
}

/// Width of the terminal stdout is attached to, or `None` when it is not a terminal.
#[must_use]
pub fn terminal_width() -> Option<usize> {
    terminal_size::terminal_size().map(|(w, _)| usize::from(w.0))
}

/// Columns `s` takes on screen: its characters, not counting ANSI SGR color sequences.
#[must_use]
pub fn display_width(s: &str) -> usize {
    let mut width = 0;
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        if c == '\x1b' {
            for c in chars.by_ref() {
                if c == 'm' {
                    break;
                }
            }
        } else {
            width += 1;
        }
    }
    width
}

/// Cut `s` to `max` columns with a trailing `…`; `None` leaves it whole, and so does color
/// in `s` (cutting could split an escape sequence).
#[must_use]
pub fn fit_width(s: &str, max: Option<usize>) -> String {
    match max {
        Some(max) if max > 0 && !s.contains('\x1b') => truncate(s, max),
        _ => s.to_owned(),
    }
}

/// Truncate `s` to at most `max` display columns, using `…` for the last cell when shortened.
#[must_use]
pub fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_owned();
    }
    if max == 0 {
        return String::new();
    }
    let mut out: String = s.chars().take(max - 1).collect();
    out.push('…');
    out
}

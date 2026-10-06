//! Git-compatible `core.whitespace` rules and `ws_fix_copy` (`git/ws.c`).
//!
//! Used by `git rebase --whitespace=fix` (apply path) to normalize text blobs.

/// Trailing whitespace at end of line (`blank-at-eol` and `trailing-space`).
pub const WS_BLANK_AT_EOL: u32 = 1 << 6;
/// Space before a tab in the indent.
pub const WS_SPACE_BEFORE_TAB: u32 = 1 << 7;
/// Indent using spaces when tabs are expected.
pub const WS_INDENT_WITH_NON_TAB: u32 = 1 << 8;
/// Treat carriage return at end of line specially.
pub const WS_CR_AT_EOL: u32 = 1 << 9;
/// Blank lines at end of file (used by apply; not passed to `ws_fix_copy`).
pub const WS_BLANK_AT_EOF: u32 = 1 << 10;
/// Tabs in indent should become spaces.
pub const WS_TAB_IN_INDENT: u32 = 1 << 11;
/// Missing newline at end of file.
pub const WS_INCOMPLETE_LINE: u32 = 1 << 12;

/// Default `trailing-space`: both blank-at-eol and blank-at-eof.
pub const WS_TRAILING_SPACE: u32 = WS_BLANK_AT_EOL | WS_BLANK_AT_EOF;

const WS_TAB_WIDTH_MASK: u32 = (1 << 6) - 1;

/// Git `WS_DEFAULT_RULE`: trailing space checks, space-before-tab, tab width 8.
pub const WS_DEFAULT_RULE: u32 = WS_TRAILING_SPACE | WS_SPACE_BEFORE_TAB | 8;

#[inline]
fn ws_tab_width(rule: u32) -> usize {
    (rule & WS_TAB_WIDTH_MASK) as usize
}

#[inline]
fn is_git_space(b: u8) -> bool {
    matches!(b, b' ' | b'\t' | b'\n' | b'\r' | 0x0b | 0x0c)
}

/// Parse `core.whitespace` the same way as Git's `parse_whitespace_rule`.
#[must_use]
pub fn parse_whitespace_rule(string: &str) -> u32 {
    let mut rule = WS_DEFAULT_RULE;
    let mut s = string;

    loop {
        s = s.trim_start_matches([',', ' ', '\t', '\n', '\r']);
        if s.is_empty() {
            break;
        }

        let (token, rest) = match s.find(',') {
            Some(i) => (&s[..i], &s[i + 1..]),
            None => (s, ""),
        };
        s = rest;

        let token = token.trim_matches(|c: char| matches!(c, ',' | ' ' | '\t' | '\n' | '\r'));
        if token.is_empty() {
            continue;
        }

        let (negated, name) = token
            .strip_prefix('-')
            .map(|n| (true, n))
            .unwrap_or((false, token));

        if let Some(arg) = name.strip_prefix("tabwidth=") {
            if let Ok(w) = arg.parse::<u32>() {
                if w > 0 && w < 0o100 {
                    rule &= !WS_TAB_WIDTH_MASK;
                    rule |= w;
                }
            }
            continue;
        }

        let bit = match name {
            "trailing-space" => Some(WS_TRAILING_SPACE),
            "space-before-tab" => Some(WS_SPACE_BEFORE_TAB),
            "indent-with-non-tab" => Some(WS_INDENT_WITH_NON_TAB),
            "cr-at-eol" => Some(WS_CR_AT_EOL),
            "blank-at-eol" => Some(WS_BLANK_AT_EOL),
            "blank-at-eof" => Some(WS_BLANK_AT_EOF),
            "tab-in-indent" => Some(WS_TAB_IN_INDENT),
            "incomplete-line" => Some(WS_INCOMPLETE_LINE),
            _ => None,
        };

        if let Some(bits) = bit {
            if negated {
                rule &= !bits;
            } else {
                rule |= bits;
            }
        }
    }

    rule
}

/// Append one line to `dst` while applying Git `ws_fix_copy` rules.
///
/// `src` is typically one line; the last byte is often `b'\n'` unless the line is incomplete.
pub fn ws_fix_copy(dst: &mut Vec<u8>, src: &[u8], rule: u32) {
    let mut len = src.len();
    let mut add_nl_to_tail = false;
    let mut add_cr_to_tail = false;
    let mut last_tab_in_indent: isize = -1;
    let mut last_space_in_indent: isize = -1;
    let mut need_fix_leading_space = false;

    if rule & WS_INCOMPLETE_LINE != 0 && len > 0 && src[len - 1] != b'\n' {
        add_nl_to_tail = true;
    }

    if rule & WS_BLANK_AT_EOL != 0 {
        if len > 0 && src[len - 1] == b'\n' {
            add_nl_to_tail = true;
            len -= 1;
            if len > 0 && src[len - 1] == b'\r' {
                add_cr_to_tail = rule & WS_CR_AT_EOL != 0;
                len -= 1;
            }
        }
        if len > 0 && is_git_space(src[len - 1]) {
            while len > 0 && is_git_space(src[len - 1]) {
                len -= 1;
            }
        }
    }

    let mut i = 0usize;
    while i < len {
        let ch = src[i];
        if ch == b'\t' {
            last_tab_in_indent = i as isize;
            if (rule & WS_SPACE_BEFORE_TAB) != 0 && last_space_in_indent >= 0 {
                need_fix_leading_space = true;
            }
        } else if ch == b' ' {
            last_space_in_indent = i as isize;
            if (rule & WS_INDENT_WITH_NON_TAB) != 0
                && ws_tab_width(rule) as isize <= i as isize - last_tab_in_indent
            {
                need_fix_leading_space = true;
            }
        } else {
            break;
        }
        i += 1;
    }

    let tw = ws_tab_width(rule).max(1);

    if need_fix_leading_space {
        let mut consecutive_spaces = 0u32;
        let mut last = (last_tab_in_indent + 1).max(0) as usize;
        if (rule & WS_INDENT_WITH_NON_TAB) != 0 {
            if last_tab_in_indent < last_space_in_indent {
                last = last_space_in_indent as usize + 1;
            } else {
                last = last_tab_in_indent as usize + 1;
            }
        }
        for &ch in &src[..last] {
            if ch != b' ' {
                consecutive_spaces = 0;
                dst.push(ch);
            } else {
                consecutive_spaces += 1;
                if consecutive_spaces == tw as u32 {
                    dst.push(b'\t');
                    consecutive_spaces = 0;
                }
            }
        }
        for _ in 0..consecutive_spaces {
            dst.push(b' ');
        }
        dst.extend_from_slice(&src[last..len]);
    } else if (rule & WS_TAB_IN_INDENT) != 0 && last_tab_in_indent >= 0 {
        let start = dst.len();
        let last = last_tab_in_indent as usize + 1;
        for &ch in &src[..last] {
            if ch == b'\t' {
                loop {
                    dst.push(b' ');
                    if (dst.len() - start).is_multiple_of(tw) {
                        break;
                    }
                }
            } else {
                dst.push(ch);
            }
        }
        dst.extend_from_slice(&src[last..len]);
    } else {
        dst.extend_from_slice(&src[..len]);
    }

    if add_cr_to_tail {
        dst.push(b'\r');
    }
    if add_nl_to_tail {
        dst.push(b'\n');
    }
}

/// Remove trailing blank lines at end of file when `WS_BLANK_AT_EOF` is set (Git `apply` behaviour).
/// Only strips from the **end** of `data`; internal blank lines are preserved.
fn trim_trailing_blank_lines_eof(mut data: Vec<u8>, rule: u32) -> Vec<u8> {
    if rule & WS_BLANK_AT_EOF == 0 || data.is_empty() {
        return data;
    }
    let want_final_newline = data.last() == Some(&b'\n');
    loop {
        if data.is_empty() {
            break;
        }
        if !data.ends_with(b"\n") {
            break;
        }
        let without_last_nl = &data[..data.len() - 1];
        if without_last_nl.is_empty() {
            data.clear();
            break;
        }
        let line_start = without_last_nl
            .iter()
            .rposition(|&b| b == b'\n')
            .map(|i| i + 1)
            .unwrap_or(0);
        let line_body = &without_last_nl[line_start..];
        let is_blank = line_body.is_empty() || line_body.iter().all(|b| is_git_space(*b));
        if !is_blank {
            break;
        }
        data.truncate(without_last_nl.len());
    }
    if want_final_newline && !data.ends_with(b"\n") {
        data.push(b'\n');
    }
    data
}

/// Apply `ws_fix_copy` to every line in `data` (split on `b'\n'`, same as Git apply), then trim
/// trailing blank lines when `blank-at-eof` / default `trailing-space` is active.
#[must_use]
pub fn fix_blob_bytes(data: &[u8], rule: u32) -> Vec<u8> {
    let mut out = Vec::new();
    let mut start = 0usize;
    for (i, &b) in data.iter().enumerate() {
        if b == b'\n' {
            ws_fix_copy(&mut out, &data[start..=i], rule);
            start = i + 1;
        }
    }
    if start < data.len() {
        ws_fix_copy(&mut out, &data[start..], rule);
    }
    trim_trailing_blank_lines_eof(out, rule)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_rule_parses_like_git() {
        assert_eq!(parse_whitespace_rule(""), WS_DEFAULT_RULE);
        let r = parse_whitespace_rule("-blank-at-eol");
        assert_eq!(r & WS_BLANK_AT_EOL, 0);
        assert_ne!(r & WS_BLANK_AT_EOF, 0);
    }

    #[test]
    fn blank_at_eol_only_strips_trailing_on_line() {
        // Default Git rules include blank-at-eof; use explicit mask to test EOL-only stripping.
        let rule = parse_whitespace_rule("blank-at-eol") & !WS_BLANK_AT_EOF;
        let s = String::from_utf8(fix_blob_bytes(b"a\t    \n       \n", rule)).unwrap();
        assert_eq!(s, "a\n\n");
    }

    #[test]
    fn negated_blank_at_eol_preserves_trailing_spaces() {
        let rule = parse_whitespace_rule("-blank-at-eol");
        let s = String::from_utf8(fix_blob_bytes(b"x    \n", rule)).unwrap();
        assert_eq!(s, "x    \n");
    }
}

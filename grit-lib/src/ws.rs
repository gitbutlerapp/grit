//! Git-compatible whitespace rules (`core.whitespace`, `whitespace` attribute).
//!
//! Ported from Git's `ws.c` / `ws.h` for `git apply` parity.

/// Trailing whitespace at end of line (spaces/tabs before newline).
pub const WS_BLANK_AT_EOL: u32 = 1 << 6;
/// Space characters before a tab in the indentation area.
pub const WS_SPACE_BEFORE_TAB: u32 = 1 << 7;
/// Indent uses spaces where Git expects tabs (width ≥ tab width).
pub const WS_INDENT_WITH_NON_TAB: u32 = 1 << 8;
/// Allow CR before LF at end of line.
pub const WS_CR_AT_EOL: u32 = 1 << 9;
/// Blank lines at end of file (handled at apply layer, not in `ws_check`).
pub const WS_BLANK_AT_EOF: u32 = 1 << 10;
/// Tab characters in the indentation area.
pub const WS_TAB_IN_INDENT: u32 = 1 << 11;
/// Missing newline at end of file.
pub const WS_INCOMPLETE_LINE: u32 = 1 << 12;

pub const WS_TRAILING_SPACE: u32 = WS_BLANK_AT_EOL | WS_BLANK_AT_EOF;
pub const WS_TAB_WIDTH_MASK: u32 = (1 << 6) - 1;
/// Default `core.whitespace` when unset: trailing + space-before-tab, tab width 8.
pub const WS_DEFAULT_RULE: u32 = WS_TRAILING_SPACE | WS_SPACE_BEFORE_TAB | 8;

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum WhitespaceGitAttr {
    /// No `whitespace` gitattribute applies (use `core.whitespace` / default).
    #[default]
    Unspecified,
    /// `-whitespace` (`ATTR_FALSE`).
    False,
    /// Bare `whitespace` (`ATTR_TRUE`).
    True,
    /// `whitespace=<rules>`.
    String(String),
}

impl WhitespaceGitAttr {
    /// Combine with `core.whitespace` the same way Git's `whitespace_rule()` does.
    pub fn merge_with_config(self, cfg_rule: u32) -> Result<u32, WhitespaceRuleError> {
        match self {
            WhitespaceGitAttr::Unspecified => Ok(cfg_rule),
            WhitespaceGitAttr::False => Ok(tab_width_only(cfg_rule)),
            WhitespaceGitAttr::True => {
                let mut all = tab_width_only(cfg_rule);
                for entry in WS_RULE_ENTRIES {
                    if !entry.loosens_error && !entry.exclude_default {
                        all |= entry.bits;
                    }
                }
                Ok(all)
            }
            // Git `whitespace_rule()`: a string attribute replaces the rule entirely via
            // `parse_whitespace_rule(value)` — it does not merge with `core.whitespace`.
            WhitespaceGitAttr::String(s) => parse_whitespace_rule(&s),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WhitespaceRuleError {
    ConflictingTabAndIndentRules,
}

struct WsRuleEntry {
    name: &'static str,
    bits: u32,
    loosens_error: bool,
    exclude_default: bool,
}

const WS_RULE_ENTRIES: &[WsRuleEntry] = &[
    WsRuleEntry {
        name: "trailing-space",
        bits: WS_TRAILING_SPACE,
        loosens_error: false,
        exclude_default: false,
    },
    WsRuleEntry {
        name: "space-before-tab",
        bits: WS_SPACE_BEFORE_TAB,
        loosens_error: false,
        exclude_default: false,
    },
    WsRuleEntry {
        name: "indent-with-non-tab",
        bits: WS_INDENT_WITH_NON_TAB,
        loosens_error: false,
        exclude_default: false,
    },
    WsRuleEntry {
        name: "cr-at-eol",
        bits: WS_CR_AT_EOL,
        loosens_error: true,
        exclude_default: false,
    },
    WsRuleEntry {
        name: "blank-at-eol",
        bits: WS_BLANK_AT_EOL,
        loosens_error: false,
        exclude_default: false,
    },
    WsRuleEntry {
        name: "blank-at-eof",
        bits: WS_BLANK_AT_EOF,
        loosens_error: false,
        exclude_default: false,
    },
    WsRuleEntry {
        name: "tab-in-indent",
        bits: WS_TAB_IN_INDENT,
        loosens_error: false,
        exclude_default: true,
    },
    WsRuleEntry {
        name: "incomplete-line",
        bits: WS_INCOMPLETE_LINE,
        loosens_error: false,
        exclude_default: false,
    },
];

/// Tab width embedded in the low bits of a whitespace rule (1–63).
#[must_use]
pub fn ws_tab_width(rule: u32) -> usize {
    (rule & WS_TAB_WIDTH_MASK) as usize
}

fn tab_width_only(rule: u32) -> u32 {
    rule & WS_TAB_WIDTH_MASK
}

/// Parse a `core.whitespace` / `whitespace=` attribute value into rule bits.
pub fn parse_whitespace_rule(string: &str) -> Result<u32, WhitespaceRuleError> {
    let mut rule = WS_DEFAULT_RULE;
    let mut s = string;

    while !s.is_empty() {
        s = s.trim_start_matches([',', ' ', '\t', '\n', '\r']);
        if s.is_empty() {
            break;
        }
        let (negated, rest) = if let Some(r) = s.strip_prefix('-') {
            (true, r)
        } else {
            (false, s)
        };
        let end = rest.find(',').unwrap_or(rest.len());
        let token = rest[..end].trim();
        s = &rest[end..];

        if token.is_empty() {
            continue;
        }

        if let Some(arg) = token.strip_prefix("tabwidth=") {
            if let Ok(w) = arg.parse::<u32>() {
                if (0 < w) && (w < 0o100) {
                    rule &= !WS_TAB_WIDTH_MASK;
                    rule |= w & WS_TAB_WIDTH_MASK;
                }
            }
            continue;
        }

        let mut matched = false;
        for entry in WS_RULE_ENTRIES {
            // Git matches with `strncmp(rule_name, token, token.len())`: the config
            // token is a prefix (e.g. `trailing` → `trailing-space`).
            if entry.name.starts_with(token) {
                if negated {
                    rule &= !entry.bits;
                } else {
                    rule |= entry.bits;
                }
                matched = true;
                break;
            }
        }
        if !matched {
            // Unknown token: Git ignores (with optional warning); we ignore.
        }
    }

    if (rule & WS_TAB_IN_INDENT) != 0 && (rule & WS_INDENT_WITH_NON_TAB) != 0 {
        return Err(WhitespaceRuleError::ConflictingTabAndIndentRules);
    }
    Ok(rule)
}

/// Human-readable summary of `ws_check` result flags (Git `whitespace_error_string`).
#[must_use]
pub fn whitespace_error_string(ws: u32) -> String {
    let mut parts: Vec<&'static str> = Vec::new();
    if (ws & WS_TRAILING_SPACE) == WS_TRAILING_SPACE {
        parts.push("trailing whitespace");
    } else {
        if (ws & WS_BLANK_AT_EOL) != 0 {
            parts.push("trailing whitespace");
        }
        if (ws & WS_BLANK_AT_EOF) != 0 {
            parts.push("new blank line at EOF");
        }
    }
    if (ws & WS_SPACE_BEFORE_TAB) != 0 {
        parts.push("space before tab in indent");
    }
    if (ws & WS_INDENT_WITH_NON_TAB) != 0 {
        parts.push("indent with spaces");
    }
    if (ws & WS_TAB_IN_INDENT) != 0 {
        parts.push("tab in indent");
    }
    if (ws & WS_INCOMPLETE_LINE) != 0 {
        parts.push("no newline at the end of file");
    }
    parts.join(", ")
}

fn is_space_git(c: u8) -> bool {
    matches!(c, b' ' | b'\t' | b'\n' | b'\r' | 0x0b | 0x0c)
}

/// Check one line of patch body (without the leading `+`/`-`/` ` prefix) for whitespace issues.
///
/// Matches Git `ws_check_emit_1` (`ws.c`): `WS_INCOMPLETE_LINE` is set only when the line has no
/// trailing newline in the patch (so context lines that end at `\` are not flagged).
#[must_use]
pub fn ws_check(line: &str, ws_rule: u32) -> u32 {
    let mut result = 0u32;
    let bytes = line.as_bytes();
    let mut len = bytes.len();

    let mut trailing_newline = false;
    if len > 0 && bytes[len - 1] == b'\n' {
        trailing_newline = true;
        len -= 1;
    }

    let mut trailing_carriage_return = false;
    if (ws_rule & WS_CR_AT_EOL) != 0 && len > 0 && bytes[len - 1] == b'\r' {
        trailing_carriage_return = true;
        len -= 1;
    }

    let mut trailing_whitespace: isize = -1;
    if (ws_rule & WS_BLANK_AT_EOL) != 0 {
        let mut i = len as isize - 1;
        while i >= 0 {
            if bytes[i as usize].is_ascii_whitespace() {
                trailing_whitespace = i;
                result |= WS_BLANK_AT_EOL;
            } else {
                break;
            }
            i -= 1;
        }
    }
    let tw_end = if trailing_whitespace < 0 {
        len
    } else {
        trailing_whitespace as usize
    };

    if !trailing_newline && (ws_rule & WS_INCOMPLETE_LINE) != 0 {
        result |= WS_INCOMPLETE_LINE;
    }

    let mut i = 0usize;
    let mut written = 0usize;
    while i < tw_end {
        let c = bytes[i];
        if c == b' ' {
            i += 1;
            continue;
        }
        if c != b'\t' {
            break;
        }
        if (ws_rule & WS_SPACE_BEFORE_TAB) != 0 && written < i {
            result |= WS_SPACE_BEFORE_TAB;
        } else if (ws_rule & WS_TAB_IN_INDENT) != 0 {
            result |= WS_TAB_IN_INDENT;
        }
        written = i + 1;
        i += 1;
    }

    if (ws_rule & WS_INDENT_WITH_NON_TAB) != 0 && i - written >= ws_tab_width(ws_rule) {
        result |= WS_INDENT_WITH_NON_TAB;
    }

    let _ = trailing_carriage_return;
    result
}

/// Returns true if the line is empty or only ASCII whitespace.
#[must_use]
pub fn ws_blank_line(line: &str) -> bool {
    line.bytes().all(is_space_git)
}

fn isspace_c(ch: u8) -> bool {
    matches!(ch, b' ' | b'\t' | b'\n' | b'\r' | 0x0b | 0x0c)
}

/// Fix whitespace on one line, matching Git `ws_fix_copy` (patch `+` line body without prefix).
pub fn ws_fix_copy_line(src: &str, ws_rule: u32) -> (String, bool) {
    let mut dst = String::new();
    let mut fixed = false;
    let mut len = src.len();
    if len == 0 {
        return (dst, false);
    }
    let bytes = src.as_bytes();

    let mut add_nl_to_tail = false;
    let mut add_cr_to_tail = false;

    if (ws_rule & WS_INCOMPLETE_LINE) != 0 && bytes[len - 1] != b'\n' {
        fixed = true;
        add_nl_to_tail = true;
    }

    if (ws_rule & WS_BLANK_AT_EOL) != 0 {
        if len > 0 && bytes[len - 1] == b'\n' {
            add_nl_to_tail = true;
            len -= 1;
            if len > 0 && bytes[len - 1] == b'\r' {
                add_cr_to_tail = (ws_rule & WS_CR_AT_EOL) != 0;
                len -= 1;
            }
        }
        if len > 0 && isspace_c(bytes[len - 1]) {
            while len > 0 && isspace_c(bytes[len - 1]) {
                len -= 1;
            }
            fixed = true;
        }
    }

    let mut last_tab_in_indent: i32 = -1;
    let mut last_space_in_indent: i32 = -1;
    let mut need_fix_leading_space = false;
    let mut i = 0usize;
    while i < len {
        let ch = bytes[i];
        if ch == b'\t' {
            last_tab_in_indent = i as i32;
            if (ws_rule & WS_SPACE_BEFORE_TAB) != 0 && last_space_in_indent >= 0 {
                need_fix_leading_space = true;
            }
        } else if ch == b' ' {
            last_space_in_indent = i as i32;
            if (ws_rule & WS_INDENT_WITH_NON_TAB) != 0
                && (i as i32 - last_tab_in_indent) >= ws_tab_width(ws_rule) as i32
            {
                need_fix_leading_space = true;
            }
        } else {
            break;
        }
        i += 1;
    }

    let mut src_rest = &src[..len];
    let mut rest_len = len;

    if need_fix_leading_space {
        let mut last = (last_tab_in_indent + 1) as usize;
        if (ws_rule & WS_INDENT_WITH_NON_TAB) != 0 {
            if last_tab_in_indent < last_space_in_indent {
                last = (last_space_in_indent + 1) as usize;
            } else {
                last = (last_tab_in_indent + 1) as usize;
            }
        }

        let mut consecutive_spaces = 0i32;
        let tw = ws_tab_width(ws_rule);
        for &ch in &bytes[..last] {
            if ch != b' ' {
                consecutive_spaces = 0;
                dst.push(ch as char);
            } else {
                consecutive_spaces += 1;
                if consecutive_spaces == tw as i32 {
                    dst.push('\t');
                    consecutive_spaces = 0;
                }
            }
        }
        while consecutive_spaces > 0 {
            dst.push(' ');
            consecutive_spaces -= 1;
        }
        src_rest = &src[last..len];
        rest_len = src_rest.len();
        fixed = true;
    } else if (ws_rule & WS_TAB_IN_INDENT) != 0 && last_tab_in_indent >= 0 {
        let last = (last_tab_in_indent + 1) as usize;
        let start = dst.len();
        for &ch in &bytes[..last] {
            if ch == b'\t' {
                loop {
                    dst.push(' ');
                    if (dst.len() - start).is_multiple_of(ws_tab_width(ws_rule)) {
                        break;
                    }
                }
            } else {
                dst.push(ch as char);
            }
        }
        src_rest = &src[last..len];
        rest_len = src_rest.len();
        fixed = true;
    }

    dst.push_str(&src_rest[..rest_len]);
    if add_cr_to_tail {
        dst.push('\r');
    }
    if add_nl_to_tail {
        dst.push('\n');
    }
    (dst, fixed)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_rule_parses() {
        let r = parse_whitespace_rule("").unwrap();
        assert!((r & WS_TRAILING_SPACE) != 0);
        assert!((r & WS_SPACE_BEFORE_TAB) != 0);
        assert_eq!(ws_tab_width(r), 8);
    }

    #[test]
    fn ws_check_trailing_space() {
        let rule = WS_BLANK_AT_EOL | 8;
        assert_eq!(ws_check("hello \n", rule), WS_BLANK_AT_EOL);
        assert_eq!(ws_check("hello\n", rule), 0);
    }

    #[test]
    fn ws_fix_tab_in_indent() {
        let rule = WS_TAB_IN_INDENT | 8;
        let (out, fx) = ws_fix_copy_line("\tfoo\n", rule);
        assert!(fx);
        assert_eq!(out, "        foo\n");
    }
}

//! C-style path quoting and decoding compatible with Git's `quote.c` / `core.quotepath`.
//!
//! Git quotes pathnames for human-facing output (`ls-files`, `diff --name-only`,
//! `ls-tree --name-only`) using a byte lookup table and optional full octal
//! escaping for non-ASCII bytes when `quote_path_fully` is set (`core.quotepath`,
//! default true).
//!
//! ```rust
//! use grit_lib::porcelain::quote_path::quote_c_style;
//! # fn main() {
//! assert_eq!(quote_c_style("bell\x07.txt", true), "\"bell\\a.txt\"");
//! # }
//! ```

/// Lookup table from Git `quote.c` (`cq_lookup`). Values:
/// - `1`: emit as `\<ooo>` three-digit octal
/// - `-1`: safe byte (no escape needed inside a quoted string)
/// - `>= 32`: letter escape (`\n`, `\t`, `\"`, …)
/// - `0` for `0x80..=0xFF`: octal only when `quote_fully` is true
const fn cq_byte(b: u8) -> i8 {
    match b {
        0x00..=0x06 => 1,
        0x07 => b'a' as i8,
        0x08 => b'b' as i8,
        0x09 => b't' as i8,
        0x0a => b'n' as i8,
        0x0b => b'v' as i8,
        0x0c => b'f' as i8,
        0x0d => b'r' as i8,
        0x0e..=0x1f => 1,
        0x20 | 0x21 => -1,
        0x22 => b'"' as i8,
        0x23..=0x5b => -1,
        0x5c => b'\\' as i8,
        0x5d..=0x7e => -1,
        0x7f => 1,
        0x80..=0xff => 0,
    }
}

const fn cq_lookup_table() -> [i8; 256] {
    let mut t = [0i8; 256];
    let mut i = 0usize;
    while i < 256 {
        t[i] = cq_byte(i as u8);
        i += 1;
    }
    t
}

// hygiene: immutable quote-path lookup table (facts, not mutable process state)
static CQ_LOOKUP: [i8; 256] = cq_lookup_table();

#[inline]
fn cq_must_quote(byte: u8, quote_fully: bool) -> bool {
    i32::from(CQ_LOOKUP[byte as usize]) + i32::from(quote_fully) > 0
}

fn quote_c_style_inner(path: &str, quote_fully: bool, force_quotes: bool) -> String {
    let bytes = path.as_bytes();
    let mut any = force_quotes;
    if !any {
        for &b in bytes {
            if cq_must_quote(b, quote_fully) {
                any = true;
                break;
            }
        }
    }
    if !any {
        return path.to_owned();
    }

    let mut out = String::with_capacity(path.len() + 2);
    out.push('"');
    let mut p = 0usize;
    while p < bytes.len() {
        let mut len = 0usize;
        while p + len < bytes.len() && !cq_must_quote(bytes[p + len], quote_fully) {
            len += 1;
        }
        out.push_str(path.get(p..p + len).unwrap_or(""));
        p += len;
        if p >= bytes.len() {
            break;
        }
        let ch = bytes[p];
        p += 1;
        out.push('\\');
        let cq = CQ_LOOKUP[ch as usize];
        if cq >= b' ' as i8 {
            out.push(cq as u8 as char);
        } else {
            out.push(char::from(((ch >> 6) & 3) + b'0'));
            out.push(char::from(((ch >> 3) & 7) + b'0'));
            out.push(char::from((ch & 7) + b'0'));
        }
    }
    out.push('"');
    out
}

/// Quote `path` in Git C style when needed, matching `quote_c_style` + `core.quotepath`.
///
/// When `quote_fully` is true (Git default, `core.quotepath=true`), non-ASCII bytes are
/// emitted as `\ooo` escapes. When false, UTF-8 / high bytes are copied literally and only
/// ASCII special characters are escaped.
#[must_use]
pub fn quote_c_style(path: &str, quote_fully: bool) -> String {
    quote_c_style_inner(path, quote_fully, false)
}

/// Quote a repository-relative path stored as raw bytes (non-UTF-8 names on Unix).
#[must_use]
pub fn quote_c_style_bytes(path: &[u8], quote_fully: bool) -> String {
    let mut any = false;
    for &b in path {
        if cq_must_quote(b, quote_fully) {
            any = true;
            break;
        }
    }
    if !any {
        return String::from_utf8_lossy(path).into_owned();
    }

    let mut out = String::with_capacity(path.len() + 2);
    out.push('"');
    let mut p = 0usize;
    while p < path.len() {
        let mut len = 0usize;
        while p + len < path.len() && !cq_must_quote(path[p + len], quote_fully) {
            len += 1;
        }
        for &b in &path[p..p + len] {
            out.push(char::from(b));
        }
        p += len;
        if p >= path.len() {
            break;
        }
        let ch = path[p];
        p += 1;
        out.push('\\');
        let cq = CQ_LOOKUP[ch as usize];
        if cq >= b' ' as i8 {
            out.push(cq as u8 as char);
        } else {
            out.push(char::from(((ch >> 6) & 3) + b'0'));
            out.push(char::from(((ch >> 3) & 7) + b'0'));
            out.push(char::from((ch & 7) + b'0'));
        }
    }
    out.push('"');
    out
}

/// Quote a repository-relative path for human-facing CLI output (status, diff headers).
///
/// `quote_fully` mirrors Git's `core.quotepath` (see [`crate::config::ConfigSet::quote_path_fully`]).
#[must_use]
pub fn quote_path_for_display(path: &str, quote_fully: bool) -> String {
    quote_c_style(path, quote_fully)
}

/// Format one side of a `diff --git` / `---` / `+++` line: either `prefix/path` or
/// `"prefix<escaped-path>"` when Git would C-quote the path (see `t3300-funny-names`).
#[must_use]
pub fn format_diff_path_with_prefix(prefix: &str, path: &str, quote_fully: bool) -> String {
    let quoted = quote_c_style(path, quote_fully);
    if quoted == path {
        format!("{prefix}{path}")
    } else {
        let inner = quoted
            .strip_prefix('"')
            .and_then(|s| s.strip_suffix('"'))
            .unwrap_or(path);
        format!("\"{prefix}{inner}\"")
    }
}

/// Decode a leading C-style quoted path and return the text after its closing quote.
///
/// Accepts named control escapes and exactly three octal digits in `000..=377`.
/// Returns `None` for missing quotes or invalid escapes. Decoded bytes may be
/// non-UTF-8; callers decide whether their path representation accepts them.
pub(crate) fn unquote_c_style_prefix(input: &str) -> Option<(Vec<u8>, &str)> {
    let mut remaining = input.strip_prefix('"')?;
    let mut decoded = Vec::new();
    loop {
        let bytes = remaining.as_bytes();
        let end = bytes.iter().position(|b| matches!(b, b'"' | b'\\'))?;
        decoded.extend_from_slice(&bytes[..end]);
        remaining = &remaining[end + 1..];
        if bytes[end] == b'"' {
            return Some((decoded, remaining));
        }
        let (&escape, tail) = remaining.as_bytes().split_first()?;
        let (byte, width) = match escape {
            b'a' => (7, 1),
            b'b' => (8, 1),
            b'f' => (12, 1),
            b'n' => (b'\n', 1),
            b'r' => (b'\r', 1),
            b't' => (b'\t', 1),
            b'v' => (11, 1),
            b'\\' | b'"' => (escape, 1),
            b'0'..=b'3' => {
                let octal = tail.get(..2)?;
                if !octal.iter().all(|b| matches!(b, b'0'..=b'7')) {
                    return None;
                }
                let byte = (escape - b'0') * 64 + (octal[0] - b'0') * 8 + (octal[1] - b'0');
                (byte, 3)
            }
            _ => return None,
        };
        decoded.push(byte);
        remaining = &remaining[width..];
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unquote_named_escapes_and_suffix() {
        assert_eq!(
            unquote_c_style_prefix(r#""\a\b\f\n\r\t\v\\\"" trailing "text""#),
            Some((b"\x07\x08\x0c\n\r\t\x0b\\\"".to_vec(), " trailing \"text\"")),
        );
        assert_eq!(
            unquote_c_style_prefix("\"\"\"next\""),
            Some((vec![], "\"next\""))
        );
    }

    #[test]
    fn unquote_all_octal_bytes() {
        for byte in 0..=255u8 {
            let quoted = format!("\"\\{byte:03o}\" suffix");
            assert_eq!(
                unquote_c_style_prefix(&quoted),
                Some((vec![byte], " suffix"))
            );
        }
        let bytes: Vec<u8> = (0..=255).collect();
        let quoted = quote_c_style_bytes(&bytes, true);
        assert_eq!(unquote_c_style_prefix(&quoted), Some((bytes, "")));
    }

    #[test]
    fn unquote_utf8_with_both_quoting_modes() {
        for path in [
            "",
            "safe path",
            "café\t東京",
            "quote\"and\\slash",
            "raw\nline",
        ] {
            for fully in [false, true] {
                let quoted = quote_c_style(path, fully);
                let quoted = if quoted.starts_with('"') {
                    quoted
                } else {
                    format!("\"{quoted}\"")
                };
                let input = format!("{quoted} 尾");
                assert_eq!(
                    unquote_c_style_prefix(&input),
                    Some((path.as_bytes().to_vec(), " 尾"))
                );
            }
        }
    }

    #[test]
    fn unquote_rejects_missing_quotes_and_invalid_escapes() {
        for input in [
            "",
            "plain",
            "\"",
            "\"unclosed",
            "\"trailing\\",
            r#""\x""#,
            r#""\/""#,
            r#""\1""#,
            r#""\12""#,
            r#""\128""#,
            r#""\400""#,
            r#""\777""#,
            "\"\\é\"",
            "\"\\0é\"",
        ] {
            assert_eq!(unquote_c_style_prefix(input), None, "{input:?}");
        }
    }

    #[test]
    fn ascii_safe_unchanged() {
        assert_eq!(quote_c_style("Name", true), "Name");
        assert_eq!(quote_c_style("With SP in it", true), "With SP in it");
    }

    #[test]
    fn t3902_expect_quoted() {
        assert_eq!(quote_c_style("Name and a\nLF", true), "\"Name and a\\nLF\"");
        assert_eq!(
            quote_c_style("Name and an\tHT", true),
            "\"Name and an\\tHT\""
        );
        assert_eq!(quote_c_style("Name\"", true), "\"Name\\\"\"");
    }

    #[test]
    fn t3902_expect_raw_mode() {
        let s = "濱野\t純";
        assert_eq!(quote_c_style(s, false), "\"濱野\\t純\"");
        let s2 = "濱野 純";
        assert_eq!(quote_c_style(s2, false), "濱野 純");
    }

    #[test]
    fn c0_control_bytes_quoted() {
        assert_eq!(
            quote_c_style("evil\x1b[31mRED\x1b[0m.txt", true),
            "\"evil\\033[31mRED\\033[0m.txt\""
        );
        assert_eq!(quote_c_style("bell\x07.txt", true), "\"bell\\a.txt\"");
        assert_eq!(
            quote_c_style("ctl\x01char.txt", true),
            "\"ctl\\001char.txt\""
        );
    }

    #[test]
    fn diff_git_prefix_quoting() {
        let p = "tabs\t,\" (dq) and spaces";
        assert_eq!(
            format_diff_path_with_prefix("a/", p, true),
            "\"a/tabs\\t,\\\" (dq) and spaces\""
        );
        assert_eq!(format_diff_path_with_prefix("b/", "plain", true), "b/plain");
    }
}

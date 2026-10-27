//! Cherry-pick / sign-off trailer handling compatible with Git's `sequencer.c` and `trailer.c`.
//!
//! Used when rewriting commit messages for `cherry-pick -x` / `-s` so spacing and trailer
//! detection match upstream tests (e.g. `t3511-cherry-pick-x`).

use crate::config::ConfigSet;

const CHERRY_PICKED_PREFIX: &str = "(cherry picked from commit ";
const SIGN_OFF_HEADER: &str = "Signed-off-by: ";

// hygiene: immutable trailer prefix table (Git keyword list)
static GIT_GENERATED_PREFIXES: &[&str] = &["Signed-off-by: ", "(cherry picked from commit "];

const RESERVED_TRAILER_SUBSECTIONS: &[&str] = &["where", "ifexists", "ifmissing", "separators"];

/// One configured trailer token from `trailer.<name>.*` config entries.
#[derive(Debug, Clone)]
struct TrailerRule {
    /// Subsection name (e.g. `Myfooter`).
    name: String,
    /// Optional `trailer.<name>.key` override for token matching.
    key: Option<String>,
}

fn load_trailer_rules(config: &ConfigSet) -> Vec<TrailerRule> {
    let mut rules: std::collections::BTreeMap<String, TrailerRule> =
        std::collections::BTreeMap::new();
    for e in config.entries() {
        let mut parts = e.key.split('.');
        let (Some("trailer"), Some(subsection), Some(var)) =
            (parts.next(), parts.next(), parts.next())
        else {
            continue;
        };
        if RESERVED_TRAILER_SUBSECTIONS.contains(&subsection) {
            continue;
        }
        let rule = rules
            .entry(subsection.to_string())
            .or_insert_with(|| TrailerRule {
                name: subsection.to_string(),
                key: None,
            });
        if var == "key" {
            if let Some(v) = &e.value {
                rule.key = Some(v.clone());
            }
        }
    }
    rules.into_values().collect()
}

fn next_line_start(buf: &[u8], pos: usize) -> usize {
    if pos >= buf.len() {
        return buf.len();
    }
    match buf[pos..].iter().position(|&b| b == b'\n') {
        Some(p) => pos + p + 1,
        None => buf.len(),
    }
}

/// Start of the line that ends at `len` (`None` when `len` is 0).
fn last_line_start(buf: &[u8], len: usize) -> Option<usize> {
    let before_last_byte = buf.get(..len.checked_sub(1)?)?;
    Some(
        before_last_byte
            .iter()
            .rposition(|&b| b == b'\n')
            .map_or(0, |i| i + 1),
    )
}

fn is_blank_line_bytes(line: &[u8]) -> bool {
    line.iter()
        .copied()
        .take_while(|&b| b != b'\n')
        .all(|b| b.is_ascii_whitespace())
}

/// Git `find_separator` with `separators = ":"`.
fn find_separator_colon(line: &[u8]) -> Option<usize> {
    let mut whitespace_found = false;
    for (i, &c) in line.iter().enumerate() {
        if c == b':' {
            return Some(i);
        }
        if !whitespace_found && (c.is_ascii_alphanumeric() || c == b'-') {
            continue;
        }
        if i != 0 && (c == b' ' || c == b'\t') {
            whitespace_found = true;
            continue;
        }
        break;
    }
    None
}

fn token_len_without_separator(token: &[u8]) -> usize {
    let mut len = token.len();
    while len > 0 && !token[len - 1].is_ascii_alphanumeric() {
        len -= 1;
    }
    len
}

fn line_bytes_starts_with_git_generated(line: &[u8]) -> bool {
    let line_one_line = line.split(|&b| b == b'\n').next().unwrap_or(line);
    for p in GIT_GENERATED_PREFIXES {
        let pb = p.as_bytes();
        if line_one_line.len() >= pb.len() && &line_one_line[..pb.len()] == pb {
            return true;
        }
    }
    false
}

/// Whether `buf` (full message, possibly without a final `\\n`) ends with a line that Git would
/// classify as a trailer (`trailer.c` / `sequencer.c`), including the no-final-newline case from
/// `commit-tree` stdin.
fn last_line_looks_like_trailer(buf: &[u8], rules: &[TrailerRule]) -> bool {
    if buf.is_empty() {
        return false;
    }
    let bol = last_line_start(buf, buf.len()).unwrap_or(0);
    let last = &buf[bol..];
    let mut trim_end = last.len();
    while trim_end > 0 && matches!(last[trim_end - 1], b' ' | b'\t' | b'\r') {
        trim_end -= 1;
    }
    let t = &last[..trim_end];
    if t.is_empty() {
        return false;
    }
    if line_bytes_starts_with_git_generated(t) {
        return true;
    }
    if let Some(sep) = find_separator_colon(t) {
        if sep >= 1 && !t[0].is_ascii_whitespace() {
            return token_matches_rule(&t[..sep], rules);
        }
    }
    false
}

fn token_matches_rule(token: &[u8], rules: &[TrailerRule]) -> bool {
    let tlen = token_len_without_separator(token);
    let token = &token[..tlen];
    let Ok(tok_str) = std::str::from_utf8(token) else {
        return false;
    };
    for r in rules {
        if r.name.eq_ignore_ascii_case(tok_str) {
            return true;
        }
        if r.key
            .as_ref()
            .is_some_and(|k| k.eq_ignore_ascii_case(tok_str))
        {
            return true;
        }
    }
    false
}

/// Byte offset where the trailer block starts, or `len` if none (`find_trailer_block_start`).
fn find_trailer_block_start(buf: &[u8], len: usize, rules: &[TrailerRule]) -> usize {
    // First paragraph (until first blank line) is never part of the trailer block.
    // If there is no blank line, `end_of_title` stays 0 so scanning can treat the
    // whole message as body + trailers (matches single-line subjects in t3511).
    let mut end_of_title = 0usize;
    let mut pos = 0usize;
    while pos < len {
        let line_end = next_line_start(buf, pos);
        let line = &buf[pos..line_end.min(len)];
        if line.first().is_some_and(|b| *b == b'#') {
            pos = line_end;
            continue;
        }
        if is_blank_line_bytes(line) {
            end_of_title = line_end;
            break;
        }
        pos = line_end;
    }

    let mut only_spaces = true;
    let mut recognized_prefix = false;
    let mut trailer_lines = 0i32;
    let mut non_trailer_lines = 0i32;
    let mut possible_continuation_lines = 0i32;

    let is_trailer_block = |recognized_prefix: bool, trailer_lines: i32, non_trailer_lines: i32| {
        (recognized_prefix && trailer_lines * 3 >= non_trailer_lines)
            || (trailer_lines > 0 && non_trailer_lines == 0)
    };

    let mut next = last_line_start(buf, len);
    while let Some(l) = next {
        if l < end_of_title {
            // Reached the title boundary without an intervening blank line: the post-title content
            // is entirely candidate trailers. Mirror Git's blank-line decision using the same ratio
            // so a trailer block flush against the title (e.g. `subject\n\nSigned-off-by: ...`) is
            // recognized rather than treated as plain body.
            if !only_spaces
                && is_trailer_block(
                    recognized_prefix,
                    trailer_lines,
                    non_trailer_lines + possible_continuation_lines,
                )
            {
                return end_of_title;
            }
            break;
        }
        next = last_line_start(buf, l);
        let line = &buf[l..next_line_start(buf, l).min(len)];

        if line.first() == Some(&b'#') {
            non_trailer_lines += possible_continuation_lines;
            possible_continuation_lines = 0;
            continue;
        }

        if is_blank_line_bytes(line) {
            if only_spaces {
                continue;
            }
            non_trailer_lines += possible_continuation_lines;
            return if is_trailer_block(recognized_prefix, trailer_lines, non_trailer_lines) {
                next_line_start(buf, l)
            } else {
                len
            };
        }

        only_spaces = false;

        let indented = line.first().is_some_and(u8::is_ascii_whitespace);
        if line_bytes_starts_with_git_generated(line) {
            trailer_lines += 1;
            possible_continuation_lines = 0;
            recognized_prefix = true;
        } else if let Some(sep_pos) = find_separator_colon(line).filter(|&p| p >= 1 && !indented) {
            trailer_lines += 1;
            possible_continuation_lines = 0;
            recognized_prefix = recognized_prefix || token_matches_rule(&line[..sep_pos], rules);
        } else if indented {
            possible_continuation_lines += 1;
        } else {
            non_trailer_lines += 1 + possible_continuation_lines;
            possible_continuation_lines = 0;
        }
    }

    len
}

/// Iterator over raw trailer lines in Git's sense (lines in the trailer block).
fn trailer_raw_lines<'a>(msg: &'a str, rules: &[TrailerRule]) -> Vec<&'a str> {
    let start = find_trailer_block_start(msg.as_bytes(), msg.len(), rules);
    msg.get(start..).unwrap_or("").lines().collect()
}

/// Where a sign-off line sits relative to the message's trailer block.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SignoffFooter {
    /// The message has no trailer block.
    NoTrailers,
    /// The trailer block does not contain the sign-off.
    Absent,
    /// The sign-off is in the trailer block but is not its last line.
    NotLast,
    /// The last trailer line is the sign-off.
    Last,
}

fn signoff_footer(msg: &str, sob_line: &str, rules: &[TrailerRule]) -> SignoffFooter {
    let lines = trailer_raw_lines(msg, rules);
    let sob_prefix = sob_line.strip_suffix('\n').unwrap_or(sob_line);
    // Prefix match, like Git's `strncmp` against the sign-off line.
    let last_sob = lines.iter().rposition(|raw| {
        raw.strip_suffix('\r')
            .unwrap_or(raw)
            .starts_with(sob_prefix)
    });
    match last_sob {
        _ if lines.is_empty() => SignoffFooter::NoTrailers,
        None => SignoffFooter::Absent,
        Some(idx) if idx + 1 == lines.len() => SignoffFooter::Last,
        Some(_) => SignoffFooter::NotLast,
    }
}

fn strbuf_complete_line(s: &mut String) {
    if !s.is_empty() && !s.ends_with('\n') {
        s.push('\n');
    }
}

/// Append `-x` trailer matching `sequencer.c` (`record_origin`).
pub fn append_cherry_picked_from_line(msg: &mut String, full_hex: &str, config: &ConfigSet) {
    let rules = load_trailer_rules(config);
    strbuf_complete_line(msg);
    let has_footer = !trailer_raw_lines(msg, &rules).is_empty()
        || last_line_looks_like_trailer(msg.trim_end_matches('\n').as_bytes(), &rules);
    if !has_footer {
        msg.push('\n');
    }
    msg.push_str(CHERRY_PICKED_PREFIX);
    msg.push_str(full_hex);
    msg.push_str(")\n");
}

/// Append sign-off matching `append_signoff` in `sequencer.c` (no `APPEND_SIGNOFF_DEDUP`).
pub fn append_signoff_trailer(msg: &mut String, sob_line: &str, config: &ConfigSet) {
    append_signoff_trailer_with_dedup(msg, sob_line, config, false);
}

/// Append sign-off with optional `APPEND_SIGNOFF_DEDUP` (set by `format-patch --signoff`, which
/// suppresses adding a sign-off that already exists anywhere in the trailer block, not just at the
/// very end).
pub fn append_signoff_trailer_with_dedup(
    msg: &mut String,
    sob_line: &str,
    config: &ConfigSet,
    dedup: bool,
) {
    let rules = load_trailer_rules(config);
    strbuf_complete_line(msg);

    // A message that is exactly the sign-off line counts as already signed off.
    let footer = if msg == sob_line {
        SignoffFooter::Last
    } else {
        signoff_footer(msg, sob_line, &rules)
    };

    if footer == SignoffFooter::NoTrailers
        && !last_line_looks_like_trailer(msg.trim_end_matches('\n').as_bytes(), &rules)
    {
        let separator = match msg.len() {
            0 => "\n\n",
            1 => "\n",
            n if msg.as_bytes()[n - 2] != b'\n' => "\n",
            _ => "",
        };
        msg.push_str(separator);
    }

    let already_signed = match footer {
        SignoffFooter::Last => true,
        SignoffFooter::NotLast => dedup,
        SignoffFooter::NoTrailers | SignoffFooter::Absent => false,
    };
    if !already_signed {
        msg.push_str(sob_line.strip_suffix('\n').unwrap_or(sob_line));
        msg.push('\n');
    }
}

/// Build `Signed-off-by: Name <email>\n` using the same identity resolution as cherry-pick.
pub fn format_signoff_line(name: &str, email: &str) -> String {
    format!("{SIGN_OFF_HEADER}{name} <{email}>\n")
}

/// Apply `-x` / `-s` rewriting plus optional `commit.cleanup` when `-x` is set.
pub fn finalize_cherry_pick_message(
    original_message: &str,
    append_source: bool,
    signoff: bool,
    committer_name: &str,
    committer_email: &str,
    config: &ConfigSet,
    picked_commit_hex: &str,
) -> String {
    let mut msg = original_message.to_owned();

    let explicit_cleanup = config.get("commit.cleanup").is_some();
    let cleanup_space = append_source && !explicit_cleanup;
    let cleanup_strip_comments =
        explicit_cleanup && matches!(config.get("commit.cleanup").as_deref(), Some("strip"));

    if cleanup_space {
        let processed =
            crate::stripspace::process(msg.as_bytes(), &crate::stripspace::Mode::Default);
        let cleaned = String::from_utf8_lossy(&processed);
        msg = cleaned.into_owned();
    } else if cleanup_strip_comments {
        let processed = crate::stripspace::process(
            msg.as_bytes(),
            &crate::stripspace::Mode::StripComments("#".to_owned()),
        );
        let cleaned = String::from_utf8_lossy(&processed);
        msg = cleaned.into_owned();
    }

    if append_source {
        append_cherry_picked_from_line(&mut msg, picked_commit_hex, config);
    }

    if signoff {
        let sob = format_signoff_line(committer_name, committer_email);
        append_signoff_trailer(&mut msg, &sob, config);
    }

    msg
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cherry_pick_x_one_line_subject_inserts_blank_before_trailer() {
        let config = ConfigSet::new();
        let mut msg = "base: commit message".to_owned();
        append_cherry_picked_from_line(&mut msg, "abcd".repeat(10).as_str(), &config);
        assert!(msg.contains("\n\n(cherry picked from commit "));
    }

    #[test]
    fn signoff_after_non_conforming_footer_inserts_blank_paragraph() {
        let config = ConfigSet::new();
        let body = "base: commit message\n\nOneWordBodyThatsNotA-S-o-B";
        let mut msg = body.to_owned();
        let sob = format_signoff_line("C O Mitter", "committer@example.com");
        append_signoff_trailer(&mut msg, &sob, &config);
        assert!(msg.contains("OneWordBodyThatsNotA-S-o-B\n\nSigned-off-by:"));
    }

    #[test]
    fn cherry_pick_x_after_sob_without_final_newline_no_extra_blank_before_cherry_line() {
        let config = ConfigSet::new();
        let mut msg = "title\n\nSigned-off-by: A <a@example.com>".to_owned();
        append_cherry_picked_from_line(&mut msg, "d".repeat(40).as_str(), &config);
        assert!(msg.ends_with(")\n"));
        assert!(
            msg.contains("Signed-off-by: A <a@example.com>\n(cherry picked from commit "),
            "unexpected spacing: {msg:?}"
        );
    }

    #[test]
    fn signoff_after_other_sob_without_final_newline_single_separator() {
        let config = ConfigSet::new();
        let mut msg = "title\n\nSigned-off-by: A <a@example.com>".to_owned();
        let sob = format_signoff_line("C O Mitter", "committer@example.com");
        append_signoff_trailer(&mut msg, &sob, &config);
        assert!(
            msg.contains("Signed-off-by: A <a@example.com>\nSigned-off-by: C O Mitter"),
            "unexpected spacing: {msg:?}"
        );
    }
}

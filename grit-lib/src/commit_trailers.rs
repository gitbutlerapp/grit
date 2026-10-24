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
        if !e.key.starts_with("trailer.") {
            continue;
        }
        let parts: Vec<&str> = e.key.split('.').collect();
        if parts.len() < 3 || parts[0] != "trailer" {
            continue;
        }
        let subsection = parts[1];
        if RESERVED_TRAILER_SUBSECTIONS.contains(&subsection) {
            continue;
        }
        let rule = rules
            .entry(subsection.to_string())
            .or_insert_with(|| TrailerRule {
                name: subsection.to_string(),
                key: None,
            });
        if parts.len() >= 3 && parts[2] == "key" {
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

fn last_line_start(buf: &[u8], len: usize) -> Option<usize> {
    if len == 0 {
        return None;
    }
    if len == 1 {
        return Some(0);
    }
    let mut i = len - 2;
    loop {
        if buf[i] == b'\n' {
            return Some(i + 1);
        }
        if i == 0 {
            return Some(0);
        }
        i -= 1;
    }
}

/// Start byte of the last line in `buf[..len]` (Git `last_line`).
fn last_line_start_bounded(buf: &[u8], len: usize) -> usize {
    if len == 0 {
        return 0;
    }
    if len == 1 {
        return 0;
    }
    let mut i = len - 2;
    loop {
        if buf[i] == b'\n' {
            return i + 1;
        }
        if i == 0 {
            return 0;
        }
        i -= 1;
    }
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
    let bol = last_line_start_bounded(buf, buf.len());
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

fn find_end_of_log_message(input: &[u8]) -> usize {
    input.len()
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

    let mut l = match last_line_start(buf, len) {
        Some(s) => s,
        None => return len,
    };

    loop {
        if l < end_of_title {
            // Reached the title boundary without an intervening blank line: the post-title content
            // is entirely candidate trailers. Mirror Git's blank-line decision using the same ratio
            // so a trailer block flush against the title (e.g. `subject\n\nSigned-off-by: ...`) is
            // recognized rather than treated as plain body.
            if !only_spaces {
                non_trailer_lines += possible_continuation_lines;
                if recognized_prefix && trailer_lines * 3 >= non_trailer_lines {
                    return end_of_title;
                }
                if trailer_lines > 0 && non_trailer_lines == 0 {
                    return end_of_title;
                }
            }
            break;
        }
        let line_end = next_line_start(buf, l).min(len);
        let line = &buf[l..line_end];

        if line.first().is_some_and(|b| *b == b'#') {
            non_trailer_lines += possible_continuation_lines;
            possible_continuation_lines = 0;
            l = match last_line_start(buf, l) {
                Some(s) => s,
                None => break,
            };
            continue;
        }

        if is_blank_line_bytes(line) {
            if only_spaces {
                l = match last_line_start(buf, l) {
                    Some(s) => s,
                    None => break,
                };
                continue;
            }
            non_trailer_lines += possible_continuation_lines;
            if recognized_prefix && trailer_lines * 3 >= non_trailer_lines {
                return next_line_start(buf, l);
            }
            if trailer_lines > 0 && non_trailer_lines == 0 {
                return next_line_start(buf, l);
            }
            return len;
        }

        only_spaces = false;

        if line_bytes_starts_with_git_generated(line) {
            trailer_lines += 1;
            possible_continuation_lines = 0;
            recognized_prefix = true;
            l = match last_line_start(buf, l) {
                Some(s) => s,
                None => break,
            };
            continue;
        }

        if let Some(sep_pos) = find_separator_colon(line) {
            if sep_pos >= 1 && !line.first().is_some_and(|b| b.is_ascii_whitespace()) {
                trailer_lines += 1;
                possible_continuation_lines = 0;
                if !recognized_prefix && token_matches_rule(&line[..sep_pos], rules) {
                    recognized_prefix = true;
                }
                l = match last_line_start(buf, l) {
                    Some(s) => s,
                    None => break,
                };
                continue;
            }
        }

        if line.first().is_some_and(|b| b.is_ascii_whitespace()) {
            possible_continuation_lines += 1;
        } else {
            non_trailer_lines += 1;
            non_trailer_lines += possible_continuation_lines;
            possible_continuation_lines = 0;
        }

        l = match last_line_start(buf, l) {
            Some(s) => s,
            None => break,
        };
    }

    len
}

/// Iterator over raw trailer lines in Git's sense (lines in the trailer block).
fn trailer_raw_lines<'a>(msg: &'a str, rules: &[TrailerRule]) -> Vec<&'a str> {
    let bytes = msg.as_bytes();
    let end = find_end_of_log_message(bytes);
    let start = find_trailer_block_start(bytes, end, rules);
    if start >= end {
        return Vec::new();
    }
    let slice = msg.get(start..end).unwrap_or("");
    slice.lines().collect()
}

/// Returns 0 = no conforming footer, 1 = footer without matching sob, 2 = sob in footer not last,
/// 3 = last trailer is sob (matches `has_conforming_footer` in Git when sob is set).
fn has_conforming_footer_with_sob(msg: &str, sob_line: Option<&str>, rules: &[TrailerRule]) -> u8 {
    let lines = trailer_raw_lines(msg, rules);
    if lines.is_empty() {
        return 0;
    }
    let Some(sob) = sob_line else {
        return 1;
    };
    let sob_prefix = sob.strip_suffix('\n').unwrap_or(sob);
    let mut found_sob = 0usize;
    for (idx, raw) in lines.iter().enumerate() {
        let raw_trim = raw.strip_suffix('\r').unwrap_or(raw);
        // Git: `!strncmp(iter.raw, sob->buf, sob->len)` on C strings; equivalent to prefix match.
        if raw_trim
            .as_bytes()
            .get(..sob_prefix.len())
            .is_some_and(|head| head == sob_prefix.as_bytes())
        {
            found_sob = idx + 1;
        }
    }
    let n = lines.len();
    if found_sob == 0 {
        return 1;
    }
    if found_sob == n {
        return 3;
    }
    2
}

/// Returns 1 if there is a conforming footer, else 0 (sob unset).
fn has_conforming_footer_any(msg: &str, rules: &[TrailerRule]) -> bool {
    !trailer_raw_lines(msg, rules).is_empty()
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
    let body_wo_final_blank_lines = msg.trim_end_matches('\n');
    let has_footer = has_conforming_footer_any(msg, &rules)
        || last_line_looks_like_trailer(body_wo_final_blank_lines.as_bytes(), &rules);
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
    let ignore_footer = 0usize;
    strbuf_complete_line(msg);

    let footer_kind = has_conforming_footer_with_sob(msg, Some(sob_line), &rules);

    let sob_prefix = sob_line.strip_suffix('\n').unwrap_or(sob_line);
    let msg_core_len = msg.len().saturating_sub(ignore_footer);
    // Git: if the whole message buffer equals the sob (including final newline), treat as matching.
    let has_footer = if msg_core_len == sob_line.len()
        && msg.get(..sob_line.len()).is_some_and(|p| p == sob_line)
    {
        3u8
    } else {
        footer_kind
    };

    if has_footer == 0 {
        let body_scan = msg.trim_end_matches('\n');
        let trailer_tail = last_line_looks_like_trailer(body_scan.as_bytes(), &rules);
        if !trailer_tail {
            let len = msg.len().saturating_sub(ignore_footer);
            let append_newlines: Option<&'static str> = if len == 0 {
                Some("\n\n")
            } else if len == 1
                || msg
                    .as_bytes()
                    .get(len - 2)
                    .copied()
                    .is_some_and(|b| b != b'\n')
            {
                Some("\n")
            } else {
                None
            };
            if let Some(nl) = append_newlines {
                let insert_at = msg.len() - ignore_footer;
                msg.insert_str(insert_at, nl);
            }
        }
    }

    let no_dup_sob = dedup;
    if has_footer != 3 && (!no_dup_sob || has_footer != 2) {
        let insert_at = msg.len() - ignore_footer;
        msg.insert_str(insert_at, sob_prefix);
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

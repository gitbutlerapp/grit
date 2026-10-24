//! Commit-message text transforms shared by history edits.
//!
//! [`super::squash`] builds an autosquash-style combined message with these helpers, and
//! [`super::absorb`] matches commits by their folded subject line. They work on text alone.

use crate::interpret_trailers::complete_line;

/// How one commit contributes to a combined squash message.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum SquashStep {
    /// The first commit, or any commit without a `fixup!`/`squash!`/`amend!` subject.
    Pick,
    /// A `fixup!` or `amend!` commit; its message is dropped (or replaces the buffer for `amend!`).
    Fixup,
    /// A `squash!` commit; its message is kept, with the `squash!` subject commented out.
    Squash,
}

/// First line of a commit message with continuation lines folded like `git format_subject(..., " ")`.
pub(crate) fn commit_subject_single_line(message: &str) -> String {
    let mut lines = message.lines();
    let Some(first) = lines.next() else {
        return String::new();
    };
    let mut out = first.trim_end().to_string();
    for line in lines {
        let t = line.trim_end();
        if t.is_empty() {
            break;
        }
        if !out.is_empty() {
            out.push(' ');
        }
        out.push_str(t.trim_start());
    }
    out
}

/// Strips one `fixup! ` / `amend! ` / `squash! ` prefix (bang + space), like Git's autosquash
/// subject matching.
pub(crate) fn skip_fixupish_prefix(subject: &str) -> Option<&str> {
    let s = subject.trim_start();
    ["fixup! ", "amend! ", "squash! "]
        .iter()
        .find_map(|prefix| s.strip_prefix(prefix))
}

/// The message body (everything after the first line), or `""` if single-line.
pub(crate) fn message_body_after_subject(message: &str) -> &str {
    match message.find('\n') {
        Some(i) => &message[i + 1..],
        None => "",
    }
}

/// Skip leading blank lines (lines containing only spaces/tabs) of `message`.
fn skip_blank_lines(mut message: &str) -> &str {
    loop {
        let trimmed = message.trim_start_matches([' ', '\t']);
        if let Some(rest) = trimmed.strip_prefix('\n') {
            message = rest;
            continue;
        }
        return message;
    }
}

/// The message that replaces an accumulated buffer for an `amend!` step: an `amend!` subject
/// contributes only its body, every other message its whole (newline-terminated) text.
pub(crate) fn fixup_replacement_message(message: &str) -> String {
    let subject = message.lines().next().unwrap_or("").trim_start();
    if subject.starts_with("amend! ") {
        complete_line(skip_blank_lines(message_body_after_subject(message)))
    } else {
        complete_line(message)
    }
}

/// Byte length of the first line of `body` (excluding the newline).
fn first_line_len(body: &str) -> usize {
    body.find('\n').unwrap_or(body.len())
}

/// How many leading bytes of `body` form a `fixup!`/`squash!`/`amend!` subject
/// that should be commented out in a squash buffer section.
fn squash_comment_subject_prefix(body: &str, step: SquashStep, seen_squash: bool) -> usize {
    let t = body.trim_start();
    if t.starts_with("amend! ") {
        return first_line_len(body);
    }
    if (step == SquashStep::Squash || seen_squash)
        && (t.starts_with("squash! ") || t.starts_with("fixup! "))
    {
        return first_line_len(body);
    }
    0
}

/// Append `text` to `buf` with every line commented (`# `, or bare `#` for empty lines).
fn append_commented(buf: &mut String, text: &str) {
    for line in text.lines() {
        if line.is_empty() {
            buf.push_str("#\n");
        } else {
            buf.push_str("# ");
            buf.push_str(line);
            buf.push('\n');
        }
    }
}

/// Comment out any un-commented commit messages in the squash buffer and rewrite their headers
/// from "This is the Nth commit message:" to "The Nth commit message will be skipped:", leaving
/// already-skipped sections untouched.
///
/// Run when an `amend!` step replaces the accumulated message, so every section accumulated so
/// far is dropped from the final message. It walks the whole buffer, so a chain such as
/// `pick, fixup, amend!` skips both the pick target's message and the plain fixup's section.
pub(crate) fn update_squash_message_for_fixup(buf: &mut String) {
    let mut buf1 = String::from("# This is the 1st commit message:\n");
    let mut buf2 = String::from("# The 1st commit message will be skipped:\n");
    let update_comment_bufs = |b1: &mut String, b2: &mut String, n: usize| {
        *b1 = format!("# This is the commit message #{n}:\n");
        *b2 = format!("# The commit message #{n} will be skipped:\n");
    };

    let orig = std::mem::take(buf);
    let bytes = orig.as_bytes();
    let mut out = String::new();
    // `start` marks the beginning of the not-yet-copied region; `comment_mode` selects whether
    // the copied body is passed through verbatim or commented out.
    let mut start = 0usize;
    let mut comment_mode = false;
    let mut i = 1usize;
    let mut s = 0usize;
    while s < orig.len() {
        if orig[s..].starts_with(buf1.as_str()) {
            // An un-skipped header: copy the preceding section, drop the blank line that precedes
            // this header, emit the "skipped" header, comment the following body.
            let off = usize::from(s > start + 1 && bytes[s - 2] == b'\n');
            copy_section(&mut out, &orig[start..s - off], comment_mode);
            if off == 1 {
                out.push('\n');
            }
            out.push_str(&buf2);
            let mut next = s + buf1.len();
            if next < orig.len() && bytes[next] == b'\n' {
                out.push('\n');
                next += 1;
            }
            start = next;
            s = next;
            comment_mode = true;
            i += 1;
            update_comment_bufs(&mut buf1, &mut buf2, i);
        } else if orig[s..].starts_with(buf2.as_str()) {
            // An already-skipped header: copy the preceding section verbatim (the body that follows
            // is already commented), then continue in verbatim mode.
            let off = usize::from(s > start + 1 && bytes[s - 2] == b'\n');
            copy_section(&mut out, &orig[start..s - off], comment_mode);
            start = s - off;
            s += buf2.len();
            comment_mode = false;
            i += 1;
            update_comment_bufs(&mut buf1, &mut buf2, i);
        } else {
            match orig[s..].find('\n') {
                Some(rel) => s += rel + 1,
                None => break,
            }
        }
    }
    copy_section(&mut out, &orig[start..], comment_mode);
    *buf = out;
}

/// Copy `text` into `out`, commenting it out when `comment_mode` is set.
///
/// Already-commented lines (starting with `#`) are passed through verbatim; every other line —
/// including empty ones, which become a bare `#` — is prefixed.
fn copy_section(out: &mut String, text: &str, comment_mode: bool) {
    if !comment_mode || text.is_empty() {
        out.push_str(text);
        return;
    }
    for line in text.split_inclusive('\n') {
        let content = line.strip_suffix('\n').unwrap_or(line);
        if content.starts_with('#') {
            out.push_str(line);
        } else if content.is_empty() {
            out.push('#');
            out.push_str(line);
        } else {
            out.push_str("# ");
            out.push_str(line);
        }
    }
}

/// Append a commented, "will be skipped" squash section for `body` numbered `n`.
pub(crate) fn append_skipped_squash_message(buf: &mut String, body: &str, n: usize) {
    if !buf.ends_with("\n\n") {
        buf.push('\n');
    }
    buf.push_str("# The commit message #");
    buf.push_str(&n.to_string());
    buf.push_str(" will be skipped:\n\n");
    append_commented(buf, body.trim_end_matches('\n'));
}

/// Append the Nth squash-message section for `body`, commenting out only the
/// `fixup!`/`squash!`/`amend!` subject prefix (per [`squash_comment_subject_prefix`]).
pub(crate) fn append_nth_squash_message(
    buf: &mut String,
    body: &str,
    step: SquashStep,
    seen_squash: bool,
    n: usize,
) {
    if !buf.ends_with("\n\n") {
        buf.push('\n');
    }
    buf.push_str("# This is the commit message #");
    buf.push_str(&n.to_string());
    buf.push_str(":\n\n");
    let pre = squash_comment_subject_prefix(body, step, seen_squash).min(body.len());
    if pre > 0 {
        append_commented(buf, &body[..pre]);
        let rest = &body[pre..];
        let rest = rest.strip_prefix('\n').unwrap_or(rest);
        buf.push_str(rest);
        return;
    }
    buf.push_str(&body[pre..]);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn subject_folds_continuation_lines_until_blank() {
        assert_eq!(
            commit_subject_single_line("subject line\n\nbody\n"),
            "subject line"
        );
        assert_eq!(
            commit_subject_single_line("one\n  two\n\nbody\n"),
            "one two"
        );
        assert_eq!(commit_subject_single_line(""), "");
    }

    #[test]
    fn fixupish_prefix_and_body_helpers() {
        assert_eq!(skip_fixupish_prefix("fixup! foo"), Some("foo"));
        assert_eq!(skip_fixupish_prefix("  amend! foo"), Some("foo"));
        assert_eq!(skip_fixupish_prefix("squash! foo"), Some("foo"));
        assert_eq!(skip_fixupish_prefix("fixup!foo"), None);
        assert_eq!(message_body_after_subject("s\n\nbody\n"), "\nbody\n");
        assert_eq!(message_body_after_subject("only"), "");
        assert_eq!(skip_blank_lines("\n \t\nrest"), "rest");
        assert_eq!(first_line_len("abc\ndef"), 3);
        assert_eq!(first_line_len("abc"), 3);
    }

    #[test]
    fn fixup_replacement_uses_amend_body_only() {
        assert_eq!(fixup_replacement_message("amend! x\n\n\ny\n"), "y\n");
        assert_eq!(fixup_replacement_message("regular"), "regular\n");
    }

    #[test]
    fn squash_sections_comment_only_fixupish_subjects() {
        let body = "squash! subject\n\nparagraph\n";
        assert_eq!(
            squash_comment_subject_prefix(body, SquashStep::Squash, false),
            "squash! subject".len()
        );
        assert_eq!(
            squash_comment_subject_prefix("plain\n", SquashStep::Pick, true),
            0
        );

        let mut buf = String::from("# This is a combination of 2 commits.\n");
        append_nth_squash_message(&mut buf, "first\n", SquashStep::Pick, false, 1);
        append_nth_squash_message(&mut buf, body, SquashStep::Squash, false, 2);
        assert_eq!(
            buf,
            "# This is a combination of 2 commits.\n\
             \n# This is the commit message #1:\n\nfirst\n\
             \n# This is the commit message #2:\n\n# squash! subject\n\nparagraph\n"
        );

        let mut skipped = String::from("seed\n\n");
        append_skipped_squash_message(&mut skipped, "fixup! x\n\nbody\n", 3);
        assert_eq!(
            skipped,
            "seed\n\n# The commit message #3 will be skipped:\n\n# fixup! x\n#\n# body\n"
        );
    }

    #[test]
    fn update_for_fixup_comments_every_unskipped_section() {
        let mut buf = String::from(
            "# This is a combination of 2 commits.\n\
             # This is the 1st commit message:\n\nfirst\n\
             \n# The commit message #2 will be skipped:\n\n# fixup! first\n",
        );
        update_squash_message_for_fixup(&mut buf);
        assert_eq!(
            buf,
            "# This is a combination of 2 commits.\n\
             # The 1st commit message will be skipped:\n\n# first\n\
             \n# The commit message #2 will be skipped:\n\n# fixup! first\n"
        );

        let mut out = String::new();
        copy_section(&mut out, "a\n\n# kept\n", true);
        assert_eq!(out, "# a\n#\n# kept\n");
        let mut verbatim = String::new();
        copy_section(&mut verbatim, "a\n", false);
        assert_eq!(verbatim, "a\n");
    }
}

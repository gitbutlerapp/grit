//! `git rebase` todo-list model and squash/fixup message assembly.
//!
//! The full rebase command in the `grit` binary is a ~12k-line stateful
//! sequencer: it parses args, computes onto/upstream, opens the sequence and
//! commit-message editors, runs hooks, prints progress, and mutates the working
//! tree. Those responsibilities — argv parsing, terminal output,
//! editor/hook subprocess dispatch, revision resolution against a live
//! [`Repository`](crate::repo::Repository), and worktree writes — stay in the
//! CLI.
//!
//! What lives here is the presentation-free, repository-free core of that
//! sequencer: the **todo command vocabulary** and the **pure string transforms**
//! that build a rebase's squash/fixup message buffer from commit messages.
//! These compute their results from text alone, so the CLI calls them while
//! keeping every side effect on its own side of the boundary.
//!
//! # What this module owns
//!
//! - [`RebaseTodoCmd`] — the pick/reword/fixup/squash command keyword, with the
//!   keyword<->variant mapping ([`RebaseTodoCmd::as_str`],
//!   [`RebaseTodoCmd::parse_word`]) used both when generating a todo list and
//!   when parsing the user-edited one.
//! - [`FixupMessageMode`] — whether a `fixup -C`/`fixup -c` step uses or edits
//!   the replaced commit message.
//! - [`commit_subject_single_line`] / [`skip_fixupish_prefix`] /
//!   [`strip_fixupish_chain`] / [`format_autosquash_subject_for_match`] — the
//!   autosquash subject-matching helpers (Git's `skip_fixupish` /
//!   `format_subject`).
//! - [`rebase_todo_command_for_display`] / [`rebase_todo_command_for_display_abbrev`]
//!   — how a command keyword is rendered in a generated todo list (honouring
//!   amend-fixup and `rebase.abbreviateCommands`).
//! - [`fixup_replacement_message`] plus the squash-message buffer builders
//!   [`update_squash_message_for_fixup`], [`append_nth_squash_message`],
//!   [`append_skipped_squash_message`], [`squash_comment_subject_prefix`],
//!   [`append_commented`], [`copy_section`] — implementing Git's
//!   `sequencer.c` squash-message assembly behavior.

use crate::interpret_trailers::complete_line;
use crate::objects::CommitData;

/// A linear interactive-rebase todo command keyword.
///
/// These are the four commands that operate on a single commit message in the
/// sequencer's message-building path; `exec`/`label`/`reset`/`merge`/`break`
/// and friends are parsed separately in the CLI.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RebaseTodoCmd {
    Pick,
    Reword,
    Fixup,
    Squash,
}

impl RebaseTodoCmd {
    /// The canonical (long) keyword for this command.
    pub fn as_str(self) -> &'static str {
        match self {
            RebaseTodoCmd::Pick => "pick",
            RebaseTodoCmd::Reword => "reword",
            RebaseTodoCmd::Fixup => "fixup",
            RebaseTodoCmd::Squash => "squash",
        }
    }

    /// Parse a todo command word (long or single-letter form), returning `None`
    /// for any word that is not one of the four pick-like commands.
    pub fn parse_word(word: &str) -> Option<Self> {
        match word {
            "pick" | "p" => Some(RebaseTodoCmd::Pick),
            "reword" | "r" => Some(RebaseTodoCmd::Reword),
            "fixup" | "f" => Some(RebaseTodoCmd::Fixup),
            "squash" | "s" => Some(RebaseTodoCmd::Squash),
            _ => None,
        }
    }
}

/// Whether a `fixup -C`/`fixup -c` step uses the replaced commit message
/// verbatim or opens an editor to amend it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FixupMessageMode {
    UseCommit,
    EditCommit,
}

/// First line of a commit message with continuation lines folded like `git format_subject(..., " ")`.
pub fn commit_subject_single_line(message: &str) -> String {
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

/// Strips one `fixup! ` / `amend! ` / `squash! ` prefix (bang + space), matching Git's
/// `skip_fixupish` in `sequencer.c` (`todo_list_rearrange_squash`).
pub fn skip_fixupish_prefix(subject: &str) -> Option<&str> {
    let s = subject.trim_start();
    if let Some(rest) = s.strip_prefix("fixup! ") {
        return Some(rest);
    }
    if let Some(rest) = s.strip_prefix("amend! ") {
        return Some(rest);
    }
    if let Some(rest) = s.strip_prefix("squash! ") {
        return Some(rest);
    }
    None
}

/// Strips every leading `fixup!`/`amend!`/`squash!` prefix from a subject.
pub fn strip_fixupish_chain(mut p: &str) -> &str {
    while let Some(rest) = skip_fixupish_prefix(p) {
        p = rest;
        p = p.trim_start();
    }
    p
}

/// The autosquash match key for a commit message: its folded subject line.
pub fn format_autosquash_subject_for_match(message: &str) -> String {
    commit_subject_single_line(message)
}

/// The command keyword shown for `cmd` in a generated todo list, accounting for
/// amend-fixup (an `amend!` subject under `fixup` renders as `fixup -C`).
pub fn rebase_todo_command_for_display(cmd: RebaseTodoCmd, commit: &CommitData) -> &'static str {
    if cmd == RebaseTodoCmd::Fixup
        && commit
            .message
            .lines()
            .next()
            .unwrap_or("")
            .trim_start()
            .starts_with("amend! ")
    {
        "fixup -C"
    } else {
        cmd.as_str()
    }
}

/// Abbreviation-aware variant of [`rebase_todo_command_for_display`]. With `abbrev` set, mirrors
/// Git's `command_to_char` (`pick`→`p`, `fixup`→`f`, `squash`→`s`, `reword`→`r`), keeping the
/// `-C`/`-c` suffix for amend-fixup (`f -C`).
pub fn rebase_todo_command_for_display_abbrev(
    cmd: RebaseTodoCmd,
    commit: &CommitData,
    abbrev: bool,
) -> String {
    let is_amend_fixup = cmd == RebaseTodoCmd::Fixup
        && commit
            .message
            .lines()
            .next()
            .unwrap_or("")
            .trim_start()
            .starts_with("amend! ");
    if !abbrev {
        return if is_amend_fixup {
            "fixup -C".to_owned()
        } else {
            cmd.as_str().to_owned()
        };
    }
    let ch = match cmd {
        RebaseTodoCmd::Pick => "p",
        RebaseTodoCmd::Reword => "r",
        RebaseTodoCmd::Fixup => "f",
        RebaseTodoCmd::Squash => "s",
    };
    if is_amend_fixup {
        format!("{ch} -C")
    } else {
        ch.to_owned()
    }
}

/// The message body (everything after the first line), or `""` if single-line.
pub fn message_body_after_subject(message: &str) -> &str {
    match message.find('\n') {
        Some(i) => &message[i + 1..],
        None => "",
    }
}

/// Skip leading blank lines (lines containing only spaces/tabs) of `message`.
pub fn skip_blank_lines(mut message: &str) -> &str {
    loop {
        let trimmed = message.trim_start_matches([' ', '\t']);
        if let Some(rest) = trimmed.strip_prefix('\n') {
            message = rest;
            continue;
        }
        return message;
    }
}

/// The message that replaces an accumulated buffer for a `fixup -C`/`amend!`
/// step: an `amend!` subject contributes only its body, every other message its
/// whole (newline-terminated) text.
pub fn fixup_replacement_message(message: &str) -> String {
    let subject = message.lines().next().unwrap_or("").trim_start();
    if subject.starts_with("amend! ") {
        complete_line(skip_blank_lines(message_body_after_subject(message)))
    } else {
        complete_line(message)
    }
}

/// Byte length of the first line of `body` (excluding the newline).
pub fn first_line_len(body: &str) -> usize {
    match body.find('\n') {
        Some(i) => i,
        None => body.len(),
    }
}

/// How many leading bytes of `body` form a `fixup!`/`squash!`/`amend!` subject
/// that should be commented out in a squash buffer section.
pub fn squash_comment_subject_prefix(body: &str, cmd: RebaseTodoCmd, seen_squash: bool) -> usize {
    let t = body.trim_start();
    if t.starts_with("amend! ") {
        return first_line_len(body);
    }
    if (cmd == RebaseTodoCmd::Squash || seen_squash)
        && (t.starts_with("squash! ") || t.starts_with("fixup! "))
    {
        return first_line_len(body);
    }
    0
}

/// Append `text` to `buf` with every line commented (`# `, or bare `#` for
/// empty lines), matching Git's `strbuf_add_commented_lines`.
pub fn append_commented(buf: &mut String, text: &str) {
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
/// This reproduces the behavior of Git's `update_squash_message_for_fixup`: it is run
/// when a `fixup -C`/`fixup -c` step replaces the accumulated message (`is_fixup_flag && !seen_squash`)
/// so every section accumulated so far is dropped from the final message. Unlike a single-section
/// marker, it walks the whole buffer, so a chain such as `pick, fixup, fixup -C` correctly skips
/// both the pick target's message and the plain fixup's section (t3437 #8/#12).
pub fn update_squash_message_for_fixup(buf: &mut String) {
    // `comment_line_str` is "#"; the commented header forms Git compares against.
    let mut buf1 = String::from("# This is the 1st commit message:\n");
    let mut buf2 = String::from("# The 1st commit message will be skipped:\n");
    let update_comment_bufs = |b1: &mut String, b2: &mut String, n: usize| {
        *b1 = format!("# This is the commit message #{n}:\n");
        *b2 = format!("# The commit message #{n} will be skipped:\n");
    };

    let orig = std::mem::take(buf);
    let bytes = orig.as_bytes();
    let mut out = String::new();
    // `start` marks the beginning of the not-yet-copied region; `comment_mode` selects whether the
    // copied body is passed through verbatim or commented out (matching Git's `copy_lines` switch).
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

/// Copy `text` into `out`, commenting it out (Git's `add_commented_lines`, which avoids
/// double-commenting already-commented lines) when `comment_mode` is set.
///
/// Already-commented lines (starting with `#`) are passed through verbatim; every other line —
/// including empty ones, which become a bare `#` — is prefixed, matching Git's
/// `strbuf_add_commented_lines`.
pub fn copy_section(out: &mut String, text: &str, comment_mode: bool) {
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
pub fn append_skipped_squash_message(buf: &mut String, body: &str, n: usize) {
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
pub fn append_nth_squash_message(
    buf: &mut String,
    body: &str,
    cmd: RebaseTodoCmd,
    seen_squash: bool,
    n: usize,
) {
    if !buf.ends_with("\n\n") {
        buf.push('\n');
    }
    buf.push_str("# This is the commit message #");
    buf.push_str(&n.to_string());
    buf.push_str(":\n\n");
    let pre = squash_comment_subject_prefix(body, cmd, seen_squash).min(body.len());
    if pre > 0 {
        append_commented(buf, &body[..pre]);
        let rest = &body[pre..];
        let rest = rest.strip_prefix('\n').unwrap_or(rest);
        buf.push_str(rest);
        return;
    }
    buf.push_str(&body[pre..]);
}

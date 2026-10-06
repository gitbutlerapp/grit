//! Mailbox/patch parsing core for `grit am`.
//!
//! This is the self-contained *parse* slice of `grit am`: it turns mbox/stgit/hg
//! patch text into structured [`MboxPatch`] values (author, date, message, diff,
//! …) with no repository, index, filesystem, environment, or CLI dependencies.
//! The `.git/rebase-apply` state machine, patch-to-worktree application, commit
//! assembly, hooks, and all CLI output still live in the `grit` crate; only the
//! text-to-structured-data layer lives here so it can be unit-tested and reused.
//!
//! Warnings that `git am` prints to stderr while parsing (quoted CRLF, lossy
//! `format=flowed`) are collected into a caller-supplied `Vec<String>` rather than
//! printed here; the CLI emits them verbatim so behavior is byte-identical.

use crate::commit_encoding;
use crate::error::{Error, Result};
use crate::objects::ObjectId;

/// A parsed patch from an mbox message.
#[derive(Debug, Clone)]
pub struct MboxPatch {
    /// Author name + email (e.g. "Name <email>").
    pub author: String,
    /// Author date string (for the ident line).
    pub date: String,
    /// Commit message (subject + body).
    pub message: String,
    /// `charset=` from `Content-Type` when present (mbox body encoding).
    pub content_charset: Option<String>,
    /// The unified diff portion.
    pub diff: String,
    /// Message-ID from the email headers.
    pub message_id: String,
    /// When present, the commit OID from a `git format-patch` mbox `From <hex> Mon ...` line.
    pub format_patch_commit: Option<ObjectId>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum QuotedCrAction {
    Warn,
    Strip,
    Nowarn,
}

pub fn parse_quoted_cr_action(value: &str) -> QuotedCrAction {
    match value.trim().to_ascii_lowercase().as_str() {
        "strip" => QuotedCrAction::Strip,
        "nowarn" => QuotedCrAction::Nowarn,
        "warn" => QuotedCrAction::Warn,
        _ => QuotedCrAction::Warn,
    }
}

/// Pine and similar mailers embed folder metadata messages; skip them when applying a concatenated mbox.
pub fn is_skippable_mail_folder_message(patch: &MboxPatch) -> bool {
    let subj = patch
        .message
        .lines()
        .next()
        .unwrap_or("")
        .to_ascii_lowercase();
    if subj.contains("folder internal data") || subj.contains("don't delete this message") {
        return true;
    }
    patch.author.to_ascii_lowercase().contains("mailer-daemon")
}

/// Detect the patch format from file content.
pub fn detect_patch_format(input: &str) -> &'static str {
    let trimmed = input.trim_start();
    if trimmed.starts_with("# HG changeset patch") {
        return "hg";
    }
    // stgit format: first non-blank line is the subject (not a header),
    // followed by From:/Date: headers
    let mut lines = trimmed.lines();
    if let Some(first) = lines.next() {
        // Skip blanks after first line
        let mut peeked = lines.clone();
        // Look at lines 2-5 for From:/Date: pattern typical of stgit
        for _ in 0..5 {
            if let Some(l) = peeked.next() {
                let lt = l.trim();
                if lt.is_empty() {
                    continue;
                }
                if lt.starts_with("From:") || lt.starts_with("Date:") {
                    // Looks like stgit if first line isn't a standard mbox header
                    if !first.starts_with("From ")
                        && !first.starts_with("From:")
                        && !first.starts_with("Subject:")
                        && !first.starts_with("Date:")
                        && !first.starts_with("Message-ID:")
                        && !first.starts_with("X-")
                    {
                        return "stgit";
                    }
                }
                break;
            }
        }
    }
    "mbox"
}

/// Detect if a file is an stgit series file.
/// A series file has the specific comment "# This series applies on GIT commit"
/// followed by filenames.
pub fn is_stgit_series(input: &str) -> bool {
    let mut has_series_header = false;
    let mut has_from_or_date = false;
    for line in input.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        if trimmed.starts_with("# This series applies on GIT commit") {
            has_series_header = true;
        }
        if trimmed.starts_with("From:") || trimmed.starts_with("Date:") {
            has_from_or_date = true;
        }
    }
    // It's a series file if it has the series header and no From:/Date: headers
    has_series_header && !has_from_or_date
}

/// Parse an stgit-format patch into an MboxPatch.
pub fn parse_stgit_patch(input: &str) -> Result<Vec<MboxPatch>> {
    let mut lines = input.lines();
    let mut subject = String::new();
    let mut author = String::new();
    let mut date = String::new();
    let mut body_lines = Vec::new();
    let mut diff_lines = Vec::new();
    let mut in_diff = false;
    let mut in_headers;
    let mut past_separator = false;

    // First non-blank line is the subject
    for line in lines.by_ref() {
        if !line.trim().is_empty() {
            subject = line.trim().to_string();
            break;
        }
    }

    // Next lines are headers (From:, Date:) until blank line
    in_headers = true;
    for line in lines.by_ref() {
        if in_headers {
            if line.trim().is_empty() {
                in_headers = false;
                continue;
            }
            if let Some(val) = line.strip_prefix("From:") {
                author = val.trim().to_string();
                continue;
            }
            if let Some(val) = line.strip_prefix("Date:") {
                date = val.trim().to_string();
                continue;
            }
            // Not a header — must be body
            in_headers = false;
            body_lines.push(line);
            continue;
        }

        if !in_diff {
            if line == "---" {
                past_separator = true;
                continue;
            }
            if past_separator && line.starts_with("diff --git ") {
                in_diff = true;
                diff_lines.push(line);
                continue;
            }
            if past_separator {
                // Skip diffstat lines between --- and diff --git
                continue;
            }
            if line.starts_with("diff --git ") {
                in_diff = true;
                diff_lines.push(line);
                continue;
            }
            body_lines.push(line);
        } else {
            if line == "-- " {
                break;
            }
            diff_lines.push(line);
        }
    }

    let author_ident = parse_author_ident(&author, &date);
    let body = body_lines.join("\n").trim().to_string();
    let message = if body.is_empty() {
        format!("{}\n", subject)
    } else {
        format!("{}\n\n{}\n", subject, body)
    };
    let mut diff = diff_lines.join("\n");
    if !diff.is_empty() {
        diff.push('\n');
    }

    Ok(vec![MboxPatch {
        author: author_ident.0,
        date: author_ident.1,
        message,
        content_charset: None,
        diff,
        message_id: String::new(),
        format_patch_commit: None,
    }])
}

/// Parse an hg (Mercurial) format patch into an MboxPatch.
pub fn parse_hg_patch(input: &str) -> Result<Vec<MboxPatch>> {
    let mut lines = input.lines();
    let mut author = String::new();
    let mut date = String::new();
    let mut body_lines = Vec::new();
    let mut diff_lines = Vec::new();
    let mut in_diff = false;

    // Parse HG headers (lines starting with #)
    for line in lines.by_ref() {
        let trimmed = line.trim();
        if trimmed == "# HG changeset patch" {
            continue;
        }
        if let Some(val) = trimmed.strip_prefix("# User ") {
            author = val.to_string();
            continue;
        }
        if let Some(val) = trimmed.strip_prefix("# Date ") {
            // HG date format: "epoch offset" where offset is seconds west of UTC
            // Convert to git format: "epoch +/-HHMM"
            let parts: Vec<&str> = val.split_whitespace().collect();
            if parts.len() >= 2 {
                if let (Ok(epoch), Ok(offset_secs)) =
                    (parts[0].parse::<i64>(), parts[1].parse::<i64>())
                {
                    // HG offset is seconds west of UTC (positive = west)
                    // Git offset is +/-HHMM (positive = east)
                    let git_offset_secs = -offset_secs;
                    let sign = if git_offset_secs >= 0 { '+' } else { '-' };
                    let abs_secs = git_offset_secs.unsigned_abs();
                    let hours = abs_secs / 3600;
                    let mins = (abs_secs % 3600) / 60;
                    date = format!("{} {}{:02}{:02}", epoch, sign, hours, mins);
                } else {
                    date = val.to_string();
                }
            } else {
                date = val.to_string();
            }
            continue;
        }
        if trimmed.starts_with("# ") || trimmed == "#" {
            // Skip other HG headers (Node ID, Parent, etc.)
            continue;
        }
        // First non-header line — this is the start of the body
        body_lines.push(line);
        break;
    }

    // Parse remaining body + diff
    for line in lines {
        if !in_diff {
            if line.starts_with("diff --git ") || line.starts_with("diff -r ") {
                in_diff = true;
                diff_lines.push(line);
                continue;
            }
            body_lines.push(line);
        } else {
            diff_lines.push(line);
        }
    }

    let author_ident = parse_author_ident(&author, &date);
    let body = body_lines.join("\n").trim().to_string();
    // For HG patches, the first line of the body is the subject
    let (subject, rest) = if let Some(idx) = body.find('\n') {
        (body[..idx].to_string(), body[idx + 1..].trim().to_string())
    } else {
        (body.clone(), String::new())
    };

    let message = if rest.is_empty() {
        format!("{}\n", subject)
    } else {
        format!("{}\n\n{}\n", subject, rest)
    };
    let mut diff = diff_lines.join("\n");
    if !diff.is_empty() {
        diff.push('\n');
    }

    Ok(vec![MboxPatch {
        author: author_ident.0,
        date: author_ident.1,
        message,
        content_charset: None,
        diff,
        message_id: String::new(),
        format_patch_commit: None,
    }])
}
#[expect(clippy::too_many_arguments)]
/// Parse patches from input, auto-detecting or using the specified format.
///
/// `warnings` collects stderr warnings (`format=flowed`, quoted CRLF) that `git am`
/// prints while parsing; the caller emits them verbatim.
pub fn parse_patches(
    input: &str,
    format: Option<&str>,
    keep: bool,
    keep_non_patch: bool,
    scissors: bool,
    no_scissors: bool,
    keep_cr: bool,
    quoted_cr_action: QuotedCrAction,
    warnings: &mut Vec<String>,
) -> Result<Vec<MboxPatch>> {
    let fmt = format.unwrap_or_else(|| detect_patch_format(input));
    match fmt {
        "stgit" => parse_stgit_patch(input),
        "hg" => parse_hg_patch(input),
        _ => parse_mbox_with_opts(
            input,
            keep,
            keep_non_patch,
            scissors,
            no_scissors,
            keep_cr,
            quoted_cr_action,
            warnings,
        ),
    }
}

/// Unquote mboxrd format: lines starting with >From (or >>From, etc.) are unquoted.
/// In mboxrd, "From " lines inside messages are escaped by prepending ">".
/// Un-flow format=flowed lines (RFC 3676).
/// Lines ending with a trailing space are "flowed" — joined with the next line.
/// Also handles space-unstuffing: one leading space is removed from lines
/// that start with a space (to undo the space-stuffing required by RFC 3676).
fn unflow_format_flowed(lines: &[&str]) -> Vec<String> {
    let mut result = Vec::new();
    let mut current = String::new();

    for line in lines {
        // Space-unstuffing: remove one leading space
        let unstuffed = line.strip_prefix(' ').unwrap_or(line);

        if unstuffed.ends_with(' ') {
            // Flowed line: keep the trailing space (it's content), join with next
            current.push_str(unstuffed);
        } else if !current.is_empty() {
            current.push_str(unstuffed);
            result.push(current.clone());
            current.clear();
        } else {
            result.push(unstuffed.to_string());
        }
    }
    if !current.is_empty() {
        result.push(current);
    }
    result
}

fn split_lines_preserve_cr(input: &str) -> Vec<&str> {
    if input.is_empty() {
        return Vec::new();
    }
    let mut lines: Vec<&str> = input.split('\n').collect();
    if input.ends_with('\n') {
        lines.pop();
    }
    lines
}

fn unquote_mboxrd(input: &str) -> String {
    let mut result = String::with_capacity(input.len());
    let mut in_body = false;

    for line in split_lines_preserve_cr(input) {
        let line_no_cr = line.strip_suffix('\r').unwrap_or(line);
        if line_no_cr.starts_with("From ") && line_no_cr.len() > 5 {
            // mbox separator - reset state
            in_body = false;
            result.push_str(line);
            result.push('\n');
            continue;
        }

        if !in_body {
            if line_no_cr.is_empty() {
                in_body = true;
            }
            result.push_str(line);
            result.push('\n');
            continue;
        }

        // In body: unquote >From lines
        if line_no_cr.starts_with(">From ")
            || (line_no_cr.starts_with(">>") && line_no_cr.contains("From "))
        {
            // Strip one leading > if the line matches >+From pattern
            let stripped = line.strip_prefix(">").unwrap_or(line);
            result.push_str(stripped);
        } else {
            result.push_str(line);
        }
        result.push('\n');
    }

    // Remove trailing extra newline if input didn't end with one
    if !input.ends_with('\n') && result.ends_with('\n') {
        result.pop();
    }

    result
}

fn base64_decode(input: &str) -> Result<Vec<u8>> {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut output = Vec::new();
    let mut buf: u32 = 0;
    let mut bits: u32 = 0;

    for &byte in input.as_bytes() {
        if byte == b'=' {
            break;
        }
        if byte.is_ascii_whitespace() {
            continue;
        }
        let val = TABLE
            .iter()
            .position(|&c| c == byte)
            .ok_or_else(|| Error::Message("invalid base64 payload in mbox".to_string()))?;
        buf = (buf << 6) | val as u32;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            output.push((buf >> bits) as u8);
            buf &= (1 << bits) - 1;
        }
    }

    Ok(output)
}

fn decode_transfer_payload(
    payload: &str,
    transfer_encoding: &str,
    keep_cr: bool,
    quoted_cr_action: QuotedCrAction,
    warnings: &mut Vec<String>,
) -> Result<String> {
    if transfer_encoding != "base64" {
        if keep_cr {
            return Ok(payload.to_string());
        }
        return Ok(payload.replace('\r', ""));
    }

    let decoded = base64_decode(payload)?;
    let mut text = String::from_utf8_lossy(&decoded).into_owned();
    if !keep_cr && text.contains('\r') {
        match quoted_cr_action {
            QuotedCrAction::Strip => {
                text = text.replace('\r', "");
            }
            QuotedCrAction::Warn => {
                warnings.push("warning: quoted CRLF detected".to_string());
            }
            QuotedCrAction::Nowarn => {}
        }
    }
    Ok(text)
}

fn split_message_body_and_diff(payload_lines: &[String]) -> (Vec<String>, Vec<String>) {
    let mut body_lines = Vec::new();
    let mut diff_lines = Vec::new();
    let mut i = 0usize;
    let mut in_diff = false;

    while i < payload_lines.len() {
        let line = payload_lines[i].as_str();
        let line_no_cr = line.strip_suffix('\r').unwrap_or(line);
        if !in_diff {
            if line_no_cr == "---" {
                i += 1;
                while i < payload_lines.len() {
                    let stat_line = payload_lines[i].as_str();
                    let stat_line_no_cr = stat_line.strip_suffix('\r').unwrap_or(stat_line);
                    if stat_line_no_cr.starts_with("diff --git ") {
                        in_diff = true;
                        break;
                    }
                    i += 1;
                }
                continue;
            }
            if line_no_cr.starts_with("diff --git ") {
                in_diff = true;
            } else {
                body_lines.push(payload_lines[i].clone());
                i += 1;
                continue;
            }
        }

        if line_no_cr == "-- " {
            break;
        }
        diff_lines.push(payload_lines[i].clone());
        i += 1;
    }

    (body_lines, diff_lines)
}

/// If `line` is a `git format-patch` mbox separator (`From <40-hex> Mon Sep 17 ...`), return the
/// commit OID; otherwise `None` (e.g. `From: user@host` in mail headers).
fn parse_format_patch_commit_oid_from_mbox_line(line: &str) -> Option<ObjectId> {
    let after_from = line.strip_prefix("From")?;
    if after_from.starts_with(':') {
        return None;
    }
    let rest = after_from.trim_start();
    let (token, tail) = rest.split_once(char::is_whitespace)?;
    if token.len() != 40 || !tail.trim_start().starts_with("Mon ") {
        return None;
    }
    ObjectId::from_hex(token).ok()
}
#[expect(clippy::too_many_arguments)]
/// Parse an mbox file into individual patches with options.
///
/// `warnings` collects stderr warnings (`format=flowed`, quoted CRLF) that `git am`
/// prints while parsing; the caller emits them verbatim.
pub fn parse_mbox_with_opts(
    input: &str,
    keep: bool,
    keep_non_patch: bool,
    scissors: bool,
    no_scissors: bool,
    keep_cr: bool,
    quoted_cr_action: QuotedCrAction,
    warnings: &mut Vec<String>,
) -> Result<Vec<MboxPatch>> {
    // Handle mboxrd: unquote >From lines
    let input = unquote_mboxrd(input);
    let mut patches = Vec::new();
    let line_storage = split_lines_preserve_cr(&input);
    let mut lines = line_storage.iter().copied().peekable();

    while lines.peek().is_some() {
        // Skip to next "From " line (mbox separator)
        // Or if we're at the start and there's no "From " line, treat as single patch
        let mut _in_headers = false;
        let mut author = String::new();
        let mut date = String::new();
        let mut subject = String::new();
        let mut message_id = String::new();
        let _body = String::new();
        let mut found_from = false;
        let mut format_patch_commit: Option<ObjectId> = None;

        // Look for "From " separator line
        while let Some(&line) = lines.peek() {
            let line_no_cr = line.strip_suffix('\r').unwrap_or(line);
            if line_no_cr.starts_with("From ") && line_no_cr.len() > 5 {
                found_from = true;
                format_patch_commit = parse_format_patch_commit_oid_from_mbox_line(line_no_cr);
                lines.next(); // consume "From " line
                break;
            }
            // If we haven't found any "From " line yet and we see headers, treat as raw patch
            if !found_from
                && (line_no_cr.starts_with("From:")
                    || line_no_cr.starts_with("Subject:")
                    || line_no_cr.starts_with("Date:")
                    || line_no_cr.starts_with("Message-ID:")
                    || line_no_cr.starts_with("Message-Id:")
                    || line_no_cr.starts_with("X-"))
            {
                found_from = true;
                break;
            }
            if !found_from {
                lines.next(); // skip non-header lines before first message
                continue;
            }
            break;
        }

        if !found_from && lines.peek().is_none() {
            break;
        }

        // Parse headers
        _in_headers = true;
        let mut last_header = String::new();
        let mut is_format_flowed = false;
        let mut content_transfer_encoding = String::new();
        let mut content_charset: Option<String> = None;

        while let Some(&line) = lines.peek() {
            let line_no_cr = line.strip_suffix('\r').unwrap_or(line);
            if line_no_cr.is_empty() {
                lines.next();
                _in_headers = false;
                break;
            }
            // Continuation line (starts with whitespace)
            if (line_no_cr.starts_with(' ') || line_no_cr.starts_with('\t'))
                && !last_header.is_empty()
            {
                if last_header == "subject" {
                    subject.push(' ');
                    subject.push_str(line_no_cr.trim());
                }
                lines.next();
                continue;
            }

            if let Some(value) = line_no_cr.strip_prefix("From: ") {
                author = commit_encoding::decode_rfc2047_mailbox_from_line(value.trim());
                last_header = "from".to_string();
            } else if let Some(value) = line_no_cr.strip_prefix("Date: ") {
                date = value.trim().to_string();
                last_header = "date".to_string();
            } else if let Some(value) = line_no_cr.strip_prefix("Subject: ") {
                // Strip [PATCH ...] prefix unless --keep
                let subj = if keep {
                    value.trim().to_string()
                } else if keep_non_patch {
                    strip_patch_prefix_keep_non_patch(value.trim())
                } else {
                    strip_patch_prefix(value.trim())
                };
                subject = subj;
                last_header = "subject".to_string();
            } else if let Some(value) = line_no_cr
                .strip_prefix("Message-ID: ")
                .or_else(|| line_no_cr.strip_prefix("Message-Id: "))
                .or_else(|| line_no_cr.strip_prefix("Message-id: "))
            {
                message_id = value.trim().to_string();
                last_header = "message-id".to_string();
            } else if let Some(value) = line_no_cr
                .strip_prefix("Content-Type: ")
                .or_else(|| line_no_cr.strip_prefix("Content-type: "))
            {
                for part in value.split(';').skip(1) {
                    let p = part.trim();
                    let lower = p.to_ascii_lowercase();
                    if let Some(rest) = lower.strip_prefix("charset=") {
                        let mut cs = rest.trim().trim_matches('"').trim_matches('\'');
                        if let Some((a, _)) = cs.split_once(';') {
                            cs = a.trim();
                        }
                        if !cs.is_empty() {
                            content_charset = Some(cs.to_owned());
                        }
                    }
                }
                if value.to_lowercase().contains("format=flowed") {
                    is_format_flowed = true;
                }
                last_header = "content-type".to_string();
            } else if let Some(value) = line_no_cr
                .strip_prefix("Content-Transfer-Encoding: ")
                .or_else(|| line_no_cr.strip_prefix("Content-transfer-encoding: "))
            {
                content_transfer_encoding = value.trim().to_ascii_lowercase();
                last_header = "content-transfer-encoding".to_string();
            } else {
                last_header = String::new();
            }
            lines.next();
        }

        let mut raw_payload_lines = Vec::new();
        while let Some(&line) = lines.peek() {
            let line_no_cr = line.strip_suffix('\r').unwrap_or(line);
            if line_no_cr.starts_with("From ") && line_no_cr.len() > 5 {
                break;
            }
            raw_payload_lines.push(line.to_string());
            lines.next();
        }

        let raw_payload = raw_payload_lines.join("\n");
        let decoded_payload = decode_transfer_payload(
            &raw_payload,
            &content_transfer_encoding,
            keep_cr,
            quoted_cr_action,
            warnings,
        )?;
        let mut payload_lines: Vec<String> = decoded_payload
            .split('\n')
            .map(|l| {
                if keep_cr {
                    l.to_string()
                } else {
                    l.strip_suffix('\r').unwrap_or(l).to_string()
                }
            })
            .collect();
        if payload_lines.last().is_some_and(String::is_empty) {
            payload_lines.pop();
        }
        let (body_lines, diff_lines) = split_message_body_and_diff(&payload_lines);

        // Build message from subject + body. Subject continuation lines in
        // mailbox headers are folded in two ways:
        // - default (`git am`): unwrap subject continuations into one line;
        // - keep mode (`git am -k`): preserve continuation line breaks.
        //
        // `Subject:` continuation lines are captured in `body_lines` by this
        // parser, so normalize here before constructing the final message.
        let mut effective_body_lines: Vec<String> = if is_format_flowed {
            let body_refs: Vec<&str> = body_lines.iter().map(String::as_str).collect();
            unflow_format_flowed(&body_refs)
        } else {
            body_lines.clone()
        };
        let mut body_str = effective_body_lines.join("\n").trim().to_string();
        if !body_str.is_empty() && !subject.is_empty() {
            let mut consumed = 0usize;
            let mut continuation = Vec::new();
            for line in &effective_body_lines {
                if line.trim().is_empty() {
                    break;
                }
                continuation.push(line.trim().to_string());
                consumed += 1;
            }
            if !continuation.is_empty() {
                if keep {
                    subject = format!("{subject}\n{}", continuation.join("\n"));
                } else {
                    subject = format!("{subject} {}", continuation.join(" "));
                }
                effective_body_lines.drain(0..consumed);
                body_str = effective_body_lines.join("\n").trim().to_string();
            }
        }

        // Handle --scissors: trim at scissors line, potentially replace subject
        if scissors && !no_scissors {
            let (new_subject, new_body) = apply_scissors_to_message(&subject, &body_str);
            subject = new_subject;
            body_str = new_body;
        }

        let message = if body_str.is_empty() {
            format!("{}\n", subject)
        } else {
            format!("{}\n\n{}\n", subject, body_str)
        };

        // Parse author into "Name <email>" format and extract date
        let author_ident = parse_author_ident(&author, &date);

        // Un-flow format=flowed content
        let effective_diff_lines: Vec<String> = if is_format_flowed {
            warnings.push(
                "warning: Patch sent with format=flowed; space at the end of lines might be lost."
                    .to_string(),
            );
            let diff_refs: Vec<&str> = diff_lines.iter().map(String::as_str).collect();
            unflow_format_flowed(&diff_refs)
        } else {
            diff_lines.clone()
        };

        let mut diff_section = effective_diff_lines.join("\n");
        if !diff_section.is_empty() {
            diff_section.push('\n');
        }

        if !subject.is_empty() || !diff_section.is_empty() {
            patches.push(MboxPatch {
                author: author_ident.0,
                date: author_ident.1,
                message,
                content_charset,
                diff: diff_section,
                message_id: message_id.clone(),
                format_patch_commit,
            });
        }
    }

    Ok(patches)
}

/// Strip "[PATCH n/m] " or "[PATCH] " prefix from subject.
fn strip_patch_prefix(subject: &str) -> String {
    if subject.starts_with('[') {
        if let Some(end) = subject.find(']') {
            let rest = subject[end + 1..].trim();
            if !rest.is_empty() {
                return rest.to_string();
            }
        }
    }
    subject.to_string()
}

/// Strip only PATCH-related bracket content, keep non-patch brackets.
fn strip_patch_prefix_keep_non_patch(subject: &str) -> String {
    if subject.starts_with('[') {
        if let Some(end) = subject.find(']') {
            let bracket_content = &subject[1..end];
            // If it looks like a PATCH prefix, strip it
            if bracket_content.contains("PATCH") {
                let rest = subject[end + 1..].trim();
                if !rest.is_empty() {
                    return rest.to_string();
                }
            }
        }
    }
    subject.to_string()
}

/// Apply scissors to the full message (subject + body), replacing subject if needed.
fn apply_scissors_to_message(subject: &str, body: &str) -> (String, String) {
    // Check if scissors line is in the body
    let mut scissors_idx = None;
    let body_lines: Vec<&str> = body.lines().collect();
    for (i, line) in body_lines.iter().enumerate() {
        if is_scissors_line(line.trim()) {
            scissors_idx = Some(i);
            break;
        }
    }

    if let Some(idx) = scissors_idx {
        // Everything after scissors
        let after: Vec<&str> = body_lines[idx + 1..].to_vec();
        let after_text = after.join("\n");
        let after_trimmed = after_text.trim();

        // Look for Subject: pseudo-header after scissors
        let mut new_subject = String::new();
        let mut new_body_lines = Vec::new();
        let mut in_headers = true;

        for line in after_trimmed.lines() {
            if in_headers {
                if line.is_empty() {
                    in_headers = false;
                    continue;
                }
                if let Some(val) = line.strip_prefix("Subject: ") {
                    new_subject = val.trim().to_string();
                    continue;
                }
                // Non-header line
                in_headers = false;
                new_body_lines.push(line);
            } else {
                new_body_lines.push(line);
            }
        }

        if new_subject.is_empty() {
            new_subject = subject.to_string();
        }

        let new_body = new_body_lines.join("\n").trim().to_string();
        (new_subject, new_body)
    } else {
        (subject.to_string(), body.to_string())
    }
}

/// Check if a line is a scissors line.
/// Git looks for lines containing ">8" or "8<" preceded by dashes/spaces.
/// Examples: "-- >8 --", " - - >8 - - remove everything above"
fn is_scissors_line(line: &str) -> bool {
    // Find ">8" or "8<" in the line
    let scissors_pos = if let Some(pos) = line.find(">8") {
        pos
    } else if let Some(pos) = line.find("8<") {
        pos
    } else {
        return false;
    };

    // Everything before the scissors marker must be only '-' and ' '
    let prefix = &line[..scissors_pos];
    if prefix.is_empty() {
        return false;
    }
    prefix.chars().all(|c| c == '-' || c == ' ')
}

/// Parse "Name <email>" and date string into (author_ident, epoch_offset).
fn parse_author_ident(author: &str, date: &str) -> (String, String) {
    // Try to parse the date into epoch format
    let epoch_date = parse_date_to_epoch(date);
    (author.to_string(), epoch_date)
}

/// Try to parse various date formats into "epoch offset" format.
fn parse_date_to_epoch(date: &str) -> String {
    if date.is_empty() {
        return String::new();
    }

    // Already in "epoch offset" format?
    let parts: Vec<&str> = date.split_whitespace().collect();
    if parts.len() == 2 && parts[0].parse::<i64>().is_ok() {
        return date.to_string();
    }

    // Try RFC 2822-like: "Thu, 07 Apr 2005 22:14:13 -0700"
    if let Some(parsed) = parse_rfc2822_date(date) {
        return parsed;
    }

    // Fall back: just use the date string as-is
    date.to_string()
}

/// Parse an RFC 2822-style date into "epoch offset" format.
fn parse_rfc2822_date(date: &str) -> Option<String> {
    // Format: "Day, DD Mon YYYY HH:MM:SS +/-HHMM" or without the day prefix
    let trimmed = date.trim();

    // Extract the timezone offset (last token)
    let (date_part, tz_str) = {
        let parts: Vec<&str> = trimmed.rsplitn(2, ' ').collect();
        if parts.len() != 2 {
            return None;
        }
        (parts[1], parts[0])
    };

    // Parse timezone offset like +0700 or -0700
    if tz_str.len() != 5 {
        return None;
    }
    let tz_sign = match tz_str.chars().next()? {
        '+' => 1i32,
        '-' => -1i32,
        _ => return None,
    };
    let tz_hours: i32 = tz_str[1..3].parse().ok()?;
    let tz_mins: i32 = tz_str[3..5].parse().ok()?;
    let tz_offset_secs = tz_sign * (tz_hours * 3600 + tz_mins * 60);

    // Strip leading "Day, " if present
    let date_str = if date_part.contains(',') {
        let (_, rest) = date_part.split_once(',')?;
        rest.trim()
    } else {
        date_part.trim()
    };

    // Parse "DD Mon YYYY HH:MM:SS"
    let tokens: Vec<&str> = date_str.split_whitespace().collect();
    if tokens.len() < 4 {
        return None;
    }

    let day: u32 = tokens[0].parse().ok()?;
    let month = match tokens[1].to_lowercase().as_str() {
        "jan" => 1u32,
        "feb" => 2,
        "mar" => 3,
        "apr" => 4,
        "may" => 5,
        "jun" => 6,
        "jul" => 7,
        "aug" => 8,
        "sep" => 9,
        "oct" => 10,
        "nov" => 11,
        "dec" => 12,
        _ => return None,
    };
    let year: i32 = tokens[2].parse().ok()?;
    let time_parts: Vec<&str> = tokens[3].split(':').collect();
    if time_parts.len() < 2 {
        return None;
    }
    let hour: u32 = time_parts[0].parse().ok()?;
    let min: u32 = time_parts[1].parse().ok()?;
    let sec: u32 = if time_parts.len() > 2 {
        time_parts[2].parse().ok()?
    } else {
        0
    };

    // Convert to Unix timestamp
    // Days from year 0 to year, then month/day, then subtract Unix epoch
    let epoch = datetime_to_epoch(year, month, day, hour, min, sec, tz_offset_secs)?;

    Some(format!("{} {}", epoch, tz_str))
}

/// Convert a date to Unix epoch seconds.
fn datetime_to_epoch(
    year: i32,
    month: u32,
    day: u32,
    hour: u32,
    min: u32,
    sec: u32,
    tz_offset_secs: i32,
) -> Option<i64> {
    // Use a simple calculation
    let m = if month <= 2 { month + 12 } else { month };
    let y = if month <= 2 { year - 1 } else { year };

    // Julian Day Number
    let jdn = (day as i64) + (153 * (m as i64 - 3) + 2) / 5 + 365 * (y as i64) + (y as i64) / 4
        - (y as i64) / 100
        + (y as i64) / 400
        + 1721119;

    // Unix epoch = JDN of 1970-01-01 = 2440588
    let days_since_epoch = jdn - 2440588;
    let secs = days_since_epoch * 86400 + (hour as i64) * 3600 + (min as i64) * 60 + (sec as i64);
    let utc_secs = secs - (tz_offset_secs as i64);

    Some(utc_secs)
}

/// Serialize an MboxPatch for storage in the state directory.
pub fn serialize_mbox_patch(patch: &MboxPatch) -> String {
    let mut out = String::new();
    out.push_str(&format!("Author: {}\n", patch.author));
    out.push_str(&format!("Date: {}\n", patch.date));
    if let Some(oid) = patch.format_patch_commit {
        out.push_str(&format!("Format-Patch-Commit: {}\n", oid.to_hex()));
    }
    if let Some(ref cs) = patch.content_charset {
        out.push_str(&format!("Content-Charset: {cs}\n"));
    }
    if !patch.message_id.is_empty() {
        out.push_str(&format!("Message-ID: {}\n", patch.message_id));
    }
    out.push_str(&format!("Message-Length: {}\n", patch.message.len()));
    out.push_str(&format!("Diff-Length: {}\n", patch.diff.len()));
    out.push('\n');
    out.push_str(&patch.message);
    out.push_str(&patch.diff);
    out
}

/// Deserialize an MboxPatch from state directory storage.
pub fn deserialize_mbox_patch(data: &str) -> Result<MboxPatch> {
    let mut author = String::new();
    let mut date = String::new();
    let mut message_id = String::new();
    let mut content_charset: Option<String> = None;
    let mut format_patch_commit: Option<ObjectId> = None;
    let mut msg_len = 0usize;
    let mut diff_len = 0usize;

    let split_at = data.find("\n\n").unwrap_or(data.len());
    let header = &data[..split_at];
    let remaining = if split_at < data.len() {
        &data[split_at + 2..]
    } else {
        ""
    };

    for line in header.split('\n') {
        let line = line.trim_end_matches('\r');
        if let Some(v) = line.strip_prefix("Author: ") {
            author = v.to_string();
        } else if let Some(v) = line.strip_prefix("Date: ") {
            date = v.to_string();
        } else if let Some(v) = line.strip_prefix("Message-ID: ") {
            message_id = v.to_string();
        } else if let Some(v) = line.strip_prefix("Format-Patch-Commit: ") {
            format_patch_commit = ObjectId::from_hex(v.trim()).ok();
        } else if let Some(v) = line.strip_prefix("Content-Charset: ") {
            content_charset = Some(v.to_string());
        } else if let Some(v) = line.strip_prefix("Message-Length: ") {
            msg_len = v.parse().unwrap_or(0);
        } else if let Some(v) = line.strip_prefix("Diff-Length: ") {
            diff_len = v.parse().unwrap_or(0);
        }
    }

    let message = if msg_len > 0 && msg_len <= remaining.len() {
        remaining[..msg_len].to_string()
    } else {
        remaining.to_string()
    };

    let diff = if diff_len > 0 && msg_len.saturating_add(diff_len) <= remaining.len() {
        remaining[msg_len..msg_len + diff_len].to_string()
    } else if msg_len < remaining.len() {
        remaining[msg_len..].to_string()
    } else {
        String::new()
    };

    Ok(MboxPatch {
        author,
        date,
        message,
        content_charset,
        diff,
        message_id,
        format_patch_commit,
    })
}

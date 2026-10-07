//! `git format-patch` mail-rendering primitives.
//!
//! The full `format-patch` command in the `grit` binary parses argv, walks the
//! revision range, reads config, runs rename detection, talks to the notes /
//! range-diff machinery, decides on output files vs. stdout, prints the written
//! filenames, and maps exit codes. Those responsibilities — argv parsing,
//! revision selection, config/env resolution, file output, terminal printing,
//! and the `crate`-internal log/range-diff dispatch — stay in the CLI.
//!
//! What lives here is the self-contained, presentation-free core of mbox patch
//! assembly: the byte-exact RFC 2047 / RFC 822 header encoders and folders, the
//! `[PATCH n/m]` subject builder, the filename sanitizer/truncator, the
//! committer-date formatter, the threading-header writer, and the small string
//! transforms (`mboxrd` escaping, ident formatting, reroll/version labels) that
//! `git`'s `pretty.c`, `utf8.c`, and `builtin/log.c` use to turn a commit plus
//! its diff into an email. Each function computes a result from plain strings,
//! bytes, or a [`CommitData`] alone — no argv, no terminal output, and no
//! process/filesystem state — so the CLI can call them while still owning every
//! I/O and config decision.

use crate::objects::CommitData;

/// Format an identity string as `"Name <email>"`.
pub fn format_ident(ident: &str) -> String {
    if let Some(bracket) = ident.find('<') {
        if let Some(end) = ident.find('>') {
            let name = ident[..bracket].trim();
            let email = &ident[bracket..=end];
            return format!("{name} {email}");
        }
    }
    ident.to_owned()
}

/// Encode an email address for use in email headers.
///
/// Rules:
/// - If the display name contains non-ASCII chars → RFC 2047 encode it
/// - If the display name contains RFC 822 special chars (like `.`) → quote it
/// - Otherwise → use as-is
pub fn encode_email_address(addr: &str) -> String {
    // Parse "Display Name <email@example.com>" form
    if let (Some(lt), Some(gt)) = (addr.rfind('<'), addr.rfind('>')) {
        if lt < gt {
            let name = addr[..lt].trim();
            let email_part = &addr[lt..=gt]; // "<email>"
            if name.is_empty() {
                return addr.to_string();
            }
            let encoded_name = encode_display_name(name);
            return format!("{encoded_name} {email_part}");
        }
    }
    // No angle brackets — return as-is
    addr.to_string()
}

/// Charset token for RFC 2047 `=?charset?q?...?=` (matches Git test expectations).
pub fn rfc2047_charset_label(log_output_encoding: &str) -> String {
    let t = log_output_encoding.trim();
    let lower = t.to_ascii_lowercase();
    if lower == "utf-8" || lower == "utf8" {
        return "UTF-8".to_owned();
    }
    if matches!(
        lower.as_str(),
        "iso-8859-1" | "iso8859-1" | "latin1" | "latin-1"
    ) {
        return "ISO8859-1".to_owned();
    }
    t.to_owned()
}

/// Like [`encode_email_address`] but uses `charset_label` for RFC 2047 when non-ASCII.
pub fn encode_email_address_for_charset(addr: &str, charset_label: &str) -> String {
    if charset_label.eq_ignore_ascii_case("UTF-8") {
        return encode_email_address(addr);
    }
    if let (Some(lt), Some(gt)) = (addr.rfind('<'), addr.rfind('>')) {
        if lt < gt {
            let name = addr[..lt].trim();
            let email_part = &addr[lt..=gt];
            if name.is_empty() {
                return addr.to_string();
            }
            let encoded_name = encode_display_name_for_charset(name, charset_label);
            return format!("{encoded_name} {email_part}");
        }
    }
    addr.to_string()
}

fn encode_display_name_for_charset(name: &str, charset_label: &str) -> String {
    if charset_label.eq_ignore_ascii_case("UTF-8") {
        return encode_display_name(name);
    }
    if name.bytes().any(|b| b > 0x7f) {
        return rfc2047_encode_with_charset(name, charset_label);
    }
    let specials = |c: char| {
        matches!(
            c,
            '(' | ')' | '<' | '>' | '[' | ']' | ':' | ';' | '@' | '\\' | ',' | '.' | '"'
        )
    };
    if name.chars().any(specials) {
        let escaped = name.replace('\\', "\\\\").replace('"', "\\\"");
        return format!("\"{escaped}\"");
    }
    name.to_string()
}

fn rfc2047_encode_with_charset(name: &str, charset_label: &str) -> String {
    let bytes = if charset_label.eq_ignore_ascii_case("UTF-8") {
        name.as_bytes().to_vec()
    } else {
        match crate::commit_encoding::encode_unicode(charset_label, name) {
            Some(mut raw) => {
                while raw.last() == Some(&b'\n') {
                    raw.pop();
                }
                raw
            }
            None => return rfc2047_encode(name),
        }
    };
    let mut encoded = String::new();
    for &byte in &bytes {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' => {
                encoded.push(byte as char);
            }
            b' ' => encoded.push_str("=20"),
            _ => encoded.push_str(&format!("={byte:02X}")),
        }
    }
    format!("=?{charset_label}?q?{encoded}?=")
}

/// Encode a display name portion of an email address.
///
/// - Non-ASCII → RFC 2047 UTF-8 quoted-printable
/// - Contains RFC 822 specials → RFC 822 quoted string
/// - Otherwise → plain
pub fn encode_display_name(name: &str) -> String {
    // Check for non-ASCII
    if name.bytes().any(|b| b > 0x7f) {
        return rfc2047_encode(name);
    }
    // RFC 822 specials that require quoting
    // Specials are: ( ) < > [ ] : ; @ \ , . "
    let specials = |c: char| {
        matches!(
            c,
            '(' | ')' | '<' | '>' | '[' | ']' | ':' | ';' | '@' | '\\' | ',' | '.' | '"'
        )
    };
    if name.chars().any(specials) {
        // Quote the name
        let escaped = name.replace('\\', "\\\\").replace('"', "\\\"");
        return format!("\"{escaped}\"");
    }
    name.to_string()
}

/// RFC 2047 UTF-8 quoted-printable encoding for an email display name.
pub fn rfc2047_encode(name: &str) -> String {
    let mut encoded = String::new();
    for byte in name.as_bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' => {
                encoded.push(*byte as char);
            }
            b' ' => {
                encoded.push_str("=20");
            }
            _ => {
                encoded.push_str(&format!("={:02X}", byte));
            }
        }
    }
    format!("=?UTF-8?q?{encoded}?=")
}

/// Write a folded email header with multiple values.
///
/// Emits:
/// ```text
/// HeaderName: value1,
///  value2
/// ```
pub fn write_folded_header(out: &mut String, name: &str, values: &[String]) {
    if values.is_empty() {
        return;
    }
    out.push_str(name);
    out.push_str(": ");
    for (i, val) in values.iter().enumerate() {
        if i > 0 {
            out.push_str(",\n ");
        }
        out.push_str(val);
    }
    out.push('\n');
}

/// Extract date from identity string and format as RFC 2822-like.
pub fn format_date_rfc2822(ident: &str) -> String {
    // Git ident: "Name <email> timestamp offset"
    let parts: Vec<&str> = ident.rsplitn(3, ' ').collect();
    if parts.len() >= 2 {
        let ts_str = parts[1];
        let offset_str = parts[0];
        if let Ok(ts) = ts_str.parse::<i64>() {
            // Parse the offset string (e.g. "+0000", "-0700") into a UtcOffset
            let tz_offset = parse_tz_offset(offset_str).unwrap_or(time::UtcOffset::UTC);
            let dt = time::OffsetDateTime::from_unix_timestamp(ts)
                .unwrap_or(time::OffsetDateTime::UNIX_EPOCH)
                .to_offset(tz_offset);
            // git uses a space-padded day-of-month (e.g. "Thu, 7 Apr 2005"), not zero-padded.
            let format = time::format_description::parse_borrowed::<1>(
                "[weekday repr:short], [day padding:none] [month repr:short] [year] [hour]:[minute]:[second] ",
            );
            if let Ok(fmt) = format {
                if let Ok(formatted) = dt.format(&fmt) {
                    return format!("{formatted}{offset_str}");
                }
            }
        }
        format!("{ts_str} {offset_str}")
    } else {
        ident.to_owned()
    }
}

fn parse_tz_offset(s: &str) -> Option<time::UtcOffset> {
    if s.len() != 5 {
        return None;
    }
    let sign: i8 = match s.as_bytes()[0] {
        b'+' => 1,
        b'-' => -1,
        _ => return None,
    };
    let hours: i8 = s[1..3].parse::<i8>().ok()?;
    let minutes: i8 = s[3..5].parse::<i8>().ok()?;
    time::UtcOffset::from_hms(sign * hours, sign * minutes, 0).ok()
}

/// Build the full patch basename `<file_prefix><NNNN>-<sanitized-subject>.patch`, truncating the
/// whole basename to `filename_max_length - 1` chars (Git's `FORMAT_PATCH_NAME_MAX`, default 64).
pub fn build_patch_filename(
    file_prefix: &str,
    patch_num: usize,
    subject: &str,
    max_len: Option<usize>,
    suffix: &str,
) -> String {
    let max = max_len.unwrap_or(64);
    let head = format!("{file_prefix}{patch_num:04}-");
    let sanitized = sanitize_subject(subject);
    // Cap so that head + sanitized + suffix has length <= max - 1.
    let budget = (max.saturating_sub(1)).saturating_sub(suffix.len());
    let mut name = head.clone();
    name.push_str(&sanitized);
    let truncated = truncate_on_char_boundary(&name, budget);
    let truncated = truncated.trim_end_matches('-');
    format!("{truncated}{suffix}")
}

/// Truncate `s` to at most `max` bytes, on a UTF-8 char boundary (never splits a multi-byte char).
fn truncate_on_char_boundary(s: &str, max: usize) -> &str {
    if s.len() <= max {
        return s;
    }
    let mut end = max;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    &s[..end]
}

/// True for the "title characters" Git keeps verbatim in a sanitized subject: ASCII alnum, `.`, `_`.
fn is_title_char(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'.' || b == b'_'
}

/// Sanitize a subject line for use as a filename, matching Git's `format_sanitized_subject`
/// byte-for-byte: runs of non-title bytes collapse into a single `-`, consecutive `.` collapse
/// into one, and trailing `.`/`-` are trimmed. Operates on raw bytes (non-ASCII → separators).
pub fn sanitize_subject(subject: &str) -> String {
    let bytes = subject.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(bytes.len());
    let mut space = 2i32;
    let mut i = 0usize;
    while i < bytes.len() {
        let b = bytes[i];
        if is_title_char(b) {
            if space == 1 {
                out.push(b'-');
            }
            space = 0;
            out.push(b);
            if b == b'.' {
                while i + 1 < bytes.len() && bytes[i + 1] == b'.' {
                    i += 1;
                }
            }
        } else {
            space |= 1;
        }
        i += 1;
    }
    // Trim trailing '.' and '-'.
    while matches!(out.last(), Some(b'.') | Some(b'-')) {
        out.pop();
    }
    String::from_utf8_lossy(&out).into_owned()
}

// ---------------------------------------------------------------------------
// Header encoding / folding (Git-compatible behavior, cf. pretty.c + utf8.c)
// ---------------------------------------------------------------------------

/// Length of the last line of `s` (bytes after the final `\n`).
pub fn last_line_length(s: &str) -> usize {
    match s.rfind('\n') {
        Some(i) => s.len() - (i + 1),
        None => s.len(),
    }
}

/// True if `line` needs RFC2047 encoding (non-ASCII, newline, or `=?`).
pub fn needs_rfc2047_encoding(line: &str) -> bool {
    let b = line.as_bytes();
    for i in 0..b.len() {
        let c = b[i];
        if c >= 0x80 || c == b'\n' {
            return true;
        }
        if i + 1 < b.len() && c == b'=' && b[i + 1] == b'?' {
            return true;
        }
    }
    false
}

/// True for chars Git considers RFC822 special (require quoting in a display name).
fn is_rfc822_special(c: u8) -> bool {
    matches!(
        c,
        b'(' | b')' | b'<' | b'>' | b'[' | b']' | b':' | b';' | b'@' | b',' | b'.' | b'"' | b'\\'
    )
}

pub fn needs_rfc822_quoting(s: &str) -> bool {
    s.bytes().any(is_rfc822_special)
}

pub fn add_rfc822_quoted(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        if c == '"' || c == '\\' {
            out.push('\\');
        }
        out.push(c);
    }
    out.push('"');
    out
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Rfc2047Type {
    Subject,
    Address,
}

fn is_rfc2047_special(c: u8, ty: Rfc2047Type) -> bool {
    if c >= 0x80 || !(c as char).is_ascii_graphic() && c != b' ' {
        return true;
    }
    if c == b' ' || c == b'\t' || c == b'=' || c == b'?' || c == b'_' {
        return true;
    }
    if ty != Rfc2047Type::Address {
        return false;
    }
    !(c.is_ascii_alphanumeric() || c == b'!' || c == b'*' || c == b'+' || c == b'-' || c == b'/')
}

/// Append `line` RFC2047-Q-encoded to `out`, folding at 76 columns with continuation lines.
pub fn add_rfc2047(out: &mut String, line: &str, encoding: &str, ty: Rfc2047Type) {
    if !encoding.eq_ignore_ascii_case("UTF-8") {
        if let Some(bytes) = crate::commit_encoding::encode_header_text(encoding, line) {
            add_rfc2047_bytes(out, &bytes, encoding, ty);
            return;
        }
    }

    const MAX_ENCODED_LENGTH: usize = 76;
    let mut line_len = last_line_length(out);
    out.push_str(&format!("=?{encoding}?q?"));
    line_len += encoding.len() + 5; // "=??q?"

    // Iterate by Unicode chars (multi-octet chars must not split across encoded-words).
    for ch in line.chars() {
        let mut buf = [0u8; 4];
        let bytes = ch.encode_utf8(&mut buf).as_bytes();
        let chrlen = bytes.len();
        let is_special = chrlen > 1 || is_rfc2047_special(bytes[0], ty);
        let encoded_len = if is_special { 3 * chrlen } else { 1 };

        if line_len + encoded_len + 2 > MAX_ENCODED_LENGTH {
            out.push_str(&format!("?=\n =?{encoding}?q?"));
            line_len = encoding.len() + 5 + 1; // "=??q?" plus leading SP
        }

        if is_special {
            for b in bytes {
                out.push_str(&format!("={b:02X}"));
            }
        } else {
            out.push(bytes[0] as char);
        }
        line_len += encoded_len;
    }
    out.push_str("?=");
}

fn add_rfc2047_bytes(out: &mut String, bytes: &[u8], encoding: &str, ty: Rfc2047Type) {
    const MAX_ENCODED_LENGTH: usize = 76;
    let mut line_len = last_line_length(out);
    out.push_str(&format!("=?{encoding}?q?"));
    line_len += encoding.len() + 5; // "=??q?"

    for &byte in bytes {
        let is_special = is_rfc2047_special(byte, ty);
        let encoded_len = if is_special { 3 } else { 1 };

        if line_len + encoded_len + 2 > MAX_ENCODED_LENGTH {
            out.push_str(&format!("?=\n =?{encoding}?q?"));
            line_len = encoding.len() + 5 + 1; // "=??q?" plus leading SP
        }

        if is_special {
            out.push_str(&format!("={byte:02X}"));
        } else {
            out.push(byte as char);
        }
        line_len += encoded_len;
    }
    out.push_str("?=");
}

/// Port of git's `strbuf_add_wrapped_text` for ASCII text (used for subject/From folding).
/// `indent1` negative means `-indent1` columns are already consumed on the current line.
pub fn add_wrapped_text(out: &mut String, text: &str, indent1: i32, indent2: i32, width: i32) {
    if width <= 0 {
        // strbuf_add_indented_text
        let mut indent = indent1.max(0);
        for (i, line) in split_keep_newlines(text).into_iter().enumerate() {
            let ind = if i == 0 { indent } else { indent2.max(0) };
            for _ in 0..ind {
                out.push(' ');
            }
            out.push_str(&line);
            indent = indent2.max(0);
        }
        return;
    }

    let bytes = text.as_bytes();
    // Each char treated width 1 (ASCII path). Reproduce git's loop on byte positions.
    let mut w: i32;
    let mut indent: i32;
    let mut bol: usize;
    let mut space: Option<usize>;
    let mut text_pos: usize = 0;

    bol = 0;
    w = indent1;
    indent = indent1;
    space = None;
    if indent < 0 {
        w = -indent;
        space = Some(0);
    }

    loop {
        let c = if text_pos < bytes.len() {
            bytes[text_pos]
        } else {
            0
        };
        if c == 0 || (c as char).is_ascii_whitespace() {
            if w <= width || space.is_none() {
                let start = if c == 0 && text_pos == bol {
                    return;
                } else if let Some(sp) = space {
                    sp
                } else {
                    for _ in 0..indent.max(0) {
                        out.push(' ');
                    }
                    bol
                };
                out.push_str(&text[start..text_pos]);
                if c == 0 {
                    return;
                }
                space = Some(text_pos);
                if c == b'\t' {
                    w |= 0x07;
                } else if c == b'\n' {
                    let sp = text_pos + 1;
                    space = Some(sp);
                    let next = bytes.get(sp).copied().unwrap_or(0);
                    if next == b'\n' {
                        out.push('\n');
                        // goto new_line
                        out.push('\n');
                        text_pos = bol_after_space(bytes, space);
                        bol = text_pos;
                        space = None;
                        w = indent2;
                        indent = indent2;
                        continue;
                    } else if !(next as char).is_ascii_alphanumeric() {
                        out.push('\n');
                        text_pos = bol_after_space(bytes, space);
                        bol = text_pos;
                        space = None;
                        w = indent2;
                        indent = indent2;
                        continue;
                    } else {
                        out.push(' ');
                    }
                }
                w += 1;
                text_pos += 1;
            } else {
                // new_line
                out.push('\n');
                let sp = space.unwrap_or(text_pos);
                let skip = if (bytes.get(sp).copied().unwrap_or(0) as char).is_ascii_whitespace() {
                    1
                } else {
                    0
                };
                text_pos = sp + skip;
                bol = text_pos;
                space = None;
                w = indent2;
                indent = indent2;
            }
            continue;
        }
        w += 1;
        text_pos += 1;
    }
}

fn bol_after_space(bytes: &[u8], space: Option<usize>) -> usize {
    let sp = space.unwrap_or(0);
    if (bytes.get(sp).copied().unwrap_or(0) as char).is_ascii_whitespace() {
        sp + 1
    } else {
        sp
    }
}

fn split_keep_newlines(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    for c in text.chars() {
        cur.push(c);
        if c == '\n' {
            out.push(std::mem::take(&mut cur));
        }
    }
    if !cur.is_empty() {
        out.push(cur);
    }
    out
}

/// Write the `Subject:` header (already-built subject string), encoding/folding like git.
pub fn write_subject_header(out: &mut String, subject: &str, encode: bool, charset_label: &str) {
    const MAX_LENGTH: i32 = 78;
    out.push_str("Subject: ");
    // Git keeps the bracketed subject prefix (`[PATCH N/M] `) literal and only RFC2047-encodes
    // the title that follows it. Split off a leading `[...] ` prefix so it is emitted verbatim.
    let (literal_prefix, title) = split_subject_prefix(subject);
    if encode && needs_rfc2047_encoding(title) {
        if !literal_prefix.is_empty() {
            out.push_str(literal_prefix);
        }
        add_rfc2047(out, title, charset_label, Rfc2047Type::Subject);
    } else {
        let consumed = last_line_length(out) as i32;
        add_wrapped_text(out, subject, -consumed, 1, MAX_LENGTH);
    }
    out.push('\n');
}

/// Split a subject into its literal `[...] ` prefix (kept verbatim by git) and the remaining
/// title. Returns `("", subject)` when there is no bracketed prefix.
fn split_subject_prefix(subject: &str) -> (&str, &str) {
    if !subject.starts_with('[') {
        return ("", subject);
    }
    if let Some(close) = subject.find(']') {
        // Include the closing bracket and a single following space (if present) in the prefix.
        let mut end = close + 1;
        if subject[end..].starts_with(' ') {
            end += 1;
        }
        return (&subject[..end], &subject[end..]);
    }
    ("", subject)
}

/// Write a `From:`/recipient address header `<Name> <mail>`, encoding/folding the display name.
pub fn write_addr_header(
    out: &mut String,
    what: &str,
    mailbox: &str,
    encode: bool,
    charset_label: &str,
) {
    let (name, mail) = split_mailbox(mailbox);
    let mut max_length: i32 = 78;
    out.push_str(what);
    out.push_str(": ");
    if name.is_empty() {
        // No display name: just "<mail>" (or the raw mailbox if unparsable).
        if mail.is_empty() {
            out.push_str(mailbox);
        } else {
            out.push_str(&format!("<{mail}>"));
        }
        out.push('\n');
        return;
    }
    if encode && needs_rfc2047_encoding(&name) {
        add_rfc2047(out, &name, charset_label, Rfc2047Type::Address);
        max_length = 76;
    } else if needs_rfc822_quoting(&name) {
        let quoted = add_rfc822_quoted(&name);
        let consumed = last_line_length(out) as i32;
        add_wrapped_text(out, &quoted, -consumed, 1, max_length);
    } else {
        let consumed = last_line_length(out) as i32;
        add_wrapped_text(out, &name, -consumed, 1, max_length);
    }
    if (max_length as usize) < last_line_length(out) + " <".len() + mail.len() + ">".len() {
        out.push('\n');
    }
    out.push_str(&format!(" <{mail}>\n"));
}

/// Split "Name <mail>" into (name, mail). If no brackets, name is the whole thing, mail empty.
fn split_mailbox(mailbox: &str) -> (String, String) {
    if let (Some(lt), Some(gt)) = (mailbox.rfind('<'), mailbox.rfind('>')) {
        if lt < gt {
            let name = mailbox[..lt].trim().to_string();
            let mail = mailbox[lt + 1..gt].to_string();
            return (name, mail);
        }
    }
    (mailbox.trim().to_string(), String::new())
}

/// Write In-Reply-To / References / Message-ID threading headers.
pub fn write_thread_headers(
    out: &mut String,
    message_id: &str,
    in_reply_to: Option<&str>,
    references: &[String],
) {
    if !message_id.is_empty() {
        out.push_str(&format!("Message-ID: <{message_id}>\n"));
    }
    if let Some(irt) = in_reply_to {
        out.push_str(&format!("In-Reply-To: <{}>\n", strip_angles(irt)));
    }
    if !references.is_empty() {
        out.push_str("References: ");
        for (i, r) in references.iter().enumerate() {
            if i > 0 {
                out.push_str("\n\t");
            }
            out.push_str(&format!("<{}>", strip_angles(r)));
        }
        out.push('\n');
    }
}

pub fn strip_angles(s: &str) -> &str {
    s.trim().trim_start_matches('<').trim_end_matches('>')
}

/// Write the trailing signature block `-- \n<sig>\n\n`, or nothing when suppressed.
pub fn write_signature(out: &mut String, signature: Option<&str>) {
    if let Some(sig) = signature {
        out.push_str("-- \n");
        out.push_str(sig);
        out.push('\n');
        out.push('\n');
    }
}

// ---------------------------------------------------------------------------
// Subject / prefix / reroll / threading helpers
// ---------------------------------------------------------------------------

/// The first physical line of the subject (used for the patch filename, matching git which stops
/// `format_sanitized_subject` at the first newline). Returns the whole trimmed message if single-line.
pub fn first_subject_line(message: &str) -> &str {
    let start = message.len() - message.trim_start().len();
    let rest = &message[start..];
    match rest.find('\n') {
        Some(nl) => rest[..nl].trim_end(),
        None => rest.trim_end(),
    }
}

/// Flatten a multi-line commit message into a single-line subject (paragraph join with spaces).
pub fn flatten_subject(message: &str) -> String {
    let mut out = String::new();
    for line in message.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            break;
        }
        if !out.is_empty() {
            out.push(' ');
        }
        out.push_str(trimmed);
    }
    out
}

/// Build a patch Subject value: `[<prefix> n/m] <subject>` with proper handling of empty prefix.
pub fn build_patch_subject(
    prefix: &str,
    keep_subject: bool,
    use_numbering: bool,
    patch_num: usize,
    display_total: usize,
    subject_line: &str,
) -> String {
    if keep_subject {
        return subject_line.to_string();
    }
    let tag = if use_numbering {
        if prefix.is_empty() {
            format!("[{patch_num}/{display_total}]")
        } else {
            format!("[{prefix} {patch_num}/{display_total}]")
        }
    } else if prefix.is_empty() {
        // Git emits no bracket tag when the prefix is empty and numbering is off.
        String::new()
    } else {
        format!("[{prefix}]")
    };
    if tag.is_empty() {
        subject_line.to_string()
    } else {
        // Git always joins the tag and subject with a single space, so an empty subject yields
        // a trailing space after the tag (`Subject: [PATCH] `).
        format!("{tag} {subject_line}")
    }
}

/// Apply the `--rfc[=<str>]` modifier to a subject prefix.
/// Default `RFC` prepends "RFC "; a value starting with `-` appends `(...)`; else replaces leader.
pub fn apply_rfc_prefix(prefix: &str, rfc: &str) -> String {
    if let Some(rest) = rfc.strip_prefix('-') {
        // Append form: `--rfc=-(WIP)` → "PATCH (WIP)".
        if prefix.is_empty() {
            rest.trim_start_matches('-').to_string()
        } else {
            format!("{prefix} {}", rest.trim_start())
        }
    } else if prefix.is_empty() {
        rfc.to_string()
    } else {
        format!("{rfc} {prefix}")
    }
}

pub fn commit_author_timestamp(commit: &CommitData) -> i64 {
    let parts: Vec<&str> = commit.author.rsplitn(3, ' ').collect();
    parts
        .get(1)
        .and_then(|s| s.parse::<i64>().ok())
        .unwrap_or(0)
}

/// Validate a `--from=<ident>` value: must look like an email ident (contain `@`).
pub fn is_valid_from_ident(ident: &str) -> bool {
    ident.contains('@')
}

/// Ensure a directory prefix ends with `/`.
pub fn ensure_trailing_slash(s: &str) -> String {
    if s.ends_with('/') {
        s.to_string()
    } else {
        format!("{s}/")
    }
}

pub fn path_matches_spec(path: &str, spec: &str) -> bool {
    path == spec || path.starts_with(&format!("{spec}/"))
}

/// Sanitize a reroll-count string for use in a filename prefix (`v<x>-`), like git's sanitizer.
pub fn sanitize_reroll(v: &str) -> String {
    sanitize_subject(v)
}

/// Map a `format.notes` / `--notes=` value to a full notes ref (`refs/notes/<x>` unless already a
/// full `refs/...` ref). An empty value or `true` means the default `refs/notes/commits`.
pub fn notes_value_to_ref(val: &str) -> String {
    let v = val.trim();
    if v.is_empty() || v == "true" {
        "refs/notes/commits".to_string()
    } else if v.starts_with("refs/") {
        v.to_string()
    } else {
        format!("refs/notes/{v}")
    }
}

/// `Interdiff against v<N-1>:` label, or `None` if reroll is not an integer >= 2.
pub fn prev_version_label(reroll: &str) -> Option<String> {
    let n: u32 = reroll.parse().ok()?;
    if n >= 2 {
        Some(format!("v{}", n - 1))
    } else {
        None
    }
}

/// Append `body` to `out`, indenting every line by two spaces (matching `sed -e "s/^/  /"`).
pub fn push_indented(out: &mut String, body: &str) {
    for line in body.split_inclusive('\n') {
        out.push_str("  ");
        out.push_str(line);
    }
    if !body.is_empty() && !body.ends_with('\n') {
        out.push('\n');
    }
}

/// Apply mboxrd `>From ` escaping to body lines if `mboxrd` is set.
pub fn mboxrd_escape(body: &str, mboxrd: bool) -> String {
    if !mboxrd {
        return body.to_string();
    }
    let mut out = String::with_capacity(body.len());
    for line in split_keep_newlines(body) {
        let content = line.strip_suffix('\n').unwrap_or(&line);
        // Escape lines matching `>*From ` (zero or more leading '>' then `From` followed by a
        // space). A bare `From` is never an mbox delimiter, so git leaves it unescaped.
        let trimmed_gt = content.trim_start_matches('>');
        if trimmed_gt.starts_with("From ") || trimmed_gt.starts_with("From\t") {
            out.push('>');
        }
        out.push_str(&line);
    }
    out
}

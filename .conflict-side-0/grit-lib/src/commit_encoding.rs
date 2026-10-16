//! Git commit encoding labels (`encoding` header, `i18n.commitEncoding`) mapped to codecs.
//!
//! Git's `ISO-8859-1` is strict Latin-1; `encoding_rs` maps that label to Windows-1252, so we
//! handle Latin-1 separately.

use encoding_rs::Encoding;

fn is_iso_8859_1(label: &str) -> bool {
    matches!(
        label.trim().to_ascii_lowercase().as_str(),
        "iso-8859-1" | "iso8859-1" | "latin1" | "latin-1"
    )
}

fn decode_latin1(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len());
    for &b in bytes {
        s.push(char::from_u32(u32::from(b)).unwrap_or('\u{FFFD}'));
    }
    s
}

fn encode_latin1_lossy(unicode: &str) -> Vec<u8> {
    unicode
        .chars()
        .map(|c| {
            let cp = u32::from(c);
            if cp <= 0xFF {
                cp as u8
            } else {
                b'?'
            }
        })
        .collect()
}

/// Find the offset of the first byte that is not part of a strictly valid UTF-8
/// sequence, mirroring Git's `find_invalid_utf8` (commit.c).
///
/// This is stricter than [`core::str::from_utf8`]: in addition to rejecting
/// malformed/overlong sequences and surrogates, it also rejects the Unicode
/// non-characters `U+xxFFFE`, `U+xxFFFF`, and the range `U+FDD0..=U+FDEF`, which
/// Rust's standard library accepts. Returns `None` when the whole buffer is valid.
#[must_use]
pub fn find_invalid_utf8(buf: &[u8]) -> Option<usize> {
    const MAX_CODEPOINT: [u32; 4] = [0x7f, 0x7ff, 0xffff, 0x10ffff];
    let mut i = 0usize;
    while i < buf.len() {
        let c = buf[i];
        let bad_offset = i;
        i += 1;
        // Simple US-ASCII? No worries.
        if c < 0x80 {
            continue;
        }
        // Count how many more high bits are set: that's how many more bytes
        // this sequence should have.
        let mut bytes = 0usize;
        let mut cc = c;
        while cc & 0x40 != 0 {
            cc <<= 1;
            bytes += 1;
        }
        // Must be between 1 and 3 more bytes.
        if !(1..=3).contains(&bytes) {
            return Some(bad_offset);
        }
        // Do we have that many bytes?
        if buf.len() - i < bytes {
            return Some(bad_offset);
        }
        let mut codepoint = (u32::from(cc) & 0x7f) >> bytes;
        let min_val = MAX_CODEPOINT[bytes - 1] + 1;
        let max_val = MAX_CODEPOINT[bytes];
        // Verify that they are good continuation bytes.
        for _ in 0..bytes {
            let b = buf[i];
            codepoint = (codepoint << 6) | (u32::from(b) & 0x3f);
            if b & 0xc0 != 0x80 {
                return Some(bad_offset);
            }
            i += 1;
        }
        if codepoint < min_val || codepoint > max_val {
            return Some(bad_offset);
        }
        // Reject the UTF-16 surrogate block (U+D800..=U+DFFF): it has no
        // legal UTF-8 encoding.
        if codepoint & 0x1f_f800 == 0xd800 {
            return Some(bad_offset);
        }
        // The last two code points of every plane (..FFFE and ..FFFF) are
        // permanent non-characters.
        if codepoint & 0xfffe == 0xfffe {
            return Some(bad_offset);
        }
        // So is anything in the range U+FDD0..=U+FDEF.
        if (0xfdd0..=0xfdef).contains(&codepoint) {
            return Some(bad_offset);
        }
    }
    None
}

/// Whether `buf` is strictly valid UTF-8 per Git's rules (see [`find_invalid_utf8`]).
#[must_use]
pub fn is_strict_utf8(buf: &[u8]) -> bool {
    find_invalid_utf8(buf).is_none()
}

/// Git stores the commit message body with a trailing newline when non-empty.
#[must_use]
pub fn ensure_body_trailing_newline(mut bytes: Vec<u8>) -> Vec<u8> {
    if !bytes.is_empty() && !bytes.ends_with(b"\n") {
        bytes.push(b'\n');
    }
    bytes
}

/// Whether `label` names an encoding Git can decode (ISO-8859-1 or any encoding
/// resolvable via [`resolve`]). Unknown names (e.g. the test's `non-utf-8`) return
/// false, matching Git's `logmsg_reencode` no-op fallback.
pub fn is_known_encoding(label: &str) -> bool {
    is_iso_8859_1(label) || resolve(label).is_some()
}

/// Resolve an encoding label the way Git uses it in config and commit objects.
///
/// Git accepts names like `eucJP` that [`Encoding::for_label`] does not recognize.
/// ISO-8859-1 is handled separately as strict Latin-1 and returns `None`.
#[must_use]
pub fn resolve(label: &str) -> Option<&'static Encoding> {
    let t = label.trim();
    if t.is_empty() || is_iso_8859_1(t) {
        return None;
    }
    let normalized = t.replace('_', "-");
    let lower = normalized.to_ascii_lowercase();
    let mapped = match lower.as_str() {
        "eucjp" => "euc-jp",
        "cp932" | "mskanji" | "sjis" => "shift_jis",
        _ => normalized.as_str(),
    };
    Encoding::for_label(mapped.as_bytes()).or_else(|| Encoding::for_label(t.as_bytes()))
}

/// Encode `unicode` for storage in a commit message body using Git's encoding name.
#[must_use]
pub fn encode_unicode(label: &str, unicode: &str) -> Option<Vec<u8>> {
    let t = label.trim();
    let raw = if is_iso_8859_1(t) {
        encode_latin1_lossy(unicode)
    } else {
        let enc = resolve(t)?;
        let (cow, _, _) = enc.encode(unicode);
        cow.into_owned()
    };
    Some(ensure_body_trailing_newline(raw))
}

/// Encode a single header field (author/committer line) without adding a trailing newline.
#[must_use]
pub fn encode_header_text(label: &str, unicode: &str) -> Option<Vec<u8>> {
    let t = label.trim();
    if is_iso_8859_1(t) {
        return Some(encode_latin1_lossy(unicode));
    }
    let enc = resolve(t)?;
    let (cow, _, _) = enc.encode(unicode);
    Some(cow.into_owned())
}

/// Decode `bytes` using Git's encoding name, or lossy UTF-8 if unknown.
#[must_use]
pub fn decode_bytes(label: Option<&str>, bytes: &[u8]) -> String {
    if let Some(l) = label {
        if is_iso_8859_1(l) {
            return decode_latin1(bytes);
        }
        if let Some(enc) = resolve(l) {
            let (cow, _) = enc.decode_without_bom_handling(bytes);
            return cow.into_owned();
        }
    }
    String::from_utf8_lossy(bytes).into_owned()
}

/// Re-encode `unicode` from UTF-8 into `output_label`, or `None` if unsupported.
#[must_use]
pub fn reencode_utf8_to_label(output_label: &str, unicode: &str) -> Option<Vec<u8>> {
    encode_header_text(output_label, unicode)
}

/// Prepare a commit message for storage per `i18n.commitEncoding` (or equivalent).
///
/// When the configured encoding is not UTF-8, returns [`Some`] raw bytes for the body
/// and sets `encoding` in the commit object; otherwise UTF-8 is stored without an
/// `encoding` header.
#[must_use]
pub fn finalize_stored_commit_message(
    message: String,
    commit_encoding: Option<&str>,
) -> (String, Option<String>, Option<Vec<u8>>) {
    let is_utf8 = match commit_encoding {
        None => true,
        Some(e) => e.eq_ignore_ascii_case("utf-8") || e.eq_ignore_ascii_case("utf8"),
    };
    if is_utf8 {
        return (message, None, None);
    }
    let Some(label) = commit_encoding.filter(|s| !s.trim().is_empty()) else {
        return (message, None, None);
    };
    let Some(raw) = encode_unicode(label, &message) else {
        return (message, None, None);
    };
    (message, Some(label.to_owned()), Some(raw))
}

/// Decode `=?charset?q?...?=` encoded-words in an email display name (before `<`).
///
/// Used when applying patches: `git format-patch` emits RFC 2047 in `From:`; the stored
/// commit author should be the decoded Unicode form.
#[must_use]
pub fn decode_rfc2047_mailbox_from_line(from: &str) -> String {
    let from = from.trim();
    let Some(lt) = from.find('<') else {
        return decode_rfc2047_encoded_words(from);
    };
    let name = from[..lt].trim();
    let tail = &from[lt..];
    let decoded = decode_rfc2047_encoded_words(name);
    if decoded.is_empty() {
        tail.trim_start().to_string()
    } else {
        format!("{decoded} {tail}")
    }
}

fn decode_rfc2047_encoded_words(s: &str) -> String {
    let mut out = String::new();
    let mut rest = s;
    while let Some(start) = rest.find("=?") {
        out.push_str(&rest[..start]);
        rest = &rest[start + 2..];
        let Some(d1) = rest.find('?') else {
            out.push_str("=?");
            out.push_str(rest);
            return out;
        };
        let charset = &rest[..d1];
        let after_cs = &rest[d1 + 1..];
        let Some(d2) = after_cs.find('?') else {
            out.push_str("=?");
            out.push_str(rest);
            return out;
        };
        let encoding = after_cs[..d2].to_ascii_lowercase();
        let after_enc = &after_cs[d2 + 1..];
        let Some(end) = after_enc.find("?=") else {
            out.push_str("=?");
            out.push_str(rest);
            return out;
        };
        let payload = &after_enc[..end];
        rest = &after_enc[end + 2..];
        if encoding == "q" {
            let bytes = decode_quoted_printable_soft(payload);
            out.push_str(&decode_bytes(Some(charset), &bytes));
        } else if encoding == "b" {
            if let Some(bytes) = base64_decode_rfc2047(payload) {
                out.push_str(&decode_bytes(Some(charset), &bytes));
            }
        }
    }
    out.push_str(rest);
    out
}

fn decode_quoted_printable_soft(payload: &str) -> Vec<u8> {
    let mut out = Vec::new();
    let mut it = payload.as_bytes().iter().copied().peekable();
    while let Some(b) = it.next() {
        if b == b'_' {
            out.push(b' ');
        } else if b == b'=' {
            let h1 = it.next();
            let h2 = it.next();
            if let (Some(a), Some(c)) = (h1, h2) {
                if let (Some(hi), Some(lo)) = (hex_nibble(a), hex_nibble(c)) {
                    out.push((hi << 4) | lo);
                    continue;
                }
            }
            out.push(b'=');
        } else {
            out.push(b);
        }
    }
    out
}

fn hex_nibble(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

fn base64_decode_rfc2047(input: &str) -> Option<Vec<u8>> {
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
        let val = TABLE.iter().position(|&c| c == byte)? as u32;
        buf = (buf << 6) | val;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            output.push((buf >> bits) as u8);
            buf &= (1 << bits) - 1;
        }
    }
    Some(output)
}

/// Raw `author` / `committer` header payloads for a new commit object.
///
/// When `encoding` is unset or UTF-8, returns empty vectors so
/// [`crate::objects::serialize_commit`] writes the Unicode [`String`] fields as UTF-8.
/// When `encoding` is non-UTF-8, encodes the full identity lines (name, email, timestamp)
/// for storage in that charset.
#[must_use]
pub fn identity_raw_for_serialized_commit(
    encoding: &Option<String>,
    author: &str,
    committer: &str,
) -> (Vec<u8>, Vec<u8>) {
    let is_utf8 = match encoding.as_deref() {
        None => true,
        Some(e) => e.eq_ignore_ascii_case("utf-8") || e.eq_ignore_ascii_case("utf8"),
    };
    if is_utf8 {
        return (Vec::new(), Vec::new());
    }
    let Some(label) = encoding.as_deref() else {
        return (Vec::new(), Vec::new());
    };
    let author_raw = encode_header_text(label, author).unwrap_or_default();
    let committer_raw = encode_header_text(label, committer).unwrap_or_default();
    (author_raw, committer_raw)
}

/// Unicode commit message body for display (for example, `format-patch`).
///
/// Uses `raw_message` when set; otherwise returns `message`.
#[must_use]
pub fn commit_message_unicode_for_display(
    encoding: Option<&str>,
    message: &str,
    raw_message: Option<&[u8]>,
) -> String {
    if let Some(raw) = raw_message {
        decode_bytes(encoding, raw)
    } else {
        message.to_owned()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strict_utf8_accepts_plain_ascii_and_multibyte() {
        assert!(is_strict_utf8(b"Commit message\n"));
        // Valid multi-byte UTF-8 (Latin small letter a with acute, CJK).
        assert!(is_strict_utf8("Ábçdèfg はれひほふ".as_bytes()));
        // ISO-2022-JP is a 7-bit encoding using ESC control bytes; valid UTF-8.
        assert!(is_strict_utf8(b"\x1b$B$O$l$R$[$U\x1b(B"));
    }

    #[test]
    fn strict_utf8_rejects_surrogates() {
        // Encoded surrogate U+D800 (ED A0 80) — invalid in UTF-8.
        assert_eq!(find_invalid_utf8(b"abc\xed\xa0\x80"), Some(3));
        assert!(!is_strict_utf8(b"\xed\xa0\x80"));
    }

    #[test]
    fn strict_utf8_rejects_overlong_sequences() {
        // Overlong encoding of U+0029 and the C0 A0 "fake space".
        assert!(!is_strict_utf8(b"\xe0\x82\xa9"));
        assert!(!is_strict_utf8(b"\xc0\xa0"));
    }

    #[test]
    fn strict_utf8_rejects_noncharacters_rust_would_accept() {
        // U+10FFFE non-character: F4 8F BF BE.
        assert!(core::str::from_utf8(b"\xf4\x8f\xbf\xbe").is_ok());
        assert!(!is_strict_utf8(b"\xf4\x8f\xbf\xbe"));
        // U+FDD0 (in the U+FDD0..=U+FDEF non-character block): EF B7 90.
        assert!(core::str::from_utf8(b"\xef\xb7\x90").is_ok());
        assert!(!is_strict_utf8(b"\xef\xb7\x90"));
    }

    #[test]
    fn latin1_round_trips_through_encode_and_decode() {
        let unicode = "Áéí óú";
        let encoded = encode_header_text("ISO8859-1", unicode).expect("latin1 encodes");
        assert_eq!(encoded, vec![0xC1, 0xE9, 0xED, 0x20, 0xF3, 0xFA]);
        assert_eq!(decode_bytes(Some("ISO8859-1"), &encoded), unicode);
    }
}

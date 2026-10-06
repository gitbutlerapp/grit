//! Email parsing for `mailinfo` (aligned with Git's `mailinfo.c` for `t5100-mailinfo`).
//!
//! The [`mailinfo`] function reads one raw RFC822/MIME message and writes the commit message
//! body, extracted patch, and metadata summary the way Git's plumbing does.

use std::io::{self, Write};

use crate::commit_encoding::{decode_bytes, reencode_utf8_to_label};
use crate::config::ConfigSet;

/// Quoted-printable / base64 body: what to do with decoded CRLF when not format=flowed.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum QuotedCrAction {
    #[default]
    Warn,
    NoWarn,
    Strip,
}

impl QuotedCrAction {
    #[must_use]
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "warn" => Some(Self::Warn),
            "nowarn" => Some(Self::NoWarn),
            "strip" => Some(Self::Strip),
            _ => None,
        }
    }
}

/// Options controlling subject cleanup, scissors, in-body headers, charset handling, and
/// quoted-printable CRLF warnings — corresponding to the `git mailinfo` CLI flags.
#[derive(Debug, Clone)]
pub struct MailinfoOptions {
    pub keep_subject: bool,
    pub keep_non_patch_brackets_in_subject: bool,
    pub add_message_id: bool,
    pub metainfo_charset: Option<String>,
    pub use_scissors: bool,
    pub use_inbody_headers: bool,
    pub quoted_cr: QuotedCrAction,
}

impl Default for MailinfoOptions {
    fn default() -> Self {
        Self {
            keep_subject: false,
            keep_non_patch_brackets_in_subject: false,
            add_message_id: false,
            metainfo_charset: Some("utf-8".to_string()),
            use_scissors: false,
            use_inbody_headers: true,
            quoted_cr: QuotedCrAction::Warn,
        }
    }
}

/// Reads `mailinfo.scissors` and `mailinfo.quotedcr` (or `mailinfo.quotedCr`) from `cfg`.
pub fn apply_mailinfo_config(cfg: &ConfigSet, opts: &mut MailinfoOptions) {
    if let Some(v) = cfg.get("mailinfo.scissors") {
        if let Ok(b) = parse_bool_loose(v.as_str()) {
            opts.use_scissors = b;
        }
    }
    if let Some(v) = cfg
        .get("mailinfo.quotedcr")
        .or_else(|| cfg.get("mailinfo.quotedCr"))
    {
        if let Some(a) = QuotedCrAction::parse(v.trim()) {
            opts.quoted_cr = a;
        }
    }
}

fn parse_bool_loose(s: &str) -> Result<bool, ()> {
    match s.trim().to_ascii_lowercase().as_str() {
        "true" | "yes" | "on" | "1" => Ok(true),
        "false" | "no" | "off" | "0" => Ok(false),
        _ => Err(()),
    }
}

struct Input<'a> {
    bytes: &'a [u8],
    pos: usize,
}

impl<'a> Input<'a> {
    fn new(bytes: &'a [u8]) -> Self {
        Self { bytes, pos: 0 }
    }

    fn eof(&self) -> bool {
        self.pos >= self.bytes.len()
    }

    fn skip_leading_ws(&mut self) {
        while self
            .bytes
            .get(self.pos)
            .is_some_and(|b| b.is_ascii_whitespace())
        {
            self.pos += 1;
        }
    }

    fn peek(&self) -> Option<u8> {
        self.bytes.get(self.pos).copied()
    }

    /// Read one LF-terminated line (or rest of file); strip trailing CR/LF from buffer.
    fn read_line(&mut self, out: &mut Vec<u8>) -> io::Result<bool> {
        out.clear();
        if self.pos >= self.bytes.len() {
            return Ok(false);
        }
        let start = self.pos;
        while self.pos < self.bytes.len() {
            let b = self.bytes[self.pos];
            self.pos += 1;
            out.push(b);
            if b == b'\n' {
                break;
            }
        }
        trim_end_crlf(out);
        Ok(self.pos > start)
    }

    /// Like [`Self::read_line`] but keep a single trailing `\n` (only normalize CRLF → LF).
    fn read_line_keep_lf(&mut self, out: &mut Vec<u8>) -> io::Result<bool> {
        out.clear();
        if self.pos >= self.bytes.len() {
            return Ok(false);
        }
        let start = self.pos;
        while self.pos < self.bytes.len() {
            let b = self.bytes[self.pos];
            self.pos += 1;
            out.push(b);
            if b == b'\n' {
                break;
            }
        }
        if out.len() >= 2 && out[out.len() - 2] == b'\r' && out[out.len() - 1] == b'\n' {
            out.remove(out.len() - 2);
        }
        Ok(self.pos > start)
    }
}

fn trim_end_crlf(line: &mut Vec<u8>) {
    while line.last() == Some(&b'\r') || line.last() == Some(&b'\n') {
        line.pop();
    }
}

fn is_rfc2822_header(line: &[u8]) -> bool {
    if line.starts_with(b"From ") || line.starts_with(b">From ") {
        return true;
    }
    let mut i = 0;
    while i < line.len() {
        let ch = line[i];
        if ch == b':' {
            return true;
        }
        if (33..=57).contains(&ch) || (59..=126).contains(&ch) {
            i += 1;
            continue;
        }
        break;
    }
    false
}

fn read_header_line(inp: &mut Input<'_>, line: &mut Vec<u8>) -> io::Result<bool> {
    line.clear();
    if !inp.read_line(line)? {
        return Ok(false);
    }
    if line.is_empty() || !is_rfc2822_header(line) {
        line.push(b'\n');
        return Ok(false);
    }
    while matches!(inp.peek(), Some(b' ') | Some(b'\t')) {
        inp.pos += 1;
        let mut cont = Vec::new();
        if !inp.read_line(&mut cont)? {
            break;
        }
        line.push(b' ');
        line.extend_from_slice(&cont);
    }
    Ok(true)
}

#[derive(Default, Clone)]
struct OptHdr(Option<String>);

impl OptHdr {
    fn set_if_empty(&mut self, v: String) {
        if self.0.is_none() {
            self.0 = Some(v);
        }
    }
    fn clear(&mut self) {
        self.0 = None;
    }
    fn as_opt(&self) -> Option<&str> {
        self.0.as_deref()
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Default)]
enum TE {
    #[default]
    DontCare,
    Qp,
    Base64,
}

struct Mime {
    boundaries: Vec<Vec<u8>>,
    charset: String,
    te: TE,
    format_flowed: bool,
    delsp: bool,
}

impl Default for Mime {
    fn default() -> Self {
        Self {
            boundaries: Vec::new(),
            charset: String::new(),
            te: TE::DontCare,
            format_flowed: false,
            delsp: false,
        }
    }
}

/// Extract author/subject/date and split message vs patch.
///
/// # Parameters
///
/// - `input` — full message bytes (may contain NULs in bodies).
/// - `opts` — behaviour flags and optional UTF-8 metadata re-encoding target.
/// - `msg_out` — commit message body (before the patch).
/// - `patch_out` — diff text; binary-identical to Git where tests require it.
/// - `info_out` — `Author:` / `Email:` / `Subject:` / `Date:` block.
/// - `stderr` — receives `warning: quoted CRLF detected` when applicable.
///
/// # Errors
///
/// Returns an I/O error with kind [`io::ErrorKind::InvalidData`] for empty input or malformed
/// RFC2047 (matching Git's `input_error` behaviour).
pub fn mailinfo(
    input: &[u8],
    opts: &MailinfoOptions,
    mut msg_out: impl Write,
    mut patch_out: impl Write,
    mut info_out: impl Write,
    mut stderr: impl Write,
) -> io::Result<()> {
    let mut inp = Input::new(input);
    inp.skip_leading_ws();
    if inp.eof() {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "empty patch"));
    }

    let mut p_from = OptHdr::default();
    let mut p_subj = OptHdr::default();
    let mut p_date = OptHdr::default();
    let mut message_id: Option<String> = None;
    let mut mime = Mime::default();
    let mut header_err = false;

    let mut hl = Vec::new();
    while read_header_line(&mut inp, &mut hl)? {
        let ls = String::from_utf8_lossy(&hl).into_owned();
        if !parse_rfc_header(
            &ls,
            opts,
            &mut p_from,
            &mut p_subj,
            &mut p_date,
            &mut mime,
            &mut message_id,
        ) {
            header_err = true;
        }
    }
    if header_err {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "mailinfo: bad header",
        ));
    }

    let mut s_from = OptHdr::default();
    let mut s_subj = OptHdr::default();
    let mut s_date = OptHdr::default();
    let mut log = Vec::new();
    let mut patch_lines: u64 = 0;
    let mut have_quoted_cr = false;
    let mut body_err = false;

    let mut st = BodyState {
        filter_stage: 0,
        header_stage: true,
        inbody_accum: String::new(),
        qp_carry: Vec::new(),
        flowed_prev: Vec::new(),
        body_done: false,
    };

    let mut line = Vec::new();
    let mut need_first_boundary = !mime.boundaries.is_empty();
    loop {
        if need_first_boundary {
            need_first_boundary = false;
            if !find_boundary_line(&mut inp, &mime, &mut line)? {
                flush_inbody_accum(
                    &mut st.inbody_accum,
                    opts,
                    &mut s_from,
                    &mut s_subj,
                    &mut s_date,
                );
                break;
            }
        } else if !inp.read_line_keep_lf(&mut line)? {
            break;
        }
        process_body_raw_line(
            &mut inp,
            &mut line,
            opts,
            &mut mime,
            &mut st,
            &mut s_from,
            &mut s_subj,
            &mut s_date,
            &mut message_id,
            &mut log,
            &mut patch_out,
            &mut patch_lines,
            &mut have_quoted_cr,
            &mut body_err,
        )?;
        if body_err {
            break;
        }
        if st.body_done {
            break;
        }
    }

    flush_qp_tail(
        opts,
        &mime,
        &mut st,
        &mut s_from,
        &mut s_subj,
        &mut s_date,
        &mut message_id,
        &mut log,
        &mut patch_out,
        &mut patch_lines,
        &mut have_quoted_cr,
    )?;
    if !st.flowed_prev.is_empty() {
        let p = std::mem::take(&mut st.flowed_prev);
        handle_filter(
            opts,
            &p,
            &mut s_from,
            &mut s_subj,
            &mut s_date,
            &mut message_id,
            &mut log,
            &mut patch_out,
            &mut patch_lines,
            &mut st.filter_stage,
            &mut st.header_stage,
            &mut st.inbody_accum,
            &mime,
        )?;
    }
    flush_inbody_accum(
        &mut st.inbody_accum,
        opts,
        &mut s_from,
        &mut s_subj,
        &mut s_date,
    );

    if have_quoted_cr && opts.quoted_cr == QuotedCrAction::Warn {
        writeln!(stderr, "warning: quoted CRLF detected")?;
    }
    if body_err {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "mailinfo: bad body",
        ));
    }

    msg_out.write_all(&log)?;
    write_info(
        opts,
        patch_lines,
        &p_from,
        &p_subj,
        &p_date,
        &s_from,
        &s_subj,
        &s_date,
        &mut info_out,
    )?;
    Ok(())
}

struct BodyState {
    filter_stage: u8,
    header_stage: bool,
    inbody_accum: String,
    qp_carry: Vec<u8>,
    flowed_prev: Vec<u8>,
    /// Set when Git would leave `handle_body` (no more MIME body to read).
    body_done: bool,
}

fn parse_rfc_header(
    line: &str,
    opts: &MailinfoOptions,
    p_from: &mut OptHdr,
    p_subj: &mut OptHdr,
    p_date: &mut OptHdr,
    mime: &mut Mime,
    message_id: &mut Option<String>,
) -> bool {
    let Some(colon) = line.find(':') else {
        return true;
    };
    let name = line[..colon].trim();
    let val = line[colon + 1..].trim_start();

    if name.eq_ignore_ascii_case("From") {
        let mut v = val.to_string();
        if decode_rfc2047(opts, &mut v).is_err() {
            return false;
        }
        p_from.set_if_empty(v);
        return true;
    }
    if name.eq_ignore_ascii_case("Subject") {
        let mut v = val.to_string();
        if decode_rfc2047(opts, &mut v).is_err() {
            return false;
        }
        p_subj.set_if_empty(v);
        return true;
    }
    if name.eq_ignore_ascii_case("Date") {
        let mut v = val.to_string();
        if decode_rfc2047(opts, &mut v).is_err() {
            return false;
        }
        p_date.set_if_empty(v);
        return true;
    }
    if name.eq_ignore_ascii_case("Content-Type") {
        mime.format_flowed = has_attr_ci(line, "format=", "flowed");
        mime.delsp = has_attr_ci(line, "delsp=", "yes");
        if let Some(b) = slurp_attr_ci(line, "boundary=") {
            let mut full = vec![b'-', b'-'];
            full.extend_from_slice(b.as_bytes());
            if mime.boundaries.len() < 5 {
                mime.boundaries.push(full);
            }
        }
        if let Some(cs) = slurp_attr_ci(line, "charset=") {
            mime.charset = cs;
        }
        return true;
    }
    if name.eq_ignore_ascii_case("Content-Transfer-Encoding") {
        let lower = val.to_ascii_lowercase();
        mime.te = if lower.contains("base64") {
            TE::Base64
        } else if lower.contains("quoted-printable") {
            TE::Qp
        } else {
            TE::DontCare
        };
        return true;
    }
    if opts.add_message_id
        && (name.eq_ignore_ascii_case("Message-ID") || name.eq_ignore_ascii_case("Message-Id"))
    {
        let mut v = val.to_string();
        if decode_rfc2047(opts, &mut v).is_err() {
            return false;
        }
        *message_id = Some(v);
    }
    true
}

fn slurp_attr_ci(line: &str, name: &str) -> Option<String> {
    let lower = line.to_ascii_lowercase();
    let needle = name.to_ascii_lowercase();
    let pos = lower.find(&needle)?;
    let mut ap = &line[pos + needle.len()..];
    if ap.starts_with('"') {
        ap = &ap[1..];
        let end = ap.find('"')?;
        Some(ap[..end].to_string())
    } else {
        let end = ap.find([';', ' ', '\t']).unwrap_or(ap.len());
        let s = ap[..end].trim();
        (!s.is_empty()).then(|| s.to_string())
    }
}

fn has_attr_ci(line: &str, name: &str, value: &str) -> bool {
    slurp_attr_ci(line, name).is_some_and(|s| s.eq_ignore_ascii_case(value))
}

fn decode_rfc2047(opts: &MailinfoOptions, s: &mut String) -> Result<(), ()> {
    let Some(out) = decode_rfc2047_inner(s, opts.metainfo_charset.as_deref()) else {
        return Err(());
    };
    *s = out;
    Ok(())
}

fn decode_rfc2047_inner(input: &str, metainfo: Option<&str>) -> Option<String> {
    let mut out = String::new();
    let mut pos = 0usize;
    while let Some(rel) = input[pos..].find("=?") {
        let ep = pos + rel;
        let before = &input[pos..ep];
        if !before.is_empty() {
            let only_ws = before.chars().all(|c| c.is_whitespace());
            if !only_ws || pos == 0 {
                out.push_str(before);
            }
        }
        let rest = &input[ep + 2..];
        let d1 = rest.find('?')?;
        let charset = &rest[..d1];
        let after = &rest[d1 + 1..];
        let d2 = after.find('?')?;
        let enc = after[..d2].to_ascii_lowercase();
        let after_enc = &after[d2 + 1..];
        let end = after_enc.find("?=")?;
        let payload = &after_enc[..end];
        pos = ep + 2 + d1 + 1 + d2 + 1 + end + 2;
        let bytes = match enc.as_str() {
            "q" => decode_qp(payload, true),
            "b" => base64_decode(payload),
            _ => return None,
        };
        let mut piece = decode_bytes(Some(charset), &bytes);
        if let Some(target) = metainfo {
            if let Some(raw) = reencode_utf8_to_label(target, &piece) {
                piece = decode_bytes(Some(target), &raw);
            }
        }
        out.push_str(&piece);
    }
    out.push_str(&input[pos..]);
    Some(out)
}

fn decode_qp(input: &str, rfc2047: bool) -> Vec<u8> {
    let mut out = Vec::new();
    let bytes = input.as_bytes();
    let mut i = 0usize;
    while i < bytes.len() {
        let b = bytes[i];
        if rfc2047 && b == b'_' {
            out.push(b' ');
            i += 1;
            continue;
        }
        if b == b'=' {
            let d1 = bytes.get(i + 1).copied();
            if d1 == Some(b'\n') {
                i += 2;
                continue;
            }
            if d1 == Some(b'\r') && bytes.get(i + 2) == Some(&b'\n') {
                i += 3;
                continue;
            }
            if d1.is_none() || d1 == Some(b'\n') {
                break;
            }
            let h1 = d1;
            let h2 = bytes.get(i + 2).copied();
            if let (Some(a), Some(c)) = (h1, h2) {
                if let (Some(hi), Some(lo)) = (hex_nibble(a), hex_nibble(c)) {
                    out.push((hi << 4) | lo);
                    i += 3;
                    continue;
                }
            }
            out.push(b'=');
            i += 1;
            continue;
        }
        out.push(b);
        i += 1;
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

fn base64_decode(input: &str) -> Vec<u8> {
    const T: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut o = Vec::new();
    let mut buf: u32 = 0;
    let mut bits: u32 = 0;
    for &byte in input.as_bytes() {
        if byte == b'=' {
            break;
        }
        if byte.is_ascii_whitespace() {
            continue;
        }
        let Some(v) = T.iter().position(|&c| c == byte) else {
            continue;
        };
        buf = (buf << 6) | v as u32;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            o.push((buf >> bits) as u8);
            buf &= (1 << bits) - 1;
        }
    }
    o
}

#[allow(clippy::too_many_arguments)]
fn process_body_raw_line(
    inp: &mut Input<'_>,
    raw: &mut Vec<u8>,
    opts: &MailinfoOptions,
    mime: &mut Mime,
    st: &mut BodyState,
    s_from: &mut OptHdr,
    s_subj: &mut OptHdr,
    s_date: &mut OptHdr,
    message_id: &mut Option<String>,
    log: &mut Vec<u8>,
    patch_out: &mut impl Write,
    patch_lines: &mut u64,
    have_quoted_cr: &mut bool,
    body_err: &mut bool,
) -> io::Result<()> {
    if let Some(boundary) = mime.boundaries.last().cloned() {
        if raw.len() >= boundary.len() && raw[..boundary.len()] == boundary[..] {
            let after = &raw[boundary.len()..];
            let is_closing = after.starts_with(b"--");
            if is_closing {
                mime.boundaries.pop();
                if mime.boundaries.is_empty() {
                    handle_filter(
                        opts,
                        b"\n",
                        s_from,
                        s_subj,
                        s_date,
                        message_id,
                        log,
                        patch_out,
                        patch_lines,
                        &mut st.filter_stage,
                        &mut st.header_stage,
                        &mut st.inbody_accum,
                        mime,
                    )?;
                }
                loop {
                    if !find_boundary_line(inp, mime, raw)? {
                        flush_inbody_accum(&mut st.inbody_accum, opts, s_from, s_subj, s_date);
                        st.body_done = true;
                        return Ok(());
                    }
                    let Some(b) = mime.boundaries.last() else {
                        break;
                    };
                    if raw.len() >= b.len() + 2
                        && raw[..b.len()] == b[..]
                        && &raw[b.len()..b.len() + 2] == b"--"
                    {
                        mime.boundaries.pop();
                        if mime.boundaries.is_empty() {
                            handle_filter(
                                opts,
                                b"\n",
                                s_from,
                                s_subj,
                                s_date,
                                message_id,
                                log,
                                patch_out,
                                patch_lines,
                                &mut st.filter_stage,
                                &mut st.header_stage,
                                &mut st.inbody_accum,
                                mime,
                            )?;
                        }
                        continue;
                    }
                    break;
                }
            } else {
                *have_quoted_cr = false;
            }
            mime.te = TE::DontCare;
            mime.charset.clear();
            mime.format_flowed = false;
            mime.delsp = false;
            read_part_headers(inp, mime, raw, opts, body_err)?;
            if *body_err {
                return Ok(());
            }
            if !inp.read_line_keep_lf(raw)? {
                flush_inbody_accum(&mut st.inbody_accum, opts, s_from, s_subj, s_date);
                st.body_done = true;
                return Ok(());
            }
        }
    }

    if st.body_done {
        return Ok(());
    }

    let mut decoded = raw.clone();
    match mime.te {
        TE::Qp => {
            let s = String::from_utf8_lossy(&decoded).into_owned();
            decoded = decode_qp(&s, false);
        }
        TE::Base64 => {
            let s: String = String::from_utf8_lossy(&decoded)
                .chars()
                .filter(|c| !c.is_ascii_whitespace())
                .collect();
            decoded = base64_decode(&s);
        }
        TE::DontCare => {}
    }

    match mime.te {
        TE::Qp | TE::Base64 => {
            st.qp_carry.extend_from_slice(&decoded);
            split_qp_carry(
                opts,
                mime,
                st,
                s_from,
                s_subj,
                s_date,
                message_id,
                log,
                patch_out,
                patch_lines,
                have_quoted_cr,
            )?;
        }
        TE::DontCare => {
            if !decoded.ends_with(b"\n") && !decoded.is_empty() {
                decoded.push(b'\n');
            }
            process_decoded_physical_line(
                opts,
                mime,
                st,
                &decoded,
                s_from,
                s_subj,
                s_date,
                message_id,
                log,
                patch_out,
                patch_lines,
                have_quoted_cr,
            )?;
        }
    }
    Ok(())
}

fn find_boundary_line(inp: &mut Input<'_>, mime: &Mime, line: &mut Vec<u8>) -> io::Result<bool> {
    let Some(b) = mime.boundaries.last() else {
        return Ok(false);
    };
    loop {
        line.clear();
        if !inp.read_line_keep_lf(line)? {
            return Ok(false);
        }
        if line.len() >= b.len() && line[..b.len()] == b[..] {
            return Ok(true);
        }
    }
}

fn read_part_headers(
    inp: &mut Input<'_>,
    mime: &mut Mime,
    line: &mut Vec<u8>,
    opts: &MailinfoOptions,
    body_err: &mut bool,
) -> io::Result<()> {
    while read_header_line(inp, line)? {
        let ls = String::from_utf8_lossy(line).into_owned();
        let mut d1 = OptHdr::default();
        let mut d2 = OptHdr::default();
        let mut d3 = OptHdr::default();
        if !parse_rfc_header(&ls, opts, &mut d1, &mut d2, &mut d3, mime, &mut None) {
            *body_err = true;
            return Ok(());
        }
    }
    Ok(())
}
#[expect(clippy::too_many_arguments)]
fn split_qp_carry(
    opts: &MailinfoOptions,
    mime: &Mime,
    st: &mut BodyState,
    s_from: &mut OptHdr,
    s_subj: &mut OptHdr,
    s_date: &mut OptHdr,
    message_id: &mut Option<String>,
    log: &mut Vec<u8>,
    patch_out: &mut impl Write,
    patch_lines: &mut u64,
    have_quoted_cr: &mut bool,
) -> io::Result<()> {
    let mut start = 0;
    let mut i = 0;
    while i < st.qp_carry.len() {
        if st.qp_carry[i] == b'\n' {
            let chunk = st.qp_carry[start..=i].to_vec();
            start = i + 1;
            process_decoded_physical_line(
                opts,
                mime,
                st,
                &chunk,
                s_from,
                s_subj,
                s_date,
                message_id,
                log,
                patch_out,
                patch_lines,
                have_quoted_cr,
            )?;
        }
        i += 1;
    }
    st.qp_carry.drain(..start);
    Ok(())
}
#[expect(clippy::too_many_arguments)]
fn flush_qp_tail(
    opts: &MailinfoOptions,
    mime: &Mime,
    st: &mut BodyState,
    s_from: &mut OptHdr,
    s_subj: &mut OptHdr,
    s_date: &mut OptHdr,
    message_id: &mut Option<String>,
    log: &mut Vec<u8>,
    patch_out: &mut impl Write,
    patch_lines: &mut u64,
    have_quoted_cr: &mut bool,
) -> io::Result<()> {
    if st.qp_carry.is_empty() {
        return Ok(());
    }
    let mut chunk = std::mem::take(&mut st.qp_carry);
    if !chunk.ends_with(b"\n") {
        chunk.push(b'\n');
    }
    process_decoded_physical_line(
        opts,
        mime,
        st,
        &chunk,
        s_from,
        s_subj,
        s_date,
        message_id,
        log,
        patch_out,
        patch_lines,
        have_quoted_cr,
    )
}
#[expect(clippy::too_many_arguments)]
fn process_decoded_physical_line(
    opts: &MailinfoOptions,
    mime: &Mime,
    st: &mut BodyState,
    line: &[u8],
    s_from: &mut OptHdr,
    s_subj: &mut OptHdr,
    s_date: &mut OptHdr,
    message_id: &mut Option<String>,
    log: &mut Vec<u8>,
    patch_out: &mut impl Write,
    patch_lines: &mut u64,
    have_quoted_cr: &mut bool,
) -> io::Result<()> {
    if mime.format_flowed {
        handle_filter_flowed(
            opts,
            mime,
            line,
            st,
            s_from,
            s_subj,
            s_date,
            message_id,
            log,
            patch_out,
            patch_lines,
            have_quoted_cr,
        )
    } else {
        let mut l = line.to_vec();
        if l.len() >= 2 && l[l.len() - 2] == b'\r' && l[l.len() - 1] == b'\n' {
            *have_quoted_cr = true;
            if opts.quoted_cr == QuotedCrAction::Strip {
                l.truncate(l.len() - 2);
                l.push(b'\n');
            }
        }
        handle_filter(
            opts,
            &l,
            s_from,
            s_subj,
            s_date,
            message_id,
            log,
            patch_out,
            patch_lines,
            &mut st.filter_stage,
            &mut st.header_stage,
            &mut st.inbody_accum,
            mime,
        )
    }
}

fn bytes_to_log_text(line: &[u8], mime: &Mime) -> String {
    let cs = (!mime.charset.trim().is_empty()).then_some(mime.charset.as_str());
    decode_bytes(cs, line)
}
#[expect(clippy::too_many_arguments)]
fn handle_filter_flowed(
    opts: &MailinfoOptions,
    mime: &Mime,
    line: &[u8],
    st: &mut BodyState,
    s_from: &mut OptHdr,
    s_subj: &mut OptHdr,
    s_date: &mut OptHdr,
    message_id: &mut Option<String>,
    log: &mut Vec<u8>,
    patch_out: &mut impl Write,
    patch_lines: &mut u64,
    have_quoted_cr: &mut bool,
) -> io::Result<()> {
    let mut len = line.len();
    if len > 0 && line[len - 1] == b'\n' {
        len -= 1;
        if len > 0 && line[len - 1] == b'\r' {
            len -= 1;
        }
    }

    if line.len() >= 3 && &line[..3] == b"-- " && len == 3 {
        if !st.flowed_prev.is_empty() {
            let p = std::mem::take(&mut st.flowed_prev);
            handle_filter(
                opts,
                &p,
                s_from,
                s_subj,
                s_date,
                message_id,
                log,
                patch_out,
                patch_lines,
                &mut st.filter_stage,
                &mut st.header_stage,
                &mut st.inbody_accum,
                mime,
            )?;
        }
        return handle_filter(
            opts,
            line,
            s_from,
            s_subj,
            s_date,
            message_id,
            log,
            patch_out,
            patch_lines,
            &mut st.filter_stage,
            &mut st.header_stage,
            &mut st.inbody_accum,
            mime,
        );
    }

    if len > 0 && line[0] == b' ' {
        let mut l = line[1..].to_vec();
        if !l.ends_with(b"\n") {
            l.push(b'\n');
        }
        return process_decoded_physical_line(
            opts,
            &Mime {
                format_flowed: false,
                charset: mime.charset.clone(),
                ..Default::default()
            },
            st,
            &l,
            s_from,
            s_subj,
            s_date,
            message_id,
            log,
            patch_out,
            patch_lines,
            have_quoted_cr,
        );
    }

    if len > 0 && line[len - 1] == b' ' {
        let take = len - usize::from(mime.delsp);
        st.flowed_prev.extend_from_slice(&line[..take]);
        return Ok(());
    }

    let mut combined = Vec::new();
    combined.extend_from_slice(&st.flowed_prev);
    combined.extend_from_slice(&line[..len]);
    st.flowed_prev.clear();
    if !combined.ends_with(b"\n") {
        combined.push(b'\n');
    }
    process_decoded_physical_line(
        opts,
        &Mime {
            format_flowed: false,
            charset: mime.charset.clone(),
            ..Default::default()
        },
        st,
        &combined,
        s_from,
        s_subj,
        s_date,
        message_id,
        log,
        patch_out,
        patch_lines,
        have_quoted_cr,
    )
}
#[expect(clippy::too_many_arguments)]
fn handle_filter(
    opts: &MailinfoOptions,
    line: &[u8],
    s_from: &mut OptHdr,
    s_subj: &mut OptHdr,
    s_date: &mut OptHdr,
    message_id: &mut Option<String>,
    log: &mut Vec<u8>,
    patch_out: &mut impl Write,
    patch_lines: &mut u64,
    filter_stage: &mut u8,
    header_stage: &mut bool,
    inbody_accum: &mut String,
    mime: &Mime,
) -> io::Result<()> {
    match *filter_stage {
        0 => {
            if !handle_commit_msg(
                opts,
                line,
                mime,
                s_from,
                s_subj,
                s_date,
                message_id,
                log,
                header_stage,
                inbody_accum,
            )? {
                return Ok(());
            }
            *filter_stage = 1;
            patch_out.write_all(line)?;
            *patch_lines += 1;
            Ok(())
        }
        1 => {
            patch_out.write_all(line)?;
            *patch_lines += 1;
            Ok(())
        }
        _ => Ok(()),
    }
}
#[expect(clippy::too_many_arguments)]
fn handle_commit_msg(
    opts: &MailinfoOptions,
    line: &[u8],
    mime: &Mime,
    s_from: &mut OptHdr,
    s_subj: &mut OptHdr,
    s_date: &mut OptHdr,
    message_id: &mut Option<String>,
    log: &mut Vec<u8>,
    header_stage: &mut bool,
    inbody_accum: &mut String,
) -> io::Result<bool> {
    let text = bytes_to_log_text(line, mime);

    if *header_stage {
        let only_ws = text.chars().all(|c| c.is_whitespace());
        if only_ws {
            if !inbody_accum.is_empty() {
                flush_inbody_accum(inbody_accum, opts, s_from, s_subj, s_date);
                *header_stage = false;
            }
            return Ok(false);
        }
    }

    if opts.use_inbody_headers && *header_stage {
        *header_stage = check_inbody(opts, &text, inbody_accum, s_from, s_subj, s_date);
        if *header_stage {
            return Ok(false);
        }
    } else {
        *header_stage = false;
    }

    if opts.use_scissors && is_scissors_line(&text) {
        log.clear();
        *header_stage = true;
        s_from.clear();
        s_subj.clear();
        s_date.clear();
        return Ok(false);
    }

    if patchbreak(line) {
        if let Some(mid) = message_id.clone() {
            log.extend_from_slice(format!("Message-ID: {mid}\n").as_bytes());
        }
        return Ok(true);
    }

    log.extend_from_slice(text.as_bytes());
    Ok(false)
}

fn flush_inbody_accum(
    acc: &mut String,
    opts: &MailinfoOptions,
    s_from: &mut OptHdr,
    s_subj: &mut OptHdr,
    s_date: &mut OptHdr,
) {
    if acc.is_empty() {
        return;
    }
    let line = std::mem::take(acc);
    apply_inbody_line(&line, opts, s_from, s_subj, s_date);
}

fn apply_inbody_line(
    line: &str,
    opts: &MailinfoOptions,
    s_from: &mut OptHdr,
    s_subj: &mut OptHdr,
    s_date: &mut OptHdr,
) {
    let Some(colon) = line.find(':') else {
        return;
    };
    let name = line[..colon].trim();
    let mut val = line[colon + 1..].trim_start().to_string();
    let _ = decode_rfc2047(opts, &mut val);
    if name.eq_ignore_ascii_case("From") && s_from.as_opt().is_none() {
        s_from.set_if_empty(val);
    } else if name.eq_ignore_ascii_case("Subject") && s_subj.as_opt().is_none() {
        s_subj.set_if_empty(val);
    } else if name.eq_ignore_ascii_case("Date") && s_date.as_opt().is_none() {
        s_date.set_if_empty(val);
    }
}

fn inbody_header_candidate(line: &str, s_from: &OptHdr, s_subj: &OptHdr, s_date: &OptHdr) -> bool {
    let Some(colon) = line.find(':') else {
        return false;
    };
    let name = line[..colon].trim();
    (name.eq_ignore_ascii_case("From") && s_from.as_opt().is_none())
        || (name.eq_ignore_ascii_case("Subject") && s_subj.as_opt().is_none())
        || (name.eq_ignore_ascii_case("Date") && s_date.as_opt().is_none())
}

fn check_inbody(
    opts: &MailinfoOptions,
    line: &str,
    accum: &mut String,
    s_from: &mut OptHdr,
    s_subj: &mut OptHdr,
    s_date: &mut OptHdr,
) -> bool {
    if !accum.is_empty() && (line.starts_with(' ') || line.starts_with('\t')) {
        if opts.use_scissors && is_scissors_line(line) {
            flush_inbody_accum(accum, opts, s_from, s_subj, s_date);
            return false;
        }
        while accum.ends_with('\n') {
            accum.pop();
        }
        accum.push_str(line);
        return true;
    }
    flush_inbody_accum(accum, opts, s_from, s_subj, s_date);

    if line.starts_with(">From ") {
        let rest = &line[1..];
        if is_format_patch_sep(rest) {
            return true;
        }
    }
    if line.starts_with("[PATCH] ") {
        s_subj.set_if_empty(line.to_string());
        return true;
    }
    if inbody_header_candidate(line, s_from, s_subj, s_date) {
        accum.push_str(line);
        return true;
    }
    false
}

fn is_format_patch_sep(line: &str) -> bool {
    const TAIL: &str = " Mon Sep 17 00:00:00 2001\n";
    if !line.starts_with("From ") || line.len() != 5 + 40 + TAIL.len() {
        return false;
    }
    let hex = &line[5..45];
    hex.chars().all(|c| c.is_ascii_hexdigit()) && &line[45..] == TAIL
}

fn patchbreak(line: &[u8]) -> bool {
    if line.starts_with(b"diff -") || line.starts_with(b"Index: ") {
        return true;
    }
    if line.len() < 4 || !line.starts_with(b"---") {
        return false;
    }
    if line.len() > 3 && line[3] == b' ' {
        return line.len() > 4 && !line[4].is_ascii_whitespace();
    }
    for &b in &line[3..] {
        match b {
            b'\n' => return true,
            b if !b.is_ascii_whitespace() => break,
            _ => {}
        }
    }
    false
}

fn is_scissors_line(line: &str) -> bool {
    let mut scissors = 0;
    let mut gap = 0;
    let mut first_nb = None;
    let mut last_nb = None;
    let mut perforation: usize = 0;
    let mut in_perf = false;
    let c: Vec<char> = line.chars().collect();
    let mut i = 0;
    while i < c.len() {
        let ch = c[i];
        if ch.is_whitespace() {
            if in_perf {
                perforation += 1;
                gap += 1;
            }
            i += 1;
            continue;
        }
        last_nb = Some(i);
        if first_nb.is_none() {
            first_nb = Some(i);
        }
        if ch == '-' {
            in_perf = true;
            perforation += 1;
            i += 1;
            continue;
        }
        let rest: String = c[i..].iter().collect();
        if rest.starts_with(">8")
            || rest.starts_with("8<")
            || rest.starts_with(">%")
            || rest.starts_with("%<")
        {
            in_perf = true;
            perforation += 2;
            scissors += 2;
            i += 2;
            continue;
        }
        in_perf = false;
        i += 1;
    }
    let visible = match (first_nb, last_nb) {
        (Some(a), Some(b)) => b - a + 1,
        _ => 0,
    };
    scissors > 0 && visible >= 8 && visible < perforation.saturating_mul(3) && gap * 2 < perforation
}
#[expect(clippy::too_many_arguments)]
fn write_info(
    opts: &MailinfoOptions,
    patch_lines: u64,
    p_from: &OptHdr,
    p_subj: &OptHdr,
    p_date: &OptHdr,
    s_from: &OptHdr,
    s_subj: &OptHdr,
    s_date: &OptHdr,
    out: &mut impl Write,
) -> io::Result<()> {
    for (hdr, val) in [
        (
            "From",
            if patch_lines > 0 && s_from.as_opt().is_some() {
                s_from.as_opt()
            } else {
                p_from.as_opt()
            },
        ),
        (
            "Subject",
            if patch_lines > 0 && s_subj.as_opt().is_some() {
                s_subj.as_opt()
            } else {
                p_subj.as_opt()
            },
        ),
        (
            "Date",
            if patch_lines > 0 && s_date.as_opt().is_some() {
                s_date.as_opt()
            } else {
                p_date.as_opt()
            },
        ),
    ] {
        let Some(val) = val else { continue };
        if val.as_bytes().contains(&0) {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "NUL in header"));
        }
        match hdr {
            "Subject" => {
                let mut subj = val.to_string();
                if !opts.keep_subject {
                    cleanup_subject(opts, &mut subj);
                    cleanup_space(&mut subj);
                }
                for part in subj.split('\n') {
                    writeln!(out, "Subject: {part}")?;
                }
            }
            "From" => {
                let mut f = val.to_string();
                cleanup_space(&mut f);
                let (name, email) = handle_from(&f);
                writeln!(out, "Author: {name}")?;
                writeln!(out, "Email: {email}")?;
            }
            _ => {
                let mut d = val.to_string();
                cleanup_space(&mut d);
                writeln!(out, "{hdr}: {d}")?;
            }
        }
    }
    writeln!(out)?;
    Ok(())
}

fn cleanup_space(s: &mut String) {
    let chs: Vec<char> = s.chars().collect();
    let mut out = String::new();
    let mut i = 0;
    while i < chs.len() {
        if chs[i].is_whitespace() {
            out.push(' ');
            i += 1;
            while i < chs.len() && chs[i].is_whitespace() {
                i += 1;
            }
        } else {
            out.push(chs[i]);
            i += 1;
        }
    }
    *s = out;
}

fn cleanup_subject(opts: &MailinfoOptions, subject: &mut String) {
    let mut at = 0;
    while at < subject.len() {
        let rest = &subject[at..];
        let Some(ch0) = rest.chars().next() else {
            break;
        };
        match ch0 {
            'r' | 'R' => {
                let b = rest.as_bytes();
                if rest.len() >= 3 && (b[1] == b'e' || b[1] == b'E') && b[2] == b':' {
                    subject.drain(at..at + 3);
                    continue;
                }
                at += ch0.len_utf8();
            }
            ' ' | '\t' | ':' => {
                subject.remove(at);
                continue;
            }
            '[' => {
                let Some(end_rel) = rest.find(']') else {
                    break;
                };
                let remove = end_rel + 1;
                let bracket = &rest[..remove];
                let strip = !opts.keep_non_patch_brackets_in_subject
                    || (remove >= 7 && bracket.to_ascii_uppercase().contains("PATCH"));
                if strip {
                    subject.drain(at..at + remove);
                } else {
                    at += remove;
                    if at < subject.len() && subject[at..].starts_with(|c: char| c.is_whitespace())
                    {
                        at += 1;
                    }
                }
                continue;
            }
            _ => break,
        }
    }
    *subject = subject.trim().to_string();
}

fn handle_from(from: &str) -> (String, String) {
    let mut f = unquote_quoted_pair(from);
    let Some(mut at) = f.find('@') else {
        return parse_bogus_from(from);
    };
    if f[at + 1..].contains('@') {
        return (String::new(), String::new());
    }
    let mut bytes = std::mem::take(&mut f).into_bytes();
    while at > 0 {
        let prev = bytes[at - 1];
        if prev.is_ascii_whitespace() {
            break;
        }
        if prev == b'<' {
            bytes[at - 1] = b' ';
            break;
        }
        at -= 1;
    }
    let el = bytes[at..]
        .iter()
        .take_while(|&&b| !b.is_ascii_whitespace() && b != b'>')
        .count();
    let email = String::from_utf8_lossy(&bytes[at..at + el]).into_owned();
    let skip = bytes
        .get(at + el)
        .filter(|&&b| b == b'>')
        .map(|_| 1)
        .unwrap_or(0);
    let remove = el + skip;
    let mut name = String::new();
    name.push_str(&String::from_utf8_lossy(&bytes[..at]));
    name.push_str(&String::from_utf8_lossy(&bytes[at + remove..]));
    cleanup_space(&mut name);
    let mut name = name.trim().to_string();
    if name.starts_with('(') && name.len() > 1 && name.ends_with(')') {
        name = name[1..name.len() - 1].to_string();
    }
    let mut disp = name.clone();
    if disp.is_empty()
        || disp.len() > 60
        || disp.contains('@')
        || disp.contains('<')
        || disp.contains('>')
    {
        disp.clone_from(&email);
    }
    (disp, email)
}

fn parse_bogus_from(line: &str) -> (String, String) {
    let Some(bra) = line.find('<') else {
        return (String::new(), String::new());
    };
    let Some(k) = line[bra + 1..].find('>') else {
        return (String::new(), String::new());
    };
    let ket = bra + 1 + k;
    let email = line[bra + 1..ket].to_string();
    let mut name = line[..bra].trim().trim_matches('"').to_string();
    if name.is_empty()
        || name.len() > 60
        || name.contains('@')
        || name.contains('<')
        || name.contains('>')
    {
        name.clone_from(&email);
    }
    (name, email)
}

fn unquote_quoted_pair(input: &str) -> String {
    let mut out = String::new();
    let mut it = input.chars().peekable();
    while let Some(c) = it.next() {
        match c {
            '"' => {
                while let Some(d) = it.next() {
                    if d == '\\' {
                        if let Some(e) = it.next() {
                            out.push(e);
                        }
                    } else if d == '"' {
                        break;
                    } else {
                        out.push(d);
                    }
                }
            }
            '(' => {
                out.push('(');
                let mut depth = 1;
                let mut lit = false;
                for d in it.by_ref() {
                    if lit {
                        out.push(d);
                        lit = false;
                        continue;
                    }
                    if d == '\\' {
                        lit = true;
                        continue;
                    }
                    match d {
                        '(' => {
                            out.push('(');
                            depth += 1;
                        }
                        ')' => {
                            out.push(')');
                            depth -= 1;
                            if depth == 0 {
                                break;
                            }
                        }
                        _ => out.push(d),
                    }
                }
            }
            _ => out.push(c),
        }
    }
    out
}

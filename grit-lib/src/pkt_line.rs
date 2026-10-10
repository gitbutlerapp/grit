//! Git pkt-line format helpers.
//!
//! Implements reading and writing of the pkt-line framing used in the
//! Git wire protocol (both v0 and v2).

use std::io::{self, Read, Write};

/// Special packet type: flush packet (`0000`).
pub const FLUSH: &str = "0000";
/// Special packet type: delimiter packet (`0001`).
pub const DELIM: &str = "0001";
/// Special packet type: response-end packet (`0002`).
pub const RESPONSE_END: &str = "0002";

/// Write a single pkt-line to `w`. The data should not include a trailing
/// newline; one is appended automatically.
pub fn write_line(w: &mut impl Write, data: &str) -> io::Result<()> {
    let len = 4 + data.len() + 1;
    writeln!(w, "{len:04x}{data}")
}

/// Append a pkt-line encoding of `data` (with trailing newline) to `buf`.
pub fn write_line_to_vec(buf: &mut Vec<u8>, data: &str) -> io::Result<()> {
    let len = 4 + data.len() + 1;
    let line = format!("{len:04x}{data}\n");
    buf.extend_from_slice(line.as_bytes());
    Ok(())
}

/// Write a flush packet (`0000`).
pub fn write_flush(w: &mut impl Write) -> io::Result<()> {
    write!(w, "0000")
}

/// Write one pkt-line with arbitrary bytes (no trailing newline added).
pub fn write_packet_raw(w: &mut impl Write, payload: &[u8]) -> io::Result<()> {
    let total = payload
        .len()
        .checked_add(4)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "pkt-line payload too large"))?;
    if total > 65520 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "pkt-line exceeds maximum size",
        ));
    }
    write!(w, "{total:04x}")?;
    w.write_all(payload)?;
    Ok(())
}

/// Write a delimiter packet (`0001`).
pub fn write_delim(w: &mut impl Write) -> io::Result<()> {
    write!(w, "0001")
}

/// A single packet read from the wire.
#[derive(Debug, PartialEq, Eq)]
pub enum Packet {
    /// A data line (content without trailing newline).
    Data(String),
    /// Flush packet (`0000`).
    Flush,
    /// Delimiter packet (`0001`).
    Delim,
    /// Response-end packet (`0002`).
    ResponseEnd,
}

/// Read one pkt-line from `r`. Returns `None` at EOF.
pub fn read_packet(r: &mut impl Read) -> io::Result<Option<Packet>> {
    let mut len_buf = [0u8; 4];
    match r.read_exact(&mut len_buf) {
        Ok(()) => {}
        Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(e) => return Err(e),
    }
    let len_str =
        std::str::from_utf8(&len_buf).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
    let len = usize::from_str_radix(len_str, 16)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;

    match len {
        0 => Ok(Some(Packet::Flush)),
        1 => Ok(Some(Packet::Delim)),
        2 => Ok(Some(Packet::ResponseEnd)),
        n if n <= 4 => Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("invalid pkt-line length: {n}"),
        )),
        n => {
            let payload_len = n - 4;
            let mut buf = vec![0u8; payload_len];
            r.read_exact(&mut buf)?;
            // Smart HTTP / CGI paths may emit non-UTF-8 in diagnostic pkt-lines; Git tolerates
            // lossy decoding. NUL remains valid and is preserved.
            let s = String::from_utf8_lossy(&buf).into_owned();
            Ok(Some(Packet::Data(
                s.strip_suffix('\n').unwrap_or(&s).to_owned(),
            )))
        }
    }
}

/// Read packets from `r` until a flush or delimiter packet, or EOF.
///
/// Returns the collected data lines and the terminator (`Flush`, `Delim`, or `None`).
pub fn read_until_flush_or_delim(r: &mut impl Read) -> io::Result<(Vec<String>, Option<Packet>)> {
    let mut lines = Vec::new();
    loop {
        match read_packet(r)? {
            None => return Ok((lines, None)),
            Some(Packet::Flush) => return Ok((lines, Some(Packet::Flush))),
            Some(Packet::Delim) => return Ok((lines, Some(Packet::Delim))),
            Some(Packet::ResponseEnd) => return Ok((lines, Some(Packet::ResponseEnd))),
            Some(Packet::Data(s)) => lines.push(s),
        }
    }
}

/// Read data pkt-lines until a flush packet, matching Git's command argument sections.
///
/// A delimiter or response-end packet is treated as a protocol violation and reported with
/// `err_not_flush` (for example: `"expected flush after ls-refs arguments"`).
pub fn read_data_lines_until_flush(
    r: &mut impl Read,
    err_not_flush: &str,
) -> io::Result<Vec<String>> {
    let mut lines = Vec::new();
    loop {
        match read_packet(r)? {
            None => {
                return Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    err_not_flush.to_string(),
                ));
            }
            Some(Packet::Flush) => return Ok(lines),
            Some(Packet::Delim) | Some(Packet::ResponseEnd) => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    err_not_flush.to_string(),
                ));
            }
            Some(Packet::Data(s)) => lines.push(s),
        }
    }
}

/// Write one sideband pkt-line on `band`.
pub fn write_sideband_packet(
    w: &mut (impl Write + ?Sized),
    band: u8,
    payload: &[u8],
) -> io::Result<()> {
    let len = 4 + 1 + payload.len();
    write!(w, "{len:04x}")?;
    w.write_all(&[band])?;
    w.write_all(payload)?;
    Ok(())
}

/// Write payload on sideband channel 1 using the same 64k chunking as `git upload-pack`.
pub fn write_sideband_channel1_64k(w: &mut impl Write, payload: &[u8]) -> io::Result<()> {
    const MAX_PAYLOAD: usize = 65515;
    for chunk in payload.chunks(MAX_PAYLOAD) {
        let len = 4 + 1 + chunk.len();
        write!(w, "{len:04x}")?;
        w.write_all(&[1u8])?;
        w.write_all(chunk)?;
    }
    Ok(())
}

/// Parse a 4-byte hexadecimal pkt-line length prefix.
pub fn parse_hex_len(prefix: &[u8]) -> io::Result<usize> {
    let s = std::str::from_utf8(prefix)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, format!("{e}")))?;
    usize::from_str_radix(s, 16)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, format!("invalid length: {e}")))
}

/// Decode a sideband pkt-line stream into the raw primary (band 1) payload.
pub fn decode_sideband_primary(mut input: &[u8]) -> io::Result<Vec<u8>> {
    let mut out = Vec::new();
    while !input.is_empty() {
        if input.len() < 4 {
            break;
        }
        let len = parse_hex_len(&input[..4])?;
        input = &input[4..];
        if len == 0 {
            break;
        }
        if len <= 4 || input.len() < len - 4 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "truncated sideband packet",
            ));
        }
        let payload_len = len - 4;
        let payload = &input[..payload_len];
        input = &input[payload_len..];
        if payload.is_empty() {
            continue;
        }
        let band = payload[0];
        let data = &payload[1..];
        if band == 1 {
            out.extend_from_slice(data);
        }
    }
    Ok(out)
}

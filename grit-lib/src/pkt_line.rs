//! Git pkt-line format helpers.
//!
//! Implements reading and writing of the pkt-line framing used in the
//! Git wire protocol (both v0 and v2).

use std::io::{self, Read, Write};

use crate::error::{Error, Result};

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

/// Incrementally frame pack bytes on sideband channel 1 (64k pkt-lines).
pub struct SidebandChannel1Writer<'a, W: Write + ?Sized + 'a> {
    inner: &'a mut W,
    pending: Vec<u8>,
}

impl<'a, W: Write + ?Sized + 'a> SidebandChannel1Writer<'a, W> {
    /// Wrap `inner` as a [`Write`] target that emits sideband-64k frames.
    pub fn new(inner: &'a mut W) -> Self {
        Self {
            inner,
            pending: Vec::new(),
        }
    }

    /// Flush any buffered band-1 bytes.
    pub fn finish(mut self) -> io::Result<()> {
        if !self.pending.is_empty() {
            write_sideband_packet(self.inner, 1, &self.pending)?;
            self.pending.clear();
        }
        Ok(())
    }
}

impl<'a, W: Write + ?Sized + 'a> Write for SidebandChannel1Writer<'a, W> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        const MAX_PAYLOAD: usize = 65515;
        let mut offset = 0usize;
        // Drain any carry-over first without duplicating the new slice.
        if !self.pending.is_empty() {
            let need = MAX_PAYLOAD.saturating_sub(self.pending.len());
            if need > 0 && offset < buf.len() {
                let take = need.min(buf.len());
                self.pending.extend_from_slice(&buf[..take]);
                offset += take;
            }
            while self.pending.len() >= MAX_PAYLOAD {
                let chunk: Vec<u8> = self.pending.drain(..MAX_PAYLOAD).collect();
                write_sideband_packet(self.inner, 1, &chunk)?;
            }
        }
        while offset + MAX_PAYLOAD <= buf.len() {
            write_sideband_packet(self.inner, 1, &buf[offset..offset + MAX_PAYLOAD])?;
            offset += MAX_PAYLOAD;
        }
        if offset < buf.len() {
            self.pending.extend_from_slice(&buf[offset..]);
        }
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        self.inner.flush()
    }
}

/// Parse a 4-byte hexadecimal pkt-line length prefix.
pub fn parse_hex_len(prefix: &[u8]) -> io::Result<usize> {
    let s = std::str::from_utf8(prefix)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, format!("{e}")))?;
    usize::from_str_radix(s, 16)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, format!("invalid length: {e}")))
}

/// Fail with [`Error::RemoteFatal`] when `line` is an `ERR <message>` data line.
///
/// A server may send `ERR` in place of any line it would otherwise send; call this on
/// each data line before interpreting it.
///
/// # Errors
///
/// [`Error::RemoteFatal`] carrying the message, trailing whitespace trimmed.
pub(crate) fn reject_err_line(line: &str) -> Result<()> {
    match line.strip_prefix("ERR ") {
        Some(message) => Err(Error::RemoteFatal {
            message: message.trim_end().to_owned(),
        }),
        None => Ok(()),
    }
}

/// [`Error::RemoteFatal`] for the payload of a side-band channel-3 packet (band byte removed).
pub(crate) fn sideband_fatal(payload: &[u8]) -> Error {
    Error::RemoteFatal {
        message: String::from_utf8_lossy(payload).trim_end().to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fatal_message(err: Error) -> String {
        match err {
            Error::RemoteFatal { message } => message,
            other => panic!("expected RemoteFatal, got {other:?}"),
        }
    }

    #[test]
    fn err_line_is_a_remote_fatal_error() {
        let err = reject_err_line("ERR upload-pack: not our ref deadbeef\n").unwrap_err();
        assert_eq!(err.kind_name(), "remote_fatal");
        assert_eq!(
            err.to_string(),
            "remote error: upload-pack: not our ref deadbeef"
        );
        assert_eq!(fatal_message(err), "upload-pack: not our ref deadbeef");
    }

    #[test]
    fn other_lines_pass_through() {
        for line in [
            "NAK",
            "ACK 0123",
            "ERRATA",
            "ERR",
            " ERR x",
            "err lower",
            "",
        ] {
            assert!(reject_err_line(line).is_ok(), "{line:?}");
        }
    }

    #[test]
    fn sideband_fatal_trims_and_decodes_lossily() {
        let err = sideband_fatal(b"pack-objects died \xff\n");
        assert_eq!(fatal_message(err), "pack-objects died \u{fffd}");
    }
}

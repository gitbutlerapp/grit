//! Reusable zlib inflate for pack and loose object reads.

use std::io::Read;

use flate2::{Decompress, FlushDecompress, Status};

use crate::error::{Error, Result};
use crate::unpack_objects::delta_uncompressed_result_size_if_complete;

const READ_CHUNK: usize = 64 * 1024;

/// Largest up-front output reservation taken from a loose object's declared size.
///
/// The header is untrusted until the stream has been fully inflated, so a corrupt size
/// must not trigger a huge allocation; larger objects grow geometrically past this.
const MAX_HEADER_SIZE_RESERVE: usize = 16 * 1024 * 1024;

/// Bytes of inflated output inspected for a loose object's `"<type> <size>\0"` header.
const LOOSE_HEADER_MAX: usize = 128;

// hygiene: per-thread reusable inflate scratch; holds no state between objects
thread_local! {
    static LOOSE_SCRATCH: std::cell::RefCell<ZlibInflateScratch> =
        std::cell::RefCell::new(ZlibInflateScratch::default());
}

/// Run `f` with this thread's reusable loose-object inflate scratch.
///
/// Reusing the inflater and input buffer avoids allocating and initialising fresh zlib
/// state for every loose object read. Falls back to a fresh scratch when the thread's
/// scratch is already borrowed.
pub(crate) fn with_loose_scratch<R>(f: impl FnOnce(&mut ZlibInflateScratch) -> R) -> R {
    LOOSE_SCRATCH.with(|cell| match cell.try_borrow_mut() {
        Ok(mut scratch) => f(&mut scratch),
        Err(_) => f(&mut ZlibInflateScratch::default()),
    })
}

/// Total inflated length (`header + payload`) declared by a loose object header in `out`.
///
/// Returns `None` until the header's NUL is present, or when the header is malformed (the
/// caller's later header parse reports the corruption).
fn declared_loose_object_len(out: &[u8]) -> Option<usize> {
    let head = &out[..out.len().min(LOOSE_HEADER_MAX)];
    let nul = memchr::memchr(0, head)?;
    let space = memchr::memchr(b' ', &head[..nul])?;
    let digits = &head[space + 1..nul];
    if digits.is_empty() || !digits.iter().all(u8::is_ascii_digit) {
        return None;
    }
    let size: usize = std::str::from_utf8(digits).ok()?.parse().ok()?;
    size.checked_add(nul + 1)
}

/// `read` into `buf`, retrying when interrupted by a signal.
fn read_retrying(file: &mut std::fs::File, buf: &mut [u8]) -> Result<usize> {
    loop {
        match file.read(buf) {
            Ok(n) => return Ok(n),
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
            Err(e) => return Err(Error::Io(e)),
        }
    }
}

/// Scratch space for one zlib stream decode; reset before each object.
pub(crate) struct ZlibInflateScratch {
    dec: Decompress,
    /// Reused prefix buffer for pack delta size reads (batch-check hot path).
    delta_prefix: Vec<u8>,
    /// Reused compressed-input buffer for loose object reads.
    input: Vec<u8>,
}

impl Default for ZlibInflateScratch {
    fn default() -> Self {
        Self {
            dec: Decompress::new(true),
            delta_prefix: Vec::with_capacity(64),
            input: Vec::new(),
        }
    }
}

impl ZlibInflateScratch {
    /// Prepare for a new zlib wrapper stream.
    pub fn reset(&mut self) {
        self.dec.reset(true);
        self.delta_prefix.clear();
    }

    /// Read the Git delta "result size" varint from a pack zlib stream without inflating the object.
    ///
    /// Stops inflating as soon as both delta varints are available. Advances `*pos` by compressed
    /// bytes consumed from `bytes`.
    pub fn read_pack_delta_result_size(&mut self, bytes: &[u8], pos: &mut usize) -> Result<u64> {
        const MAX_PREFIX: usize = 256;
        self.reset();
        let input = &bytes[*pos..];
        let mut in_off = 0usize;
        let mut chunk = [0u8; 32];
        loop {
            if let Some(dest) = delta_uncompressed_result_size_if_complete(&self.delta_prefix)? {
                *pos += in_off;
                return u64::try_from(dest)
                    .map_err(|_| Error::CorruptObject("delta result size overflow".to_owned()));
            }
            if self.delta_prefix.len() >= MAX_PREFIX {
                return Err(Error::CorruptObject(
                    "delta size prefix exceeds header limit".to_owned(),
                ));
            }
            if in_off >= input.len() {
                return Err(Error::CorruptObject(
                    "truncated delta zlib stream".to_owned(),
                ));
            }
            let before_in = self.dec.total_in();
            let before_out = self.dec.total_out();
            let status = self
                .dec
                .decompress(&input[in_off..], &mut chunk, FlushDecompress::None)
                .map_err(|e| Error::Zlib(e.to_string()))?;
            let consumed = (self.dec.total_in() - before_in) as usize;
            let produced = (self.dec.total_out() - before_out) as usize;
            in_off += consumed;
            if produced > 0 {
                self.delta_prefix
                    .extend_from_slice(&chunk[..produced.min(chunk.len())]);
            }
            match status {
                Status::StreamEnd => {
                    if let Some(dest) =
                        delta_uncompressed_result_size_if_complete(&self.delta_prefix)?
                    {
                        *pos += in_off;
                        return u64::try_from(dest).map_err(|_| {
                            Error::CorruptObject("delta result size overflow".to_owned())
                        });
                    }
                    return Err(Error::CorruptObject(
                        "delta size prefix exceeds header limit".to_owned(),
                    ));
                }
                Status::Ok | Status::BufError => {
                    if consumed == 0 && produced == 0 {
                        return Err(Error::CorruptObject(
                            "zlib stream stalled before delta size".to_owned(),
                        ));
                    }
                }
            }
        }
    }

    /// Inflate a zlib stream in `bytes` starting at `*pos` to exactly `expected_size` bytes.
    ///
    /// Advances `*pos` by the number of compressed bytes consumed.
    ///
    /// # Errors
    ///
    /// Returns [`Error::CorruptObject`] when declared size does not match inflated output.
    /// Returns [`Error::Zlib`] for invalid or truncated zlib data.
    /// Inflate exactly `expected_size` bytes and pass each produced slice to `update`.
    ///
    /// Uses a fixed stack buffer so large pack objects are not allocated whole in memory.
    pub fn decompress_fixed_update(
        &mut self,
        bytes: &[u8],
        pos: &mut usize,
        expected_size: u64,
        mut update: impl FnMut(&[u8]) -> Result<()>,
    ) -> Result<()> {
        let expected = usize::try_from(expected_size).map_err(|_| {
            Error::CorruptObject(format!("pack object size overflow: {expected_size}"))
        })?;
        self.reset();
        let input = &bytes[*pos..];
        let mut in_off = 0usize;
        let mut out_pos = 0usize;
        let mut chunk = [0u8; READ_CHUNK];
        loop {
            let flush = if in_off >= input.len() {
                FlushDecompress::Finish
            } else {
                FlushDecompress::None
            };
            let before_in = self.dec.total_in();
            let before_out = self.dec.total_out();
            let status = self
                .dec
                .decompress(&input[in_off..], &mut chunk, flush)
                .map_err(|e| Error::Zlib(e.to_string()))?;
            let consumed = (self.dec.total_in() - before_in) as usize;
            let produced = (self.dec.total_out() - before_out) as usize;
            in_off += consumed;
            if produced > 0 {
                update(&chunk[..produced])?;
                out_pos += produced;
            }
            match status {
                Status::StreamEnd => {
                    if out_pos != expected {
                        return Err(Error::CorruptObject(format!(
                            "pack object size mismatch: expected {expected}, got {out_pos}"
                        )));
                    }
                    *pos += in_off;
                    return Ok(());
                }
                Status::Ok | Status::BufError => {
                    if in_off >= input.len() && flush == FlushDecompress::Finish {
                        return Err(Error::Zlib("unexpected end of zlib stream".to_owned()));
                    }
                    if consumed == 0 && produced == 0 {
                        return Err(Error::CorruptObject(
                            "zlib stream stalled before declared object size".to_owned(),
                        ));
                    }
                }
            }
        }
    }

    pub fn decompress_fixed(
        &mut self,
        bytes: &[u8],
        pos: &mut usize,
        expected_size: u64,
    ) -> Result<Vec<u8>> {
        let expected = usize::try_from(expected_size).map_err(|_| {
            Error::CorruptObject(format!("pack object size overflow: {expected_size}"))
        })?;
        let input = &bytes[*pos..];
        let mut out = vec![0u8; expected];
        let consumed = self.decompress_into(input, &mut out, true)?;
        *pos += consumed;
        Ok(out)
    }

    /// Decompress at most `max_out` bytes into `out`, reusing `out`'s capacity when possible.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Zlib`] for invalid or truncated zlib data.
    pub fn decompress_prefix_into(
        &mut self,
        bytes: &[u8],
        max_out: usize,
        out: &mut Vec<u8>,
    ) -> Result<usize> {
        out.clear();
        if max_out == 0 {
            let (_, consumed) = self.inflate_prefix_zero(bytes)?;
            return Ok(consumed);
        }
        out.resize(max_out, 0);
        let consumed = self.decompress_into(bytes, out, false)?;
        out.truncate(
            self.dec
                .total_out()
                .try_into()
                .map_err(|_| Error::CorruptObject("inflate output overflow".to_owned()))?,
        );
        Ok(consumed)
    }

    fn inflate_prefix_zero(&mut self, bytes: &[u8]) -> Result<(Vec<u8>, usize)> {
        self.reset();
        let mut sink = [0u8; 1];
        let status = self
            .dec
            .decompress(bytes, &mut sink, FlushDecompress::Finish)
            .map_err(|e| Error::Zlib(e.to_string()))?;
        let consumed = self.dec.total_in() as usize;
        match status {
            Status::StreamEnd => Ok((Vec::new(), consumed)),
            Status::Ok | Status::BufError if consumed == 0 && bytes.is_empty() => {
                Err(Error::Zlib("unexpected end of zlib stream".to_owned()))
            }
            Status::Ok | Status::BufError => Ok((Vec::new(), consumed)),
        }
    }

    /// Inflate a whole zlib-wrapped loose object file into `out`, reusing `out`'s capacity.
    ///
    /// Inflates straight into `out`'s spare capacity. Once the Git object header has been
    /// inflated, `out` is sized from the declared object size (bounded by
    /// [`MAX_HEADER_SIZE_RESERVE`]) so typical objects decode without reallocating. The file
    /// is read in [`READ_CHUNK`] pieces and reading stops at the end of the zlib stream, so a
    /// small object costs a single `read`.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Zlib`] for invalid zlib, preset-dictionary streams, or truncated input.
    /// Returns [`Error::Io`] for read failures, including a file too short to hold a zlib header.
    pub fn inflate_loose_file(
        &mut self,
        file: &mut std::fs::File,
        out: &mut Vec<u8>,
    ) -> Result<()> {
        self.reset();
        out.clear();
        let mut input = std::mem::take(&mut self.input);
        if input.len() < READ_CHUNK {
            input.resize(READ_CHUNK, 0);
        }
        let result = self.inflate_loose_reads(&mut input, file, out);
        self.input = input;
        result
    }

    fn inflate_loose_reads(
        &mut self,
        input: &mut [u8],
        file: &mut std::fs::File,
        out: &mut Vec<u8>,
    ) -> Result<()> {
        let mut filled = 0usize;
        while filled < 2 {
            let n = read_retrying(file, &mut input[filled..])?;
            if n == 0 {
                return Err(Error::Io(std::io::ErrorKind::UnexpectedEof.into()));
            }
            filled += n;
        }
        let cmf_flg = u16::from(input[0]) << 8 | u16::from(input[1]);
        let preset_dictionary = cmf_flg != 0 && cmf_flg % 31 == 0 && (input[1] & 0x20) != 0;
        let mut in_off = 0usize;
        let mut eof = false;
        let mut sized_from_header = false;
        loop {
            if in_off == filled && !eof {
                filled = read_retrying(file, input)?;
                in_off = 0;
                eof = filled == 0;
            }
            if out.len() == out.capacity() {
                let guess = if out.is_empty() {
                    filled.saturating_mul(4).max(256)
                } else {
                    out.capacity()
                };
                out.reserve(guess);
            }
            let flush = if eof {
                FlushDecompress::Finish
            } else {
                FlushDecompress::None
            };
            let before_in = self.dec.total_in();
            let before_out = out.len();
            let status = match self.dec.decompress_vec(&input[in_off..filled], out, flush) {
                Ok(s) => s,
                Err(e) => {
                    if preset_dictionary {
                        return Err(Error::Zlib("needs dictionary".to_owned()));
                    }
                    return Err(Error::Zlib(e.to_string()));
                }
            };
            let consumed = usize::try_from(self.dec.total_in() - before_in)
                .map_err(|_| Error::CorruptObject("inflate input overflow".to_owned()))?;
            in_off += consumed;
            let produced = out.len() - before_out;
            if !sized_from_header {
                if let Some(total) = declared_loose_object_len(out) {
                    sized_from_header = true;
                    let wanted = total.min(MAX_HEADER_SIZE_RESERVE);
                    out.reserve_exact(wanted.saturating_sub(out.len()));
                }
            }
            match status {
                Status::StreamEnd => return Ok(()),
                Status::Ok | Status::BufError => {
                    let out_has_room = out.len() < out.capacity();
                    if eof && in_off == filled && out_has_room {
                        return Err(Error::Zlib("unexpected end of zlib stream".to_owned()));
                    }
                    if consumed == 0 && produced == 0 && out_has_room && in_off < filled {
                        return Err(Error::CorruptObject(
                            "zlib stream stalled in loose object".to_owned(),
                        ));
                    }
                }
            }
        }
    }

    /// Inflate just the `"<type> <size>\0"` header of a loose object from `file`.
    ///
    /// Stops as soon as the header's NUL has been produced, the header buffer limit is
    /// reached, or the stream ends, and returns the inflated prefix.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Zlib`] for invalid zlib data and [`Error::Io`] for read failures.
    pub fn inflate_loose_header(&mut self, file: &mut std::fs::File) -> Result<Vec<u8>> {
        const HEADER_INPUT_CHUNK: usize = 512;
        self.reset();
        let mut prefix = Vec::with_capacity(LOOSE_HEADER_MAX);
        let mut input = [0u8; HEADER_INPUT_CHUNK];
        loop {
            let n = file.read(&mut input).map_err(Error::Io)?;
            if n == 0 {
                return Err(Error::Zlib("unexpected end of zlib stream".to_owned()));
            }
            let mut in_off = 0usize;
            while in_off < n {
                let before_in = self.dec.total_in();
                let before_out = prefix.len();
                let status = self
                    .dec
                    .decompress_vec(&input[in_off..n], &mut prefix, FlushDecompress::None)
                    .map_err(|e| Error::Zlib(e.to_string()))?;
                let consumed = usize::try_from(self.dec.total_in() - before_in)
                    .map_err(|_| Error::CorruptObject("inflate input overflow".to_owned()))?;
                in_off += consumed;
                let done = status == Status::StreamEnd
                    || prefix.len() == prefix.capacity()
                    || prefix[before_out..].contains(&0);
                if done {
                    return Ok(prefix);
                }
                if consumed == 0 && prefix.len() == before_out {
                    return Err(Error::CorruptObject(
                        "zlib stream stalled in loose object header".to_owned(),
                    ));
                }
            }
        }
    }

    /// Decompress a fixed-size pack zlib stream into `out`, advancing `*pos`.
    pub fn decompress_fixed_into(
        &mut self,
        bytes: &[u8],
        pos: &mut usize,
        expected_size: u64,
        out: &mut Vec<u8>,
    ) -> Result<()> {
        let expected = usize::try_from(expected_size).map_err(|_| {
            Error::CorruptObject(format!("pack object size overflow: {expected_size}"))
        })?;
        out.clear();
        out.resize(expected, 0);
        let consumed = self.decompress_into(&bytes[*pos..], out, true)?;
        *pos += consumed;
        Ok(())
    }

    /// Advance past a zlib stream without retaining inflated bytes (for pack skipping).
    ///
    /// # Errors
    ///
    /// Returns [`Error::CorruptObject`] when inflated output exceeds `expected_size`, or when
    /// the stream ends before producing `expected_size` bytes. Returns [`Error::Zlib`] for
    /// invalid or truncated zlib data.
    pub fn skip_zlib_stream(
        &mut self,
        bytes: &[u8],
        pos: &mut usize,
        expected_size: u64,
    ) -> Result<()> {
        let expected = usize::try_from(expected_size).map_err(|_| {
            Error::CorruptObject(format!("pack object size overflow: {expected_size}"))
        })?;
        let input = &bytes[*pos..];
        self.reset();
        let mut in_off = 0usize;
        let mut out_written = 0usize;
        let mut discard = vec![0u8; expected.clamp(1, READ_CHUNK)];
        while out_written < expected {
            let remaining = expected - out_written;
            let flush = if in_off >= input.len() {
                FlushDecompress::Finish
            } else {
                FlushDecompress::None
            };
            let out_cap = discard.len().min(remaining);
            let before_in = self.dec.total_in();
            let before_out = self.dec.total_out();
            let status = self
                .dec
                .decompress(&input[in_off..], &mut discard[..out_cap], flush)
                .map_err(|e| Error::Zlib(e.to_string()))?;
            let consumed = (self.dec.total_in() - before_in) as usize;
            let produced = (self.dec.total_out() - before_out) as usize;
            in_off += consumed;
            out_written += produced;
            match status {
                Status::StreamEnd => {
                    if out_written != expected {
                        return Err(Error::CorruptObject(format!(
                            "pack object size mismatch: expected {expected_size}, got {out_written}"
                        )));
                    }
                    *pos += in_off;
                    return Ok(());
                }
                Status::Ok | Status::BufError => {
                    if in_off >= input.len() && out_written < expected {
                        return Err(Error::Zlib("unexpected end of zlib stream".to_owned()));
                    }
                    if consumed == 0 && produced == 0 {
                        return Err(Error::CorruptObject(
                            "zlib stream stalled before declared object size".to_owned(),
                        ));
                    }
                }
            }
        }
        self.finish_zlib_without_extra_output(&input[in_off..], &mut in_off)?;
        if out_written != expected {
            return Err(Error::CorruptObject(format!(
                "pack object size mismatch: expected {expected_size}, got {out_written}"
            )));
        }
        *pos += in_off;
        Ok(())
    }

    /// Consume the remainder of a zlib stream after the declared uncompressed size is satisfied.
    fn finish_zlib_without_extra_output(&mut self, input: &[u8], in_off: &mut usize) -> Result<()> {
        let mut sink = [0u8; 1];
        loop {
            let flush = if *in_off >= input.len() {
                FlushDecompress::Finish
            } else {
                FlushDecompress::None
            };
            let before_in = self.dec.total_in();
            let before_out = self.dec.total_out();
            let status = self
                .dec
                .decompress(&input[*in_off..], &mut sink, flush)
                .map_err(|e| Error::Zlib(e.to_string()))?;
            let consumed = (self.dec.total_in() - before_in) as usize;
            let produced = (self.dec.total_out() - before_out) as usize;
            *in_off += consumed;
            if produced > 0 {
                return Err(Error::CorruptObject(
                    "zlib stream larger than declared object size".to_owned(),
                ));
            }
            match status {
                Status::StreamEnd => return Ok(()),
                Status::Ok | Status::BufError => {
                    if *in_off >= input.len() && status != Status::StreamEnd {
                        return Err(Error::Zlib("unexpected end of zlib stream".to_owned()));
                    }
                    if consumed == 0 && produced == 0 {
                        return Err(Error::CorruptObject(
                            "zlib stream stalled after declared object size".to_owned(),
                        ));
                    }
                }
            }
        }
    }

    fn decompress_into(
        &mut self,
        input: &[u8],
        out: &mut [u8],
        require_full_output: bool,
    ) -> Result<usize> {
        self.reset();
        let mut in_off = 0usize;
        let mut out_pos = 0usize;
        loop {
            let flush = if in_off >= input.len() {
                if require_full_output {
                    FlushDecompress::Finish
                } else if out_pos >= out.len() {
                    FlushDecompress::Sync
                } else {
                    FlushDecompress::Finish
                }
            } else {
                FlushDecompress::None
            };
            let before_in = self.dec.total_in();
            let before_out = self.dec.total_out();
            let status = self
                .dec
                .decompress(&input[in_off..], &mut out[out_pos..], flush)
                .map_err(|e| Error::Zlib(e.to_string()))?;
            let consumed = (self.dec.total_in() - before_in) as usize;
            let produced = (self.dec.total_out() - before_out) as usize;
            in_off += consumed;
            out_pos += produced;
            match status {
                Status::StreamEnd => {
                    if require_full_output && out_pos != out.len() {
                        return Err(Error::CorruptObject(format!(
                            "pack object size mismatch: expected {}, got {out_pos}",
                            out.len()
                        )));
                    }
                    return Ok(in_off);
                }
                Status::Ok | Status::BufError => {
                    if !require_full_output && out_pos >= out.len() {
                        return Ok(in_off);
                    }
                    if require_full_output && out_pos >= out.len() {
                        let mut tail_off = 0usize;
                        self.finish_zlib_without_extra_output(&input[in_off..], &mut tail_off)?;
                        return Ok(in_off + tail_off);
                    }
                    if in_off >= input.len() && flush == FlushDecompress::Finish {
                        return Err(Error::Zlib("unexpected end of zlib stream".to_owned()));
                    }
                    if consumed == 0 && produced == 0 {
                        return Err(Error::CorruptObject(
                            "zlib stream stalled before declared object size".to_owned(),
                        ));
                    }
                }
            }
        }
    }
}

/// Inflate at most `max_out` bytes from a zlib stream at the start of `bytes`.
#[cfg(test)]
pub(crate) fn inflate_prefix(bytes: &[u8], max_out: usize) -> Result<(Vec<u8>, usize)> {
    let mut out = Vec::new();
    let consumed =
        ZlibInflateScratch::default().decompress_prefix_into(bytes, max_out, &mut out)?;
    Ok((out, consumed))
}

#[cfg(test)]
mod tests {
    use super::*;
    use flate2::write::ZlibEncoder;
    use flate2::Compression;
    use std::io::Write;

    fn deflate(data: &[u8]) -> Vec<u8> {
        let mut enc = ZlibEncoder::new(Vec::new(), Compression::default());
        enc.write_all(data).unwrap();
        enc.finish().unwrap()
    }

    #[test]
    fn decompress_fixed_round_trip() {
        let plain = b"blob 11\0hello world";
        let zlib = deflate(plain);
        let mut pos = 0usize;
        let mut scratch = ZlibInflateScratch::default();
        let out = scratch
            .decompress_fixed(&zlib, &mut pos, plain.len() as u64)
            .unwrap();
        assert_eq!(out.as_slice(), plain);
        assert_eq!(pos, zlib.len());
    }

    #[test]
    fn inflate_prefix_reads_header_only() {
        let plain = b"commit 100\0".to_vec().repeat(10);
        let zlib = deflate(&plain);
        let (prefix, consumed) = inflate_prefix(&zlib, 32).unwrap();
        assert!(prefix.starts_with(b"commit "));
        assert!(consumed <= zlib.len());
    }

    #[test]
    fn truncated_zlib_returns_zlib_error_not_panic() {
        let plain = b"blob 3\0foo";
        let zlib = deflate(plain);
        let truncated = &zlib[..zlib.len() / 2];
        let mut pos = 0usize;
        let mut scratch = ZlibInflateScratch::default();
        let err = scratch
            .decompress_fixed(truncated, &mut pos, plain.len() as u64)
            .unwrap_err();
        assert!(matches!(err, Error::Zlib(_)));
    }

    #[test]
    fn corrupt_zlib_returns_zlib_error() {
        let mut bad = deflate(b"ok");
        if let Some(b) = bad.get_mut(4) {
            *b ^= 0xff;
        }
        let mut pos = 0usize;
        let mut scratch = ZlibInflateScratch::default();
        let err = scratch.decompress_fixed(&bad, &mut pos, 2).unwrap_err();
        assert!(
            matches!(err, Error::Zlib(_) | Error::CorruptObject(_)),
            "got {err:?}"
        );
    }

    #[test]
    fn declared_size_zero_rejects_nonempty_zlib() {
        let zlib = deflate(b"x");
        let mut pos = 0usize;
        let mut scratch = ZlibInflateScratch::default();
        let err = scratch.decompress_fixed(&zlib, &mut pos, 0).unwrap_err();
        assert!(matches!(err, Error::CorruptObject(_)), "got {err:?}");
    }

    #[test]
    fn declared_size_smaller_than_zlib_output_is_corrupt() {
        let plain = b"hello";
        let zlib = deflate(plain);
        let mut pos = 0usize;
        let mut scratch = ZlibInflateScratch::default();
        let err = scratch.decompress_fixed(&zlib, &mut pos, 2).unwrap_err();
        assert!(matches!(err, Error::CorruptObject(_)), "got {err:?}");
    }

    #[test]
    fn skip_zlib_stream_rejects_size_zero_with_payload() {
        let zlib = deflate(b"x");
        let mut pos = 0usize;
        let mut scratch = ZlibInflateScratch::default();
        let err = scratch.skip_zlib_stream(&zlib, &mut pos, 0).unwrap_err();
        assert!(matches!(err, Error::CorruptObject(_)), "got {err:?}");
    }

    #[test]
    fn skip_zlib_stream_rejects_declared_size_smaller_than_output() {
        let zlib = deflate(b"abcd");
        let mut pos = 0usize;
        let mut scratch = ZlibInflateScratch::default();
        let err = scratch.skip_zlib_stream(&zlib, &mut pos, 1).unwrap_err();
        assert!(matches!(err, Error::CorruptObject(_)), "got {err:?}");
    }

    #[test]
    fn skip_zlib_stream_accepts_exact_size() {
        let plain = b"pack-sized payload";
        let zlib = deflate(plain);
        let mut pos = 0usize;
        let mut scratch = ZlibInflateScratch::default();
        scratch
            .skip_zlib_stream(&zlib, &mut pos, plain.len() as u64)
            .expect("skip");
        assert_eq!(pos, zlib.len());
    }

    #[test]
    fn inflate_loose_file_reads_from_file() {
        use std::io::Write;
        let plain = b"blob 5\0hello";
        let zlib = deflate(plain);
        let mut file = tempfile::NamedTempFile::new().expect("temp");
        file.write_all(&zlib).expect("write");
        let mut scratch = ZlibInflateScratch::default();
        let mut out = Vec::new();
        scratch
            .inflate_loose_file(&mut file.reopen().expect("reopen"), &mut out)
            .expect("decompress file");
        assert_eq!(out.as_slice(), plain);
    }

    #[test]
    fn inflate_prefix_zero_on_empty_bytes_errors() {
        let err = inflate_prefix(b"", 0).unwrap_err();
        assert!(matches!(err, Error::Zlib(_)));
    }

    #[test]
    fn reset_allows_reuse_after_failed_decode() {
        let mut scratch = ZlibInflateScratch::default();
        let mut pos = 0usize;
        let _ = scratch.decompress_fixed(b"not-zlib", &mut pos, 1);
        scratch.reset();
        let plain = b"ok";
        let zlib = deflate(plain);
        pos = 0;
        let out = scratch
            .decompress_fixed(&zlib, &mut pos, plain.len() as u64)
            .expect("after reset");
        assert_eq!(out.as_slice(), plain);
    }

    #[test]
    fn inflate_prefix_zero_consumes_header_without_output() {
        let zlib = deflate(b"blob 10\0");
        let (out, consumed) = inflate_prefix(&zlib, 0).expect("prefix zero");
        assert!(out.is_empty());
        assert!(consumed > 0);
    }

    #[test]
    fn inflate_loose_file_preset_dictionary_from_file() {
        let mut file = tempfile::NamedTempFile::new().expect("temp");
        file.write_all(&[0x78, 0x20, 0, 0, 0, 0, 0x03, 0x00])
            .expect("write");
        let mut scratch = ZlibInflateScratch::default();
        let err = scratch
            .inflate_loose_file(&mut file.reopen().expect("reopen"), &mut Vec::new())
            .unwrap_err();
        match err {
            Error::Zlib(msg) if msg == "needs dictionary" => {}
            other => panic!("expected needs dictionary, got {other:?}"),
        }
    }

    /// Incompressible bytes, so the deflated stream is about as large as the input.
    fn noise(len: usize) -> Vec<u8> {
        let mut state = 0x9e37_79b9_7f4a_7c15u64;
        (0..len)
            .map(|_| {
                state ^= state << 13;
                state ^= state >> 7;
                state ^= state << 17;
                state.to_le_bytes()[0]
            })
            .collect()
    }

    fn loose_file(plain: &[u8]) -> tempfile::NamedTempFile {
        let mut file = tempfile::NamedTempFile::new().expect("temp");
        file.write_all(&deflate(plain)).expect("write");
        file
    }

    fn loose_bytes(kind: &str, payload: &[u8]) -> Vec<u8> {
        let mut plain = format!("{kind} {}\0", payload.len()).into_bytes();
        plain.extend_from_slice(payload);
        plain
    }

    #[test]
    fn inflate_loose_file_spans_several_read_chunks() {
        let plain = loose_bytes("blob", &noise(3 * READ_CHUNK + 123));
        let file = loose_file(&plain);
        assert!(file.as_file().metadata().expect("meta").len() > 2 * READ_CHUNK as u64);
        let mut out = Vec::new();
        ZlibInflateScratch::default()
            .inflate_loose_file(&mut file.reopen().expect("reopen"), &mut out)
            .expect("inflate");
        assert_eq!(out, plain);
    }

    #[test]
    fn inflate_loose_file_sizes_output_from_header() {
        let plain = loose_bytes("blob", &vec![b'a'; 200_000]);
        let file = loose_file(&plain);
        let mut out = Vec::new();
        ZlibInflateScratch::default()
            .inflate_loose_file(&mut file.reopen().expect("reopen"), &mut out)
            .expect("inflate");
        assert_eq!(out, plain);
        assert_eq!(out.capacity(), plain.len());
    }

    #[test]
    fn inflate_loose_file_bounds_reservation_for_lying_header() {
        let plain = b"blob 99999999999999\0tiny".to_vec();
        let file = loose_file(&plain);
        let mut out = Vec::new();
        ZlibInflateScratch::default()
            .inflate_loose_file(&mut file.reopen().expect("reopen"), &mut out)
            .expect("inflate");
        assert_eq!(out, plain);
        assert!(out.capacity() <= MAX_HEADER_SIZE_RESERVE);
    }

    #[test]
    fn inflate_loose_file_truncated_stream_is_zlib_error() {
        let zlib = deflate(&loose_bytes("blob", &noise(5000)));
        let mut file = tempfile::NamedTempFile::new().expect("temp");
        file.write_all(&zlib[..zlib.len() / 2]).expect("write");
        let err = ZlibInflateScratch::default()
            .inflate_loose_file(&mut file.reopen().expect("reopen"), &mut Vec::new())
            .unwrap_err();
        assert!(matches!(err, Error::Zlib(_)), "got {err:?}");
    }

    #[test]
    fn inflate_loose_file_shorter_than_zlib_header_is_eof() {
        for bytes in [&[][..], &[0x78][..]] {
            let mut file = tempfile::NamedTempFile::new().expect("temp");
            file.write_all(bytes).expect("write");
            let err = ZlibInflateScratch::default()
                .inflate_loose_file(&mut file.reopen().expect("reopen"), &mut Vec::new())
                .unwrap_err();
            match err {
                Error::Io(e) => assert_eq!(e.kind(), std::io::ErrorKind::UnexpectedEof),
                other => panic!("expected unexpected EOF, got {other:?}"),
            }
        }
    }

    #[test]
    fn inflate_loose_file_ignores_bytes_after_stream_end() {
        let plain = loose_bytes("blob", b"payload");
        let mut file = tempfile::NamedTempFile::new().expect("temp");
        file.write_all(&deflate(&plain)).expect("write");
        file.write_all(b"trailing junk").expect("write");
        let mut out = Vec::new();
        ZlibInflateScratch::default()
            .inflate_loose_file(&mut file.reopen().expect("reopen"), &mut out)
            .expect("inflate");
        assert_eq!(out, plain);
    }

    #[test]
    fn reused_scratch_decodes_consecutive_loose_files() {
        let first = loose_bytes("blob", &noise(2 * READ_CHUNK));
        let second = loose_bytes("tree", b"");
        let mut scratch = ZlibInflateScratch::default();
        for plain in [&first, &second, &first] {
            let mut out = vec![0xAA; 7];
            scratch
                .inflate_loose_file(&mut loose_file(plain).reopen().expect("reopen"), &mut out)
                .expect("inflate");
            assert_eq!(&out, plain);
        }
        let header = with_loose_scratch(|s| {
            s.inflate_loose_header(&mut loose_file(&second).reopen().expect("reopen"))
        })
        .expect("header");
        assert_eq!(header, b"tree 0\0");
    }

    #[test]
    fn inflate_loose_header_stops_at_header() {
        let plain = loose_bytes("blob", &noise(4 * READ_CHUNK));
        let header = ZlibInflateScratch::default()
            .inflate_loose_header(&mut loose_file(&plain).reopen().expect("reopen"))
            .expect("header");
        let nul = header.iter().position(|&b| b == 0).expect("nul");
        assert_eq!(
            &header[..=nul],
            format!("blob {}\0", 4 * READ_CHUNK).as_bytes()
        );
        assert!(header.len() <= LOOSE_HEADER_MAX);
    }

    #[test]
    fn inflate_loose_header_on_corrupt_file_is_zlib_error() {
        let mut file = tempfile::NamedTempFile::new().expect("temp");
        file.write_all(b"definitely not zlib").expect("write");
        let err = ZlibInflateScratch::default()
            .inflate_loose_header(&mut file.reopen().expect("reopen"))
            .unwrap_err();
        assert!(matches!(err, Error::Zlib(_)), "got {err:?}");
    }

    #[test]
    fn declared_loose_object_len_parses_only_complete_headers() {
        assert_eq!(declared_loose_object_len(b"blob 5\0hello"), Some(12));
        assert_eq!(declared_loose_object_len(b"commit 0\0"), Some(9));
        assert_eq!(declared_loose_object_len(b"blob 12"), None);
        assert_eq!(declared_loose_object_len(b"blob \0"), None);
        assert_eq!(declared_loose_object_len(b"blob x1\0"), None);
        assert_eq!(declared_loose_object_len(b"blob\0"), None);
    }
}

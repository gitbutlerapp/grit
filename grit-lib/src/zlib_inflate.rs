//! Reusable zlib inflate for pack and loose object reads.

use std::io::Read;

use flate2::{Decompress, FlushDecompress, Status};

use crate::error::{Error, Result};

const READ_CHUNK: usize = 64 * 1024;

/// Scratch space for one zlib stream decode; reset before each object.
#[doc(hidden)]
pub struct ZlibInflateScratch {
    dec: Decompress,
}

impl Default for ZlibInflateScratch {
    fn default() -> Self {
        Self {
            dec: Decompress::new(true),
        }
    }
}

impl ZlibInflateScratch {
    /// Prepare for a new zlib wrapper stream.
    pub fn reset(&mut self) {
        self.dec.reset(true);
    }

    /// Inflate a zlib stream in `bytes` starting at `*pos` to exactly `expected_size` bytes.
    ///
    /// Advances `*pos` by the number of compressed bytes consumed.
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
        if out.len() != expected {
            return Err(Error::CorruptObject(format!(
                "pack object size mismatch: expected {expected_size}, got {}",
                out.len()
            )));
        }
        *pos += consumed;
        Ok(out)
    }

    /// Inflate at most `max_out` bytes from the start of a zlib stream.
    ///
    /// Returns decompressed prefix and the number of compressed bytes consumed.
    pub fn inflate_prefix(&mut self, bytes: &[u8], max_out: usize) -> Result<(Vec<u8>, usize)> {
        if max_out == 0 {
            return self.inflate_prefix_zero(bytes);
        }
        let mut out = vec![0u8; max_out];
        let consumed = self.decompress_into(bytes, &mut out, false)?;
        out.truncate(
            self.dec
                .total_out()
                .try_into()
                .map_err(|_| Error::CorruptObject("inflate output overflow".to_owned()))?,
        );
        Ok((out, consumed))
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

    /// Decompress a zlib-wrapped loose object from `file` (after optional 2-byte prefix in `prefix`).
    pub fn decompress_loose_payload(
        &mut self,
        prefix: &[u8],
        mut file: std::fs::File,
        preset_dictionary: bool,
    ) -> Result<Vec<u8>> {
        self.reset();
        let mut pending = prefix.to_vec();
        let mut out = Vec::new();
        let mut scratch = [0u8; READ_CHUNK];
        let mut eof = false;
        loop {
            if pending.is_empty() && !eof {
                let n = file.read(&mut scratch).map_err(Error::Io)?;
                if n == 0 {
                    eof = true;
                } else {
                    pending.extend_from_slice(&scratch[..n]);
                }
            }
            let flush = if eof && pending.is_empty() {
                FlushDecompress::Finish
            } else {
                FlushDecompress::None
            };
            let before_in = self.dec.total_in();
            let before_out = self.dec.total_out();
            let mut out_chunk = [0u8; READ_CHUNK];
            let status = match self
                .dec
                .decompress(pending.as_slice(), &mut out_chunk, flush)
            {
                Ok(s) => s,
                Err(e) => {
                    if preset_dictionary {
                        return Err(Error::Zlib("needs dictionary".to_owned()));
                    }
                    return Err(Error::Zlib(e.to_string()));
                }
            };
            let consumed = (self.dec.total_in() - before_in) as usize;
            if consumed > pending.len() {
                return Err(Error::CorruptObject(
                    "zlib consumed more than pending buffer".to_owned(),
                ));
            }
            pending.drain(..consumed);
            let produced = (self.dec.total_out() - before_out) as usize;
            out.extend_from_slice(&out_chunk[..produced]);
            match status {
                Status::StreamEnd => return Ok(out),
                Status::Ok | Status::BufError => {
                    if eof && pending.is_empty() && status != Status::StreamEnd {
                        return Err(Error::Zlib("unexpected end of zlib stream".to_owned()));
                    }
                }
            }
        }
    }

    /// Advance past a zlib stream without retaining inflated bytes (for pack skipping).
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
        let mut discard = vec![0u8; expected.clamp(1, READ_CHUNK)];
        let mut remaining = expected;
        self.reset();
        let mut in_off = 0usize;
        let mut out_written = 0usize;
        loop {
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
            in_off += (self.dec.total_in() - before_in) as usize;
            out_written += (self.dec.total_out() - before_out) as usize;
            remaining = expected.saturating_sub(out_written);
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
                Status::StreamEnd => return Ok(in_off),
                Status::Ok | Status::BufError => {
                    if !require_full_output && out_pos >= out.len() {
                        return Ok(in_off);
                    }
                    if require_full_output && out_pos >= out.len() {
                        return Err(Error::CorruptObject(
                            "zlib stream larger than declared object size".to_owned(),
                        ));
                    }
                    if in_off >= input.len() && flush == FlushDecompress::Finish {
                        return Err(Error::Zlib("unexpected end of zlib stream".to_owned()));
                    }
                    if consumed == 0 && produced == 0 && in_off >= input.len() {
                        return Err(Error::Zlib("invalid zlib stream".to_owned()));
                    }
                }
            }
        }
    }
}

/// Inflate at most `max_out` bytes from a zlib stream at the start of `bytes`.
///
/// Used for header-only pack/loose reads (object info APIs).
pub(crate) fn inflate_prefix(bytes: &[u8], max_out: usize) -> Result<(Vec<u8>, usize)> {
    ZlibInflateScratch::default().inflate_prefix(bytes, max_out)
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
}

//! Pack zlib helpers for index-pack (parallel inflate path).

#![allow(dead_code)]

use crate::error::ObjectError;
use std::io::Read;

use flate2::read::ZlibDecoder;
use flate2::Compression;

use crate::error::{Error, Result};
use crate::zlib_inflate::ZlibInflateScratch;

const SKIP_BUF_LEN: usize = 64 * 1024;

/// Advance past a zlib stream without retaining decompressed bytes.
///
/// Returns the number of compressed bytes consumed from `pack` starting at `offset`.
pub(crate) fn skip_zlib_at(pack: &[u8], offset: usize, expected_size: usize) -> Result<usize> {
    let slice = pack.get(offset..).ok_or_else(|| {
        Error::Object(ObjectError::Corrupt {
            detail: format!("pack stream truncated: need zlib at offset {offset}"),
        })
    })?;
    if expected_size == 0 {
        let mut decoder = ZlibDecoder::new(slice);
        let mut out = Vec::new();
        decoder.read_to_end(&mut out).map_err(|e| {
            Error::Object(ObjectError::Zlib {
                detail: e.to_string(),
            })
        })?;
        if !out.is_empty() {
            return Err(Error::Object(ObjectError::Corrupt {
                detail: "0-byte packed object inflated to non-empty output".to_owned(),
            }));
        }
        return Ok(decoder.total_in() as usize);
    }
    let mut decoder = ZlibDecoder::new(slice);
    let mut scratch = [0u8; SKIP_BUF_LEN];
    let mut produced = 0usize;
    while produced < expected_size {
        let n = decoder.read(&mut scratch).map_err(|e| {
            Error::Object(ObjectError::Zlib {
                detail: e.to_string(),
            })
        })?;
        if n == 0 {
            break;
        }
        produced += n;
    }
    if produced != expected_size {
        return Err(Error::Object(ObjectError::Corrupt {
            detail: format!("decompressed {produced} bytes but expected {expected_size}"),
        }));
    }
    // Consume the zlib trailer (Adler-32) so `total_in` spans the full member, matching Git's
    // pack index CRC and object boundaries.
    loop {
        let n = decoder.read(&mut scratch).map_err(|e| {
            Error::Object(ObjectError::Zlib {
                detail: e.to_string(),
            })
        })?;
        if n == 0 {
            break;
        }
    }
    Ok(decoder.total_in() as usize)
}

/// Like [`skip_zlib_at`], reusing `scratch` for the inflate state machine.
pub(crate) fn skip_zlib_at_with_scratch(
    scratch: &mut ZlibInflateScratch,
    pack: &[u8],
    offset: usize,
    expected_size: usize,
) -> Result<usize> {
    let mut pos = offset;
    scratch.skip_zlib_stream(pack, &mut pos, expected_size as u64)?;
    Ok(pos - offset)
}

/// Map Git pack zlib level (`pack.compression` / `core.compression`) to flate2.
///
/// Level `0` is stored (no compression), not zlib level 1 (`Compression::fast()`).
pub(crate) fn flate2_compression_for_pack_level(level: i32) -> Compression {
    match level {
        0 => Compression::none(),
        -1 => Compression::default(),
        1..=9 => Compression::new(level as u32),
        _ => Compression::default(),
    }
}

/// Payloads at least this large are sampled by [`compression_for_payload`].
const SAMPLE_PAYLOADS_FROM: usize = 1 << 20;
/// Size of each sample [`compression_for_payload`] deflates.
const SAMPLE_LEN: usize = 32 * 1024;
/// Number of evenly spaced samples [`compression_for_payload`] deflates.
const SAMPLES: usize = 4;

/// The level to deflate `data` at: `configured`, or stored blocks
/// ([`Compression::none`]) for a large payload that deflate cannot shrink.
///
/// Already-compressed content (archives, media, encrypted files) comes out of
/// deflate no smaller than it went in, at a cost of tens of milliseconds per
/// megabyte; stored blocks give the same size at copy speed. A few evenly spaced
/// samples decide, so a payload that compresses anywhere it was sampled keeps the
/// configured level.
pub(crate) fn compression_for_payload(data: &[u8], configured: Compression) -> Compression {
    if configured.level() == 0 || data.len() < SAMPLE_PAYLOADS_FROM {
        return configured;
    }
    let mut zlib = flate2::Compress::new(configured, false);
    let mut out = vec![0u8; SAMPLE_LEN + SAMPLE_LEN / 8 + 64];
    let stride = (data.len() - SAMPLE_LEN) / (SAMPLES - 1);
    for sample in 0..SAMPLES {
        let start = sample * stride;
        zlib.reset();
        let status = zlib.compress(
            &data[start..start + SAMPLE_LEN],
            &mut out,
            flate2::FlushCompress::Finish,
        );
        if !matches!(status, Ok(flate2::Status::StreamEnd)) {
            return configured;
        }
        // Saving at least 2% anywhere is worth the configured level.
        if zlib.total_out() * 50 < zlib.total_in() * 49 {
            return configured;
        }
    }
    Compression::none()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::unpack_objects::decompress_zlib_at;
    use flate2::write::ZlibEncoder;
    use std::io::Write;

    fn zlib_compress_pack_payload(data: &[u8], level: i32) -> Result<Vec<u8>> {
        let mut enc = ZlibEncoder::new(Vec::new(), flate2_compression_for_pack_level(level));
        enc.write_all(data).map_err(|e| {
            Error::Object(ObjectError::Zlib {
                detail: e.to_string(),
            })
        })?;
        enc.finish().map_err(|e| {
            Error::Object(ObjectError::Zlib {
                detail: e.to_string(),
            })
        })
    }

    #[test]
    fn skip_zlib_at_with_scratch_matches_skip_zlib_at() {
        let plain = b"hello pack zlib skip scratch";
        let mut enc = flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::default());
        use std::io::Write;
        enc.write_all(plain).expect("zlib");
        let zlib = enc.finish().expect("finish");
        let mut pack = Vec::new();
        pack.extend_from_slice(b"PACK");
        pack.extend_from_slice(&2u32.to_be_bytes());
        pack.extend_from_slice(&1u32.to_be_bytes());
        pack.push(0x32);
        let offset = pack.len();
        pack.extend_from_slice(&zlib);
        let mut scratch = ZlibInflateScratch::default();
        let skipped = skip_zlib_at_with_scratch(&mut scratch, &pack, offset, plain.len())
            .expect("scratch skip");
        let direct = skip_zlib_at(&pack, offset, plain.len()).expect("direct skip");
        assert_eq!(skipped, direct);
    }

    #[test]
    fn pack_compression_level_zero_is_stored_not_fast() {
        let plain = b"commit 120\0tree abc\n";
        let stored = zlib_compress_pack_payload(plain, 0).expect("level 0");
        let fast = {
            let mut enc = ZlibEncoder::new(Vec::new(), Compression::fast());
            enc.write_all(plain).unwrap();
            enc.finish().unwrap()
        };
        assert_ne!(
            stored, fast,
            "pack.compression=0 must not map to Compression::fast() (level 1)"
        );
        let default_level = {
            let mut enc = ZlibEncoder::new(Vec::new(), Compression::default());
            enc.write_all(plain).unwrap();
            enc.finish().unwrap()
        };
        assert_ne!(
            stored, default_level,
            "pack.compression=0 must differ from default deflate (level 6)"
        );
    }

    #[test]
    fn skip_zlib_at_matches_decompress_for_zero_byte_object() {
        let mut pack = Vec::new();
        pack.extend_from_slice(b"PACK");
        pack.extend_from_slice(&2u32.to_be_bytes());
        pack.extend_from_slice(&1u32.to_be_bytes());
        pack.push(0x30);
        pack.extend_from_slice(&[0x78, 0x9c, 0x03, 0x00, 0x00, 0x00, 0x00, 0x01]);
        let offset = pack.len() - 8;
        let skipped = skip_zlib_at(&pack, offset, 0).expect("skip");
        let (_, consumed) = decompress_zlib_at(&pack, offset, 0).expect("decompress");
        assert_eq!(skipped, consumed);
        assert!(
            consumed > 0,
            "zlib wrapper must be consumed for 0-byte object"
        );
    }

    #[test]
    fn incompressible_payloads_are_stored() {
        let mut seed = 0x9e37_79b9_7f4a_7c15u64;
        let noise: Vec<u8> = (0..(2 << 20))
            .map(|_| {
                seed ^= seed << 13;
                seed ^= seed >> 7;
                seed ^= seed << 17;
                seed as u8
            })
            .collect();
        let level = Compression::default();
        assert_eq!(compression_for_payload(&noise, level).level(), 0);
        let stored = zlib_compress_pack_payload(&noise, 0).expect("stored");
        assert!(stored.len() < noise.len() + noise.len() / 1000);

        // Text, small payloads and an explicit level 0 keep the configured level.
        let text = b"fn main() { println!(\"hello\"); }\n".repeat(64 * 1024);
        assert_eq!(compression_for_payload(&text, level), level);
        assert_eq!(compression_for_payload(&noise[..4096], level), level);
        let none = Compression::none();
        assert_eq!(compression_for_payload(&noise, none), none);

        // Compressible anywhere sampled keeps the level, even if the rest is noise.
        let mut mixed = noise.clone();
        mixed[..64 * 1024].copy_from_slice(&text[..64 * 1024]);
        assert_eq!(compression_for_payload(&mixed, level), level);
    }
}

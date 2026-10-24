//! Pack zlib helpers for index-pack (parallel inflate path).

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
        Error::CorruptObject(format!(
            "pack stream truncated: need zlib at offset {offset}"
        ))
    })?;
    if expected_size == 0 {
        let mut decoder = ZlibDecoder::new(slice);
        let mut out = Vec::new();
        decoder
            .read_to_end(&mut out)
            .map_err(|e| Error::Zlib(e.to_string()))?;
        if !out.is_empty() {
            return Err(Error::CorruptObject(
                "0-byte packed object inflated to non-empty output".to_owned(),
            ));
        }
        return Ok(decoder.total_in() as usize);
    }
    let mut decoder = ZlibDecoder::new(slice);
    let mut scratch = [0u8; SKIP_BUF_LEN];
    let mut produced = 0usize;
    while produced < expected_size {
        let n = decoder
            .read(&mut scratch)
            .map_err(|e| Error::Zlib(e.to_string()))?;
        if n == 0 {
            break;
        }
        produced += n;
    }
    if produced != expected_size {
        return Err(Error::CorruptObject(format!(
            "decompressed {produced} bytes but expected {expected_size}"
        )));
    }
    // Consume the zlib trailer (Adler-32) so `total_in` spans the full member, matching Git's
    // pack index CRC and object boundaries.
    loop {
        let n = decoder
            .read(&mut scratch)
            .map_err(|e| Error::Zlib(e.to_string()))?;
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::unpack_objects::decompress_zlib_at;
    use flate2::write::ZlibEncoder;
    use std::io::Write;

    fn zlib_compress_pack_payload(data: &[u8], level: i32) -> Result<Vec<u8>> {
        let mut enc = ZlibEncoder::new(Vec::new(), flate2_compression_for_pack_level(level));
        enc.write_all(data)
            .map_err(|e| Error::Zlib(e.to_string()))?;
        enc.finish().map_err(|e| Error::Zlib(e.to_string()))
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
}

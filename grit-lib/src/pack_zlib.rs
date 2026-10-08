//! Pack zlib helpers for index-pack (parallel inflate path).

use std::io::Read;

use flate2::read::ZlibDecoder;

use crate::error::{Error, Result};
use crate::unpack_objects::decompress_zlib_at;

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
    loop {
        if produced >= expected_size {
            break;
        }
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
    Ok(decoder.total_in() as usize)
}

/// Decompress a zlib stream at `offset` for parallel index-pack workers.
pub(crate) fn decompress_zlib_at_index(
    pack: &[u8],
    offset: usize,
    expected_size: usize,
) -> Result<Vec<u8>> {
    decompress_zlib_at(pack, offset, expected_size).map(|(data, _)| data)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::unpack_objects::decompress_zlib_at;

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

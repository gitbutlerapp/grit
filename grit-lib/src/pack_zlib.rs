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

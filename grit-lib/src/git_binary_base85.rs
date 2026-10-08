//! Base-85 codec for `GIT binary patch` sections (matches `git/base85.c`).

use thiserror::Error;

const EN85: &[u8; 85] =
    b"0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz!#$%&()*+-;<=>?@^_`{|}~";

/// Errors returned while decoding a Git binary-patch base85 line.
#[derive(Clone, Debug, Error, PartialEq, Eq)]
pub enum DecodeError {
    /// The encoded line ended before all required base85 digits were read.
    #[error("truncated base85 line")]
    TruncatedLine,
    /// The encoded line contained a byte outside Git's base85 alphabet.
    #[error("invalid base85 alphabet byte {0}")]
    InvalidAlphabetByte(u8),
    /// The decoded base85 accumulator overflowed `u32`.
    #[error("base85 overflow")]
    Overflow,
    /// The encoded line contained extra data after the expected output length.
    #[error("trailing base85 data")]
    TrailingData,
}

fn prep_decode_table() -> [u8; 256] {
    let mut de85 = [0u8; 256];
    for (i, &c) in EN85.iter().enumerate() {
        de85[c as usize] = (i + 1) as u8;
    }
    de85
}

/// Encode bytes using Git's base-85 alphabet (matches `encode_85` in `git/base85.c`).
#[must_use]
pub fn encode(mut data: &[u8]) -> String {
    let mut result = String::new();
    while !data.is_empty() {
        let mut acc: u32 = 0;
        let mut cnt = 24i32;
        while cnt >= 0 {
            let ch = u32::from(data[0]);
            data = &data[1..];
            acc |= ch << cnt;
            if data.is_empty() {
                break;
            }
            cnt -= 8;
        }
        let mut buf = [0u8; 5];
        for i in (0..5).rev() {
            let val = (acc % 85) as usize;
            acc /= 85;
            buf[i] = EN85[val];
        }
        result.extend(buf.iter().map(|b| char::from(*b)));
    }
    result
}

/// Decode the base-85 body of one binary patch line.
///
/// `out_len` is the number of raw (compressed) bytes this line contributes.
pub fn decode_body(buffer: &[u8], mut out_len: usize) -> Result<Vec<u8>, DecodeError> {
    // hygiene: immutable base-85 decode table, initialized once
    static DE85: std::sync::OnceLock<[u8; 256]> = std::sync::OnceLock::new();
    let de85 = DE85.get_or_init(prep_decode_table);

    let mut dst = Vec::with_capacity(out_len);
    let mut pos = 0usize;

    while out_len > 0 {
        let mut acc: u32 = 0;
        for _ in 0..4 {
            let ch = *buffer.get(pos).ok_or(DecodeError::TruncatedLine)?;
            pos += 1;
            let de = de85[ch as usize];
            if de == 0 {
                return Err(DecodeError::InvalidAlphabetByte(ch));
            }
            acc = acc
                .checked_mul(85)
                .and_then(|a| a.checked_add(u32::from(de - 1)))
                .ok_or(DecodeError::Overflow)?;
        }
        let ch = *buffer.get(pos).ok_or(DecodeError::TruncatedLine)?;
        pos += 1;
        let de = de85[ch as usize];
        if de == 0 {
            return Err(DecodeError::InvalidAlphabetByte(ch));
        }
        acc = acc
            .checked_mul(85)
            .and_then(|a| a.checked_add(u32::from(de - 1)))
            .ok_or(DecodeError::Overflow)?;

        let chunk = out_len.min(4);
        out_len -= chunk;
        let mut a = acc;
        for _ in 0..chunk {
            a = a.rotate_left(8);
            dst.push(a as u8);
        }
    }

    if pos != buffer.len() {
        return Err(DecodeError::TrailingData);
    }
    Ok(dst)
}

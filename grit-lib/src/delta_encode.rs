//! Encode Git pack binary deltas (format decoded by [`crate::unpack_objects::apply_delta`]).
//!
//! Used when writing `REF_DELTA` pack objects so repositories can store similar blobs
//! compactly (see `git diff-delta` / `create_delta` in upstream Git).

use crate::error::{Error, Result};

fn write_delta_varint(out: &mut Vec<u8>, mut n: usize) {
    loop {
        let mut b = (n & 0x7f) as u8;
        n >>= 7;
        if n != 0 {
            b |= 0x80;
        }
        out.push(b);
        if n == 0 {
            break;
        }
    }
}

/// Emit one `COPY` opcode (copy `size` bytes from `offset` in the base object).
fn push_copy(out: &mut Vec<u8>, mut offset: usize, mut size: usize) -> Result<()> {
    if size == 0 {
        return Ok(());
    }
    // Git limits one COPY to 64 KiB; split larger runs.
    while size > 0 {
        let chunk = size.min(0x10000);
        let mut op = 0x80u8;
        let moff = offset;
        let msize = chunk;

        if moff & 0x0000_00ff != 0 {
            op |= 0x01;
        }
        if moff & 0x0000_ff00 != 0 {
            op |= 0x02;
        }
        if moff & 0x00ff_0000 != 0 {
            op |= 0x04;
        }
        if moff & 0xff00_0000 != 0 {
            op |= 0x08;
        }

        // 65536-byte copies omit size bytes; the decoder treats missing size as 0x10000.
        if msize != 0x10000 {
            if msize & 0x00ff != 0 {
                op |= 0x10;
            }
            if msize & 0xff00 != 0 {
                op |= 0x20;
            }
        }

        out.push(op);
        if op & 0x01 != 0 {
            out.push(moff as u8);
        }
        if op & 0x02 != 0 {
            out.push((moff >> 8) as u8);
        }
        if op & 0x04 != 0 {
            out.push((moff >> 16) as u8);
        }
        if op & 0x08 != 0 {
            out.push((moff >> 24) as u8);
        }
        if msize != 0x10000 {
            if op & 0x10 != 0 {
                out.push(msize as u8);
            }
            if op & 0x20 != 0 {
                out.push((msize >> 8) as u8);
            }
        }

        offset = offset.saturating_add(chunk);
        size -= chunk;
    }
    Ok(())
}

fn push_insert(out: &mut Vec<u8>, mut data: &[u8]) {
    while !data.is_empty() {
        let n = data.len().min(127);
        out.push(n as u8);
        out.extend_from_slice(&data[..n]);
        data = &data[n..];
    }
}

/// Build a delta when `target` begins with the entire `base` buffer (strict extension).
///
/// This matches the common pack-objects case where one blob is a suffix of another.
pub fn encode_prefix_extension_delta(base: &[u8], target: &[u8]) -> Result<Vec<u8>> {
    if !target.starts_with(base) || target.len() <= base.len() {
        return Err(Error::CorruptObject(
            "encode_prefix_extension_delta: target must strictly extend base".into(),
        ));
    }
    let mut out = Vec::new();
    write_delta_varint(&mut out, base.len());
    write_delta_varint(&mut out, target.len());
    push_copy(&mut out, 0, base.len())?;
    push_insert(&mut out, &target[base.len()..]);
    Ok(out)
}

/// Encode a binary delta using the longest common prefix of `base` and `target`.
///
/// The reconstructed object is `target`; this matches Git's thin-pack deltas when successive
/// versions share a long prefix but are not strict extensions (e.g. `…\\n9` vs `…\\n10`).
pub fn encode_lcp_delta(base: &[u8], target: &[u8]) -> Result<Vec<u8>> {
    let lcp = base
        .iter()
        .zip(target.iter())
        .take_while(|(a, b)| a == b)
        .count();
    let mut out = Vec::new();
    write_delta_varint(&mut out, base.len());
    write_delta_varint(&mut out, target.len());
    push_copy(&mut out, 0, lcp)?;
    push_insert(&mut out, &target[lcp..]);
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::unpack_objects::apply_delta;

    #[test]
    fn roundtrip_prefix_delta() {
        let base = b"hello world".repeat(100);
        let mut target = base.clone();
        target.extend_from_slice(b"\nextra suffix\n");
        let delta = encode_prefix_extension_delta(&base, &target).unwrap();
        let got = apply_delta(&base, &delta).unwrap();
        assert_eq!(got, target);
    }

    #[test]
    fn roundtrip_lcp_delta() {
        let base = b"padding\n9";
        let target = b"padding\n10";
        let delta = encode_lcp_delta(base, target).unwrap();
        let got = apply_delta(base, &delta).unwrap();
        assert_eq!(got, target);
    }

    #[test]
    fn prefix_extension_requires_strict_suffix() {
        let base = b"same";
        let err = encode_prefix_extension_delta(base, base).unwrap_err();
        assert!(matches!(err, Error::CorruptObject(_)));
    }

    #[test]
    fn prefix_extension_splits_large_copy_runs() {
        let base = vec![0xABu8; 70_000];
        let mut target = base.clone();
        target.push(b'!');
        let delta = encode_prefix_extension_delta(&base, &target).unwrap();
        let got = apply_delta(&base, &delta).unwrap();
        assert_eq!(got, target);
    }
}

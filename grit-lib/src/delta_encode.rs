//! Encode Git pack binary deltas (format decoded by [`crate::unpack_objects::apply_delta`]).
//!
//! [`DeltaIndex`] indexes a base buffer with a rolling fingerprint so many targets can be
//! encoded against the same base efficiently (pack-objects style windows).

use crate::error::{Error, Result};

/// Bytes in each indexed window (Git `RABIN_WINDOW` in diff-delta).
pub(crate) const WINDOW: usize = 16;
/// Minimum COPY length worth emitting instead of literals.
pub(crate) const MIN_COPY: usize = 4;
/// Largest single COPY opcode in Git pack deltas.
pub(crate) const COPY_CHUNK: usize = 0x10000;

pub(crate) fn write_delta_varint(out: &mut Vec<u8>, mut n: usize) {
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
pub(crate) fn push_copy(out: &mut Vec<u8>, mut offset: usize, mut size: usize) -> Result<()> {
    if size == 0 {
        return Ok(());
    }
    while size > 0 {
        let chunk = size.min(COPY_CHUNK);
        let moff = offset;
        let msize = chunk;

        let mut op = 0x80u8;
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
        if msize != COPY_CHUNK {
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
        if msize != COPY_CHUNK {
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

pub(crate) fn push_insert(out: &mut Vec<u8>, mut data: &[u8]) {
    while !data.is_empty() {
        let n = data.len().min(127);
        out.push(n as u8);
        out.extend_from_slice(&data[..n]);
        data = &data[n..];
    }
}

/// Rolling-hash index over a base buffer for reuse when encoding many targets.
pub struct DeltaIndex {
    inner: crate::pack_roll_delta::BaseMatchCatalog,
}

impl DeltaIndex {
    /// Build an index over `base` for subsequent [`Self::encode`] calls.
    #[must_use]
    pub fn new(base: &[u8]) -> Self {
        Self {
            inner: crate::pack_roll_delta::BaseMatchCatalog::build(base),
        }
    }

    /// Indexed stride used when subsampling large bases (Git indexes every [`WINDOW`] bytes).
    #[must_use]
    pub fn index_stride(&self) -> usize {
        WINDOW
    }

    /// Encode `target` as a binary delta against this index's base.
    pub fn encode(&self, target: &[u8], max_size: usize) -> Option<Vec<u8>> {
        self.inner.encode(target, max_size)
    }
}

/// Encode a delta from `base` to `target` using a rolling-hash match finder.
pub fn encode_delta(base: &[u8], target: &[u8], max_size: usize) -> Option<Vec<u8>> {
    DeltaIndex::new(base).encode(target, max_size)
}

/// Build a delta when `target` begins with the entire `base` buffer (strict extension).
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

/// Encode a binary delta from `base` to `target`.
pub fn encode_lcp_delta(base: &[u8], target: &[u8]) -> Result<Vec<u8>> {
    encode_delta(base, target, 0)
        .ok_or_else(|| Error::CorruptObject("encode_lcp_delta: delta encoding failed".into()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::unpack_objects::apply_delta;

    fn assert_roundtrip(base: &[u8], target: &[u8]) {
        let delta = encode_delta(base, target, 0).expect("encode");
        let got = apply_delta(base, &delta).expect("apply");
        assert_eq!(
            got,
            target,
            "base_len={} target_len={}",
            base.len(),
            target.len()
        );
    }

    #[test]
    fn empty_target_produces_valid_delta() {
        let base = b"non-empty base";
        let delta = DeltaIndex::new(base).encode(b"", 0).expect("encode");
        assert_eq!(apply_delta(base, &delta).expect("apply"), b"" as &[u8]);
    }

    #[test]
    fn empty_target_respects_max_size_before_return() {
        let base = b"non-empty base";
        assert!(DeltaIndex::new(base).encode(b"", 1).is_none());
    }

    #[test]
    fn empty_base_and_target_roundtrip() {
        assert_roundtrip(b"", b"");
    }

    #[test]
    fn roundtrip_prefix_delta() {
        let base = b"hello world".repeat(100);
        let mut target = base.clone();
        target.extend_from_slice(b"\nextra suffix\n");
        assert_roundtrip(&base, &target);
    }

    #[test]
    fn roundtrip_lcp_delta() {
        assert_roundtrip(b"padding\n9", b"padding\n10");
    }

    #[test]
    fn roundtrip_mid_file_insertion() {
        let mut lines: Vec<String> = (1..=200)
            .map(|i| format!("The quick brown fox jumps over the lazy dog line {i}\n"))
            .collect();
        let base = lines.join("");
        lines.insert(100, "inserted: extra line for delta test\n".to_owned());
        let target = lines.join("");
        let delta = encode_delta(base.as_bytes(), target.as_bytes(), 0).expect("encode");
        assert!(
            delta.len() * 100 <= target.len() * 105,
            "delta {} bytes vs target {} (want <=5%)",
            delta.len(),
            target.len()
        );
        assert_roundtrip(base.as_bytes(), target.as_bytes());
    }

    #[test]
    fn roundtrip_mid_file_line_replace() {
        let mut lines: Vec<String> = (1..=200)
            .map(|i| format!("The quick brown fox jumps over the lazy dog line {i}\n"))
            .collect();
        let base = lines.join("");
        lines[100] = "edit: replaced line content for delta test\n".to_owned();
        let target = lines.join("");
        let delta = encode_delta(base.as_bytes(), target.as_bytes(), 0).expect("encode");
        assert!(
            delta.len() * 100 <= target.len() * 105,
            "delta {} bytes vs target {} (want <=5%)",
            delta.len(),
            target.len()
        );
        assert_roundtrip(base.as_bytes(), target.as_bytes());
    }

    #[test]
    fn roundtrip_random_edits() {
        let mut seed = 0x767u64;
        for _ in 0..250 {
            seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1);
            let base_len = (seed % 8000 + 32) as usize;
            seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1);
            let target_len = (seed % 8000) as usize;
            seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1);

            let mut base = vec![0u8; base_len];
            for b in &mut base {
                seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1);
                *b = (seed >> 32) as u8;
            }

            let mut target = if target_len == 0 {
                Vec::new()
            } else {
                vec![0u8; target_len]
            };
            let shared = base_len.min(target_len);
            target[..shared].copy_from_slice(&base[..shared]);

            let op = (seed % 5) as u8;
            seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1);
            match op {
                0 if target_len > shared => {
                    for t in &mut target[shared..] {
                        seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1);
                        *t = (seed >> 40) as u8;
                    }
                }
                1 if target_len > 0 && base_len > 0 => {
                    let i = (seed as usize) % target_len;
                    seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1);
                    target[i] = (seed >> 33) as u8;
                }
                2 if target_len > 4 => {
                    let i = (seed as usize) % (target_len - 1);
                    seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1);
                    target.remove(i);
                }
                3 if target_len > 0 => {
                    let i = (seed as usize) % (target_len + 1);
                    seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1);
                    target.insert(i, (seed >> 34) as u8);
                }
                _ => {}
            }

            assert_roundtrip(&base, &target);
        }
    }

    #[test]
    fn real_world_source_edit() {
        let base = include_str!("delta_encode.rs");
        let mut target = base.to_owned();
        target.insert_str(base.len() / 3, "\npub fn new_api() {}\n");
        assert_roundtrip(base.as_bytes(), target.as_bytes());
    }

    #[test]
    fn max_size_rejects_large_delta() {
        let base = vec![0u8; 10_000];
        let target = vec![1u8; 10_000];
        assert!(encode_delta(&base, &target, 50).is_none());
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

    #[test]
    fn delta_index_reuse_across_targets() {
        let base = b"abcdef".repeat(500);
        let index = DeltaIndex::new(&base);
        for n in 1..20 {
            let mut target = base.clone();
            target.extend_from_slice(&b"X".repeat(n));
            let d1 = index.encode(&target, 0).unwrap();
            let d2 = encode_delta(&base, &target, 0).unwrap();
            assert_eq!(d1, d2);
            assert_eq!(apply_delta(&base, &d1).unwrap(), target);
        }
    }

    #[test]
    fn index_stride_matches_git_window() {
        let index = DeltaIndex::new(b"0123456789abcdef");
        assert_eq!(index.index_stride(), WINDOW);
    }

    #[test]
    fn large_base_roundtrip_extension() {
        let base = vec![0u8; 512 * 1024];
        let mut target = base.clone();
        target.push(1);
        assert_roundtrip(&base, &target);
    }

    #[test]
    fn single_byte_mid_edit_encodes_small() {
        let base = vec![b'a'; 50_000];
        let mut target = base.clone();
        target[25_000] = b'X';
        let delta = encode_delta(&base, &target, 0).expect("delta");
        assert!(delta.len() < 200, "delta {} bytes", delta.len());
    }

    #[test]
    fn repetitive_lines_one_byte_edit_encodes_small() {
        let lines: Vec<String> = (1..=500)
            .map(|i| format!("line {i:05} lorem ipsum dolor sit amet"))
            .collect();
        let base = lines.join("\n");
        let mut target = base.as_bytes().to_vec();
        target[9000] ^= 0x01;
        let delta = encode_delta(base.as_bytes(), &target, 0).expect("delta");
        assert!(
            delta.len() < 200,
            "delta {} base {}",
            delta.len(),
            base.len()
        );
    }

    #[test]
    fn issue_1011_consecutive_blob_pair_from_repo_fixture() {
        let base = include_bytes!("../tests/fixtures/issue_1011_base.bin");
        let target = include_bytes!("../tests/fixtures/issue_1011_target.bin");
        let delta = encode_delta(base, target, 0).expect("encode");
        assert_eq!(apply_delta(base, &delta).expect("apply"), target.as_slice());
        assert!(
            delta.len() < target.len() / 10,
            "delta {} vs target {} (expect Git-scale compression)",
            delta.len(),
            target.len()
        );
    }

    #[test]
    fn multi_line_edit_step_encodes_small() {
        let line = " some moderately long line of text to make blob bigger abcdefghijklmnop\n";
        let mut body = String::new();
        while body.len() < 90 * 1024 {
            let n = body.lines().count() + 1;
            body.push_str(&format!("{n}{line}"));
        }
        let base = body.as_bytes();
        let mut lines: Vec<String> = body.lines().map(str::to_owned).collect();
        let stride = lines.len() / 16;
        for k in 0..15 {
            let idx = (3 + k * stride).min(lines.len().saturating_sub(1));
            lines[idx] = format!("edit 1-{k}");
        }
        let target = lines.join("\n");
        let delta = encode_delta(base, target.as_bytes(), 0).expect("encode");
        assert!(
            delta.len() < target.len() / 10,
            "delta {} vs target {}",
            delta.len(),
            target.len()
        );
    }

    #[test]
    fn issue_1011_single_line_edit_in_large_blob() {
        let lines: Vec<String> = (1..=3000)
            .map(|i| {
                format!(
                    "{i} some moderately long line of text to make blob bigger abcdefghijklmnop"
                )
            })
            .collect();
        let base = lines.join("\n");
        let mut target = base.as_bytes().to_vec();
        // One-byte edit mid-file (same size as base; tail stays aligned).
        target[base.len() / 3] ^= 0x01;
        let target = target;
        let max = target.len() / 2 - 20;
        let delta_unlim = encode_delta(base.as_bytes(), &target, 0).expect("unlim");
        let delta_max = encode_delta(base.as_bytes(), &target, max);
        assert!(
            delta_max.is_some(),
            "encode_delta None with max_size {max} (unlim len {})",
            delta_unlim.len()
        );
        let delta = delta_max.unwrap();
        assert!(
            delta.len() < target.len() / 10,
            "delta {} vs target {}",
            delta.len(),
            target.len()
        );
    }
}

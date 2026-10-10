//! Encode Git pack binary deltas (format decoded by [`crate::unpack_objects::apply_delta`]).
//!
//! [`DeltaIndex`] indexes a base buffer with a rolling fingerprint so many targets can be
//! encoded against the same base efficiently (pack-objects style windows).

use std::collections::HashMap;
use std::sync::Arc;

use crate::error::{Error, Result};

/// Bytes in each indexed window (fixed block size for fingerprinting).
const WINDOW: usize = 12;
/// Minimum COPY length worth emitting instead of literals.
const MIN_COPY: usize = 4;
/// Maximum index entries sharing one fingerprint bucket (bounds worst-case match work).
const MAX_SLOTS_PER_KEY: usize = 48;
/// Ignore hash collisions whose base offset is far from the current target offset.
const MAX_CANDIDATE_SKEW: usize = 16_384;
/// Cap on total indexed window positions across the whole base (large blobs subsample).
const MAX_INDEX_TOTAL: usize = 262_144;
/// Largest single COPY opcode in Git pack deltas.
const COPY_CHUNK: usize = 0x10000;

const FINGERPRINT_SEED: u32 = 0x9e37_79b9;
const FINGERPRINT_STEP: u32 = 0x85eb_ca6b;
const OUTGOING_WEIGHT: u32 = fingerprint_pow(WINDOW - 1);

const fn fingerprint_pow(exp: usize) -> u32 {
    let mut p = 1u32;
    let mut i = 0;
    while i < exp {
        p = p.wrapping_mul(FINGERPRINT_STEP);
        i += 1;
    }
    p
}

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

fn push_insert(out: &mut Vec<u8>, mut data: &[u8]) {
    while !data.is_empty() {
        let n = data.len().min(127);
        out.push(n as u8);
        out.extend_from_slice(&data[..n]);
        data = &data[n..];
    }
}

/// Fingerprint of `slice` (must be at least [`WINDOW`] bytes).
#[inline]
fn fingerprint(slice: &[u8]) -> u32 {
    debug_assert!(slice.len() >= WINDOW);
    let mut h = FINGERPRINT_SEED;
    for &b in &slice[..WINDOW] {
        h = h.wrapping_mul(FINGERPRINT_STEP).wrapping_add(u32::from(b));
    }
    h
}

#[inline]
fn roll_fingerprint(prev: u32, outgoing: u8, incoming: u8) -> u32 {
    prev.wrapping_sub(u32::from(outgoing).wrapping_mul(OUTGOING_WEIGHT))
        .wrapping_mul(FINGERPRINT_STEP)
        .wrapping_add(u32::from(incoming))
}

/// Rolling-hash index over a base buffer for reuse when encoding many targets.
pub struct DeltaIndex {
    base: Arc<[u8]>,
    /// Fingerprint value → base offsets where that window begins.
    slots: HashMap<u32, Vec<usize>>,
    /// Distance between indexed window starts (1 indexes every position).
    index_stride: usize,
}

impl DeltaIndex {
    fn push_slot(slots: &mut HashMap<u32, Vec<usize>>, key: u32, start: usize) {
        let list = slots.entry(key).or_default();
        if list.len() >= MAX_SLOTS_PER_KEY {
            list.remove(0);
        }
        list.push(start);
    }

    fn build_index(base: &[u8]) -> (HashMap<u32, Vec<usize>>, usize) {
        let mut slots: HashMap<u32, Vec<usize>> = HashMap::new();
        if base.len() < WINDOW {
            return (slots, 1);
        }
        let last_start = base.len() - WINDOW;
        let window_count = last_start + 1;
        let stride = if window_count <= MAX_INDEX_TOTAL {
            1
        } else {
            window_count.div_ceil(MAX_INDEX_TOTAL)
        };
        slots.reserve(window_count.min(MAX_INDEX_TOTAL) / 4);
        let mut total = 0usize;
        let mut h = fingerprint(&base[..WINDOW]);
        Self::push_slot(&mut slots, h, 0);
        total += 1;

        for start in 1..=last_start {
            h = roll_fingerprint(h, base[start - 1], base[start + WINDOW - 1]);
            if start % stride != 0 {
                continue;
            }
            if total >= MAX_INDEX_TOTAL {
                continue;
            }
            Self::push_slot(&mut slots, h, start);
            total += 1;
        }
        (slots, stride)
    }

    /// Build an index over `base` for subsequent [`Self::encode`] calls.
    #[must_use]
    pub fn new(base: &[u8]) -> Self {
        let base: Arc<[u8]> = Arc::from(base);
        let (slots, index_stride) = Self::build_index(&base);
        Self {
            base,
            slots,
            index_stride,
        }
    }

    /// Indexed stride used when subsampling large bases (1 means every window).
    #[must_use]
    pub fn index_stride(&self) -> usize {
        self.index_stride
    }

    fn extend_match(&self, base_off: usize, target: &[u8], target_off: usize) -> usize {
        let base = &self.base;
        let max = (base.len() - base_off).min(target.len() - target_off);
        let mut len = 0usize;
        while len + 8 <= max {
            if base[base_off + len..base_off + len + 8]
                != target[target_off + len..target_off + len + 8]
            {
                break;
            }
            len += 8;
        }
        while len < max && base[base_off + len] == target[target_off + len] {
            len += 1;
        }
        len
    }

    fn consider_match(
        &self,
        target: &[u8],
        target_off: usize,
        base_off: usize,
        best: &mut (usize, usize, usize),
    ) {
        if base_off >= self.base.len() {
            return;
        }
        let len = self.extend_match(base_off, target, target_off);
        if len < MIN_COPY {
            return;
        }
        let dist = base_off.abs_diff(target_off);
        let (best_len, best_off, best_dist) = best;
        let better = len > *best_len || (len == *best_len && dist < *best_dist);
        if better {
            *best_len = len;
            *best_off = base_off;
            *best_dist = dist;
        }
    }

    fn best_match(
        &self,
        target: &[u8],
        target_off: usize,
        roll_key: Option<u32>,
    ) -> Option<(usize, usize)> {
        if self.base.is_empty() || target_off >= target.len() {
            return None;
        }
        let remaining = target.len() - target_off;
        if remaining < MIN_COPY {
            return None;
        }

        let diverged = target_off < self.base.len() && target[target_off] != self.base[target_off];
        let mut best = (0usize, 0usize, usize::MAX);

        if !diverged && target_off < self.base.len() {
            let aligned_len = self.extend_match(target_off, target, target_off);
            if aligned_len >= 64 {
                return Some((target_off, aligned_len));
            }
            if aligned_len >= MIN_COPY {
                self.consider_match(target, target_off, target_off, &mut best);
            }
        }

        if remaining >= WINDOW {
            let key = roll_key.unwrap_or_else(|| fingerprint(&target[target_off..]));
            if let Some(candidates) = self.slots.get(&key) {
                let mut tried = 0usize;
                for &base_off in candidates {
                    if base_off.abs_diff(target_off) > MAX_CANDIDATE_SKEW {
                        continue;
                    }
                    self.consider_match(target, target_off, base_off, &mut best);
                    tried += 1;
                    if best.0 >= remaining {
                        return Some((best.1, best.0));
                    }
                    if diverged && tried >= 4 {
                        break;
                    }
                }
            }
        }

        let (best_len, best_off, _) = best;
        (best_len >= MIN_COPY).then_some((best_off, best_len))
    }

    fn write_header(out: &mut Vec<u8>, base_len: usize, target_len: usize) {
        write_delta_varint(out, base_len);
        write_delta_varint(out, target_len);
    }

    fn would_exceed_max(out_len: usize, max_size: usize) -> bool {
        max_size > 0 && out_len > max_size
    }

    fn flush_pending_literals(
        out: &mut Vec<u8>,
        pending_literals: &mut Vec<u8>,
        max_size: usize,
    ) -> Option<()> {
        if pending_literals.is_empty() {
            return Some(());
        }
        push_insert(out, pending_literals);
        pending_literals.clear();
        if Self::would_exceed_max(out.len(), max_size) {
            return None;
        }
        Some(())
    }

    /// Encode `target` as a binary delta against this index's base.
    ///
    /// Returns `None` when the encoded delta would exceed `max_size` (when `max_size > 0`).
    /// When `max_size` is zero, no size limit is applied.
    ///
    /// An empty `target` yields a valid delta with result size zero (header varints only).
    pub fn encode(&self, target: &[u8], max_size: usize) -> Option<Vec<u8>> {
        let mut out = Vec::with_capacity(64 + target.len() / 4);
        Self::write_header(&mut out, self.base.len(), target.len());
        if Self::would_exceed_max(out.len(), max_size) {
            return None;
        }
        if target.is_empty() {
            return Some(out);
        }

        let mut target_off = 0usize;
        let mut pending_literals: Vec<u8> = Vec::new();
        let mut roll: Option<u32> = None;

        while target_off < target.len() {
            let remaining = target.len() - target_off;
            if remaining >= WINDOW && roll.is_none() {
                roll = Some(fingerprint(&target[target_off..]));
            }

            let diverged =
                target_off < self.base.len() && target[target_off] != self.base[target_off];
            let probe_match =
                !diverged || target_off.is_multiple_of(32) || pending_literals.len() >= 127;

            if probe_match {
                if let Some((mut base_off, mut match_len)) =
                    self.best_match(target, target_off, roll)
                {
                    roll = None;
                    while base_off > 0
                        && target_off > 0
                        && self.base[base_off - 1] == target[target_off - 1]
                    {
                        base_off -= 1;
                        target_off -= 1;
                        match_len += 1;
                        while pending_literals.pop().is_some() {}
                    }
                    Self::flush_pending_literals(&mut out, &mut pending_literals, max_size)?;
                    while match_len > 0 {
                        let chunk = match_len.min(COPY_CHUNK);
                        push_copy(&mut out, base_off, chunk).ok()?;
                        base_off += chunk;
                        target_off += chunk;
                        match_len -= chunk;
                        if Self::would_exceed_max(out.len(), max_size) {
                            return None;
                        }
                    }
                    continue;
                }
            }

            pending_literals.push(target[target_off]);
            if target_off + WINDOW < target.len() {
                roll = Some(match roll {
                    Some(h) => roll_fingerprint(h, target[target_off], target[target_off + WINDOW]),
                    None => fingerprint(&target[target_off..]),
                });
            } else {
                roll = None;
            }
            target_off += 1;
            if pending_literals.len() >= 127 {
                Self::flush_pending_literals(&mut out, &mut pending_literals, max_size)?;
            }
        }

        Self::flush_pending_literals(&mut out, &mut pending_literals, max_size)?;
        Some(out)
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
        let base = b"The quick brown fox jumps over the lazy dog.\n".repeat(200);
        let insert_at = base.len() / 2;
        let mut target = base.clone();
        target.splice(
            insert_at..insert_at,
            b"\n// inserted line for test\n".repeat(20),
        );
        let delta = encode_delta(&base, &target, 0).expect("encode");
        assert!(
            delta.len() * 100 <= target.len() * 105,
            "delta {} bytes vs target {} (want <=5%)",
            delta.len(),
            target.len()
        );
        assert_roundtrip(&base, &target);
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
    fn index_populated_for_nonempty_base() {
        let index = DeltaIndex::new(b"0123456789abcdef");
        assert!(!index.slots.is_empty());
    }

    #[test]
    fn large_base_uses_index_stride() {
        let base = vec![0u8; MAX_INDEX_TOTAL * WINDOW * 2];
        let index = DeltaIndex::new(&base);
        assert!(index.index_stride() > 1);
        let mut target = base.clone();
        target.push(1);
        assert_roundtrip(&base, &target);
    }
}

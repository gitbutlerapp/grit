//! Git-compatible Rabin delta index and encoder (diff-delta algorithm).

use std::sync::Arc;

use crate::delta_encode::{push_copy, push_insert, write_delta_varint, MIN_COPY, WINDOW};

#[path = "delta_rabin_tables.rs"]
pub(crate) mod tables;
use tables::{T, U};

const RABIN_SHIFT: u32 = 23;
const HASH_LIMIT: usize = 64;

#[derive(Clone, Copy)]
struct IndexEntry {
    val: u32,
    /// Base offset where a rolling-hash match may begin extending forward.
    offset: usize,
}

/// Git-style delta index over a base buffer.
pub(crate) struct RabinDeltaIndex {
    base: Arc<[u8]>,
    hash_mask: u32,
    /// `bucket_starts[i]..bucket_starts[i+1]` indexes into `entries` for hash bucket `i`.
    bucket_starts: Vec<usize>,
    entries: Vec<IndexEntry>,
}

impl RabinDeltaIndex {
    fn hash_at(base: &[u8], start: usize) -> u32 {
        let mut val = 0u32;
        for i in 1..=WINDOW {
            val = ((val << 8) | u32::from(base[start + i])) ^ T[(val >> RABIN_SHIFT) as usize];
        }
        val
    }

    #[must_use]
    pub fn new(base: &[u8]) -> Self {
        let base: Arc<[u8]> = Arc::from(base);
        if base.len() < WINDOW + 1 {
            return Self {
                base,
                hash_mask: 0,
                bucket_starts: vec![0, 0],
                entries: Vec::new(),
            };
        }

        let bufsize = base.len();
        let mut entries_count = (bufsize - 1) / WINDOW;
        let hsize = {
            let hs = (entries_count / 4).max(1);
            let mut pow = 4usize;
            while (1usize << pow) < hs {
                pow += 1;
            }
            1usize << pow
        };
        let hmask = (hsize - 1) as u32;

        let mut buckets: Vec<Vec<IndexEntry>> = vec![Vec::new(); hsize];
        let mut prev_val = !0u32;
        let start_data = entries_count * WINDOW - WINDOW;
        let mut data = start_data;
        loop {
            let val = Self::hash_at(&base, data);
            if val == prev_val {
                if let Some(last) = buckets[(val & hmask) as usize].last_mut() {
                    last.offset = data + WINDOW;
                }
                entries_count -= 1;
            } else {
                prev_val = val;
                buckets[(val & hmask) as usize].push(IndexEntry {
                    val,
                    offset: data + WINDOW,
                });
            }
            if data == 0 {
                break;
            }
            data = data.saturating_sub(WINDOW);
        }

        for bucket in &mut buckets {
            if bucket.len() <= HASH_LIMIT {
                continue;
            }
            let n = bucket.len();
            let step = n.div_ceil(HASH_LIMIT);
            let trimmed: Vec<IndexEntry> = bucket.iter().step_by(step).copied().collect();
            entries_count -= n - trimmed.len();
            *bucket = trimmed;
        }

        let mut packed = Vec::with_capacity(entries_count);
        let mut bucket_starts = Vec::with_capacity(hsize + 1);
        for bucket in buckets {
            bucket_starts.push(packed.len());
            packed.extend_from_slice(&bucket);
        }
        bucket_starts.push(packed.len());

        Self {
            base,
            hash_mask: hmask,
            bucket_starts,
            entries: packed,
        }
    }

    fn bucket_entries(&self, hash_key: u32) -> &[IndexEntry] {
        let i = (hash_key & self.hash_mask) as usize;
        let start = self.bucket_starts[i];
        let end = self.bucket_starts[i + 1];
        &self.entries[start..end]
    }

    /// Longest copy match at `target_off` using Git rolling hash `val`.
    pub fn longest_match(
        &self,
        target: &[u8],
        target_off: usize,
        val: u32,
        min_len: usize,
    ) -> (usize, usize) {
        let ref_data = &self.base;
        let ref_top = ref_data.len();
        let top = target.len();
        let mut moff = 0usize;
        let mut msize = min_len;
        for entry in self.bucket_entries(val) {
            if entry.val != val {
                continue;
            }
            let mut ref_pos = entry.offset;
            if ref_pos >= ref_top {
                continue;
            }
            let mut src = target_off;
            let mut ref_size = ref_top - ref_pos;
            if ref_size > top - src {
                ref_size = top - src;
            }
            if ref_size <= msize {
                break;
            }
            while ref_size > 0 && target[src] == ref_data[ref_pos] {
                ref_pos += 1;
                src += 1;
                ref_size -= 1;
            }
            let match_len = ref_pos - entry.offset;
            if match_len > msize {
                msize = match_len;
                moff = entry.offset;
                if msize >= 4096 {
                    break;
                }
            }
        }
        (moff, msize)
    }

    fn reinit_val(target: &[u8], data_idx: usize) -> u32 {
        let mut val = 0u32;
        for j in 0..WINDOW {
            val = ((val << 8) | u32::from(target[data_idx - WINDOW + j]))
                ^ T[(val >> RABIN_SHIFT) as usize];
        }
        val
    }

    /// Encode `target` against this base (Git `create_delta` match finder + grit op encoding).
    pub fn encode(&self, target: &[u8], max_size: usize) -> Option<Vec<u8>> {
        let mut out = Vec::with_capacity(8192.min(target.len() / 4 + 64));
        write_delta_varint(&mut out, self.base.len());
        write_delta_varint(&mut out, target.len());
        if target.is_empty() {
            return Some(out);
        }
        if max_size > 0 && out.len() > max_size {
            return None;
        }

        let ref_data = &self.base;
        let top = target.len();
        let mut data_idx = 0usize;
        let mut val = 0u32;
        let mut pending: Vec<u8> = Vec::new();
        let mut msize = 0usize;
        let mut moff = 0usize;

        // Match Git `create_delta`: prime Rabin state from the first window bytes kept as literals.
        let prime_count = WINDOW.min(top);
        if prime_count > 0 {
            pending.extend_from_slice(&target[..prime_count]);
            for &byte in &target[..prime_count] {
                val = ((val << 8) | u32::from(byte)) ^ T[(val >> RABIN_SHIFT) as usize];
            }
            data_idx = prime_count;
        }

        while data_idx < top {
            if msize < 4096 {
                val ^= U[target[data_idx - WINDOW] as usize];
                val = ((val << 8) | u32::from(target[data_idx])) ^ T[(val >> RABIN_SHIFT) as usize];
                let (off, len) = self.longest_match(target, data_idx, val, msize);
                if len > msize {
                    moff = off;
                    msize = len;
                }
            }

            if msize < MIN_COPY {
                pending.push(target[data_idx]);
                data_idx += 1;
                msize = 0;
                if pending.len() >= 127 {
                    push_insert(&mut out, &pending);
                    pending.clear();
                    if max_size > 0 && out.len() > max_size {
                        return None;
                    }
                }
                continue;
            }

            while moff > 0
                && data_idx > 0
                && !pending.is_empty()
                && ref_data[moff - 1] == target[data_idx - 1]
            {
                moff -= 1;
                data_idx -= 1;
                pending.pop();
                msize += 1;
            }
            if msize > top - data_idx {
                msize = top - data_idx;
            }
            if moff + msize > ref_data.len() {
                msize = ref_data.len().saturating_sub(moff);
            }
            if msize < MIN_COPY {
                msize = 0;
                pending.push(target[data_idx]);
                data_idx += 1;
                if pending.len() >= 127 {
                    push_insert(&mut out, &pending);
                    pending.clear();
                    if max_size > 0 && out.len() > max_size {
                        return None;
                    }
                }
                continue;
            }

            if !pending.is_empty() {
                push_insert(&mut out, &pending);
                pending.clear();
                if max_size > 0 && out.len() > max_size {
                    return None;
                }
            }

            let emit = if msize >= 0x10000 { 0x10000 } else { msize };
            push_copy(&mut out, moff, emit).ok()?;
            moff += emit;
            data_idx += emit;
            msize -= emit;
            if max_size > 0 && out.len() > max_size {
                return None;
            }
            if msize < 4096 && data_idx >= WINDOW {
                val = Self::reinit_val(target, data_idx);
            }
        }

        if !pending.is_empty() {
            push_insert(&mut out, &pending);
            if max_size > 0 && out.len() > max_size {
                return None;
            }
        }
        Some(out)
    }
}

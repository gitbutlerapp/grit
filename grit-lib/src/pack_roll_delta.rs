//! Rabin-window match catalog and pack delta encoder (Git pack delta format).

use std::sync::Arc;

use crate::delta_encode::{push_copy, write_delta_varint, MIN_COPY, WINDOW};

#[path = "roll_hash_tables.rs"]
mod roll_tables;
use roll_tables::{T, U};

const FINGERPRINT_SHIFT: u32 = 23;
const BUCKET_CAP: usize = 64;
const COPY_GOOD_ENOUGH: usize = 4096;
const MAX_INSERT_RUN: u8 = 0x7f;
const MAX_COPY_SLICE: usize = 0x10000;

#[derive(Clone, Copy, Debug)]
struct WindowAnchor {
    fingerprint: u32,
    copy_start: usize,
}

/// Precomputed window fingerprints over a base object for delta encoding.
pub(crate) struct BaseMatchCatalog {
    base: Arc<[u8]>,
    bucket_mask: u32,
    bucket_ranges: Vec<usize>,
    anchors: Vec<WindowAnchor>,
}

impl BaseMatchCatalog {
    fn fingerprint_at(base: &[u8], window_start: usize) -> u32 {
        let mut fp = 0u32;
        for i in 1..=WINDOW {
            fp = ((fp << 8) | u32::from(base[window_start + i]))
                ^ T[(fp >> FINGERPRINT_SHIFT) as usize];
        }
        fp
    }

    fn table_slots(window_count: usize) -> usize {
        let target = (window_count / 4).max(1);
        let mut bits = 4usize;
        while (1usize << bits) < target {
            bits += 1;
        }
        1usize << bits
    }

    fn subsample_bucket(list: &mut Vec<WindowAnchor>, cap: usize) {
        if list.len() <= cap {
            return;
        }
        let step = list.len().div_ceil(cap);
        *list = list.iter().step_by(step).copied().collect();
    }

    #[must_use]
    pub fn build(base: &[u8]) -> Self {
        Self::build_shared(Arc::from(base))
    }

    /// Like [`Self::build`], indexing a buffer the caller already shares instead of copying it.
    #[must_use]
    pub fn build_shared(base: Arc<[u8]>) -> Self {
        if base.len() < WINDOW + 1 {
            return Self {
                base,
                bucket_mask: 0,
                bucket_ranges: vec![0, 0],
                anchors: Vec::new(),
            };
        }

        let window_count = (base.len() - 1) / WINDOW;
        let slots = Self::table_slots(window_count);
        let bucket_mask = (slots - 1) as u32;
        let mut buckets: Vec<Vec<WindowAnchor>> = vec![Vec::new(); slots];

        let mut scan = window_count * WINDOW - WINDOW;
        let mut last_fp = !0u32;
        loop {
            let fp = Self::fingerprint_at(&base, scan);
            let bucket = (fp & bucket_mask) as usize;
            if fp == last_fp {
                if let Some(tail) = buckets[bucket].last_mut() {
                    tail.copy_start = scan + WINDOW;
                }
            } else {
                last_fp = fp;
                buckets[bucket].push(WindowAnchor {
                    fingerprint: fp,
                    copy_start: scan + WINDOW,
                });
            }
            if scan == 0 {
                break;
            }
            scan -= WINDOW;
        }

        for bucket in &mut buckets {
            Self::subsample_bucket(bucket, BUCKET_CAP);
        }

        let mut anchors = Vec::new();
        let mut bucket_ranges = Vec::with_capacity(slots + 1);
        for bucket in buckets {
            bucket_ranges.push(anchors.len());
            anchors.extend_from_slice(&bucket);
        }
        bucket_ranges.push(anchors.len());

        Self {
            base,
            bucket_mask,
            bucket_ranges,
            anchors,
        }
    }

    fn candidates(&self, fingerprint: u32) -> &[WindowAnchor] {
        let idx = (fingerprint & self.bucket_mask) as usize;
        let start = self.bucket_ranges[idx];
        let end = self.bucket_ranges[idx + 1];
        &self.anchors[start..end]
    }

    fn best_copy_span(
        &self,
        target: &[u8],
        at: usize,
        fingerprint: u32,
        at_least: usize,
    ) -> (usize, usize) {
        let base = &self.base;
        let base_len = base.len();
        let target_len = target.len();
        let mut best_base = 0usize;
        let mut best_len = at_least;

        for anchor in self.candidates(fingerprint) {
            if anchor.fingerprint != fingerprint {
                continue;
            }
            let mut base_pos = anchor.copy_start;
            if base_pos >= base_len {
                continue;
            }
            let mut tgt_pos = at;
            let mut remain = base_len - base_pos;
            if remain > target_len - tgt_pos {
                remain = target_len - tgt_pos;
            }
            if remain <= best_len {
                break;
            }
            while remain > 0 && target[tgt_pos] == base[base_pos] {
                base_pos += 1;
                tgt_pos += 1;
                remain -= 1;
            }
            let matched = base_pos - anchor.copy_start;
            if matched > best_len {
                best_len = matched;
                best_base = anchor.copy_start;
                if best_len >= COPY_GOOD_ENOUGH {
                    break;
                }
            }
        }
        (best_base, best_len)
    }

    #[must_use]
    pub fn encode(&self, target: &[u8], max_size: usize) -> Option<Vec<u8>> {
        let mut out = Vec::with_capacity(8192.min(target.len() / 4 + 64));
        write_delta_varint(&mut out, self.base.len());
        write_delta_varint(&mut out, target.len());
        if max_size > 0 && out.len() > max_size {
            return None;
        }
        if target.is_empty() {
            return Some(out);
        }

        let body_start = out.len();
        let mut cursor = 0usize;
        let mut rolling = RollingFingerprint::default();
        let mut pending_copy_len = 0usize;
        let mut pending_copy_base = 0usize;
        let mut open_literals: Option<usize> = None;
        let target_len = target.len();

        let prime = WINDOW.min(target_len);
        if prime > 0 {
            for &b in &target[..prime] {
                push_literal(&mut out, &mut open_literals, b, body_start);
            }
            rolling.absorb_prefix(&target[..prime]);
            cursor = prime;
        }

        while cursor < target_len {
            if pending_copy_len < COPY_GOOD_ENOUGH {
                rolling.slide(target, cursor);
                let (base, len) =
                    self.best_copy_span(target, cursor, rolling.value, pending_copy_len);
                if len > pending_copy_len {
                    pending_copy_base = base;
                    pending_copy_len = len;
                }
            }

            if pending_copy_len < MIN_COPY {
                push_literal(&mut out, &mut open_literals, target[cursor], body_start);
                cursor += 1;
                pending_copy_len = 0;
                if max_size > 0 && out.len() > max_size {
                    return None;
                }
                continue;
            }

            let base = &self.base;
            while pending_copy_base > 0
                && cursor > 0
                && open_literals.is_some()
                && base[pending_copy_base - 1] == target[cursor - 1]
            {
                pending_copy_base -= 1;
                cursor -= 1;
                if !pop_literal(&mut out, &mut open_literals, body_start) {
                    pending_copy_base += 1;
                    cursor += 1;
                    break;
                }
                pending_copy_len += 1;
            }

            if pending_copy_len > target_len - cursor {
                pending_copy_len = target_len - cursor;
            }
            if pending_copy_base + pending_copy_len > base.len() {
                pending_copy_len = base.len().saturating_sub(pending_copy_base);
            }
            if pending_copy_len < MIN_COPY {
                pending_copy_len = 0;
                push_literal(&mut out, &mut open_literals, target[cursor], body_start);
                cursor += 1;
                if max_size > 0 && out.len() > max_size {
                    return None;
                }
                continue;
            }

            open_literals = None;

            let mut remaining = pending_copy_len;
            while remaining > 0 {
                let chunk = remaining.min(MAX_COPY_SLICE);
                push_copy(&mut out, pending_copy_base, chunk).ok()?;
                pending_copy_base += chunk;
                cursor += chunk;
                remaining -= chunk;
                if max_size > 0 && out.len() > max_size {
                    return None;
                }
            }
            pending_copy_len = remaining;

            if pending_copy_len < COPY_GOOD_ENOUGH && cursor >= WINDOW {
                rolling.reset_at(target, cursor);
            }
        }

        if max_size > 0 && out.len() > max_size {
            return None;
        }
        Some(out)
    }
}

#[derive(Default)]
struct RollingFingerprint {
    value: u32,
}

impl RollingFingerprint {
    fn absorb_prefix(&mut self, bytes: &[u8]) {
        for &b in bytes {
            self.value =
                ((self.value << 8) | u32::from(b)) ^ T[(self.value >> FINGERPRINT_SHIFT) as usize];
        }
    }

    fn slide(&mut self, target: &[u8], at: usize) {
        self.value ^= U[target[at - WINDOW] as usize];
        self.value = ((self.value << 8) | u32::from(target[at]))
            ^ T[(self.value >> FINGERPRINT_SHIFT) as usize];
    }

    fn reset_at(&mut self, target: &[u8], at: usize) {
        self.value = 0;
        for j in 0..WINDOW {
            self.value = ((self.value << 8) | u32::from(target[at - WINDOW + j]))
                ^ T[(self.value >> FINGERPRINT_SHIFT) as usize];
        }
    }
}

fn push_literal(out: &mut Vec<u8>, open_prefix: &mut Option<usize>, byte: u8, body_start: usize) {
    if open_prefix.is_none() {
        out.push(0);
        *open_prefix = Some(out.len() - 1);
    }
    out.push(byte);
    let Some(prefix) = *open_prefix else {
        return;
    };
    debug_assert!(prefix >= body_start);
    let run_len = out[prefix].saturating_add(1);
    if run_len >= MAX_INSERT_RUN {
        out[prefix] = MAX_INSERT_RUN;
        *open_prefix = None;
    } else {
        out[prefix] = run_len;
    }
}

fn pop_literal(out: &mut Vec<u8>, open_prefix: &mut Option<usize>, body_start: usize) -> bool {
    let prefix = match open_prefix {
        Some(p) => *p,
        None => return false,
    };
    if prefix < body_start || out.len() <= prefix + 1 {
        return false;
    }
    out.pop();
    let run_len = out[prefix].saturating_sub(1);
    if run_len == 0 {
        out.truncate(prefix.max(body_start));
        *open_prefix = None;
    } else {
        out[prefix] = run_len;
    }
    true
}

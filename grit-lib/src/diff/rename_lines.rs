//! Span-hash tables for Git-style rename/copy similarity (`diffcore-delta` / `hash_chars`).
//!
//! Each blob body is scanned once into [`RenameLineTable`]; pairing many candidates
//! reuses those tables instead of re-hashing for every pair.

const DIFF_HASHBASE: u32 = 107_927;

/// Per-blob span hashes and byte weights for rename similarity scoring.
#[derive(Debug, Clone)]
pub(crate) struct RenameLineTable {
    byte_len: usize,
    /// Sorted by hash value, with accumulated byte weight per distinct hash.
    spans: Vec<(u32, u32)>,
}

impl RenameLineTable {
    /// Build a table from raw blob bytes (no CRLF normalization).
    #[must_use]
    pub(crate) fn from_blob(data: &[u8]) -> Self {
        let is_text = is_text_for_rename(data);
        Self {
            byte_len: data.len(),
            spans: span_weights(data, is_text),
        }
    }

    /// Similarity percentage (0–100) between two tables (Git span-hash intersection).
    #[must_use]
    pub(crate) fn similarity_score(a: &Self, b: &Self) -> u32 {
        let max_size = a.byte_len.max(b.byte_len);
        if max_size == 0 {
            return 100;
        }
        let copied = count_copied_span_bytes(a, b);
        ((copied * 100) / max_size).min(100) as u32
    }
}

/// Git `buffer_is_binary`: NUL in the first 8000 bytes ⇒ not text for CR skipping.
fn is_text_for_rename(data: &[u8]) -> bool {
    !data.iter().take(8000).any(|&b| b == 0)
}

fn span_weights(buf: &[u8], is_text: bool) -> Vec<(u32, u32)> {
    let mut out: Vec<(u32, u32)> = Vec::new();
    let mut n = 0u32;
    let mut accum1: u32 = 0;
    let mut accum2: u32 = 0;
    let mut i = 0usize;
    while i < buf.len() {
        let c = buf[i] as u32;
        let old_1 = accum1;
        i += 1;

        if is_text && c == b'\r' as u32 && i < buf.len() && buf[i] == b'\n' {
            continue;
        }

        accum1 = accum1.wrapping_shl(7) ^ accum2.wrapping_shr(25);
        accum2 = accum2.wrapping_shl(7) ^ old_1.wrapping_shr(25);
        accum1 = accum1.wrapping_add(c);
        n += 1;
        if n < 64 && c != b'\n' as u32 {
            continue;
        }
        let hashval = (accum1.wrapping_add(accum2.wrapping_mul(0x61))) % DIFF_HASHBASE;
        out.push((hashval, n));
        n = 0;
        accum1 = 0;
        accum2 = 0;
    }
    // Git rename similarity drops a trailing partial span (unlike dirstat's hash_blob_spans).
    out.sort_unstable_by_key(|&(hash, _)| hash);
    out.dedup_by(|later, first| {
        if later.0 != first.0 {
            return false;
        }
        first.1 = first.1.saturating_add(later.1);
        true
    });
    out
}

/// Copied bytes from `src` side (`sc` in Git's `diffcore_count_changes`).
fn count_copied_span_bytes(src: &RenameLineTable, dst: &RenameLineTable) -> usize {
    let mut copied = 0usize;
    let mut si = 0usize;
    let mut di = 0usize;
    let src_spans = &src.spans;
    let dst_spans = &dst.spans;
    while si < src_spans.len() {
        let (s_hash, s_cnt) = src_spans[si];
        while di < dst_spans.len() && dst_spans[di].0 < s_hash {
            di += 1;
        }
        let dst_cnt = if di < dst_spans.len() && dst_spans[di].0 == s_hash {
            dst_spans[di].1
        } else {
            0
        };
        copied += s_cnt.min(dst_cnt) as usize;
        si += 1;
    }
    copied
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn trailing_partial_span_is_ignored_for_similarity() {
        // Without a final newline, a trailing run shorter than 64 bytes is not hashed,
        // so span-only scoring sees no overlap (exact rename uses same oid, not this path).
        let a = RenameLineTable::from_blob(b"hello");
        let b = RenameLineTable::from_blob(b"hello");
        assert_eq!(RenameLineTable::similarity_score(&a, &b), 0);
    }

    #[test]
    fn similarity_scores_are_symmetric() {
        let pairs: &[(&[u8], &[u8])] = &[
            (b"".as_slice(), b"".as_slice()),
            (b"hello\nworld\n", b"hello\nworld\n"),
            (b"short\n", b"much longer\nunchanged tail\n"),
        ];
        for (old, new) in pairs {
            let a = RenameLineTable::from_blob(old);
            let b = RenameLineTable::from_blob(new);
            assert_eq!(
                RenameLineTable::similarity_score(&a, &b),
                RenameLineTable::similarity_score(&b, &a),
                "old={old:?} new={new:?}"
            );
        }
    }

    #[test]
    fn long_line_edit_after_64_bytes_can_still_match_spans() {
        let prefix: Vec<u8> = (0..70).map(|_| b'x').collect();
        let mut old = prefix.clone();
        old.push(b'\n');
        let mut new = prefix;
        new[65] = b'y';
        new.push(b'\n');
        let score = RenameLineTable::similarity_score(
            &RenameLineTable::from_blob(&old),
            &RenameLineTable::from_blob(&new),
        );
        assert!(score >= 90, "score={score}");
    }
}

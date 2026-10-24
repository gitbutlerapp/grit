//! Line-hash tables for Git-style rename/copy similarity (diffcore-delta).
//!
//! Each blob body is scanned once into [`RenameLineTable`]; pairing many candidates
//! reuses those tables instead of re-hashing every line for every pair.

use std::collections::HashMap;
use std::hash::{Hash, Hasher};

/// Per-blob line hashes and byte weights for rename similarity scoring.
#[derive(Debug, Clone)]
pub(crate) struct RenameLineTable {
    byte_len: usize,
    lines: Vec<(u64, usize)>,
}

impl RenameLineTable {
    /// Build a table from bytes already normalized for rename detection (LF-only).
    #[must_use]
    pub(crate) fn from_normalized_blob(data: &[u8]) -> Self {
        Self {
            byte_len: data.len(),
            lines: line_weights(data),
        }
    }

    /// Build a table using CRLF→LF normalization (copy/rename similarity path).
    #[must_use]
    pub(crate) fn from_similarity_blob(data: &[u8]) -> Self {
        let norm = crate::crlf::crlf_to_lf(data);
        Self::from_normalized_blob(&norm)
    }

    /// Similarity percentage (0–100) between two tables (Git line-hash intersection).
    #[must_use]
    pub(crate) fn similarity_score(a: &Self, b: &Self) -> u32 {
        if a.byte_len == 0 && b.byte_len == 0 {
            return 100;
        }
        let max_size = a.byte_len.max(b.byte_len);
        if max_size == 0 {
            return 100;
        }
        let copied = count_copied_line_bytes_between(a, b);
        ((copied * 100) / max_size).min(100) as u32
    }
}

fn hash_rename_line(line: &[u8]) -> u64 {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    line.hash(&mut hasher);
    hasher.finish()
}

fn line_weights(data: &[u8]) -> Vec<(u64, usize)> {
    let mut out = Vec::new();
    if data.is_empty() {
        return out;
    }
    let mut start = 0usize;
    for (i, &b) in data.iter().enumerate() {
        if b != b'\n' {
            continue;
        }
        // Hash includes the line terminator so `a` and `a\n` are distinct lines.
        let line = &data[start..=i];
        let weight = line.len();
        out.push((hash_rename_line(line), weight));
        start = i + 1;
    }
    if start < data.len() {
        let line = &data[start..];
        out.push((hash_rename_line(line), data.len() - start));
    }
    out
}

fn count_copied_line_bytes_between(a: &RenameLineTable, b: &RenameLineTable) -> usize {
    let (short, long) = if a.byte_len <= b.byte_len {
        (a, b)
    } else {
        (b, a)
    };
    let mut counts: HashMap<u64, usize> = HashMap::new();
    for (hash, weight) in &short.lines {
        *counts.entry(*hash).or_default() += weight;
    }
    let mut copied = 0usize;
    for (hash, weight) in &long.lines {
        if let Some(slot) = counts.get_mut(hash) {
            if *slot > 0 {
                let take = (*weight).min(*slot);
                copied += take;
                *slot -= take;
            }
        }
    }
    copied
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn similarity_scores_are_stable_and_symmetric() {
        let pairs: &[(&[u8], &[u8])] = &[
            (b"".as_slice(), b"".as_slice()),
            (b"hello\nworld\n", b"hello\nworld\n"),
            (b"short\n", b"much longer\nunchanged tail\n"),
            (
                b"= Title\n\nparagraph one\nparagraph two\n",
                b"= New title\n\nparagraph one\nparagraph three\n",
            ),
        ];
        for (old, new) in pairs {
            let a = RenameLineTable::from_normalized_blob(old);
            let b = RenameLineTable::from_normalized_blob(new);
            let ab = RenameLineTable::similarity_score(&a, &b);
            let ba = RenameLineTable::similarity_score(&b, &a);
            assert_eq!(ab, ba, "old={old:?} new={new:?}");
        }
    }

    #[test]
    fn from_similarity_blob_normalizes_crlf() {
        let cr = RenameLineTable::from_similarity_blob(b"a\r\nb\r\n");
        let lf = RenameLineTable::from_normalized_blob(b"a\nb\n");
        assert_eq!(RenameLineTable::similarity_score(&cr, &lf), 100);
    }
}

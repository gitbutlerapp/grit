//! Line-hash tables for Git-style rename/copy similarity (diffcore-delta).
//!
//! Each blob body is scanned once into [`RenameLineTable`]; pairing many candidates
//! reuses those tables instead of re-hashing every line for every pair.

use std::hash::{Hash, Hasher};

/// Per-blob line hashes and byte weights for rename similarity scoring.
#[derive(Debug, Clone)]
pub(crate) struct RenameLineTable {
    byte_len: usize,
    // Sorted by hash, with one accumulated byte weight per distinct hash.
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
    out.sort_unstable_by_key(|&(hash, _)| hash);
    out.dedup_by(|later, first| {
        if later.0 != first.0 {
            return false;
        }
        first.1 += later.1;
        true
    });
    out
}

fn count_copied_line_bytes_between(a: &RenameLineTable, b: &RenameLineTable) -> usize {
    let mut copied = 0usize;
    let (mut left, mut right) = (0, 0);
    while let (Some(&(left_hash, left_weight)), Some(&(right_hash, right_weight))) =
        (a.lines.get(left), b.lines.get(right))
    {
        if left_hash == right_hash {
            copied += left_weight.min(right_weight);
        }
        // Advance both on a match, otherwise just the smaller hash. Boolean
        // increments avoid an unpredictable ordering branch for unrelated files.
        left += usize::from(left_hash <= right_hash);
        right += usize::from(right_hash <= left_hash);
    }
    copied
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cached_scores_match_byte_weighted_line_intersection() {
        // The reference works on complete lines, independently of table order
        // and compaction. Repeats contribute bytes only up to their multiplicity
        // in the other blob; a final unterminated line is a distinct value.
        fn reference(old: &[u8], new: &[u8]) -> u32 {
            let mut weights = std::collections::HashMap::<&[u8], usize>::new();
            for line in old.split_inclusive(|&byte| byte == b'\n') {
                *weights.entry(line).or_default() += line.len();
            }
            let mut copied = 0;
            for line in new.split_inclusive(|&byte| byte == b'\n') {
                if let Some(remaining) = weights.get_mut(line) {
                    let take = line.len().min(*remaining);
                    copied += take;
                    *remaining -= take;
                }
            }
            let max_len = old.len().max(new.len());
            if max_len == 0 {
                100
            } else {
                (copied * 100 / max_len) as u32
            }
        }

        let fixtures: &[&[u8]] = &[
            b"",
            b"\n",
            b"a",
            b"a\n",
            b"a\na\na\nb\n",
            b"b\na\na\n",
            b"a\nb\na",
            b"\0binary\n\xff\n",
            b"= Chapter\n\nparagraph one\nparagraph two\n",
            b"= Changed\n\nparagraph two\nparagraph one\n",
        ];
        let tables: Vec<_> = fixtures
            .iter()
            .map(|data| RenameLineTable::from_normalized_blob(data))
            .collect();
        for (i, old) in fixtures.iter().enumerate() {
            assert_eq!(
                tables[i].lines.iter().map(|line| line.1).sum::<usize>(),
                old.len()
            );
            assert!(tables[i].lines.windows(2).all(|pair| pair[0].0 < pair[1].0));
            for (j, new) in fixtures.iter().enumerate() {
                let expected = reference(old, new);
                // Reusing a table across pairs must not consume its weights.
                assert_eq!(
                    RenameLineTable::similarity_score(&tables[i], &tables[j]),
                    expected
                );
                assert_eq!(crate::diff::rename_similarity_score(old, new), expected);
            }
        }
    }

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

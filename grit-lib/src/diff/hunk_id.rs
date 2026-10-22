//! Stable identifiers for structured diff hunks (patch selection in later steps).
//!
//! Ids hash side, path, and hunk line bodies (not line numbers) so they stay stable
//! when earlier hunks are staged and later hunks shift in the file.

use std::fmt;
use std::str::FromStr;

use thiserror::Error;

use super::structured::{StructuredHunk, StructuredLine, StructuredLineKind};
use crate::hash::ChecksumHasher;
use crate::objects::HashAlgo;

const HUNK_ID_HEX_LEN: usize = 12;

/// Which tree/index/worktree sides a hunk was produced from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum DiffSide {
    /// Index compared to worktree (unstaged changes).
    Unstaged,
    /// `HEAD` tree compared to index (staged changes).
    Staged,
    /// Commit, tree, or other comparisons (ids are computed but not selectable yet).
    Other,
}

impl DiffSide {
    fn tag_byte(self) -> u8 {
        match self {
            Self::Unstaged => 0,
            Self::Staged => 1,
            Self::Other => 2,
        }
    }
}

/// Twelve lowercase hex characters identifying one structured hunk within a side and path.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct HunkId([u8; HUNK_ID_HEX_LEN / 2]);

/// Parse or format failure for [`HunkId`].
#[derive(Debug, Error, PartialEq, Eq)]
pub enum HunkIdError {
    /// Wrong length or non-hex character.
    #[error("hunk id must be {HUNK_ID_HEX_LEN} lowercase hex characters")]
    Invalid,
}

impl HunkId {
    fn from_digest(digest: &[u8]) -> Self {
        let mut raw = [0u8; HUNK_ID_HEX_LEN / 2];
        raw.copy_from_slice(&digest[..HUNK_ID_HEX_LEN / 2]);
        Self(raw)
    }

    /// Construct from exactly six raw digest bytes (used in tests).
    #[must_use]
    pub fn from_bytes(bytes: [u8; HUNK_ID_HEX_LEN / 2]) -> Self {
        Self(bytes)
    }

    /// View as lowercase hex (twelve characters).
    #[must_use]
    pub fn as_str(&self) -> String {
        self.to_string()
    }
}

impl fmt::Display for HunkId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for b in self.0 {
            write!(f, "{b:02x}")?;
        }
        Ok(())
    }
}

impl FromStr for HunkId {
    type Err = HunkIdError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        if s.len() != HUNK_ID_HEX_LEN
            || !s
                .bytes()
                .all(|c| c.is_ascii_digit() || (b'a'..=b'f').contains(&c))
        {
            return Err(HunkIdError::Invalid);
        }
        let mut raw = [0u8; HUNK_ID_HEX_LEN / 2];
        for (i, chunk) in s.as_bytes().chunks(2).enumerate() {
            let hi = hex_nibble(chunk[0]).ok_or(HunkIdError::Invalid)?;
            let lo = hex_nibble(chunk[1]).ok_or(HunkIdError::Invalid)?;
            raw[i] = (hi << 4) | lo;
        }
        Ok(Self(raw))
    }
}

fn hex_nibble(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        _ => None,
    }
}

fn hash_hunk_id(
    side: DiffSide,
    path: &str,
    hunk: &StructuredHunk,
    duplicate_rank: Option<u32>,
) -> HunkId {
    let mut hasher = HashAlgo::Sha1.checksum_hasher();
    hasher.update(&[side.tag_byte()]);
    hasher.update(path.as_bytes());
    hasher.update(&[0]);
    hash_hunk_body(&mut hasher, hunk);
    if let Some(rank) = duplicate_rank {
        hasher.update(&rank.to_le_bytes());
    }
    let digest = hasher.finalize();
    HunkId::from_digest(&digest)
}

#[derive(Eq, PartialEq, Ord, PartialOrd)]
struct DuplicateSortKey {
    old_start: usize,
    new_start: usize,
    is_peer: bool,
    index: usize,
}

fn duplicate_ranks(hunks: &[StructuredHunk], peer_hunks: &[StructuredHunk]) -> Vec<Option<u32>> {
    let body_counts = body_count_map(hunks, peer_hunks);
    hunks
        .iter()
        .enumerate()
        .map(|(i, h)| {
            let body = hunk_body_digest(h);
            if body_counts.get(&body).copied().unwrap_or(0) <= 1 {
                return None;
            }
            let mut keys: Vec<DuplicateSortKey> = Vec::new();
            for (j, x) in hunks.iter().enumerate() {
                if hunk_body_digest(x) == body {
                    keys.push(DuplicateSortKey {
                        old_start: x.old_start,
                        new_start: x.new_start,
                        is_peer: false,
                        index: j,
                    });
                }
            }
            for (j, x) in peer_hunks.iter().enumerate() {
                if hunk_body_digest(x) == body {
                    keys.push(DuplicateSortKey {
                        old_start: x.old_start,
                        new_start: x.new_start,
                        is_peer: true,
                        index: j,
                    });
                }
            }
            keys.sort();
            let rank = keys
                .iter()
                .position(|k| !k.is_peer && k.index == i)
                .map(|pos| pos as u32)?;
            Some(rank)
        })
        .collect()
}

fn body_count_map(
    hunks: &[StructuredHunk],
    peer_hunks: &[StructuredHunk],
) -> std::collections::HashMap<Vec<u8>, u32> {
    let mut counts: std::collections::HashMap<Vec<u8>, u32> = std::collections::HashMap::new();
    for h in hunks.iter().chain(peer_hunks.iter()) {
        let body = hunk_body_digest(h);
        *counts.entry(body).or_insert(0) += 1;
    }
    counts
}

fn hash_hunk_body(hasher: &mut ChecksumHasher, hunk: &StructuredHunk) {
    for line in &hunk.lines {
        hash_line(hasher, line);
    }
}

fn hash_line(hasher: &mut ChecksumHasher, line: &StructuredLine) {
    let kind = match line.kind {
        StructuredLineKind::Context => b' ',
        StructuredLineKind::Add => b'+',
        StructuredLineKind::Del => b'-',
    };
    hasher.update(&[kind]);
    for seg in &line.segments {
        hasher.update(seg.text.as_bytes());
    }
    hasher.update(&[if line.no_newline_at_eof { 1 } else { 0 }]);
}

fn hunk_body_digest(hunk: &StructuredHunk) -> Vec<u8> {
    let mut hasher = HashAlgo::Sha1.checksum_hasher();
    hash_hunk_body(&mut hasher, hunk);
    hasher.finalize()
}

/// Stable ids for `hunks` in file order (0-based index matches vector position).
#[must_use]
pub fn hunk_ids(side: DiffSide, path: &str, hunks: &[StructuredHunk]) -> Vec<HunkId> {
    hunk_ids_with_duplicate_peers(side, path, hunks, &[])
}

/// Like [`hunk_ids`], but `peer_hunks` on the same path (typically the other index/worktree
/// side) participate in duplicate-body detection so ids stay stable when an earlier identical
/// hunk moves to the staged side.
#[must_use]
pub fn hunk_ids_with_duplicate_peers(
    side: DiffSide,
    path: &str,
    hunks: &[StructuredHunk],
    peer_hunks: &[StructuredHunk],
) -> Vec<HunkId> {
    let ranks = duplicate_ranks(hunks, peer_hunks);
    hunks
        .iter()
        .zip(ranks)
        .map(|(h, rank)| hash_hunk_id(side, path, h, rank))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::diff::structured::{StructuredLineKind, StructuredSegment};

    fn line(kind: StructuredLineKind, text: &str) -> StructuredLine {
        StructuredLine {
            kind,
            old: None,
            new: None,
            segments: vec![StructuredSegment {
                text: text.to_owned(),
                emphasis: false,
            }],
            no_newline_at_eof: false,
        }
    }

    fn hunk(lines: Vec<StructuredLine>) -> StructuredHunk {
        StructuredHunk {
            old_start: 1,
            new_start: 1,
            old_lines: lines.len(),
            new_lines: lines.len(),
            context: None,
            lines,
        }
    }

    #[test]
    fn hunk_id_stable_across_line_shift() {
        let body = vec![
            line(StructuredLineKind::Context, "ctx"),
            line(StructuredLineKind::Add, "new"),
        ];
        let mut a = hunk(body.clone());
        a.old_start = 10;
        a.new_start = 10;
        let mut b = hunk(body);
        b.old_start = 20;
        b.new_start = 22;
        let ids_a = hunk_ids(DiffSide::Unstaged, "file.txt", &[a]);
        let ids_b = hunk_ids(DiffSide::Unstaged, "file.txt", &[b]);
        assert_eq!(ids_a, ids_b);
    }

    #[test]
    fn hunk_id_changes_on_content_change() {
        let a = hunk(vec![line(StructuredLineKind::Add, "one")]);
        let b = hunk(vec![line(StructuredLineKind::Add, "two")]);
        let ids_a = hunk_ids(DiffSide::Unstaged, "f", &[a]);
        let ids_b = hunk_ids(DiffSide::Unstaged, "f", &[b]);
        assert_ne!(ids_a[0], ids_b[0]);
    }

    #[test]
    fn hunk_id_differs_by_side_and_path() {
        let h = hunk(vec![line(StructuredLineKind::Add, "x")]);
        let unstaged = hunk_ids(DiffSide::Unstaged, "a", &[h.clone()]);
        let staged = hunk_ids(DiffSide::Staged, "a", &[h.clone()]);
        let other_path = hunk_ids(DiffSide::Unstaged, "b", &[h]);
        assert_ne!(unstaged[0], staged[0]);
        assert_ne!(unstaged[0], other_path[0]);
    }

    #[test]
    fn hunk_id_duplicate_hunks_disambiguated() {
        let body = vec![line(StructuredLineKind::Add, "dup")];
        let mut a = hunk(body.clone());
        a.old_start = 1;
        a.new_start = 1;
        let mut b = hunk(body);
        b.old_start = 5;
        b.new_start = 5;
        let ids = hunk_ids(DiffSide::Staged, "x", &[a, b]);
        assert_ne!(ids[0], ids[1]);
    }

    #[test]
    fn hunk_id_fromstr_rejects_uppercase() {
        assert_eq!(HunkId::from_str("ABCDEF123456"), Err(HunkIdError::Invalid));
    }

    #[test]
    fn hunk_id_stable_when_earlier_duplicate_on_peer_side() {
        let body = vec![
            line(StructuredLineKind::Context, "ctx"),
            line(StructuredLineKind::Add, "same"),
        ];
        let mut first = hunk(body.clone());
        first.old_start = 1;
        first.new_start = 1;
        let mut second = hunk(body);
        second.old_start = 10;
        second.new_start = 10;

        let both = hunk_ids(DiffSide::Unstaged, "f", &[first.clone(), second.clone()]);
        assert_ne!(both[0], both[1]);

        let with_peer =
            hunk_ids_with_duplicate_peers(DiffSide::Unstaged, "f", &[second.clone()], &[first]);
        assert_eq!(with_peer[0], both[1]);
    }

    #[test]
    fn hunk_id_stable_when_insertion_coordinates_shift_after_staging() {
        let body = vec![line(StructuredLineKind::Add, "insert")];
        let mut first = hunk(body.clone());
        first.old_start = 19;
        first.new_start = 19;
        let mut second = hunk(body);
        second.old_start = 19;
        second.new_start = 20;

        let both = hunk_ids(DiffSide::Unstaged, "f", &[first.clone(), second.clone()]);
        assert_ne!(both[0], both[1]);
        let id_later = both[1].clone();

        let mut shifted = second;
        shifted.old_start = 20;
        shifted.new_start = 20;

        let after = hunk_ids_with_duplicate_peers(DiffSide::Unstaged, "f", &[shifted], &[first]);
        assert_eq!(after[0], id_later);
    }

    #[test]
    fn hunk_id_roundtrips_display_fromstr() {
        let id = hunk_ids(
            DiffSide::Other,
            "p",
            &[hunk(vec![line(StructuredLineKind::Del, "z")])],
        )[0]
        .clone();
        let s = id.to_string();
        assert_eq!(s.len(), HUNK_ID_HEX_LEN);
        assert!(
            s.bytes()
                .all(|c| c.is_ascii_digit() || (b'a'..=b'f').contains(&c)),
            "expected lowercase hex: {s}"
        );
        assert_eq!(HunkId::from_str(&s).expect("parse"), id);
    }
}

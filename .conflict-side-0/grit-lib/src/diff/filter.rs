//! Git `--diff-filter` selection over [`DiffStatus`](super::DiffStatus).

use super::{DiffEntry, DiffStatus};

/// Typed include/exclude set for [`DiffStatus`] using Git's letter syntax.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct DiffFilter {
    include: u8,
    exclude: u8,
    has_include: bool,
}

/// Unknown or unsupported `--diff-filter` letter.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum InvalidDiffFilter {
    /// Character is not a recognized diff status letter.
    #[error("unknown diff filter letter `{0}`")]
    Unknown(char),
    /// Letter is valid in Git but not produced by grit's diff (copy, broken, unknown).
    #[error("diff filter letter `{0}` is not supported")]
    Unsupported(char),
}

impl DiffFilter {
    /// Apply this filter to tree diff entries (after rename detection).
    #[must_use]
    pub fn filter_entries(self, entries: Vec<DiffEntry>) -> Vec<DiffEntry> {
        entries
            .into_iter()
            .filter(|e| self.matches(e.status))
            .collect()
    }

    #[must_use]
    fn matches(self, status: DiffStatus) -> bool {
        let bit = status_bit(status);
        if self.has_include && self.include & bit == 0 {
            return false;
        }
        if self.exclude & bit != 0 {
            return false;
        }
        true
    }

    fn from_letter(ch: char) -> Result<(bool, u8), InvalidDiffFilter> {
        let upper = ch.to_ascii_uppercase();
        let bit = match upper {
            'A' => status_bit(DiffStatus::Added),
            'D' => status_bit(DiffStatus::Deleted),
            'M' => status_bit(DiffStatus::Modified),
            'R' => status_bit(DiffStatus::Renamed),
            'T' => status_bit(DiffStatus::TypeChanged),
            'U' => status_bit(DiffStatus::Unmerged),
            'C' | 'X' | 'B' => return Err(InvalidDiffFilter::Unsupported(ch)),
            _ if ch.is_ascii_alphabetic() => return Err(InvalidDiffFilter::Unknown(ch)),
            _ => return Err(InvalidDiffFilter::Unknown(ch)),
        };
        Ok((ch.is_ascii_uppercase(), bit))
    }
}

fn status_bit(status: DiffStatus) -> u8 {
    1 << match status {
        DiffStatus::Added => 0,
        DiffStatus::Deleted => 1,
        DiffStatus::Modified => 2,
        DiffStatus::Renamed => 3,
        DiffStatus::Copied => 4,
        DiffStatus::TypeChanged => 5,
        DiffStatus::Unmerged => 6,
    }
}

impl std::str::FromStr for DiffFilter {
    type Err = InvalidDiffFilter;

    fn from_str(spec: &str) -> Result<Self, Self::Err> {
        let mut filter = Self::default();
        for ch in spec.chars() {
            if ch.is_whitespace() {
                continue;
            }
            let (is_upper, bit) = Self::from_letter(ch)?;
            if is_upper {
                filter.include |= bit;
                filter.has_include = true;
            } else {
                filter.exclude |= bit;
            }
        }
        Ok(filter)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn uppercase_only_shows_matching_status() {
        let f: DiffFilter = "AD".parse().expect("parse");
        assert!(f.matches(DiffStatus::Added));
        assert!(f.matches(DiffStatus::Deleted));
        assert!(!f.matches(DiffStatus::Modified));
    }

    #[test]
    fn lowercase_excludes_status() {
        let f: DiffFilter = "a".parse().expect("parse");
        assert!(!f.matches(DiffStatus::Added));
        assert!(f.matches(DiffStatus::Deleted));
    }

    #[test]
    fn rejects_unsupported_letters() {
        assert!(matches!(
            "C".parse::<DiffFilter>(),
            Err(InvalidDiffFilter::Unsupported('C'))
        ));
        assert!(matches!(
            "z".parse::<DiffFilter>(),
            Err(InvalidDiffFilter::Unknown('z'))
        ));
    }
}

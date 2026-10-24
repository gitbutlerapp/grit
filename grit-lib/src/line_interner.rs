//! Fast line interning for xdiff-style Myers pipelines.

use std::borrow::Cow;

use hashbrown::HashMap;

/// Intern line text to dense `u32` ids (equal id iff equal line bytes).
///
/// Ids are assigned in first-seen order starting at zero, so `0..len()` covers every id handed
/// out. Borrowed lines are stored without copying; only normalized keys are owned.
#[derive(Default)]
pub struct LineInterner<'a> {
    ids: HashMap<Cow<'a, str>, u32>,
}

impl<'a> LineInterner<'a> {
    /// Return the id for `line`, inserting when unseen.
    #[must_use]
    pub fn intern(&mut self, line: Cow<'a, str>) -> u32 {
        let next = u32::try_from(self.ids.len()).unwrap_or(u32::MAX);
        *self.ids.entry(line).or_insert(next)
    }

    /// Intern every line in `lines`.
    pub fn intern_all(&mut self, lines: &[&'a str]) -> Vec<u32> {
        self.ids.reserve(lines.len());
        lines
            .iter()
            .map(|line| self.intern(Cow::Borrowed(*line)))
            .collect()
    }

    /// Intern normalized keys for diff comparison while preserving display via original line slices.
    pub fn intern_all_normalized(
        &mut self,
        lines: &[&'a str],
        normalize: impl Fn(&'a str) -> Cow<'a, str>,
    ) -> Vec<u32> {
        self.ids.reserve(lines.len());
        lines
            .iter()
            .map(|line| self.intern(normalize(line)))
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::LineInterner;
    use std::borrow::Cow;

    #[test]
    fn intern_many_unique_lines_is_linear() {
        let n = 10_000usize;
        let lines: Vec<String> = (0..n).map(|i| format!("unique line {i}\n")).collect();
        let mut interner = LineInterner::default();
        for (i, line) in lines.iter().enumerate() {
            let id = interner.intern(Cow::Borrowed(line.as_str()));
            assert_eq!(id as usize, i);
        }
        assert_eq!(interner.intern(Cow::Borrowed(lines[0].as_str())), 0);
    }

    #[test]
    fn equal_lines_share_ids_across_sides() {
        let old = ["a\n", "b\n", "a\n"];
        let new = ["b\n", "c\n", "a"];
        let mut interner = LineInterner::default();
        let ids1 = interner.intern_all(&old);
        let ids2 = interner.intern_all(&new);
        assert_eq!(ids1, vec![0, 1, 0]);
        assert_eq!(ids2, vec![1, 2, 3]);
    }

    #[test]
    fn normalized_keys_merge_lines_that_differ_only_after_normalizing() {
        let lines = ["a b\n", "a  b\n", "ab\n"];
        let mut interner = LineInterner::default();
        let ids = interner.intern_all_normalized(&lines, |line| {
            Cow::Owned(line.split_whitespace().collect::<Vec<_>>().join(" "))
        });
        assert_eq!(ids, vec![0, 0, 1]);
    }
}

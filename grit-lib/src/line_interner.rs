//! Fast line interning for xdiff-style Myers pipelines.

use std::hash::BuildHasher;

use hashbrown::hash_table::Entry;
use hashbrown::{DefaultHashBuilder, HashTable};

/// Intern line text to dense `u32` ids (equal id iff equal line bytes).
///
/// Ids are assigned in first-seen order starting at zero, so `0..n` covers every id handed out.
/// Lines are borrowed, never copied. The hash table holds 4-byte ids and resolves them through
/// `lines`, which keeps it small enough to stay in cache for files with hundreds of thousands of
/// lines.
#[derive(Default)]
pub struct LineInterner<'a> {
    lines: Vec<&'a str>,
    table: HashTable<u32>,
    hasher: DefaultHashBuilder,
}

impl<'a> LineInterner<'a> {
    /// Interner with room for `lines` distinct lines before its table has to grow.
    ///
    /// Pass the total line count of every side that will be interned: growing the table between
    /// sides rehashes every line seen so far.
    #[must_use]
    pub fn with_capacity(lines: usize) -> Self {
        Self {
            lines: Vec::new(),
            table: HashTable::with_capacity(lines),
            hasher: DefaultHashBuilder::default(),
        }
    }

    /// Return the id for `line`, inserting when unseen.
    #[must_use]
    pub fn intern(&mut self, line: &'a str) -> u32 {
        let Self {
            lines,
            table,
            hasher,
        } = self;
        let hash = hasher.hash_one(line);
        let entry = table.entry(
            hash,
            |&id| lines[id as usize] == line,
            |&id| hasher.hash_one(lines[id as usize]),
        );
        match entry {
            Entry::Occupied(found) => *found.get(),
            Entry::Vacant(slot) => {
                let id = u32::try_from(lines.len()).unwrap_or(u32::MAX);
                lines.push(line);
                slot.insert(id);
                id
            }
        }
    }

    /// Intern every line in `lines`.
    pub fn intern_all(&mut self, lines: &[&'a str]) -> Vec<u32> {
        lines.iter().map(|line| self.intern(line)).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::LineInterner;

    #[test]
    fn intern_many_unique_lines_is_linear() {
        let n = 10_000usize;
        let lines: Vec<String> = (0..n).map(|i| format!("unique line {i}\n")).collect();
        let mut interner = LineInterner::default();
        for (i, line) in lines.iter().enumerate() {
            assert_eq!(interner.intern(line) as usize, i);
        }
        for (i, line) in lines.iter().enumerate() {
            assert_eq!(interner.intern(line) as usize, i);
        }
    }

    #[test]
    fn equal_lines_share_ids_across_sides() {
        let old = ["a\n", "b\n", "a\n"];
        let new = ["b\n", "c\n", "a"];
        let mut interner = LineInterner::with_capacity(old.len() + new.len());
        let ids1 = interner.intern_all(&old);
        let ids2 = interner.intern_all(&new);
        assert_eq!(ids1, vec![0, 1, 0]);
        assert_eq!(ids2, vec![1, 2, 3]);
    }
}

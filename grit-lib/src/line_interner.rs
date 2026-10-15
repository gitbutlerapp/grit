//! Fast line interning for xdiff-style Myers pipelines (hash + length, full compare on collision).

use std::collections::HashMap;

/// Intern line text to dense `u32` ids (equal id iff equal line bytes).
#[derive(Default)]
pub struct LineInterner {
    lines: Vec<Box<str>>,
    index: HashMap<(u64, u32), u32>,
}

impl LineInterner {
    /// Return the id for `line`, inserting when unseen.
    #[must_use]
    pub fn intern(&mut self, line: &str) -> u32 {
        let key = (
            crc32fast::hash(line.as_bytes()) as u64,
            u32::try_from(line.len()).unwrap_or(u32::MAX),
        );
        if let Some(&id) = self.index.get(&key) {
            if self.lines[id as usize].as_ref() == line {
                return id;
            }
        }
        for (i, existing) in self.lines.iter().enumerate() {
            if existing.as_ref() == line {
                return i as u32;
            }
        }
        let id = u32::try_from(self.lines.len()).expect("line interner overflow");
        self.lines.push(line.into());
        self.index.insert(key, id);
        id
    }

    /// Intern every line in `lines`.
    pub fn intern_all(&mut self, lines: &[&str]) -> Vec<u32> {
        let mut out = Vec::with_capacity(lines.len());
        for line in lines {
            out.push(self.intern(line));
        }
        out
    }
}

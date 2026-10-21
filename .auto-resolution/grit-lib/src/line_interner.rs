//! Fast line interning for xdiff-style Myers pipelines (hash + length, full compare on collision).

use std::collections::HashMap;

/// Intern line text to dense `u32` ids (equal id iff equal line bytes).
#[derive(Default)]
pub struct LineInterner {
    lines: Vec<Box<str>>,
    /// Hash bucket chains (only lines sharing the same crc32+length key).
    index: HashMap<(u64, u32), Vec<u32>>,
}

impl LineInterner {
    /// Return the id for `line`, inserting when unseen.
    #[must_use]
    pub fn intern(&mut self, line: &str) -> u32 {
        let key = (
            crc32fast::hash(line.as_bytes()) as u64,
            u32::try_from(line.len()).unwrap_or(u32::MAX),
        );
        if let Some(candidates) = self.index.get(&key) {
            for &id in candidates {
                if self.lines[id as usize].as_ref() == line {
                    return id;
                }
            }
        }
        let len = self.lines.len();
        if len > u32::MAX as usize {
            return u32::MAX;
        }
        let id = len as u32;
        self.lines.push(line.into());
        self.index.entry(key).or_default().push(id);
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

    /// Intern normalized keys for diff comparison while preserving display via original line slices.
    #[allow(dead_code)]
    pub fn intern_all_normalized<'a>(
        &mut self,
        lines: &[&'a str],
        normalize: impl Fn(&'a str) -> std::borrow::Cow<'a, str>,
    ) -> Vec<u32> {
        let mut out = Vec::with_capacity(lines.len());
        for line in lines {
            let key = normalize(line);
            out.push(self.intern(key.as_ref()));
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::LineInterner;

    #[test]
    fn intern_many_unique_lines_is_linear() {
        let mut interner = LineInterner::default();
        let n = 10_000usize;
        for i in 0..n {
            let line = format!("unique line {i}\n");
            let id = interner.intern(&line);
            assert_eq!(id as usize, i);
        }
        assert_eq!(interner.lines.len(), n);
    }
}

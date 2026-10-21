//! Raw extra header bytes in Git commit and tag objects.
//!
//! Extra headers are the lines between the fixed headers (`author` / `tagger`, …) and the blank
//! line that starts the message body. Each logical header is one or more lines: the first line is
//! `name SP value LF`, and continuation lines start with SP.

/// Advance past one header block (first line plus any SP-prefixed continuations).
///
/// `start` must be the offset of a header line within `extra` (not past its end). Returns
/// `(key, end_offset, _continued)` where `key` is empty for orphan continuation lines.
#[must_use]
pub fn next_header_block(extra: &[u8], start: usize) -> (&[u8], usize, bool) {
    let pos = start;
    let line_start = pos;
    let mut line_end = pos;
    while line_end < extra.len() && extra[line_end] != b'\n' {
        line_end += 1;
    }
    let after_nl = (line_end + 1).min(extra.len());
    let line = &extra[line_start..line_end];
    if line.first() == Some(&b' ') {
        return (b"", after_nl, true);
    }
    let key_end = line.iter().position(|&b| b == b' ').unwrap_or(line.len());
    let key = &line[..key_end];
    let mut end = after_nl;
    while end < extra.len() {
        let ls = end;
        let mut le = end;
        while le < extra.len() && extra[le] != b'\n' {
            le += 1;
        }
        let an = (le + 1).min(extra.len());
        if extra.get(ls) == Some(&b' ') {
            end = an;
        } else {
            break;
        }
    }
    (key, end, false)
}

/// Iterator over `(name, value)` pairs in a raw extra-header blob.
#[derive(Debug, Clone)]
pub struct ExtraHeaderIter<'a> {
    extra: &'a [u8],
    pos: usize,
}

impl<'a> ExtraHeaderIter<'a> {
    /// Iterate header pairs in `extra`.
    #[must_use]
    pub fn new(extra: &'a [u8]) -> Self {
        Self { extra, pos: 0 }
    }
}

/// Iterate `(name, value)` pairs in a raw extra-header blob.
#[must_use]
pub fn iter_extra_headers(extra: &[u8]) -> ExtraHeaderIter<'_> {
    ExtraHeaderIter::new(extra)
}

impl<'a> Iterator for ExtraHeaderIter<'a> {
    type Item = (&'a [u8], &'a [u8]);

    fn next(&mut self) -> Option<Self::Item> {
        if self.pos >= self.extra.len() {
            return None;
        }
        let line_start = self.pos;
        let mut line_end = self.pos;
        while line_end < self.extra.len() && self.extra[line_end] != b'\n' {
            line_end += 1;
        }
        if self.extra[line_start..line_end].first() == Some(&b' ') {
            self.pos = (line_end + 1).min(self.extra.len());
            return self.next();
        }
        let key_end = self.extra[line_start..line_end]
            .iter()
            .position(|&b| b == b' ')
            .unwrap_or(line_end - line_start);
        let key = &self.extra[line_start..line_start + key_end];
        let (_, block_end, _) = next_header_block(self.extra, line_start);
        let value_start = line_start
            + key_end
            + if key_end < line_end - line_start {
                1
            } else {
                0
            };
        let mut value_end = block_end;
        if value_end > value_start && self.extra[value_end - 1] == b'\n' {
            value_end -= 1;
        }
        let value = &self.extra[value_start..value_end];
        self.pos = block_end;
        Some((key, value))
    }
}

/// All values for `name` (one entry per header line with that name).
#[must_use]
pub fn extra_header_values<'a>(extra: &'a [u8], name: &str) -> Vec<&'a [u8]> {
    let name = name.as_bytes();
    ExtraHeaderIter::new(extra)
        .filter(|(k, _)| *k == name)
        .map(|(_, v)| v)
        .collect()
}

/// Remove every header block whose name satisfies `drop_key`.
#[must_use]
pub fn remove_header_lines(extra: &[u8], drop_key: impl Fn(&[u8]) -> bool) -> Vec<u8> {
    let mut out = Vec::new();
    let mut pos = 0usize;
    while pos < extra.len() {
        let (key, block_end, _) = next_header_block(extra, pos);
        if drop_key(key) {
            pos = block_end;
            continue;
        }
        out.extend_from_slice(&extra[pos..block_end]);
        pos = block_end;
    }
    out
}

/// Replace every header block named `name` with `line`, or append `line` when no such block exists.
///
/// `line` must be a full header record (`name SP value LF`, plus any SP-prefixed continuation
/// lines and their trailing LFs) in the same encoding used in [`CommitData::extra_headers`] /
/// [`crate::objects::TagData::extra_headers`].
pub fn set_extra_header(extra: &mut Vec<u8>, name: &str, line: &[u8]) {
    let name_bytes = name.as_bytes();
    *extra = remove_header_lines(extra, |key| key == name_bytes);
    extra.extend_from_slice(line);
}

/// Insert `line` before the first `gpgsig` / `gpgsig-sha256` block, dropping existing blocks
/// whose names satisfy `drop_key`.
#[must_use]
pub fn insert_header_before_signature(
    extra: &[u8],
    line: &[u8],
    drop_key: impl Fn(&[u8]) -> bool,
) -> Vec<u8> {
    let mut out = Vec::new();
    let mut pos = 0usize;
    let mut inserted = false;
    while pos < extra.len() {
        let (key, block_end, _) = next_header_block(extra, pos);
        if !inserted && (key == b"gpgsig" || key == b"gpgsig-sha256") {
            out.extend_from_slice(line);
            inserted = true;
        }
        if drop_key(key) {
            pos = block_end;
            continue;
        }
        out.extend_from_slice(&extra[pos..block_end]);
        pos = block_end;
    }
    if !inserted {
        out.extend_from_slice(line);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn iter_multivalued_and_continuation() {
        let raw = b"foo one\nfoo two\nbar multi\n line\n";
        let pairs: Vec<_> = ExtraHeaderIter::new(raw).collect();
        assert_eq!(pairs.len(), 3);
        assert_eq!(pairs[0], (b"foo".as_slice(), b"one".as_slice()));
        assert_eq!(pairs[1], (b"foo".as_slice(), b"two".as_slice()));
        assert_eq!(pairs[2], (b"bar".as_slice(), b"multi\n line".as_slice()));
        assert_eq!(
            extra_header_values(raw, "foo"),
            vec![b"one".as_slice(), b"two".as_slice()]
        );
    }

    #[test]
    fn set_replaces_all_same_name() {
        let mut extra = b"foo old\nbar keep\nfoo also\n".to_vec();
        set_extra_header(&mut extra, "foo", b"foo new\n");
        assert_eq!(extra, b"bar keep\nfoo new\n");
    }

    #[test]
    fn insert_before_gpgsig_drops_matching() {
        let extra = b"change-id old\nencoding UTF-8\n";
        let out = insert_header_before_signature(extra, b"change-id new\n", |key| {
            key == b"change-id" || key == b"gitbutler-change-id"
        });
        assert_eq!(out, b"encoding UTF-8\nchange-id new\n");

        let signed = b"change-id x\nencoding UTF-8\ngpgsig sig\n line\n";
        let out2 =
            insert_header_before_signature(signed, b"change-id y\n", |key| key == b"change-id");
        assert_eq!(out2, b"encoding UTF-8\nchange-id y\ngpgsig sig\n line\n");
    }
}

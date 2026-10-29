//! Borrowed view of a commit object body ([`CommitRef`]) with on-demand decoding.
//!
//! [`CommitRef::parse`] is the single commit parser; [`super::parse_commit`] is
//! `CommitRef::parse(data)?.to_owned()`.

use std::borrow::Cow;

use crate::commit_encoding;
use crate::error::{Error, Result};
use crate::ident::{
    committer_timestamp_for_until_filter_bytes, committer_unix_seconds_for_ordering_bytes,
};
use smallvec::SmallVec;

use super::{parse_oid_field, CommitData, HashAlgo, ObjectId};

/// Parsed commit headers and message body borrowing from the object bytes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommitRef<'a> {
    headers_block: &'a [u8],
    message: &'a [u8],
    tree: ObjectId,
    parents: SmallVec<[ObjectId; 2]>,
    author_raw: Cow<'a, [u8]>,
    committer_raw: Cow<'a, [u8]>,
    encoding: Option<&'a [u8]>,
}

impl<'a> CommitRef<'a> {
    /// Parse a commit object without allocating author, committer, or message strings.
    ///
    /// # Errors
    ///
    /// Returns [`Error::CorruptObject`] when required headers are missing or malformed.
    ///
    /// A header block that ends at EOF (no blank line before the message) is accepted with
    /// an empty message, matching Git's commit parser.
    pub fn parse(data: &'a [u8]) -> Result<Self> {
        parse_one_pass(data)
    }

    /// Root tree object id.
    #[must_use]
    pub fn tree(&self) -> ObjectId {
        self.tree
    }

    /// Parent commit ids in header order.
    #[must_use]
    pub fn parents(&self) -> &[ObjectId] {
        &self.parents
    }

    /// Raw `author` header payload (after `author `), with continuation lines merged.
    #[must_use]
    pub fn author_raw(&self) -> &[u8] {
        &self.author_raw
    }

    /// Raw `committer` header payload (after `committer `), with continuation lines merged.
    #[must_use]
    pub fn committer_raw(&self) -> &[u8] {
        &self.committer_raw
    }

    /// Optional `encoding` header value bytes when present and UTF-8.
    #[must_use]
    pub fn encoding_raw(&self) -> Option<&[u8]> {
        self.encoding
    }

    /// Extra header bytes (`gpgsig`, `mergetag`, `change-id`, …) excluding `tree`/`parent`/`author`/`committer`/`encoding`.
    #[must_use]
    pub fn extra_headers(&self) -> Cow<'_, [u8]> {
        collect_extra_header_bytes(self.headers_block)
    }

    /// Commit message body bytes (everything after the blank line).
    #[must_use]
    pub fn message_bytes(&self) -> &[u8] {
        self.message
    }

    fn encoding_label(&self) -> Option<&str> {
        self.encoding.and_then(|b| std::str::from_utf8(b).ok())
    }

    /// Decoded author identity line (uses [`Self::encoding_raw`] when set).
    #[must_use]
    pub fn author(&self) -> String {
        commit_encoding::decode_bytes(self.encoding_label(), &self.author_raw)
    }

    /// Decoded committer identity line (uses [`Self::encoding_raw`] when set).
    #[must_use]
    pub fn committer(&self) -> String {
        commit_encoding::decode_bytes(self.encoding_label(), &self.committer_raw)
    }

    /// Decoded commit message (uses [`Self::encoding_raw`] when set).
    #[must_use]
    pub fn message(&self) -> Cow<'_, str> {
        decode_message(self.encoding.is_some(), self.encoding_label(), self.message)
    }

    /// Committer unix seconds for `--until` / `--since` filtering (unknown → `0`).
    #[must_use]
    pub fn committer_timestamp(&self) -> i64 {
        committer_timestamp_for_until_filter_bytes(&self.committer_raw)
    }

    /// Committer unix seconds for rev-list date ordering (unknown → `0`).
    #[must_use]
    pub fn committer_unix_seconds_for_ordering(&self) -> i64 {
        committer_unix_seconds_for_ordering_bytes(&self.committer_raw)
    }

    /// Author unix seconds for rev-list author-date ordering (unknown → `0`).
    #[must_use]
    pub fn author_unix_seconds_for_ordering(&self) -> i64 {
        committer_unix_seconds_for_ordering_bytes(&self.author_raw)
    }

    /// Re-serialize using the original header block (byte-identical when the source object was valid).
    #[must_use]
    pub fn to_bytes(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(self.headers_block.len() + 1 + self.message.len());
        out.extend_from_slice(self.headers_block);
        out.push(b'\n');
        out.extend_from_slice(self.message);
        out
    }

    /// Build an owning [`CommitData`] matching [`super::parse_commit`] output.
    #[must_use]
    pub fn to_owned(&self) -> CommitData {
        let encoding = self.encoding_label().map(str::to_owned);
        let message = self.message();
        let body = self.message;
        let has_non_utf8_encoding = encoding.as_deref().is_some_and(|label| {
            !label.eq_ignore_ascii_case("utf-8") && !label.eq_ignore_ascii_case("utf8")
        });
        let raw_message = if body.is_empty() {
            None
        } else if has_non_utf8_encoding
            || std::str::from_utf8(body).is_err()
            || !body.ends_with(b"\n")
        {
            Some(body.to_vec())
        } else {
            None
        };
        CommitData {
            tree: self.tree,
            parents: self.parents.iter().copied().collect(),
            author: self.author(),
            committer: self.committer(),
            author_raw: self.author_raw.to_vec(),
            committer_raw: self.committer_raw.to_vec(),
            encoding,
            message: message.into_owned(),
            raw_message,
            extra_headers: self.extra_headers().into_owned(),
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum HeaderKind {
    Tree,
    Parent,
    Author,
    Committer,
    Encoding,
    Extra,
    Malformed,
}

fn classify_header_line(line: &[u8]) -> HeaderKind {
    if line.starts_with(b"tree ") {
        HeaderKind::Tree
    } else if line.starts_with(b"parent ") {
        HeaderKind::Parent
    } else if line.starts_with(b"author ") {
        HeaderKind::Author
    } else if line.starts_with(b"committer ") {
        HeaderKind::Committer
    } else if line.starts_with(b"encoding ") {
        HeaderKind::Encoding
    } else if line.is_empty() || line.first() == Some(&b' ') {
        HeaderKind::Malformed
    } else {
        HeaderKind::Extra
    }
}

fn header_value(line: &[u8], prefix_len: usize) -> &[u8] {
    line.get(prefix_len..).unwrap_or_default()
}

fn parse_oid_fast(value: &[u8]) -> Result<ObjectId> {
    if value.len() == HashAlgo::Sha1.hex_len() {
        return ObjectId::from_hex_ascii(value);
    }
    parse_oid_field(value)
}

/// Header fields found by [`scan_commit`]; required ones may be absent in lenient mode.
struct CommitScan<'a> {
    headers_block: &'a [u8],
    message: &'a [u8],
    tree: Option<ObjectId>,
    parents: SmallVec<[ObjectId; 2]>,
    author_raw: Option<Cow<'a, [u8]>>,
    committer_raw: Option<Cow<'a, [u8]>>,
    encoding: Option<&'a [u8]>,
}

/// One pass over the commit headers.
///
/// Strict mode rejects malformed header lines, unparsable ids, orphan continuations and a
/// non-UTF-8 `encoding`. Lenient mode skips those lines instead, the way Git's display code
/// reads whatever headers a commit does have.
fn scan_commit(data: &[u8], lenient: bool) -> Result<CommitScan<'_>> {
    #[derive(Clone, Copy)]
    enum Continuation {
        Author,
        Committer,
        MultilineExtra,
        Ignore,
    }

    let mut pos = 0usize;
    let mut tree = None;
    let mut parents = SmallVec::<[ObjectId; 2]>::new();
    let mut author_raw: Option<Cow<'_, [u8]>> = None;
    let mut committer_raw: Option<Cow<'_, [u8]>> = None;
    let mut encoding: Option<&[u8]> = None;
    let mut cont = Continuation::Ignore;

    let mut headers_end = 0usize;
    let mut message: &[u8] = &[];
    let mut has_blank_line = false;

    while pos < data.len() {
        let line_start = pos;
        let mut line_end = pos;
        while line_end < data.len() && data[line_end] != b'\n' {
            line_end += 1;
        }
        let line = &data[line_start..line_end];
        let after_nl = (line_end + 1).min(data.len());

        if line.is_empty() {
            headers_end = line_start;
            message = data.get(after_nl..).unwrap_or_default();
            has_blank_line = true;
            break;
        }

        if line.first() == Some(&b' ') {
            let rest = line.get(1..).unwrap_or_default();
            let merged = match cont {
                Continuation::Author => merge_identity_continuation(&mut author_raw, rest),
                Continuation::Committer => merge_identity_continuation(&mut committer_raw, rest),
                Continuation::MultilineExtra | Continuation::Ignore => Ok(()),
            };
            if !lenient {
                merged?;
            }
            pos = after_nl;
            continue;
        }

        match classify_header_line(line) {
            HeaderKind::Tree => {
                match parse_oid_fast(header_value(line, 5)) {
                    Ok(oid) => tree = Some(oid),
                    Err(err) if !lenient => return Err(err),
                    Err(_) => {}
                }
                cont = Continuation::Ignore;
            }
            HeaderKind::Parent => {
                match parse_oid_fast(header_value(line, 7)) {
                    Ok(oid) => parents.push(oid),
                    Err(err) if !lenient => return Err(err),
                    Err(_) => {}
                }
                cont = Continuation::Ignore;
            }
            HeaderKind::Author => {
                author_raw = Some(Cow::Borrowed(header_value(line, 7)));
                cont = Continuation::Author;
            }
            HeaderKind::Committer => {
                committer_raw = Some(Cow::Borrowed(header_value(line, 10)));
                cont = Continuation::Committer;
            }
            HeaderKind::Encoding => {
                let rest = header_value(line, 9);
                if std::str::from_utf8(rest).is_ok() {
                    encoding = Some(rest);
                } else if !lenient {
                    return Err(Error::CorruptObject("encoding is not UTF-8".to_owned()));
                }
                cont = Continuation::Ignore;
            }
            HeaderKind::Extra => {
                cont = Continuation::MultilineExtra;
            }
            HeaderKind::Malformed => {
                if !lenient {
                    return Err(Error::CorruptObject(
                        "malformed commit header line".to_owned(),
                    ));
                }
                cont = Continuation::Ignore;
            }
        }
        pos = after_nl;
    }

    if !has_blank_line {
        headers_end = data.len();
    }
    Ok(CommitScan {
        headers_block: &data[..headers_end],
        message,
        tree,
        parents,
        author_raw,
        committer_raw,
        encoding,
    })
}

fn parse_one_pass<'a>(data: &'a [u8]) -> Result<CommitRef<'a>> {
    let scan = scan_commit(data, false)?;
    let author_bytes = scan
        .author_raw
        .ok_or_else(|| Error::CorruptObject("commit missing author header".to_owned()))?;
    let committer_bytes = scan
        .committer_raw
        .ok_or_else(|| Error::CorruptObject("commit missing committer header".to_owned()))?;

    Ok(CommitRef {
        headers_block: scan.headers_block,
        message: scan.message,
        tree: scan
            .tree
            .ok_or_else(|| Error::CorruptObject("commit missing tree header".to_owned()))?,
        parents: scan.parents,
        author_raw: author_bytes,
        committer_raw: committer_bytes,
        encoding: scan.encoding,
    })
}

/// Author identity and message of a commit, read leniently.
///
/// Unlike [`CommitRef::parse`], a missing `author`/`committer` header, a malformed or
/// unparsable header line, or a missing blank line does not fail: the author is `None` when
/// absent and the message is empty without a blank line. Decoding (including `encoding`)
/// matches [`CommitRef::author`] and [`CommitRef::message`], so well-formed commits yield the
/// same strings as [`super::parse_commit`].
pub(crate) fn commit_author_and_message_lenient(data: &[u8]) -> (Option<String>, String) {
    let Ok(scan) = scan_commit(data, true) else {
        return (None, String::new());
    };
    let label = scan.encoding.and_then(|b| std::str::from_utf8(b).ok());
    let author = scan
        .author_raw
        .map(|raw| commit_encoding::decode_bytes(label, &raw));
    let message = decode_message(scan.encoding.is_some(), label, scan.message).into_owned();
    (author, message)
}

fn decode_message<'a>(has_encoding: bool, label: Option<&str>, body: &'a [u8]) -> Cow<'a, str> {
    if has_encoding {
        return Cow::Owned(commit_encoding::decode_bytes(label, body));
    }
    match std::str::from_utf8(body) {
        Ok(s) => Cow::Borrowed(s),
        Err(_) => Cow::Owned(commit_encoding::decode_bytes(None, body)),
    }
}

fn collect_extra_header_bytes(headers_block: &[u8]) -> Cow<'_, [u8]> {
    let mut extra = ExtraScan::default();
    let mut pos = 0usize;
    #[derive(Clone, Copy)]
    enum Cont {
        MultilineExtra,
        Ignore,
    }
    let mut cont = Cont::Ignore;
    while pos < headers_block.len() {
        let line_start = pos;
        let mut line_end = pos;
        while line_end < headers_block.len() && headers_block[line_end] != b'\n' {
            line_end += 1;
        }
        let line = &headers_block[line_start..line_end];
        let after_nl = (line_end + 1).min(headers_block.len());
        if line.first() == Some(&b' ') {
            if matches!(cont, Cont::MultilineExtra) {
                extra.push_line(line_start, after_nl);
            }
            pos = after_nl;
            continue;
        }
        let kind = classify_header_line(line);
        match kind {
            HeaderKind::Tree | HeaderKind::Parent | HeaderKind::Author | HeaderKind::Committer => {
                cont = Cont::Ignore;
                extra.mark_structured_gap();
            }
            HeaderKind::Encoding => {
                cont = Cont::Ignore;
                extra.mark_structured_gap();
            }
            HeaderKind::Extra => {
                cont = Cont::MultilineExtra;
                extra.push_line(line_start, after_nl);
            }
            HeaderKind::Malformed => {
                cont = Cont::Ignore;
            }
        }
        pos = after_nl;
    }
    extra.finish(headers_block)
}

#[derive(Default)]
struct ExtraScan {
    start: Option<usize>,
    end: usize,
    structured_gap: bool,
    piece_ranges: SmallVec<[(usize, usize); 4]>,
}

impl ExtraScan {
    fn mark_structured_gap(&mut self) {
        if self.start.is_some() {
            self.structured_gap = true;
        }
    }

    fn push_line(&mut self, line_start: usize, after_nl: usize) {
        if self.start.is_none() {
            self.start = Some(line_start);
            self.end = after_nl;
            return;
        }
        if !self.structured_gap && self.end == line_start {
            self.end = after_nl;
        } else {
            self.structured_gap = true;
            self.piece_ranges.push((line_start, after_nl));
        }
    }

    fn finish<'a>(self, block: &'a [u8]) -> Cow<'a, [u8]> {
        if self.start.is_none() && self.piece_ranges.is_empty() {
            return Cow::Borrowed(&[]);
        }
        if self.structured_gap || !self.piece_ranges.is_empty() {
            let mut out = Vec::new();
            if let Some(start) = self.start {
                out.extend_from_slice(&block[start..self.end]);
            }
            for &(from, to) in &self.piece_ranges {
                out.extend_from_slice(&block[from..to]);
            }
            Cow::Owned(out)
        } else if let Some(start) = self.start {
            Cow::Borrowed(&block[start..self.end])
        } else {
            Cow::Borrowed(&[])
        }
    }
}

fn merge_identity_continuation(slot: &mut Option<Cow<'_, [u8]>>, rest: &[u8]) -> Result<()> {
    let merged = slot
        .as_mut()
        .ok_or_else(|| Error::CorruptObject("orphan header continuation".to_owned()))?;
    match merged {
        Cow::Borrowed(first) => {
            let mut buf = first.to_vec();
            buf.extend_from_slice(rest);
            *merged = Cow::Owned(buf);
        }
        Cow::Owned(buf) => buf.extend_from_slice(rest),
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lenient_author_and_message_match_strict_parse_on_valid_commits() {
        let latin1: &[u8] = b"tree 4b825dc642cb6eb9a060e54bf8d69288fbee4904\n\
            author J\xf6rg <j@example.com> 1 +0100\n\
            committer C <c@example.com> 2 +0000\n\
            encoding ISO-8859-1\n\
            \n\
            Gr\xfc\xdfe\n";
        let continued: &[u8] = b"tree 4b825dc642cb6eb9a060e54bf8d69288fbee4904\n\
            author A\n Continued <a@example.com> 1 +0000\n\
            committer C <c@example.com> 2 +0000\n\
            gpgsig -----BEGIN-----\n sig\n -----END-----\n\
            \n\
            subject\n\nbody\n";
        for data in [latin1, continued] {
            let strict = CommitRef::parse(data).expect("valid commit");
            let (author, message) = commit_author_and_message_lenient(data);
            assert_eq!(author.as_deref(), Some(strict.author().as_str()));
            assert_eq!(message, strict.message());
        }
    }

    #[test]
    fn lenient_author_and_message_tolerate_missing_and_malformed_headers() {
        let tree = "tree 4b825dc642cb6eb9a060e54bf8d69288fbee4904\n";
        let no_committer = format!("{tree}author A <a@x> 1700000000 +0000\n\nmessage\n");
        assert!(CommitRef::parse(no_committer.as_bytes()).is_err());
        assert_eq!(
            commit_author_and_message_lenient(no_committer.as_bytes()),
            (
                Some("A <a@x> 1700000000 +0000".to_owned()),
                "message\n".to_owned()
            )
        );

        let no_author = format!("{tree}committer C <c@x> 2 +0000\n\n\nsubject\n");
        assert!(CommitRef::parse(no_author.as_bytes()).is_err());
        assert_eq!(
            commit_author_and_message_lenient(no_author.as_bytes()),
            (None, "\nsubject\n".to_owned())
        );

        let malformed =
            format!("{tree}parent not-an-oid\n\n bad continuation\nauthor A <a@x> 1 +0000\n\nm\n");
        assert_eq!(
            commit_author_and_message_lenient(malformed.as_bytes()).1,
            " bad continuation\nauthor A <a@x> 1 +0000\n\nm\n",
            "the first blank line still ends the headers"
        );

        let mut odd_lines = tree.as_bytes().to_vec();
        odd_lines.extend_from_slice(b"encoding \xff\n orphan\nauthor A <a@x> 1 +0000\n\nm\n");
        assert!(CommitRef::parse(&odd_lines).is_err());
        let (author, message) = commit_author_and_message_lenient(&odd_lines);
        assert_eq!(author.as_deref(), Some("A <a@x> 1 +0000"));
        assert_eq!(message, "m\n");

        let no_blank = format!("{tree}author A <a@x> 1 +0000\ncommitter C <c@x> 2 +0000\n");
        let parsed = CommitRef::parse(no_blank.as_bytes()).expect("headers-only commit");
        assert_eq!(parsed.message(), "");
        assert_eq!(
            commit_author_and_message_lenient(no_blank.as_bytes()),
            (Some("A <a@x> 1 +0000".to_owned()), String::new())
        );
        assert_eq!(
            commit_author_and_message_lenient(b""),
            (None, String::new())
        );
    }

    #[test]
    fn commit_ref_roundtrip_vectors() {
        let cases: &[&[u8]] = &[
            concat!(
                "tree 4b825dc642cb6eb9a060e54bf8d69288fbee4904\n",
                "parent 1111111111111111111111111111111111111111\n",
                "author A <a@example.com> 1 +0000\n",
                "committer C <c@example.com> 2 +0000\n",
                "\n",
                "ignored body\n",
            )
            .as_bytes(),
            concat!(
                "tree 4b825dc642cb6eb9a060e54bf8d69288fbee4904\n",
                "author A U Thor <author@example.com> 1 +0000\n",
                "committer C O Mitter <committer@example.com> 1 +0000\n",
                "gpgsig -----BEGIN PGP SIGNATURE-----\n",
                " abcdef\n",
                " -----END PGP SIGNATURE-----\n",
                "\n",
                "msg\n",
            )
            .as_bytes(),
            concat!(
                "tree 4b825dc642cb6eb9a060e54bf8d69288fbee4904\n",
                "author A <a@example.com> 1 +0000\n",
                "committer C <c@example.com> 1 +0000\n",
                "encoding ISO-8859-1\n",
                "gpgsig -----BEGIN PGP SIGNATURE-----\n",
                " line2\n",
                " -----END PGP SIGNATURE-----\n",
                "mergetag object 4b825dc642cb6eb9a060e54bf8d69288fbee4904\n",
                " type commit\n",
                " tag merge-tag\n",
                " tagger T <t@example.com> 1 +0000\n",
                " -----BEGIN PGP SIGNATURE-----\n",
                " inner\n",
                " -----END PGP SIGNATURE-----\n",
                "change-id test-change-id\n",
                "\n",
                "msg-only\n",
            )
            .as_bytes(),
            concat!(
                "tree 4b825dc642cb6eb9a060e54bf8d69288fbee4904\n",
                "author A <a@example.com> 1 +0000\n",
                "committer C <c@example.com> 1 +0000\n",
                "change-id interleaved-change\n",
                "encoding UTF-8\n",
                "gpgsig -----BEGIN PGP SIGNATURE-----\n",
                " sig\n",
                " -----END PGP SIGNATURE-----\n",
                "\n",
                "body\n",
            )
            .as_bytes(),
            b"tree 4b825dc642cb6eb9a060e54bf8d69288fbee4904\n\
author A <a@example.com> 1 +0000\n\
committer C <c@example.com> 1 +0000\n\
\n\
no-final-newline",
        ];
        for raw in cases {
            let parsed = super::super::parse_commit(raw).expect("parse_commit");
            let cref = CommitRef::parse(raw).expect("CommitRef::parse");
            assert_eq!(cref.to_owned(), parsed, "to_owned mismatch");
            assert_eq!(cref.to_bytes(), *raw, "byte-identical serialize");
        }
    }

    fn median(mut samples: Vec<u128>) -> u128 {
        samples.sort_unstable();
        samples[samples.len() / 2]
    }

    fn bench_commit_bodies(count: usize) -> Vec<Vec<u8>> {
        let empty = "4b825dc642cb6eb9a060e54bf8d69288fbee4904";
        let ident = "Bench <b@example.com> 1700000000 +0000";
        let mut bodies = Vec::with_capacity(count);
        let mut parent = empty.to_owned();
        for i in 0..count {
            let body = format!(
                "tree {empty}\nparent {parent}\nauthor {ident}\ncommitter {ident}\nchange-id bench-{i:06}\n\nm{i}\n"
            );
            parent = format!("{:040x}", i + 1);
            bodies.push(body.into_bytes());
        }
        bodies
    }

    #[test]
    fn commit_ref_parse_at_least_2x_faster_than_parse_commit_100k() {
        if cfg!(debug_assertions) {
            return;
        }
        let bodies = bench_commit_bodies(100_000);
        for body in &bodies[..128] {
            let _ = CommitRef::parse(body).expect("parse");
            let _ = super::super::parse_commit(body).expect("parse_commit");
        }

        let mut parse_commit_medians = Vec::with_capacity(9);
        let mut commit_ref_medians = Vec::with_capacity(9);
        for run in 0..9 {
            let use_commit_ref = run % 2 == 0;
            let start = std::time::Instant::now();
            if use_commit_ref {
                for body in &bodies {
                    std::hint::black_box(CommitRef::parse(body).expect("parse"));
                }
            } else {
                for body in &bodies {
                    std::hint::black_box(super::super::parse_commit(body).expect("parse_commit"));
                }
            }
            let elapsed = start.elapsed().as_nanos();
            if use_commit_ref {
                commit_ref_medians.push(elapsed);
            } else {
                parse_commit_medians.push(elapsed);
            }
        }
        let parse_commit_median = median(parse_commit_medians);
        let commit_ref_median = median(commit_ref_medians);
        let ratio = parse_commit_median as f64 / commit_ref_median as f64;
        assert!(
            ratio >= 2.0,
            "CommitRef::parse median {commit_ref_median}ns vs parse_commit {parse_commit_median}ns (ratio {ratio:.2}x, need >=2x)"
        );
    }
}

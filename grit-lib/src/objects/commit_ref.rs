//! Borrowed view of a commit object body ([`CommitRef`]) with on-demand decoding.
//!
//! [`CommitRef::parse`] is the single commit parser; [`super::parse_commit`] is
//! `CommitRef::parse(data)?.to_owned()`.

use std::borrow::Cow;

use crate::commit_encoding;
use crate::error::{Error, Result};
use crate::ident::committer_timestamp_for_until_filter;

use super::{parse_oid_field, CommitData, ObjectId};

/// Parsed commit headers and message body borrowing from the object bytes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommitRef<'a> {
    tree: ObjectId,
    parents: Vec<ObjectId>,
    author_raw: Cow<'a, [u8]>,
    committer_raw: Cow<'a, [u8]>,
    encoding: Option<Cow<'a, [u8]>>,
    extra_headers: &'a [u8],
    message: &'a [u8],
}

impl<'a> CommitRef<'a> {
    /// Parse a commit object without allocating author, committer, or message strings.
    ///
    /// # Errors
    ///
    /// Returns [`Error::CorruptObject`] when required headers are missing or malformed.
    pub fn parse(data: &'a [u8]) -> Result<Self> {
        #[derive(Clone, Copy)]
        enum Continuation {
            Author,
            Committer,
            Multiline,
            Ignore,
        }

        let mut pos = 0usize;
        let mut tree = None;
        let mut parents = Vec::new();
        let mut author_raw: Option<Cow<'a, [u8]>> = None;
        let mut committer_raw: Option<Cow<'a, [u8]>> = None;
        let mut encoding: Option<Cow<'a, [u8]>> = None;
        let mut extra_start: Option<usize> = None;
        let mut extra_end = 0usize;
        let mut cont = Continuation::Ignore;

        let mut push_extra = |line_start: usize, after_nl: usize| {
            if extra_start.is_none() {
                extra_start = Some(line_start);
            }
            extra_end = after_nl;
        };

        while pos < data.len() {
            let line_start = pos;
            let mut line_end = pos;
            while line_end < data.len() && data[line_end] != b'\n' {
                line_end += 1;
            }
            let line = &data[line_start..line_end];
            let after_nl = (line_end + 1).min(data.len());
            if line.is_empty() {
                let message = data.get(after_nl..).unwrap_or_default();
                let author_bytes = author_raw.ok_or_else(|| {
                    Error::CorruptObject("commit missing author header".to_owned())
                })?;
                let committer_bytes = committer_raw.ok_or_else(|| {
                    Error::CorruptObject("commit missing committer header".to_owned())
                })?;
                let extra_headers = match extra_start {
                    Some(start) => data.get(start..extra_end).unwrap_or_default(),
                    None => &[],
                };
                return Ok(Self {
                    tree: tree.ok_or_else(|| {
                        Error::CorruptObject("commit missing tree header".to_owned())
                    })?,
                    parents,
                    author_raw: author_bytes,
                    committer_raw: committer_bytes,
                    encoding,
                    extra_headers,
                    message,
                });
            }

            if line.first() == Some(&b' ') {
                let rest = line.get(1..).unwrap_or_default();
                match cont {
                    Continuation::Author => {
                        let a = author_raw.as_mut().ok_or_else(|| {
                            Error::CorruptObject("orphan header continuation".to_owned())
                        })?;
                        match a {
                            Cow::Borrowed(first) => {
                                let mut merged = first.to_vec();
                                merged.extend_from_slice(rest);
                                *a = Cow::Owned(merged);
                            }
                            Cow::Owned(merged) => merged.extend_from_slice(rest),
                        }
                    }
                    Continuation::Committer => {
                        let c = committer_raw.as_mut().ok_or_else(|| {
                            Error::CorruptObject("orphan header continuation".to_owned())
                        })?;
                        match c {
                            Cow::Borrowed(first) => {
                                let mut merged = first.to_vec();
                                merged.extend_from_slice(rest);
                                *c = Cow::Owned(merged);
                            }
                            Cow::Owned(merged) => merged.extend_from_slice(rest),
                        }
                    }
                    Continuation::Multiline => {
                        push_extra(line_start, after_nl);
                    }
                    Continuation::Ignore => {}
                }
                pos = after_nl;
                continue;
            }

            let key_end = line
                .iter()
                .position(|&b| b == b' ')
                .ok_or_else(|| Error::CorruptObject("malformed commit header line".to_owned()))?;
            let key = &line[..key_end];
            let rest = line.get(key_end + 1..).unwrap_or_default();

            match key {
                b"tree" => {
                    tree = Some(parse_oid_field(rest)?);
                    cont = Continuation::Ignore;
                }
                b"parent" => {
                    parents.push(parse_oid_field(rest)?);
                    cont = Continuation::Ignore;
                }
                b"author" => {
                    author_raw = Some(Cow::Borrowed(rest));
                    cont = Continuation::Author;
                }
                b"committer" => {
                    committer_raw = Some(Cow::Borrowed(rest));
                    cont = Continuation::Committer;
                }
                b"encoding" => {
                    std::str::from_utf8(rest)
                        .map_err(|_| Error::CorruptObject("encoding is not UTF-8".to_owned()))?;
                    encoding = Some(Cow::Borrowed(rest));
                    cont = Continuation::Ignore;
                }
                _ => {
                    cont = Continuation::Multiline;
                    push_extra(line_start, after_nl);
                }
            }
            pos = after_nl;
        }

        Err(Error::CorruptObject(
            "commit missing blank line before message".to_owned(),
        ))
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

    /// Iterate parent commit ids.
    pub fn parents_iter(&self) -> impl Iterator<Item = &ObjectId> + '_ {
        self.parents.iter()
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
        self.encoding.as_deref()
    }

    /// Extra header block (`gpgsig`, `mergetag`, `change-id`, …) including line terminators.
    #[must_use]
    pub fn extra_headers(&self) -> &[u8] {
        self.extra_headers
    }

    /// Commit message body bytes (everything after the blank line).
    #[must_use]
    pub fn message_bytes(&self) -> &[u8] {
        self.message
    }

    fn encoding_label(&self) -> Option<&str> {
        self.encoding_raw()
            .and_then(|b| std::str::from_utf8(b).ok())
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
        Cow::Owned(commit_encoding::decode_bytes(
            self.encoding_label(),
            self.message,
        ))
    }

    /// Committer unix seconds for `--until` / `--since` filtering (unknown → `0`).
    #[must_use]
    pub fn committer_timestamp(&self) -> i64 {
        committer_timestamp_for_until_filter(&self.committer())
    }

    /// Committer unix seconds for rev-list date ordering (unknown → `0`).
    #[must_use]
    pub fn committer_unix_seconds_for_ordering(&self) -> i64 {
        crate::ident::committer_unix_seconds_for_ordering(&self.committer())
    }

    /// Author unix seconds for rev-list author-date ordering (unknown → `0`).
    #[must_use]
    pub fn author_unix_seconds_for_ordering(&self) -> i64 {
        crate::ident::committer_unix_seconds_for_ordering(&self.author())
    }

    /// Serialize to the same bytes as [`super::serialize_commit`] on [`Self::to_owned`].
    #[must_use]
    pub fn to_bytes(&self) -> Vec<u8> {
        let owned = self.to_owned();
        super::serialize_commit(&owned)
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
            parents: self.parents.clone(),
            author: self.author(),
            committer: self.committer(),
            author_raw: self.author_raw.to_vec(),
            committer_raw: self.committer_raw.to_vec(),
            encoding,
            message: message.into_owned(),
            raw_message,
            extra_headers: self.extra_headers.to_vec(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
            b"tree 4b825dc642cb6eb9a060e54bf8d69288fbee4904\n\
author A <a@example.com> 1 +0000\n\
committer C <c@example.com> 1 +0000\n\
\n\
no-final-newline",
        ];
        for raw in cases {
            let parsed = super::super::parse_commit(raw).expect("parse_commit");
            let cref = CommitRef::parse(raw).expect("CommitRef::parse");
            assert_eq!(cref.to_owned(), parsed);
            assert_eq!(cref.to_bytes(), *raw);
        }
    }
}

//! Standalone object fsck for `hash-object` and similar entry points.
//!
//! Mirrors the buffer-safe checks in Git's `fsck.c` (`verify_headers`,
//! `fsck_commit`, `fsck_tag_standalone`, `fsck_tree`) so error messages match
//! `error: object fails fsck: <camelCaseId>: <detail>`.

use crate::check_ref_format::{check_refname_format, RefNameOptions};
use crate::git_date::tm::date_overflows;
use crate::objects::{ObjectId, ObjectKind};

/// Git-compatible fsck failure for loose object validation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FsckError {
    /// CamelCase message id (e.g. `missingTree`).
    pub id: &'static str,
    /// Human-readable detail after `id: `.
    pub detail: String,
}

impl FsckError {
    /// Construct an fsck diagnostic (library tests and `mktag` use this for uniform messages).
    #[must_use]
    pub fn new(id: &'static str, detail: impl Into<String>) -> Self {
        Self {
            id,
            detail: detail.into(),
        }
    }

    /// Full line after `error: object fails fsck: ` (matches Git).
    #[must_use]
    pub fn report_line(&self) -> String {
        format!("{}: {}", self.id, self.detail)
    }
}

/// Validate raw object bytes the same way `git hash-object` does before hashing.
///
/// Returns `Ok(())` when the object is well-formed, or the first fsck error Git
/// would report for truncated or malformed buffers.
pub fn fsck_object(kind: ObjectKind, data: &[u8]) -> Result<(), FsckError> {
    match kind {
        ObjectKind::Blob => Ok(()),
        ObjectKind::Commit => fsck_commit(data),
        ObjectKind::Tag => fsck_tag(data),
        ObjectKind::Tree => fsck_tree(data),
    }
}

fn verify_headers(data: &[u8], nul_msg_id: &'static str) -> Result<(), FsckError> {
    for (i, &b) in data.iter().enumerate() {
        if b == 0 {
            return Err(FsckError::new(
                nul_msg_id,
                format!("unterminated header: NUL at offset {i}"),
            ));
        }
        if b == b'\n' && i + 1 < data.len() && data[i + 1] == b'\n' {
            return Ok(());
        }
    }
    if !data.is_empty() && data[data.len() - 1] == b'\n' {
        Ok(())
    } else {
        Err(FsckError::new("unterminatedHeader", "unterminated header"))
    }
}

fn is_hex_lower(b: u8) -> bool {
    matches!(b, b'0'..=b'9' | b'a'..=b'f')
}

/// Parse a lowercase hex object id at the start of `buf` (40 chars for SHA-1 or
/// 64 for SHA-256), requiring the next byte to be `\n`. Returns bytes consumed
/// (hex width + 1).
fn parse_oid_line(buf: &[u8], bad_sha1_id: &'static str) -> Result<usize, FsckError> {
    let bad = || {
        FsckError::new(
            bad_sha1_id,
            format!(
                "invalid '{}' line format - bad sha1",
                line_kind(bad_sha1_id)
            ),
        )
    };
    // The hex width follows the repository hash (a `\n` terminates the id).
    let hex_len = buf.iter().position(|&b| b == b'\n').ok_or_else(bad)?;
    if !ObjectId::is_hex_len(hex_len) {
        return Err(bad());
    }
    let hex = &buf[..hex_len];
    if !hex.iter().copied().all(is_hex_lower) {
        return Err(bad());
    }
    let hex_str = std::str::from_utf8(hex).map_err(|_| bad())?;
    hex_str.parse::<ObjectId>().map_err(|_| bad())?;
    Ok(hex_len + 1)
}

fn line_kind(bad_sha1_id: &'static str) -> &'static str {
    match bad_sha1_id {
        "badObjectSha1" => "object",
        "badParentSha1" => "parent",
        _ => "tree",
    }
}

fn fsck_ident(
    data: &[u8],
    start: usize,
    buffer_end: usize,
    oid_line: &'static str,
) -> Result<usize, FsckError> {
    let mut p = start;
    if p >= buffer_end {
        return Err(FsckError::new(
            "missingEmail",
            format!("invalid {oid_line} line - missing email"),
        ));
    }

    let line_end = data[p..buffer_end]
        .iter()
        .position(|&b| b == b'\n')
        .map(|rel| p + rel)
        .ok_or_else(|| {
            FsckError::new(
                "missingEmail",
                format!("invalid {oid_line} line - missing email"),
            )
        })?;

    let ident_end = line_end;

    if data[p] == b'<' {
        return Err(FsckError::new(
            "missingNameBeforeEmail",
            format!("invalid {oid_line} line - missing space before email"),
        ));
    }

    // Name: scan until '<' (Git `fsck_ident`).
    loop {
        if p >= ident_end || data[p] == b'\n' {
            return Err(FsckError::new(
                "missingEmail",
                format!("invalid {oid_line} line - missing email"),
            ));
        }
        if data[p] == b'>' {
            return Err(FsckError::new(
                "badName",
                format!("invalid {oid_line} line - bad name"),
            ));
        }
        if data[p] == b'<' {
            break;
        }
        p += 1;
    }

    if p == start || data[p - 1] != b' ' {
        return Err(FsckError::new(
            "missingSpaceBeforeEmail",
            format!("invalid {oid_line} line - missing space before email"),
        ));
    }
    p += 1; // skip '<'

    // Email (may be empty between `<>`).
    loop {
        if p >= ident_end || data[p] == b'<' || data[p] == b'\n' {
            return Err(FsckError::new(
                "badEmail",
                format!("invalid {oid_line} line - bad email"),
            ));
        }
        if data[p] == b'>' {
            break;
        }
        p += 1;
    }
    p += 1; // skip '>'

    if p >= ident_end || data[p] != b' ' {
        return Err(FsckError::new(
            "missingSpaceBeforeDate",
            format!("invalid {oid_line} line - missing space before date"),
        ));
    }
    p += 1;

    while p < ident_end && (data[p] == b' ' || data[p] == b'\t') {
        p += 1;
    }

    if p >= ident_end || !data[p].is_ascii_digit() {
        return Err(FsckError::new(
            "badDate",
            format!("invalid {oid_line} line - bad date"),
        ));
    }

    if data[p] == b'0' && p + 1 < ident_end && data[p + 1] != b' ' {
        return Err(FsckError::new(
            "zeroPaddedDate",
            format!("invalid {oid_line} line - zero-padded date"),
        ));
    }

    let ts_start = p;
    while p < ident_end && data[p].is_ascii_digit() {
        p += 1;
    }
    let ts_len = p - ts_start;
    if ts_len > 21 {
        return Err(FsckError::new(
            "badDateOverflow",
            format!("invalid {oid_line} line - date causes integer overflow"),
        ));
    }
    let ts_str = std::str::from_utf8(&data[ts_start..p])
        .map_err(|_| FsckError::new("badDate", format!("invalid {oid_line} line - bad date")))?;
    let raw: u128 = ts_str
        .parse()
        .map_err(|_| FsckError::new("badDate", format!("invalid {oid_line} line - bad date")))?;
    if raw > u64::MAX as u128 || date_overflows(raw as u64) {
        return Err(FsckError::new(
            "badDateOverflow",
            format!("invalid {oid_line} line - date causes integer overflow"),
        ));
    }

    if p >= ident_end || data[p] != b' ' {
        return Err(FsckError::new(
            "badDate",
            format!("invalid {oid_line} line - bad date"),
        ));
    }
    p += 1;

    // Timezone: `[+-]HHMM` then newline (Git allows e.g. `-1430`).
    if p + 5 > ident_end
        || (data[p] != b'+' && data[p] != b'-')
        || !data[p + 1..p + 5].iter().all(|b| b.is_ascii_digit())
        || data[p + 5] != b'\n'
    {
        return Err(FsckError::new(
            "badTimezone",
            format!("invalid {oid_line} line - bad time zone"),
        ));
    }

    Ok(line_end + 1)
}

fn fsck_commit(data: &[u8]) -> Result<(), FsckError> {
    verify_headers(data, "nulInHeader")?;

    let buffer_end = data.len();
    let mut i = 0usize;

    if i >= buffer_end || !data[i..].starts_with(b"tree ") {
        return Err(FsckError::new(
            "missingTree",
            "invalid format - expected 'tree' line",
        ));
    }
    i += 5;
    let n = parse_oid_line(&data[i..], "badTreeSha1")?;
    i += n;

    while i < buffer_end && data[i..].starts_with(b"parent ") {
        i += 7;
        let n = parse_oid_line(&data[i..], "badParentSha1")?;
        i += n;
    }

    let mut author_count = 0usize;
    while i < buffer_end && data[i..].starts_with(b"author ") {
        author_count += 1;
        i += 7;
        i = fsck_ident(data, i, buffer_end, "author/committer")?;
    }

    if author_count < 1 {
        return Err(FsckError::new(
            "missingAuthor",
            "invalid format - expected 'author' line",
        ));
    }
    if author_count > 1 {
        return Err(FsckError::new(
            "multipleAuthors",
            "invalid format - multiple 'author' lines",
        ));
    }

    if i >= buffer_end || !data[i..].starts_with(b"committer ") {
        return Err(FsckError::new(
            "missingCommitter",
            "invalid format - expected 'committer' line",
        ));
    }
    i += 10;
    fsck_ident(data, i, buffer_end, "author/committer")?;

    if data.contains(&0) {
        return Err(FsckError::new(
            "nulInCommit",
            "NUL byte in the commit object body",
        ));
    }

    Ok(())
}

/// Byte offset immediately after the newline that terminates the `tagger` line.
fn parse_tag_headers_through_tagger(data: &[u8]) -> Result<usize, FsckError> {
    verify_headers(data, "nulInHeader")?;

    let buffer_end = data.len();
    let mut i = 0usize;

    if i >= buffer_end || !data[i..].starts_with(b"object ") {
        return Err(FsckError::new(
            "missingObject",
            "invalid format - expected 'object' line",
        ));
    }
    i += 7;
    let n = parse_oid_line(&data[i..], "badObjectSha1")?;
    i += n;

    if i >= buffer_end || !data[i..].starts_with(b"type ") {
        return Err(FsckError::new(
            "missingTypeEntry",
            "invalid format - expected 'type' line",
        ));
    }
    i += 5;
    let type_start = i;
    let eol = data[type_start..buffer_end]
        .iter()
        .position(|&b| b == b'\n')
        .map(|rel| type_start + rel)
        .ok_or_else(|| {
            FsckError::new(
                "missingType",
                "invalid format - unexpected end after 'type' line",
            )
        })?;

    if ObjectKind::from_tag_type_field(&data[type_start..eol]).is_none() {
        return Err(FsckError::new("badType", "invalid 'type' value"));
    }
    i = eol + 1;

    if i >= buffer_end || !data[i..].starts_with(b"tag ") {
        return Err(FsckError::new(
            "missingTagEntry",
            "invalid format - expected 'tag' line",
        ));
    }
    i += 4;
    let tag_start = i;
    let eol = data[tag_start..buffer_end]
        .iter()
        .position(|&b| b == b'\n')
        .map(|rel| tag_start + rel)
        .ok_or_else(|| {
            FsckError::new(
                "missingTag",
                "invalid format - unexpected end after 'type' line",
            )
        })?;

    let tag_name = std::str::from_utf8(&data[tag_start..eol])
        .map_err(|_| FsckError::new("badTagName", "invalid 'tag' name"))?;
    let refname = format!("refs/tags/{tag_name}");
    if check_refname_format(&refname, &RefNameOptions::default()).is_err() {
        return Err(FsckError::new(
            "badTagName",
            format!("invalid 'tag' name: {tag_name}"),
        ));
    }
    i = eol + 1;

    if i >= buffer_end || !data[i..].starts_with(b"tagger ") {
        return Err(FsckError::new(
            "missingTaggerEntry",
            "invalid format - expected 'tagger' line",
        ));
    }
    i += 7;
    fsck_ident(data, i, buffer_end, "author/committer")
}

fn fsck_tag(data: &[u8]) -> Result<(), FsckError> {
    parse_tag_headers_through_tagger(data).map(|_| ())
}

fn fsck_tree(data: &[u8]) -> Result<(), FsckError> {
    let mut pos = 0usize;
    let mut names: Vec<Vec<u8>> = Vec::new();
    while pos < data.len() {
        let sp = data[pos..]
            .iter()
            .position(|&b| b == b' ')
            .ok_or_else(|| FsckError::new("badTree", "cannot be parsed as a tree"))?;
        let mode_bytes = &data[pos..pos + sp];
        let mode = std::str::from_utf8(mode_bytes)
            .ok()
            .and_then(|s| u32::from_str_radix(s, 8).ok());
        if !matches!(
            mode,
            Some(0o100644 | 0o100755 | 0o120000 | 0o040000 | 0o160000)
        ) {
            return Err(FsckError::new(
                "badFilemode",
                "malformed mode in tree entry",
            ));
        }
        pos += sp + 1;

        let nul = data[pos..]
            .iter()
            .position(|&b| b == 0)
            .ok_or_else(|| FsckError::new("badTree", "cannot be parsed as a tree"))?;
        if nul == 0 {
            return Err(FsckError::new("emptyName", "empty filename in tree entry"));
        }
        names.push(data[pos..pos + nul].to_vec());
        pos += nul + 1;

        if pos + 20 > data.len() {
            return Err(FsckError::new("badTree", "cannot be parsed as a tree"));
        }
        if ObjectId::from_bytes(&data[pos..pos + 20]).is_err() {
            return Err(FsckError::new("badTree", "cannot be parsed as a tree"));
        }
        pos += 20;
    }
    let mut sorted = names;
    sorted.sort();
    if sorted.windows(2).any(|w| w[0] == w[1]) {
        return Err(FsckError::new(
            "duplicateEntries",
            "duplicateEntries: contains duplicate file entries",
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_commit_is_unterminated_header() {
        let e = fsck_object(ObjectKind::Commit, b"").unwrap_err();
        assert_eq!(e.id, "unterminatedHeader");
    }

    #[test]
    fn commit_missing_tree_matches_git() {
        let e = fsck_object(ObjectKind::Commit, b"\n\n").unwrap_err();
        assert_eq!(e.id, "missingTree");
    }

    #[test]
    fn tree_truncated_is_bad_tree() {
        let e = fsck_object(ObjectKind::Tree, b"100644 foo\0\x01\x01\x01\x01").unwrap_err();
        assert_eq!(e.id, "badTree");
    }
}

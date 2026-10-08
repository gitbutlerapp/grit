//! Standalone object fsck for loose-object validation before hashing.
//!
//! Checks mirror Git's buffer fsck rules; [`FsckError::id`] values are the
//! documented camelCase fsck config keys. Detail strings are Grit-owned prose.

use crate::check_ref_format::{check_refname_format, RefNameOptions};
use crate::dotfile::{is_hfs_dotgit, is_ntfs_dotgit};
use crate::git_date::tm::date_overflows;
use crate::objects::{HashAlgo, ObjectId, ObjectKind};

/// Repository hash width for object-id fields in commit, tag, and tree headers.
#[derive(Debug, Clone, Copy)]
pub struct FsckObjectOptions {
    /// Expected hex width for `tree`, `parent`, `object`, and tree entry oids.
    pub hash_algo: HashAlgo,
}

impl FsckObjectOptions {
    /// Build options for a repository object format.
    #[must_use]
    pub const fn new(hash_algo: HashAlgo) -> Self {
        Self { hash_algo }
    }
}

impl Default for FsckObjectOptions {
    fn default() -> Self {
        Self::new(HashAlgo::Sha1)
    }
}

/// Git-compatible fsck failure for loose object validation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FsckError {
    /// CamelCase message id (e.g. `missingTree`).
    pub id: &'static str,
    /// Human-readable detail after `id: ` (Grit-owned wording).
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

    /// Stable `msg-id: detail` line for logging and CLI output.
    #[must_use]
    pub fn report_line(&self) -> String {
        format!("{}: {}", self.id, self.detail)
    }
}

/// Validate raw object bytes before hashing (same rules as `git hash-object` fsck).
///
/// `options.hash_algo` selects the required object-id hex width on header and tree lines.
pub fn fsck_object(
    kind: ObjectKind,
    data: &[u8],
    options: FsckObjectOptions,
) -> Result<(), FsckError> {
    match kind {
        ObjectKind::Blob => Ok(()),
        ObjectKind::Commit => fsck_commit(data, options),
        ObjectKind::Tag => fsck_tag(data, options),
        ObjectKind::Tree => fsck_tree(data, options),
    }
}

fn verify_headers(data: &[u8], nul_msg_id: &'static str) -> Result<(), FsckError> {
    for (i, &b) in data.iter().enumerate() {
        if b == 0 {
            return Err(FsckError::new(
                nul_msg_id,
                format!("header contains a NUL at byte {i}"),
            ));
        }
        if b == b'\n' && i + 1 < data.len() && data[i + 1] == b'\n' {
            return Ok(());
        }
    }
    if !data.is_empty() && data[data.len() - 1] == b'\n' {
        Ok(())
    } else {
        Err(FsckError::new(
            "unterminatedHeader",
            "header is not terminated by a blank line",
        ))
    }
}

fn is_hex_lower(b: u8) -> bool {
    matches!(b, b'0'..=b'9' | b'a'..=b'f')
}

/// Parse a lowercase hex object id at the start of `buf`, requiring `expected_hex_len`
/// hex digits followed by `\n`. Returns bytes consumed (hex width + 1).
fn parse_oid_line(
    buf: &[u8],
    bad_id: &'static str,
    expected_hex_len: usize,
) -> Result<usize, FsckError> {
    let bad = || {
        FsckError::new(
            bad_id,
            format!(
                "{} line object id is not valid for this repository format",
                oid_line_label(bad_id)
            ),
        )
    };
    let hex_len = buf.iter().position(|&b| b == b'\n').ok_or_else(bad)?;
    if hex_len != expected_hex_len {
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

fn oid_line_label(bad_id: &'static str) -> &'static str {
    match bad_id {
        "badObjectSha1" => "object",
        "badParentSha1" => "parent",
        _ => "tree",
    }
}

fn fsck_ident(data: &[u8], start: usize, buffer_end: usize) -> Result<usize, FsckError> {
    let mut p = start;
    if p >= buffer_end {
        return Err(FsckError::new(
            "missingEmail",
            "identity line has no email address",
        ));
    }

    let line_end = data[p..buffer_end]
        .iter()
        .position(|&b| b == b'\n')
        .map(|rel| p + rel)
        .ok_or_else(|| FsckError::new("missingEmail", "identity line has no email address"))?;

    let ident_end = line_end;

    if data[p] == b'<' {
        return Err(FsckError::new(
            "missingNameBeforeEmail",
            "identity line lists email before a display name",
        ));
    }

    loop {
        if p >= ident_end || data[p] == b'\n' {
            return Err(FsckError::new(
                "missingEmail",
                "identity line has no email address",
            ));
        }
        if data[p] == b'>' {
            return Err(FsckError::new(
                "badName",
                "identity display name is malformed",
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
            "identity line needs whitespace before the email address",
        ));
    }
    p += 1;

    loop {
        if p >= ident_end || data[p] == b'<' || data[p] == b'\n' {
            return Err(FsckError::new(
                "badEmail",
                "email address on identity line is malformed",
            ));
        }
        if data[p] == b'>' {
            break;
        }
        p += 1;
    }
    p += 1;

    if p >= ident_end || data[p] != b' ' {
        return Err(FsckError::new(
            "missingSpaceBeforeDate",
            "identity line needs whitespace before the timestamp",
        ));
    }
    p += 1;

    while p < ident_end && (data[p] == b' ' || data[p] == b'\t') {
        p += 1;
    }

    if p >= ident_end || !data[p].is_ascii_digit() {
        return Err(FsckError::new(
            "badDate",
            "timestamp on identity line is not a decimal integer",
        ));
    }

    if data[p] == b'0' && p + 1 < ident_end && data[p + 1] != b' ' {
        return Err(FsckError::new(
            "zeroPaddedDate",
            "timestamp on identity line has leading zeros",
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
            "timestamp on identity line overflows",
        ));
    }
    let ts_str = std::str::from_utf8(&data[ts_start..p]).map_err(|_| {
        FsckError::new(
            "badDate",
            "timestamp on identity line is not a decimal integer",
        )
    })?;
    let raw: u128 = ts_str.parse().map_err(|_| {
        FsckError::new(
            "badDate",
            "timestamp on identity line is not a decimal integer",
        )
    })?;
    if raw > u64::MAX as u128 || date_overflows(raw as u64) {
        return Err(FsckError::new(
            "badDateOverflow",
            "timestamp on identity line overflows",
        ));
    }

    if p >= ident_end || data[p] != b' ' {
        return Err(FsckError::new(
            "badDate",
            "timestamp on identity line is not a decimal integer",
        ));
    }
    p += 1;

    if p + 5 > ident_end
        || (data[p] != b'+' && data[p] != b'-')
        || !data[p + 1..p + 5].iter().all(|b| b.is_ascii_digit())
        || data[p + 5] != b'\n'
    {
        return Err(FsckError::new(
            "badTimezone",
            "timezone on identity line is not ±HHMM",
        ));
    }

    Ok(line_end + 1)
}

fn fsck_commit(data: &[u8], options: FsckObjectOptions) -> Result<(), FsckError> {
    verify_headers(data, "nulInHeader")?;

    let oid_hex_len = options.hash_algo.hex_len();
    let buffer_end = data.len();
    let mut i = 0usize;

    if i >= buffer_end || !data[i..].starts_with(b"tree ") {
        return Err(FsckError::new(
            "missingTree",
            "commit header must start with a tree line",
        ));
    }
    i += 5;
    let n = parse_oid_line(&data[i..], "badTreeSha1", oid_hex_len)?;
    i += n;

    while i < buffer_end && data[i..].starts_with(b"parent ") {
        i += 7;
        let n = parse_oid_line(&data[i..], "badParentSha1", oid_hex_len)?;
        i += n;
    }

    let mut author_count = 0usize;
    while i < buffer_end && data[i..].starts_with(b"author ") {
        author_count += 1;
        i += 7;
        i = fsck_ident(data, i, buffer_end)?;
    }

    if author_count < 1 {
        return Err(FsckError::new(
            "missingAuthor",
            "commit header must include an author line",
        ));
    }
    if author_count > 1 {
        return Err(FsckError::new(
            "multipleAuthors",
            "commit header has more than one author line",
        ));
    }

    if i >= buffer_end || !data[i..].starts_with(b"committer ") {
        return Err(FsckError::new(
            "missingCommitter",
            "commit header must include a committer line",
        ));
    }
    i += 10;
    fsck_ident(data, i, buffer_end)?;

    if data.contains(&0) {
        return Err(FsckError::new(
            "nulInCommit",
            "commit body contains a NUL byte",
        ));
    }

    Ok(())
}

/// Byte offset immediately after the newline that terminates the `tagger` line.
fn parse_tag_headers_through_tagger(
    data: &[u8],
    options: FsckObjectOptions,
) -> Result<usize, FsckError> {
    verify_headers(data, "nulInHeader")?;

    let oid_hex_len = options.hash_algo.hex_len();
    let buffer_end = data.len();
    let mut i = 0usize;

    if i >= buffer_end || !data[i..].starts_with(b"object ") {
        return Err(FsckError::new(
            "missingObject",
            "annotated tag must start with an object line",
        ));
    }
    i += 7;
    let n = parse_oid_line(&data[i..], "badObjectSha1", oid_hex_len)?;
    i += n;

    if i >= buffer_end || !data[i..].starts_with(b"type ") {
        return Err(FsckError::new(
            "missingTypeEntry",
            "annotated tag header must include a type line",
        ));
    }
    i += 5;
    let type_start = i;
    let eol = data[type_start..buffer_end]
        .iter()
        .position(|&b| b == b'\n')
        .map(|rel| type_start + rel)
        .ok_or_else(|| FsckError::new("missingType", "type line on annotated tag is incomplete"))?;

    if ObjectKind::from_tag_type_field(&data[type_start..eol]).is_none() {
        return Err(FsckError::new(
            "badType",
            "annotated tag has unknown type name",
        ));
    }
    i = eol + 1;

    if i >= buffer_end || !data[i..].starts_with(b"tag ") {
        return Err(FsckError::new(
            "missingTagEntry",
            "annotated tag header must include a tag line",
        ));
    }
    i += 4;
    let tag_start = i;
    let eol = data[tag_start..buffer_end]
        .iter()
        .position(|&b| b == b'\n')
        .map(|rel| tag_start + rel)
        .ok_or_else(|| {
            FsckError::new("missingTag", "tag name line on annotated tag is incomplete")
        })?;

    let tag_name = std::str::from_utf8(&data[tag_start..eol])
        .map_err(|_| FsckError::new("badTagName", "annotated tag name is not a valid ref name"))?;
    let refname = format!("refs/tags/{tag_name}");
    if check_refname_format(&refname, &RefNameOptions::default()).is_err() {
        return Err(FsckError::new(
            "badTagName",
            format!("annotated tag name is not a valid ref name: {tag_name}"),
        ));
    }
    i = eol + 1;

    if i >= buffer_end || !data[i..].starts_with(b"tagger ") {
        return Err(FsckError::new(
            "missingTaggerEntry",
            "annotated tag header must include a tagger line",
        ));
    }
    i += 7;
    fsck_ident(data, i, buffer_end)
}

fn fsck_tag(data: &[u8], options: FsckObjectOptions) -> Result<(), FsckError> {
    let mut i = parse_tag_headers_through_tagger(data, options)?;
    i = skip_tag_gpgsig_headers(data, i)?;
    if i < data.len() && data[i] != b'\n' {
        return Err(FsckError::new(
            "extraHeaderEntry",
            "annotated tag header has unexpected lines after tagger",
        ));
    }
    Ok(())
}

fn skip_tag_gpgsig_headers(data: &[u8], mut i: usize) -> Result<usize, FsckError> {
    let buffer_end = data.len();
    if i < buffer_end
        && (data[i..].starts_with(b"gpgsig ") || data[i..].starts_with(b"gpgsig-sha256 "))
    {
        let sig_start = i;
        let sig_eol = data[sig_start..buffer_end]
            .iter()
            .position(|&b| b == b'\n')
            .map(|rel| sig_start + rel)
            .ok_or_else(|| FsckError::new("badGpgsig", "gpgsig header line is incomplete"))?;
        i = sig_eol + 1;
        while i < buffer_end && data[i] == b' ' {
            let cont_eol = data[i..buffer_end]
                .iter()
                .position(|&b| b == b'\n')
                .map(|rel| i + rel)
                .ok_or_else(|| {
                    FsckError::new(
                        "badHeaderContinuation",
                        "gpgsig continuation line is incomplete",
                    )
                })?;
            i = cont_eol + 1;
        }
    }
    Ok(i)
}

const MAX_TREE_ENTRY_LEN: usize = 4096;

fn is_tree_mode(mode: u32) -> bool {
    mode == 0o040000
}

fn is_less_than_slash(c: u8) -> bool {
    c > 0 && c < b'/'
}

#[derive(Copy, Clone, Eq, PartialEq)]
enum TreeOrderIssue {
    DuplicateEntries,
    NotSorted,
}

fn verify_tree_order(
    mode1: u32,
    name1: &[u8],
    mode2: u32,
    name2: &[u8],
    candidates: &mut Vec<Vec<u8>>,
) -> Result<(), TreeOrderIssue> {
    let len = name1.len().min(name2.len());
    let cmp = name1[..len].cmp(&name2[..len]);
    if cmp == std::cmp::Ordering::Less {
        return Ok(());
    }
    if cmp == std::cmp::Ordering::Greater {
        return Err(TreeOrderIssue::NotSorted);
    }

    let c1 = name1
        .get(len)
        .copied()
        .unwrap_or(if is_tree_mode(mode1) { b'/' } else { 0 });
    let c2 = name2
        .get(len)
        .copied()
        .unwrap_or(if is_tree_mode(mode2) { b'/' } else { 0 });

    if c1 == 0 && c2 == 0 {
        return Err(TreeOrderIssue::DuplicateEntries);
    }

    if c1 == 0 && is_less_than_slash(c2) {
        candidates.push(name1.to_vec());
    } else if c2 == b'/' && is_less_than_slash(c1) {
        loop {
            let Some(f_name) = candidates.pop() else {
                break;
            };
            if !name2.starts_with(&f_name) {
                continue;
            }
            let p = f_name.len();
            if name2.len() == p {
                return Err(TreeOrderIssue::DuplicateEntries);
            }
            if is_less_than_slash(name2[p]) {
                candidates.push(f_name);
                break;
            }
        }
    }

    if c1 < c2 {
        Ok(())
    } else {
        Err(TreeOrderIssue::NotSorted)
    }
}

fn tree_entry_name_is_dotgit(name: &[u8]) -> bool {
    if name == b".git" || name.eq_ignore_ascii_case(b"git~1") {
        return true;
    }
    let Ok(s) = std::str::from_utf8(name) else {
        return false;
    };
    is_hfs_dotgit(s) || is_ntfs_dotgit(s)
}

fn scan_ntfs_backslash_dotgit(name: &str) -> bool {
    let mut slash_rest = name;
    while let Some(idx) = slash_rest.find('\\') {
        let after = &slash_rest[idx + 1..];
        if is_ntfs_dotgit(after) {
            return true;
        }
        slash_rest = after;
    }
    false
}

fn is_null_oid_bytes(raw: &[u8]) -> bool {
    raw.iter().all(|&b| b == 0)
}

fn mode_allowed(mode: u32) -> bool {
    matches!(mode, 0o100644 | 0o100755 | 0o120000 | 0o040000 | 0o160000)
}

fn tree_issue_after_scan(
    has_null_sha1: bool,
    has_full_path: bool,
    has_dot: bool,
    has_dotdot: bool,
    has_dotgit: bool,
    has_zero_pad: bool,
    has_bad_modes: bool,
    has_dup_entries: bool,
    not_properly_sorted: bool,
    has_large_name: bool,
) -> Option<FsckError> {
    if has_null_sha1 {
        return Some(FsckError::new("nullSha1", "tree lists the null object id"));
    }
    if has_full_path {
        return Some(FsckError::new(
            "fullPathname",
            "tree entry name contains a slash",
        ));
    }
    if has_dot {
        return Some(FsckError::new("hasDot", "tree entry name is a single dot"));
    }
    if has_dotdot {
        return Some(FsckError::new(
            "hasDotdot",
            "tree entry name is parent directory",
        ));
    }
    if has_dotgit {
        return Some(FsckError::new(
            "hasDotgit",
            "tree entry name is a .git alias",
        ));
    }
    if has_zero_pad {
        return Some(FsckError::new(
            "zeroPaddedFilemode",
            "tree entry mode has leading zeros",
        ));
    }
    if has_bad_modes {
        return Some(FsckError::new(
            "badFilemode",
            "tree entry mode is not allowed",
        ));
    }
    if has_dup_entries {
        return Some(FsckError::new(
            "duplicateEntries",
            "tree lists the same name more than once",
        ));
    }
    if not_properly_sorted {
        return Some(FsckError::new(
            "treeNotSorted",
            "tree entries are out of canonical order",
        ));
    }
    if has_large_name {
        return Some(FsckError::new(
            "largePathname",
            "tree entry name exceeds the length limit",
        ));
    }
    None
}

fn fsck_tree(data: &[u8], options: FsckObjectOptions) -> Result<(), FsckError> {
    let oid_len = options.hash_algo.len();
    if !tree_walk_consumes_all(data, oid_len) {
        return Err(FsckError::new(
            "badTree",
            "tree object bytes are not a valid tree encoding",
        ));
    }

    let mut pos = 0usize;
    let mut has_null_sha1 = false;
    let mut has_full_path = false;
    let mut has_dot = false;
    let mut has_dotdot = false;
    let mut has_dotgit = false;
    let mut has_zero_pad = false;
    let mut has_bad_modes = false;
    let mut has_dup_entries = false;
    let mut not_properly_sorted = false;
    let mut has_large_name = false;
    let mut dup_candidates: Vec<Vec<u8>> = Vec::new();
    let mut prev: Option<(u32, Vec<u8>)> = None;

    while pos < data.len() {
        if data[pos] == b'0' {
            has_zero_pad = true;
        }

        let sp = data[pos..].iter().position(|&b| b == b' ').ok_or_else(|| {
            FsckError::new("badTree", "tree object bytes are not a valid tree encoding")
        })?;
        let mode_bytes = &data[pos..pos + sp];
        let mode = std::str::from_utf8(mode_bytes)
            .ok()
            .and_then(|s| u32::from_str_radix(s, 8).ok());
        let Some(mode) = mode else {
            return Err(FsckError::new(
                "badTree",
                "tree object bytes are not a valid tree encoding",
            ));
        };
        if !mode_allowed(mode) {
            has_bad_modes = true;
        }
        pos += sp + 1;

        let nul = data[pos..].iter().position(|&b| b == 0).ok_or_else(|| {
            FsckError::new("badTree", "tree object bytes are not a valid tree encoding")
        })?;
        if nul == 0 {
            return Err(FsckError::new(
                "badTree",
                "tree object bytes are not a valid tree encoding",
            ));
        }
        let name = &data[pos..pos + nul];
        if name == b"." {
            has_dot = true;
        }
        if name == b".." {
            has_dotdot = true;
        }
        if name.contains(&b'/') {
            has_full_path = true;
        }
        if name.len() > MAX_TREE_ENTRY_LEN {
            has_large_name = true;
        }
        if tree_entry_name_is_dotgit(name) {
            has_dotgit = true;
        }
        if let Ok(name_str) = std::str::from_utf8(name) {
            if is_hfs_dotgit(name_str) || is_ntfs_dotgit(name_str) {
                has_dotgit = true;
            }
            if scan_ntfs_backslash_dotgit(name_str) {
                has_dotgit = true;
            }
        }
        pos += nul + 1;

        if pos + oid_len > data.len() {
            return Err(FsckError::new(
                "badTree",
                "tree object bytes are not a valid tree encoding",
            ));
        }
        let oid_bytes = &data[pos..pos + oid_len];
        if ObjectId::from_bytes(oid_bytes).is_err() {
            return Err(FsckError::new(
                "badTree",
                "tree object bytes are not a valid tree encoding",
            ));
        }
        if is_null_oid_bytes(oid_bytes) {
            has_null_sha1 = true;
        }
        pos += oid_len;

        if let Some((prev_mode, ref prev_name)) = prev {
            match verify_tree_order(prev_mode, prev_name, mode, name, &mut dup_candidates) {
                Ok(()) => {}
                Err(TreeOrderIssue::DuplicateEntries) => has_dup_entries = true,
                Err(TreeOrderIssue::NotSorted) => not_properly_sorted = true,
            }
        }
        prev = Some((mode, name.to_vec()));
    }

    if let Some(err) = tree_issue_after_scan(
        has_null_sha1,
        has_full_path,
        has_dot,
        has_dotdot,
        has_dotgit,
        has_zero_pad,
        has_bad_modes,
        has_dup_entries,
        not_properly_sorted,
        has_large_name,
    ) {
        return Err(err);
    }
    Ok(())
}

fn tree_walk_consumes_all(data: &[u8], oid_len: usize) -> bool {
    let mut pos = 0usize;
    while pos < data.len() {
        let Some(sp) = data[pos..].iter().position(|&b| b == b' ') else {
            return false;
        };
        pos += sp + 1;
        let Some(nul) = data[pos..].iter().position(|&b| b == 0) else {
            return false;
        };
        pos += nul + 1;
        if pos + oid_len > data.len() {
            return false;
        }
        pos += oid_len;
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    const SHA1_OPTS: FsckObjectOptions = FsckObjectOptions::new(HashAlgo::Sha1);
    const SHA256_OPTS: FsckObjectOptions = FsckObjectOptions::new(HashAlgo::Sha256);

    #[test]
    fn empty_commit_is_unterminated_header() {
        let e = fsck_object(ObjectKind::Commit, b"", SHA1_OPTS).unwrap_err();
        assert_eq!(e.id, "unterminatedHeader");
    }

    #[test]
    fn commit_missing_tree_matches_git() {
        let e = fsck_object(ObjectKind::Commit, b"\n\n", SHA1_OPTS).unwrap_err();
        assert_eq!(e.id, "missingTree");
    }

    #[test]
    fn tree_truncated_is_bad_tree() {
        let e =
            fsck_object(ObjectKind::Tree, b"100644 foo\0\x01\x01\x01\x01", SHA1_OPTS).unwrap_err();
        assert_eq!(e.id, "badTree");
    }

    #[test]
    fn tree_null_sha1_reports_null_sha1() {
        let null = [0u8; 20];
        let mut body = b"100644 file\0".to_vec();
        body.extend_from_slice(&null);
        let e = fsck_object(ObjectKind::Tree, &body, SHA1_OPTS).unwrap_err();
        assert_eq!(e.id, "nullSha1");
    }

    #[test]
    fn tag_gpgsig_continuation_is_accepted() {
        let tree = "4b825dc642cb6eb9a060e54bf8d69288fbee4904";
        let tag = format!(
            "object {tree}\ntype commit\ntag signed\ntagger T <t@e.com> 1 +0000\ngpgsig sig\n line\n\nbody\n"
        );
        assert!(fsck_object(ObjectKind::Tag, tag.as_bytes(), SHA1_OPTS).is_ok());
    }

    #[test]
    fn tag_gpgsig_truncated_before_header_blank_line() {
        let tree = "4b825dc642cb6eb9a060e54bf8d69288fbee4904";
        let tag = format!(
            "object {tree}\ntype commit\ntag signed\ntagger T <t@e.com> 1 +0000\ngpgsig sig\n cont"
        );
        let e = fsck_object(ObjectKind::Tag, tag.as_bytes(), SHA1_OPTS).unwrap_err();
        assert_eq!(e.id, "unterminatedHeader");
    }

    #[test]
    fn commit_bad_name_in_ident() {
        let tree = "4b825dc642cb6eb9a060e54bf8d69288fbee4904";
        let body =
            format!("tree {tree}\nauthor A > <a@e.com> 1 +0000\ncommitter C <c@e.com> 1 +0000\n\n");
        let e = fsck_object(ObjectKind::Commit, body.as_bytes(), SHA1_OPTS).unwrap_err();
        assert_eq!(e.id, "badName");
    }

    #[test]
    fn fsck_error_report_line_format() {
        let e = FsckError::new("badTree", "tree object bytes are not a valid tree encoding");
        assert_eq!(
            e.report_line(),
            "badTree: tree object bytes are not a valid tree encoding"
        );
    }

    #[test]
    fn blob_fsck_is_noop() {
        assert!(fsck_object(ObjectKind::Blob, b"anything", SHA1_OPTS).is_ok());
    }

    #[test]
    fn commit_with_parent_is_accepted() {
        let tree = "4b825dc642cb6eb9a060e54bf8d69288fbee4904";
        let parent = "1a2b3c4d5e6f7890abcdef1234567890abcdef12";
        let body = format!(
            "tree {tree}\nparent {parent}\nauthor A <a@e.com> 1 +0000\ncommitter C <c@e.com> 1 +0000\n\n"
        );
        assert!(fsck_object(ObjectKind::Commit, body.as_bytes(), SHA1_OPTS).is_ok());
    }

    #[test]
    fn tree_well_formed_single_entry() {
        let tree = "4b825dc642cb6eb9a060e54bf8d69288fbee4904";
        let oid = hex::decode(tree).expect("hex");
        let mut body = b"100644 file\0".to_vec();
        body.extend_from_slice(&oid);
        assert!(fsck_object(ObjectKind::Tree, &body, SHA1_OPTS).is_ok());
    }

    #[test]
    fn sha256_commit_rejects_sha1_width_tree_oid() {
        let sha1_tree = "4b825dc642cb6eb9a060e54bf8d69288fbee4904";
        let body = format!(
            "tree {sha1_tree}\nauthor A <a@e.com> 1 +0000\ncommitter C <c@e.com> 1 +0000\n\n"
        );
        let e = fsck_object(ObjectKind::Commit, body.as_bytes(), SHA256_OPTS).unwrap_err();
        assert_eq!(e.id, "badTreeSha1");
    }

    #[test]
    fn sha1_commit_rejects_sha256_width_tree_oid() {
        let sha256_tree = "6ef19b41225c5369f1c104d45d8d85efa9b057b53b14b4b9b939dd74decc5321";
        let body = format!(
            "tree {sha256_tree}\nauthor A <a@e.com> 1 +0000\ncommitter C <c@e.com> 1 +0000\n\n"
        );
        let e = fsck_object(ObjectKind::Commit, body.as_bytes(), SHA1_OPTS).unwrap_err();
        assert_eq!(e.id, "badTreeSha1");
    }

    #[test]
    fn sha256_tag_rejects_sha1_width_object_oid() {
        let sha1_tree = "4b825dc642cb6eb9a060e54bf8d69288fbee4904";
        let tag = format!("object {sha1_tree}\ntype commit\ntag t\ntagger T <t@e.com> 1 +0000\n\n");
        let e = fsck_object(ObjectKind::Tag, tag.as_bytes(), SHA256_OPTS).unwrap_err();
        assert_eq!(e.id, "badObjectSha1");
    }

    #[test]
    fn tree_mode_100664_is_bad_filemode() {
        let tree = "4b825dc642cb6eb9a060e54bf8d69288fbee4904";
        let oid = hex::decode(tree).expect("hex");
        let mut body = b"100664 file\0".to_vec();
        body.extend_from_slice(&oid);
        let e = fsck_object(ObjectKind::Tree, &body, SHA1_OPTS).unwrap_err();
        assert_eq!(e.id, "badFilemode");
    }

    #[test]
    fn tree_multibyte_utf8_name_does_not_panic() {
        let tree = "4b825dc642cb6eb9a060e54bf8d69288fbee4904";
        let oid = hex::decode(tree).expect("hex");
        let mut body = b"100644 ".to_vec();
        body.extend_from_slice(".éé".as_bytes());
        body.push(0);
        body.extend_from_slice(&oid);
        assert!(fsck_object(ObjectKind::Tree, &body, SHA1_OPTS).is_ok());
    }
}

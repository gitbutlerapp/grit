//! Unified/`git`-diff patch parsing for `grit apply`.
//!
//! This is the self-contained *parse* core extracted from `grit apply`: it turns
//! patch text into structured [`FilePatch`]/[`Hunk`] data with no I/O, no
//! environment access, and no CLI dependencies. The worktree/index application
//! engine and all CLI output still live in the `grit` crate; only the
//! text-to-structured-data layer lives here so it can be unit-tested and reused
//! as a library.

use crate::error::{ApplyError, Error, Result};
use regex::Regex;
use std::sync::OnceLock;

/// A single hunk in a unified diff.
#[derive(Debug, Clone)]
pub struct Hunk {
    /// 1-based line number in the old file.
    pub old_start: usize,
    /// Number of lines in the old side.
    pub old_count: usize,
    /// 1-based line number in the new file.
    pub new_start: usize,
    /// Number of lines on the new side.
    pub new_count: usize,
    /// 1-based line number in the patch file of the first hunk body line (line after `@@`).
    pub first_body_line: usize,
    /// Lines of the hunk body (' ', '+', '-' prefixed, or bare '\' no newline).
    pub lines: Vec<HunkLine>,
}

#[derive(Debug, Clone)]
pub enum HunkLine {
    Context(String),
    Add(String),
    Remove(String),
    /// "\ No newline at end of file"
    NoNewline,
}

/// Represents one file in a unified diff.
#[derive(Debug, Clone)]
pub struct FilePatch {
    /// Path from `diff --git` old side (`a/...`) when present.
    pub diff_old_path: Option<String>,
    /// Path from `diff --git` new side (`b/...`) when present.
    pub diff_new_path: Option<String>,
    /// Path on the old side (None for new files).
    pub old_path: Option<String>,
    /// Path on the new side (None for deleted files).
    pub new_path: Option<String>,
    /// Whether an explicit `---` header line was present.
    pub saw_old_header: bool,
    /// Whether an explicit `+++` header line was present.
    pub saw_new_header: bool,
    /// Old mode from extended header.
    pub old_mode: Option<String>,
    /// New mode from extended header.
    pub new_mode: Option<String>,
    /// Source line (1-based) of `old mode` / `deleted file mode` for diagnostics.
    pub old_mode_line: Option<usize>,
    /// Source line (1-based) of `new mode` / `new file mode` for diagnostics.
    pub new_mode_line: Option<usize>,
    /// Whether this file is being newly created.
    pub is_new: bool,
    /// Whether this file is being deleted.
    pub is_deleted: bool,
    /// Whether this is a rename.
    pub is_rename: bool,
    /// Whether this is a copy.
    pub is_copy: bool,
    /// Similarity index (e.g., 90 for 90%).
    pub similarity_index: Option<u32>,
    /// Dissimilarity index for rewrites.
    pub dissimilarity_index: Option<u32>,
    /// Old blob OID from the index header (abbreviated).
    pub old_oid: Option<String>,
    /// New blob OID from the index header (abbreviated).
    pub new_oid: Option<String>,
    /// Parsed binary patch payload (`GIT binary patch`) if present.
    pub binary_patch: Option<BinaryPatchPayload>,
    /// Whether this is a binary change (`GIT binary patch` payload or a
    /// `Binary files ... differ` marker); stat/numstat show `Bin` / `-`.
    pub is_binary: bool,
    /// Hunks to apply.
    pub hunks: Vec<Hunk>,
    /// Merged `core.whitespace` + `whitespace` attribute (Git `ws_rule`); `0` before assignment.
    pub ws_rule: u32,
    /// Git `patch->is_toplevel_relative`: set for `diff --git` patches only. When false, paths are
    /// prefixed with the setup directory (work-tree-relative CWD) like `prefix_patch` in Git.
    pub is_toplevel_relative: bool,
}

/// Binary patch payload as compressed base85 chunks for forward/reverse apply.
#[derive(Debug, Clone)]
pub struct BinaryPatchPayload {
    pub forward_compressed: Vec<u8>,
    pub forward_declared_size: usize,
    pub reverse_compressed: Vec<u8>,
    pub reverse_declared_size: usize,
}

impl FilePatch {
    /// Effective path for the file.
    /// For deletions, use old_path (new is /dev/null).
    /// For additions, use new_path (old is /dev/null).
    /// Otherwise prefer new_path.
    pub fn effective_path(&self) -> Option<&str> {
        if self.is_deleted {
            return self
                .old_path
                .as_deref()
                .filter(|p| *p != "/dev/null")
                .or(self.new_path.as_deref().filter(|p| *p != "/dev/null"));
        }
        if self.is_new {
            return self
                .new_path
                .as_deref()
                .filter(|p| *p != "/dev/null")
                .or(self.old_path.as_deref().filter(|p| *p != "/dev/null"));
        }
        self.new_path
            .as_deref()
            .filter(|p| *p != "/dev/null")
            .or(self.old_path.as_deref().filter(|p| *p != "/dev/null"))
    }

    /// Source path to read preimage content from.
    ///
    /// For rename/copy patches this is the old path, otherwise this is the
    /// effective path.
    pub fn source_path(&self) -> Option<&str> {
        if self.is_rename || self.is_copy {
            self.old_path
                .as_deref()
                .filter(|p| *p != "/dev/null")
                .or(self.effective_path())
        } else if let (Some(old), Some(new)) = (self.old_path.as_deref(), self.new_path.as_deref())
        {
            if old != "/dev/null" && new != "/dev/null" && old != new {
                Some(old)
            } else {
                self.effective_path()
            }
        } else {
            self.effective_path()
        }
    }

    /// Destination path to write postimage content to.
    ///
    /// For additions/renames/copies this is the new path, otherwise this is
    /// the effective path.
    pub fn target_path(&self) -> Option<&str> {
        if self.is_new || self.is_rename || self.is_copy {
            self.new_path
                .as_deref()
                .filter(|p| *p != "/dev/null")
                .or(self.effective_path())
        } else {
            self.effective_path()
        }
    }

    /// True when this patch touches a gitlink/submodule (mode `160000`).
    pub fn involves_gitlink(&self) -> bool {
        self.old_mode.as_deref() == Some("160000") || self.new_mode.as_deref() == Some("160000")
    }

    /// Work-tree-relative path for filesystem IO and `.gitattributes` (Git `prefix_patch`).
    pub fn worktree_rel_operational(&self, adjusted: &str, setup_prefix: &str) -> String {
        if self.is_toplevel_relative {
            adjusted.to_string()
        } else {
            format!("{setup_prefix}{adjusted}")
        }
    }
}

/// Strip trailing `\r` and surrounding whitespace from parsed header tokens.
///
/// `git diff` may emit CRLF line endings; without this, `new mode 160000\r` fails to match
/// submodule handling (`t4137-apply-submodule`).
fn sanitize_patch_header_value(s: &mut String) {
    *s = s.trim().trim_end_matches('\r').to_string();
}

/// Strip Git's `diff --git a/... b/...` path prefix when it leaked into stored paths.
///
/// Binary patches often omit `---`/`+++` lines that would normally resynchronize names; without
/// this, paths like `a/bin.png` are misinterpreted as real file paths (`t4108-apply-threeway`).
fn strip_git_diff_path_prefix(path: &str) -> String {
    if path == "/dev/null" {
        return path.to_string();
    }
    let p = path.trim_start_matches("./");
    if let Some(rest) = p.strip_prefix("a/") {
        return rest.to_string();
    }
    if let Some(rest) = p.strip_prefix("b/") {
        return rest.to_string();
    }
    path.to_string()
}

fn sanitize_file_patch_headers(fp: &mut FilePatch) {
    if let Some(ref mut s) = fp.old_mode {
        sanitize_patch_header_value(s);
        if s.is_empty() {
            fp.old_mode = None;
        }
    }
    if let Some(ref mut s) = fp.new_mode {
        sanitize_patch_header_value(s);
        if s.is_empty() {
            fp.new_mode = None;
        }
    }
    if let Some(ref mut s) = fp.old_oid {
        sanitize_patch_header_value(s);
    }
    if let Some(ref mut s) = fp.new_oid {
        sanitize_patch_header_value(s);
    }
    for ref mut s in [
        &mut fp.diff_old_path,
        &mut fp.diff_new_path,
        &mut fp.old_path,
        &mut fp.new_path,
    ]
    .into_iter()
    .flatten()
    {
        sanitize_patch_header_value(s);
        **s = strip_git_diff_path_prefix(s);
    }
}

/// Collapse runs of `/` to a single slash (Git `squash_slash`).
fn squash_slash_path(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut prev_slash = false;
    for ch in s.chars() {
        if ch == '/' {
            if !prev_slash {
                out.push('/');
            }
            prev_slash = true;
        } else {
            prev_slash = false;
            out.push(ch);
        }
    }
    out
}

/// Unquote a leading C-style `"..."` from `line`; returns decoded bytes and remainder after closing `"`.
/// Matches Git `unquote_c_style` / `quote.c` escapes used in diff headers.
fn unquote_c_style_diff_prefix(line: &str) -> Option<(Vec<u8>, &str)> {
    let b = line.as_bytes();
    if b.first() != Some(&b'"') {
        return None;
    }
    let mut q = &b[1..];
    let mut out = Vec::new();
    loop {
        let len = q
            .iter()
            .position(|&c| c == b'"' || c == b'\\')
            .unwrap_or(q.len());
        out.extend_from_slice(&q[..len]);
        q = &q[len..];
        if q.is_empty() {
            return None;
        }
        match q[0] {
            b'"' => {
                let rest = std::str::from_utf8(&q[1..]).ok()?;
                return Some((out, rest));
            }
            b'\\' => {
                q = &q[1..];
                if q.is_empty() {
                    return None;
                }
                let ch = q[0];
                q = &q[1..];
                match ch {
                    b'a' => out.push(0x07),
                    b'b' => out.push(0x08),
                    b'f' => out.push(0x0c),
                    b'n' => out.push(b'\n'),
                    b'r' => out.push(b'\r'),
                    b't' => out.push(b'\t'),
                    b'v' => out.push(0x0b),
                    b'\\' => out.push(b'\\'),
                    b'"' => out.push(b'"'),
                    b'0'..=b'3' => {
                        if q.len() < 2 {
                            return None;
                        }
                        let ch2 = q[0];
                        let ch3 = q[1];
                        if !(b'0'..=b'7').contains(&ch2) || !(b'0'..=b'7').contains(&ch3) {
                            return None;
                        }
                        let ac = u32::from(ch - b'0') * 64
                            + u32::from(ch2 - b'0') * 8
                            + u32::from(ch3 - b'0');
                        out.push(ac as u8);
                        q = &q[2..];
                    }
                    _ => return None,
                }
            }
            _ => return None,
        }
    }
}

fn bytes_to_path_string(bytes: &[u8]) -> Result<String> {
    let s = String::from_utf8(bytes.to_vec())
        .map_err(|e| Error::Message(format!("diff path is not valid UTF-8: {e}")))?;
    Ok(squash_slash_path(&s))
}

/// Skip `p_value` leading path components (Git `skip_tree_prefix`); `p_value == 0` allows absolute paths.
fn skip_tree_prefix_bytes(line: &[u8], p_value: usize) -> Option<&[u8]> {
    if p_value == 0 {
        return Some(line);
    }
    let mut nslash = p_value;
    let mut i = 0usize;
    while i < line.len() {
        if line[i] == b'/' {
            nslash = nslash.saturating_sub(1);
            if nslash == 0 {
                return if i == 0 { None } else { Some(&line[i + 1..]) };
            }
        }
        i += 1;
    }
    None
}

/// Strip `p_value` leading `/`-separated components from a UTF-8 path (for `rename from` etc.).
fn skip_tree_prefix_str(path: &str, p_value: usize) -> Option<String> {
    let stripped = skip_tree_prefix_bytes(path.as_bytes(), p_value)?;
    Some(String::from_utf8_lossy(stripped).into_owned())
}

fn sane_tz_len(line: &[u8]) -> usize {
    const SUFFIX: &[u8] = b" +0500";
    if line.len() < SUFFIX.len() || line[line.len() - SUFFIX.len()] != b' ' {
        return 0;
    }
    let tz = &line[line.len() - SUFFIX.len()..];
    if tz[1] != b'+' && tz[1] != b'-' {
        return 0;
    }
    for p in &tz[2..] {
        if !p.is_ascii_digit() {
            return 0;
        }
    }
    SUFFIX.len()
}

fn tz_with_colon_len(line: &[u8]) -> usize {
    // Git: suffix is ` ±HH:MM` (space, sign, two hour digits, colon, two minute digits) = 7 bytes.
    const SUFFIX_LEN: usize = 7;
    if line.len() < SUFFIX_LEN || line[line.len() - 3] != b':' {
        return 0;
    }
    let tz = &line[line.len() - SUFFIX_LEN..];
    if tz[0] != b' ' || (tz[1] != b'+' && tz[1] != b'-') {
        return 0;
    }
    let p = &tz[2..];
    if p.len() != 5
        || !p[0].is_ascii_digit()
        || !p[1].is_ascii_digit()
        || p[2] != b':'
        || !p[3].is_ascii_digit()
        || !p[4].is_ascii_digit()
    {
        return 0;
    }
    SUFFIX_LEN
}

fn date_len(line: &[u8]) -> usize {
    const SHORT: &[u8] = b"72-02-05";
    if line.len() < SHORT.len() || line[line.len() - 3] != b'-' {
        return 0;
    }
    let mut p = line.len() - SHORT.len();
    let date = &line[p..];
    if !date[0].is_ascii_digit()
        || !date[1].is_ascii_digit()
        || date[2] != b'-'
        || !date[3].is_ascii_digit()
        || !date[4].is_ascii_digit()
        || date[5] != b'-'
        || !date[6].is_ascii_digit()
        || !date[7].is_ascii_digit()
    {
        return 0;
    }
    if p >= 2 {
        let y1 = line[p - 1];
        let y2 = line[p - 2];
        if y1.is_ascii_digit() && y2.is_ascii_digit() {
            p -= 2;
        }
    }
    line.len() - p
}

fn short_time_len(line: &[u8]) -> usize {
    const PAT: &[u8] = b" 07:01:32";
    if line.len() < PAT.len() || line[line.len() - 3] != b':' {
        return 0;
    }
    let p = line.len() - PAT.len();
    let time = &line[p..];
    if time[0] != b' '
        || !time[1].is_ascii_digit()
        || !time[2].is_ascii_digit()
        || time[3] != b':'
        || !time[4].is_ascii_digit()
        || !time[5].is_ascii_digit()
        || time[6] != b':'
        || !time[7].is_ascii_digit()
        || !time[8].is_ascii_digit()
    {
        return 0;
    }
    PAT.len()
}

fn fractional_time_len(line: &[u8]) -> usize {
    if line.is_empty() || !line[line.len() - 1].is_ascii_digit() {
        return 0;
    }
    let mut p = line.len() - 1;
    while p > 0 && line[p].is_ascii_digit() {
        p -= 1;
    }
    if p == 0 || line[p] != b'.' {
        return 0;
    }
    let n = short_time_len(&line[..p]);
    if n == 0 {
        return 0;
    }
    line.len() - p + n
}

fn trailing_spaces_len(line: &[u8]) -> usize {
    if line.is_empty() || line[line.len() - 1] != b' ' {
        return 0;
    }
    let mut p = line.len();
    while p > 0 {
        p -= 1;
        if line[p] != b' ' {
            return line.len() - (p + 1);
        }
    }
    line.len()
}

fn diff_timestamp_len(line: &[u8]) -> usize {
    if line.is_empty() || !line[line.len() - 1].is_ascii_digit() {
        return 0;
    }
    let mut end = line.len();
    let mut n = sane_tz_len(&line[..end]);
    if n == 0 {
        n = tz_with_colon_len(&line[..end]);
    }
    if n == 0 {
        return 0;
    }
    end -= n;

    n = short_time_len(&line[..end]);
    if n == 0 {
        n = fractional_time_len(&line[..end]);
    }
    if n == 0 {
        return 0;
    }
    end -= n;

    n = date_len(&line[..end]);
    if n == 0 {
        return 0;
    }
    end -= n;

    if end == 0 {
        return 0;
    }
    match line[end - 1] {
        b'\t' => {
            end -= 1;
            line.len() - end
        }
        b' ' => {
            end -= trailing_spaces_len(&line[..end]);
            line.len() - end
        }
        _ => 0,
    }
}

/// Git `find_name_common` with optional `end` bound (exclusive).
fn find_name_common_bounded(
    line: &[u8],
    def: Option<&[u8]>,
    p_value: usize,
    end: usize,
) -> Option<Vec<u8>> {
    let end = end.min(line.len());
    let mut start: Option<usize> = if p_value == 0 { Some(0) } else { None };
    let mut p = p_value;
    let mut i = 0usize;
    while i < end {
        let c = line[i];
        i += 1;
        if c == b'/' && p > 0 {
            p -= 1;
            if p == 0 {
                start = Some(i);
            }
        }
    }
    let start = start?;
    let len = i - start;
    if len == 0 {
        return def.map(|d| d.to_vec());
    }
    let slice = &line[start..i];
    if let Some(d) = def {
        if d.len() < len && slice.starts_with(d) {
            return Some(d.to_vec());
        }
    }
    Some(slice.to_vec())
}

/// Git `find_name_traditional` on the line after `--- ` / `+++ ` (no prefix).
fn find_name_traditional(line: &[u8], def: Option<&[u8]>, p_value: usize) -> Option<Vec<u8>> {
    if line.first() == Some(&b'"') {
        let (decoded, _) = unquote_c_style_diff_prefix(std::str::from_utf8(line).ok()?)?;
        let skip = skip_tree_prefix_bytes(&decoded, p_value)?;
        return Some(skip.to_vec());
    }
    let ts = diff_timestamp_len(line);
    let name_end = line.len().saturating_sub(ts);
    find_name_common_bounded(line, def, p_value, name_end)
}

fn find_name_tab_terminated(line: &[u8], p_value: usize) -> Option<Vec<u8>> {
    if line.first() == Some(&b'"') {
        let (decoded, _) = unquote_c_style_diff_prefix(std::str::from_utf8(line).ok()?)?;
        let skip = skip_tree_prefix_bytes(&decoded, p_value)?;
        return Some(skip.to_vec());
    }
    let end = line
        .iter()
        .position(|&b| b == b'\t' || b == b'\n' || b == b'\r')
        .unwrap_or(line.len());
    find_name_common_bounded(line, None, p_value, end)
}

fn is_dev_null_nameline(line: &[u8]) -> bool {
    line.strip_prefix(b"/dev/null")
        .map(|rest| rest.is_empty() || rest.first().is_some_and(|b| b.is_ascii_whitespace()))
        .unwrap_or(false)
}

fn count_slashes_in_prefix(prefix: &str) -> usize {
    prefix.bytes().filter(|&b| b == b'/').count()
}

/// Git `guess_p_value` for traditional patches (`apply.c`). Uses `setup_git_directory` prefix.
fn guess_p_value_from_nameline(line: &[u8], setup_prefix: Option<&str>) -> Option<usize> {
    if is_dev_null_nameline(line) {
        return None;
    }
    let name = find_name_traditional(line, None, 0)?;
    let name_str = String::from_utf8_lossy(&name);
    if !name_str.contains('/') {
        return Some(0);
    }
    let pfx = setup_prefix.filter(|p| !p.is_empty())?;
    if name_str.starts_with(pfx) {
        return Some(count_slashes_in_prefix(pfx));
    }
    let slash = name_str.find('/')?;
    let rest = name_str.get(slash + 1..)?;
    if rest.starts_with(pfx) {
        return Some(count_slashes_in_prefix(pfx) + 1);
    }
    None
}

fn epoch_stamp_regex() -> &'static Regex {
    // hygiene: immutable compiled regex for patch epoch stamps
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        // Provably infallible: the pattern is a fixed string literal that is a valid regex.
        #[allow(clippy::expect_used)]
        Regex::new(r"^([0-2][0-9]):([0-5][0-9]):00(?:\.0+)? ([-+][0-2][0-9]:?[0-5][0-9])")
            .expect("epoch stamp regex is a valid constant pattern")
    })
}

/// True when the `---`/`+++` line has a tab-separated epoch timestamp (Git `has_epoch_timestamp`).
fn has_epoch_timestamp(nameline: &[u8]) -> bool {
    let Some(tab) = nameline.iter().position(|&b| b == b'\t') else {
        return false;
    };
    let mut ts = &nameline[tab + 1..];
    let epoch_hour = if let Some(r) = ts.strip_prefix(b"1969-12-31 ") {
        ts = r;
        24i32
    } else if let Some(r) = ts.strip_prefix(b"1970-01-01 ") {
        ts = r;
        0i32
    } else {
        return false;
    };
    let end = ts.iter().position(|&b| b == b'\n').unwrap_or(ts.len());
    let stamp = &ts[..end];
    let stamp_str = match std::str::from_utf8(stamp) {
        Ok(s) => s,
        Err(_) => return false,
    };
    let caps = match epoch_stamp_regex().captures(stamp_str) {
        Some(c) => c,
        None => return false,
    };
    let hour: i32 = caps
        .get(1)
        .and_then(|m| m.as_str().parse().ok())
        .unwrap_or(-1);
    let minute: i32 = caps
        .get(2)
        .and_then(|m| m.as_str().parse().ok())
        .unwrap_or(-1);
    let tz_s = match caps.get(3).map(|m| m.as_str()) {
        Some(s) if !s.is_empty() => s,
        _ => return false,
    };
    if hour < 0 || minute < 0 {
        return false;
    }
    let tz_byte = tz_s.as_bytes()[0];
    let tz_rest = &tz_s[1..];
    let zoneoffset: i32 = if let Some(colon_pos) = tz_rest.find(':') {
        let h: i32 = tz_rest[..colon_pos].parse().unwrap_or(0);
        let mm: i32 = tz_rest[colon_pos + 1..].parse().unwrap_or(0);
        h * 60 + mm
    } else if tz_rest.len() >= 4 {
        let n: i32 = tz_rest[..4].parse().unwrap_or(0);
        (n / 100) * 60 + (n % 100)
    } else {
        return false;
    };
    let zoneoffset = if tz_byte == b'-' {
        -zoneoffset
    } else {
        zoneoffset
    };
    hour * 60 + minute - zoneoffset == epoch_hour * 60
}

/// Parse `---` / `+++` pair for a traditional unified diff (Git `parse_traditional_patch`).
fn parse_traditional_patch_pair(
    old_line: &[u8],
    new_line: &[u8],
    strip: usize,
    p_guess: &mut Option<usize>,
    setup_prefix: Option<&str>,
) -> Result<FilePatch> {
    let old_p = old_line.strip_prefix(b"--- ").unwrap_or(old_line);
    let new_p = new_line.strip_prefix(b"+++ ").unwrap_or(new_line);

    if p_guess.is_none() {
        let p = guess_p_value_from_nameline(old_p, setup_prefix);
        let q = guess_p_value_from_nameline(new_p, setup_prefix);
        let chosen = match (p, q) {
            (None, None) => None,
            (Some(a), None) => Some(a),
            (None, Some(b)) => Some(b),
            (Some(a), Some(b)) if a == b => Some(a),
            _ => None,
        };
        *p_guess = chosen;
    }
    let p_val = p_guess.unwrap_or(strip);

    let mut fp = FilePatch {
        diff_old_path: None,
        diff_new_path: None,
        old_path: None,
        new_path: None,
        saw_old_header: true,
        saw_new_header: true,
        old_mode: None,
        new_mode: None,
        old_mode_line: None,
        new_mode_line: None,
        is_new: false,
        is_deleted: false,
        is_rename: false,
        is_copy: false,
        similarity_index: None,
        dissimilarity_index: None,
        old_oid: None,
        new_oid: None,
        binary_patch: None,
        is_binary: false,
        hunks: Vec::new(),
        ws_rule: 0,
        is_toplevel_relative: false,
    };

    if is_dev_null_nameline(old_p) {
        fp.is_new = true;
        let name = find_name_traditional(new_p, None, p_val).ok_or_else(|| {
            Error::Message("unable to find filename in traditional patch".to_string())
        })?;
        fp.new_path = Some(bytes_to_path_string(&name)?);
    } else if is_dev_null_nameline(new_p) {
        fp.is_deleted = true;
        let name = find_name_traditional(old_p, None, p_val).ok_or_else(|| {
            Error::Message("unable to find filename in traditional patch".to_string())
        })?;
        fp.old_path = Some(bytes_to_path_string(&name)?);
    } else {
        let first_name = find_name_traditional(old_p, None, p_val).ok_or_else(|| {
            Error::Message("unable to find filename in traditional patch".to_string())
        })?;
        let name = find_name_traditional(new_p, Some(&first_name), p_val).ok_or_else(|| {
            Error::Message("unable to find filename in traditional patch".to_string())
        })?;
        let name_str = bytes_to_path_string(&name)?;
        if has_epoch_timestamp(old_p) {
            fp.is_new = true;
            fp.new_path = Some(name_str);
        } else if has_epoch_timestamp(new_p) {
            fp.is_deleted = true;
            fp.old_path = Some(name_str);
        } else {
            // Git uses the `+++` filename for both sides when neither line carries an epoch
            // marker; the `---` line only participates via `def` when shortening `.orig` etc.
            fp.old_path = Some(name_str.clone());
            fp.new_path = Some(name_str);
        }
    }

    Ok(fp)
}

/// Default filename from `diff --git` when both sides agree (Git `git_header_name`).
fn git_header_def_name(line: &str, p_value: usize) -> Option<String> {
    let rest = line.strip_prefix("diff --git ").unwrap_or(line);
    let rest_b = rest.as_bytes();

    if rest_b.first() == Some(&b'"') {
        let (first_decoded, second_raw) = unquote_c_style_diff_prefix(rest)?;
        let rel_first = skip_tree_prefix_bytes(&first_decoded, p_value)?;
        let second = second_raw.trim_start_matches(|c: char| c.is_ascii_whitespace());
        if second.is_empty() {
            return None;
        }
        if second.as_bytes().first() == Some(&b'"') {
            let (second_decoded, _) = unquote_c_style_diff_prefix(second)?;
            let rel2 = skip_tree_prefix_bytes(&second_decoded, p_value)?;
            if rel2 != rel_first {
                return None;
            }
        } else {
            let rel2 = skip_tree_prefix_bytes(second.as_bytes(), p_value)?;
            if rel2.len() != rel_first.len() || rel2 != rel_first {
                return None;
            }
        }
        return bytes_to_path_string(rel_first).ok();
    }

    let name = skip_tree_prefix_bytes(rest_b, p_value)?;
    let name_start = name.as_ptr() as usize - rest_b.as_ptr() as usize;

    for offset in 0..name.len() {
        if name[offset] != b'"' {
            continue;
        }
        let second_slice = &rest_b[name_start + offset..];
        let (decoded, _) = unquote_c_style_diff_prefix(std::str::from_utf8(second_slice).ok()?)?;
        let np = skip_tree_prefix_bytes(&decoded, p_value)?;
        let plen = np.len();
        if plen < offset
            && name.len() > plen
            && &name[..plen] == np
            && name[plen].is_ascii_whitespace()
        {
            return bytes_to_path_string(np).ok();
        }
        return None;
    }

    let line_len = rest.len().saturating_sub(name_start);
    let mut len = 0usize;
    while len < line_len {
        match rest_b[name_start + len] {
            b'\n' => return None,
            b'\t' | b' ' => {
                let after = name_start + len + 1;
                if after > name_start + line_len {
                    return None;
                }
                let second =
                    skip_tree_prefix_bytes(&rest_b[after..name_start + line_len], p_value)?;
                let names_match =
                    name.len() >= len && second.len() >= len && name[..len] == second[..len];
                let boundary_ok = second.get(len) == Some(&b'\n') || second.len() == len;
                if names_match && boundary_ok {
                    return bytes_to_path_string(&name[..len]).ok();
                }
            }
            _ => {}
        }
        len += 1;
    }
    None
}

/// Path from `rename from` / `copy from` lines (Git `find_name` with `terminate == 0`).
fn find_name_extended_header(rest: &str, p_extended: usize) -> Option<String> {
    let rest = rest.trim_end_matches(['\r', '\n']);
    let b = rest.as_bytes();
    if b.first() == Some(&b'"') {
        let (decoded, tail) = unquote_c_style_diff_prefix(rest)?;
        if !tail.trim().is_empty() {
            return None;
        }
        let skip = skip_tree_prefix_bytes(&decoded, p_extended)?;
        return bytes_to_path_string(skip).ok();
    }
    let end = b
        .iter()
        .position(|&c| c == b'\t' || c == b'\n' || c == b'\r' || c == b' ')
        .unwrap_or(b.len());
    let name = find_name_common_bounded(b, None, p_extended, end)?;
    bytes_to_path_string(&name).ok()
}

/// Parse a unified diff into a list of `FilePatch` entries.
///
/// `strip` is Git's `p_value` (`-p` count, default 1). `setup_prefix` is Git's
/// `setup_git_directory` prefix (work-tree-relative path from CWD to the repo
/// root, with trailing `/`); pass `None` when running from the work-tree root.
/// The caller computes it because it is a CLI/environment concern.
pub fn parse_patch(
    input: &str,
    strip: usize,
    input_name: &str,
    recount: bool,
    setup_prefix: Option<&str>,
) -> Result<Vec<FilePatch>> {
    let lines: Vec<&str> = input.lines().collect();
    let mut patches = Vec::new();
    let mut i = 0;
    let mut p_guess_for_traditional: Option<usize> = None;
    let setup_prefix_for_guess = setup_prefix.filter(|p| !p.is_empty());

    let p_strip = strip;
    let p_extended = strip.saturating_sub(1);

    while i < lines.len() {
        // Look for "diff --git" header or a bare ---/+++ pair.
        if lines[i].starts_with("diff --git ") {
            let mut fp = FilePatch {
                diff_old_path: None,
                diff_new_path: None,
                old_path: None,
                new_path: None,
                saw_old_header: false,
                saw_new_header: false,
                old_mode: None,
                new_mode: None,
                old_mode_line: None,
                new_mode_line: None,
                is_new: false,
                is_deleted: false,
                is_rename: false,
                is_copy: false,
                similarity_index: None,
                dissimilarity_index: None,
                old_oid: None,
                new_oid: None,
                binary_patch: None,
                is_binary: false,
                hunks: Vec::new(),
                ws_rule: 0,
                is_toplevel_relative: true,
            };

            let header_line = lines[i];
            let def_name = git_header_def_name(header_line, p_strip);

            // Parse "diff --git a/foo b/foo"
            let rest = &header_line["diff --git ".len()..];
            if let Some((a, b)) = split_diff_git_paths(rest) {
                fp.diff_old_path = Some(a.clone());
                fp.diff_new_path = Some(b.clone());
                fp.old_path = Some(skip_tree_prefix_str(&a, p_strip).ok_or_else(|| {
                    Error::Message(format!("malformed old path in diff --git header: {a}"))
                })?);
                fp.new_path = Some(skip_tree_prefix_str(&b, p_strip).ok_or_else(|| {
                    Error::Message(format!("malformed new path in diff --git header: {b}"))
                })?);
            }
            i += 1;

            // Parse extended headers
            while i < lines.len()
                && !lines[i].starts_with("--- ")
                && !lines[i].starts_with("diff --git ")
                && !lines[i].starts_with("@@ ")
            {
                let line = lines[i];
                let line_no = i + 1;
                if let Some(val) = line.strip_prefix("old mode ") {
                    let v = val.trim_end_matches('\r').trim_end();
                    if v.is_empty() {
                        return Err(Error::Message(format!(
                            "invalid mode on line {line_no}: {line}"
                        )));
                    }
                    fp.old_mode = Some(v.to_string());
                    fp.old_mode_line = Some(line_no);
                } else if let Some(val) = line.strip_prefix("new mode ") {
                    let v = val.trim_end_matches('\r').trim_end();
                    if v.is_empty() {
                        return Err(Error::Message(format!(
                            "invalid mode on line {line_no}: {line}"
                        )));
                    }
                    fp.new_mode = Some(v.to_string());
                    fp.new_mode_line = Some(line_no);
                } else if let Some(val) = line.strip_prefix("new file mode ") {
                    let v = val.trim_end_matches('\r').trim_end();
                    if v.is_empty() {
                        return Err(Error::Message(format!(
                            "invalid mode on line {line_no}: {line}"
                        )));
                    }
                    fp.is_new = true;
                    fp.new_mode = Some(v.to_string());
                    fp.new_mode_line = Some(line_no);
                } else if let Some(val) = line.strip_prefix("deleted file mode ") {
                    let v = val.trim_end_matches('\r').trim_end();
                    if v.is_empty() {
                        return Err(Error::Message(format!(
                            "invalid mode on line {line_no}: {line}"
                        )));
                    }
                    fp.is_deleted = true;
                    fp.old_mode = Some(v.to_string());
                    fp.old_mode_line = Some(line_no);
                } else if let Some(val) = line.strip_prefix("rename from ") {
                    fp.is_rename = true;
                    if let Some(p) = find_name_extended_header(val, p_extended) {
                        fp.old_path = Some(p);
                    }
                } else if let Some(val) = line.strip_prefix("rename to ") {
                    fp.is_rename = true;
                    if let Some(p) = find_name_extended_header(val, p_extended) {
                        fp.new_path = Some(p);
                    }
                } else if let Some(val) = line.strip_prefix("copy from ") {
                    fp.is_copy = true;
                    if let Some(p) = find_name_extended_header(val, p_extended) {
                        fp.old_path = Some(p);
                    }
                } else if let Some(val) = line.strip_prefix("copy to ") {
                    fp.is_copy = true;
                    if let Some(p) = find_name_extended_header(val, p_extended) {
                        fp.new_path = Some(p);
                    }
                } else if let Some(val) = line.strip_prefix("similarity index ") {
                    fp.similarity_index = val.trim_end_matches('%').parse().ok();
                } else if let Some(val) = line.strip_prefix("dissimilarity index ") {
                    fp.dissimilarity_index = val.trim_end_matches('%').parse().ok();
                } else if let Some(val) = line.strip_prefix("index ") {
                    // Parse "index abc123..def456 100644" or "index abc123..def456"
                    let mut parts = val.split_whitespace();
                    let hash_part = parts.next().unwrap_or("");
                    if let Some((old, new)) = hash_part.split_once("..") {
                        fp.old_oid = Some(old.to_string());
                        fp.new_oid = Some(new.to_string());
                    }
                    if let Some(mode_tok) = parts.next() {
                        let v = mode_tok.trim_end_matches('\r').trim_end();
                        if !v.is_empty() {
                            fp.old_mode = Some(v.to_string());
                            fp.old_mode_line = Some(line_no);
                        }
                    }
                } else if line == "GIT binary patch" {
                    let (binary_patch, next_i) = parse_binary_patch(&lines, i + 1)?;
                    fp.binary_patch = Some(binary_patch);
                    fp.is_binary = true;
                    i = next_i;
                    break;
                } else if line.starts_with("Binary files ") && line.ends_with(" differ") {
                    // Plain (non --binary) diff of a binary change; no payload to
                    // apply but stat/numstat must report it as binary.
                    fp.is_binary = true;
                }
                // skip other extended headers
                i += 1;
            }

            if let Some(dn) = def_name {
                if fp.old_path.is_none() {
                    fp.old_path = Some(dn.clone());
                }
                if fp.new_path.is_none() {
                    fp.new_path = Some(dn);
                }
            }

            // Parse ---/+++ headers if present
            if i < lines.len() && lines[i].starts_with("--- ") {
                let old_p = lines[i]["--- ".len()..].trim_end_matches(['\r', '\n']);
                let old_b = old_p.as_bytes();
                if is_dev_null_nameline(old_b) {
                    fp.old_path = Some("/dev/null".to_string());
                } else if let Some(p) = find_name_tab_terminated(old_b, p_strip) {
                    fp.old_path = Some(bytes_to_path_string(&p)?);
                }
                fp.saw_old_header = true;
                i += 1;
                if i < lines.len() && lines[i].starts_with("+++ ") {
                    let new_p = lines[i]["+++ ".len()..].trim_end_matches(['\r', '\n']);
                    let new_b = new_p.as_bytes();
                    if is_dev_null_nameline(new_b) {
                        fp.new_path = Some("/dev/null".to_string());
                    } else if let Some(p) = find_name_tab_terminated(new_b, p_strip) {
                        fp.new_path = Some(bytes_to_path_string(&p)?);
                    }
                    fp.saw_new_header = true;
                    i += 1;
                }
            }

            // Parse hunks
            while i < lines.len() && lines[i].starts_with("@@ ") {
                let (hunk, next_i) = parse_hunk(&lines, i, input_name, recount)?;
                fp.hunks.push(hunk);
                i = next_i;
            }

            sanitize_file_patch_headers(&mut fp);
            patches.push(fp);
        } else if lines[i].starts_with("--- ")
            && i + 1 < lines.len()
            && lines[i + 1].starts_with("+++ ")
        {
            let old_line = lines[i].as_bytes();
            let new_line = lines[i + 1].as_bytes();
            let mut fp = parse_traditional_patch_pair(
                old_line,
                new_line,
                strip,
                &mut p_guess_for_traditional,
                setup_prefix_for_guess,
            )?;
            i += 2;

            // Parse hunks
            while i < lines.len() && lines[i].starts_with("@@ ") {
                let (hunk, next_i) = parse_hunk(&lines, i, input_name, recount)?;
                fp.hunks.push(hunk);
                i = next_i;
            }

            sanitize_file_patch_headers(&mut fp);
            patches.push(fp);
        } else {
            i += 1;
        }
    }

    Ok(patches)
}

/// Parse a `GIT binary patch` payload.
fn parse_binary_patch(lines: &[&str], mut i: usize) -> Result<(BinaryPatchPayload, usize)> {
    let (forward_compressed, forward_declared_size) = parse_binary_literal(lines, &mut i)?;
    let (reverse_compressed, reverse_declared_size) =
        if i < lines.len() && lines[i].starts_with("literal ") {
            parse_binary_literal(lines, &mut i)?
        } else {
            (Vec::new(), 0)
        };

    Ok((
        BinaryPatchPayload {
            forward_compressed,
            forward_declared_size,
            reverse_compressed,
            reverse_declared_size,
        },
        i,
    ))
}

/// Parse one `literal <size>` block from a binary patch.
fn parse_binary_literal(lines: &[&str], i: &mut usize) -> Result<(Vec<u8>, usize)> {
    let header = lines.get(*i).copied().unwrap_or_default();
    let Some(size_str) = header.strip_prefix("literal ") else {
        return Err(Error::Message(format!(
            "unsupported binary patch section: '{header}'"
        )));
    };
    let declared_size: usize = size_str
        .trim()
        .parse()
        .map_err(|e: std::num::ParseIntError| {
            Error::Message(format!("invalid binary patch literal size: {e}"))
        })?;
    *i += 1;

    let mut compressed = Vec::new();
    while *i < lines.len() {
        let line = lines[*i];
        if line.is_empty() {
            *i += 1;
            break;
        }
        decode_binary_patch_line(line, &mut compressed)?;
        *i += 1;
    }

    Ok((compressed, declared_size))
}

/// Decode one binary patch payload line into compressed bytes.
fn decode_binary_patch_line(line: &str, out: &mut Vec<u8>) -> Result<()> {
    let mut chars = line.chars();
    let Some(len_ch) = chars.next() else {
        return Err(Error::Message(
            "empty binary patch payload line".to_string(),
        ));
    };
    let expected_len = decode_binary_line_len(len_ch)?;
    let body = chars.as_str().as_bytes();
    let decoded = crate::git_binary_base85::decode_body(body, expected_len)
        .map_err(|e| Error::Message(format!("invalid binary patch base85: {e}")))?;
    out.extend_from_slice(&decoded);
    Ok(())
}

fn decode_binary_line_len(ch: char) -> Result<usize> {
    if ch.is_ascii_uppercase() {
        return Ok((ch as u8 - b'A' + 1) as usize);
    }
    if ch.is_ascii_lowercase() {
        return Ok((ch as u8 - b'a' + 27) as usize);
    }
    Err(Error::Message(format!(
        "invalid binary patch line length marker: '{ch}'"
    )))
}

/// Inflate zlib-compressed binary payload.
pub fn inflate_binary_payload(compressed: &[u8]) -> Result<Vec<u8>> {
    use flate2::read::ZlibDecoder;
    use std::io::Read;

    let mut decoder = ZlibDecoder::new(compressed);
    let mut out = Vec::new();
    decoder
        .read_to_end(&mut out)
        .map_err(|e| Error::Message(format!("failed to inflate binary patch payload: {e}")))?;
    Ok(out)
}

/// Split the two path tokens from the remainder of a `diff --git` line (after `diff --git `).
fn split_diff_git_paths(s: &str) -> Option<(String, String)> {
    let s = s.trim_end_matches(['\r', '\n']);

    if s.as_bytes().first() == Some(&b'"') {
        let (first, rest_raw) = unquote_c_style_diff_prefix(s)?;
        let rest = rest_raw.trim_start_matches(|c: char| c.is_ascii_whitespace());
        if rest.is_empty() {
            return None;
        }
        if rest.as_bytes().first() == Some(&b'"') {
            let (second, _) = unquote_c_style_diff_prefix(rest)?;
            return Some((
                String::from_utf8_lossy(&first).into_owned(),
                String::from_utf8_lossy(&second).into_owned(),
            ));
        }
        let second = rest;
        if second.len() != first.len() || second.as_bytes() != first.as_slice() {
            return None;
        }
        return Some((
            String::from_utf8_lossy(&first).into_owned(),
            second.to_string(),
        ));
    }

    if let Some(pos) = s.find(" b/") {
        let a = &s[..pos];
        let b = &s[pos + 1..];
        return Some((a.to_string(), b.to_string()));
    }
    if s.starts_with("a/") {
        if let Some(pos) = s.find(" /dev/null") {
            let a = &s[..pos];
            return Some((a.to_string(), "/dev/null".to_string()));
        }
    }
    if let Some(b) = s.strip_prefix("/dev/null ") {
        return Some(("/dev/null".to_string(), b.to_string()));
    }

    let name = s.as_bytes();
    let line_len = name.len();
    let mut len = 0usize;
    while len < line_len {
        match name[len] {
            b'\n' => return None,
            b'\t' | b' ' => {
                if len + 1 > line_len {
                    return None;
                }
                let second = &name[len + 1..line_len];
                let names_match =
                    name.len() >= len && second.len() >= len && name[..len] == second[..len];
                let boundary_ok = second.get(len) == Some(&b'\n') || second.len() == len;
                if names_match && boundary_ok {
                    return Some((
                        String::from_utf8_lossy(&name[..len]).into_owned(),
                        String::from_utf8_lossy(second).into_owned(),
                    ));
                }
            }
            _ => {}
        }
        len += 1;
    }
    None
}

/// Parse a single hunk starting at line `i` (which should be an `@@` line).
fn parse_hunk(
    lines: &[&str],
    start: usize,
    input_name: &str,
    recount: bool,
) -> Result<(Hunk, usize)> {
    let header = lines[start];
    let (old_start, old_count, new_start, new_count) = parse_hunk_header(header)
        .map_err(|e| Error::Message(format!("invalid hunk header: {header}: {e}")))?;

    let mut hunk = Hunk {
        old_start,
        old_count,
        new_start,
        new_count,
        first_body_line: start + 2,
        lines: Vec::new(),
    };

    // Track how many old/new lines the body actually provides so a hunk that
    // ends prematurely is diagnosed like Git: "corrupt patch at <file>:<line>"
    // (t4012; Git parse_fragment returns -1 when oldlines/newlines remain).
    let mut old_seen = 0usize;
    let mut new_seen = 0usize;
    let mut i = start + 1;
    while i < lines.len() {
        let line = lines[i];
        if line.starts_with("@@ ") || line.starts_with("diff --git ") {
            break;
        }
        // `---` / `+++` with a space begin a new file header; do not treat `---` as a `-` hunk line
        // (would misparse `--- /dev/null` as a remove of `-- /dev/null` and merge the next file).
        if line.starts_with("--- ") || line.starts_with("+++ ") {
            break;
        }
        if line == "-- " {
            // format-patch signature separator; not part of hunk body
            break;
        }
        if let Some(rest) = line.strip_prefix('+') {
            hunk.lines.push(HunkLine::Add(rest.to_string()));
            new_seen += 1;
        } else if let Some(rest) = line.strip_prefix('-') {
            hunk.lines.push(HunkLine::Remove(rest.to_string()));
            old_seen += 1;
        } else if line.is_empty() {
            hunk.lines.push(HunkLine::Context(String::new()));
            old_seen += 1;
            new_seen += 1;
        } else if let Some(rest) = line.strip_prefix(' ') {
            // context line
            hunk.lines.push(HunkLine::Context(rest.to_string()));
            old_seen += 1;
            new_seen += 1;
        } else if line.starts_with('\\') {
            hunk.lines.push(HunkLine::NoNewline);
        } else {
            // Unknown line type — could be start of something else
            break;
        }
        i += 1;
    }

    if recount {
        hunk.old_count = old_seen;
        hunk.new_count = new_seen;
    } else if old_seen < old_count || new_seen < new_count {
        return Err(ApplyError::CorruptPatch {
            input: input_name.to_owned(),
            line: i + 1,
        }
        .into());
    }

    Ok((hunk, i))
}

/// Parse "@@ -old_start[,old_count] +new_start[,new_count] @@..."
fn parse_hunk_header(line: &str) -> Result<(usize, usize, usize, usize)> {
    // Find the range part between @@ markers
    let trimmed = line.trim_start_matches('@').trim_start();
    let end = trimmed.find(" @@").unwrap_or(trimmed.len());
    let range_part = &trimmed[..end];

    let parts: Vec<&str> = range_part.split_whitespace().collect();
    if parts.len() < 2 {
        return Err(Error::Message(
            "expected old and new range in hunk header".to_string(),
        ));
    }

    let (old_start, old_count) = parse_range(parts[0].trim_start_matches('-'))?;
    let (new_start, new_count) = parse_range(parts[1].trim_start_matches('+'))?;

    Ok((old_start, old_count, new_start, new_count))
}

/// Parse "N" or "N,M" into (start, count).
fn parse_range(s: &str) -> Result<(usize, usize)> {
    if let Some((start_s, count_s)) = s.split_once(',') {
        let start = start_s
            .parse::<usize>()
            .map_err(|e| Error::Message(e.to_string()))?;
        let count = count_s
            .parse::<usize>()
            .map_err(|e| Error::Message(e.to_string()))?;
        Ok((start, count))
    } else {
        let n: usize = s
            .parse()
            .map_err(|e: std::num::ParseIntError| Error::Message(e.to_string()))?;
        Ok((n, 1))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_simple_git_diff_into_one_file_patch() {
        let input = "diff --git a/foo.txt b/foo.txt\n\
                     index e69de29..d95f3ad 100644\n\
                     --- a/foo.txt\n\
                     +++ b/foo.txt\n\
                     @@ -0,0 +1 @@\n\
                     +hello\n";
        let patches = parse_patch(input, 1, "<test>", false, None).expect("parse");
        assert_eq!(patches.len(), 1);
        let fp = &patches[0];
        assert_eq!(fp.old_path.as_deref(), Some("foo.txt"));
        assert_eq!(fp.new_path.as_deref(), Some("foo.txt"));
        assert_eq!(fp.hunks.len(), 1);
        let hunk = &fp.hunks[0];
        assert_eq!(hunk.new_count, 1);
        assert!(matches!(hunk.lines.as_slice(), [HunkLine::Add(s)] if s == "hello"));
    }

    #[test]
    fn parses_new_file_mode_and_deletion() {
        let new_file = "diff --git a/n b/n\n\
                        new file mode 100644\n\
                        index 0000000..9daeafb\n\
                        --- /dev/null\n\
                        +++ b/n\n\
                        @@ -0,0 +1 @@\n\
                        +x\n";
        let patches = parse_patch(new_file, 1, "<test>", false, None).expect("parse");
        assert_eq!(patches.len(), 1);
        assert!(patches[0].is_new);
        assert_eq!(patches[0].new_mode.as_deref(), Some("100644"));

        let deleted = "diff --git a/d b/d\n\
                       deleted file mode 100644\n\
                       index 9daeafb..0000000\n\
                       --- a/d\n\
                       +++ /dev/null\n\
                       @@ -1 +0,0 @@\n\
                       -x\n";
        let patches = parse_patch(deleted, 1, "<test>", false, None).expect("parse");
        assert!(patches[0].is_deleted);
    }

    #[test]
    fn corrupt_hunk_is_reported_with_input_name_and_line() {
        // The body provides fewer lines than the header declares.
        let input = "--- a/x\n\
                     +++ b/x\n\
                     @@ -1,3 +1,3 @@\n\
                      one\n";
        let err = parse_patch(input, 1, "patch", false, None).unwrap_err();
        assert!(matches!(
            err,
            Error::Apply(ApplyError::CorruptPatch {
                ref input,
                line: 4
            }) if input == "patch"
        ));
        assert_eq!(
            err.git_stderr_message(),
            crate::diagnostics::error_line("corrupt patch at patch:4")
        );
    }

    #[test]
    fn parse_hunk_header_parses_ranges() {
        assert_eq!(parse_hunk_header("@@ -1,3 +2,4 @@").unwrap(), (1, 3, 2, 4));
        assert_eq!(parse_hunk_header("@@ -5 +6 @@ ctx").unwrap(), (5, 1, 6, 1));
    }

    #[test]
    fn invalid_hunk_header_chains_inner_message() {
        let err = parse_hunk_header("@@ -x +1 @@").unwrap_err();
        // The numeric parse failure must surface its own message.
        assert_eq!(err.to_string(), "invalid digit found in string");
    }
}

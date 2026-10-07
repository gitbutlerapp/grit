//! Git-compatible path normalization and helpers for `test-tool path-utils`.
//! Logic matches `git/path.c` (`normalize_path_copy`, `longest_ancestor_length`,
//! `relative_path`, `strip_path_suffix`) and `git/remote.c` (`relative_url`).

use std::borrow::Cow;
use std::path::{Component, Path, PathBuf};

/// Errors returned by Git-compatible path helper routines.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GitPathError {
    /// Normalization would escape above the root.
    EscapesRoot,
    /// A relative URL cannot be resolved against the provided remote URL.
    InvalidRelativeUrl,
}

#[inline]
fn is_dir_sep(c: u8) -> bool {
    c == b'/'
}

/// Purely textual path normalization matching Git's `normalize_path_copy`.
/// Returns [`GitPathError::EscapesRoot`] when `..` would escape above the root
/// (Git returns -1).
pub fn normalize_path_copy(src: &str) -> Result<String, GitPathError> {
    let is_abs = src.starts_with('/');
    let raw_ends_dir = {
        let stripped = src.trim_end_matches('/');
        stripped.ends_with("/.")
            || stripped.ends_with("/..")
            || src.ends_with('/')
            || src == "."
            || src == ".."
    };
    let trailing_slash = raw_ends_dir && !src.is_empty();
    let mut stack: Vec<String> = Vec::new();
    let bytes = src.as_bytes();
    let mut i = 0usize;
    if is_abs {
        i = 1;
    }
    while i < bytes.len() {
        while i < bytes.len() && bytes[i] == b'/' {
            i += 1;
        }
        if i >= bytes.len() {
            break;
        }
        let start = i;
        while i < bytes.len() && bytes[i] != b'/' {
            i += 1;
        }
        let part = &src[start..i];
        if part == "." {
            continue;
        }
        if part == ".." {
            if stack.pop().is_none() {
                return Err(GitPathError::EscapesRoot);
            }
        } else {
            stack.push(part.to_string());
        }
    }

    let mut out = if is_abs {
        if stack.is_empty() {
            "/".to_string()
        } else {
            "/".to_string() + &stack.join("/")
        }
    } else if stack.is_empty() {
        String::new()
    } else {
        stack.join("/")
    };
    if trailing_slash && !out.is_empty() && !out.ends_with('/') {
        out.push('/');
    }
    Ok(out)
}

fn chomp_trailing_dir_sep(path: &[u8], mut len: usize) -> usize {
    while len > 0 && is_dir_sep(path[len - 1]) {
        len -= 1;
    }
    len
}

/// Git's `stripped_path_suffix_offset` / `strip_path_suffix`.
pub fn strip_path_suffix(path: &str, suffix: &str) -> Option<String> {
    let path = path.as_bytes();
    let suffix = suffix.as_bytes();
    let mut path_len = path.len();
    let mut suffix_len = suffix.len();

    while suffix_len > 0 {
        if path_len == 0 {
            return None;
        }
        if is_dir_sep(path[path_len - 1]) {
            if !is_dir_sep(suffix[suffix_len - 1]) {
                return None;
            }
            path_len = chomp_trailing_dir_sep(path, path_len);
            suffix_len = chomp_trailing_dir_sep(suffix, suffix_len);
        } else if path[path_len - 1] != suffix[suffix_len - 1] {
            return None;
        } else {
            path_len -= 1;
            suffix_len -= 1;
        }
    }

    if path_len > 0 && !is_dir_sep(path[path_len - 1]) {
        return None;
    }
    let off = chomp_trailing_dir_sep(path, path_len);
    Some(String::from_utf8_lossy(&path[..off]).into_owned())
}

/// Git's `longest_ancestor_length` - normalizes `path` and each colon-separated prefix.
pub fn longest_ancestor_length(path: &str, prefixes_colon_sep: &str) -> Result<i32, GitPathError> {
    let path = normalize_path_copy(path)?;
    if path == "/" {
        return Ok(-1);
    }
    let mut max_len: i64 = -1;
    for ceil_raw in prefixes_colon_sep.split(':') {
        if ceil_raw.is_empty() {
            continue;
        }
        let ceil = normalize_path_copy(ceil_raw)?;
        let mut len = ceil.len();
        if len > 0 && ceil.as_bytes()[len - 1] == b'/' {
            len -= 1;
        }
        let p = path.as_bytes();
        let c = ceil.as_bytes();
        if len > p.len() || len > c.len() || p[..len] != c[..len] {
            continue;
        }
        // Match git/path.c: need a '/' after the ceiling and another path component (not exact path).
        if len == p.len() || p[len] != b'/' || p.get(len + 1).is_none() {
            continue;
        }
        if len as i64 > max_len {
            max_len = len as i64;
        }
    }
    Ok(max_len as i32)
}

fn have_same_root(path1: &str, path2: &str) -> bool {
    let abs1 = path1.starts_with('/');
    let abs2 = path2.starts_with('/');
    (abs1 && abs2) || (!abs1 && !abs2)
}

/// Git's `relative_path` from `path.c` (POSIX subset).
pub fn relative_path<'a>(in_path: &'a str, prefix: &'a str, sb: &'a mut String) -> Option<&'a str> {
    let in_len = in_path.len();
    let prefix_len = prefix.len();
    let mut in_off = 0usize;
    let mut prefix_off = 0usize;
    let mut i = 0usize;
    let mut j = 0usize;

    if in_len == 0 {
        return Some("./");
    }
    if prefix_len == 0 {
        return Some(in_path);
    }

    if !have_same_root(in_path, prefix) {
        return Some(in_path);
    }

    let in_b = in_path.as_bytes();
    let pre_b = prefix.as_bytes();

    while i < prefix_len && j < in_len && pre_b[i] == in_b[j] {
        if is_dir_sep(pre_b[i]) {
            while i < prefix_len && is_dir_sep(pre_b[i]) {
                i += 1;
            }
            while j < in_len && is_dir_sep(in_b[j]) {
                j += 1;
            }
            prefix_off = i;
            in_off = j;
        } else {
            i += 1;
            j += 1;
        }
    }

    if i >= prefix_len && prefix_off < prefix_len {
        if j >= in_len {
            in_off = in_len;
        } else if is_dir_sep(in_b[j]) {
            while j < in_len && is_dir_sep(in_b[j]) {
                j += 1;
            }
            in_off = j;
        } else {
            i = prefix_off;
        }
    } else if j >= in_len && in_off < in_len && is_dir_sep(pre_b[i]) {
        while i < prefix_len && is_dir_sep(pre_b[i]) {
            i += 1;
        }
        in_off = in_len;
    }

    let in_suffix = &in_path[in_off..];
    let in_suffix_len = in_suffix.len();

    if i >= prefix_len {
        if in_suffix_len == 0 {
            return Some("./");
        }
        return Some(in_suffix);
    }

    sb.clear();
    sb.reserve(in_suffix_len.saturating_add(prefix_len * 3));

    while i < prefix_len {
        if is_dir_sep(pre_b[i]) {
            sb.push_str("../");
            while i < prefix_len && is_dir_sep(pre_b[i]) {
                i += 1;
            }
            continue;
        }
        i += 1;
    }
    if prefix_len > 0 && !is_dir_sep(pre_b[prefix_len - 1]) {
        sb.push_str("../");
    }
    sb.push_str(in_suffix);

    Some(sb.as_str())
}

fn find_last_dir_sep(path: &str) -> Option<usize> {
    path.rfind('/')
}

fn chop_last_dir(remoteurl: &mut String, is_relative: bool) -> Result<bool, GitPathError> {
    if let Some(pos) = find_last_dir_sep(remoteurl.as_str()) {
        remoteurl.truncate(pos);
        return Ok(false);
    }
    if let Some(pos) = remoteurl.rfind(':') {
        remoteurl.truncate(pos);
        return Ok(true);
    }
    if is_relative || remoteurl == "." {
        return Err(GitPathError::InvalidRelativeUrl);
    }
    *remoteurl = ".".to_string();
    Ok(false)
}

fn url_is_local_not_ssh(url: &str) -> bool {
    crate::transport_path::url_is_local_not_ssh(url)
}

fn starts_with_dot_slash_native(s: &str) -> bool {
    s.starts_with("./")
}

fn starts_with_dot_dot_slash_native(s: &str) -> bool {
    s.starts_with("../")
}

fn ends_with_slash(url: &str) -> bool {
    url.ends_with('/')
}

/// Git's `relative_url` from `remote.c` (POSIX; no DOS drive handling).
pub fn relative_url(
    remote_url: &str,
    url: &str,
    up_path: Option<&str>,
) -> Result<String, GitPathError> {
    if !url_is_local_not_ssh(url) || url.starts_with('/') {
        return Ok(url.to_string());
    }

    let mut remoteurl = remote_url.to_string();
    let len = remoteurl.len();
    if len == 0 {
        return Err(GitPathError::InvalidRelativeUrl);
    }
    if remoteurl.ends_with('/') {
        remoteurl.truncate(len - 1);
    }

    let is_relative = if !url_is_local_not_ssh(&remoteurl) || remoteurl.starts_with('/') {
        false
    } else {
        if !starts_with_dot_slash_native(&remoteurl)
            && !starts_with_dot_dot_slash_native(&remoteurl)
        {
            remoteurl = format!("./{remoteurl}");
        }
        true
    };

    let mut url_rest = url;
    let mut colonsep = false;
    while !url_rest.is_empty() {
        if starts_with_dot_dot_slash_native(url_rest) {
            url_rest = &url_rest[3..];
            let seg = chop_last_dir(&mut remoteurl, is_relative)?;
            colonsep |= seg;
        } else if starts_with_dot_slash_native(url_rest) {
            url_rest = &url_rest[2..];
        } else {
            break;
        }
    }

    let sep = if colonsep { ":" } else { "/" };
    let mut combined = format!("{remoteurl}{sep}{url_rest}");
    if ends_with_slash(url) && combined.ends_with('/') {
        combined.pop();
    }

    let out = if starts_with_dot_slash_native(&combined) {
        combined[2..].to_string()
    } else {
        combined
    };

    match up_path {
        Some(up) if is_relative => Ok(format!("{up}{out}")),
        _ => Ok(out),
    }
}

/// Whether `path` is an absolute Unix-style path.
#[must_use]
pub fn is_absolute_path_unix(path: &str) -> bool {
    path.starts_with('/')
}

/// Git's `cleanup_path` from `path.c`: strip a single leading `./` and any
/// directory separators immediately following it. Internal consecutive slashes
/// (e.g. `info//sparse-checkout`) are deliberately preserved so the result
/// matches `git rev-parse --git-path` byte-for-byte.
#[must_use]
pub fn cleanup_path(path: &str) -> &str {
    if let Some(rest) = path.strip_prefix("./") {
        rest.trim_start_matches('/')
    } else {
        path
    }
}

/// The relative portion of a `--git-path` argument, mirroring how Git builds the
/// buffer in `repo_git_pathv`: the caller-supplied `fmt` string is appended to
/// `<git_dir>/` verbatim and only [`cleanup_path`] runs over the whole buffer.
/// In practice that means a single leading `/` (and a leading `./`) is dropped
/// from the user-supplied component while internal `//` runs are kept intact.
#[must_use]
pub fn git_path_relative_component(path: &str) -> &str {
    // Drop one leading slash (Git appends fmt right after "<git_dir>/", so a
    // user "/foo" would otherwise become "<git_dir>//foo"); keep the rest as-is.
    let trimmed = path.strip_prefix('/').unwrap_or(path);
    cleanup_path(trimmed)
}

/// One entry in Git's `common_list` (`git/path.c`).
struct CommonDir {
    is_dir: bool,
    is_common: bool,
    path: &'static str,
}

/// Git's `common_list` table from `git/path.c`. Each entry classifies a path
/// (or directory prefix) under the git dir as belonging to the common dir or to
/// the per-worktree git dir. The order is irrelevant for classification because
/// `trie_find` always selects the longest `/`-terminated matching prefix.
const COMMON_LIST: &[CommonDir] = &[
    CommonDir {
        is_dir: true,
        is_common: true,
        path: "branches",
    },
    CommonDir {
        is_dir: true,
        is_common: true,
        path: "common",
    },
    CommonDir {
        is_dir: true,
        is_common: true,
        path: "hooks",
    },
    CommonDir {
        is_dir: true,
        is_common: true,
        path: "info",
    },
    CommonDir {
        is_dir: false,
        is_common: false,
        path: "info/sparse-checkout",
    },
    CommonDir {
        is_dir: true,
        is_common: true,
        path: "logs",
    },
    CommonDir {
        is_dir: false,
        is_common: false,
        path: "logs/HEAD",
    },
    CommonDir {
        is_dir: true,
        is_common: false,
        path: "logs/refs/bisect",
    },
    CommonDir {
        is_dir: true,
        is_common: false,
        path: "logs/refs/rewritten",
    },
    CommonDir {
        is_dir: true,
        is_common: false,
        path: "logs/refs/worktree",
    },
    CommonDir {
        is_dir: true,
        is_common: true,
        path: "lost-found",
    },
    CommonDir {
        is_dir: true,
        is_common: true,
        path: "objects",
    },
    CommonDir {
        is_dir: true,
        is_common: true,
        path: "refs",
    },
    CommonDir {
        is_dir: true,
        is_common: false,
        path: "refs/bisect",
    },
    CommonDir {
        is_dir: true,
        is_common: false,
        path: "refs/rewritten",
    },
    CommonDir {
        is_dir: true,
        is_common: false,
        path: "refs/worktree",
    },
    CommonDir {
        is_dir: true,
        is_common: true,
        path: "remotes",
    },
    CommonDir {
        is_dir: true,
        is_common: true,
        path: "worktrees",
    },
    CommonDir {
        is_dir: true,
        is_common: true,
        path: "rr-cache",
    },
    CommonDir {
        is_dir: true,
        is_common: true,
        path: "svn",
    },
    CommonDir {
        is_dir: false,
        is_common: true,
        path: "config",
    },
    CommonDir {
        is_dir: false,
        is_common: true,
        path: "gc.pid",
    },
    CommonDir {
        is_dir: false,
        is_common: true,
        path: "packed-refs",
    },
    CommonDir {
        is_dir: false,
        is_common: true,
        path: "shallow",
    },
];

/// Git's `check_common` (`git/path.c`): decide, for the matched `common_list`
/// entry and the unmatched remainder of the key, whether the path is common.
fn check_common(entry: &CommonDir, unmatched: &[u8]) -> Option<bool> {
    let first = unmatched.first().copied();
    if entry.is_dir && (first.is_none() || first == Some(b'/')) {
        return Some(entry.is_common);
    }
    if !entry.is_dir && first.is_none() {
        return Some(entry.is_common);
    }
    None
}

/// Compressed-trie lookup with the same behavior as Git's `trie_find`, specialized to
/// `common_list` + `check_common`. Returns the longest `/`-or-`\0`-terminated
/// `common_list` prefix's classification for `key`, or `None` if no prefix
/// matches (treated as "not common" by callers).
///
/// Mirrors the C trie semantics including: partial normalization (consecutive
/// slashes are skipped) and the fallback to a shorter `/`-terminated prefix when
/// a longer node yields no verdict.
fn trie_find_common(key: &[u8]) -> Option<bool> {
    // The trie distinguishes nodes by their full path; longest match wins, but
    // when the deepest matching node declines (`check_common` -> None) and we are
    // at a `/` boundary, control falls back to the next-shorter prefix that has a
    // value. We emulate this by scanning candidate prefixes from longest to
    // shortest. A candidate is a `common_list` path P that is a prefix of the
    // normalized key, terminated in the key by `\0` or `/`.
    let norm = normalize_double_slashes(key);
    // Collect matching entries, longest path first.
    let mut matches: Vec<&CommonDir> = COMMON_LIST
        .iter()
        .filter(|e| key_has_prefix_node(&norm, e.path.as_bytes()))
        .collect();
    matches.sort_by_key(|a| std::cmp::Reverse(a.path.len()));
    for entry in matches {
        let plen = entry.path.len();
        let unmatched = &norm[plen..];
        if let Some(verdict) = check_common(entry, unmatched) {
            return Some(verdict);
        }
        // No verdict at this node. The C trie only falls back to a shorter
        // prefix; continue to the next (shorter) candidate.
    }
    None
}

/// Collapse runs of consecutive `/` to a single `/` (Git's partial path
/// normalization inside `trie_find`). A trailing slash is preserved as a single
/// slash so directory-prefix matching still sees the boundary.
fn normalize_double_slashes(key: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(key.len());
    let mut prev_slash = false;
    for &b in key {
        if b == b'/' {
            if !prev_slash {
                out.push(b);
            }
            prev_slash = true;
        } else {
            out.push(b);
            prev_slash = false;
        }
    }
    out
}

/// True when `node` is a prefix of `key` terminated by end-of-string or `/`.
fn key_has_prefix_node(key: &[u8], node: &[u8]) -> bool {
    if key.len() < node.len() || &key[..node.len()] != node {
        return false;
    }
    matches!(key.get(node.len()), None | Some(b'/'))
}

/// True when the relative git-dir path `rel` belongs to the common (shared)
/// directory, mirroring Git's `update_common_dir` decision (`git/path.c`).
///
/// `rel` is the component after the git dir (e.g. `logs/refs`, `config`,
/// `HEAD`). Any trailing `.lock` suffix is ignored for the decision, exactly as
/// `update_common_dir` strips `LOCK_SUFFIX` before consulting the trie.
#[must_use]
pub fn is_common_git_path(rel: &str) -> bool {
    let stripped = rel.strip_suffix(".lock").unwrap_or(rel);
    matches!(trie_find_common(stripped.as_bytes()), Some(true))
}

/// Like Git's `strbuf_realpath` / `test-tool path-utils real_path`: resolve symlinks by
/// walking path components (so symlink targets are interpreted at each step), then if the
/// leaf is missing, resolve the longest existing prefix and append the remainder.
#[must_use]
pub fn real_path_resolving(path: &str) -> PathBuf {
    let abs = if path.starts_with('/') {
        path.to_string()
    } else {
        let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
        let joined = format!("{}/{}", cwd.display(), path);
        normalize_path_copy(&joined).unwrap_or(joined)
    };
    let p = Path::new(&abs);
    if let Ok(c) = p.canonicalize() {
        return c;
    }
    let mut cur = PathBuf::from("/");
    for part in abs.trim_start_matches('/').split('/') {
        if part.is_empty() {
            continue;
        }
        cur.push(part);
        if let Ok(c) = cur.canonicalize() {
            cur = c;
        } else if let Ok(target) = std::fs::read_link(&cur) {
            cur.pop();
            cur.push(target);
            if let Ok(c) = cur.canonicalize() {
                cur = c;
            }
        }
    }
    if cur.exists() {
        return cur;
    }
    let mut base = cur.clone();
    let mut missing = Vec::new();
    while !base.as_os_str().is_empty() && !base.exists() {
        missing.push(base.file_name().unwrap_or_default().to_owned());
        if !base.pop() {
            break;
        }
    }
    if base.as_os_str().is_empty() {
        base = PathBuf::from("/");
    }
    let Ok(mut resolved) = base.canonicalize() else {
        return cur;
    };
    while let Some(name) = missing.pop() {
        resolved.push(name);
    }
    resolved
}

/// Git `setup.c` `abspath_part_inside_repo` (POSIX).
///
/// Strips the work tree from an absolute, normalized path, preserving symlink path
/// components when they are still under the work tree as a string prefix.
pub fn abspath_part_inside_repo(path: &str, work_tree: &Path) -> Option<String> {
    let normalized = normalize_path_copy(path).ok()?;
    if !normalized.starts_with('/') {
        return None;
    }
    let wt_display = work_tree.to_string_lossy();
    let wt_trim: &str = if wt_display == "/" {
        "/"
    } else {
        wt_display.trim_end_matches('/')
    };
    let wt_len = wt_trim.len();
    let p = normalized.as_str();
    let len = p.len();

    if wt_len <= len && p.starts_with(wt_trim) {
        if len > wt_len && p.as_bytes()[wt_len] == b'/' {
            return Some(p[wt_len + 1..].to_string());
        }
        if len == wt_len {
            return Some(String::new());
        }
        if wt_len > 0 && wt_trim.as_bytes()[wt_len - 1] == b'/' {
            return Some(p[wt_len..].trim_start_matches('/').to_string());
        }
    }

    let wt_canon = path_for_disk_compare(work_tree);
    let mut cum = String::new();
    for seg in p.split('/').filter(|s| !s.is_empty()) {
        cum.push('/');
        cum.push_str(seg);
        let rp = path_for_disk_compare(Path::new(&cum));
        if rp == wt_canon {
            if p.len() == cum.len() {
                return Some(String::new());
            }
            if p.as_bytes().get(cum.len()) == Some(&b'/') {
                return Some(p[cum.len() + 1..].to_string());
            }
        }
    }
    let full = path_for_disk_compare(Path::new(p));
    if full == wt_canon {
        return Some(String::new());
    }
    None
}

/// Strip Windows extended-length `\\?\` / `\\?\UNC\` prefixes for comparison and display.
#[must_use]
pub fn strip_verbatim_path_prefix_str(path: &str) -> Cow<'_, str> {
    if let Some(rest) = path.strip_prefix(r"\\?\UNC\") {
        Cow::Owned(format!(r"\\{rest}"))
    } else if let Some(rest) = path.strip_prefix(r"\\?\") {
        Cow::Borrowed(rest)
    } else {
        Cow::Borrowed(path)
    }
}

/// User-facing path: forward slashes, no verbatim prefix (Git-style init/status paths).
#[must_use]
pub fn user_display_path(path: &Path) -> String {
    strip_verbatim_path_prefix_str(&path.display().to_string()).replace('\\', "/")
}

/// Lexical absolute path for comparison (does not follow symlinks).
///
/// Strips Windows verbatim prefixes when present. Used for repository entry paths so
/// symlinks keep their repository spelling in status and pathspec resolution.
#[must_use]
pub fn path_lexical_for_disk_compare(path: &Path) -> PathBuf {
    let abs = std::path::absolute(path).unwrap_or_else(|_| path.to_path_buf());
    let normalized = lexical_normalize_path_buf(abs);
    #[cfg(windows)]
    {
        return PathBuf::from(
            strip_verbatim_path_prefix_str(&normalized.display().to_string()).into_owned(),
        );
    }
    normalized
}

fn lexical_normalize_path_buf(path: PathBuf) -> PathBuf {
    let mut out = PathBuf::new();
    for c in path.components() {
        match c {
            Component::Prefix(prefix) => out.push(prefix.as_os_str()),
            Component::RootDir => out.push(c),
            Component::CurDir => {}
            Component::ParentDir => {
                out.pop();
            }
            Component::Normal(_) => out.push(c),
        }
    }
    out
}

/// Repository-relative path with forward slashes when `path` lies under `work_tree`.
#[must_use]
pub fn strip_worktree_prefix(path: &Path, work_tree: &Path) -> Option<String> {
    let path_key = path_lexical_for_disk_compare(path);
    let wt_key = path_for_disk_compare(work_tree);
    let rel = path_key.strip_prefix(&wt_key).ok()?;
    Some(path_to_git_slash(rel))
}

/// Convert a filesystem path to Git's slash-separated repo-relative form.
#[must_use]
pub fn path_to_git_slash(path: &Path) -> String {
    path.components()
        .filter_map(|c| match c {
            Component::Normal(s) => Some(s.to_string_lossy().into_owned()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("/")
}

/// Path of `to` relative to `from`, for display (forward slashes).
///
/// Uses normalized paths (including Windows verbatim-prefix stripping) and walks up from
/// the common ancestor with `..` when the target is not under `from`.
#[must_use]
pub fn relative_path_for_display(from: &Path, to: &Path) -> Option<String> {
    let from_cmp = path_for_disk_compare(from);
    let to_cmp = path_lexical_for_disk_compare(to);
    if from_cmp == to_cmp {
        return Some(".".to_owned());
    }

    let from_parts: Vec<_> = from_cmp.components().collect();
    let to_parts: Vec<_> = to_cmp.components().collect();
    let common = from_parts
        .iter()
        .zip(to_parts.iter())
        .take_while(|(a, b)| a == b)
        .count();

    let mut segments: Vec<String> = Vec::new();
    for part in &from_parts[common..] {
        if matches!(part, Component::Normal(_)) {
            segments.push("..".to_owned());
        }
    }
    for part in &to_parts[common..] {
        match part {
            Component::Normal(name) => segments.push(name.to_string_lossy().into_owned()),
            Component::ParentDir => {
                segments.pop();
            }
            Component::CurDir | Component::RootDir | Component::Prefix(_) => {}
        }
    }

    if segments.is_empty() {
        Some(".".to_owned())
    } else {
        Some(segments.join("/"))
    }
}

/// Canonicalize a path for on-disk comparison (macOS `/private` aliasing).
///
/// On macOS, `/tmp` and `/private/tmp` refer to the same directory; Git stores and
/// accepts both spellings when matching paths against `core.worktree`.
///
/// On Windows, `canonicalize` may return `\\?\`-prefixed paths; those are stripped so
/// comparisons match non-verbatim spellings (`C:\…`).
#[must_use]
pub fn path_for_disk_compare(path: &Path) -> PathBuf {
    let canon = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
    #[cfg(target_os = "macos")]
    {
        if let Ok(stripped) = canon.strip_prefix("/private") {
            let without_private = PathBuf::from("/").join(stripped);
            if without_private.exists() {
                return without_private;
            }
        }
    }
    #[cfg(windows)]
    {
        return PathBuf::from(strip_verbatim_path_prefix_str(&canon.display().to_string()));
    }
    #[cfg(not(any(windows, target_os = "macos")))]
    {
        let _ = &canon;
    }
    canon
}

/// Git `setup.c` `prefix_path_gently` (POSIX).
pub fn prefix_path_gently(prefix: &str, path: &str, work_tree: &Path) -> Option<String> {
    if path.starts_with('/') {
        let n = normalize_path_copy(path).ok()?;
        abspath_part_inside_repo(&n, work_tree)
    } else {
        let concat = format!("{prefix}{path}");
        normalize_path_copy(&concat).ok()
    }
}

#[cfg(test)]
mod git_path_component_tests {
    use super::*;

    #[test]
    fn cleanup_path_strips_leading_dot_slash() {
        assert_eq!(cleanup_path("./foo"), "foo");
        assert_eq!(cleanup_path(".//foo"), "foo");
        assert_eq!(cleanup_path("foo"), "foo");
    }

    #[test]
    fn cleanup_path_keeps_internal_double_slashes() {
        // Git's cleanup_path never collapses interior consecutive slashes.
        assert_eq!(
            cleanup_path("info//sparse-checkout"),
            "info//sparse-checkout"
        );
        assert_eq!(cleanup_path("./info//grafts"), "info//grafts");
    }

    #[test]
    fn git_path_component_drops_one_leading_slash_keeps_interior() {
        assert_eq!(
            git_path_relative_component("info//sparse-checkout"),
            "info//sparse-checkout"
        );
        assert_eq!(git_path_relative_component("/info//grafts"), "info//grafts");
        assert_eq!(git_path_relative_component("HEAD"), "HEAD");
    }

    #[test]
    fn is_common_git_path_matches_git_common_list() {
        // Common (resolved against the common dir) — t0060 cases.
        for p in [
            "logs/refs",
            "logs/refs/",
            "logs/refs/bisec/foo",
            "logs/refs/bisec",
            "logs/refs/bisectfoo",
            "objects",
            "objects/bar",
            "info/exclude",
            "info/grafts",
            "remotes/bar",
            "branches/bar",
            "logs/refs/heads/main",
            "refs/heads/main",
            "hooks/me",
            "config",
            "packed-refs",
            "shallow",
            "common",
            "common/file",
        ] {
            assert!(is_common_git_path(p), "{p} should be common");
        }
        // Per-worktree (resolved against the git dir) — t0060 cases.
        for p in [
            "index",
            "index.lock",
            "HEAD",
            "logs/HEAD",
            "logs/HEAD.lock",
            "logs/refs/bisect/foo",
            "info/sparse-checkout",
            "refs/bisect/foo",
        ] {
            assert!(!is_common_git_path(p), "{p} should be worktree-local");
        }
    }

    #[test]
    fn strip_verbatim_extended_path_prefix() {
        assert_eq!(strip_verbatim_path_prefix_str(r"\\?\C:\tmp\p"), r"C:\tmp\p");
        assert_eq!(
            strip_verbatim_path_prefix_str(r"\\?\UNC\server\share\dir"),
            r"\\server\share\dir"
        );
        assert_eq!(strip_verbatim_path_prefix_str(r"C:\tmp\p"), r"C:\tmp\p");
    }

    #[test]
    fn user_display_path_uses_forward_slashes() {
        assert_eq!(
            user_display_path(Path::new(r"\\?\C:\tmp\p\.git")),
            "C:/tmp/p/.git"
        );
    }

    #[test]
    fn relative_path_for_display_above_and_below_cwd() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let repo = tmp.path().join("repo");
        let deep = repo.join("d");
        std::fs::create_dir_all(&deep).expect("mkdir");
        std::fs::write(repo.join("a.txt"), b"root").expect("write");
        std::fs::write(deep.join("a.txt"), b"nested").expect("write");

        let cwd = deep.canonicalize().expect("canonicalize cwd");
        let above = repo
            .join("a.txt")
            .canonicalize()
            .expect("canonicalize above");
        let below = deep
            .join("a.txt")
            .canonicalize()
            .expect("canonicalize below");

        assert_eq!(
            relative_path_for_display(&cwd, &above).as_deref(),
            Some("../a.txt")
        );
        assert_eq!(
            relative_path_for_display(&cwd, &below).as_deref(),
            Some("a.txt")
        );
    }

    #[cfg(unix)]
    #[test]
    fn strip_worktree_prefix_preserves_symlink_spelling() {
        use std::os::unix::fs::symlink;

        let tmp = tempfile::tempdir().expect("tempdir");
        let repo = tmp.path().join("repo");
        std::fs::create_dir_all(repo.join("d")).expect("mkdir");
        let outside = tmp.path().join("outside");
        std::fs::create_dir_all(&outside).expect("mkdir outside");
        symlink(&outside, repo.join("d/link")).expect("symlink");
        assert_eq!(
            strip_worktree_prefix(&repo.join("d/link"), &repo).as_deref(),
            Some("d/link")
        );
    }

    #[test]
    fn strip_worktree_prefix_for_nested_file() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let repo = tmp.path().join("repo");
        std::fs::create_dir_all(repo.join("d")).expect("mkdir");
        let file = repo.join("d/a.txt");
        std::fs::write(&file, b"x").expect("write");
        assert_eq!(
            strip_worktree_prefix(&file, &repo).as_deref(),
            Some("d/a.txt")
        );
    }

    #[test]
    fn relative_path_preserves_interior_double_slash_suffix() {
        // Mirrors `git rev-parse --git-path info//sparse-checkout`: the suffix
        // below the shared prefix is copied verbatim, double slash intact.
        let mut sb = String::new();
        let rel = relative_path("/repo/.git/info//sparse-checkout", "/repo", &mut sb);
        assert_eq!(rel, Some(".git/info//sparse-checkout"));
    }
}

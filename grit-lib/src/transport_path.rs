//! Safety checks and path resolution for local transport URLs (matches Git `connect.c` / `path.c`).

use std::path::{Path, PathBuf};

use thiserror::Error;

use crate::error::Error;

/// Errors returned while validating local transport paths.
#[derive(Clone, Debug, Error, PartialEq, Eq)]
pub enum TransportPathError {
    /// A repository path begins with `-` and could be interpreted as a command option.
    #[error("fatal: strange pathname '{0}' blocked")]
    OptionLikePath(String),
}

/// Returns true when `s` is non-empty and begins with `-`, matching Git's
/// `looks_like_command_line_option` (used before quoting a path for shell-backed transport).
#[must_use]
pub fn looks_like_command_line_option(s: &str) -> bool {
    !s.is_empty() && s.starts_with('-')
}

/// Repository root used to resolve relative `remote.<name>.url` values — the linked or main
/// worktree directory when known, otherwise inferred from `git_dir` (including linked
/// worktree admin dirs via `gitdir` / `commondir`).
#[must_use]
pub fn configured_remote_base(git_dir: &Path, work_tree: Option<&Path>) -> PathBuf {
    if let Some(wt) = work_tree {
        return wt.to_path_buf();
    }
    if git_dir.join("commondir").is_file() {
        if let Ok(wt) = crate::worktree::read_worktree_path(git_dir) {
            return wt;
        }
    }
    if git_dir.file_name().is_some_and(|name| name == ".git") {
        git_dir
            .parent()
            .map(Path::to_path_buf)
            .unwrap_or_else(|| git_dir.to_path_buf())
    } else {
        git_dir.to_path_buf()
    }
}

/// True when a clone source should be stored as an absolute filesystem path (scheme-less
/// local paths only — not `file://`, HTTP, SSH, etc.).
#[must_use]
pub fn should_store_absolute_local_clone_url(url: &str) -> bool {
    let url = url.trim();
    !url.starts_with("file://") && is_local_path_remote_url(url)
}

/// Git `url_is_local_not_ssh` (`connect.c`): local unless scp-style `host:path` with no directory
/// separator before the first `:` (Windows drive letters and `\` separators included).
#[must_use]
pub fn url_is_local_not_ssh(url: &str) -> bool {
    let url = url.trim();
    let colon = url.find(':');
    match colon {
        None => true,
        Some(ci) => {
            let sep = url[..ci].find(['/', '\\']);
            if sep.is_some() {
                return true;
            }
            let b = url.as_bytes();
            ci == 1 && b.first().is_some_and(u8::is_ascii_alphabetic)
        }
    }
}

/// Strip the Windows verbatim `\\?\` prefix when present.
#[must_use]
pub fn strip_verbatim_path_prefix(path: &str) -> &str {
    path.strip_prefix(r"\\?\").unwrap_or(path)
}

/// Normalize a local path for storage in `remote.*.url` (Git-style forward slashes, no `\\?\`).
#[must_use]
pub fn normalize_local_path_for_config(path: &Path) -> String {
    let lossy = path.to_string_lossy();
    let stripped = strip_verbatim_path_prefix(&lossy);
    #[cfg(windows)]
    {
        stripped.replace('\\', "/")
    }
    #[cfg(not(windows))]
    {
        stripped.to_string()
    }
}

/// Absolute form of a local clone source path, matching Git's `absolute_pathdup`: prepend the
/// current directory when relative, but leave `.`/`..`/`./` components untouched (no
/// normalization). The parent of `source_path` is canonicalized when possible (Git resolves
/// the cwd via `getcwd`).
#[must_use]
pub fn absolute_local_clone_source_url(source_path: &Path) -> String {
    let absolute = if source_path.is_absolute() {
        source_path.to_path_buf()
    } else {
        let cwd = match std::env::current_dir() {
            Ok(c) => c.canonicalize().unwrap_or(c),
            Err(_) => return normalize_local_path_for_config(source_path),
        };
        cwd.join(source_path)
    };
    normalize_local_path_for_config(&absolute)
}

/// Decode a `file://` remote URL into a local filesystem path (percent-decoding and `/C:/` → `C:/`).
///
/// # Errors
///
/// Returns [`Error::Message`] when a `%` escape is truncated or invalid.
pub fn file_url_to_local_path(url: &str) -> Result<String, Error> {
    let trimmed = url.trim();
    let mut path_part = trimmed
        .strip_prefix("file://")
        .ok_or_else(|| Error::Message("not a file:// URL".to_owned()))?;
    if path_part.starts_with("localhost") {
        path_part = &path_part["localhost".len()..];
    }
    let decoded = percent_decode_path(path_part)?;
    let decoded = String::from_utf8(decoded).map_err(|_| {
        Error::Message("file URL path is not valid UTF-8 after decoding".to_owned())
    })?;
    Ok(dos_file_url_path_to_local(&decoded))
}

fn dos_file_url_path_to_local(path: &str) -> String {
    let path = strip_verbatim_path_prefix(path);
    if path.len() >= 3 {
        let b = path.as_bytes();
        if b[0].is_ascii_alphabetic() && b[1] == b':' && (b[2] == b'/' || b[2] == b'\\') {
            return path.to_string();
        }
    }
    if path.starts_with('/') && path.len() >= 4 {
        let b = path.as_bytes();
        if b[1].is_ascii_alphabetic() && b[2] == b':' && (b[3] == b'/' || b[3] == b'\\') {
            return path[1..].to_string();
        }
    }
    path.to_string()
}

fn percent_decode_path(path: &str) -> Result<Vec<u8>, Error> {
    let bytes = path.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' {
            let h1 = bytes
                .get(i + 1)
                .ok_or_else(|| Error::Message("bad % escape in file URL".to_owned()))?;
            let h2 = bytes
                .get(i + 2)
                .ok_or_else(|| Error::Message("bad % escape in file URL".to_owned()))?;
            let byte = u8::from_str_radix(
                std::str::from_utf8(&[*h1, *h2])
                    .map_err(|_| Error::Message("bad % escape in file URL".to_owned()))?,
                16,
            )
            .map_err(|_| Error::Message("bad % escape in file URL".to_owned()))?;
            out.push(byte);
            i += 3;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    Ok(out)
}

fn local_path_from_remote_url(url: &str) -> Result<PathBuf, Error> {
    let trimmed = url.trim();
    let path_str = if trimmed.starts_with("file://") {
        file_url_to_local_path(trimmed)?
    } else {
        dos_file_url_path_to_local(strip_verbatim_path_prefix(trimmed))
    };
    Ok(PathBuf::from(path_str))
}

/// True when `url` names a local filesystem remote (not `http(s)://`, `git://`, `ssh`, or `ext::`).
#[must_use]
pub fn is_local_path_remote_url(url: &str) -> bool {
    let url = url.trim();
    !(url.starts_with("git://")
        || url.starts_with("http://")
        || url.starts_with("https://")
        || url.starts_with("ext::")
        || crate::transport::is_ssh_url(url))
}

/// Resolve a local remote URL to the remote repository's git directory.
///
/// Relative paths are interpreted from [`configured_remote_base`], not from the process
/// current working directory. This matches Git's handling of configured `remote.<name>.url`
/// during fetch and push.
#[must_use]
pub fn resolve_local_remote_git_dir(
    url: &str,
    git_dir: &Path,
    work_tree: Option<&Path>,
) -> PathBuf {
    let mut remote_path = local_path_from_remote_url(url)
        .unwrap_or_else(|_| PathBuf::from(strip_verbatim_path_prefix(url.trim())));
    if remote_path.is_relative() {
        let base = configured_remote_base(git_dir, work_tree);
        remote_path = base.join(&remote_path);
    }
    local_git_dir_from_filesystem_path(&remote_path)
}

fn local_git_dir_from_filesystem_path(path: &Path) -> PathBuf {
    if path.ends_with(".git") || path.join("HEAD").is_file() {
        return path.to_path_buf();
    }
    let dot_git = path.join(".git");
    if dot_git.is_dir() {
        dot_git
    } else {
        path.to_path_buf()
    }
}

/// Rejects repository path strings that could be mistaken for options when passed to a shell.
///
/// Git dies with `strange pathname '%s' blocked` when the parsed local path starts with `-`.
/// Absolute paths like `/tmp/-repo.git` are allowed because the path string begins with `/`.
pub fn check_local_url_path_not_option_like(url: &str) -> Result<(), TransportPathError> {
    let path = url
        .strip_prefix("file://")
        .unwrap_or(url)
        .split('?')
        .next()
        .unwrap_or("");
    if looks_like_command_line_option(path) {
        return Err(TransportPathError::OptionLikePath(path.to_owned()));
    }
    Ok(())
}

/// Error returned by [`git_url_basename`] when no directory name can be derived from a URL.
#[derive(Clone, Debug, Error, PartialEq, Eq)]
#[error("No directory name could be guessed.\nPlease specify a directory on the command line")]
pub struct NoDirectoryName;

/// POSIX directory separator test, matching Git's `is_dir_sep` on non-Windows builds.
#[inline]
fn is_dir_sep(b: u8) -> bool {
    b == b'/'
}

/// Derive the "humanish" directory name Git would use for `git clone <repo>` when no explicit
/// target directory is given. It matches the behavior of Git's `git_url_basename`:
/// it operates on the **raw URL string** (not a pre-parsed path), so it can
/// fall back to the hostname when the URL has no path component (e.g. `ssh://host/` → `host`).
///
/// # Parameters
/// - `repo`: the raw repository URL or path exactly as passed on the command line.
/// - `is_bundle`: strip a trailing `.bundle` suffix instead of `.git`.
/// - `is_bare`: append `.git` to the guessed name (for `--bare` clones).
///
/// # Errors
/// Returns [`NoDirectoryName`] when the URL collapses to an empty (or single-slash) name, which
/// is the condition under which Git dies asking for an explicit directory.
///
/// Note: callers handle the `--mirror` exception (bare clone without a `.git` directory suffix)
/// by passing `is_bare = false`, matching Git's `option_bare && !option_mirror` logic.
pub fn git_url_basename(
    repo: &str,
    is_bundle: bool,
    is_bare: bool,
) -> Result<String, NoDirectoryName> {
    let bytes = repo.as_bytes();
    let mut end = bytes.len();

    // Skip scheme (everything up to and including "://").
    let mut start = match repo.find("://") {
        Some(idx) => idx + 3,
        None => 0,
    };

    // Skip authentication data, greedily up to the last '@' before the first dir separator.
    let mut ptr = start;
    while ptr < end && !is_dir_sep(bytes[ptr]) {
        if bytes[ptr] == b'@' {
            start = ptr + 1;
        }
        ptr += 1;
    }

    // Strip trailing spaces, slashes and a trailing "/.git".
    while start < end && (is_dir_sep(bytes[end - 1]) || bytes[end - 1].is_ascii_whitespace()) {
        end -= 1;
    }
    if end > start + 5 && is_dir_sep(bytes[end - 5]) && &bytes[end - 4..end] == b".git" {
        end -= 5;
        while start < end && is_dir_sep(bytes[end - 1]) {
            end -= 1;
        }
    }

    if end < start {
        return Err(NoDirectoryName);
    }

    // Strip a trailing port number when we have only a hostname (no dir separator but a colon).
    // This must NOT strip URIs like '/foo/bar:2222.git', which should guess dir '2222' for
    // backwards compatibility.
    let slice = &bytes[start..end];
    if !slice.contains(&b'/') && slice.contains(&b':') {
        let mut p = end;
        while start < p && bytes[p - 1].is_ascii_digit() && bytes[p - 1] != b':' {
            p -= 1;
        }
        if start < p && bytes[p - 1] == b':' {
            end = p - 1;
        }
    }

    // Find last component; colons also act as separators for backwards compatibility
    // (`foo:bar.git` → `bar`).
    let mut p = end;
    while start < p && !is_dir_sep(bytes[p - 1]) && bytes[p - 1] != b':' {
        p -= 1;
    }
    start = p;

    // Strip a trailing .{bundle,git}.
    let suffix: &[u8] = if is_bundle { b".bundle" } else { b".git" };
    let mut len = end - start;
    if len >= suffix.len() && &bytes[start + len - suffix.len()..start + len] == suffix {
        len -= suffix.len();
    }

    if len == 0 || (len == 1 && bytes[start] == b'/') {
        return Err(NoDirectoryName);
    }

    let core = &repo[start..start + len];
    let mut dir = if is_bare {
        format!("{core}.git")
    } else {
        core.to_string()
    };

    dir = collapse_control_and_whitespace(&dir);
    Ok(dir)
}

/// Replace runs of control characters and whitespace in a guessed directory name with a single
/// ASCII space, then strip leading and trailing spaces — mirroring the final pass of Git's
/// `git_url_basename`.
fn collapse_control_and_whitespace(dir: &str) -> String {
    let mut out = String::with_capacity(dir.len());
    let mut prev_space = true; // strip leading whitespace
    for &b in dir.as_bytes() {
        let ch = if b < 0x20 { b' ' } else { b };
        if ch.is_ascii_whitespace() {
            if prev_space {
                continue;
            }
            prev_space = true;
        } else {
            prev_space = false;
        }
        out.push(ch as char);
    }
    if prev_space {
        while out.ends_with(' ') {
            out.pop();
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn basename(url: &str) -> String {
        git_url_basename(url, false, false).expect("dir name")
    }

    #[test]
    fn scp_style_basic() {
        assert_eq!(basename("host:foo"), "foo");
        assert_eq!(basename("host:foo.git"), "foo");
        assert_eq!(basename("host:foo/.git"), "foo");
    }

    #[test]
    fn ssh_url_basic() {
        assert_eq!(basename("ssh://host/foo"), "foo");
        assert_eq!(basename("ssh://host/foo.git"), "foo");
        assert_eq!(basename("ssh://host/foo/.git"), "foo");
    }

    #[test]
    fn trailing_slashes_and_git() {
        assert_eq!(basename("ssh://host/foo/"), "foo");
        assert_eq!(basename("ssh://host/foo///"), "foo");
        assert_eq!(basename("ssh://host/foo/.git/"), "foo");
        assert_eq!(basename("ssh://host/foo.git/"), "foo");
        assert_eq!(basename("ssh://host/foo.git///"), "foo");
        assert_eq!(basename("ssh://host/foo///.git/"), "foo");
        assert_eq!(basename("ssh://host/foo/.git///"), "foo");

        assert_eq!(basename("host:foo/"), "foo");
        assert_eq!(basename("host:foo///"), "foo");
        assert_eq!(basename("host:foo.git/"), "foo");
        assert_eq!(basename("host:foo/.git/"), "foo");
        assert_eq!(basename("host:foo.git///"), "foo");
        assert_eq!(basename("host:foo///.git/"), "foo");
        assert_eq!(basename("host:foo/.git///"), "foo");
    }

    #[test]
    fn empty_path_defaults_to_hostname() {
        assert_eq!(basename("ssh://host/"), "host");
        assert_eq!(basename("ssh://host:1234/"), "host");
        assert_eq!(basename("ssh://user@host/"), "host");
        assert_eq!(basename("host:/"), "host");
    }

    #[test]
    fn auth_material_is_redacted() {
        assert_eq!(basename("ssh://user:password@host/"), "host");
        assert_eq!(basename("ssh://user:password@host:1234/"), "host");
        assert_eq!(basename("ssh://user:passw@rd@host:1234/"), "host");
        assert_eq!(basename("user@host:/"), "host");
        assert_eq!(basename("user:password@host:/"), "host");
        assert_eq!(basename("user:passw@rd@host:/"), "host");
    }

    #[test]
    fn auth_like_material_kept_in_path() {
        assert_eq!(basename("ssh://host/foo@bar"), "foo@bar");
        assert_eq!(basename("ssh://host/foo@bar.git"), "foo@bar");
        assert_eq!(basename("ssh://user:password@host/foo@bar"), "foo@bar");
        assert_eq!(basename("ssh://user:passw@rd@host/foo@bar.git"), "foo@bar");
        assert_eq!(basename("host:/foo@bar"), "foo@bar");
        assert_eq!(basename("host:/foo@bar.git"), "foo@bar");
        assert_eq!(basename("user:password@host:/foo@bar"), "foo@bar");
        assert_eq!(basename("user:passw@rd@host:/foo@bar.git"), "foo@bar");
    }

    #[test]
    fn trailing_port_like_numbers_in_path_kept() {
        assert_eq!(basename("ssh://user:password@host/test:1234"), "1234");
        assert_eq!(basename("ssh://user:password@host/test:1234.git"), "1234");
    }

    #[test]
    fn bare_appends_git() {
        assert_eq!(
            git_url_basename("host:foo", false, true).unwrap(),
            "foo.git"
        );
        assert_eq!(
            git_url_basename("host:foo.git", false, true).unwrap(),
            "foo.git"
        );
    }

    #[test]
    fn empty_name_is_error() {
        assert_eq!(git_url_basename("/", false, false), Err(NoDirectoryName));
        assert_eq!(git_url_basename("", false, false), Err(NoDirectoryName));
    }

    #[test]
    fn configured_remote_base_worktree_and_bare() {
        assert_eq!(
            configured_remote_base(Path::new("/tmp/wt/.git"), None),
            PathBuf::from("/tmp/wt")
        );
        assert_eq!(
            configured_remote_base(Path::new("/tmp/remote.git"), None),
            PathBuf::from("/tmp/remote.git")
        );
        assert_eq!(
            configured_remote_base(
                Path::new("/tmp/main/.git/worktrees/feature"),
                Some(Path::new("/tmp/linked"))
            ),
            PathBuf::from("/tmp/linked")
        );
    }

    #[test]
    fn resolve_local_remote_git_dir_from_relative_url() {
        let base = PathBuf::from("/tmp/parent/clone");
        let git_dir = base.join(".git");
        let resolved = resolve_local_remote_git_dir("origin.git", &git_dir, Some(&base));
        assert_eq!(resolved, base.join("origin.git"));
    }

    #[test]
    fn should_store_absolute_local_clone_url_classification() {
        assert!(super::should_store_absolute_local_clone_url("origin.git"));
        assert!(!super::should_store_absolute_local_clone_url(
            "file:///tmp/x.git"
        ));
        assert!(!super::should_store_absolute_local_clone_url(
            "https://h/r.git"
        ));
    }

    #[test]
    fn is_local_path_remote_url_classification() {
        assert!(is_local_path_remote_url("./foo.git"));
        assert!(is_local_path_remote_url("/abs/foo.git"));
        assert!(is_local_path_remote_url("file:///tmp/x"));
        assert!(!is_local_path_remote_url("https://example.com/x.git"));
        assert!(!is_local_path_remote_url("git://host/x.git"));
        assert!(!is_local_path_remote_url("host:repo.git"));
        assert!(is_local_path_remote_url("C:/tmp/x"));
        assert!(is_local_path_remote_url(r"C:\tmp\x"));
    }

    #[test]
    fn url_is_local_not_ssh_windows_drive_and_scp() {
        assert!(url_is_local_not_ssh("C:/tmp/src"));
        assert!(url_is_local_not_ssh(r"C:\tmp\src"));
        assert!(url_is_local_not_ssh(r"\\?\C:\tmp\src"));
        assert!(url_is_local_not_ssh("../t/src"));
        assert!(url_is_local_not_ssh("src"));
        assert!(!url_is_local_not_ssh("host:repo.git"));
    }

    #[test]
    fn file_url_to_local_path_decodes_drive() {
        assert_eq!(
            file_url_to_local_path("file:///C:/tmp/t/src").unwrap(),
            "C:/tmp/t/src"
        );
        assert_eq!(file_url_to_local_path("file:///tmp/x").unwrap(), "/tmp/x");
        assert_eq!(
            file_url_to_local_path("file://localhost/tmp/x").unwrap(),
            "/tmp/x"
        );
    }

    #[test]
    fn file_url_preserves_percent_encoded_question_mark() {
        let base = std::env::temp_dir().join(format!("grit-file-qmark-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(&base).expect("temp base");
        let repo_dir = base.join("origin?repo.git");
        std::fs::create_dir_all(&repo_dir).expect("repo dir");
        std::fs::write(repo_dir.join("HEAD"), "ref: refs/heads/main\n").expect("HEAD");

        let url = format!(
            "file://{}/origin%3Frepo.git",
            base.to_str().expect("utf8 base")
        );
        let decoded = file_url_to_local_path(&url).expect("decode");
        assert!(
            decoded.ends_with("origin?repo.git"),
            "decoded path must keep literal ?, got {decoded}"
        );
        let resolved =
            resolve_local_remote_git_dir(&url, &base.join("clone/.git"), Some(&base.join("clone")));
        assert_eq!(resolved, repo_dir);

        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn absolute_local_clone_source_url_keeps_dot_components() {
        let base = std::env::temp_dir().join(format!("grit-abs-clone-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(&base).expect("temp base");
        let prev = std::env::current_dir().ok();
        std::env::set_current_dir(&base).expect("chdir");
        let stored = absolute_local_clone_source_url(Path::new("./nested/../peer"));
        if let Some(p) = prev.as_ref() {
            let _ = std::env::set_current_dir(p);
        }
        let _ = std::fs::remove_dir_all(&base);
        assert!(
            stored.contains("nested/../peer") || stored.ends_with("peer"),
            "expected dot components preserved, got {stored}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn absolute_local_clone_source_url_preserves_symlink() {
        use std::os::unix::fs::symlink;

        let base = std::env::temp_dir().join(format!("grit-abs-symlink-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(&base).expect("temp base");
        let real = base.join("real-src");
        std::fs::create_dir_all(&real).expect("real dir");
        let link = base.join("link-src");
        symlink(&real, &link).expect("symlink");

        let stored = absolute_local_clone_source_url(&link);
        let _ = std::fs::remove_dir_all(&base);

        assert_eq!(stored, normalize_local_path_for_config(&link));
        assert_ne!(stored, normalize_local_path_for_config(&real));
    }

    #[test]
    fn normalize_local_path_for_config_strips_verbatim() {
        let path = Path::new(r"\\?\C:\tmp\t\src");
        #[cfg(windows)]
        assert_eq!(normalize_local_path_for_config(path), "C:/tmp/t/src");
        #[cfg(not(windows))]
        assert_eq!(normalize_local_path_for_config(path), r"C:\tmp\t\src");
    }
}

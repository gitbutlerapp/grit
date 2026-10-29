//! `.gitmodules` validation (Git `fsck` / `submodule-config` parity).
//!
//! Submodule `path` and `url` values must not look like command-line options
//! (non-empty and starting with `-`). See Git's `looks_like_command_line_option` in `path.c`.
//!
//! Submodule name and URL validation rules matching Git's (`check_submodule_name`,
//! `check_submodule_url`).

use std::collections::HashSet;
use std::io::Write;
use std::path::Path;
use std::path::PathBuf;

use crate::config::{ConfigFile, ConfigScope};
use crate::dotfile::{
    is_hfs_dot_gitattributes, is_hfs_dot_gitignore, is_hfs_dot_gitmodules, is_hfs_dot_mailmap,
    is_ntfs_dot_gitattributes, is_ntfs_dot_gitignore, is_ntfs_dot_gitmodules, is_ntfs_dot_mailmap,
};
use crate::error::Result;
use crate::objects::{parse_commit, parse_tree, ObjectId, ObjectKind, TreeEntry};
use crate::odb::store::PackedObjects;
use crate::odb::Odb;
use url::{Host, Url};

/// Returns `true` when `s` is non-empty and starts with `-` (Git `looks_like_command_line_option`).
#[must_use]
pub fn looks_like_command_line_option(s: &str) -> bool {
    !s.is_empty() && s.as_bytes().first() == Some(&b'-')
}

/// True when `name` names a `.gitmodules` file (HFS / NTFS spellings), not a symlink.
#[must_use]
pub fn tree_entry_is_gitmodules_blob(mode: u32, name: &[u8]) -> bool {
    if mode == 0o120000 {
        return false;
    }
    let Ok(name_str) = std::str::from_utf8(name) else {
        return false;
    };
    is_hfs_dot_gitmodules(name_str) || is_ntfs_dot_gitmodules(name_str)
}

/// True for a tree entry name that should be treated as `.gitattributes` for fsck (blob only).
#[must_use]
pub fn tree_entry_is_gitattributes_blob(mode: u32, name: &[u8]) -> bool {
    if mode == 0o120000 {
        return false;
    }
    let Ok(name_str) = std::str::from_utf8(name) else {
        return false;
    };
    is_hfs_dot_gitattributes(name_str) || is_ntfs_dot_gitattributes(name_str)
}

fn is_hfs_or_ntfs_dot_gitmodules(name: &str) -> bool {
    is_hfs_dot_gitmodules(name) || is_ntfs_dot_gitmodules(name)
}

fn is_hfs_or_ntfs_dot_gitattributes(name: &str) -> bool {
    is_hfs_dot_gitattributes(name) || is_ntfs_dot_gitattributes(name)
}

/// True when `name` is an HFS/NTFS `.gitmodules` spelling, including `foo\\.gitmodules`.
#[must_use]
fn tree_entry_name_is_dot_gitmodules(name: &str) -> bool {
    if is_hfs_or_ntfs_dot_gitmodules(name) {
        return true;
    }
    ntfs_backslash_suffix_matches(name, is_ntfs_dot_gitmodules)
}

/// After each `\`, test the remainder with `pred` (Git `fsck_tree` NTFS segment rules).
fn ntfs_backslash_suffix_matches(name: &str, mut pred: impl FnMut(&str) -> bool) -> bool {
    let mut rest = name;
    while let Some(idx) = rest.find('\\') {
        let after = &rest[idx + 1..];
        if pred(after) {
            return true;
        }
        rest = after;
    }
    false
}

/// When `mode` is `120000`, the well-known metadata file name the entry must not symlink to.
///
/// Covers HFS and NTFS spellings of `.gitmodules`, `.gitattributes`, `.gitignore`, and
/// `.mailmap`, matching [`fsck_dot_special_tree_pass`].
#[must_use]
pub fn tree_entry_reserved_dot_symlink(mode: u32, name: &[u8]) -> Option<&'static str> {
    if mode != 0o120000 {
        return None;
    }
    let Ok(name_str) = std::str::from_utf8(name) else {
        return None;
    };
    if tree_entry_name_is_dot_gitmodules(name_str) {
        return Some(".gitmodules");
    }
    if is_hfs_or_ntfs_dot_gitattributes(name_str) {
        return Some(".gitattributes");
    }
    if is_hfs_dot_gitignore(name_str) || is_ntfs_dot_gitignore(name_str) {
        return Some(".gitignore");
    }
    if is_hfs_dot_mailmap(name_str) || is_ntfs_dot_mailmap(name_str) {
        return Some(".mailmap");
    }
    None
}

/// Symlink and registration for one tree (Git `fsck_tree` entry loop).
pub fn fsck_dot_special_tree_pass(
    tree_oid: &ObjectId,
    data: &[u8],
    gitmodules_out: &mut HashSet<ObjectId>,
    gitattributes_out: &mut HashSet<ObjectId>,
) -> Result<Vec<DotFsckIssue>> {
    let entries = parse_tree(data)?;
    let mut issues = Vec::new();
    for TreeEntry { mode, name, oid } in entries {
        let Ok(name_str) = std::str::from_utf8(&name) else {
            continue;
        };
        let is_symlink = mode == 0o120000;

        if tree_entry_name_is_dot_gitmodules(name_str) {
            if is_symlink {
                issues.push(DotFsckIssue::TreeSymlink {
                    tree_oid: *tree_oid,
                    id: "gitmodulesSymlink",
                    detail: ".gitmodules is a symbolic link",
                });
            } else {
                gitmodules_out.insert(oid);
            }
        }

        if is_hfs_or_ntfs_dot_gitattributes(name_str) {
            if is_symlink {
                issues.push(DotFsckIssue::TreeSymlink {
                    tree_oid: *tree_oid,
                    id: "gitattributesSymlink",
                    detail: ".gitattributes is a symlink",
                });
            } else {
                gitattributes_out.insert(oid);
            }
        }

        if is_symlink {
            if is_hfs_dot_gitignore(name_str) || is_ntfs_dot_gitignore(name_str) {
                issues.push(DotFsckIssue::TreeSymlink {
                    tree_oid: *tree_oid,
                    id: "gitignoreSymlink",
                    detail: ".gitignore is a symlink",
                });
            }
            if is_hfs_dot_mailmap(name_str) || is_ntfs_dot_mailmap(name_str) {
                issues.push(DotFsckIssue::TreeSymlink {
                    tree_oid: *tree_oid,
                    id: "mailmapSymlink",
                    detail: ".mailmap is a symlink",
                });
            }
        }
    }
    Ok(issues)
}

/// Problems reported while walking trees / blobs for `.gitmodules` / `.gitattributes` fsck.
#[derive(Debug, Clone)]
pub enum DotFsckIssue {
    /// Returned when tree symlink.
    TreeSymlink {
        /// Tree oid for this error or warning.
        tree_oid: ObjectId,
        /// Id for this error or warning.
        id: &'static str,
        /// Additional human-readable detail for the failure.
        detail: &'static str,
    },
    /// Returned when non blob dot file.
    NonBlobDotFile {
        /// Object id (hex) related to the error.
        oid: ObjectId,
        /// Object type name when known (`commit`, `tree`, etc.).
        kind: ObjectKind,
        /// Id for this error or warning.
        id: &'static str,
        /// Additional human-readable detail for the failure.
        detail: &'static str,
    },
    /// Returned when blob gitmodules.
    BlobGitmodules {
        /// Blob oid for this error or warning.
        blob_oid: ObjectId,
        /// Id for this error or warning.
        id: &'static str,
        /// Additional human-readable detail for the failure.
        detail: String,
    },
    /// Returned when blob gitattributes.
    BlobGitattributes {
        /// Blob oid for this error or warning.
        blob_oid: ObjectId,
        /// Id for this error or warning.
        id: &'static str,
        /// Additional human-readable detail for the failure.
        detail: &'static str,
    },
}

impl DotFsckIssue {
    /// Single-line diagnostic matching `git fsck` (`error in tree` / `warning in blob`, etc.).
    #[must_use]
    pub fn format_line(&self) -> String {
        match self {
            DotFsckIssue::TreeSymlink {
                tree_oid,
                id,
                detail,
            } => {
                let prefix = if *id == "gitmodulesSymlink" {
                    "error"
                } else {
                    "warning"
                };
                format!("{prefix} in tree {}: {}: {}", tree_oid.to_hex(), id, detail)
            }
            DotFsckIssue::NonBlobDotFile {
                oid,
                kind,
                id,
                detail,
            } => format!(
                "error in {} {}: {}: {}",
                kind.as_str(),
                oid.to_hex(),
                id,
                detail
            ),
            DotFsckIssue::BlobGitmodules {
                blob_oid,
                id,
                detail,
            } => {
                let prefix = if *id == "gitmodulesParse" {
                    "warning"
                } else {
                    "error"
                };
                format!("{prefix} in blob {}: {}: {}", blob_oid.to_hex(), id, detail)
            }
            DotFsckIssue::BlobGitattributes {
                blob_oid,
                id,
                detail,
            } => format!("error in blob {}: {}: {}", blob_oid.to_hex(), id, detail),
        }
    }

    /// `true` when this fsck message is fatal by default (Git treats `gitmodulesParse` as INFO).
    #[must_use]
    pub fn is_error_severity(&self) -> bool {
        !matches!(
            self,
            DotFsckIssue::BlobGitmodules {
                id: "gitmodulesParse",
                ..
            } | DotFsckIssue::TreeSymlink {
                id: "gitattributesSymlink" | "gitignoreSymlink" | "mailmapSymlink",
                ..
            }
        )
    }
}

/// True when raw `.gitmodules` bytes cannot be parsed as Git config (Git `git_config_from_mem` failure).
fn gitmodules_blob_unparseable(data: &[u8]) -> bool {
    for raw in data.split(|b| *b == b'\n') {
        let line = trim_bytes(raw);
        if line.is_empty() || line[0] == b'#' || line[0] == b';' {
            continue;
        }
        if line.first() == Some(&b'[') && !line.contains(&b']') {
            return true;
        }
    }
    false
}

fn trim_bytes(mut s: &[u8]) -> &[u8] {
    while let Some((&f, r)) = s.split_first() {
        if f == b' ' || f == b'\t' {
            s = r;
        } else {
            break;
        }
    }
    while let Some((&l, r)) = s.split_last() {
        if l == b' ' || l == b'\t' || l == b'\r' {
            s = r;
        } else {
            break;
        }
    }
    s
}

/// Content checks for OIDs registered as `.gitmodules` / `.gitattributes` targets (Git `fsck_blob` / `fsck_blobs`).
pub fn fsck_dot_special_object(
    oid: &ObjectId,
    kind: ObjectKind,
    data: &[u8],
    gitmodules_oids: &HashSet<ObjectId>,
    gitattributes_oids: &HashSet<ObjectId>,
) -> Vec<DotFsckIssue> {
    let mut out = Vec::new();
    if gitmodules_oids.contains(oid) {
        if kind != ObjectKind::Blob {
            out.push(DotFsckIssue::NonBlobDotFile {
                oid: *oid,
                kind,
                id: "gitmodulesBlob",
                detail: "non-blob found at .gitmodules",
            });
            return out;
        }
        if let Some(msg) = validate_gitmodules_blob_line(data) {
            let (id, detail) = split_fsck_colon(&msg);
            out.push(DotFsckIssue::BlobGitmodules {
                blob_oid: *oid,
                id,
                detail: detail.to_string(),
            });
        } else {
            let text = std::str::from_utf8(data).unwrap_or("");
            let strict_bad =
                ConfigFile::parse(Path::new(".gitmodules"), text, ConfigScope::Local).is_err();
            if strict_bad || gitmodules_blob_unparseable(data) {
                out.push(DotFsckIssue::BlobGitmodules {
                    blob_oid: *oid,
                    id: "gitmodulesParse",
                    detail: "could not parse gitmodules blob".to_string(),
                });
            }
        }
    }
    if gitattributes_oids.contains(oid) {
        if kind != ObjectKind::Blob {
            out.push(DotFsckIssue::NonBlobDotFile {
                oid: *oid,
                kind,
                id: "gitattributesBlob",
                detail: "non-blob found at .gitattributes",
            });
            return out;
        }
        if data.len() > ATTR_MAX_FILE_SIZE {
            out.push(DotFsckIssue::BlobGitattributes {
                blob_oid: *oid,
                id: "gitattributesLarge",
                detail: ".gitattributes too large to parse",
            });
        } else {
            let mut ptr = 0usize;
            while ptr < data.len() {
                let rest = &data[ptr..];
                let line_end = rest.iter().position(|&b| b == b'\n').unwrap_or(rest.len());
                if line_end >= ATTR_MAX_LINE_LENGTH {
                    out.push(DotFsckIssue::BlobGitattributes {
                        blob_oid: *oid,
                        id: "gitattributesLineLength",
                        detail: ".gitattributes has too long lines to parse",
                    });
                    break;
                }
                ptr += line_end;
                if ptr < data.len() && data[ptr] == b'\n' {
                    ptr += 1;
                }
            }
        }
    }
    out
}

/// Tracks `.gitmodules` / `.gitattributes` blob OIDs discovered in trees (Git `fsck_options` oidsets).
#[derive(Debug, Default)]
pub struct DotFsckTracker {
    /// Gitmodules found for this item.
    pub gitmodules_found: HashSet<ObjectId>,
    /// Gitmodules done for this item.
    pub gitmodules_done: HashSet<ObjectId>,
    /// Gitattributes found for this item.
    pub gitattributes_found: HashSet<ObjectId>,
    /// Gitattributes done for this item.
    pub gitattributes_done: HashSet<ObjectId>,
}

impl DotFsckTracker {
    /// Run per-tree registration and symlink checks (`fsck_tree` entry loop).
    pub fn on_tree(&mut self, tree_oid: &ObjectId, data: &[u8]) -> Result<Vec<DotFsckIssue>> {
        fsck_dot_special_tree_pass(
            tree_oid,
            data,
            &mut self.gitmodules_found,
            &mut self.gitattributes_found,
        )
    }

    /// Run per-object blob checks when an OID is validated (`fsck_blob`).
    pub fn on_object(
        &mut self,
        oid: &ObjectId,
        kind: ObjectKind,
        data: &[u8],
    ) -> Vec<DotFsckIssue> {
        let need_gm = self.gitmodules_found.contains(oid) && !self.gitmodules_done.contains(oid);
        let need_ga =
            self.gitattributes_found.contains(oid) && !self.gitattributes_done.contains(oid);
        if !need_gm && !need_ga {
            return Vec::new();
        }
        if need_gm {
            self.gitmodules_done.insert(*oid);
        }
        if need_ga {
            self.gitattributes_done.insert(*oid);
        }
        fsck_dot_special_object(
            oid,
            kind,
            data,
            &self.gitmodules_found,
            &self.gitattributes_found,
        )
    }

    /// Validate any registered blobs that were not reached during the main walk (`fsck_finish` / `fsck_blobs`).
    pub fn finish_pending(&mut self, odb: &Odb) -> Result<Vec<DotFsckIssue>> {
        self.finish_pending_resolve(|oid| odb.read(oid).ok().map(|o| (o.kind, o.data)))
    }

    /// Like [`Self::finish_pending`], but resolves object bytes via `resolve` (e.g. in-memory pack map).
    pub fn finish_pending_resolve<F>(&mut self, mut resolve: F) -> Result<Vec<DotFsckIssue>>
    where
        F: FnMut(&ObjectId) -> Option<(ObjectKind, Vec<u8>)>,
    {
        let mut out = Vec::new();
        let pending_gm: Vec<ObjectId> = self
            .gitmodules_found
            .difference(&self.gitmodules_done)
            .copied()
            .collect();
        let pending_ga: Vec<ObjectId> = self
            .gitattributes_found
            .difference(&self.gitattributes_done)
            .copied()
            .collect();

        for oid in pending_gm {
            self.gitmodules_done.insert(oid);
            let Some((kind, data)) = resolve(&oid) else {
                continue;
            };
            out.extend(fsck_dot_special_object(
                &oid,
                kind,
                &data,
                &self.gitmodules_found,
                &self.gitattributes_found,
            ));
        }
        for oid in pending_ga {
            if self.gitattributes_done.contains(&oid) {
                continue;
            }
            self.gitattributes_done.insert(oid);
            let Some((kind, data)) = resolve(&oid) else {
                continue;
            };
            out.extend(fsck_dot_special_object(
                &oid,
                kind,
                &data,
                &self.gitmodules_found,
                &self.gitattributes_found,
            ));
        }
        Ok(out)
    }
}

fn split_fsck_colon(msg: &str) -> (&'static str, &str) {
    let Some((a, b)) = msg.split_once(": ") else {
        return ("gitmodules", msg);
    };
    match a {
        "gitmodulesName" => ("gitmodulesName", b),
        "gitmodulesUrl" => ("gitmodulesUrl", b),
        "gitmodulesPath" => ("gitmodulesPath", b),
        "gitmodulesUpdate" => ("gitmodulesUpdate", b),
        _ => ("gitmodules", msg),
    }
}

/// Write Git-style warnings for submodule path/url values that look like CLI options.
pub fn write_gitmodules_cli_option_warnings(
    w: &mut dyn Write,
    content: &str,
) -> std::io::Result<()> {
    if let Ok(config) = ConfigFile::parse(Path::new(".gitmodules"), content, ConfigScope::Local) {
        let mut any = false;
        for entry in &config.entries {
            let key = &entry.key;
            let Some(rest) = key.strip_prefix("submodule.") else {
                continue;
            };
            let Some(last_dot) = rest.rfind('.') else {
                continue;
            };
            let var = &rest[last_dot + 1..];
            if var != "path" && var != "url" {
                continue;
            }
            let Some(value) = entry.value.as_deref() else {
                continue;
            };
            if looks_like_command_line_option(value) {
                writeln!(
                    w,
                    "{}",
                    crate::diagnostics::warning_line(&format!(
                        "ignoring '{key}' which may be interpreted as a command-line option: {value}"
                    ))
                )?;
                any = true;
            }
        }
        if any {
            return Ok(());
        }
    }

    // Fallback: raw scan (handles minimal `.gitmodules` that the strict parser rejects).
    let mut subsection: Option<&str> = None;
    for line in content.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with('[') {
            subsection = None;
            if let Some(inner) = trimmed.strip_prefix('[').and_then(|s| s.strip_suffix(']')) {
                let inner = inner.trim();
                if let Some(rest) = inner.strip_prefix("submodule") {
                    let rest = rest.trim();
                    let name = rest
                        .strip_prefix('"')
                        .and_then(|s| s.strip_suffix('"'))
                        .unwrap_or(rest);
                    if !name.is_empty() {
                        subsection = Some(name);
                    }
                }
            }
            continue;
        }
        let Some((raw_key, raw_val)) = trimmed.split_once('=') else {
            continue;
        };
        let key = raw_key.trim();
        if key != "path" && key != "url" {
            continue;
        }
        let mut val = raw_val.trim();
        if val.len() >= 2 && val.starts_with('"') && val.ends_with('"') {
            val = &val[1..val.len() - 1];
        }
        if looks_like_command_line_option(val) {
            let key_full = match subsection {
                Some(name) => format!("submodule.{name}.{key}"),
                None => key.to_string(),
            };
            writeln!(
                w,
                "{}",
                crate::diagnostics::warning_line(&format!(
                    "ignoring '{key_full}' which may be interpreted as a command-line option: {val}"
                ))
            )?;
        }
    }
    Ok(())
}

/// Returns `true` when `name` is allowed as a submodule logical name (Git `check_submodule_name`).
#[must_use]
pub fn check_submodule_name(name: &str) -> bool {
    if name.is_empty() {
        return false;
    }
    let b = name.as_bytes();
    // Git `check_submodule_name`: `goto in_component` before the loop — first component.
    if b.len() >= 2
        && b[0] == b'.'
        && b[1] == b'.'
        && (b.len() == 2 || b[2] == b'/' || b[2] == b'\\')
    {
        return false;
    }
    let mut i = 0usize;
    while i < b.len() {
        let c = b[i];
        i += 1;
        if c == b'/' || c == b'\\' {
            let j = i;
            if b.len() >= j + 2
                && b[j] == b'.'
                && b[j + 1] == b'.'
                && (j + 2 >= b.len() || b[j + 2] == b'/' || b[j + 2] == b'\\')
            {
                return false;
            }
        }
    }
    true
}

fn is_xplatform_dir_sep(b: u8) -> bool {
    b == b'/' || b == b'\\'
}

fn starts_with_dot_dot_slash(url: &str) -> bool {
    let b = url.as_bytes();
    b.len() >= 3 && b[0] == b'.' && b[1] == b'.' && is_xplatform_dir_sep(b[2])
}

fn starts_with_dot_slash(url: &str) -> bool {
    let b = url.as_bytes();
    b.len() >= 2 && b[0] == b'.' && is_xplatform_dir_sep(b[1])
}

fn submodule_url_is_relative(url: &str) -> bool {
    starts_with_dot_slash(url) || starts_with_dot_dot_slash(url)
}

fn hex_val(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

/// Percent-decode `%XX` sequences (Git `url_decode` subset for submodule URL safety checks).
fn percent_decode_git_style(input: &str) -> Option<Vec<u8>> {
    let b = input.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0usize;
    while i < b.len() {
        if b[i] == b'%' {
            if i + 2 >= b.len() {
                return None;
            }
            let hi = hex_val(b[i + 1])?;
            let lo = hex_val(b[i + 2])?;
            out.push((hi << 4) | lo);
            i += 3;
        } else {
            out.push(b[i]);
            i += 1;
        }
    }
    Some(out)
}

/// Git `count_leading_dotdots` / leading `./` stripping (`submodule-config.c`).
fn count_leading_dotdots(url: &str) -> (usize, &str) {
    let mut n = 0usize;
    let mut s = url;
    loop {
        if starts_with_dot_dot_slash(s) {
            n += 1;
            s = &s[3..];
            continue;
        }
        if starts_with_dot_slash(s) {
            s = &s[2..];
            continue;
        }
        break;
    }
    (n, s)
}

fn url_to_curl_transport_url(url: &str) -> Option<&str> {
    url.strip_prefix("http::")
        .or_else(|| url.strip_prefix("https::"))
        .or_else(|| url.strip_prefix("ftp::"))
        .or_else(|| url.strip_prefix("ftps::"))
        .or_else(|| {
            if url.starts_with("http://")
                || url.starts_with("https://")
                || url.starts_with("ftp://")
                || url.starts_with("ftps://")
            {
                Some(url)
            } else {
                None
            }
        })
}

/// Returns `true` when `url` is safe for `.gitmodules` (Git `check_submodule_url`).
#[must_use]
pub fn check_submodule_url(url: &str) -> bool {
    if looks_like_command_line_option(url) {
        return false;
    }

    if submodule_url_is_relative(url) || url.starts_with("git://") {
        let Some(decoded) = percent_decode_git_style(url) else {
            return false;
        };
        if decoded.contains(&b'\n') {
            return false;
        }
        let (n, rest) = count_leading_dotdots(url);
        if n > 0 {
            let rb = rest.as_bytes();
            if !rb.is_empty() && (rb[0] == b':' || rb[0] == b'/') {
                return false;
            }
        }
        return true;
    }

    if let Some(curl_url) = url_to_curl_transport_url(url) {
        if (curl_url.starts_with("http://") || curl_url.starts_with("https://"))
            && curl_url.contains(":///")
        {
            return false;
        }
        let Ok(parsed) = Url::parse(curl_url) else {
            return false;
        };
        if !matches!(
            parsed.scheme(),
            "http" | "https" | "ftp" | "ftps" | "ws" | "wss"
        ) {
            return false;
        }
        if parsed.host_str().is_none() {
            return false;
        }
        match parsed.host() {
            Some(Host::Domain(d)) if d.contains(':') => return false,
            None => return false,
            _ => {}
        }
        if parsed.path().starts_with(':') {
            return false;
        }
        let normalized = parsed.as_str();
        let Some(decoded) = percent_decode_git_style(normalized) else {
            return false;
        };
        !decoded.contains(&b'\n')
    } else {
        true
    }
}

/// Max `.gitattributes` line length checked by Git `fsck` (`attr.h`).
pub const ATTR_MAX_LINE_LENGTH: usize = 2048;

/// Max `.gitattributes` blob size for fsck (`attr.h`).
pub const ATTR_MAX_FILE_SIZE: usize = 100 * 1024 * 1024;

/// `true` when `value` is a command-style submodule update (`!…`), matching Git fsck.
fn submodule_update_is_command(value: &str) -> bool {
    !value.is_empty() && value.starts_with('!')
}

fn raw_gitmodules_submodule_names(content: &str) -> Vec<String> {
    let mut out = Vec::new();
    for line in content.lines() {
        let trimmed = line.trim();
        if !trimmed.starts_with('[') {
            continue;
        }
        let Some(inner) = trimmed.strip_prefix('[').and_then(|s| s.strip_suffix(']')) else {
            continue;
        };
        let inner = inner.trim();
        let Some(rest) = inner.strip_prefix("submodule") else {
            continue;
        };
        let rest = rest.trim();
        let name = rest
            .strip_prefix('"')
            .and_then(|s| s.strip_suffix('"'))
            .unwrap_or(rest);
        if !name.is_empty() {
            out.push(name.to_string());
        }
    }
    out
}

/// Validate a `.gitmodules` blob (Git `fsck_gitmodules_fn`). Returns `object hex: msg` or `None`.
pub fn validate_gitmodules_blob_line(data: &[u8]) -> Option<String> {
    let Ok(text) = std::str::from_utf8(data) else {
        return None;
    };

    let mut worst: Option<String> = None;

    if let Ok(config) = ConfigFile::parse(Path::new(".gitmodules"), text, ConfigScope::Local) {
        for entry in &config.entries {
            let key = &entry.key;
            let Some(rest) = key.strip_prefix("submodule.") else {
                continue;
            };
            let Some(last_dot) = rest.rfind('.') else {
                continue;
            };
            let name = &rest[..last_dot];
            let var = &rest[last_dot + 1..];

            if !check_submodule_name(name) {
                worst.get_or_insert_with(|| {
                    format!("gitmodulesName: disallowed submodule name: {name}")
                });
            }

            let Some(value) = entry.value.as_deref() else {
                continue;
            };

            match var {
                "url" => {
                    if !check_submodule_url(value) {
                        worst.get_or_insert_with(|| {
                            format!("gitmodulesUrl: disallowed submodule url: {value}")
                        });
                    }
                }
                "path" => {
                    if looks_like_command_line_option(value) {
                        worst = Some(format!(
                            "gitmodulesPath: disallowed submodule path: {value}"
                        ));
                    }
                }
                "update" if submodule_update_is_command(value) => {
                    worst.get_or_insert_with(|| {
                        format!("gitmodulesUpdate: disallowed submodule update setting: {value}")
                    });
                }
                _ => {}
            }
        }
    }

    // Submodule subsection names can contain `..` and still parse as config lines, but our
    // canonical key builder rejects those keys — so entries for malicious names are dropped
    // silently. Always cross-check raw `[submodule "..."]` headers (Git fsck does this via the
    // real config parser + `check_submodule_name`).
    for name in raw_gitmodules_submodule_names(text) {
        if !check_submodule_name(&name) {
            worst.get_or_insert_with(|| {
                format!("gitmodulesName: disallowed submodule name: {name}")
            });
        }
    }

    worst
}

fn collect_gitmodules_blobs_from_tree(
    odb: &Odb,
    tree_oid: ObjectId,
    seen_trees: &mut HashSet<ObjectId>,
) -> Result<HashSet<ObjectId>> {
    let mut blobs = HashSet::new();
    let mut stack = vec![tree_oid];
    while let Some(tid) = stack.pop() {
        if !seen_trees.insert(tid) {
            continue;
        }
        let obj = odb.read(&tid)?;
        if obj.kind != ObjectKind::Tree {
            continue;
        }
        let entries = parse_tree(&obj.data)?;
        for TreeEntry { mode, name, oid } in entries {
            if tree_entry_is_gitmodules_blob(mode, &name) {
                blobs.insert(oid);
            } else if mode == 0o040000 {
                stack.push(oid);
            }
        }
    }
    Ok(blobs)
}

/// Validate every `.gitmodules` blob reachable from `commit_oid`. Returns `Some(hex: msg)` on error.
pub fn verify_gitmodules_for_commit(odb: &Odb, commit_oid: ObjectId) -> Result<Option<String>> {
    let obj = odb.read(&commit_oid)?;
    if obj.kind != ObjectKind::Commit {
        return Ok(None);
    }
    let commit = parse_commit(&obj.data)?;
    let mut seen_trees = HashSet::new();
    let blobs = collect_gitmodules_blobs_from_tree(odb, commit.tree, &mut seen_trees)?;
    for oid in blobs {
        let blob = odb.read(&oid)?;
        if blob.kind != ObjectKind::Blob {
            continue;
        }
        if let Some(msg) = validate_gitmodules_blob_line(&blob.data) {
            return Ok(Some(format!("{}: {}", oid.to_hex(), msg)));
        }
    }
    Ok(None)
}

/// Parse `objects/ab/cdef…` loose paths into OIDs; for `.idx` files load all contained OIDs.
pub fn oids_from_copied_object_paths(copied: &[PathBuf]) -> Result<HashSet<ObjectId>> {
    let mut out = HashSet::new();
    for p in copied {
        let Some(name) = p.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        if name.ends_with(".idx") {
            out.extend(PackedObjects::object_ids_from_index_path(p)?);
            continue;
        }
        if let Some(oid) = object_id_from_loose_object_path(p) {
            out.insert(oid);
        }
    }
    Ok(out)
}

fn object_id_from_loose_object_path(path: &Path) -> Option<ObjectId> {
    let file_name = path.file_name()?.to_str()?;
    if file_name.len() != 38 {
        return None;
    }
    let parent = path.parent()?.file_name()?.to_str()?;
    if parent.len() != 2 {
        return None;
    }
    let hex = format!("{parent}{file_name}");
    ObjectId::from_hex(&hex).ok()
}

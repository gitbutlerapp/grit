//! Parse `.mailmap` and resolve author/committer identities (Git-compatible).
//!
//! Behaviour matches Git's `mailmap.c`: load order, `mailmap.blob` default in bare repos,
//! nofollow for in-tree `.mailmap`, and case-insensitive email/name matching with
//! per-email buckets (simple remap vs name-specific entries).

use crate::config::ConfigSet;
use crate::error::Error as GustError;
use crate::objects::ObjectKind;
use crate::repo::Repository;
use crate::rev_parse::resolve_revision;
use std::collections::BTreeMap;
use std::fs;
#[cfg(unix)]
use std::io::Read;
use std::path::{Path, PathBuf};

type Result<T> = std::result::Result<T, GustError>;

/// Legacy line-shaped entry kept for API compatibility; prefer [`MailmapTable`].
#[derive(Debug, Clone)]
pub struct MailmapEntry {
    /// Canonical name (`None` = keep original).
    pub canonical_name: Option<String>,
    /// Canonical email (`None` = keep original).
    pub canonical_email: Option<String>,
    /// Match on this name (`None` = any name with the email).
    pub match_name: Option<String>,
    /// Match on this email.
    pub match_email: String,
}

#[derive(Debug, Default, Clone)]
struct MailmapInfo {
    name: Option<String>,
    email: Option<String>,
}

#[derive(Debug, Default, Clone)]
struct MailmapBucket {
    /// Simple entry: remap any name with this email (`old_name == None` lines).
    simple: MailmapInfo,
    /// Name-specific remaps keyed by lowercased old name.
    by_name: BTreeMap<String, MailmapInfo>,
}

/// Parsed mailmap as a lookup table (Git `string_list` + nested `namemap`).
#[derive(Debug, Default, Clone)]
pub struct MailmapTable {
    /// Key: lowercased match email.
    buckets: BTreeMap<String, MailmapBucket>,
}

impl MailmapTable {
    /// Returns true when no mappings are configured.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.buckets.is_empty()
    }

    /// Apply mailmap to a name/email pair (both may be empty strings).
    ///
    /// Returns `(mapped_name, mapped_email)` after applying the same rules as Git's `map_user`.
    #[must_use]
    pub fn map_user(&self, mut name: String, mut email: String) -> (String, String) {
        let key = email.to_ascii_lowercase();
        let Some(bucket) = self.buckets.get(&key) else {
            return (name, email);
        };

        let info = if !bucket.by_name.is_empty() {
            let nk = name.to_ascii_lowercase();
            bucket.by_name.get(&nk).or_else(|| {
                if bucket.simple.name.is_some() || bucket.simple.email.is_some() {
                    Some(&bucket.simple)
                } else {
                    None
                }
            })
        } else if bucket.simple.name.is_some() || bucket.simple.email.is_some() {
            Some(&bucket.simple)
        } else {
            None
        };

        let Some(info) = info else {
            return (name, email);
        };
        if info.name.is_none() && info.email.is_none() {
            return (name, email);
        }
        if let Some(ref e) = info.email {
            email.clone_from(e);
        }
        if let Some(ref n) = info.name {
            name.clone_from(n);
        }
        (name, email)
    }
}

fn ascii_lowercase_owned(s: &str) -> String {
    s.chars().map(|c| c.to_ascii_lowercase()).collect()
}

fn add_mapping(
    table: &mut MailmapTable,
    new_name: Option<String>,
    new_email: Option<String>,
    old_name: Option<String>,
    old_email: Option<String>,
) {
    // Match Git `add_mapping`: when the line has only one `<email>` pair, `old_email` is NULL and
    // the canonical email is the lookup key (`old_email = new_email; new_email = NULL`).
    let (old_email, new_email) = match (old_email, new_email) {
        (None, Some(e)) => (e, None),
        (Some(old), new) => (old, new),
        (None, None) => return,
    };

    let key = ascii_lowercase_owned(&old_email);
    let bucket = table.buckets.entry(key).or_default();

    if let Some(old_n) = old_name {
        let nk = ascii_lowercase_owned(&old_n);
        let mi = MailmapInfo {
            name: new_name,
            email: new_email,
        };
        bucket.by_name.insert(nk, mi);
    } else {
        if let Some(n) = new_name {
            bucket.simple.name = Some(n);
        }
        if let Some(e) = new_email {
            bucket.simple.email = Some(e);
        }
    }
}

/// Parse `buffer` like Git's `parse_name_and_email` (second pair uses `allow_empty_email`).
fn parse_name_and_email(
    buffer: &str,
    allow_empty_email: bool,
) -> Option<(Option<String>, Option<String>, &str)> {
    let left = buffer.find('<')?;
    let rest = &buffer[left + 1..];
    let right_rel = rest.find('>')?;
    if !allow_empty_email && right_rel == 0 {
        return None;
    }
    // Do not trim inside `<>` — Git keeps spaces as part of the map key so
    // `< a@example.com >` does not match a commit's `a@example.com` (t4203).
    let email = rest[..right_rel].to_string();
    let right = left + 1 + right_rel;
    let name_part = buffer[..left].trim_end_matches(|c: char| c.is_ascii_whitespace());
    let name = if name_part.is_empty() {
        None
    } else {
        Some(name_part.to_string())
    };
    let after = buffer.get(right + 1..).unwrap_or("");
    Some((name, Some(email), after))
}

fn read_mailmap_line_into(table: &mut MailmapTable, line: &str) {
    let line = line.trim_end_matches(['\r', '\n']);
    let line = line.trim_start();
    if line.is_empty() || line.starts_with('#') {
        return;
    }

    // Match Git `read_mailmap_line`: the first pair uses `allow_empty_email=0`, so a line whose
    // first `<>` is empty (e.g. `Cee <> <c@example.com>`) is ignored entirely — only the second
    // pair is parsed with `allow_empty_email=1`.
    let (name1, email1, rest1) = match parse_name_and_email(line, false) {
        Some(x) => x,
        None => return,
    };

    let (name2, email2) = if rest1.trim().is_empty() {
        (None, None)
    } else {
        match parse_name_and_email(rest1.trim_start(), true) {
            Some((n, e, tail)) if tail.trim().is_empty() => (n, e),
            _ => return,
        }
    };

    add_mapping(table, name1, email1, name2, email2);
}

/// Append mappings from a mailmap file body (Git `read_mailmap_string`).
pub fn read_mailmap_string(table: &mut MailmapTable, buf: &str) {
    let mut start = 0usize;
    for (i, ch) in buf.char_indices() {
        if ch == '\n' {
            read_mailmap_line_into(table, &buf[start..i]);
            start = i + 1;
        }
    }
    if start < buf.len() {
        read_mailmap_line_into(table, &buf[start..]);
    }
}

/// Convert a legacy vector of line entries into a table (last-wins per Git order is already in vec order).
#[must_use]
pub fn table_from_entries(entries: &[MailmapEntry]) -> MailmapTable {
    let mut table = MailmapTable::default();
    for e in entries {
        add_mapping(
            &mut table,
            e.canonical_name.clone(),
            e.canonical_email.clone(),
            e.match_name.clone(),
            Some(e.match_email.clone()),
        );
    }
    table
}

/// Parse a `.mailmap` file body into legacy line entries (for compatibility).
#[must_use]
pub fn parse_mailmap(content: &str) -> Vec<MailmapEntry> {
    table_to_entries(&build_mailmap_table_from_str(content))
}

fn build_mailmap_table_from_str(content: &str) -> MailmapTable {
    let mut table = MailmapTable::default();
    read_mailmap_string(&mut table, content);
    table
}

fn table_to_entries(table: &MailmapTable) -> Vec<MailmapEntry> {
    let mut out = Vec::new();
    for (email_lc, bucket) in &table.buckets {
        if bucket.simple.name.is_some() || bucket.simple.email.is_some() {
            out.push(MailmapEntry {
                canonical_name: bucket.simple.name.clone(),
                canonical_email: bucket.simple.email.clone(),
                match_name: None,
                match_email: email_lc.clone(),
            });
        }
        for (name_lc, mi) in &bucket.by_name {
            out.push(MailmapEntry {
                canonical_name: mi.name.clone(),
                canonical_email: mi.email.clone(),
                match_name: Some(name_lc.clone()),
                match_email: email_lc.clone(),
            });
        }
    }
    out
}

/// Parse a contact string `Name <email>` or `<email>`.
#[must_use]
pub fn parse_contact(contact: &str) -> (Option<String>, Option<String>) {
    let contact = contact.trim();
    if let Some(lt) = contact.find('<') {
        if let Some(gt) = contact.find('>') {
            let name = contact[..lt].trim();
            let email = contact[lt + 1..gt].trim();
            return (
                if name.is_empty() {
                    None
                } else {
                    Some(name.to_string())
                },
                if email.is_empty() {
                    None
                } else {
                    Some(email.to_string())
                },
            );
        }
    }
    if contact.contains('@') && !contact.chars().any(char::is_whitespace) {
        return (None, Some(contact.to_string()));
    }

    (Some(contact.to_string()), None)
}

/// Map `(name, email)` through the mailmap; uses [`MailmapTable`] internally.
#[must_use]
pub fn map_contact(
    name: Option<&str>,
    email: Option<&str>,
    mailmap: &[MailmapEntry],
) -> (String, String) {
    let mut table = MailmapTable::default();
    for e in mailmap {
        add_mapping(
            &mut table,
            e.canonical_name.clone(),
            e.canonical_email.clone(),
            e.match_name.clone(),
            Some(e.match_email.clone()),
        );
    }
    let n = name.unwrap_or("").to_string();
    let e = email.unwrap_or("").to_string();
    table.map_user(n, e)
}

/// Map using a pre-built table.
#[must_use]
pub fn map_contact_table(
    name: Option<&str>,
    email: Option<&str>,
    table: &MailmapTable,
) -> (String, String) {
    let n = name.unwrap_or("").to_string();
    let e = email.unwrap_or("").to_string();
    table.map_user(n, e)
}

/// Format a contact for display (`check-mailmap` style).
#[must_use]
pub fn render_contact(name: &str, email: &str) -> String {
    if email.is_empty() {
        return name.to_string();
    }
    if name.is_empty() {
        return format!("<{email}>");
    }
    format!("{name} <{email}>")
}

fn resolve_mailmap_path(base: &Path, value: &str) -> PathBuf {
    let candidate = Path::new(value);
    if candidate.is_absolute() {
        candidate.to_path_buf()
    } else {
        base.join(candidate)
    }
}

fn read_mailmap_file_nofollow(path: &Path) -> Result<String> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;

        // `O_NOFOLLOW` makes `open` fail rather than traverse a final symlink.
        let mut file = fs::OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW)
            .open(path)
            .map_err(|_| {
                GustError::PathError(format!("unable to open mailmap at {}", path.display()))
            })?;
        let mut s = String::new();
        file.read_to_string(&mut s)
            .map_err(|e| GustError::PathError(format!("reading {}: {e}", path.display())))?;
        Ok(s)
    }
    #[cfg(not(unix))]
    {
        fs::read_to_string(path)
            .map_err(|e| GustError::PathError(format!("reading {}: {e}", path.display())))
    }
}

fn read_optional_mailmap_file(path: &Path, nofollow: bool) -> Result<String> {
    if !path.exists() {
        return Ok(String::new());
    }
    if nofollow {
        read_mailmap_file_nofollow(path)
    } else {
        fs::read_to_string(path)
            .map_err(|e| GustError::PathError(format!("reading {}: {e}", path.display())))
    }
}

/// Read mailmap text from a blob revision (for `mailmap.blob` / CLI `--mailmap-blob`).
pub fn read_mailmap_blob(repo: &Repository, spec: &str) -> Result<String> {
    let oid = resolve_revision(repo, spec)
        .map_err(|e| GustError::PathError(format!("resolving mailmap blob '{spec}': {e}")))?;
    let obj = repo
        .odb
        .read(&oid)
        .map_err(|e| GustError::PathError(format!("reading mailmap blob '{spec}': {e}")))?;
    if obj.kind != ObjectKind::Blob {
        return Err(GustError::PathError(format!(
            "mailmap is not a blob: {spec}"
        )));
    }
    Ok(String::from_utf8_lossy(&obj.data).into_owned())
}

fn try_read_mailmap_blob(repo: &Repository, spec: &str) -> Result<Option<String>> {
    let oid = match resolve_revision(repo, spec) {
        Ok(o) => o,
        Err(_) => return Ok(None),
    };
    let obj = repo
        .odb
        .read(&oid)
        .map_err(|e| GustError::PathError(format!("reading mailmap blob '{spec}': {e}")))?;
    if obj.kind != ObjectKind::Blob {
        return Err(GustError::PathError(format!(
            "mailmap is not a blob: {spec}"
        )));
    }
    Ok(Some(String::from_utf8_lossy(&obj.data).into_owned()))
}

/// Load mailmap from the repository using Git's source order and merge rules.
pub fn load_mailmap_table(repo: &Repository) -> Result<MailmapTable> {
    let mut table = MailmapTable::default();
    load_mailmap_into(repo, &mut table)?;
    Ok(table)
}

/// Merge Git's configured mailmap sources into `table`.
pub fn load_mailmap_into(repo: &Repository, table: &mut MailmapTable) -> Result<()> {
    let config = ConfigSet::load(
        &crate::environment::Environment::capture_process(),
        Some(&repo.git_dir),
        true,
    )?;
    let mut mailmap_blob = config.get("mailmap.blob");
    let is_bare = repo.work_tree.is_none();
    if mailmap_blob.is_none() && is_bare {
        mailmap_blob = Some("HEAD:.mailmap".to_string());
    }

    let base_dir = repo
        .work_tree
        .as_deref()
        .unwrap_or(repo.git_dir.as_path())
        .to_path_buf();

    if let Some(ref wt) = repo.work_tree {
        let in_tree = wt.join(".mailmap");
        let body = read_optional_mailmap_file(&in_tree, true)?;
        read_mailmap_string(table, &body);
    }

    if let Some(ref blob) = mailmap_blob {
        match try_read_mailmap_blob(repo, blob) {
            Ok(Some(content)) => read_mailmap_string(table, &content),
            Ok(None) => {}
            Err(e) => {
                // Git's `read_mailmap` ignores the aggregated error from `read_mailmap_blob`, but
                // still emits `error("mailmap is not a blob: ...")` to stderr for wrong object types.
                let msg = e.to_string();
                if msg.contains("mailmap is not a blob") {
                    eprintln!("{msg}");
                } else {
                    return Err(e);
                }
            }
        }
    }

    if let Some(file) = config.get("mailmap.file") {
        read_mailmap_string(
            table,
            &read_optional_mailmap_file(&resolve_mailmap_path(&base_dir, &file), false)?,
        );
    }

    Ok(())
}

/// Concatenated raw mailmap text (legacy); sources joined in Git load order.
pub fn load_mailmap_raw(repo: &Repository) -> Result<String> {
    let config = ConfigSet::load(
        &crate::environment::Environment::capture_process(),
        Some(&repo.git_dir),
        true,
    )?;
    let mut mailmap_blob = config.get("mailmap.blob");
    let is_bare = repo.work_tree.is_none();
    if mailmap_blob.is_none() && is_bare {
        mailmap_blob = Some("HEAD:.mailmap".to_string());
    }

    let base_dir = repo
        .work_tree
        .as_deref()
        .unwrap_or(repo.git_dir.as_path())
        .to_path_buf();

    let mut out = String::new();

    if let Some(ref wt) = repo.work_tree {
        let body = read_optional_mailmap_file(&wt.join(".mailmap"), true)?;
        if !body.is_empty() {
            out.push_str(&body);
            if !out.ends_with('\n') {
                out.push('\n');
            }
        }
    }

    if let Some(ref blob) = mailmap_blob {
        match try_read_mailmap_blob(repo, blob) {
            Ok(Some(content)) => {
                if !content.is_empty() {
                    out.push_str(&content);
                    if !out.ends_with('\n') {
                        out.push('\n');
                    }
                }
            }
            Ok(None) => {}
            Err(e) => {
                let msg = e.to_string();
                if msg.contains("mailmap is not a blob") {
                    eprintln!("{msg}");
                } else {
                    return Err(e);
                }
            }
        }
    }

    if let Some(file) = config.get("mailmap.file") {
        let body = read_optional_mailmap_file(&resolve_mailmap_path(&base_dir, &file), false)?;
        if !body.is_empty() {
            out.push_str(&body);
            if !out.ends_with('\n') {
                out.push('\n');
            }
        }
    }

    Ok(out)
}

/// Parsed mailmap for the repository (default `.mailmap` + config).
pub fn load_mailmap(repo: &Repository) -> Result<Vec<MailmapEntry>> {
    let table = load_mailmap_table(repo)?;
    Ok(table_to_entries(&table))
}

/// Rewrite `author ` / `committer ` / `tagger ` header lines in a commit or tag object buffer.
///
/// Git applies mailmap only to the `Name <email>` prefix; the trailing ` <epoch> <tz>` is preserved.
#[must_use]
pub fn apply_mailmap_to_commit_or_tag_bytes(data: &[u8], mailmap: &MailmapTable) -> Vec<u8> {
    if mailmap.is_empty() {
        return data.to_vec();
    }
    let Some(pos) = data.windows(2).position(|w| w == b"\n\n") else {
        return data.to_vec();
    };
    let (headers, rest) = data.split_at(pos + 1);
    let header_text = String::from_utf8_lossy(headers);
    let mut out = String::with_capacity(data.len() + 64);
    for line in header_text.lines() {
        let rewritten = rewrite_identity_header_line(line, mailmap);
        out.push_str(&rewritten);
        out.push('\n');
    }
    out.push('\n');
    out.push_str(&String::from_utf8_lossy(&rest[1..]));
    out.into_bytes()
}

fn rewrite_identity_header_line(line: &str, mailmap: &MailmapTable) -> String {
    for pref in ["author ", "committer ", "tagger "] {
        if let Some(rest) = line.strip_prefix(pref) {
            let rest = rest.trim_end_matches('\r');
            let Some(gt) = rest.rfind('>') else {
                return line.to_string();
            };
            let ident = &rest[..=gt];
            let tail = rest[gt + 1..].trim_start();
            let (name, email) = parse_contact(ident);
            let (n, e) = map_contact_table(name.as_deref(), email.as_deref(), mailmap);
            let new_ident = render_contact(&n, &e);
            if tail.is_empty() {
                return format!("{pref}{new_ident}");
            }
            return format!("{pref}{new_ident} {tail}");
        }
    }
    line.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn name_entry_after_email_merges() {
        let mut t = MailmapTable::default();
        read_mailmap_string(
            &mut t,
            "<bugs@company.xy> <bugs@company.xx>\nInternal Guy <bugs@company.xx>\n",
        );
        let (n, e) = t.map_user("nick1".into(), "bugs@company.xx".into());
        assert_eq!(n, "Internal Guy");
        assert_eq!(e, "bugs@company.xy");
    }

    #[test]
    fn single_pair_line_maps_name_only() {
        let mut t = MailmapTable::default();
        read_mailmap_string(&mut t, "Committed <committer@example.com>\n");
        let (n, e) = t.map_user("C O Mitter".into(), "committer@example.com".into());
        assert_eq!(n, "Committed");
        assert_eq!(e, "committer@example.com");
    }

    #[test]
    fn whitespace_inside_angle_brackets_is_part_of_map_key() {
        let mut t = MailmapTable::default();
        read_mailmap_string(&mut t, "Ah <ah@example.com> < a@example.com >\n");
        let (n, e) = t.map_user("A".into(), "a@example.com".into());
        assert_eq!(n, "A");
        assert_eq!(e, "a@example.com");
        let (n2, e2) = t.map_user("A".into(), " a@example.com ".into());
        assert_eq!(n2, "Ah");
        assert_eq!(e2, "ah@example.com");
    }
}

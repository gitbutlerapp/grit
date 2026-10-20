//! Git-compatible `log -p` / `show` patch output streamed to a [`Write`](std::io::Write).

use std::collections::HashMap;
use std::io::Write;

use crate::attributes::load_gitattributes_for_check_attr_source;
use crate::commit_pretty::{
    abbrev_hex, approximate_object_count, auto_abbrev_len_for_object_count,
};
use crate::config::ConfigSet;
use crate::diff::options::DiffOptions;
use crate::error::Result;
use crate::merge_file;
use crate::objects::CommitData;
use crate::objects::{parse_commit, ObjectId, ObjectKind};
use crate::odb::Odb;
use crate::oid_abbrev_cache::OidAbbrevCache;
use crate::quote_path::format_diff_path_with_prefix;
use crate::repo::Repository;
use crate::userdiff::{matcher_for_path_parsed, FuncnameMatcher};

use super::{diff_trees, empty_blob_oid, zero_oid, DiffEntry, DiffStatus};

/// How much commit metadata precedes each patch in [`write_commit_log_patch`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum LogPatchCommitHeader {
    /// `git log -p` default (commit line, author, date, indented message, then patch).
    #[default]
    Full,
    /// `git log -p --format=%H` (full oid line only, then patch).
    FormatPercentH,
}

/// Options for [`write_commit_log_patch`].
#[derive(Debug, Clone)]
pub struct LogPatchOptions {
    /// Unified diff hunk settings.
    pub diff: DiffOptions,
    /// Minimum abbreviated oid length (`core.abbrev`, default 7).
    pub abbrev_len: usize,
    /// Commit header shape before patches.
    pub commit_header: LogPatchCommitHeader,
}

impl LogPatchOptions {
    /// Defaults matching `git log -p` on a typical repository.
    #[must_use]
    pub fn git_default(config: &ConfigSet) -> Self {
        Self {
            diff: DiffOptions::from_config(config),
            abbrev_len: 7,
            commit_header: LogPatchCommitHeader::Full,
        }
    }
}

fn resolve_core_abbrev_len(config: &ConfigSet, odb: &Odb) -> Result<usize> {
    if let Some(v) = config.get("core.abbrev") {
        if v.eq_ignore_ascii_case("auto") {
            let count = approximate_object_count(odb)?;
            return Ok(auto_abbrev_len_for_object_count(count));
        }
        if let Ok(n) = v.parse::<i64>() {
            return Ok((n as usize).clamp(4, 40));
        }
    }
    let count = approximate_object_count(odb)?;
    Ok(auto_abbrev_len_for_object_count(count))
}

fn effective_log_patch_options(
    repo: &Repository,
    opts: &LogPatchOptions,
) -> Result<LogPatchOptions> {
    let config = repo.config()?.clone();
    let mut out = opts.clone();
    out.abbrev_len = resolve_core_abbrev_len(&config, repo.odb())?;
    Ok(out)
}

fn log_patch_funcname_matcher(
    repo: &Repository,
    config: &ConfigSet,
    tree_oid: &ObjectId,
    rel_path: &str,
) -> Result<Option<FuncnameMatcher>> {
    let parsed = load_gitattributes_for_check_attr_source(repo, tree_oid)?;
    matcher_for_path_parsed(config, &parsed.rules, &parsed.macros, rel_path, false)
        .map_err(crate::error::Error::Message)
}

/// Write one commit in `git log -p --no-color --format=%H` shape (no patch for merges).
///
/// # Errors
///
/// Propagates ODB read, tree diff, and I/O errors.
pub fn write_commit_log_patch<W: Write>(
    repo: &Repository,
    out: &mut W,
    oid: &ObjectId,
    opts: &LogPatchOptions,
) -> Result<()> {
    let odb = repo.odb();
    let raw = odb.read(oid)?;
    let commit = parse_commit(&raw.data)?;
    let parent_tree = parent_tree_for_commit(odb, &commit, None)?;
    let opts = effective_log_patch_options(repo, opts)?;
    let mut abbrev_cache = OidAbbrevCache::new(opts.abbrev_len);
    write_parsed_commit_log_patch(
        out,
        repo,
        odb,
        oid,
        &commit,
        parent_tree,
        &opts,
        &mut abbrev_cache,
    )
}

/// Like [`write_commit_log_patch`] for many commits (avoids re-reading parent commits).
///
/// # Errors
///
/// Propagates ODB read, tree diff, and I/O errors.
pub fn write_commits_log_patch<W: Write>(
    repo: &Repository,
    out: &mut W,
    oids: &[ObjectId],
    opts: &LogPatchOptions,
) -> Result<()> {
    if oids.is_empty() {
        return Ok(());
    }
    let odb = repo.odb();
    let mut trees_by_commit: HashMap<ObjectId, ObjectId> = HashMap::with_capacity(oids.len());
    let mut parsed: Vec<(ObjectId, CommitData)> = Vec::with_capacity(oids.len());
    for oid in oids {
        let raw = odb.read(oid)?;
        let commit = parse_commit(&raw.data)?;
        trees_by_commit.insert(*oid, commit.tree);
        parsed.push((*oid, commit));
    }
    let opts = effective_log_patch_options(repo, opts)?;
    let mut abbrev_cache = OidAbbrevCache::new(opts.abbrev_len);
    for (oid, commit) in &parsed {
        let parent_tree = parent_tree_for_commit(odb, commit, Some(&trees_by_commit))?;
        write_parsed_commit_log_patch(
            out,
            repo,
            odb,
            oid,
            commit,
            parent_tree,
            &opts,
            &mut abbrev_cache,
        )?;
    }
    Ok(())
}

fn parent_tree_for_commit(
    odb: &Odb,
    commit: &CommitData,
    trees_by_commit: Option<&HashMap<ObjectId, ObjectId>>,
) -> Result<Option<ObjectId>> {
    if commit.parents.is_empty() {
        return Ok(None);
    }
    let parent = commit.parents[0];
    if let Some(map) = trees_by_commit {
        if let Some(tree) = map.get(&parent) {
            return Ok(Some(*tree));
        }
    }
    let parent_raw = odb.read(&parent)?;
    Ok(Some(parse_commit(&parent_raw.data)?.tree))
}

fn abbrev_for_index_line(
    cache: &mut OidAbbrevCache,
    repo: &Repository,
    oid: &ObjectId,
    min_len: usize,
) -> Result<String> {
    if *oid == zero_oid() {
        return Ok(abbrev_hex(oid, min_len));
    }
    cache.abbrev(repo, oid)
}

fn abbrev_index_pair(
    cache: &mut OidAbbrevCache,
    repo: &Repository,
    old_oid: &ObjectId,
    new_oid: &ObjectId,
    min_len: usize,
) -> Result<(String, String)> {
    let old = abbrev_for_index_line(cache, repo, old_oid, min_len)?;
    let new = abbrev_for_index_line(cache, repo, new_oid, min_len)?;
    let len = old.len().max(new.len());
    let pad = |oid: &ObjectId, abbrev: String| {
        if abbrev.len() >= len {
            return abbrev;
        }
        let hex = oid.to_hex();
        hex[..len.min(hex.len())].to_string()
    };
    Ok((pad(old_oid, old), pad(new_oid, new)))
}

#[allow(clippy::too_many_arguments)]
fn write_parsed_commit_log_patch<W: Write>(
    out: &mut W,
    repo: &Repository,
    odb: &Odb,
    oid: &ObjectId,
    commit: &CommitData,
    parent_tree: Option<ObjectId>,
    opts: &LogPatchOptions,
    abbrev_cache: &mut OidAbbrevCache,
) -> Result<()> {
    match opts.commit_header {
        LogPatchCommitHeader::Full => {
            writeln!(out, "commit {oid}")?;
            if commit.parents.len() > 1 {
                let mut parts = Vec::with_capacity(commit.parents.len());
                for parent in &commit.parents {
                    parts.push(abbrev_for_index_line(
                        abbrev_cache,
                        repo,
                        parent,
                        opts.abbrev_len,
                    )?);
                }
                writeln!(out, "Merge: {}", parts.join(" "))?;
            }
            write_commit_header_and_message(out, &commit.author, &commit.message)?;
            if commit.parents.len() > 1 {
                return Ok(());
            }
            writeln!(out)?;
        }
        LogPatchCommitHeader::FormatPercentH => {
            writeln!(out, "{oid}")?;
            if commit.parents.len() > 1 {
                return Ok(());
            }
            writeln!(out)?;
        }
    }
    let entries = diff_trees(odb, parent_tree.as_ref(), Some(&commit.tree), "")?;
    for entry in entries {
        write_diff_entry_log_patch_with_tree(
            out,
            repo,
            odb,
            &entry,
            opts,
            abbrev_cache,
            Some(&commit.tree),
        )?;
    }
    Ok(())
}

/// Write commit header (author, date, indented message) after the `commit` line.
pub fn write_commit_header_and_message<W: Write>(
    out: &mut W,
    author_ident: &str,
    message: &str,
) -> Result<()> {
    writeln!(out, "Author: {}", format_ident_display(author_ident))?;
    writeln!(out, "Date:   {}", format_log_date(author_ident))?;
    writeln!(out)?;
    for line in message.lines() {
        writeln!(out, "    {line}")?;
    }
    Ok(())
}

fn format_ident_display(ident: &str) -> String {
    let name = ident.find('<').map_or(ident.trim(), |i| ident[..i].trim());
    let email = ident
        .find('<')
        .and_then(|start| ident.find('>').map(|end| &ident[start..=end]))
        .unwrap_or("");
    format!("{name} {email}")
}

fn format_log_date(ident: &str) -> String {
    let parts: Vec<&str> = ident.rsplitn(3, ' ').collect();
    if parts.len() < 2 {
        return String::new();
    }
    let ts_str = parts[1];
    let offset_str = parts[0];
    let Ok(ts) = ts_str.parse::<i64>() else {
        return format!("{ts_str} {offset_str}");
    };
    let tz_bytes = offset_str.as_bytes();
    let tz_secs: i64 = if tz_bytes.len() >= 5 {
        let sign = if tz_bytes[0] == b'-' { -1 } else { 1 };
        let h: i64 = offset_str[1..3].parse().unwrap_or(0);
        let m: i64 = offset_str[3..5].parse().unwrap_or(0);
        sign * (h * 3600 + m * 60)
    } else {
        0
    };
    let adjusted = ts + tz_secs;
    let dt = time::OffsetDateTime::from_unix_timestamp(adjusted)
        .unwrap_or(time::OffsetDateTime::UNIX_EPOCH);
    let weekday = match dt.weekday() {
        time::Weekday::Monday => "Mon",
        time::Weekday::Tuesday => "Tue",
        time::Weekday::Wednesday => "Wed",
        time::Weekday::Thursday => "Thu",
        time::Weekday::Friday => "Fri",
        time::Weekday::Saturday => "Sat",
        time::Weekday::Sunday => "Sun",
    };
    let month = match dt.month() {
        time::Month::January => "Jan",
        time::Month::February => "Feb",
        time::Month::March => "Mar",
        time::Month::April => "Apr",
        time::Month::May => "May",
        time::Month::June => "Jun",
        time::Month::July => "Jul",
        time::Month::August => "Aug",
        time::Month::September => "Sep",
        time::Month::October => "Oct",
        time::Month::November => "Nov",
        time::Month::December => "Dec",
    };
    format!(
        "{weekday} {month} {} {:02}:{:02}:{:02} {} {offset_str}",
        dt.day(),
        dt.hour(),
        dt.minute(),
        dt.second(),
        dt.year()
    )
}

fn log_patch_parse_mode_u32(mode: &str) -> u32 {
    u32::from_str_radix(mode.trim(), 8).unwrap_or(0)
}

/// Write the `diff --git` header and unified body for one [`DiffEntry`].
///
/// # Errors
///
/// Propagates ODB reads and I/O errors.
pub fn write_diff_entry_log_patch<W: Write>(
    out: &mut W,
    repo: &Repository,
    odb: &Odb,
    entry: &DiffEntry,
    opts: &LogPatchOptions,
) -> Result<()> {
    let mut abbrev_cache = OidAbbrevCache::new(opts.abbrev_len);
    write_diff_entry_log_patch_with_tree(out, repo, odb, entry, opts, &mut abbrev_cache, None)
}

/// Write unified patch bodies for many tree diff entries (no commit header).
///
/// `new_tree` enables `-W` / userdiff function headers on the new side.
///
/// # Errors
///
/// Propagates ODB reads and I/O errors.
pub fn write_tree_diff_log_patch<W: Write + ?Sized>(
    repo: &Repository,
    out: &mut W,
    entries: &[DiffEntry],
    new_tree: Option<&ObjectId>,
    opts: &LogPatchOptions,
) -> Result<()> {
    let odb = repo.odb();
    let config = repo.config()?.clone();
    let mut abbrev_cache = OidAbbrevCache::new(opts.abbrev_len);
    for entry in entries {
        write_diff_entry_log_patch_with_tree_cached_config(
            out,
            repo,
            odb,
            entry,
            opts,
            &config,
            &mut abbrev_cache,
            new_tree,
        )?;
    }
    Ok(())
}

fn write_diff_entry_log_patch_with_tree<W: Write + ?Sized>(
    out: &mut W,
    repo: &Repository,
    odb: &Odb,
    entry: &DiffEntry,
    opts: &LogPatchOptions,
    abbrev_cache: &mut OidAbbrevCache,
    new_tree: Option<&ObjectId>,
) -> Result<()> {
    let config = repo.config()?.clone();
    write_diff_entry_log_patch_with_tree_cached_config(
        out,
        repo,
        odb,
        entry,
        opts,
        &config,
        abbrev_cache,
        new_tree,
    )
}

#[allow(clippy::too_many_arguments)]
fn write_diff_entry_log_patch_with_tree_cached_config<W: Write + ?Sized>(
    out: &mut W,
    repo: &Repository,
    odb: &Odb,
    entry: &DiffEntry,
    opts: &LogPatchOptions,
    config: &ConfigSet,
    abbrev_cache: &mut OidAbbrevCache,
    new_tree: Option<&ObjectId>,
) -> Result<()> {
    let git_old = entry
        .old_path
        .as_deref()
        .or(entry.new_path.as_deref())
        .unwrap_or("dev/null");
    let git_new = entry
        .new_path
        .as_deref()
        .or(entry.old_path.as_deref())
        .unwrap_or("dev/null");
    const QUOTE_FULLY: bool = true;
    writeln!(
        out,
        "diff --git {} {}",
        format_diff_path_with_prefix("a/", git_old, QUOTE_FULLY),
        format_diff_path_with_prefix("b/", git_new, QUOTE_FULLY)
    )?;

    let old_mode = log_patch_parse_mode_u32(&entry.old_mode);
    let new_mode = log_patch_parse_mode_u32(&entry.new_mode);

    if old_mode == 0 {
        writeln!(out, "new file mode {}", entry.new_mode.trim())?;
    } else if new_mode == 0 {
        writeln!(out, "deleted file mode {}", entry.old_mode.trim())?;
    }

    match entry.status {
        DiffStatus::Renamed => {
            writeln!(out, "similarity index {}%", entry.score.unwrap_or(100))?;
            if let Some(path) = &entry.old_path {
                writeln!(out, "rename from {path}")?;
            }
            if let Some(path) = &entry.new_path {
                writeln!(out, "rename to {path}")?;
            }
        }
        DiffStatus::Copied => {
            writeln!(out, "similarity index {}%", entry.score.unwrap_or(100))?;
            if let Some(path) = &entry.old_path {
                writeln!(out, "copy from {path}")?;
            }
            if let Some(path) = &entry.new_path {
                writeln!(out, "copy to {path}")?;
            }
        }
        _ if old_mode != 0 && new_mode != 0 && old_mode != new_mode => {
            writeln!(out, "old mode {}", entry.old_mode.trim())?;
            writeln!(out, "new mode {}", entry.new_mode.trim())?;
        }
        _ => {}
    }

    if entry.old_oid != entry.new_oid {
        let index_mode = if new_mode != 0 {
            entry.new_mode.trim()
        } else {
            entry.old_mode.trim()
        };
        match entry.status {
            DiffStatus::Added => {
                let (old_a, new_a) = abbrev_index_pair(
                    abbrev_cache,
                    repo,
                    &zero_oid(),
                    &entry.new_oid,
                    opts.abbrev_len,
                )?;
                writeln!(out, "index {old_a}..{new_a}")?;
            }
            DiffStatus::Deleted => {
                let (old_a, new_a) = abbrev_index_pair(
                    abbrev_cache,
                    repo,
                    &entry.old_oid,
                    &zero_oid(),
                    opts.abbrev_len,
                )?;
                writeln!(out, "index {old_a}..{new_a}")?;
            }
            DiffStatus::Renamed | DiffStatus::Copied => {
                let (old_a, new_a) = abbrev_index_pair(
                    abbrev_cache,
                    repo,
                    &entry.old_oid,
                    &entry.new_oid,
                    opts.abbrev_len,
                )?;
                writeln!(out, "index {old_a}..{new_a}")?;
            }
            _ => {
                let (old_a, new_a) = abbrev_index_pair(
                    abbrev_cache,
                    repo,
                    &entry.old_oid,
                    &entry.new_oid,
                    opts.abbrev_len,
                )?;
                writeln!(out, "index {old_a}..{new_a} {index_mode}")?;
            }
        }
    }

    let old_bytes = read_blob_side(odb, entry.old_oid, old_mode == 0)?;
    let new_bytes = read_blob_side(odb, entry.new_oid, new_mode == 0)?;
    if old_bytes == new_bytes {
        return Ok(());
    }

    if merge_file::is_binary(&old_bytes) || merge_file::is_binary(&new_bytes) {
        let old_side = if old_mode == 0 {
            "/dev/null".to_owned()
        } else {
            format_diff_path_with_prefix("a/", git_old, QUOTE_FULLY)
        };
        let new_side = if new_mode == 0 {
            "/dev/null".to_owned()
        } else {
            format_diff_path_with_prefix("b/", git_new, QUOTE_FULLY)
        };
        writeln!(out, "Binary files {old_side} and {new_side} differ")?;
        return Ok(());
    }

    let byte_oriented = !crate::commit_encoding::is_strict_utf8(&old_bytes)
        || !crate::commit_encoding::is_strict_utf8(&new_bytes);
    let (old_text, new_text) = if byte_oriented {
        (
            crate::commit_encoding::decode_latin1(&old_bytes),
            crate::commit_encoding::decode_latin1(&new_bytes),
        )
    } else {
        let old_s = std::str::from_utf8(&old_bytes)
            .map_err(|_| crate::error::Error::CorruptObject("blob is not valid UTF-8".into()))?;
        let new_s = std::str::from_utf8(&new_bytes)
            .map_err(|_| crate::error::Error::CorruptObject("blob is not valid UTF-8".into()))?;
        (old_s.to_owned(), new_s.to_owned())
    };
    let old_path = if old_mode == 0 {
        "/dev/null".to_owned()
    } else {
        git_old.to_owned()
    };
    let new_path = if new_mode == 0 {
        "/dev/null".to_owned()
    } else {
        git_new.to_owned()
    };
    let d = &opts.diff;
    let funcname_matcher = if let Some(tree) = new_tree {
        log_patch_funcname_matcher(repo, config, tree, git_new)?
    } else {
        None
    };
    let funcname_ref = funcname_matcher.as_ref();
    if byte_oriented {
        let mut patch_buf = Vec::new();
        super::write_unified_diff_from_diff_options(
            &mut patch_buf,
            &old_text,
            &new_text,
            &old_path,
            &new_path,
            "a/",
            "b/",
            funcname_ref,
            d,
            QUOTE_FULLY,
        )?;
        let patch_str = std::str::from_utf8(&patch_buf)
            .map_err(|_| crate::error::Error::CorruptObject("patch re-encoding failed".into()))?;
        out.write_all(&crate::commit_encoding::encode_latin1_lossy(patch_str))?;
    } else {
        super::write_unified_diff_from_diff_options(
            out,
            &old_text,
            &new_text,
            &old_path,
            &new_path,
            "a/",
            "b/",
            funcname_ref,
            d,
            QUOTE_FULLY,
        )?;
    }
    Ok(())
}

fn read_blob_side(odb: &Odb, oid: ObjectId, is_missing: bool) -> Result<Vec<u8>> {
    if is_missing || oid == zero_oid() || oid == empty_blob_oid() {
        return Ok(Vec::new());
    }
    let obj = odb.read(&oid)?;
    if obj.kind != ObjectKind::Blob {
        return Ok(Vec::new());
    }
    Ok(obj.data)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    use crate::hash::hash_object;
    use crate::objects::HashAlgo;

    #[test]
    fn copied_diff_emits_copy_headers() -> Result<()> {
        let blob_oid = hash_object(HashAlgo::Sha1, ObjectKind::Blob, b"x");
        let entry = DiffEntry {
            status: DiffStatus::Copied,
            old_path: Some("src/a.txt".into()),
            new_path: Some("dst/b.txt".into()),
            old_oid: blob_oid,
            new_oid: blob_oid,
            old_mode: "100644".into(),
            new_mode: "100644".into(),
            score: Some(100),
        };
        let dir = tempfile::tempdir().expect("tempdir");
        let repo = Repository::init(dir.path(), &crate::repo::InitOptions::default())?;
        repo.odb().write(ObjectKind::Blob, b"x")?;
        let mut out = Cursor::new(Vec::new());
        let opts = LogPatchOptions::git_default(&ConfigSet::default());
        write_diff_entry_log_patch(&mut out, &repo, repo.odb(), &entry, &opts)?;
        let text = String::from_utf8(out.into_inner()).expect("utf-8");
        assert!(text.contains("copy from src/a.txt"));
        assert!(text.contains("copy to dst/b.txt"));
        assert!(!text.contains("rename from"));
        Ok(())
    }
}

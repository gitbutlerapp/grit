//! Git-compatible `log -p` / `show` patch output streamed to a [`Write`](std::io::Write).

use std::io::Write;

use crate::blame::{parse_diff_algorithm_name, BlameDiffAlgorithm};
use crate::commit_pretty::abbrev_hex;
use crate::config::ConfigSet;
use crate::error::Result;
use crate::merge_file;
use crate::objects::{parse_commit, ObjectId, ObjectKind};
use crate::odb::Odb;
use crate::repo::Repository;
use crate::rev_parse::abbreviate_object_id;

use super::{
    diff_trees, empty_blob_oid, write_unified_diff_with_prefix_and_funcname_and_algorithm,
    zero_oid, DiffEntry, DiffStatus,
};

/// Line diff settings for [`write_commit_log_patch`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LogPatchDiffOptions {
    /// Context lines (`-U`, default 3).
    pub context_lines: usize,
    /// `--inter-hunk-context` (default 0).
    pub inter_hunk_context: usize,
    /// Line diff algorithm (Myers, patience, …).
    pub algorithm: similar::Algorithm,
    /// Use Git's histogram engine (`diff.algorithm = histogram`).
    pub use_git_histogram: bool,
    /// Apply Git's indent heuristic when generating hunks.
    pub indent_heuristic: bool,
}

impl Default for LogPatchDiffOptions {
    fn default() -> Self {
        Self {
            context_lines: 3,
            inter_hunk_context: 0,
            algorithm: similar::Algorithm::Myers,
            use_git_histogram: false,
            indent_heuristic: false,
        }
    }
}

impl LogPatchDiffOptions {
    /// Resolve options from repository config (`diff.algorithm`, indent heuristic).
    #[must_use]
    pub fn from_config(config: &ConfigSet) -> Self {
        let mut opts = Self::default();
        if let Some(name) = config.get("diff.algorithm") {
            if let Some(parsed) = parse_diff_algorithm_name(&name) {
                match parsed {
                    BlameDiffAlgorithm::Histogram => {
                        opts.use_git_histogram = true;
                        opts.algorithm = similar::Algorithm::Myers;
                    }
                    BlameDiffAlgorithm::Patience => {
                        opts.algorithm = similar::Algorithm::Patience;
                    }
                    BlameDiffAlgorithm::Minimal => {
                        opts.algorithm = similar::Algorithm::Lcs;
                    }
                    BlameDiffAlgorithm::Myers => {}
                }
            }
        }
        opts.indent_heuristic = super::resolve_indent_heuristic(config, false, false);
        opts
    }
}

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
    pub diff: LogPatchDiffOptions,
    /// Minimum abbreviated oid length (`core.abbrev`, default 7).
    pub abbrev_len: usize,
    /// Commit header shape before patches.
    pub commit_header: LogPatchCommitHeader,
}

impl LogPatchOptions {
    /// Defaults matching `git log -p` on a typical repository.
    #[must_use]
    pub fn git_default(config: &ConfigSet) -> Self {
        let abbrev_len = config
            .get("core.abbrev")
            .and_then(|v| v.parse::<i64>().ok())
            .map(|n| n as usize)
            .unwrap_or(7)
            .clamp(4, 40);
        Self {
            diff: LogPatchDiffOptions::from_config(config),
            abbrev_len,
            commit_header: LogPatchCommitHeader::Full,
        }
    }
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
    match opts.commit_header {
        LogPatchCommitHeader::Full => {
            writeln!(out, "commit {oid}")?;
            if commit.parents.len() > 1 {
                let mut parts = Vec::with_capacity(commit.parents.len());
                for parent in &commit.parents {
                    parts.push(abbreviate_object_id(repo, *parent, opts.abbrev_len)?);
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
    let parent_tree = if commit.parents.is_empty() {
        None
    } else {
        let parent_raw = odb.read(&commit.parents[0])?;
        Some(parse_commit(&parent_raw.data)?.tree)
    };
    let entries = diff_trees(odb, parent_tree.as_ref(), Some(&commit.tree), "")?;
    for entry in entries {
        write_diff_entry_log_patch(out, repo, odb, &entry, opts)?;
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
    writeln!(out, "diff --git a/{git_old} b/{git_new}")?;

    let old_mode = log_patch_parse_mode_u32(&entry.old_mode);
    let new_mode = log_patch_parse_mode_u32(&entry.new_mode);

    if old_mode == 0 {
        writeln!(out, "new file mode {}", entry.new_mode.trim())?;
    } else if new_mode == 0 {
        writeln!(out, "deleted file mode {}", entry.old_mode.trim())?;
    }

    match entry.status {
        DiffStatus::Renamed | DiffStatus::Copied => {
            writeln!(out, "similarity index {}%", entry.score.unwrap_or(100))?;
            if let Some(path) = &entry.old_path {
                writeln!(out, "rename from {path}")?;
            }
            if let Some(path) = &entry.new_path {
                writeln!(out, "rename to {path}")?;
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
                writeln!(
                    out,
                    "index {}..{}",
                    abbrev_hex(&zero_oid(), opts.abbrev_len),
                    abbreviate_object_id(repo, entry.new_oid, opts.abbrev_len)?
                )?;
            }
            DiffStatus::Deleted => {
                writeln!(
                    out,
                    "index {}..{}",
                    abbreviate_object_id(repo, entry.old_oid, opts.abbrev_len)?,
                    abbrev_hex(&zero_oid(), opts.abbrev_len)
                )?;
            }
            DiffStatus::Renamed | DiffStatus::Copied => {
                writeln!(
                    out,
                    "index {}..{}",
                    abbreviate_object_id(repo, entry.old_oid, opts.abbrev_len)?,
                    abbreviate_object_id(repo, entry.new_oid, opts.abbrev_len)?
                )?;
            }
            _ => {
                writeln!(
                    out,
                    "index {}..{} {}",
                    abbreviate_object_id(repo, entry.old_oid, opts.abbrev_len)?,
                    abbreviate_object_id(repo, entry.new_oid, opts.abbrev_len)?,
                    index_mode
                )?;
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
            format!("a/{git_old}")
        };
        let new_side = if new_mode == 0 {
            "/dev/null".to_owned()
        } else {
            format!("b/{git_new}")
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
        (
            std::str::from_utf8(&old_bytes)
                .expect("valid utf-8 checked above")
                .to_owned(),
            std::str::from_utf8(&new_bytes)
                .expect("valid utf-8 checked above")
                .to_owned(),
        )
    };
    let old_path = if old_mode == 0 {
        "/dev/null".to_owned()
    } else {
        format!("a/{git_old}")
    };
    let new_path = if new_mode == 0 {
        "/dev/null".to_owned()
    } else {
        format!("b/{git_new}")
    };
    let d = &opts.diff;
    let mut patch_buf = Vec::new();
    write_unified_diff_with_prefix_and_funcname_and_algorithm(
        &mut patch_buf,
        &old_text,
        &new_text,
        &old_path,
        &new_path,
        d.context_lines,
        d.inter_hunk_context,
        "",
        "",
        None,
        d.algorithm,
        false,
        d.use_git_histogram,
        d.indent_heuristic,
        true,
    )?;
    if byte_oriented {
        out.write_all(&crate::commit_encoding::encode_latin1_lossy(
            std::str::from_utf8(&patch_buf).expect("patch is latin1"),
        ))?;
    } else {
        out.write_all(&patch_buf)?;
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

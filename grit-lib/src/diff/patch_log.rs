//! Git-compatible `log -p` / `show` patch output streamed to a [`Write`](std::io::Write).

use std::io::Write;

use crate::blame::{parse_diff_algorithm_name, BlameDiffAlgorithm};
use crate::commit_pretty::abbrev_hex;
use crate::config::ConfigSet;
use crate::error::Result;
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
    let old_path = entry.old_path.as_deref().unwrap_or("/dev/null");
    let new_path = entry.new_path.as_deref().unwrap_or("/dev/null");
    let header_path = entry
        .new_path
        .as_deref()
        .or(entry.old_path.as_deref())
        .unwrap_or("");
    writeln!(out, "diff --git a/{header_path} b/{header_path}")?;
    match entry.status {
        DiffStatus::Added => {
            writeln!(out, "new file mode {}", entry.new_mode)?;
            writeln!(
                out,
                "index {}..{}",
                abbrev_hex(&zero_oid(), opts.abbrev_len),
                abbreviate_object_id(repo, entry.new_oid, opts.abbrev_len)?
            )?;
        }
        DiffStatus::Deleted => {
            writeln!(out, "deleted file mode {}", entry.old_mode)?;
            writeln!(
                out,
                "index {}..{}",
                abbreviate_object_id(repo, entry.old_oid, opts.abbrev_len)?,
                abbrev_hex(&zero_oid(), opts.abbrev_len)
            )?;
        }
        DiffStatus::Modified => {
            writeln!(
                out,
                "index {}..{} {}",
                abbreviate_object_id(repo, entry.old_oid, opts.abbrev_len)?,
                abbreviate_object_id(repo, entry.new_oid, opts.abbrev_len)?,
                entry.new_mode
            )?;
        }
        DiffStatus::Renamed | DiffStatus::Copied => {
            writeln!(out, "similarity index {}%", entry.score.unwrap_or(100))?;
            writeln!(out, "rename from {old_path}")?;
            writeln!(out, "rename to {new_path}")?;
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
                "index {}..{}",
                abbreviate_object_id(repo, entry.old_oid, opts.abbrev_len)?,
                abbreviate_object_id(repo, entry.new_oid, opts.abbrev_len)?
            )?;
        }
    }
    let old_bytes = read_blob_side(odb, entry.old_oid, entry.status == DiffStatus::Added)?;
    let new_bytes = read_blob_side(odb, entry.new_oid, entry.status == DiffStatus::Deleted)?;
    if old_bytes.contains(&0) || new_bytes.contains(&0) {
        let old_disp = log_binary_path(old_path, "a/");
        let new_disp = log_binary_path(new_path, "b/");
        writeln!(out, "Binary files {old_disp} and {new_disp} differ")?;
        return Ok(());
    }
    let old_text = std::str::from_utf8(&old_bytes).unwrap_or("");
    let new_text = std::str::from_utf8(&new_bytes).unwrap_or("");
    let d = &opts.diff;
    write_unified_diff_with_prefix_and_funcname_and_algorithm(
        out,
        old_text,
        new_text,
        old_path,
        new_path,
        d.context_lines,
        d.inter_hunk_context,
        "a/",
        "b/",
        None,
        d.algorithm,
        false,
        d.use_git_histogram,
        d.indent_heuristic,
        true,
    )?;
    Ok(())
}

fn log_binary_path(path: &str, prefix: &str) -> String {
    if path == "/dev/null" {
        path.to_owned()
    } else {
        format!("{prefix}{path}")
    }
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

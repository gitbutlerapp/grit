//! Git-compatible `log -p` / `show` patch output streamed to a [`Write`](std::io::Write).

use std::borrow::Cow;
use std::collections::HashMap;
use std::io::Write;
use std::sync::Arc;

use super::structured::{StructuredFileDiff, StructuredLine, StructuredLineKind};
use crate::attributes::load_gitattributes_for_check_attr_source;
use crate::commit_pretty::{
    abbrev_hex, approximate_object_count, auto_abbrev_len_for_object_count,
};
use crate::config::ConfigSet;
use crate::diff::options::DiffOptions;
use crate::error::{Error, Result};
use crate::index::Index;
use crate::merge_file;
use crate::objects::pack_store::PackReadContextGuard;
use crate::objects::CommitData;
use crate::objects::FileMode;
use crate::objects::{parse_commit, ObjectId, ObjectKind};
use crate::odb::Odb;
use crate::oid_abbrev_cache::OidAbbrevCache;
use crate::parallel::{run_ordered_batches, ParallelOp};
use crate::quote_path::format_diff_path_with_prefix;
use crate::repo::Repository;
use crate::userdiff::{matcher_for_path_parsed, FuncnameMatcher};
use crate::worktree_rules::WorktreeRules;

use super::merge_commit::commit_first_parent_diff_entries;
use super::merge_commit::DiffMerges;
use super::tree_content::load_odb_blob_side_for_diff;
use super::{empty_blob_oid, load_worktree_side_for_diff, zero_oid, DiffEntry, DiffStatus};

/// How much commit metadata precedes each patch in [`write_commit_log_patch`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum LogPatchCommitHeader {
    /// `git log -p` default (commit line, author, date, indented message, then patch).
    #[default]
    Full,
    /// `git log -p --format=%H` (full oid line only, then patch).
    FormatPercentH,
}

/// Binary file encoding inside a log/show patch (`git log -p` vs `git diff --binary`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum LogPatchBinaryFormat {
    /// `Binary files a/... and b/... differ` (default for `git log -p`).
    #[default]
    LogShortMessage,
    /// `GIT binary patch` sections (`git diff --binary`, accepted by `git apply`).
    GitApplyBinaryPatch,
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
    /// How to encode binary file changes in the patch body.
    pub binary_format: LogPatchBinaryFormat,
    /// Limit patches to these pathspecs (empty = all paths).
    pub pathspecs: Vec<String>,
    /// How to diff merge commits (`combined` omits apply-ready patch bodies).
    pub diff_merges: DiffMerges,
}

impl LogPatchOptions {
    /// Defaults matching `git log -p` on a typical repository.
    #[must_use]
    pub fn git_default(config: &ConfigSet) -> Self {
        Self {
            diff: DiffOptions::from_config(config),
            abbrev_len: 7,
            commit_header: LogPatchCommitHeader::Full,
            binary_format: LogPatchBinaryFormat::default(),
            pathspecs: Vec::new(),
            diff_merges: DiffMerges::Combined,
        }
    }
}

/// Options for [`write_patch`] (`git diff --binary`-compatible output for `git apply`).
#[derive(Debug, Clone)]
pub struct PatchTextOptions {
    /// Unified diff hunk settings (algorithm, context, whitespace, renames).
    pub diff: DiffOptions,
    /// Minimum abbreviated oid length on `index` lines for text files (`core.abbrev`).
    pub abbrev_len: usize,
}

impl PatchTextOptions {
    /// Resolve `core.abbrev` and attach line-diff options.
    ///
    /// # Errors
    ///
    /// Propagates config and ODB errors from abbrev resolution.
    pub fn from_repo(repo: &Repository, diff: DiffOptions) -> Result<Self> {
        let config = repo.config()?.clone();
        Ok(Self {
            diff,
            abbrev_len: resolve_core_abbrev_len(&config, repo.odb())?,
        })
    }
}

/// Write `git diff --binary`-style patches for tree diff entries (for `git apply`).
///
/// Emits `diff --git` headers, `index` lines, unified hunks or `GIT binary patch` bodies.
///
/// # Errors
///
/// Propagates ODB reads, diff generation, and I/O errors.
pub fn write_patch<W: Write>(
    repo: &Repository,
    entries: &[DiffEntry],
    opts: &PatchTextOptions,
    out: &mut W,
) -> Result<()> {
    if entries.is_empty() {
        return Ok(());
    }
    let odb = repo.odb();
    let config = repo.config()?.clone();
    let thread_policy = repo.thread_policy();
    let log_opts = LogPatchOptions {
        diff: opts.diff.clone(),
        abbrev_len: opts.abbrev_len,
        commit_header: LogPatchCommitHeader::FormatPercentH,
        binary_format: LogPatchBinaryFormat::GitApplyBinaryPatch,
        pathspecs: Vec::new(),
        diff_merges: DiffMerges::FirstParent,
    };
    let abbrev_len = opts.abbrev_len;
    let mut abbrev_cache = OidAbbrevCache::new(abbrev_len);
    let oids = entries.iter().flat_map(|e| [e.old_oid, e.new_oid]);
    abbrev_cache.warm_for_patch_batch(repo, oids, &thread_policy, &config)?;
    let mut par_opts = thread_policy.resolve(ParallelOp::DiffContent, &config);
    par_opts.min_items = 0;
    par_opts.min_total_bytes = None;
    let n_workers = par_opts.threads.get();
    if n_workers <= 1 || entries.len() < 2 {
        for entry in entries {
            write_diff_entry_patch(
                out,
                repo,
                odb,
                entry,
                &log_opts,
                &mut abbrev_cache,
                None,
                LogPatchBinaryFormat::GitApplyBinaryPatch,
                &config,
            )?;
        }
        return Ok(());
    }
    let store = odb.pack_store_for(odb.objects_dir());
    let warmed_cache = abbrev_cache;
    struct PatchWriteWorker {
        _read: PackReadContextGuard,
        abbrev: OidAbbrevCache,
    }
    run_ordered_batches(
        entries,
        1,
        n_workers,
        || PatchWriteWorker {
            _read: PackReadContextGuard::install(Arc::clone(&store)),
            abbrev: warmed_cache.clone(),
        },
        |worker, entry| {
            let mut buf = Vec::new();
            write_diff_entry_patch(
                &mut buf,
                repo,
                odb,
                entry,
                &log_opts,
                &mut worker.abbrev,
                None,
                LogPatchBinaryFormat::GitApplyBinaryPatch,
                &config,
            )?;
            Ok(buf)
        },
        |_batch, chunks: Vec<Vec<u8>>| {
            for chunk in chunks {
                out.write_all(&chunk)?;
            }
            Ok(())
        },
    )
}

/// Write patch text from precomputed [`TreeDiffStructured`](super::tree_content::TreeDiffStructured) rows.
///
/// Use after [`super::tree_diff_structured_parallel`] (or `grit diff`'s commit-range path) to avoid
/// recomputing line diffs.
///
/// # Errors
///
/// Propagates ODB reads and I/O errors.
pub fn write_patch_structured<W: Write>(
    repo: &Repository,
    items: &[super::tree_content::TreeDiffStructured],
    opts: &PatchTextOptions,
    out: &mut W,
) -> Result<()> {
    if items.is_empty() {
        return Ok(());
    }
    let odb = repo.odb();
    let config = repo.config()?.clone();
    let thread_policy = repo.thread_policy();
    let log_opts = LogPatchOptions {
        diff: opts.diff.clone(),
        abbrev_len: opts.abbrev_len,
        commit_header: LogPatchCommitHeader::FormatPercentH,
        binary_format: LogPatchBinaryFormat::GitApplyBinaryPatch,
        pathspecs: Vec::new(),
        diff_merges: DiffMerges::FirstParent,
    };
    let mut abbrev_cache = OidAbbrevCache::new(opts.abbrev_len);
    let oids = items
        .iter()
        .flat_map(|item| [item.entry.old_oid, item.entry.new_oid]);
    abbrev_cache.warm_for_patch_batch(repo, oids, &thread_policy, &config)?;
    for item in items {
        write_diff_entry_patch_from_structured(
            out,
            repo,
            odb,
            &item.entry,
            &log_opts,
            &mut abbrev_cache,
            item.binary,
            &item.structured,
        )?;
    }
    Ok(())
}

fn write_structured_line<W: Write>(out: &mut W, prefix: char, line: &StructuredLine) -> Result<()> {
    write!(out, "{prefix}")?;
    for segment in &line.segments {
        write!(out, "{}", segment.text)?;
    }
    Ok(())
}

fn format_git_diff_hunk_side(start_1based: usize, line_count: usize) -> String {
    if line_count == 1 {
        format!("{start_1based}")
    } else {
        format!("{start_1based},{line_count}")
    }
}

fn write_structured_unified_body<W: Write>(
    out: &mut W,
    git_old: &str,
    git_new: &str,
    old_mode: u32,
    new_mode: u32,
    structured: &StructuredFileDiff,
) -> Result<()> {
    const QUOTE_FULLY: bool = true;
    let old_path = if old_mode == 0 {
        "/dev/null".to_owned()
    } else {
        format_diff_path_with_prefix("a/", git_old, QUOTE_FULLY)
    };
    let new_path = if new_mode == 0 {
        "/dev/null".to_owned()
    } else {
        format_diff_path_with_prefix("b/", git_new, QUOTE_FULLY)
    };
    writeln!(out, "--- {old_path}")?;
    writeln!(out, "+++ {new_path}")?;
    for hunk in &structured.hunks {
        writeln!(
            out,
            "@@ -{} +{} @@",
            format_git_diff_hunk_side(hunk.old_start, hunk.old_lines),
            format_git_diff_hunk_side(hunk.new_start, hunk.new_lines),
        )?;
        for line in &hunk.lines {
            let prefix = match line.kind {
                StructuredLineKind::Context => ' ',
                StructuredLineKind::Add => '+',
                StructuredLineKind::Del => '-',
            };
            if line.no_newline_at_eof {
                write_structured_line(out, prefix, line)?;
                writeln!(out)?;
                writeln!(out, "\\ No newline at end of file")?;
            } else {
                write_structured_line(out, prefix, line)?;
                writeln!(out)?;
            }
        }
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn write_diff_entry_patch_from_structured<W: Write>(
    out: &mut W,
    repo: &Repository,
    odb: &Odb,
    entry: &DiffEntry,
    opts: &LogPatchOptions,
    abbrev_cache: &mut OidAbbrevCache,
    is_binary: bool,
    structured: &StructuredFileDiff,
) -> Result<()> {
    write_diff_entry_patch_headers(out, repo, entry, opts, abbrev_cache, is_binary)?;
    let old_mode = entry.old_mode.bits();
    let new_mode = entry.new_mode.bits();
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
    if entry.old_oid == entry.new_oid {
        return Ok(());
    }
    let use_structured = !is_binary
        && !structured.hunks.is_empty()
        && !structured.old_encoding_lossy
        && !structured.new_encoding_lossy;
    if use_structured {
        write_structured_unified_body(out, git_old, git_new, old_mode, new_mode, structured)?;
        return Ok(());
    }
    let path = entry.new_path.as_deref().or(entry.old_path.as_deref());
    let force_read = is_binary;
    let old_bytes = read_blob_side(odb, entry.old_oid, old_mode == 0, path, force_read)?;
    let new_bytes = read_blob_side(odb, entry.new_oid, new_mode == 0, path, force_read)?;
    if old_bytes == new_bytes {
        return Ok(());
    }
    if is_binary {
        crate::apply::write_git_binary_patch_sections(out, &old_bytes, &new_bytes)?;
        return Ok(());
    }
    write_patch_unified_body_from_bytes(
        out, &old_bytes, &new_bytes, git_old, git_new, old_mode, new_mode, opts,
    )
}

/// Unified hunk body for `git diff --binary` (no funcname context).
#[allow(clippy::too_many_arguments)]
fn write_patch_unified_body_from_bytes<W: Write>(
    out: &mut W,
    old_bytes: &[u8],
    new_bytes: &[u8],
    git_old: &str,
    git_new: &str,
    old_mode: u32,
    new_mode: u32,
    opts: &LogPatchOptions,
) -> Result<()> {
    const QUOTE_FULLY: bool = true;
    let byte_oriented = !crate::commit_encoding::is_strict_utf8(old_bytes)
        || !crate::commit_encoding::is_strict_utf8(new_bytes);
    let (old_text, new_text) = if byte_oriented {
        (
            crate::commit_encoding::decode_latin1(old_bytes),
            crate::commit_encoding::decode_latin1(new_bytes),
        )
    } else {
        let old_s = std::str::from_utf8(old_bytes)
            .map_err(|_| crate::error::Error::CorruptObject("blob is not valid UTF-8".into()))?;
        let new_s = std::str::from_utf8(new_bytes)
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
            None,
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
            None,
            d,
            QUOTE_FULLY,
        )?;
    }
    Ok(())
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
/// With [`LogPatchCommitHeader::Full`] a blank line separates consecutive commits, as in
/// `git log -p`; [`LogPatchCommitHeader::FormatPercentH`] writes none, as in
/// `git log -p --format=%H`.
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
    for (i, (oid, commit)) in parsed.iter().enumerate() {
        if i > 0 && opts.commit_header == LogPatchCommitHeader::Full {
            writeln!(out)?;
        }
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
    _parent_tree: Option<ObjectId>,
    opts: &LogPatchOptions,
    abbrev_cache: &mut OidAbbrevCache,
) -> Result<()> {
    let is_merge = commit.parents.len() > 1;
    let omit_merge_patch = is_merge && opts.diff_merges == DiffMerges::Combined;
    match opts.commit_header {
        LogPatchCommitHeader::Full => {
            writeln!(out, "commit {oid}")?;
            if is_merge {
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
            if omit_merge_patch {
                return Ok(());
            }
            writeln!(out)?;
        }
        LogPatchCommitHeader::FormatPercentH => {
            writeln!(out, "{oid}")?;
            if omit_merge_patch {
                return Ok(());
            }
            writeln!(out)?;
        }
    }
    let config = repo.config()?.clone();
    let thread_policy = repo.thread_policy();
    let entries =
        commit_first_parent_diff_entries(repo, commit, &opts.pathspecs, &config, &thread_policy)?;
    for entry in entries {
        write_diff_entry_patch(
            out,
            repo,
            odb,
            &entry,
            opts,
            abbrev_cache,
            Some(&commit.tree),
            opts.binary_format,
            &config,
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
    let config = repo.config()?.clone();
    let mut abbrev_cache = OidAbbrevCache::new(opts.abbrev_len);
    write_diff_entry_patch(
        out,
        repo,
        odb,
        entry,
        opts,
        &mut abbrev_cache,
        None,
        opts.binary_format,
        &config,
    )
}

fn index_oid_full(oid: &ObjectId) -> String {
    oid.to_hex()
}

#[allow(clippy::too_many_arguments)]
fn write_diff_entry_patch_headers<W: Write>(
    out: &mut W,
    repo: &Repository,
    entry: &DiffEntry,
    opts: &LogPatchOptions,
    abbrev_cache: &mut OidAbbrevCache,
    index_full_oid: bool,
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

    let old_mode = entry.old_mode.bits();
    let new_mode = entry.new_mode.bits();

    if old_mode == 0 {
        writeln!(out, "new file mode {}", entry.new_mode)?;
    } else if new_mode == 0 {
        writeln!(out, "deleted file mode {}", entry.old_mode)?;
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
            writeln!(out, "old mode {}", entry.old_mode)?;
            writeln!(out, "new mode {}", entry.new_mode)?;
        }
        _ => {}
    }

    if entry.old_oid != entry.new_oid {
        let index_mode = if new_mode != 0 {
            entry.new_mode
        } else {
            entry.old_mode
        };
        let mut index_pair = |old: &ObjectId, new: &ObjectId| -> Result<(String, String)> {
            if index_full_oid {
                Ok((index_oid_full(old), index_oid_full(new)))
            } else {
                abbrev_index_pair(abbrev_cache, repo, old, new, opts.abbrev_len)
            }
        };
        match entry.status {
            DiffStatus::Added => {
                let (old_a, new_a) = index_pair(&zero_oid(), &entry.new_oid)?;
                writeln!(out, "index {old_a}..{new_a}")?;
            }
            DiffStatus::Deleted => {
                let (old_a, new_a) = index_pair(&entry.old_oid, &zero_oid())?;
                writeln!(out, "index {old_a}..{new_a}")?;
            }
            DiffStatus::Renamed | DiffStatus::Copied => {
                let (old_a, new_a) = index_pair(&entry.old_oid, &entry.new_oid)?;
                writeln!(out, "index {old_a}..{new_a}")?;
            }
            _ => {
                let (old_a, new_a) = index_pair(&entry.old_oid, &entry.new_oid)?;
                writeln!(out, "index {old_a}..{new_a} {index_mode}")?;
            }
        }
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn write_diff_entry_patch<W: Write>(
    out: &mut W,
    repo: &Repository,
    odb: &Odb,
    entry: &DiffEntry,
    opts: &LogPatchOptions,
    abbrev_cache: &mut OidAbbrevCache,
    new_tree: Option<&ObjectId>,
    binary_format: LogPatchBinaryFormat,
    config: &ConfigSet,
) -> Result<()> {
    let old_mode = entry.old_mode.bits();
    let new_mode = entry.new_mode.bits();
    let path = entry.new_path.as_deref().or(entry.old_path.as_deref());
    let force_read = binary_format == LogPatchBinaryFormat::GitApplyBinaryPatch;
    let (old_bytes, old_bin) =
        load_patch_blob_side(odb, entry.old_oid, old_mode == 0, path, force_read)?;
    let (new_bytes, new_bin) =
        load_patch_blob_side(odb, entry.new_oid, new_mode == 0, path, force_read)?;
    write_diff_entry_patch_with_resolved_bytes(
        out,
        repo,
        entry,
        opts,
        abbrev_cache,
        binary_format,
        config,
        old_bin || new_bin,
        &old_bytes,
        &new_bytes,
        new_tree,
    )
}

#[allow(clippy::too_many_arguments)]
fn write_diff_entry_patch_with_resolved_bytes<W: Write>(
    out: &mut W,
    repo: &Repository,
    entry: &DiffEntry,
    opts: &LogPatchOptions,
    abbrev_cache: &mut OidAbbrevCache,
    binary_format: LogPatchBinaryFormat,
    config: &ConfigSet,
    side_known_binary: bool,
    old_bytes: &[u8],
    new_bytes: &[u8],
    new_tree: Option<&ObjectId>,
) -> Result<()> {
    const QUOTE_FULLY: bool = true;
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
    let old_mode = entry.old_mode.bits();
    let new_mode = entry.new_mode.bits();
    let is_binary =
        side_known_binary || merge_file::is_binary(old_bytes) || merge_file::is_binary(new_bytes);
    let index_full_oid = is_binary && binary_format == LogPatchBinaryFormat::GitApplyBinaryPatch;
    write_diff_entry_patch_headers(out, repo, entry, opts, abbrev_cache, index_full_oid)?;

    if old_bytes == new_bytes && entry.old_oid == entry.new_oid {
        return Ok(());
    }

    if is_binary {
        match binary_format {
            LogPatchBinaryFormat::LogShortMessage => {
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
            }
            LogPatchBinaryFormat::GitApplyBinaryPatch => {
                crate::apply::write_git_binary_patch_sections(out, old_bytes, new_bytes)?;
            }
        }
        return Ok(());
    }

    if binary_format == LogPatchBinaryFormat::GitApplyBinaryPatch && new_tree.is_none() {
        return write_patch_unified_body_from_bytes(
            out, old_bytes, new_bytes, git_old, git_new, old_mode, new_mode, opts,
        );
    }
    let byte_oriented = !crate::commit_encoding::is_strict_utf8(old_bytes)
        || !crate::commit_encoding::is_strict_utf8(new_bytes);
    let (old_text, new_text) = if byte_oriented {
        (
            crate::commit_encoding::decode_latin1(old_bytes),
            crate::commit_encoding::decode_latin1(new_bytes),
        )
    } else {
        let old_s = std::str::from_utf8(old_bytes)
            .map_err(|_| crate::error::Error::CorruptObject("blob is not valid UTF-8".into()))?;
        let new_s = std::str::from_utf8(new_bytes)
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

/// Where [`write_patch_resolved`] takes the content of one side of a file.
///
/// Pass [`PatchSide::Bytes`] for content already in memory. A caller that skipped loading
/// a side (diff collection usually drops binary content, which structured hunks never use)
/// names where the side lives instead, and the writer reads it only while rendering that
/// file. A side whose mode in [`PatchResolvedSide::entry`] is `000000` is empty whatever
/// variant is given.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PatchSide {
    /// Content the caller already loaded, in repository form.
    Bytes(Vec<u8>),
    /// The blob named by this side's id in [`PatchResolvedSide::entry`], read from the
    /// object database.
    Blob,
    /// The work-tree file at this side's path, converted to repository form the way
    /// `git add` would (clean filters, `.gitattributes` and line-ending rules).
    Worktree,
}

/// One file for [`write_patch_resolved`]: diff metadata plus where each side's content is.
#[derive(Debug, Clone)]
pub struct PatchResolvedSide {
    /// Tree diff metadata (paths, modes, ids, status).
    pub entry: DiffEntry,
    /// Old side content.
    pub old: PatchSide,
    /// New side content.
    pub new: PatchSide,
}

/// Write `git diff --binary`-compatible patch text for files whose sides may live in memory,
/// the object database, or the work tree (index and work-tree diffs).
///
/// Binary files are written as `GIT binary patch` sections. Their `index` line always carries
/// full ids of the content actually written, hashed from the bytes for [`PatchSide::Bytes`] and
/// [`PatchSide::Worktree`] sides, because `git apply` refuses a binary patch without one and
/// work-tree entries often carry no blob id for the work-tree side.
///
/// # Parameters
///
/// - `repo`: repository for objects, config, and (for [`PatchSide::Worktree`]) the work tree
///   and index.
/// - `files`: files to write, in output order.
/// - `opts`: hunk and abbreviation settings.
/// - `out`: destination for patch bytes.
///
/// # Errors
///
/// Returns [`Error::RequiresWorkTree`](crate::error::Error::RequiresWorkTree) when a
/// [`PatchSide::Worktree`] side is requested in a bare repository, and propagates index, ODB,
/// work-tree, hashing and I/O errors.
pub fn write_patch_resolved<W: Write>(
    repo: &Repository,
    files: &[PatchResolvedSide],
    opts: &PatchTextOptions,
    out: &mut W,
) -> Result<()> {
    if files.is_empty() {
        return Ok(());
    }
    let config = repo.config()?.clone();
    let thread_policy = repo.thread_policy();
    let log_opts = LogPatchOptions {
        diff: opts.diff.clone(),
        abbrev_len: opts.abbrev_len,
        commit_header: LogPatchCommitHeader::FormatPercentH,
        binary_format: LogPatchBinaryFormat::GitApplyBinaryPatch,
        pathspecs: Vec::new(),
        diff_merges: DiffMerges::FirstParent,
    };
    let mut abbrev_cache = OidAbbrevCache::new(opts.abbrev_len);
    let oids = files
        .iter()
        .flat_map(|f| [f.entry.old_oid, f.entry.new_oid]);
    abbrev_cache.warm_for_patch_batch(repo, oids, &thread_policy, &config)?;
    let mut worktree = None;
    for file in files {
        let old = resolve_patch_side(repo, file, EntrySide::Old, &mut worktree)?;
        let new = resolve_patch_side(repo, file, EntrySide::New, &mut worktree)?;
        let entry = if merge_file::is_binary(&old) || merge_file::is_binary(&new) {
            Cow::Owned(entry_with_content_ids(repo, file, &old, &new)?)
        } else {
            Cow::Borrowed(&file.entry)
        };
        write_diff_entry_patch_with_resolved_bytes(
            out,
            repo,
            &entry,
            &log_opts,
            &mut abbrev_cache,
            LogPatchBinaryFormat::GitApplyBinaryPatch,
            &config,
            false,
            &old,
            &new,
            None,
        )?;
    }
    Ok(())
}

#[derive(Clone, Copy)]
enum EntrySide {
    Old,
    New,
}

impl EntrySide {
    fn of(self, file: &PatchResolvedSide) -> (&PatchSide, FileMode, ObjectId) {
        let e = &file.entry;
        match self {
            Self::Old => (&file.old, e.old_mode, e.old_oid),
            Self::New => (&file.new, e.new_mode, e.new_oid),
        }
    }

    fn path(self, entry: &DiffEntry) -> &str {
        match self {
            Self::Old => entry.old_path.as_deref().unwrap_or_else(|| entry.path()),
            Self::New => entry.path(),
        }
    }
}

/// Index and attribute rules for reading [`PatchSide::Worktree`] sides, loaded on first use.
struct WorktreeSideReader<'r> {
    work_tree: &'r std::path::Path,
    index: Index,
    rules: WorktreeRules,
}

impl<'r> WorktreeSideReader<'r> {
    fn open(repo: &'r Repository) -> Result<Self> {
        let work_tree = repo.work_tree().ok_or(Error::RequiresWorkTree)?;
        let index = repo.index_handle().load()?;
        let rules = WorktreeRules::from_repository(repo, &index)?;
        Ok(Self {
            work_tree,
            index,
            rules,
        })
    }

    fn read(&self, odb: &Odb, path: &str, mode: u32) -> Result<Vec<u8>> {
        let index_entry = self.index.get(&crate::repo_relpath::storage_bytes(path), 0);
        Ok(load_worktree_side_for_diff(
            odb,
            self.work_tree,
            path,
            mode,
            self.rules.config(),
            index_entry,
            Some(&self.rules),
            true,
        )?
        .0
        .data)
    }
}

fn resolve_patch_side<'f, 'r>(
    repo: &'r Repository,
    file: &'f PatchResolvedSide,
    side: EntrySide,
    worktree: &mut Option<WorktreeSideReader<'r>>,
) -> Result<Cow<'f, [u8]>> {
    let (source, mode, oid) = side.of(file);
    let mode = mode.bits();
    if mode == 0 {
        return Ok(Cow::Borrowed(&[]));
    }
    match source {
        PatchSide::Bytes(bytes) => Ok(Cow::Borrowed(bytes)),
        PatchSide::Blob => Ok(Cow::Owned(read_blob_side(
            repo.odb(),
            oid,
            false,
            Some(side.path(&file.entry)),
            true,
        )?)),
        PatchSide::Worktree => {
            let reader = match worktree {
                Some(reader) => reader,
                None => worktree.insert(WorktreeSideReader::open(repo)?),
            };
            Ok(Cow::Owned(reader.read(
                repo.odb(),
                side.path(&file.entry),
                mode,
            )?))
        }
    }
}

/// `file.entry` with each present side's id set to the id of the content being written.
fn entry_with_content_ids(
    repo: &Repository,
    file: &PatchResolvedSide,
    old: &[u8],
    new: &[u8],
) -> Result<DiffEntry> {
    let content_id = |side: EntrySide, bytes: &[u8]| -> Result<ObjectId> {
        let (source, mode, oid) = side.of(file);
        if mode.is_absent() {
            return Ok(zero_oid());
        }
        match source {
            PatchSide::Blob => Ok(oid),
            PatchSide::Bytes(_) | PatchSide::Worktree => {
                repo.odb().hash_algo().hash_object(ObjectKind::Blob, bytes)
            }
        }
    };
    let mut entry = file.entry.clone();
    entry.old_oid = content_id(EntrySide::Old, old)?;
    entry.new_oid = content_id(EntrySide::New, new)?;
    Ok(entry)
}

fn load_patch_blob_side(
    odb: &Odb,
    oid: ObjectId,
    is_missing: bool,
    path: Option<&str>,
    force_read_content: bool,
) -> Result<(Vec<u8>, bool)> {
    if is_missing || oid == zero_oid() || oid == empty_blob_oid() {
        return Ok((Vec::new(), false));
    }
    let (side, binary) = load_odb_blob_side_for_diff(odb, &oid, path, None, force_read_content)?;
    Ok((side.data, binary))
}

fn read_blob_side(
    odb: &Odb,
    oid: ObjectId,
    is_missing: bool,
    path: Option<&str>,
    force_read_content: bool,
) -> Result<Vec<u8>> {
    Ok(load_patch_blob_side(odb, oid, is_missing, path, force_read_content)?.0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    use crate::hash::hash_object;
    use crate::objects::HashAlgo;

    #[test]
    fn log_short_message_skips_big_blob_reads() -> Result<()> {
        let dir = tempfile::tempdir().expect("tempdir");
        let repo = Repository::init(dir.path(), &crate::repo::InitOptions::default())?;
        std::fs::write(
            repo.git_dir().join("config"),
            "[core]\n\tbigFileThreshold = 4k\n",
        )?;
        let odb = repo.odb();
        let old = odb.write(ObjectKind::Blob, &vec![b'o'; 5000])?;
        let new = odb.write(ObjectKind::Blob, &vec![b'n'; 5001])?;
        let entry = DiffEntry {
            status: DiffStatus::Modified,
            old_path: Some("asset.bin".into()),
            new_path: Some("asset.bin".into()),
            old_oid: old,
            new_oid: new,
            old_mode: FileMode::REGULAR,
            new_mode: FileMode::REGULAR,
            score: None,
        };
        let metrics = odb.hot_path_metrics();
        metrics.reset_blob_content_reads();
        metrics.set_blob_counting(true);
        let mut out = Cursor::new(Vec::new());
        let mut opts = LogPatchOptions::git_default(&ConfigSet::default());
        opts.binary_format = LogPatchBinaryFormat::LogShortMessage;
        write_diff_entry_log_patch(&mut out, &repo, odb, &entry, &opts)?;
        metrics.set_blob_counting(false);
        let text = String::from_utf8(out.into_inner()).expect("utf-8");
        assert!(text.contains("Binary files"));
        assert_eq!(metrics.blob_content_reads(), 0);
        Ok(())
    }

    #[test]
    fn copied_diff_emits_copy_headers() -> Result<()> {
        let blob_oid = hash_object(HashAlgo::Sha1, ObjectKind::Blob, b"x").expect("hash");
        let entry = DiffEntry {
            status: DiffStatus::Copied,
            old_path: Some("src/a.txt".into()),
            new_path: Some("dst/b.txt".into()),
            old_oid: blob_oid,
            new_oid: blob_oid,
            old_mode: FileMode::REGULAR,
            new_mode: FileMode::REGULAR,
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

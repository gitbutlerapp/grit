//! `grit diff` — show changes as a delta-style, dual-line-numbered diff.
//!
//! With no argument it shows all uncommitted changes (the worktree and index
//! against HEAD's tree). Pass `--unstaged` for index against worktree only, or
//! `--staged` for the index against HEAD. With one commit-ish
//! it shows the change that commit introduced (its first parent's tree against
//! its own). Two commits, or a two-dot range such as `A..B`, diff those
//! commits' trees. Use `--against <ref>` to compare the worktree to another
//! ref's tree instead of HEAD.
//!
//! The human rendering imitates [delta](https://github.com/dandavison/delta): a
//! bold file header, a hunk header showing the enclosing definition, two
//! line-number columns (old / new), red/green line backgrounds, and brighter
//! intra-line word highlights — no `+`/`-` patch markers. Color is emitted only
//! on a TTY (honoring `NO_COLOR`); piped output is plain text with `+`/`-`.

use anyhow::{Context, Result};
use grit_lib::diff::{
    diff_commits_with_renames, diff_index_to_tree, diff_index_to_worktree_with_options,
    diff_trees_with_renames, read_submodule_head_oid, read_worktree_bytes_for_mode,
    structured::{
        format_hunk_range, structured_file_diff, SideBytes, StructuredDiffOptions,
        StructuredLineKind,
    },
    submodule_porcelain_flags, DiffEntry, DiffIndexToWorktreeOptions, DiffStatus,
};
use grit_lib::merge_base::merge_base_for_diff_two_commits;
use grit_lib::objects::ObjectId;
use grit_lib::odb::Odb;
use grit_lib::rev_parse::{
    peel_to_commit_for_merge_base, resolve_revision, resolve_revision_for_range_end,
    split_double_dot_range, split_triple_dot_range,
};
use grit_lib::state::resolve_head;
use serde::Serialize;
use std::io::IsTerminal;
use std::io::Write;
use std::path::Path;

use crate::context;
use crate::markdown;
use crate::output::{outln, HumanRender, MarkdownRender, RenderContext};

/// Lines of unchanged context to show around each change.
const CONTEXT_LINES: usize = 3;
/// Tab stop width used when expanding tabs for display.
const TAB_WIDTH: usize = 8;

// --- ANSI styling (256-color, delta-ish) -----------------------------------
const RESET: &str = "\x1b[0m";
const BG_DEL: &str = "48;5;52"; // dark red
const BG_DEL_EMPH: &str = "48;5;88"; // brighter red
const BG_ADD: &str = "48;5;22"; // dark green
const BG_ADD_EMPH: &str = "48;5;28"; // brighter green
const FG_DIM: &str = "38;5;244"; // gutter gray
const FG_DEL_NUM: &str = "38;5;167"; // removed line number
const FG_ADD_NUM: &str = "38;5;71"; // added line number
                                    // Explicit near-white foreground for the changed (colored-background) lines.
                                    // Our backgrounds are always dark, so a light fg stays readable on both light-
                                    // and dark-themed terminals (the default fg would be dark-on-dark in light mode).
const FG_ON_DIFF: &str = "38;5;231";

/// Result of `grit diff`.
#[derive(Serialize)]
pub struct DiffOutcome {
    pub files: Vec<FileDiff>,
}

/// One changed file.
#[derive(Serialize)]
pub struct FileDiff {
    /// Display path (the new path, or the old path for a deletion).
    pub path: String,
    /// Pre-rename path, when different from `path`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub old_path: Option<String>,
    pub status: String,
    /// Old git mode (octal), when present on this side.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub old_mode: Option<String>,
    /// New git mode (octal), when present on this side.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub new_mode: Option<String>,
    /// True when old side bytes were decoded with lossy UTF-8.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub old_encoding_lossy: bool,
    /// True when new side bytes were decoded with lossy UTF-8.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub new_encoding_lossy: bool,
    pub binary: bool,
    /// True when the blob OID changed (not a mode-only touch on the same content).
    #[serde(skip_serializing)]
    pub content_changed: bool,
    pub hunks: Vec<Hunk>,
}

/// A contiguous run of changes plus surrounding context.
#[derive(Serialize)]
pub struct Hunk {
    /// 1-based first old/new line numbers in the hunk.
    pub old_start: usize,
    pub new_start: usize,
    /// Lines on the old side in this hunk (context + removals).
    pub old_lines: usize,
    /// Lines on the new side in this hunk (context + additions).
    pub new_lines: usize,
    /// The enclosing definition line (function/struct/…), when found.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub context: Option<String>,
    pub lines: Vec<Line>,
}

/// One rendered line of a hunk.
#[derive(Serialize)]
pub struct Line {
    pub kind: LineKind,
    /// 1-based old/new line numbers (absent on the side where the line is new/gone).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub old: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub new: Option<usize>,
    /// The line split into segments; `emphasis` marks the intra-line word changes.
    pub segments: Vec<Segment>,
    /// Missing newline at end of file on this side (Git `\ No newline at end of file`).
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub no_newline_at_eof: bool,
}

#[derive(Serialize, Clone, Copy, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum LineKind {
    Context,
    Add,
    Del,
}

/// A run of text within a line, flagged if it's part of the word-level change.
#[derive(Serialize)]
pub struct Segment {
    pub text: String,
    pub emphasis: bool,
}

/// Inputs for `grit diff` (flags and revision arguments).
pub struct DiffRunOptions {
    pub staged: bool,
    pub unstaged: bool,
    pub against: Option<String>,
    pub commits: Vec<String>,
}

pub fn run(opts: DiffRunOptions) -> Result<DiffOutcome> {
    let repo = context::discover()?;
    if opts.staged && opts.unstaged {
        anyhow::bail!("--staged and --unstaged cannot be combined");
    }
    if opts.staged {
        if !opts.commits.is_empty() || opts.against.is_some() {
            anyhow::bail!("--staged cannot be combined with revision arguments or --against");
        }
        return Ok(outcome_from_changes(staged_changes(&repo)?));
    }
    if opts.unstaged {
        if !opts.commits.is_empty() || opts.against.is_some() {
            anyhow::bail!("--unstaged cannot be combined with revision arguments or --against");
        }
        return Ok(outcome_from_changes(unstaged_changes(&repo)?));
    }
    if opts.commits.len() > 2 {
        anyhow::bail!("at most two commits, or one two-dot range such as A..B");
    }
    if opts.against.is_some() && !opts.commits.is_empty() {
        anyhow::bail!("--against compares the worktree to a ref and takes no commit arguments");
    }
    let changes = match opts.commits.len() {
        0 => {
            if let Some(spec) = opts.against {
                against_ref_changes(&repo, &spec)?
            } else {
                worktree_changes(&repo)?
            }
        }
        1 => {
            let spec = &opts.commits[0];
            if let Some((base, tip)) = parse_commit_range(&repo, spec)? {
                tree_commit_changes(&repo, &base, &tip)?
            } else {
                let oid = resolve_diff_commit(&repo, spec)?;
                commit_changes(&repo, &oid)?
            }
        }
        2 => {
            let base = resolve_diff_commit(&repo, &opts.commits[0])?;
            let tip = resolve_diff_commit(&repo, &opts.commits[1])?;
            tree_commit_changes(&repo, &base, &tip)?
        }
        _ => unreachable!("checked above"),
    };
    Ok(outcome_from_changes(changes))
}

/// The diff a single commit introduced, as a [`DiffOutcome`]. Reused by `grit show`.
pub fn diff_of_commit(repo: &grit_lib::repo::Repository, oid: &ObjectId) -> Result<DiffOutcome> {
    Ok(outcome_from_changes(commit_changes(repo, oid)?))
}

/// Build a full patch-style [`DiffOutcome`] from tree diff entries (e.g. stash W vs base).
pub fn outcome_from_tree_entries(
    repo: &grit_lib::repo::Repository,
    mut entries: Vec<DiffEntry>,
) -> Result<DiffOutcome> {
    sort_entries(&mut entries);
    let changes = entries
        .into_iter()
        .map(|e| {
            let (old_bytes, old_bin) = old_side_bytes(&repo.odb, &e)?;
            let (new_bytes, new_bin) = new_side_bytes(&repo.odb, &e, None)?;
            Ok(file_change(e, old_bytes, new_bytes, old_bin || new_bin))
        })
        .collect::<Result<Vec<_>>>()?;
    Ok(outcome_from_changes(changes))
}

fn outcome_from_changes(changes: Vec<FileChange>) -> DiffOutcome {
    DiffOutcome {
        files: changes.into_iter().map(build_file_diff).collect(),
    }
}

/// A changed file with both sides' text resolved.
struct FileChange {
    path: String,
    old_path: Option<String>,
    status: DiffStatus,
    old_mode: String,
    new_mode: String,
    old_bytes: SideBytes,
    new_bytes: SideBytes,
    binary: bool,
    content_changed: bool,
}

/// Unstaged changes: worktree vs index (what changed on disk since `git add`).
fn unstaged_changes(repo: &grit_lib::repo::Repository) -> Result<Vec<FileChange>> {
    let work_tree = repo
        .work_tree
        .as_deref()
        .context("grit diff needs a working tree")?;
    let mut index = repo.load_index().context("could not load the index")?;
    let worktree_rules = grit_lib::worktree_rules::WorktreeRules::from_repository(repo, &index)
        .context("could not load worktree rules")?;
    let config_arc = worktree_rules.config_arc();
    let rules_arc = std::sync::Arc::new(std::sync::Mutex::new(worktree_rules));
    let diff_opts = DiffIndexToWorktreeOptions {
        repository_git_dir: Some(repo.git_dir.clone()),
        config: Some(config_arc),
        worktree_rules: Some(std::sync::Arc::clone(&rules_arc)),
        ..DiffIndexToWorktreeOptions::default()
    };
    let (mut entries, _) =
        diff_index_to_worktree_with_options(&repo.odb, &mut index, work_tree, diff_opts)
            .context("could not diff the index against the working tree")?;
    sort_entries(&mut entries);
    entries
        .into_iter()
        .map(|e| {
            let (old_bytes, old_bin) = old_side_bytes(&repo.odb, &e)?;
            let (new_bytes, new_bin) = new_side_bytes(&repo.odb, &e, Some(work_tree))?;
            Ok(file_change(e, old_bytes, new_bytes, old_bin || new_bin))
        })
        .collect()
}

/// Staged changes: index vs HEAD's tree.
fn staged_changes(repo: &grit_lib::repo::Repository) -> Result<Vec<FileChange>> {
    let head = resolve_head(&repo.git_dir).context("could not resolve HEAD")?;
    let head_tree = match head.oid() {
        Some(oid) => Some(context::commit_tree(repo, oid)?),
        None => None,
    };
    let index = repo.load_index().context("could not load the index")?;
    let mut entries = diff_index_to_tree(&repo.odb, &index, head_tree.as_ref(), false)
        .context("could not diff the index against HEAD")?;
    sort_entries(&mut entries);
    entries
        .into_iter()
        .map(|e| {
            let (old_bytes, old_bin) = old_side_bytes(&repo.odb, &e)?;
            let (new_bytes, new_bin) = new_side_bytes(&repo.odb, &e, None)?;
            Ok(file_change(e, old_bytes, new_bytes, old_bin || new_bin))
        })
        .collect()
}

/// Worktree (and index) vs the tree at `ref_spec`.
fn against_ref_changes(
    repo: &grit_lib::repo::Repository,
    ref_spec: &str,
) -> Result<Vec<FileChange>> {
    let commit = resolve_diff_commit(repo, ref_spec)?;
    let base_tree = context::commit_tree(repo, &commit)?;
    worktree_against_tree(repo, Some(&base_tree))
}

/// Resolve a user revision to a commit OID, peeling annotated tags.
fn resolve_diff_commit(repo: &grit_lib::repo::Repository, spec: &str) -> Result<ObjectId> {
    let oid =
        resolve_revision(repo, spec).with_context(|| format!("could not resolve '{spec}'"))?;
    peel_to_commit_for_merge_base(repo, oid).map_err(map_peel_to_anyhow)
}

/// Resolve a range endpoint and peel to a commit (tags, etc.).
fn resolve_diff_range_end(repo: &grit_lib::repo::Repository, spec: &str) -> Result<ObjectId> {
    let oid = resolve_revision_for_range_end(repo, spec)?;
    peel_to_commit_for_merge_base(repo, oid).map_err(map_peel_to_anyhow)
}

fn map_peel_to_anyhow(err: grit_lib::error::Error) -> anyhow::Error {
    match err {
        grit_lib::error::Error::InvalidRef(msg) => anyhow::anyhow!(msg),
        other => other.into(),
    }
}

/// Diff trees at two commits.
fn tree_commit_changes(
    repo: &grit_lib::repo::Repository,
    base: &ObjectId,
    tip: &ObjectId,
) -> Result<Vec<FileChange>> {
    let mut entries =
        diff_commits_with_renames(&repo.odb, base, tip, 50).context("could not diff commits")?;
    sort_entries(&mut entries);
    entries
        .into_iter()
        .map(|e| {
            let (old_bytes, old_bin) = old_side_bytes(&repo.odb, &e)?;
            let (new_bytes, new_bin) = new_side_bytes(&repo.odb, &e, None)?;
            Ok(file_change(e, old_bytes, new_bytes, old_bin || new_bin))
        })
        .collect()
}

/// Resolve `spec` when it is a revision range (`A..B`, `A...B`, `..B`, etc.).
fn parse_commit_range(
    repo: &grit_lib::repo::Repository,
    spec: &str,
) -> Result<Option<(ObjectId, ObjectId)>> {
    if let Some((left, right)) = split_triple_dot_range(spec) {
        let left_c = if left.is_empty() {
            resolve_diff_range_end(repo, "HEAD")?
        } else {
            resolve_diff_range_end(repo, left)?
        };
        let right_c = if right.is_empty() {
            resolve_diff_range_end(repo, "HEAD")?
        } else {
            resolve_diff_range_end(repo, right)?
        };
        let base = merge_base_for_diff_two_commits(repo, left_c, right_c).map_err(|e| {
            anyhow::anyhow!(match e {
                grit_lib::merge_base::MergeBaseForDiffError::None => {
                    "no merge base for that revision range".to_owned()
                }
                grit_lib::merge_base::MergeBaseForDiffError::Multiple => {
                    "multiple merge bases for that revision range".to_owned()
                }
                grit_lib::merge_base::MergeBaseForDiffError::Other(msg) => msg,
            })
        })?;
        return Ok(Some((base, right_c)));
    }
    let Some((left, right)) = split_double_dot_range(spec) else {
        return Ok(None);
    };
    let base = if left.is_empty() {
        resolve_diff_range_end(repo, "HEAD")?
    } else {
        resolve_diff_range_end(repo, left)?
    };
    let tip = if right.is_empty() {
        resolve_diff_range_end(repo, "HEAD")?
    } else {
        resolve_diff_range_end(repo, right)?
    };
    Ok(Some((base, tip)))
}

/// All uncommitted changes: HEAD's tree vs the worktree.
fn worktree_changes(repo: &grit_lib::repo::Repository) -> Result<Vec<FileChange>> {
    let head = resolve_head(&repo.git_dir).context("could not resolve HEAD")?;
    let head_tree = match head.oid() {
        Some(oid) => Some(context::commit_tree(repo, oid)?),
        None => None,
    };
    worktree_against_tree(repo, head_tree.as_ref())
}

fn worktree_against_tree(
    repo: &grit_lib::repo::Repository,
    base_tree: Option<&ObjectId>,
) -> Result<Vec<FileChange>> {
    let work_tree = repo
        .work_tree
        .as_deref()
        .context("grit diff needs a working tree")?;
    let mut index = repo.load_index().context("could not load the index")?;
    let config = repo.config().context("could not load config")?;

    let index_backed = base_tree
        .zip(index.cache_tree_root.as_ref())
        .map(|(left, right)| left == right)
        .unwrap_or(base_tree.is_none() && index.cache_tree_root.is_none())
        && grit_lib::write_tree::cache_tree_fully_valid(&repo.odb, index.cache_tree.as_ref());
    let diff_index_mtime = grit_lib::diff::index_mtime_for_diff(&index, None);
    let mut entries = if index_backed {
        let (entries, _) = diff_index_to_worktree_with_options(
            &repo.odb,
            &mut index,
            work_tree,
            DiffIndexToWorktreeOptions {
                repository_git_dir: Some(repo.git_dir.clone()),
                config: Some(config.clone()),
                index_mtime: diff_index_mtime,
                thread_policy: Some(repo.thread_policy()),
                refresh_index_stat_in_pass: false,
                for_status: true,
                ..DiffIndexToWorktreeOptions::default()
            },
        )
        .context("could not diff the working tree")?;
        entries
    } else {
        let worktree_rules = grit_lib::worktree_rules::WorktreeRules::from_repository(repo, &index)
            .context("could not load worktree rules")?;
        grit_lib::diff::diff_tree_to_worktree_with_git_dir_and_rules(
            &repo.odb,
            base_tree,
            work_tree,
            &repo.git_dir,
            &index,
            Some(config.as_ref()),
            Some(&worktree_rules),
        )
        .context("could not diff the working tree")?
    };
    sort_entries(&mut entries);

    entries
        .into_iter()
        .map(|e| {
            let (old_bytes, old_bin) = old_side_bytes(&repo.odb, &e)?;
            let (new_bytes, new_bin) = new_side_bytes(&repo.odb, &e, Some(work_tree))?;
            Ok(file_change(e, old_bytes, new_bytes, old_bin || new_bin))
        })
        .collect()
}

/// The change a single commit introduced: its first parent's tree vs its own.
fn commit_changes(repo: &grit_lib::repo::Repository, oid: &ObjectId) -> Result<Vec<FileChange>> {
    let commit = context::read_commit(repo, oid)?;
    let new_tree = commit.tree;
    let old_tree = match commit.parents.first() {
        Some(parent) => Some(context::commit_tree(repo, parent)?),
        None => None,
    };

    let mut entries =
        diff_trees_with_renames(&repo.odb, old_tree.as_ref(), Some(&new_tree), "", 50)
            .context("could not diff the commit")?;
    sort_entries(&mut entries);

    entries
        .into_iter()
        .map(|e| {
            let (old_bytes, old_bin) = old_side_bytes(&repo.odb, &e)?;
            let (new_bytes, new_bin) = new_side_bytes(&repo.odb, &e, None)?;
            Ok(file_change(e, old_bytes, new_bytes, old_bin || new_bin))
        })
        .collect()
}

fn sort_entries(entries: &mut [DiffEntry]) {
    entries.sort_by(|a, b| a.path().cmp(b.path()));
}

fn file_change(
    e: DiffEntry,
    old_bytes: SideBytes,
    new_bytes: SideBytes,
    binary: bool,
) -> FileChange {
    let path = e
        .new_path
        .clone()
        .or_else(|| e.old_path.clone())
        .unwrap_or_default();
    // Record the pre-rename path only on a true rename (old differs from the
    // displayed path). For a deletion the displayed path *is* the old path, so
    // this must not treat it as a rename.
    let old_path = e.old_path.filter(|op| *op != path);
    FileChange {
        path,
        old_path,
        status: e.status,
        old_mode: e.old_mode,
        new_mode: e.new_mode,
        old_bytes,
        new_bytes,
        binary,
        content_changed: e.old_oid != e.new_oid,
    }
}

fn old_side_bytes(odb: &Odb, e: &DiffEntry) -> Result<(SideBytes, bool)> {
    if e.old_mode == "160000" {
        return Ok((
            SideBytes::from_bytes(gitlink_line(&e.old_oid, None).into_bytes()),
            false,
        ));
    }
    if e.old_mode == "000000" {
        return Ok((SideBytes::from_bytes(Vec::new()), false));
    }
    blob_bytes(odb, &e.old_oid)
}

fn new_side_bytes(odb: &Odb, e: &DiffEntry, work_tree: Option<&Path>) -> Result<(SideBytes, bool)> {
    if e.new_mode == "160000" {
        return Ok((
            SideBytes::from_bytes(gitlink_new_line(e, work_tree).into_bytes()),
            false,
        ));
    }
    if e.new_mode == "000000" {
        return Ok((SideBytes::from_bytes(Vec::new()), false));
    }
    if let (Some(wt), Some(p)) = (work_tree, e.new_path.as_deref().or(e.old_path.as_deref())) {
        let mode = u32::from_str_radix(&e.new_mode, 8).unwrap_or(0);
        Ok(file_bytes(&wt.join(p), mode))
    } else {
        blob_bytes(odb, &e.new_oid)
    }
}

fn gitlink_line(oid: &ObjectId, dirty_suffix: Option<&str>) -> String {
    if *oid == ObjectId::zero() {
        return String::new();
    }
    let suffix = dirty_suffix.unwrap_or("");
    format!("Subproject commit {}{suffix}\n", oid.to_hex())
}

/// Resolve the worktree-side gitlink line, including `-dirty` when the submodule has modified
/// tracked content (Git patch / `--submodule` parity), including when HEAD moved ahead of the tree.
fn gitlink_new_line(e: &DiffEntry, work_tree: Option<&Path>) -> String {
    let path = e.new_path.as_deref().or(e.old_path.as_deref());
    let resolved = if e.new_oid != ObjectId::zero() {
        e.new_oid
    } else if let (Some(wt), Some(p)) = (work_tree, path) {
        read_submodule_head_oid(&wt.join(p)).unwrap_or(ObjectId::zero())
    } else {
        ObjectId::zero()
    };
    if resolved == ObjectId::zero() {
        return String::new();
    }
    let dirty = e.old_mode == "160000"
        && e.new_mode == "160000"
        && work_tree.is_some_and(|wt| {
            path.is_some_and(|p| submodule_porcelain_flags(wt, p, e.old_oid).modified)
        });
    gitlink_line(&resolved, if dirty { Some("-dirty") } else { None })
}

/// Read a blob. Returns `(bytes, is_binary)`; a zero oid → empty.
fn blob_bytes(odb: &Odb, oid: &ObjectId) -> Result<(SideBytes, bool)> {
    if *oid == ObjectId::zero() {
        return Ok((SideBytes::from_bytes(Vec::new()), false));
    }
    let data = odb.read(oid)?.data;
    Ok(bytes_for_diff(&data))
}

/// Read a worktree file. A missing file reads as empty (treated as a deletion).
fn file_bytes(path: &Path, mode: u32) -> (SideBytes, bool) {
    match read_worktree_bytes_for_mode(path, mode) {
        Ok(data) => bytes_for_diff(&data),
        Err(_) => (SideBytes::from_bytes(Vec::new()), false),
    }
}

/// Convert bytes to `(SideBytes, is_binary)`. NUL in the first 8 KiB ⇒ binary.
fn bytes_for_diff(data: &[u8]) -> (SideBytes, bool) {
    if data.iter().take(8000).any(|&b| b == 0) {
        (SideBytes::from_bytes(Vec::new()), true)
    } else {
        (SideBytes::from_bytes(data.to_vec()), false)
    }
}

fn status_str(status: DiffStatus) -> &'static str {
    crate::output::change_status_str(&status)
}

fn build_file_diff(fc: FileChange) -> FileDiff {
    let structured = structured_file_diff(
        &fc.old_mode,
        &fc.new_mode,
        &fc.old_bytes,
        &fc.new_bytes,
        StructuredDiffOptions {
            context_lines: CONTEXT_LINES,
        },
    );
    let hunks = structured
        .hunks
        .into_iter()
        .map(|h| Hunk {
            old_start: h.old_start,
            new_start: h.new_start,
            old_lines: h.old_lines,
            new_lines: h.new_lines,
            context: h.context,
            lines: h
                .lines
                .into_iter()
                .map(|l| Line {
                    kind: match l.kind {
                        StructuredLineKind::Context => LineKind::Context,
                        StructuredLineKind::Add => LineKind::Add,
                        StructuredLineKind::Del => LineKind::Del,
                    },
                    old: l.old,
                    new: l.new,
                    segments: l
                        .segments
                        .into_iter()
                        .map(|s| Segment {
                            text: s.text,
                            emphasis: s.emphasis,
                        })
                        .collect(),
                    no_newline_at_eof: l.no_newline_at_eof,
                })
                .collect(),
        })
        .collect();

    FileDiff {
        path: fc.path,
        old_path: fc.old_path,
        status: status_str(fc.status).to_owned(),
        old_mode: structured.old_mode,
        new_mode: structured.new_mode,
        old_encoding_lossy: structured.old_encoding_lossy,
        new_encoding_lossy: structured.new_encoding_lossy,
        binary: fc.binary,
        content_changed: fc.content_changed,
        hunks,
    }
}

// --- Human (delta-style) rendering -----------------------------------------

impl HumanRender for DiffOutcome {
    fn render_human(&self, w: &mut dyn Write, _ctx: &RenderContext) -> Result<()> {
        if self.files.is_empty() {
            outln!(w, "No changes.");
            return Ok(());
        }
        let color = use_color();
        for file in &self.files {
            render_file(w, file, color)?;
        }
        Ok(())
    }
}

impl MarkdownRender for DiffOutcome {
    fn render_markdown(&self, w: &mut dyn Write, _ctx: &RenderContext) -> Result<()> {
        markdown::print_diff_files(w, &self.files)?;
        Ok(())
    }
}

/// Color only on a TTY with `NO_COLOR` unset (<https://no-color.org>), and only
/// when the console can render ANSI (always on Unix; VT-capable on Windows).
fn use_color() -> bool {
    std::io::stdout().is_terminal()
        && grit_lib::terminal::ansi_supported()
        && std::env::var_os("NO_COLOR").is_none()
}

fn paint(color: bool, code: &str, text: &str) -> String {
    if color {
        format!("\x1b[{code}m{text}{RESET}")
    } else {
        text.to_owned()
    }
}

fn render_file(w: &mut dyn Write, file: &FileDiff, color: bool) -> Result<()> {
    outln!(w);
    let header = match &file.old_path {
        Some(old) => format!("{old} → {}", file.path),
        None => file.path.clone(),
    };
    outln!(w, "{}", paint(color, "1;4", &header)); // bold + underline

    if file.old_mode != file.new_mode {
        if let Some(m) = &file.old_mode {
            outln!(w, "old mode {m}");
        }
        if let Some(m) = &file.new_mode {
            outln!(w, "new mode {m}");
        }
    }

    if file.binary {
        if file.content_changed {
            outln!(w, "{}", paint(color, FG_DIM, "Binary file differs"));
        }
        return Ok(());
    }

    // Width of each line-number column = widest number shown.
    let width = file
        .hunks
        .iter()
        .flat_map(|h| h.lines.iter())
        .flat_map(|l| [l.old, l.new])
        .flatten()
        .max()
        .map_or(1, |n| n.to_string().len())
        .max(2);

    for hunk in &file.hunks {
        render_hunk(w, hunk, width, color)?;
    }
    Ok(())
}

fn render_hunk(w: &mut dyn Write, hunk: &Hunk, width: usize, color: bool) -> Result<()> {
    let old_rng = format_hunk_range(hunk.old_start, hunk.old_lines);
    let new_rng = format_hunk_range(hunk.new_start, hunk.new_lines);
    let range = format!("@@ -{old_rng} +{new_rng} @@");
    match (&hunk.context, color) {
        (Some(ctx), true) => {
            outln!(w, "{}", paint(color, "33", &format!("┄┄ {range} {ctx}")));
        }
        (Some(ctx), false) => {
            outln!(w, "{range} {ctx}");
        }
        (None, false) => {
            outln!(w, "{range}");
        }
        (None, true) => {
            outln!(w, "{}", paint(color, "33", &format!("┄┄ {range}")));
        }
    }

    for line in &hunk.lines {
        outln!(w, "{}", render_line(line, width, color));
        if line.no_newline_at_eof {
            let marker = "\\ No newline at end of file";
            outln!(w, "{}", paint(color, FG_DIM, marker));
        }
    }
    Ok(())
}

fn render_line(line: &Line, width: usize, color: bool) -> String {
    let (base_bg, emph_bg, num_code, sign) = match line.kind {
        LineKind::Del => (BG_DEL, BG_DEL_EMPH, FG_DEL_NUM, '-'),
        LineKind::Add => (BG_ADD, BG_ADD_EMPH, FG_ADD_NUM, '+'),
        LineKind::Context => ("", "", FG_DIM, ' '),
    };

    // Gutter: two right-aligned line-number columns (changed side colored).
    let old_col = num_cell(line.old, width, num_code, line.kind == LineKind::Del, color);
    let new_col = num_cell(line.new, width, num_code, line.kind == LineKind::Add, color);
    let bar = paint(color, FG_DIM, "│");
    let gutter = format!("{old_col} {new_col} {bar} ");

    if !color {
        // Plain text: lead with a +/-/space marker so piped output is readable.
        let raw: String = line.segments.iter().map(|s| s.text.as_str()).collect();
        return format!("{gutter}{sign} {}", expand_tabs(&raw, 0));
    }

    // Colored: paint the background behind the text only (brighter for the
    // emphasized word-level changes). We deliberately do NOT pad to a fixed
    // width — padding past the terminal edge would wrap onto extra rows and
    // render those as blank colored bands.
    let mut body = String::new();
    let mut col = 0;
    for seg in &line.segments {
        let text = expand_tabs(&seg.text, col);
        col += text.chars().count();
        if base_bg.is_empty() {
            body.push_str(&text);
        } else {
            // Pair the dark background with an explicit light foreground so the
            // text is legible regardless of the terminal's theme.
            let bg = if seg.emphasis { emph_bg } else { base_bg };
            body.push_str(&format!("\x1b[{FG_ON_DIFF};{bg}m{text}"));
        }
    }
    if !base_bg.is_empty() {
        body.push_str(RESET);
    }
    format!("{gutter}{body}")
}

/// Render one line-number cell, right-aligned to `width`, colored when the line
/// is the changed side; blank when the number is absent.
fn num_cell(n: Option<usize>, width: usize, code: &str, changed: bool, color: bool) -> String {
    match n {
        Some(n) => {
            let s = format!("{n:>width$}");
            if color {
                paint(true, if changed { code } else { FG_DIM }, &s)
            } else {
                s
            }
        }
        None => " ".repeat(width),
    }
}

/// Expand tabs to the next [`TAB_WIDTH`] stop, given the starting column.
fn expand_tabs(s: &str, start_col: usize) -> String {
    let mut out = String::new();
    let mut col = start_col;
    for ch in s.chars() {
        if ch == '\t' {
            let n = TAB_WIDTH - (col % TAB_WIDTH);
            out.extend(std::iter::repeat_n(' ', n));
            col += n;
        } else {
            out.push(ch);
            col += 1;
        }
    }
    out
}

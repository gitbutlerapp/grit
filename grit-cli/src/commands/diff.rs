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
    apply_rename_detection_to_diff_entries, diff_commits_with_renames_and_policy,
    diff_index_to_tree_with_renames, diff_index_to_worktree_with_renames,
    diff_trees_with_renames_and_policy, read_submodule_head_oid, read_worktree_side_bytes_for_diff,
    structured::{
        format_hunk_range, structured_file_diff, SideBytes, StructuredFileDiff, StructuredLineKind,
    },
    submodule_porcelain_flags, tree_diff_structured_parallel, write_patch, write_patch_structured,
    DiffEntry, DiffIndexToWorktreeOptions, DiffOptions, DiffStatus, PatchTextOptions,
    TreeDiffStructured,
};
use grit_lib::index::Index;
use grit_lib::objects::odb::Odb;
use grit_lib::objects::ObjectId;
use grit_lib::parallel::ParallelOp;
use grit_lib::porcelain::pathspec_match::{
    ensure_pathspec_matches_known_in_repository, pathspec_matches_known_in_repository,
};
use grit_lib::revwalk::merge_base::merge_base_for_diff_two_commits;
use grit_lib::revwalk::rev_parse::{
    peel_to_commit_for_merge_base, resolve_revision, resolve_revision_for_range_end,
    split_double_dot_range, split_triple_dot_range,
};
use grit_lib::state::resolve_head;
use grit_lib::worktree::fsmonitor::fsmonitor_enabled;
use grit_lib::worktree::pathspec::{
    pathdiff, resolve_pathspec, resolve_pathspec_in_worktree_with_git_dir,
};
use grit_lib::worktree::worktree_rules::WorktreeRules;
use schemars::JsonSchema;
use serde::Serialize;
use std::io::IsTerminal;
use std::io::Write;
use std::path::Path;

use crate::context;
use crate::markdown;
use crate::output::{outln, ChangeStatus, HumanRender, MarkdownRender, RenderContext};
use crate::ui::{self, PathDisplayOptions};

/// Git `-M` rename similarity threshold (matches commit-to-commit diffs in this command).
fn rename_threshold(config: &grit_lib::config::ConfigSet) -> u32 {
    DiffOptions::from_config(config)
        .renames
        .threshold_percent()
        .unwrap_or(101)
}

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
#[derive(Serialize, JsonSchema)]
pub struct DiffOutcome {
    pub files: Vec<FileDiff>,
    #[serde(skip)]
    #[schemars(skip)]
    path_display: Option<PathDisplayOptions>,
}

/// One changed file.
#[derive(Serialize, JsonSchema)]
pub struct FileDiff {
    /// Display path (the new path, or the old path for a deletion).
    pub path: String,
    /// Pre-rename path, when different from `path`.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schemars(skip_serializing_if = "Option::is_none")]
    pub old_path: Option<String>,
    pub status: ChangeStatus,
    /// Old git mode (octal), when present on this side.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schemars(skip_serializing_if = "Option::is_none")]
    pub old_mode: Option<String>,
    /// New git mode (octal), when present on this side.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schemars(skip_serializing_if = "Option::is_none")]
    pub new_mode: Option<String>,
    /// True when old side bytes were decoded with lossy UTF-8.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    #[schemars(default)]
    pub old_encoding_lossy: bool,
    /// True when new side bytes were decoded with lossy UTF-8.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    #[schemars(default)]
    pub new_encoding_lossy: bool,
    pub binary: bool,
    /// True when the blob OID changed (not a mode-only touch on the same content).
    #[serde(skip_serializing)]
    #[schemars(skip)]
    pub content_changed: bool,
    pub hunks: Vec<Hunk>,
}

/// A contiguous run of changes plus surrounding context.
#[derive(Serialize, JsonSchema)]
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
    #[schemars(skip_serializing_if = "Option::is_none")]
    pub context: Option<String>,
    pub lines: Vec<Line>,
}

/// JSON schema shape for [`Line`] (`text` or `segments`, never both).
#[derive(JsonSchema)]
#[allow(dead_code)]
struct LineJsonSchema {
    kind: LineKind,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schemars(skip_serializing_if = "Option::is_none")]
    old: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schemars(skip_serializing_if = "Option::is_none")]
    new: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schemars(skip_serializing_if = "Option::is_none")]
    text: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schemars(skip_serializing_if = "Option::is_none")]
    segments: Option<Vec<Segment>>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    #[schemars(default)]
    no_newline_at_eof: bool,
}

impl JsonSchema for Line {
    fn schema_name() -> String {
        "Line".to_string()
    }

    fn json_schema(gen: &mut schemars::gen::SchemaGenerator) -> schemars::schema::Schema {
        LineJsonSchema::json_schema(gen)
    }
}

/// One rendered line of a hunk.
pub struct Line {
    pub kind: LineKind,
    /// 1-based old/new line numbers (absent on the side where the line is new/gone).
    pub old: Option<usize>,
    pub new: Option<usize>,
    /// The line split into segments; `emphasis` marks the intra-line word changes.
    pub segments: Vec<Segment>,
    /// Missing newline at end of file on this side (Git `\ No newline at end of file`).
    pub no_newline_at_eof: bool,
}

impl Serialize for Line {
    fn serialize<S>(&self, serializer: S) -> std::result::Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        use serde::ser::SerializeStruct;
        let field_count = 3
            + usize::from(self.old.is_some())
            + usize::from(self.new.is_some())
            + usize::from(self.no_newline_at_eof)
            + 1;
        let mut state = serializer.serialize_struct("Line", field_count)?;
        state.serialize_field("kind", &self.kind)?;
        if let Some(old) = self.old {
            state.serialize_field("old", &old)?;
        }
        if let Some(new) = self.new {
            state.serialize_field("new", &new)?;
        }
        if segments_have_emphasis(&self.segments) {
            state.serialize_field("segments", &self.segments)?;
        } else {
            state.serialize_field("text", &line_text(&self.segments))?;
        }
        if self.no_newline_at_eof {
            state.serialize_field("no_newline_at_eof", &true)?;
        }
        state.end()
    }
}

fn line_text(segments: &[Segment]) -> String {
    segments.iter().map(|s| s.text.as_str()).collect()
}

fn segments_have_emphasis(segments: &[Segment]) -> bool {
    segments.iter().any(|s| s.emphasis)
}

#[derive(Serialize, JsonSchema, Clone, Copy, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum LineKind {
    Context,
    Add,
    Del,
}

/// A run of text within a line, flagged if it's part of the word-level change.
#[derive(Serialize, JsonSchema)]
pub struct Segment {
    pub text: String,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    #[schemars(default)]
    pub emphasis: bool,
}

/// Inputs for `grit diff` (flags and revision arguments).
pub struct DiffRunOptions {
    pub staged: bool,
    pub unstaged: bool,
    pub against: Option<String>,
    pub commits: Vec<String>,
    /// Pathspecs from trailing `PATH` arguments (after `--`).
    pub paths: Vec<String>,
    /// Intra-line word highlights in structured hunks (human TTY output only).
    pub word_level_inline: bool,
    /// Emit `git diff --binary`-compatible patch text for `git apply`.
    #[expect(dead_code)]
    pub patch_text: bool,
}

/// Whether human diff output should compute word-level inline emphasis.
#[must_use]
pub fn diff_word_level_inline_for_output() -> bool {
    std::io::stdout().is_terminal()
        && grit_lib::porcelain::terminal::ansi_supported()
        && std::env::var_os("NO_COLOR").is_none()
}

fn attach_path_display(repo: &grit_lib::repo::Repository, mut outcome: DiffOutcome) -> DiffOutcome {
    outcome.path_display = PathDisplayOptions::from_repo(repo).ok();
    outcome
}

/// Write `git apply`-compatible patch text to stdout (`--patch-text`).
pub fn run_patch_text(opts: DiffRunOptions) -> Result<()> {
    let repo = context::discover()?;
    let config = repo.config().context("could not load config")?;
    let DiffRunOptions {
        staged,
        unstaged,
        against,
        commits,
        paths,
        word_level_inline: _,
        patch_text: _,
    } = opts;
    let (commits, path_args) = disambiguate_diff_revision_and_paths(&repo, commits, paths);
    let run_opts = DiffRunOptions {
        staged,
        unstaged,
        against,
        commits: commits.clone(),
        paths: path_args.clone(),
        word_level_inline: false,
        patch_text: true,
    };
    let mut diff_options = DiffOptions::from_config(config.as_ref());
    diff_options.context_lines = CONTEXT_LINES;
    diff_options.word_level_inline = false;
    let patch_opts = PatchTextOptions::from_repo(&repo, diff_options)?;
    let mut out = std::io::stdout();
    if let Some(structured) =
        collect_structured_patch_items(&repo, &run_opts, &commits, &path_args, diff_options)?
    {
        write_patch_structured(&repo, &structured, &patch_opts, &mut out)
            .context("could not write patch text")?;
    } else {
        let entries = collect_diff_entries(&repo, &run_opts, &commits, &path_args, diff_options)?;
        write_patch(&repo, &entries, &patch_opts, &mut out)
            .context("could not write patch text")?;
    }
    Ok(())
}

/// Precomputed structured rows for commit-tree diffs (avoids a second line-diff pass in `--patch-text`).
fn collect_structured_patch_items(
    repo: &grit_lib::repo::Repository,
    opts: &DiffRunOptions,
    commits: &[String],
    path_args: &[String],
    diff_options: DiffOptions,
) -> Result<Option<Vec<TreeDiffStructured>>> {
    if opts.staged || opts.unstaged || opts.against.is_some() || commits.is_empty() {
        return Ok(None);
    }
    let config = repo.config().context("could not load config")?;
    let thread_policy = repo.thread_policy();
    if commits.len() > 2 {
        return Ok(None);
    }
    let items = match commits.len() {
        1 => {
            let spec = &commits[0];
            if let Some((base, tip)) = parse_commit_range(repo, spec)? {
                let pathspecs = resolve_commit_diff_pathspecs(repo, path_args)?;
                filter_tree_diff(
                    tree_commit_changes(
                        repo,
                        &base,
                        &tip,
                        config.as_ref(),
                        &thread_policy,
                        diff_options,
                    )?,
                    repo,
                    &pathspecs,
                )
            } else if let Ok(oid) = resolve_diff_commit(repo, spec) {
                let pathspecs = resolve_commit_diff_pathspecs(repo, path_args)?;
                filter_tree_diff(
                    commit_changes(repo, &oid, config.as_ref(), &thread_policy, diff_options)?,
                    repo,
                    &pathspecs,
                )
            } else {
                return Ok(None);
            }
        }
        2 => {
            if let Some((base, tip)) = parse_commit_range(repo, &commits[0])? {
                if token_is_known_worktree_pathspec(repo, &commits[1]) {
                    let pathspecs =
                        resolve_commit_diff_pathspecs(repo, std::slice::from_ref(&commits[1]))?;
                    filter_tree_diff(
                        tree_commit_changes(
                            repo,
                            &base,
                            &tip,
                            config.as_ref(),
                            &thread_policy,
                            diff_options,
                        )?,
                        repo,
                        &pathspecs,
                    )
                } else {
                    return Ok(None);
                }
            } else {
                let pathspecs = resolve_commit_diff_pathspecs(repo, path_args)?;
                let base = resolve_diff_commit(repo, &commits[0])?;
                let tip = resolve_diff_commit(repo, &commits[1])?;
                filter_tree_diff(
                    tree_commit_changes(
                        repo,
                        &base,
                        &tip,
                        config.as_ref(),
                        &thread_policy,
                        diff_options,
                    )?,
                    repo,
                    &pathspecs,
                )
            }
        }
        _ => return Ok(None),
    };
    Ok(Some(items))
}

#[allow(clippy::needless_return)]
fn collect_diff_entries(
    repo: &grit_lib::repo::Repository,
    opts: &DiffRunOptions,
    commits: &[String],
    path_args: &[String],
    _diff_options: DiffOptions,
) -> Result<Vec<DiffEntry>> {
    let config = repo.config().context("could not load config")?;
    let thread_policy = repo.thread_policy();
    if opts.staged && opts.unstaged {
        anyhow::bail!("--staged and --unstaged cannot be combined");
    }
    if opts.staged {
        if !commits.is_empty() || opts.against.is_some() {
            anyhow::bail!("--staged cannot be combined with revision arguments or --against");
        }
        let pathspecs = resolve_worktree_diff_pathspecs(repo, path_args)?;
        return Ok(filter_diff_entries(
            staged_diff_entries(repo)?,
            repo,
            &pathspecs,
        ));
    }
    if opts.unstaged {
        if !commits.is_empty() || opts.against.is_some() {
            anyhow::bail!("--unstaged cannot be combined with revision arguments or --against");
        }
        let pathspecs = resolve_worktree_diff_pathspecs(repo, path_args)?;
        return Ok(filter_diff_entries(
            unstaged_diff_entries(repo)?,
            repo,
            &pathspecs,
        ));
    }
    if commits.len() > 2 {
        anyhow::bail!("at most two commits, or one two-dot range such as A..B");
    }
    if opts.against.is_some() && !commits.is_empty() {
        anyhow::bail!("--against compares the worktree to a ref and takes no commit arguments");
    }
    match commits.len() {
        0 => {
            let pathspecs = resolve_worktree_diff_pathspecs(repo, path_args)?;
            let entries = if let Some(spec) = opts.against.as_deref() {
                let commit = resolve_diff_commit(repo, spec)?;
                let base_tree = context::commit_tree(repo, &commit)?;
                worktree_against_tree_entries(repo, Some(&base_tree))?
            } else {
                let head = resolve_head(repo.git_dir()).context("could not resolve HEAD")?;
                let head_tree = match head.oid() {
                    Some(oid) => Some(context::commit_tree(repo, oid)?),
                    None => None,
                };
                worktree_against_tree_entries(repo, head_tree.as_ref())?
            };
            return Ok(filter_diff_entries(entries, repo, &pathspecs));
        }
        1 => {
            let spec = &commits[0];
            if let Some((base, tip)) = parse_commit_range(repo, spec)? {
                let pathspecs = resolve_commit_diff_pathspecs(repo, path_args)?;
                return Ok(filter_diff_entries(
                    tree_commit_diff_entries(repo, &base, &tip, config.as_ref(), &thread_policy)?,
                    repo,
                    &pathspecs,
                ));
            }
            if let Ok(oid) = resolve_diff_commit(repo, spec) {
                let pathspecs = resolve_commit_diff_pathspecs(repo, path_args)?;
                return Ok(filter_diff_entries(
                    commit_diff_entries(repo, &oid, config.as_ref(), &thread_policy)?,
                    repo,
                    &pathspecs,
                ));
            }
            if path_args.is_empty() {
                let pathspecs = resolve_worktree_diff_pathspecs(repo, std::slice::from_ref(spec))?;
                return Ok(filter_diff_entries(
                    unstaged_diff_entries(repo)?,
                    repo,
                    &pathspecs,
                ));
            }
            resolve_diff_commit(repo, spec)?;
            unreachable!("resolve_diff_commit errors above");
        }
        2 => {
            if let Some((base, tip)) = parse_commit_range(repo, &commits[0])? {
                if token_is_known_worktree_pathspec(repo, &commits[1]) {
                    let pathspecs =
                        resolve_commit_diff_pathspecs(repo, std::slice::from_ref(&commits[1]))?;
                    return Ok(filter_diff_entries(
                        tree_commit_diff_entries(
                            repo,
                            &base,
                            &tip,
                            config.as_ref(),
                            &thread_policy,
                        )?,
                        repo,
                        &pathspecs,
                    ));
                }
                match resolve_diff_commit(repo, &commits[1]) {
                    Ok(_) => anyhow::bail!(
                        "a two-dot or three-dot range uses the other positional as a path limit; pass paths after `--`"
                    ),
                    Err(e) => return Err(e),
                }
            }
            let pathspecs = resolve_commit_diff_pathspecs(repo, path_args)?;
            let base = resolve_diff_commit(repo, &commits[0])?;
            let tip = resolve_diff_commit(repo, &commits[1])?;
            return Ok(filter_diff_entries(
                tree_commit_diff_entries(repo, &base, &tip, config.as_ref(), &thread_policy)?,
                repo,
                &pathspecs,
            ));
        }
        _ => unreachable!("checked above"),
    }
}

pub fn run(opts: DiffRunOptions) -> Result<DiffOutcome> {
    let repo = context::discover()?;
    let config = repo.config().context("could not load config")?;
    let thread_policy = repo.thread_policy();
    let (commits, path_args) =
        disambiguate_diff_revision_and_paths(&repo, opts.commits, opts.paths);
    let mut diff_options = DiffOptions::from_config(config.as_ref());
    diff_options.context_lines = CONTEXT_LINES;
    diff_options.word_level_inline = opts.word_level_inline;
    if opts.staged && opts.unstaged {
        anyhow::bail!("--staged and --unstaged cannot be combined");
    }
    if opts.staged {
        if !commits.is_empty() || opts.against.is_some() {
            anyhow::bail!("--staged cannot be combined with revision arguments or --against");
        }
        let pathspecs = resolve_worktree_diff_pathspecs(&repo, &path_args)?;
        return Ok(attach_path_display(
            &repo,
            outcome_from_changes(
                filter_file_changes(staged_changes(&repo)?, &repo, &pathspecs),
                &config,
                &thread_policy,
                diff_options,
            ),
        ));
    }
    if opts.unstaged {
        if !commits.is_empty() || opts.against.is_some() {
            anyhow::bail!("--unstaged cannot be combined with revision arguments or --against");
        }
        let pathspecs = resolve_worktree_diff_pathspecs(&repo, &path_args)?;
        return Ok(attach_path_display(
            &repo,
            outcome_from_changes(
                filter_file_changes(unstaged_changes(&repo)?, &repo, &pathspecs),
                &config,
                &thread_policy,
                diff_options,
            ),
        ));
    }
    if commits.len() > 2 {
        anyhow::bail!("at most two commits, or one two-dot range such as A..B");
    }
    if opts.against.is_some() && !commits.is_empty() {
        anyhow::bail!("--against compares the worktree to a ref and takes no commit arguments");
    }
    let changes = match commits.len() {
        0 => {
            let pathspecs = resolve_worktree_diff_pathspecs(&repo, &path_args)?;
            let changes = if let Some(spec) = opts.against {
                against_ref_changes(&repo, &spec)?
            } else {
                worktree_changes(&repo)?
            };
            return Ok(attach_path_display(
                &repo,
                outcome_from_changes(
                    filter_file_changes(changes, &repo, &pathspecs),
                    &config,
                    &thread_policy,
                    diff_options,
                ),
            ));
        }
        1 => {
            let spec = &commits[0];
            if let Some((base, tip)) = parse_commit_range(&repo, spec)? {
                let pathspecs = resolve_commit_diff_pathspecs(&repo, &path_args)?;
                return Ok(attach_path_display(
                    &repo,
                    outcome_from_tree_structured(filter_tree_diff(
                        tree_commit_changes(
                            &repo,
                            &base,
                            &tip,
                            &config,
                            &thread_policy,
                            diff_options,
                        )?,
                        &repo,
                        &pathspecs,
                    )),
                ));
            } else if let Ok(oid) = resolve_diff_commit(&repo, spec) {
                let pathspecs = resolve_commit_diff_pathspecs(&repo, &path_args)?;
                return Ok(attach_path_display(
                    &repo,
                    outcome_from_tree_structured(filter_tree_diff(
                        commit_changes(&repo, &oid, &config, &thread_policy, diff_options)?,
                        &repo,
                        &pathspecs,
                    )),
                ));
            } else if path_args.is_empty() {
                single_argument_path_unstaged_diff(&repo, spec)?
            } else {
                resolve_diff_commit(&repo, spec)?;
                unreachable!("resolve_diff_commit errors above");
            }
        }
        2 => {
            if let Some((base, tip)) = parse_commit_range(&repo, &commits[0])? {
                if token_is_known_worktree_pathspec(&repo, &commits[1]) {
                    let pathspecs =
                        resolve_commit_diff_pathspecs(&repo, std::slice::from_ref(&commits[1]))?;
                    return Ok(attach_path_display(
                        &repo,
                        outcome_from_tree_structured(filter_tree_diff(
                            tree_commit_changes(
                                &repo,
                                &base,
                                &tip,
                                &config,
                                &thread_policy,
                                diff_options,
                            )?,
                            &repo,
                            &pathspecs,
                        )),
                    ));
                }
                match resolve_diff_commit(&repo, &commits[1]) {
                    Ok(_) => anyhow::bail!(
                        "a two-dot or three-dot range uses the other positional as a path limit; pass paths after `--`"
                    ),
                    Err(e) => return Err(e),
                }
            }
            let pathspecs = resolve_commit_diff_pathspecs(&repo, &path_args)?;
            let base = resolve_diff_commit(&repo, &commits[0])?;
            let tip = resolve_diff_commit(&repo, &commits[1])?;
            return Ok(attach_path_display(
                &repo,
                outcome_from_tree_structured(filter_tree_diff(
                    tree_commit_changes(&repo, &base, &tip, &config, &thread_policy, diff_options)?,
                    &repo,
                    &pathspecs,
                )),
            ));
        }
        _ => unreachable!("checked above"),
    };
    let pathspecs = resolve_worktree_diff_pathspecs(&repo, &path_args)?;
    Ok(attach_path_display(
        &repo,
        outcome_from_changes(
            filter_file_changes(changes, &repo, &pathspecs),
            &config,
            &thread_policy,
            diff_options,
        ),
    ))
}

/// The diff a single commit introduced, as a [`DiffOutcome`]. Reused by `grit show`.
pub fn diff_of_commit(repo: &grit_lib::repo::Repository, oid: &ObjectId) -> Result<DiffOutcome> {
    let config = repo.config().context("could not load config")?;
    let thread_policy = repo.thread_policy();
    let mut diff_options = DiffOptions::from_config(config.as_ref());
    diff_options.context_lines = CONTEXT_LINES;
    diff_options.word_level_inline = false;
    Ok(attach_path_display(
        repo,
        outcome_from_tree_structured(commit_changes(
            repo,
            oid,
            &config,
            &thread_policy,
            diff_options,
        )?),
    ))
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
            let (old_bytes, old_bin) = old_side_bytes(repo.odb(), &e)?;
            let (new_bytes, new_bin) = new_side_bytes(repo.odb(), &e, None)?;
            Ok(file_change(e, old_bytes, new_bytes, old_bin || new_bin))
        })
        .collect::<Result<Vec<_>>>()?;
    let config = repo.config().context("could not load config")?;
    let thread_policy = repo.thread_policy();
    let mut diff_options = DiffOptions::from_config(config.as_ref());
    diff_options.context_lines = CONTEXT_LINES;
    diff_options.word_level_inline = false;
    Ok(attach_path_display(
        repo,
        outcome_from_changes(changes, &config, &thread_policy, diff_options),
    ))
}

fn outcome_from_tree_structured(items: Vec<TreeDiffStructured>) -> DiffOutcome {
    DiffOutcome {
        files: items
            .into_iter()
            .map(file_diff_from_tree_structured)
            .collect(),
        path_display: None,
    }
}

fn file_diff_from_tree_structured(t: TreeDiffStructured) -> FileDiff {
    let e = t.entry;
    let path = e
        .new_path
        .clone()
        .or_else(|| e.old_path.clone())
        .unwrap_or_default();
    let old_path = e.old_path.filter(|op| *op != path);
    let structured = t.structured;
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
        path,
        old_path,
        status: e.status.into(),
        old_mode: structured.old_mode,
        new_mode: structured.new_mode,
        old_encoding_lossy: structured.old_encoding_lossy,
        new_encoding_lossy: structured.new_encoding_lossy,
        binary: t.binary,
        content_changed: e.old_oid != e.new_oid,
        hunks,
    }
}

fn outcome_from_changes(
    changes: Vec<FileChange>,
    config: &grit_lib::config::ConfigSet,
    thread_policy: &grit_lib::parallel::ThreadPolicy,
    diff_options: DiffOptions,
) -> DiffOutcome {
    let diff_options = diff_options.cap_word_level_inline_for_changed_files(changes.len());
    let mut par_opts = thread_policy.resolve(ParallelOp::DiffContent, config);
    par_opts.min_items = 0;
    let est_bytes = changes.len().saturating_mul(8192);
    use grit_lib::parallel::par_map;
    let files = par_map(&changes, par_opts, est_bytes, |fc| {
        build_file_diff(fc, diff_options)
    });
    DiffOutcome {
        files,
        path_display: None,
    }
}

/// Worktree-side byte normalization context for unstaged and tree↔worktree diffs.
struct WorktreeDiffSide<'a> {
    work_tree: &'a Path,
    index: &'a Index,
    rules: std::sync::Arc<std::sync::Mutex<WorktreeRules>>,
}

/// A changed file with both sides' text resolved.
#[derive(Clone)]
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

fn token_is_diff_revision(repo: &grit_lib::repo::Repository, token: &str) -> bool {
    resolve_diff_commit(repo, token).is_ok()
        || parse_commit_range(repo, token).ok().flatten().is_some()
}

/// Whether `token` names a path in the index, `HEAD`, or worktree (for `commit path` disambiguation).
fn token_is_known_worktree_pathspec(repo: &grit_lib::repo::Repository, token: &str) -> bool {
    let Some(work_tree) = repo.work_tree() else {
        return false;
    };
    let Ok(cwd) = std::env::current_dir() else {
        return false;
    };
    let prefix = pathdiff(&cwd, work_tree);
    let Ok(resolved) = resolve_pathspec_in_worktree_with_git_dir(
        token,
        token,
        work_tree,
        prefix.as_deref(),
        Some(repo.git_dir()),
    ) else {
        return false;
    };
    pathspec_matches_known_in_repository(repo, &resolved).unwrap_or(false)
}

/// When a path is given without `--`, clap may place it in `commits`; move non-revisions to paths.
fn disambiguate_diff_revision_and_paths(
    repo: &grit_lib::repo::Repository,
    mut commits: Vec<String>,
    mut paths: Vec<String>,
) -> (Vec<String>, Vec<String>) {
    if paths.is_empty() && commits.len() == 2 {
        let first_rev = token_is_diff_revision(repo, &commits[0]);
        let second_rev = token_is_diff_revision(repo, &commits[1]);
        match (first_rev, second_rev) {
            (true, false) if token_is_known_worktree_pathspec(repo, &commits[1]) => {
                if let Some(path) = commits.pop() {
                    paths.push(path);
                }
            }
            (true, true) => {}
            (true, false) | (false, true) | (false, false) => {}
        }
    } else if paths.is_empty() {
        while commits.len() >= 2 {
            let Some(tail) = commits.last() else {
                break;
            };
            if token_is_diff_revision(repo, tail) {
                break;
            }
            let Some(path) = commits.pop() else {
                break;
            };
            paths.push(path);
        }
    }
    (commits, paths)
}

fn pathspec_prefix_for_repo(repo: &grit_lib::repo::Repository) -> Result<Option<String>> {
    let Some(work_tree) = repo.work_tree() else {
        return Ok(None);
    };
    let cwd = std::env::current_dir().context("could not read the current directory")?;
    Ok(pathdiff(&cwd, work_tree))
}

/// Root used to normalize literal pathspec segments (work tree, or bare `git_dir`).
fn pathspec_resolution_root(repo: &grit_lib::repo::Repository) -> &Path {
    repo.work_tree().unwrap_or_else(|| repo.git_dir())
}

/// Worktree diffs: paths must exist in the index, `HEAD`, or on disk.
fn resolve_worktree_diff_pathspecs(
    repo: &grit_lib::repo::Repository,
    raw_paths: &[String],
) -> Result<Vec<String>> {
    if raw_paths.is_empty() {
        return Ok(Vec::new());
    }
    let work_tree = repo.work_tree().context("grit diff needs a working tree")?;
    let prefix = pathspec_prefix_for_repo(repo)?;
    let mut specs = Vec::with_capacity(raw_paths.len());
    for pathspec in raw_paths {
        let resolved = resolve_pathspec_in_worktree_with_git_dir(
            pathspec,
            pathspec,
            work_tree,
            prefix.as_deref(),
            Some(repo.git_dir()),
        )
        .map_err(|e| anyhow::anyhow!("{e}"))?;
        ensure_pathspec_matches_known_in_repository(repo, pathspec, &resolved)
            .map_err(|e| anyhow::anyhow!("{e}"))?;
        specs.push(resolved);
    }
    Ok(specs)
}

/// Commit and range diffs: normalize pathspecs repository-relative without requiring a worktree
/// or a match against the current checkout (deleted-on-tip paths and exclude magic are allowed).
fn resolve_commit_diff_pathspecs(
    repo: &grit_lib::repo::Repository,
    raw_paths: &[String],
) -> Result<Vec<String>> {
    if raw_paths.is_empty() {
        return Ok(Vec::new());
    }
    let root = pathspec_resolution_root(repo);
    let prefix = pathspec_prefix_for_repo(repo)?;
    let mut specs = Vec::with_capacity(raw_paths.len());
    for pathspec in raw_paths {
        if let Some(work_tree) = repo.work_tree() {
            let resolved = resolve_pathspec_in_worktree_with_git_dir(
                pathspec,
                pathspec,
                work_tree,
                prefix.as_deref(),
                Some(repo.git_dir()),
            )
            .map_err(|e| anyhow::anyhow!("{e}"))?;
            specs.push(resolved);
        } else {
            specs.push(resolve_pathspec(pathspec, root, prefix.as_deref()));
        }
    }
    Ok(specs)
}

fn file_change_matches_pathspecs(
    repo: &grit_lib::repo::Repository,
    path: &str,
    old_path: Option<&str>,
    specs: &[String],
) -> bool {
    if specs.is_empty() {
        return true;
    }
    if repo.paths_match_pathspecs(path, specs) {
        return true;
    }
    if let Some(op) = old_path {
        if op != path && repo.paths_match_pathspecs(op, specs) {
            return true;
        }
    }
    false
}

fn filter_file_changes(
    changes: Vec<FileChange>,
    repo: &grit_lib::repo::Repository,
    specs: &[String],
) -> Vec<FileChange> {
    if specs.is_empty() {
        return changes;
    }
    changes
        .into_iter()
        .filter(|c| file_change_matches_pathspecs(repo, &c.path, c.old_path.as_deref(), specs))
        .collect()
}

fn filter_diff_entries(
    entries: Vec<DiffEntry>,
    repo: &grit_lib::repo::Repository,
    specs: &[String],
) -> Vec<DiffEntry> {
    if specs.is_empty() {
        return entries;
    }
    entries
        .into_iter()
        .filter(|e| {
            let path = e
                .new_path
                .as_deref()
                .or(e.old_path.as_deref())
                .unwrap_or("");
            let old_path = e.old_path.as_deref();
            file_change_matches_pathspecs(repo, path, old_path, specs)
        })
        .collect()
}

fn filter_tree_diff(
    items: Vec<TreeDiffStructured>,
    repo: &grit_lib::repo::Repository,
    specs: &[String],
) -> Vec<TreeDiffStructured> {
    if specs.is_empty() {
        return items;
    }
    items
        .into_iter()
        .filter(|t| {
            let e = &t.entry;
            let path = e
                .new_path
                .as_deref()
                .or(e.old_path.as_deref())
                .unwrap_or("");
            let old_path = e.old_path.as_deref();
            file_change_matches_pathspecs(repo, path, old_path, specs)
        })
        .collect()
}

/// Unstaged diff for one pathspec argument when revision parsing failed (`git diff <path>`).
fn single_argument_path_unstaged_diff(
    repo: &grit_lib::repo::Repository,
    pathspec: &str,
) -> Result<Vec<FileChange>> {
    let work_tree = repo.work_tree().context("grit diff needs a working tree")?;
    let cwd = std::env::current_dir().context("could not read the current directory")?;
    let prefix = pathdiff(&cwd, work_tree);
    let resolved = resolve_pathspec_in_worktree_with_git_dir(
        pathspec,
        pathspec,
        work_tree,
        prefix.as_deref(),
        Some(repo.git_dir()),
    )
    .map_err(|e| anyhow::anyhow!("{e}"))?;
    ensure_pathspec_matches_known_in_repository(repo, pathspec, &resolved)
        .map_err(|e| anyhow::anyhow!("{e}"))?;
    let specs = vec![resolved];
    let all = unstaged_changes(repo)?;
    Ok(all
        .into_iter()
        .filter(|c| repo.paths_match_pathspecs(&c.path, &specs))
        .collect())
}

/// Unstaged changes: worktree vs index (what changed on disk since `git add`).
fn unstaged_diff_entries(repo: &grit_lib::repo::Repository) -> Result<Vec<DiffEntry>> {
    let work_tree = repo.work_tree().context("grit diff needs a working tree")?;
    let mut index = repo
        .index_handle()
        .load()
        .context("could not load the index")?;
    let rules_arc = std::sync::Arc::new(std::sync::Mutex::new(
        WorktreeRules::from_repository(repo, &index).context("could not load worktree rules")?,
    ));
    let config_arc = rules_arc
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .config_arc();
    let diff_index_mtime = grit_lib::diff::index_mtime_for_diff(&index, None);
    let diff_opts = DiffIndexToWorktreeOptions {
        repository_git_dir: Some(repo.git_dir().to_path_buf()),
        index_mtime: diff_index_mtime,
        config: Some(config_arc.clone()),
        thread_policy: Some(repo.thread_policy()),
        refresh_index_stat_in_pass: false,
        for_status: true,
        refresh_fsmonitor: fsmonitor_enabled(config_arc.as_ref()),
        worktree_rules: Some(std::sync::Arc::clone(&rules_arc)),
        command_runner: Some(repo.command_runner()),
        ..DiffIndexToWorktreeOptions::default()
    };
    let (mut entries, _) = diff_index_to_worktree_with_renames(
        repo,
        &mut index,
        work_tree,
        diff_opts,
        rename_threshold(config_arc.as_ref()),
    )
    .context("could not diff the index against the working tree")?;
    sort_entries(&mut entries);
    Ok(entries)
}

fn unstaged_changes(repo: &grit_lib::repo::Repository) -> Result<Vec<FileChange>> {
    let work_tree = repo.work_tree().context("grit diff needs a working tree")?;
    let index = repo
        .index_handle()
        .load()
        .context("could not load the index")?;
    let rules_arc = std::sync::Arc::new(std::sync::Mutex::new(
        WorktreeRules::from_repository(repo, &index).context("could not load worktree rules")?,
    ));
    let side = WorktreeDiffSide {
        work_tree,
        index: &index,
        rules: rules_arc,
    };
    unstaged_diff_entries(repo)?
        .into_iter()
        .map(|e| {
            let (old_bytes, old_bin) = old_side_bytes(repo.odb(), &e)?;
            let (new_bytes, new_bin) = new_side_bytes(repo.odb(), &e, Some(&side))?;
            Ok(file_change(e, old_bytes, new_bytes, old_bin || new_bin))
        })
        .collect()
}

/// Staged changes: index vs HEAD's tree.
fn staged_diff_entries(repo: &grit_lib::repo::Repository) -> Result<Vec<DiffEntry>> {
    let head = resolve_head(repo.git_dir()).context("could not resolve HEAD")?;
    let head_tree = match head.oid() {
        Some(oid) => Some(context::commit_tree(repo, oid)?),
        None => None,
    };
    let index = repo
        .index_handle()
        .load()
        .context("could not load the index")?;
    let config = repo.config().context("could not load config")?;
    let thread_policy = repo.thread_policy();
    let mut entries = diff_index_to_tree_with_renames(
        repo.odb(),
        &index,
        head_tree.as_ref(),
        false,
        rename_threshold(config.as_ref()),
        config.as_ref(),
        &thread_policy,
    )
    .context("could not diff the index against HEAD")?;
    sort_entries(&mut entries);
    Ok(entries)
}

fn staged_changes(repo: &grit_lib::repo::Repository) -> Result<Vec<FileChange>> {
    staged_diff_entries(repo)?
        .into_iter()
        .map(|e| {
            let (old_bytes, old_bin) = old_side_bytes(repo.odb(), &e)?;
            let (new_bytes, new_bin) = new_side_bytes(repo.odb(), &e, None)?;
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
    config: &grit_lib::config::ConfigSet,
    thread_policy: &grit_lib::parallel::ThreadPolicy,
    diff_options: DiffOptions,
) -> Result<Vec<TreeDiffStructured>> {
    let mut entries = diff_commits_with_renames_and_policy(
        repo.odb(),
        base,
        tip,
        rename_threshold(config),
        config,
        thread_policy,
    )
    .context("could not diff commits")?;
    sort_entries(&mut entries);
    tree_diff_structured_parallel(repo.odb(), entries, config, thread_policy, diff_options)
        .context("could not build structured diff")
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
                grit_lib::revwalk::merge_base::MergeBaseForDiffError::None => {
                    "no merge base for that revision range".to_owned()
                }
                grit_lib::revwalk::merge_base::MergeBaseForDiffError::Multiple => {
                    "multiple merge bases for that revision range".to_owned()
                }
                grit_lib::revwalk::merge_base::MergeBaseForDiffError::Other(msg) => msg,
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
    let head = resolve_head(repo.git_dir()).context("could not resolve HEAD")?;
    let head_tree = match head.oid() {
        Some(oid) => Some(context::commit_tree(repo, oid)?),
        None => None,
    };
    worktree_against_tree(repo, head_tree.as_ref())
}

fn tree_commit_diff_entries(
    repo: &grit_lib::repo::Repository,
    base: &ObjectId,
    tip: &ObjectId,
    config: &grit_lib::config::ConfigSet,
    thread_policy: &grit_lib::parallel::ThreadPolicy,
) -> Result<Vec<DiffEntry>> {
    let mut entries = diff_commits_with_renames_and_policy(
        repo.odb(),
        base,
        tip,
        rename_threshold(config),
        config,
        thread_policy,
    )
    .context("could not diff commits")?;
    sort_entries(&mut entries);
    Ok(entries)
}

fn commit_diff_entries(
    repo: &grit_lib::repo::Repository,
    oid: &ObjectId,
    config: &grit_lib::config::ConfigSet,
    thread_policy: &grit_lib::parallel::ThreadPolicy,
) -> Result<Vec<DiffEntry>> {
    let commit = context::read_commit(repo, oid)?;
    let new_tree = commit.tree;
    let old_tree = match commit.parents.first() {
        Some(parent) => Some(context::commit_tree(repo, parent)?),
        None => None,
    };
    let mut entries = diff_trees_with_renames_and_policy(
        repo.odb(),
        old_tree.as_ref(),
        Some(&new_tree),
        "",
        rename_threshold(config),
        config,
        thread_policy,
    )
    .context("could not diff the commit")?;
    sort_entries(&mut entries);
    Ok(entries)
}

fn worktree_against_tree(
    repo: &grit_lib::repo::Repository,
    base_tree: Option<&ObjectId>,
) -> Result<Vec<FileChange>> {
    let entries = worktree_against_tree_entries(repo, base_tree)?;
    let work_tree = repo.work_tree().context("grit diff needs a working tree")?;
    let index = repo
        .index_handle()
        .load()
        .context("could not load the index")?;
    let rules_arc = std::sync::Arc::new(std::sync::Mutex::new(
        WorktreeRules::from_repository(repo, &index).context("could not load worktree rules")?,
    ));
    let side = WorktreeDiffSide {
        work_tree,
        index: &index,
        rules: rules_arc,
    };
    entries
        .into_iter()
        .map(|e| {
            let (old_bytes, old_bin) = old_side_bytes(repo.odb(), &e)?;
            let (new_bytes, new_bin) = new_side_bytes(repo.odb(), &e, Some(&side))?;
            Ok(file_change(e, old_bytes, new_bytes, old_bin || new_bin))
        })
        .collect()
}

fn worktree_against_tree_entries(
    repo: &grit_lib::repo::Repository,
    base_tree: Option<&ObjectId>,
) -> Result<Vec<DiffEntry>> {
    let work_tree = repo.work_tree().context("grit diff needs a working tree")?;
    let mut index = repo
        .index_handle()
        .load()
        .context("could not load the index")?;
    let config = repo.config().context("could not load config")?;

    let index_backed = base_tree
        .zip(index.cache_tree_root.as_ref())
        .map(|(left, right)| left == right)
        .unwrap_or(base_tree.is_none() && index.cache_tree_root.is_none())
        && grit_lib::index::write_tree::cache_tree_extension_valid(index.cache_tree.as_ref());
    let diff_index_mtime = grit_lib::diff::index_mtime_for_diff(&index, None);
    let rules_arc = std::sync::Arc::new(std::sync::Mutex::new(
        WorktreeRules::from_repository(repo, &index).context("could not load worktree rules")?,
    ));
    let mut entries = if index_backed {
        let (entries, _) = diff_index_to_worktree_with_renames(
            repo,
            &mut index,
            work_tree,
            DiffIndexToWorktreeOptions {
                repository_git_dir: Some(repo.git_dir().to_path_buf()),
                config: Some(config.clone()),
                index_mtime: diff_index_mtime,
                thread_policy: Some(repo.thread_policy()),
                refresh_index_stat_in_pass: false,
                for_status: true,
                refresh_fsmonitor: fsmonitor_enabled(config.as_ref()),
                worktree_rules: Some(std::sync::Arc::clone(&rules_arc)),
                command_runner: Some(repo.command_runner()),
                ..DiffIndexToWorktreeOptions::default()
            },
            rename_threshold(config.as_ref()),
        )
        .context("could not diff the working tree")?;
        entries
    } else {
        let worktree_rules =
            grit_lib::worktree::worktree_rules::WorktreeRules::from_repository(repo, &index)
                .context("could not load worktree rules")?;
        let raw = grit_lib::diff::diff_tree_to_worktree_with_git_dir_and_rules(
            repo.odb(),
            base_tree,
            work_tree,
            repo.git_dir(),
            &index,
            Some(config.as_ref()),
            Some(&worktree_rules),
        )
        .context("could not diff the working tree")?;
        apply_rename_detection_to_diff_entries(
            repo.odb(),
            raw,
            Some(work_tree),
            rename_threshold(config.as_ref()),
            config.as_ref(),
            &repo.thread_policy(),
        )
        .context("could not detect renames in the working tree diff")?
    };
    sort_entries(&mut entries);
    Ok(entries)
}

/// The change a single commit introduced: its first parent's tree vs its own.
fn commit_changes(
    repo: &grit_lib::repo::Repository,
    oid: &ObjectId,
    config: &grit_lib::config::ConfigSet,
    thread_policy: &grit_lib::parallel::ThreadPolicy,
    diff_options: DiffOptions,
) -> Result<Vec<TreeDiffStructured>> {
    let commit = context::read_commit(repo, oid)?;
    let new_tree = commit.tree;
    let old_tree = match commit.parents.first() {
        Some(parent) => Some(context::commit_tree(repo, parent)?),
        None => None,
    };

    let mut entries = diff_trees_with_renames_and_policy(
        repo.odb(),
        old_tree.as_ref(),
        Some(&new_tree),
        "",
        rename_threshold(config),
        config,
        thread_policy,
    )
    .context("could not diff the commit")?;
    sort_entries(&mut entries);
    tree_diff_structured_parallel(repo.odb(), entries, config, thread_policy, diff_options)
        .context("could not build structured diff")
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

fn new_side_bytes(
    odb: &Odb,
    e: &DiffEntry,
    work_tree: Option<&WorktreeDiffSide<'_>>,
) -> Result<(SideBytes, bool)> {
    if e.new_mode == "160000" {
        let wt = work_tree.map(|s| s.work_tree);
        return Ok((
            SideBytes::from_bytes(gitlink_new_line(e, wt).into_bytes()),
            false,
        ));
    }
    if e.new_mode == "000000" {
        return Ok((SideBytes::from_bytes(Vec::new()), false));
    }
    if let (Some(ctx), Some(p)) = (work_tree, e.new_path.as_deref().or(e.old_path.as_deref())) {
        // Tree↔worktree collection often stores only a hash of normalized worktree bytes; the
        // Blob is not in the ODB until staging writes it. Always read the live worktree here.
        let mode = u32::from_str_radix(&e.new_mode, 8).unwrap_or(0);
        let index_entry = ctx.index.get(p.as_bytes(), 0);
        let rules = ctx.rules.lock().unwrap_or_else(|e| e.into_inner());
        let data = read_worktree_side_bytes_for_diff(
            odb,
            ctx.work_tree,
            p,
            mode,
            rules.config(),
            index_entry,
            Some(&rules),
        )?;
        Ok(bytes_for_diff(&data))
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

/// Convert bytes to `(SideBytes, is_binary)`. NUL in the first 8 KiB ⇒ binary.
fn bytes_for_diff(data: &[u8]) -> (SideBytes, bool) {
    if data.iter().take(8000).any(|&b| b == 0) {
        (SideBytes::from_bytes(Vec::new()), true)
    } else {
        (SideBytes::from_bytes(data.to_vec()), false)
    }
}

fn diff_mode_display(mode: &str) -> Option<String> {
    if mode == "000000" {
        None
    } else {
        Some(mode.to_owned())
    }
}

fn structured_for_file_change(fc: &FileChange, diff_options: DiffOptions) -> StructuredFileDiff {
    if fc.binary {
        StructuredFileDiff {
            old_mode: diff_mode_display(&fc.old_mode),
            new_mode: diff_mode_display(&fc.new_mode),
            old_encoding_lossy: fc.old_bytes.encoding_lossy,
            new_encoding_lossy: fc.new_bytes.encoding_lossy,
            hunks: Vec::new(),
        }
    } else {
        structured_file_diff(
            &fc.old_mode,
            &fc.new_mode,
            &fc.old_bytes,
            &fc.new_bytes,
            diff_options,
        )
    }
}

fn build_file_diff(fc: &FileChange, diff_options: DiffOptions) -> FileDiff {
    let structured = structured_for_file_change(fc, diff_options);
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
        path: fc.path.clone(),
        old_path: fc.old_path.clone(),
        status: fc.status.into(),
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
            render_file(w, file, color, self.path_display.as_ref())?;
        }
        Ok(())
    }
}

impl MarkdownRender for DiffOutcome {
    fn render_markdown(&self, w: &mut dyn Write, _ctx: &RenderContext) -> Result<()> {
        markdown::print_diff_files(w, &self.files, self.path_display.as_ref())?;
        Ok(())
    }
}

/// Color only on a TTY with `NO_COLOR` unset (<https://no-color.org>), and only
/// when the console can render ANSI (always on Unix; VT-capable on Windows).
fn use_color() -> bool {
    std::io::stdout().is_terminal()
        && grit_lib::porcelain::terminal::ansi_supported()
        && std::env::var_os("NO_COLOR").is_none()
}

fn paint(color: bool, code: &str, text: &str) -> String {
    if color {
        format!("\x1b[{code}m{text}{RESET}")
    } else {
        text.to_owned()
    }
}

/// Status word aligned with [`ChangeStatus`] JSON (`--json` / `--markdown` metadata).
#[must_use]
pub(crate) fn file_diff_status_label(status: ChangeStatus) -> &'static str {
    match status {
        ChangeStatus::Added => "added",
        ChangeStatus::Deleted => "deleted",
        ChangeStatus::Modified => "modified",
        ChangeStatus::TypeChanged => "type_changed",
        ChangeStatus::Renamed => "renamed",
        ChangeStatus::Copied => "copied",
        ChangeStatus::Unmerged => "unmerged",
    }
}

pub(crate) fn write_file_diff_metadata_human(w: &mut dyn Write, file: &FileDiff) -> Result<()> {
    match file.status {
        ChangeStatus::Deleted => outln!(w, "deleted"),
        ChangeStatus::Renamed => outln!(w, "renamed"),
        _ => {}
    }
    write_file_diff_mode_lines_human(w, file)
}

fn write_file_diff_mode_lines_human(w: &mut dyn Write, file: &FileDiff) -> Result<()> {
    if file.old_mode != file.new_mode {
        if let Some(m) = &file.old_mode {
            outln!(w, "old mode {m}");
        }
        if let Some(m) = &file.new_mode {
            outln!(w, "new mode {m}");
        }
    }
    Ok(())
}

fn render_file(
    w: &mut dyn Write,
    file: &FileDiff,
    color: bool,
    path_opts: Option<&PathDisplayOptions>,
) -> Result<()> {
    outln!(w);
    let header = ui::format_diff_file_header(file.old_path.as_deref(), &file.path, path_opts);
    outln!(w, "{}", paint(color, "1;4", &header)); // bold + underline

    write_file_diff_metadata_human(w, file)?;

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

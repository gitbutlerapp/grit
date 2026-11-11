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

use super::convert::{cli_file_from_lib, cli_file_from_target_file_diff, map_listed_file};
use super::{DiffFormatArgs, DiffOutcome, DiffRunOptions, DiffTotalsJson, PatchBodies};
use anyhow::{Context, Result};
use grit_lib::diff::{
    include_file_change_in_patch, peer_hunks_by_primary_path, should_report_inexact_rename_skipped,
    sort_entries_by_path, write_commit_log_patch, write_commits_log_patch, write_patch,
    write_patch_resolved, DiffArguments, DiffDetail, DiffEntry, DiffFilesOutput, DiffMerges,
    DiffOptions, DiffRequest, DiffSide, DiffTarget, FileChange, LineStat, ListedFileDiff,
    LogPatchCommitHeader, LogPatchOptions, PatchResolvedSide, PatchSide, PatchTextOptions,
    ResolveDiffArgumentsError, UncommittedDiff,
};
use grit_lib::objects::ObjectId;
use grit_lib::worktree::pathspec::pathdiff;
use std::io::Write;

use crate::context;
use crate::render::diff::{
    diffstat_from_files, render_file, render_name_only, render_name_status, render_numstat,
    render_shortstat, render_stat, FileDiff, LineStatJson,
};
use crate::ui::PathDisplayOptions;

const CONTEXT_LINES: usize = 3;

fn attach_path_display(
    repo: &grit_lib::repo::Repository,
    format: DiffFormatArgs,
    mut outcome: DiffOutcome,
) -> DiffOutcome {
    outcome.path_display = PathDisplayOptions::from_repo(repo).ok();
    outcome.format = format;
    outcome
}

fn finish_no_index_outcome(format: DiffFormatArgs, mut outcome: DiffOutcome) -> DiffOutcome {
    outcome.changed = !outcome.files.is_empty();
    if format.detail() == DiffDetail::Stat && outcome.totals.is_none() {
        outcome.totals = Some(diffstat_from_files(&outcome.files).lib().totals().into());
    }
    outcome.format = format;
    outcome
}

fn finish_outcome(
    repo: &grit_lib::repo::Repository,
    format: DiffFormatArgs,
    mut outcome: DiffOutcome,
) -> DiffOutcome {
    if !outcome.preserve_changed {
        outcome.changed = !outcome.files.is_empty();
    }
    if format.detail() == DiffDetail::Stat && outcome.totals.is_none() {
        outcome.totals = Some(diffstat_from_files(&outcome.files).lib().totals().into());
    }
    attach_path_display(repo, format, outcome)
}

fn outcome_from_change_names(changes: Vec<FileChange>, format: DiffFormatArgs) -> DiffOutcome {
    let files = changes
        .into_iter()
        .map(|fc| FileDiff {
            path: fc.path,
            old_path: fc.old_path,
            status: fc.status.into(),
            old_mode: fc.old_mode.octal_if_present(),
            new_mode: fc.new_mode.octal_if_present(),
            old_encoding_lossy: fc.old_bytes.encoding_lossy,
            new_encoding_lossy: fc.new_bytes.encoding_lossy,
            binary: fc.binary,
            old_size: fc.binary.then_some(fc.old_bytes.byte_len()),
            new_size: fc.binary.then_some(fc.new_bytes.byte_len()),
            content_changed: fc.content_changed,
            similarity: fc.similarity,
            stat: None,
            combined_status: None,
            hunks: None,
        })
        .collect::<Vec<FileDiff>>();
    DiffOutcome {
        changed: !files.is_empty(),
        files,
        totals: None,
        format,
        path_display: None,
        body_streamed: false,
        preserve_changed: false,
    }
}

/// Build a [`DiffOutcome`] from library [`DiffFilesOutput`] (for `grit log` per commit).
pub fn cli_outcome_from_lib(
    repo: &grit_lib::repo::Repository,
    output: DiffFilesOutput,
    detail: DiffDetail,
    format: DiffFormatArgs,
) -> Result<DiffOutcome> {
    Ok(finish_outcome(
        repo,
        format,
        outcome_from_lib(output, format, detail),
    ))
}

/// Human diff detail under one `grit log` commit row (no placeholder when empty).
pub fn render_log_commit_diff(w: &mut dyn Write, outcome: &DiffOutcome) -> Result<()> {
    if outcome.files.is_empty() {
        return Ok(());
    }
    let color = crate::output::stdout_color_enabled();
    let fmt = outcome.format;
    if fmt.human_numstat() {
        return render_numstat(w, &outcome.files, outcome.path_display.as_ref());
    }
    if fmt.human_name_only() {
        return render_name_only(w, &outcome.files, outcome.path_display.as_ref());
    }
    if fmt.human_name_status() {
        return render_name_status(w, &outcome.files, outcome.path_display.as_ref());
    }
    if fmt.human_shortstat_only() {
        let stat = diffstat_from_files(&outcome.files);
        return render_shortstat(w, &stat);
    }
    if fmt.human_stat_block() {
        let stat = diffstat_from_files(&outcome.files);
        return render_stat(w, &stat, color, outcome.path_display.as_ref());
    }
    for file in &outcome.files {
        render_file(w, file, color, outcome.path_display.as_ref())?;
    }
    Ok(())
}

fn emit_inexact_rename_notice(skipped: &grit_lib::diff::InexactRenameDetectionSkipped) {
    let limit = skipped
        .configured_limit
        .map(|n| n.to_string())
        .unwrap_or_else(|| "none".into());
    eprintln!(
        "grit: skipped inexact rename detection for {} delete and {} add candidates (diff.renameLimit={limit}); raise the limit toward {} to compare more pairs",
        skipped.unmatched_sources,
        skipped.unmatched_destinations,
        skipped.suggested_limit,
    );
}

fn outcome_from_lib(
    output: DiffFilesOutput,
    format: DiffFormatArgs,
    detail: DiffDetail,
) -> DiffOutcome {
    if let Some(skipped) = output.inexact_rename_skipped {
        if should_report_inexact_rename_skipped(&skipped) {
            emit_inexact_rename_notice(&skipped);
        }
    }
    let totals = output.totals.map(DiffTotalsJson::from);
    let files = output
        .files
        .into_iter()
        .map(|f| map_listed_file(f, detail))
        .collect();
    outcome_from_files(files, totals, format)
}

fn outcome_from_files(
    files: Vec<FileDiff>,
    totals: Option<DiffTotalsJson>,
    format: DiffFormatArgs,
) -> DiffOutcome {
    DiffOutcome {
        changed: !files.is_empty(),
        files,
        totals,
        format,
        path_display: None,
        body_streamed: false,
        preserve_changed: false,
    }
}

fn outcome_from_entries(
    repo: &grit_lib::repo::Repository,
    diff_result: grit_lib::diff::DiffEntriesResult,
    detail: DiffDetail,
    diff_options: DiffOptions,
    format: DiffFormatArgs,
) -> Result<DiffOutcome> {
    if let Some(skipped) = diff_result.inexact_rename_skipped {
        if should_report_inexact_rename_skipped(&skipped) {
            emit_inexact_rename_notice(&skipped);
        }
    }
    let entries = diff_result.entries;
    if matches!(detail, DiffDetail::NameOnly | DiffDetail::NameStatus) {
        let files = entries
            .into_iter()
            .map(|entry| map_listed_file(ListedFileDiff::from_metadata(entry), detail))
            .collect();
        return Ok(outcome_from_files(files, None, format));
    }
    let output = repo.diff().files_for_entries(
        entries,
        detail,
        &diff_options.with_renames(grit_lib::diff::RenameDetection::Off),
    )?;
    Ok(outcome_from_lib(output, format, detail))
}

/// Compare two filesystem paths outside a repository ([`grit_lib::diff::diff_paths`]).
pub fn run_no_index(opts: DiffRunOptions, ctx: super::DiffRunContext) -> Result<DiffOutcome> {
    if opts.staged || opts.unstaged || opts.against.is_some() {
        anyhow::bail!("--no-index cannot be combined with --staged, --unstaged, or --against");
    }
    if opts.pathspecs.pathspec_from_file.is_some() {
        anyhow::bail!("--no-index cannot be combined with --pathspec-from-file");
    }
    let mut path_args: Vec<String> = opts.commits;
    path_args.extend(opts.pathspecs.paths);
    if path_args.len() != 2 {
        anyhow::bail!(
            "--no-index requires exactly two paths (use `--` before paths that start with `-`)"
        );
    }
    let mut diff_options = DiffOptions::default();
    diff_options.context_lines = CONTEXT_LINES;
    opts.format.apply_context(&mut diff_options);
    diff_options = opts.line_format.to_options(diff_options)?;
    diff_options.word_level_inline = ctx.word_level_inline;
    let detail = opts.format.detail();
    let format = opts.format;
    if detail == DiffDetail::Stat {
        diff_options = diff_options.with_no_index_stat_only(true);
    }
    let files = grit_lib::diff::diff_paths(
        std::path::Path::new(&path_args[0]),
        std::path::Path::new(&path_args[1]),
        &diff_options,
    )
    .map_err(|e| anyhow::anyhow!(e.to_string()))?;
    let cli_files: Vec<FileDiff> = files
        .into_iter()
        .map(|f| cli_file_from_lib(f, detail))
        .collect();
    Ok(finish_no_index_outcome(
        format,
        DiffOutcome {
            changed: !cli_files.is_empty(),
            files: cli_files,
            totals: None,
            format,
            path_display: None,
            body_streamed: false,
            preserve_changed: false,
        },
    ))
}

fn run_no_index_patch_text(
    opts: DiffRunOptions,
    ctx: super::DiffRunContext,
) -> Result<DiffOutcome> {
    if opts.staged || opts.unstaged || opts.against.is_some() {
        anyhow::bail!("--no-index cannot be combined with --staged, --unstaged, or --against");
    }
    let quiet = opts.quiet;
    if opts.pathspecs.pathspec_from_file.is_some() {
        anyhow::bail!("--no-index cannot be combined with --pathspec-from-file");
    }
    let mut path_args: Vec<String> = opts.commits;
    path_args.extend(opts.pathspecs.paths);
    if path_args.len() != 2 {
        anyhow::bail!(
            "--no-index requires exactly two paths (use `--` before paths that start with `-`)"
        );
    }
    let mut diff_options = DiffOptions::default();
    diff_options.context_lines = CONTEXT_LINES;
    opts.format.apply_context(&mut diff_options);
    diff_options = opts.line_format.to_options(diff_options)?;
    diff_options.word_level_inline = ctx.word_level_inline;
    let format = opts.format;
    let a = std::path::Path::new(&path_args[0]);
    let b = std::path::Path::new(&path_args[1]);
    let changed = if quiet {
        grit_lib::diff::write_paths_patch(a, b, &diff_options, &mut std::io::sink())?
    } else {
        let mut changed = false;
        crate::output::with_streaming_stdout(|stdout| {
            changed = grit_lib::diff::write_paths_patch(a, b, &diff_options, stdout)?;
            Ok(())
        })?;
        changed
    };
    Ok(DiffOutcome {
        changed,
        files: Vec::new(),
        totals: None,
        format,
        path_display: None,
        body_streamed: true,
        preserve_changed: true,
    })
}

/// Collected changes for `--patch-text`, plus where the new side of a binary file lives
/// (collection keeps no bytes for binary files).
type PatchChanges = (Vec<FileChange>, PatchSide);

fn emit_patch_text_to_stdout(
    repo: &grit_lib::repo::Repository,
    entries: Vec<DiffEntry>,
    changes: Option<PatchChanges>,
    diff_options: DiffOptions,
    quiet: bool,
) -> Result<()> {
    if quiet {
        return Ok(());
    }
    let patch_opts = PatchTextOptions::from_repo(repo, diff_options)?;
    crate::output::with_streaming_stdout(|stdout| {
        if let Some((changes, binary_new)) = changes {
            if entries.len() != changes.len() {
                anyhow::bail!("internal error: patch-text entry/change count mismatch");
            }
            let resolved: Vec<PatchResolvedSide> = entries
                .into_iter()
                .zip(changes)
                .map(|(entry, fc)| {
                    let (old, new) = if fc.binary {
                        (PatchSide::Blob, binary_new.clone())
                    } else {
                        (
                            PatchSide::Bytes(fc.old_bytes.data),
                            PatchSide::Bytes(fc.new_bytes.data),
                        )
                    };
                    PatchResolvedSide::new(entry, old, new)
                })
                .collect();
            write_patch_resolved(repo, &resolved, &patch_opts, stdout)?;
        } else {
            write_patch(repo, &entries, &patch_opts, stdout)?;
        }
        Ok(())
    })
}

/// Resolved inputs for deferred `grit show --patch-text` rendering.
pub struct ShowLogPatchTextParams {
    pub oid: ObjectId,
    pub format: DiffFormatArgs,
    pub pathspec: grit_lib::worktree::pathspec::Pathspec,
    pub diff_merges: DiffMerges,
}

/// Build [`LogPatchOptions`] for git-apply-compatible `grit show` / `grit log --patch-text`.
pub fn log_patch_options_for_patch_text(
    repo: &grit_lib::repo::Repository,
    format: DiffFormatArgs,
    pathspec: &grit_lib::worktree::pathspec::Pathspec,
    diff_merges: DiffMerges,
) -> Result<LogPatchOptions> {
    let config = repo.config().snapshot().context("could not load config")?;
    let mut diff_options = DiffOptions::from_config(config.as_ref());
    diff_options.context_lines = CONTEXT_LINES;
    format.apply_context(&mut diff_options);
    Ok(LogPatchOptions {
        diff: diff_options,
        abbrev_len: 7,
        commit_header: LogPatchCommitHeader::Full,
        binary_format: grit_lib::diff::LogPatchBinaryFormat::GitApplyBinaryPatch,
        pathspec: pathspec.clone(),
        diff_merges,
    })
}

fn map_log_patch_error(err: grit_lib::error::Error) -> anyhow::Error {
    anyhow::Error::new(err)
}

/// Outcome after streaming a show/log `--patch-text` body to stdout.
#[must_use]
pub fn streamed_log_patch_text_outcome(format: DiffFormatArgs) -> DiffOutcome {
    DiffOutcome {
        changed: true,
        files: Vec::new(),
        totals: None,
        format,
        path_display: None,
        body_streamed: false,
        preserve_changed: true,
    }
}

/// Stream one commit in `git show --patch-text` shape.
pub fn emit_show_log_patch_text(
    repo: &grit_lib::repo::Repository,
    params: &ShowLogPatchTextParams,
    mut out: &mut dyn Write,
) -> Result<()> {
    let opts = log_patch_options_for_patch_text(
        repo,
        params.format,
        &params.pathspec,
        params.diff_merges,
    )?;
    repo.odb()
        .with_pack_read_context(|| write_commit_log_patch(repo, &mut out, &params.oid, &opts))
        .map_err(map_log_patch_error)
}

/// Stream many commits in `git log --patch-text` shape.
pub fn emit_commits_log_patch_text(
    repo: &grit_lib::repo::Repository,
    oids: &[ObjectId],
    format: DiffFormatArgs,
    pathspec: &grit_lib::worktree::pathspec::Pathspec,
    mut out: &mut dyn Write,
) -> Result<()> {
    if oids.is_empty() {
        return Ok(());
    }
    let opts = log_patch_options_for_patch_text(repo, format, pathspec, DiffMerges::Combined)?;
    repo.odb()
        .with_pack_read_context(|| write_commits_log_patch(repo, &mut out, oids, &opts))
        .map_err(map_log_patch_error)
}

fn patch_text_outcome(
    repo: &grit_lib::repo::Repository,
    entries: Vec<DiffEntry>,
    changes: Option<PatchChanges>,
    diff_options: DiffOptions,
    format: DiffFormatArgs,
    quiet: bool,
) -> Result<DiffOutcome> {
    let changed = !entries.is_empty();
    emit_patch_text_to_stdout(repo, entries, changes, diff_options, quiet)?;
    Ok(DiffOutcome {
        changed,
        files: Vec::new(),
        totals: None,
        format,
        path_display: None,
        body_streamed: false,
        preserve_changed: true,
    })
}

fn run_patch_text(opts: DiffRunOptions, _ctx: super::DiffRunContext) -> Result<DiffOutcome> {
    let quiet = opts.quiet;
    let repo = context::discover()?;
    let config = repo.config().snapshot().context("could not load config")?;
    let DiffArguments {
        target, pathspec, ..
    } = resolve_diff_target(&repo, &opts)?;
    let mut diff_options = DiffOptions::from_config(config.as_ref());
    diff_options.context_lines = CONTEXT_LINES;
    opts.format.apply_context(&mut diff_options);
    diff_options = opts.line_format.to_options(diff_options)?;
    let format = opts.format;
    let (entries, changes) = match target {
        DiffTarget::IndexVsHead => {
            let diff_result = uncommitted_entries(
                &mut uncommitted_diff(&repo)?,
                &target,
                &pathspec,
                &diff_options,
            )?;
            let changes = repo.diff().object_changes(&diff_result.entries)?;
            (diff_result.entries, Some((changes, PatchSide::Blob)))
        }
        DiffTarget::WorktreeVsIndex
        | DiffTarget::WorktreeVsHead
        | DiffTarget::WorktreeVsTree(_) => {
            let mut uncommitted = uncommitted_diff(&repo)?;
            let diff_result =
                uncommitted_entries(&mut uncommitted, &target, &pathspec, &diff_options)?;
            let changes = uncommitted.load_worktree_changes(diff_result.entries.clone())?;
            (diff_result.entries, Some((changes, PatchSide::Worktree)))
        }
        _ => {
            let diff_result = stored_entries(&repo, &target, &pathspec, &diff_options)?;
            if let Some(skipped) = diff_result.inexact_rename_skipped {
                emit_inexact_rename_notice(&skipped);
            }
            (diff_result.entries, None)
        }
    };
    patch_text_outcome(&repo, entries, changes, diff_options, format, quiet)
}

pub fn run(opts: DiffRunOptions, ctx: super::DiffRunContext) -> Result<DiffOutcome> {
    if opts.no_index {
        if opts.format.patch_text {
            return run_no_index_patch_text(opts, ctx);
        }
        return run_no_index(opts, ctx);
    }
    if opts.format.patch_text {
        return run_patch_text(opts, ctx);
    }
    let repo = context::discover()?;
    let config = repo.config().snapshot().context("could not load config")?;
    let DiffArguments {
        target, pathspec, ..
    } = resolve_diff_target(&repo, &opts)?;
    let mut diff_options = DiffOptions::from_config(config.as_ref());
    diff_options.context_lines = CONTEXT_LINES;
    opts.format.apply_context(&mut diff_options);
    diff_options = opts.line_format.to_options(diff_options)?;
    diff_options.word_level_inline = ctx.word_level_inline;
    let detail = opts.format.detail();
    let format = opts.format;
    let outcome = match target {
        DiffTarget::IndexVsHead => {
            let mut uncommitted = uncommitted_diff(&repo)?;
            let entries = uncommitted_entries(&mut uncommitted, &target, &pathspec, &diff_options)?;
            match detail {
                DiffDetail::Stat | DiffDetail::NameOnly | DiffDetail::NameStatus => {
                    drop(uncommitted);
                    outcome_from_entries(&repo, entries, detail, diff_options, format)?
                }
                _ => {
                    let staged = repo.diff().object_changes(&entries.entries)?;
                    let peer = if ctx.emit_json_hunk_ids {
                        uncommitted
                            .peer_changes(
                                DiffSide::Staged,
                                &pathspec,
                                diff_options.renames,
                                staged.iter().map(|fc| fc.path.as_str()),
                            )
                            .unwrap_or_default()
                    } else {
                        Vec::new()
                    };
                    drop(uncommitted);
                    outcome_from_changes(
                        &repo,
                        staged,
                        peer,
                        diff_options,
                        format,
                        DiffSide::Staged,
                        ctx.emit_json_hunk_ids,
                    )?
                }
            }
        }
        DiffTarget::WorktreeVsIndex => {
            let mut uncommitted = uncommitted_diff(&repo)?;
            let entries = uncommitted_entries(&mut uncommitted, &target, &pathspec, &diff_options)?;
            match detail {
                DiffDetail::Stat => {
                    let changes = uncommitted.load_worktree_changes(entries.entries)?;
                    drop(uncommitted);
                    outcome_stat_from_changes(changes, diff_options, format)
                }
                DiffDetail::NameOnly | DiffDetail::NameStatus => {
                    drop(uncommitted);
                    outcome_from_entries(&repo, entries, detail, diff_options, format)?
                }
                _ => {
                    let unstaged = uncommitted.load_worktree_changes(entries.entries)?;
                    let peer = if ctx.emit_json_hunk_ids {
                        uncommitted
                            .peer_changes(
                                DiffSide::Unstaged,
                                &pathspec,
                                diff_options.renames,
                                unstaged.iter().map(|fc| fc.path.as_str()),
                            )
                            .unwrap_or_default()
                    } else {
                        Vec::new()
                    };
                    drop(uncommitted);
                    outcome_from_changes(
                        &repo,
                        unstaged,
                        peer,
                        diff_options,
                        format,
                        DiffSide::Unstaged,
                        ctx.emit_json_hunk_ids,
                    )?
                }
            }
        }
        DiffTarget::WorktreeVsHead | DiffTarget::WorktreeVsTree(_) => {
            let mut uncommitted = uncommitted_diff(&repo)?;
            let entries = uncommitted_entries(&mut uncommitted, &target, &pathspec, &diff_options)?;
            let changes = uncommitted.load_worktree_changes(entries.entries)?;
            drop(uncommitted);
            match detail {
                DiffDetail::NameOnly | DiffDetail::NameStatus => {
                    outcome_from_change_names(changes, format)
                }
                DiffDetail::Stat => outcome_stat_from_changes(changes, diff_options, format),
                _ => outcome_from_changes(
                    &repo,
                    changes,
                    Vec::new(),
                    diff_options,
                    format,
                    DiffSide::Other,
                    false,
                )?,
            }
        }
        _ => tree_diff_outcome(
            &repo,
            stored_entries(&repo, &target, &pathspec, &diff_options)?,
            detail,
            diff_options,
            format,
            ctx.patch_bodies,
            &ctx,
        )?,
    };
    Ok(finish_outcome(&repo, format, outcome))
}

/// Resolve `grit diff` flags and operands into what to compare and the path limits.
fn resolve_diff_target(
    repo: &grit_lib::repo::Repository,
    opts: &DiffRunOptions,
) -> Result<DiffArguments> {
    let request = DiffRequest {
        staged: opts.staged,
        unstaged: opts.unstaged,
        against: opts.against.clone(),
        positionals: opts.commits.clone(),
        paths: opts.pathspecs.paths.clone(),
        pathspec_file: opts.pathspecs.pathspec_file_entries()?,
    };
    let cwd_prefix = pathspec_prefix_for_repo(repo)?;
    repo.diff()
        .resolve_request(cwd_prefix.as_deref(), &request)
        .map_err(map_diff_arguments_error)
}

/// Keep library failures downcastable to [`grit_lib::error::Error`] so revision messages, error
/// hints and `--json` kinds see them.
pub(crate) fn map_diff_arguments_error(err: grit_lib::error::Error) -> anyhow::Error {
    match err {
        grit_lib::error::Error::Diff(grit_lib::error::DiffError::Arguments(
            ResolveDiffArgumentsError::UnresolvedRevision { spec, source },
        )) => anyhow::Error::new(grit_lib::error::Error::Revision(*source))
            .context(format!("could not resolve '{spec}'")),
        grit_lib::error::Error::Diff(grit_lib::error::DiffError::Arguments(args)) => args.into(),
        other => other.into(),
    }
}

/// The diff a single commit introduced, as a [`DiffOutcome`]. Reused by `grit show`.
/// Options for diffing a single commit's patch (first parent vs commit tree).
pub struct CommitDiffOptions {
    /// Pathspecs from trailing `PATH` arguments (after `--`).
    pub paths: Vec<String>,
    pub format: DiffFormatArgs,
    pub word_level_inline: bool,
    /// Merge commit parent diff mode (`first-parent` or `combined`).
    pub diff_merges: DiffMerges,
}

/// Diff the change one commit introduced (first-parent tree vs commit tree).
pub fn diff_for_commit(
    repo: &grit_lib::repo::Repository,
    oid: &ObjectId,
    opts: CommitDiffOptions,
) -> Result<DiffOutcome> {
    let config = repo.config().snapshot().context("could not load config")?;
    let pathspec = resolve_commit_diff_pathspecs(repo, &opts.paths)?;
    let mut diff_options = DiffOptions::from_config(config.as_ref());
    diff_options.context_lines = CONTEXT_LINES;
    opts.format.apply_context(&mut diff_options);
    diff_options.word_level_inline = opts.word_level_inline;
    let detail = opts.format.detail();
    let format = opts.format;
    let output =
        repo.diff()
            .commit_files(oid, opts.diff_merges, detail, &diff_options, &pathspec)?;
    Ok(finish_outcome(
        repo,
        format,
        outcome_from_lib(output, format, detail),
    ))
}

/// Build a full patch-style [`DiffOutcome`] from tree diff entries (e.g. stash W vs base).
pub fn outcome_from_tree_entries(
    repo: &grit_lib::repo::Repository,
    mut entries: Vec<DiffEntry>,
) -> Result<DiffOutcome> {
    sort_entries_by_path(&mut entries);
    let changes = repo.diff().object_changes(&entries)?;
    let config = repo.config().snapshot().context("could not load config")?;
    let mut diff_options = DiffOptions::from_config(config.as_ref());
    diff_options.context_lines = CONTEXT_LINES;
    diff_options.word_level_inline = false;
    let format = DiffFormatArgs::default();
    Ok(finish_outcome(
        repo,
        format,
        outcome_from_changes(
            repo,
            changes,
            Vec::new(),
            diff_options,
            format,
            DiffSide::Other,
            false,
        )?,
    ))
}

fn uncommitted_diff(repo: &grit_lib::repo::Repository) -> Result<UncommittedDiff<'_>> {
    repo.diff()
        .uncommitted()
        .context("could not load the index")
}

/// Entries for a staged, unstaged or working-tree target, on the index `uncommitted` loaded.
fn uncommitted_entries(
    uncommitted: &mut UncommittedDiff<'_>,
    target: &DiffTarget,
    pathspec: &grit_lib::worktree::pathspec::Pathspec,
    diff_options: &DiffOptions,
) -> Result<grit_lib::diff::DiffEntriesResult> {
    uncommitted
        .entries(target, pathspec, diff_options)
        .context("could not diff the index or working tree")
}

/// Entries for a commit or commit-range target (no index read).
fn stored_entries(
    repo: &grit_lib::repo::Repository,
    target: &DiffTarget,
    pathspec: &grit_lib::worktree::pathspec::Pathspec,
    diff_options: &DiffOptions,
) -> Result<grit_lib::diff::DiffEntriesResult> {
    repo.diff()
        .entries(target, pathspec, diff_options)
        .context("could not diff commits")
}

fn outcome_from_changes(
    repo: &grit_lib::repo::Repository,
    changes: Vec<FileChange>,
    peer_changes: Vec<FileChange>,
    diff_options: DiffOptions,
    format: DiffFormatArgs,
    side: DiffSide,
    assign_hunk_ids: bool,
) -> Result<DiffOutcome> {
    use std::collections::HashSet;

    let changes: Vec<_> = changes
        .into_iter()
        .filter(|fc| include_file_change_in_patch(fc, &diff_options))
        .collect();
    let diff_options = diff_options.cap_word_level_inline_for_changed_files(changes.len());
    let primary_paths: HashSet<&str> = changes.iter().map(|fc| fc.path.as_str()).collect();
    let peer_hunks_by_path =
        if assign_hunk_ids && matches!(side, DiffSide::Unstaged | DiffSide::Staged) {
            let peer_pairs = peer_changes.into_iter().map(|fc| {
                let structured = fc.structured(diff_options.clone());
                (fc.path, structured.hunks)
            });
            peer_hunks_by_primary_path(&primary_paths, peer_pairs)
        } else {
            std::collections::HashMap::new()
        };
    let lib_files = repo
        .diff()
        .file_diffs_from_changes(&changes, diff_options.clone())?;
    let files: Vec<FileDiff> = lib_files
        .into_iter()
        .map(|f| {
            let peer_hunks = peer_hunks_by_path
                .get(&f.path)
                .map(|h| h.as_slice())
                .unwrap_or(&[]);
            cli_file_from_target_file_diff(f, side, peer_hunks, assign_hunk_ids)
        })
        .collect();
    Ok(DiffOutcome {
        changed: !files.is_empty(),
        files,
        totals: None,
        format,
        path_display: None,
        body_streamed: false,
        preserve_changed: false,
    })
}

fn pathspec_prefix_for_repo(repo: &grit_lib::repo::Repository) -> Result<Option<String>> {
    let Some(work_tree) = repo.work_tree() else {
        return Ok(None);
    };
    let cwd = std::env::current_dir().context("could not read the current directory")?;
    Ok(pathdiff(&cwd, work_tree))
}

/// Commit and range pathspecs, resolved relative to the current directory.
pub(crate) fn resolve_commit_diff_pathspecs(
    repo: &grit_lib::repo::Repository,
    raw_paths: &[String],
) -> Result<grit_lib::worktree::pathspec::Pathspec> {
    let cwd_prefix = pathspec_prefix_for_repo(repo)?;
    repo.diff()
        .resolve_pathspec(cwd_prefix.as_deref(), raw_paths)
        .map_err(map_diff_arguments_error)
}

fn outcome_stat_from_changes(
    changes: Vec<FileChange>,
    diff_options: DiffOptions,
    format: DiffFormatArgs,
) -> DiffOutcome {
    let mut lines = LineStat::default();
    let files = changes
        .into_iter()
        .map(|fc| {
            let stat = fc.line_stat(&diff_options);
            if let Some(counts) = stat {
                lines += counts;
            }
            let stat = stat.map(LineStatJson::from);
            FileDiff {
                path: fc.path,
                old_path: fc.old_path,
                status: fc.status.into(),
                old_mode: fc.old_mode.octal_if_present(),
                new_mode: fc.new_mode.octal_if_present(),
                old_encoding_lossy: fc.old_bytes.encoding_lossy,
                new_encoding_lossy: fc.new_bytes.encoding_lossy,
                binary: fc.binary,
                old_size: fc.binary.then_some(fc.old_bytes.byte_len()),
                new_size: fc.binary.then_some(fc.new_bytes.byte_len()),
                content_changed: fc.content_changed,
                similarity: fc.similarity,
                stat,
                combined_status: None,
                hunks: None,
            }
        })
        .collect::<Vec<_>>();
    let files_changed = files.len();
    DiffOutcome {
        changed: files_changed > 0,
        totals: Some(DiffTotalsJson {
            files_changed,
            insertions: lines.insertions,
            deletions: lines.deletions,
        }),
        files,
        format,
        path_display: None,
        body_streamed: false,
        preserve_changed: false,
    }
}

fn tree_diff_outcome(
    repo: &grit_lib::repo::Repository,
    diff_result: grit_lib::diff::DiffEntriesResult,
    detail: DiffDetail,
    diff_options: DiffOptions,
    format: DiffFormatArgs,
    patch_bodies: PatchBodies,
    ctx: &super::DiffRunContext,
) -> Result<DiffOutcome> {
    let grit_lib::diff::DiffEntriesResult {
        entries,
        inexact_rename_skipped,
        ..
    } = diff_result;
    if let Some(skipped) = inexact_rename_skipped {
        if should_report_inexact_rename_skipped(&skipped) {
            emit_inexact_rename_notice(&skipped);
        }
    }
    if detail == DiffDetail::Patch {
        match patch_bodies {
            PatchBodies::MetadataOnly => Ok(DiffOutcome {
                changed: !entries.is_empty(),
                files: Vec::new(),
                totals: None,
                format,
                path_display: None,
                body_streamed: false,
                preserve_changed: true,
            }),
            PatchBodies::Stream => super::stream::stream_tree_patch_to_stdout(
                repo,
                &entries,
                diff_options,
                format,
                patch_bodies,
                &ctx.render,
            ),
            PatchBodies::Collect if ctx.json_stream => super::stream::stream_tree_patch_to_stdout(
                repo,
                &entries,
                diff_options,
                format,
                patch_bodies,
                &ctx.render,
            ),
            PatchBodies::Collect => {
                let lib_files = repo
                    .diff()
                    .file_diffs_from_entries(entries, diff_options.clone())?;
                let files: Vec<FileDiff> = lib_files
                    .into_iter()
                    .map(|f| cli_file_from_target_file_diff(f, DiffSide::Other, &[], false))
                    .collect();
                Ok(DiffOutcome {
                    changed: !files.is_empty(),
                    files,
                    totals: None,
                    format,
                    path_display: None,
                    body_streamed: false,
                    preserve_changed: false,
                })
            }
        }
    } else {
        outcome_from_entries(
            repo,
            grit_lib::diff::DiffEntriesResult::new(entries),
            detail,
            diff_options,
            format,
        )
    }
}

#[cfg(test)]
mod tests {
    mod stream_output_parity {
        use super::super::super::convert::cli_file_from_tree;
        use crate::markdown;
        use crate::output::OutputMode;
        use crate::render::diff::render_file;
        use crate::ui::PathDisplayOptions;
        use grit_lib::diff::{
            write_patch, write_patch_structured, DiffEntry, DiffOptions, PatchTextOptions,
            TreeDiffOptions,
        };
        use grit_lib::objects::{serialize_tree, FileMode, ObjectKind, TreeEntry};
        use grit_lib::repo::{InitOptions, Repository};

        fn repo_with_range() -> (tempfile::TempDir, Repository, Vec<DiffEntry>) {
            let dir = tempfile::tempdir().expect("tempdir");
            let (repo, _) = Repository::init(dir.path(), &InitOptions::default()).expect("init");
            let odb = repo.odb();
            let tree = |tag: &str| {
                let entries: Vec<TreeEntry> = (0..6)
                    .map(|i| {
                        let oid = odb
                            .write(
                                ObjectKind::Blob,
                                format!("{tag} {i}\nsecond line\n").as_bytes(),
                            )
                            .expect("blob");
                        TreeEntry::new(format!("dir/f{i}.txt").into_bytes(), FileMode::REGULAR, oid)
                    })
                    .collect();
                odb.write(ObjectKind::Tree, &serialize_tree(&entries))
                    .expect("tree")
            };
            let old_tree = tree("old");
            let new_tree = tree("new");
            let entries = repo
                .diff()
                .tree_to_tree(
                    Some(&old_tree),
                    Some(&new_tree),
                    &TreeDiffOptions::default(),
                )
                .expect("diff entries")
                .entries;
            (dir, repo, entries)
        }

        #[derive(Clone, Copy)]
        enum DiffTestStyle {
            Human,
            Markdown,
        }

        fn render_human_or_markdown(
            repo: &Repository,
            entries: &[DiffEntry],
            mode: DiffTestStyle,
            stream: bool,
        ) -> Vec<u8> {
            let diff_options = DiffOptions::default().with_word_level_inline(false);
            let path_display = PathDisplayOptions::from_repo(repo).ok();
            let mut buf = Vec::new();
            let mut render_one = |index: usize, structured| {
                let file = cli_file_from_tree(structured);
                match mode {
                    DiffTestStyle::Human => {
                        render_file(&mut buf, &file, false, path_display.as_ref()).expect("human")
                    }
                    DiffTestStyle::Markdown => {
                        markdown::print_diff_file(&mut buf, &file, path_display.as_ref(), index > 0)
                            .expect("markdown")
                    }
                }
            };
            if stream {
                let mut index = 0usize;
                repo.diff()
                    .for_each_structured(entries, &diff_options, |structured| {
                        render_one(index, structured);
                        index += 1;
                        Ok(())
                    })
                    .expect("stream render");
            } else {
                let rows = repo
                    .diff()
                    .structured(entries.to_vec(), &diff_options)
                    .expect("collect structured");
                for (index, row) in rows.into_iter().enumerate() {
                    render_one(index, row);
                }
            }
            buf
        }

        #[test]
        fn human_stream_matches_collected() {
            let (_dir, repo, entries) = repo_with_range();
            let collected = render_human_or_markdown(&repo, &entries, DiffTestStyle::Human, false);
            let streamed = render_human_or_markdown(&repo, &entries, DiffTestStyle::Human, true);
            assert_eq!(collected, streamed);
        }

        #[test]
        fn markdown_stream_matches_collected() {
            let (_dir, repo, entries) = repo_with_range();
            let collected =
                render_human_or_markdown(&repo, &entries, DiffTestStyle::Markdown, false);
            let streamed = render_human_or_markdown(&repo, &entries, DiffTestStyle::Markdown, true);
            assert_eq!(collected, streamed);
        }

        #[test]
        fn human_render_batches_writes_through_buffer() {
            use crate::output::OUTPUT_BUFFER_BYTES;
            use std::cell::RefCell;
            use std::io::{self, BufWriter, Write};
            use std::rc::Rc;

            struct SpyWriter {
                sizes: Rc<RefCell<Vec<usize>>>,
            }

            impl Write for SpyWriter {
                fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
                    self.sizes.borrow_mut().push(buf.len());
                    Ok(buf.len())
                }

                fn flush(&mut self) -> io::Result<()> {
                    Ok(())
                }
            }

            let (_dir, repo, entries) = repo_with_range();
            let diff_options = DiffOptions::default().with_word_level_inline(false);
            let structured = repo
                .diff()
                .structured(entries, &diff_options)
                .expect("structured")
                .into_iter()
                .next()
                .expect("one file");
            let file = cli_file_from_tree(structured);
            let sizes = Rc::new(RefCell::new(Vec::new()));
            let spy = SpyWriter {
                sizes: Rc::clone(&sizes),
            };
            let mut buffered = BufWriter::with_capacity(OUTPUT_BUFFER_BYTES, spy);
            for _ in 0..400 {
                render_file(&mut buffered, &file, false, None).expect("render");
            }
            buffered.flush().expect("flush");
            let write_count = sizes.borrow().len();
            let total_bytes: usize = sizes.borrow().iter().sum();
            let line_budget = total_bytes / OUTPUT_BUFFER_BYTES + 4;
            assert!(
                write_count <= line_budget,
                "expected O(bytes/64KiB) writes, got {write_count} for {total_bytes} bytes (budget {line_budget})"
            );
            assert!(
                write_count < 400,
                "expected buffering, got one write batch per render ({write_count})"
            );
        }

        #[test]
        fn patch_text_pipeline_matches_collected() {
            let (_dir, repo, entries) = repo_with_range();
            let diff_options = DiffOptions::default();
            let patch_opts =
                PatchTextOptions::from_repo(&repo, diff_options.clone()).expect("patch opts");
            let mut pipelined = Vec::new();
            write_patch(&repo, &entries, &patch_opts, &mut pipelined).expect("write_patch");
            let structured = repo
                .diff()
                .structured(entries, &diff_options)
                .expect("structured");
            let mut collected = Vec::new();
            write_patch_structured(&repo, &structured, &patch_opts, &mut collected)
                .expect("write_patch_structured");
            assert_eq!(pipelined, collected);
        }
    }
}

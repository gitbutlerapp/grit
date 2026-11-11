//! `grit history` — stateless history rewrite helpers.

use std::io::{self, Write};

use anyhow::{bail, Context, Result};
use clap::ValueEnum;
use grit_lib::command_runner::hooks::PostOperationHooks;
use grit_lib::command_runner::system_command_runner;
use grit_lib::config::ident_resolve::IdentRole;
use grit_lib::error::Error as LibError;
use grit_lib::objects::ObjectId;
use grit_lib::porcelain::hunk_select::{parse_hunk_id_list, HunkSelection};
use grit_lib::repo::Repository;
use grit_lib::revwalk::history::{
    AbsorbOptions, ApplyOptions, EmptyPolicy, ExecRunRecord, ExecSpec, FixupOptions, HistoryError,
    Placement, PreRebase, RebaseCheckPlan, RebaseOntoOptions, RebaseOutcome, RebasePlan,
    RebaseRequest, RebaseSessionError, RefScope, RewordOptions, RewriteOptions, SplitOptions,
    SquashMessage, SquashOptions, StartRebaseOptions, UndoMode,
};
use grit_lib::revwalk::{Peel, ResolveOptions};

use crate::commands::merge_strategy::MergeStrategyArg;
use crate::context;
use crate::json_error;
use crate::message_input::{parse_author_cli, read_message_input, AuthorArg, MessageInputArgs};
use crate::pathspec_args::TrailingPathspecArgs;
use crate::render::operation::conflict_paths;

#[allow(unused_imports)]
pub use crate::render::history::{
    AbsorbAssignmentOut, AbsorbExcludedRefOut, ExecResultJson, HeadPair, HistoryAbsorbOutcome,
    HistoryRewriteOutcome, HistoryUndoOutcome, HunkRangeOut, RefPair, RewrittenPair,
    UnassignedHunkOut,
};
#[allow(unused_imports)]
pub use crate::render::rebase::{
    RebaseCheckOutcome, RebaseCheckResult, RebaseCliOutcome, RebaseResult, RebaseRunOutcome,
};

/// Which local refs a history rewrite may move.
#[derive(Debug, Clone, Copy, ValueEnum, Default, PartialEq, Eq)]
pub enum UpdateRefs {
    /// Every local branch descending from the edited commit, plus `HEAD` when applicable,
    /// except other branches at the `HEAD` commit (backups keep the old history).
    #[default]
    Branches,
    /// Only the commit `HEAD` currently points at.
    Head,
}

impl From<UpdateRefs> for RefScope {
    fn from(value: UpdateRefs) -> Self {
        match value {
            UpdateRefs::Branches => RefScope::Branches,
            UpdateRefs::Head => RefScope::Head,
        }
    }
}

/// How empty commits are handled during history rewrite.
#[derive(Debug, Clone, Copy, ValueEnum, Default, PartialEq, Eq)]
pub enum EmptyPolicyArg {
    /// Omit commits whose tree matches the new parent.
    #[default]
    Drop,
    /// Keep empty commits.
    Keep,
    /// Refuse when a commit would become empty.
    Abort,
}

impl From<EmptyPolicyArg> for EmptyPolicy {
    fn from(value: EmptyPolicyArg) -> Self {
        match value {
            EmptyPolicyArg::Drop => EmptyPolicy::Drop,
            EmptyPolicyArg::Keep => EmptyPolicy::Keep,
            EmptyPolicyArg::Abort => EmptyPolicy::Abort,
        }
    }
}

/// Extra flags for `grit rebase` / `grit history rebase`.
#[derive(Debug, clap::Args)]
pub struct RebaseRunFlags {
    /// Predict whether the rebase would succeed without changing refs, index, or worktree.
    #[arg(long = "check", conflicts_with_all = ["dry_run", "exec"])]
    pub check: bool,
    /// Auto-resolve replay conflicts: `ours` keeps the onto side, `theirs` keeps replayed commits.
    #[arg(short = 'X', value_enum, value_name = "OURS|THEIRS")]
    pub strategy: Option<MergeStrategyArg>,
    /// Run a shell command after each replayed commit is checked out (repeatable; incompatible with `--dry-run` and `--check`).
    #[arg(long = "exec", value_name = "CMD", conflicts_with = "dry_run")]
    pub exec: Vec<String>,
    /// Skip the `pre-rebase` hook.
    #[arg(long = "no-verify")]
    pub no_verify: bool,
    /// Use the upstream reflog to find where the branch forked (implicit upstream only unless set).
    #[arg(long = "fork-point", conflicts_with = "no_fork_point")]
    pub fork_point: bool,
    /// Do not use the upstream reflog fork point (merge base only).
    #[arg(long = "no-fork-point", conflicts_with = "fork_point")]
    pub no_fork_point: bool,
}

impl RebaseRunFlags {
    fn pre_rebase(&self, upstream: Option<&str>) -> PreRebase {
        if self.no_verify {
            PreRebase::Skip
        } else {
            PreRebase::Run {
                upstream: upstream.map(str::to_owned),
            }
        }
    }
}

/// Shared flags for history rewrite subcommands.
#[derive(Debug, clap::Args)]
pub struct HistoryRewriteArgs {
    /// Print planned ref updates without moving refs (new commit objects are still written).
    #[arg(long = "dry-run")]
    pub dry_run: bool,
    /// Which refs to rewrite: all descendant branches (default) or only `HEAD`.
    #[arg(long = "update-refs", value_enum, default_value_t = UpdateRefs::Branches)]
    pub update_refs: UpdateRefs,
    /// When a replayed commit would not change the tree: drop (default), keep an empty commit, or abort.
    #[arg(long = "empty", value_enum, default_value_t = EmptyPolicyArg::Drop)]
    pub empty: EmptyPolicyArg,
    /// Keep each replayed commit's original committer line instead of the configured committer.
    #[arg(long = "keep-committer")]
    pub keep_committer: bool,
}

/// Options for [`run_rebase`].
#[derive(Debug)]
pub struct RebaseRunOptions<'a> {
    pub upstream: Option<String>,
    pub onto: Option<String>,
    pub continue_rebase: bool,
    pub abort: bool,
    pub skip: bool,
    pub flags: &'a RebaseRunFlags,
    pub shared: &'a HistoryRewriteArgs,
}

/// Repository, config and rewrite options shared by every history subcommand.
struct Rewrite {
    repo: Repository,
    options: RewriteOptions,
}

impl Rewrite {
    fn new(repo: Repository, shared: &HistoryRewriteArgs) -> Result<Self> {
        let env = repo.environment();
        let committer =
            context::signature(&repo, IdentRole::Committer, context::wall_clock_now(env))?;
        Ok(Self {
            repo,
            options: RewriteOptions::new(committer)
                .with_scope(shared.update_refs.into())
                .with_empty(shared.empty.into())
                .with_preserve_committer(shared.keep_committer)
                .with_sign(!shared.dry_run),
        })
    }

    fn apply_options(&self, operation: &str, reflog_message: String) -> Result<ApplyOptions> {
        let now = context::wall_clock_now(self.repo.environment());
        Ok(ApplyOptions::new(
            context::signature(
                &self.repo,
                grit_lib::config::ident_resolve::IdentRole::Committer,
                now,
            )?,
            reflog_message,
        )
        .with_record_operation(Some(operation.into())))
    }

    fn subject(&self, oid: ObjectId) -> Result<String> {
        let commit = context::read_commit(&self.repo, &oid)?;
        Ok(context::subject_line(&commit.message))
    }
}

fn resolve_commit(repo: &Repository, rev: &str, what: &str) -> Result<ObjectId> {
    repo.revwalk()
        .resolve(rev, &ResolveOptions::default().with_peel(Peel::Commit))
        .with_context(|| format!("could not resolve {what} '{rev}'"))
}

/// Reorder `commit` relative to `--before` or `--after` on the first-parent chain of `HEAD`.
pub fn run_move(
    commit: &str,
    before: Option<String>,
    after: Option<String>,
    shared: &HistoryRewriteArgs,
) -> Result<HistoryRewriteOutcome> {
    let (anchor, placement): (String, fn(ObjectId) -> Placement) = match (before, after) {
        (Some(rev), _) => (rev, Placement::Before),
        (None, Some(rev)) => (rev, Placement::After),
        (None, None) => bail!("--before or --after is required"),
    };
    let on_err = history_error("history move");
    let repo = context::discover()?;
    let commit_oid = resolve_commit(&repo, commit, "commit")?;
    let anchor_oid = resolve_commit(&repo, &anchor, "anchor commit")?;
    let rewrite = Rewrite::new(repo, shared)?;
    let plan = rewrite
        .repo
        .history()
        .move_commit(commit_oid, placement(anchor_oid), &rewrite.options)
        .map_err(on_err)?;
    if !shared.dry_run {
        let _interrupt = crate::interrupt::InterruptCancel::install();
        let message = format!("history move: {}", commit_oid.to_short_hex());
        rewrite
            .repo
            .history()
            .apply(
                &plan,
                &rewrite.apply_options("move", message)?,
                &mut grit_lib::progress::NullProgress,
                &crate::interrupt::process_cancel_token(),
            )
            .map_err(on_err)?;
    }
    Ok(HistoryRewriteOutcome::new("move", &plan, shared.dry_run))
}

/// Remove `commit` from history and replay descendants onto its parent.
pub fn run_drop(commit: &str, shared: &HistoryRewriteArgs) -> Result<HistoryRewriteOutcome> {
    let on_err = history_error("history drop");
    let repo = context::discover()?;
    let commit_oid = resolve_commit(&repo, commit, "commit")?;
    let rewrite = Rewrite::new(repo, shared)?;
    let plan = rewrite
        .repo
        .history()
        .drop_commit(commit_oid, &rewrite.options)
        .map_err(on_err)?;
    rewrite.repo.history().verify_plan(&plan).map_err(on_err)?;
    if !shared.dry_run {
        let _interrupt = crate::interrupt::InterruptCancel::install();
        let message = format!("history drop: {}", rewrite.subject(commit_oid)?);
        rewrite
            .repo
            .history()
            .apply(
                &plan,
                &rewrite.apply_options("drop", message)?,
                &mut grit_lib::progress::NullProgress,
                &crate::interrupt::process_cancel_token(),
            )
            .map_err(on_err)?;
    }
    Ok(HistoryRewriteOutcome::new("drop", &plan, shared.dry_run))
}

/// Rewrite `commit`'s message and/or author and replay descendants.
pub fn run_reword(
    commit: &str,
    message: &MessageInputArgs,
    author: &AuthorArg,
    shared: &HistoryRewriteArgs,
) -> Result<HistoryRewriteOutcome> {
    let message = read_message_input(message)?;
    let author = author
        .author
        .as_deref()
        .filter(|a| !a.trim().is_empty())
        .map(str::to_owned);
    if let Some(raw) = author.as_deref() {
        parse_author_cli(raw)?;
    }
    let has_author = author.is_some();
    if message.is_none() && !has_author {
        bail!("reword requires a new message (-m/--message or --message-file) or --author");
    }
    let on_err = history_error("history reword");
    let repo = context::discover()?;
    let commit_oid = resolve_commit(&repo, commit, "commit")?;
    let rewrite = Rewrite::new(repo, shared)?;
    let mut reword = RewordOptions::default();
    reword.message = message;
    reword.author = author;
    let plan = rewrite
        .repo
        .history()
        .reword(commit_oid, &reword, &rewrite.options)
        .map_err(on_err)?;
    rewrite.repo.history().verify_plan(&plan).map_err(on_err)?;
    let new_oid = plan
        .replacement(commit_oid)
        .ok_or_else(|| anyhow::anyhow!("rewritten commit missing from plan"))?;
    if !shared.dry_run {
        let _interrupt = crate::interrupt::InterruptCancel::install();
        let message = format!("history reword: {}", rewrite.subject(new_oid)?);
        rewrite
            .repo
            .history()
            .apply(
                &plan,
                &rewrite.apply_options("reword", message)?,
                &mut grit_lib::progress::NullProgress,
                &crate::interrupt::process_cancel_token(),
            )
            .map_err(on_err)?;
    }
    Ok(HistoryRewriteOutcome::new("reword", &plan, shared.dry_run))
}

/// Apply staged changes onto `commit` and replay descendants.
pub fn run_fixup(
    commit: &str,
    message: &MessageInputArgs,
    hunks: &[String],
    lines: &[String],
    pathspecs: &TrailingPathspecArgs,
    shared: &HistoryRewriteArgs,
) -> Result<HistoryRewriteOutcome> {
    let message = read_message_input(message)?;
    let on_err = history_error("history fixup");
    let repo = context::discover()?;
    let commit_oid = resolve_commit(&repo, commit, "commit")?;
    let resolved = pathspecs.resolve(&repo)?;
    let pathspec_list = resolved.specs().to_vec();
    let hunk_selection = hunk_selection_from_args(hunks, lines)?;
    let rewrite = Rewrite::new(repo, shared)?;
    let empty = rewrite.options.empty;
    let plan = rewrite
        .repo
        .history()
        .fixup(
            commit_oid,
            &FixupOptions {
                message,
                empty,
                rewrite: rewrite.options.clone(),
                selection: hunk_selection.clone(),
                pathspecs: pathspec_list.clone(),
            },
        )
        .map_err(map_fixup_error)?;
    if !shared.dry_run {
        let _interrupt = crate::interrupt::InterruptCancel::install();
        let subject = match plan.replacement(commit_oid) {
            Some(new_oid) => rewrite.subject(new_oid)?,
            None => commit.to_owned(),
        };
        let apply_opts = rewrite.apply_options("fixup", format!("history fixup: {subject}"))?;
        if let Some(selection) = hunk_selection.as_ref() {
            rewrite
                .repo
                .history()
                .apply_fixup_hunks(
                    &plan,
                    &apply_opts,
                    selection,
                    &pathspec_list,
                    &crate::interrupt::process_cancel_token(),
                )
                .map_err(map_fixup_error)?;
        } else {
            let staged_paths = rewrite.repo.history().staged_paths().map_err(on_err)?;
            rewrite
                .repo
                .history()
                .apply_fixup(
                    &plan,
                    &apply_opts,
                    &staged_paths,
                    &crate::interrupt::process_cancel_token(),
                )
                .map_err(on_err)?;
        }
    }
    Ok(HistoryRewriteOutcome::new("fixup", &plan, shared.dry_run))
}

fn hunk_selection_from_args(hunks: &[String], lines: &[String]) -> Result<Option<HunkSelection>> {
    if hunks.is_empty() && lines.is_empty() {
        return Ok(None);
    }
    let mut selection = parse_hunk_id_list(hunks).map_err(crate::commands::add::map_hunk_error)?;
    selection.ranges = crate::commands::add::parse_line_range_list(lines)?;
    Ok(Some(selection))
}

fn map_fixup_error(err: LibError) -> anyhow::Error {
    match err {
        LibError::History(HistoryError::HunkSelect(h))
        | LibError::Diff(grit_lib::error::DiffError::HunkSelection(h)) => anyhow::Error::new(h),
        other => history_error("history fixup")(other),
    }
}

/// Squash a revision range into the oldest commit and replay descendants.
pub fn run_squash(
    range: &[String],
    message: &MessageInputArgs,
    combine: bool,
    shared: &HistoryRewriteArgs,
) -> Result<HistoryRewriteOutcome> {
    let message = if combine {
        SquashMessage::Combine
    } else {
        read_message_input(message)?.map_or(SquashMessage::Oldest, SquashMessage::Custom)
    };
    let on_err = history_error("history squash");
    let range_refs: Vec<&str> = range.iter().map(String::as_str).collect();
    let rewrite = Rewrite::new(context::discover()?, shared)?;
    let plan = rewrite
        .repo
        .history()
        .squash(
            &range_refs,
            &SquashOptions {
                message,
                rewrite: rewrite.options.clone(),
            },
        )
        .map_err(on_err)?;
    rewrite.repo.history().verify_plan(&plan).map_err(on_err)?;
    if !shared.dry_run {
        let _interrupt = crate::interrupt::InterruptCancel::install();
        let subject = match plan.commits.first() {
            Some(first) => rewrite.subject(first.new.unwrap_or(first.old))?,
            None => "squash".into(),
        };
        let message = format!("history squash: {subject}");
        rewrite
            .repo
            .history()
            .apply(
                &plan,
                &rewrite.apply_options("squash", message)?,
                &mut grit_lib::progress::NullProgress,
                &crate::interrupt::process_cancel_token(),
            )
            .map_err(on_err)?;
    }
    Ok(HistoryRewriteOutcome::new("squash", &plan, shared.dry_run))
}

/// Split path-limited changes out of `commit` into a new parent commit and replay descendants.
pub fn run_split(
    commit: &str,
    message: &MessageInputArgs,
    rest_message: Option<String>,
    paths: &TrailingPathspecArgs,
    shared: &HistoryRewriteArgs,
) -> Result<HistoryRewriteOutcome> {
    let Some(split_message) = read_message_input(message)? else {
        bail!("a message is required for the new commit (-m/--message or -F / --message-file)");
    };
    let on_err = history_error("history split");
    let repo = context::discover()?;
    paths.require_non_empty()?;
    let resolved = paths.resolve(&repo)?;
    let commit_oid = resolve_commit(&repo, commit, "commit")?;
    let rewrite = Rewrite::new(repo, shared)?;
    let plan = rewrite
        .repo
        .history()
        .split(
            commit_oid,
            resolved.specs(),
            split_message,
            &SplitOptions { rest_message },
            &rewrite.options,
        )
        .map_err(on_err)?;
    rewrite.repo.history().verify_plan(&plan).map_err(on_err)?;
    if !shared.dry_run {
        let _interrupt = crate::interrupt::InterruptCancel::install();
        let subject = rewrite.subject(commit_oid)?;
        rewrite
            .repo
            .history()
            .apply(
                &plan,
                &rewrite.apply_options("split", format!("history split: {subject}"))?,
                &mut grit_lib::progress::NullProgress,
                &crate::interrupt::process_cancel_token(),
            )
            .map_err(on_err)?;
    }
    Ok(HistoryRewriteOutcome::new("split", &plan, shared.dry_run))
}

/// Non-zero exit from `rebase --exec` (refs were not updated).
#[derive(Debug)]
pub struct RebaseExecExit {
    /// Subprocess exit code to return to the shell.
    pub code: i32,
}

impl std::fmt::Display for RebaseExecExit {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "rebase --exec command failed (exit {})", self.code)
    }
}

impl std::error::Error for RebaseExecExit {}

fn exec_rows(runs: &[ExecRunRecord]) -> Vec<ExecResultJson> {
    runs.iter()
        .map(|row| {
            write_exec_streams_to_stderr(row);
            ExecResultJson {
                commit: row.commit.to_hex(),
                command: row.command.clone(),
                exit_code: row.exit_code,
            }
        })
        .collect()
}

fn write_exec_streams_to_stderr(row: &ExecRunRecord) {
    let _ = io::stderr().write_all(&row.stdout);
    let _ = io::stderr().write_all(&row.stderr);
}

fn exec_failure_error(
    exit_code: i32,
    stdout: &[u8],
    stderr: &[u8],
    completed_runs: &[ExecRunRecord],
) -> anyhow::Error {
    for row in completed_runs {
        write_exec_streams_to_stderr(row);
    }
    let _ = io::stderr().write_all(stdout);
    let _ = io::stderr().write_all(stderr);
    RebaseExecExit { code: exit_code }.into()
}

fn run_rebase_with_exec(
    repo: &Repository,
    start: &StartRebaseOptions,
    exec: &[String],
) -> Result<RebaseRunOutcome> {
    // Do not install InterruptCancel here: while `--exec` runs a child command, SIGINT
    // must reach the process (or the exec runner) so the rebase can stop mid-exec.
    let mut start = start.clone();
    start.exec = Some(ExecSpec {
        commands: exec.to_vec(),
    });
    let runner = system_command_runner();
    let outcome = match repo.history().start_rebase(
        &start,
        runner.as_ref(),
        &mut grit_lib::progress::NullProgress,
        &grit_lib::progress::CancelToken::never(),
    ) {
        Ok(RebaseOutcome::Done { .. }) => {
            RebaseCliOutcome::from_rewrite(HistoryRewriteOutcome::up_to_date("rebase", false))
        }
        Ok(outcome) => map_rebase_outcome(outcome)?,
        Err(LibError::History(HistoryError::ExecDirty {
            commit,
            command,
            paths,
            runs,
        })) => RebaseCliOutcome::from_exec_dirty(commit, command, paths, exec_rows(&runs)),
        Err(LibError::History(HistoryError::ExecConflict {
            commit,
            conflicts,
            runs,
        })) => RebaseCliOutcome::from_exec_conflict(commit, conflicts.as_ref(), exec_rows(&runs)),
        Err(LibError::History(HistoryError::ExecFailed {
            exit_code,
            stdout,
            stderr,
            completed_runs,
            ..
        })) => {
            return Err(exec_failure_error(
                exit_code,
                &stdout,
                &stderr,
                &completed_runs,
            ));
        }
        Err(other) => return Err(map_rebase_session_error(repo, other)),
    };
    Ok(RebaseRunOutcome::Rebase(outcome))
}

/// Replay local commits onto a new base (`grit rebase` / `grit history rebase`).
pub fn run_rebase(opts: &RebaseRunOptions<'_>) -> Result<RebaseRunOutcome> {
    let shared = opts.shared;
    let flags = opts.flags;
    if !flags.exec.is_empty() && (opts.continue_rebase || opts.skip || opts.abort) {
        bail!("--exec cannot be used with --continue, --skip, or --abort");
    }
    let on_err = history_error("rebase");
    let repo = context::discover()?;
    if opts.abort {
        let abort = repo.history().abort_rebase(&reflog_identity(&repo)?).map_err(|err| match err {
            LibError::History(HistoryError::RebaseSession(ref session))
                if matches!(**session, RebaseSessionError::ForeignRebaseInProgress) =>
            {
                anyhow::anyhow!(
                    "this rebase was started by git and cannot be restored by grit; cancel it with `git rebase --abort`"
                )
            }
            other => map_rebase_session_error(&repo, other),
        })?;
        let mut outcome = RebaseCliOutcome::aborted();
        outcome.refs = abort
            .overwritten
            .iter()
            .map(|update| RefPair {
                ref_name: update.name.to_string(),
                name: update.name.to_string(),
                old: update.old.to_hex(),
                new: update.new.to_hex(),
            })
            .collect();
        return Ok(RebaseRunOutcome::Rebase(outcome));
    }

    let rewrite = Rewrite::new(repo, shared)?;
    let resume_message = if opts.skip {
        "rebase: skip"
    } else {
        "rebase: continue"
    };
    let resume_apply = rewrite.apply_options("rebase", resume_message.into())?;

    if opts.continue_rebase || opts.skip {
        let _interrupt = crate::interrupt::InterruptCancel::install();
        let head_oid = resolve_commit(&rewrite.repo, "HEAD", "HEAD")?;
        let resume_opts = StartRebaseOptions {
            request: RebaseRequest {
                upstream: head_oid,
                onto: None,
                branch_tip: head_oid,
            },
            rebase: RebaseOntoOptions::new(rewrite.options.clone()),
            apply: resume_apply,
            exec: None,
        };
        let outcome = if opts.continue_rebase {
            rewrite.repo.history().continue_rebase(
                &resume_opts,
                &mut grit_lib::progress::NullProgress,
                &crate::interrupt::process_cancel_token(),
            )
        } else {
            rewrite.repo.history().skip_rebase(
                &resume_opts,
                &mut grit_lib::progress::NullProgress,
                &crate::interrupt::process_cancel_token(),
            )
        };
        let rendered = match outcome {
            Ok(outcome) => map_rebase_outcome(outcome)?,
            Err(LibError::History(HistoryError::ExecDirty {
                commit,
                command,
                paths,
                runs,
            })) => RebaseCliOutcome::from_exec_dirty(commit, command, paths, exec_rows(&runs)),
            Err(LibError::History(HistoryError::ExecConflict {
                commit,
                conflicts,
                runs,
            })) => {
                RebaseCliOutcome::from_exec_conflict(commit, conflicts.as_ref(), exec_rows(&runs))
            }
            Err(LibError::History(HistoryError::ExecFailed {
                exit_code,
                stdout,
                stderr,
                completed_runs,
                ..
            })) => {
                return Err(exec_failure_error(
                    exit_code,
                    &stdout,
                    &stderr,
                    &completed_runs,
                ));
            }
            Err(other) => return Err(map_rebase_session_error(&rewrite.repo, other)),
        };
        return Ok(RebaseRunOutcome::Rebase(rendered));
    }

    let branch_tip = resolve_commit(&rewrite.repo, "HEAD", "HEAD")?;
    let upstream_oid = resolve_rebase_upstream(&rewrite.repo, opts.upstream.as_deref())?;
    let onto_oid = opts
        .onto
        .as_deref()
        .map(|rev| resolve_commit(&rewrite.repo, rev, "onto"))
        .transpose()?;
    let fork_override = if flags.fork_point {
        Some(true)
    } else if flags.no_fork_point {
        Some(false)
    } else {
        None
    };
    let request = rewrite
        .repo
        .history()
        .rebase_run_request(
            branch_tip,
            upstream_oid,
            onto_oid,
            opts.upstream.as_deref(),
            opts.onto.is_some(),
            fork_override,
        )
        .map_err(on_err)?;
    let mut rebase_opts = RebaseOntoOptions::new(rewrite.options.clone());
    if flags.check {
        rebase_opts.rewrite.sign = false;
    }
    if let Some(strategy) = flags.strategy {
        rebase_opts.favor = strategy.into();
    }
    if flags.check {
        return run_rebase_check(&rewrite.repo, &request, &rebase_opts, on_err);
    }

    if !flags.exec.is_empty() {
        if shared.dry_run {
            bail!("--exec cannot be used with --dry-run");
        }
        if flags.strategy.is_some() {
            bail!("--exec cannot be used with -X");
        }
    }

    let effective_onto = onto_oid.unwrap_or(upstream_oid);
    let apply_message = format!("rebase: onto {}", effective_onto.to_short_hex());
    let mut start_opts = StartRebaseOptions {
        request,
        rebase: rebase_opts.clone(),
        apply: rewrite.apply_options("rebase", apply_message)?,
        exec: None,
    };
    start_opts.rebase.pre_rebase = flags.pre_rebase(opts.upstream.as_deref());

    if !flags.exec.is_empty() {
        return run_rebase_with_exec(&rewrite.repo, &start_opts, &flags.exec);
    }

    if shared.dry_run {
        let plan = rewrite
            .repo
            .history()
            .plan_rebase(&start_opts.request, &rebase_opts)
            .map_err(on_err)?;
        return match plan {
            RebasePlan::UpToDate => Ok(RebaseRunOutcome::Rebase(RebaseCliOutcome::from_rewrite(
                HistoryRewriteOutcome::up_to_date("rebase", true),
            ))),
            RebasePlan::Conflict { .. } => bail!(
                "rebase would conflict; run without --dry-run to start a resumable rebase session"
            ),
            RebasePlan::Plan(plan) => {
                rewrite.repo.history().verify_plan(&plan).map_err(on_err)?;
                Ok(RebaseRunOutcome::Rebase(RebaseCliOutcome::from_rewrite(
                    HistoryRewriteOutcome::new("rebase", &plan, true),
                )))
            }
            _ => bail!("rebase planned an outcome this version of grit cannot handle"),
        };
    }

    let _interrupt = crate::interrupt::InterruptCancel::install();
    let outcome = rewrite
        .repo
        .history()
        .start_rebase(
            &start_opts,
            rewrite.repo.command_runner().as_ref(),
            &mut grit_lib::progress::NullProgress,
            &crate::interrupt::process_cancel_token(),
        )
        .map_err(|err| map_rebase_session_error(&rewrite.repo, err))?;
    Ok(RebaseRunOutcome::Rebase(map_rebase_outcome(outcome)?))
}

fn reflog_identity(repo: &Repository) -> Result<grit_lib::ident::Signature> {
    let env = repo.environment();
    let now = context::wall_clock_now(env);
    context::signature(
        repo,
        grit_lib::config::ident_resolve::IdentRole::Committer,
        now,
    )
}

fn omit_identity_rewrite_rows(outcome: &mut HistoryRewriteOutcome) {
    outcome.rewritten.retain(|pair| {
        pair.new
            .as_deref()
            .is_none_or(|new| new != pair.old.as_str())
    });
}

fn map_rebase_outcome(outcome: RebaseOutcome) -> Result<RebaseCliOutcome> {
    match outcome {
        RebaseOutcome::Done {
            plan,
            head_change,
            rerere,
            ..
        } => {
            let mut outcome = HistoryRewriteOutcome::new("rebase", &plan, false);
            outcome.head = head_change.map(|(old, new)| HeadPair {
                old: old.to_hex(),
                new: new.to_hex(),
            });
            let mut cli = RebaseCliOutcome::from_rewrite(outcome);
            cli.rerere = crate::render::operation::rerere_events_json(&rerere);
            Ok(cli)
        }
        RebaseOutcome::ExecDone { plan, exec, .. } => {
            let mut outcome =
                HistoryRewriteOutcome::new("rebase", &plan, false).with_exec(exec_rows(&exec.runs));
            omit_identity_rewrite_rows(&mut outcome);
            Ok(RebaseCliOutcome::from_rewrite(outcome))
        }
        RebaseOutcome::Stopped(stop) => Ok(RebaseCliOutcome::from_stop(
            stop.commit,
            &stop.conflicts,
            &stop.rerere,
        )),
        _ => bail!("rebase finished with an outcome this version of grit cannot display"),
    }
}

fn map_rebase_session_error(repo: &Repository, err: LibError) -> anyhow::Error {
    let err = match err {
        LibError::History(HistoryError::RebaseSession(session)) => *session,
        LibError::Commit(grit_lib::porcelain::CommitError::NothingToCommit) => {
            RebaseSessionError::NothingToCommit
        }
        other => return history_error("rebase")(other),
    };
    match err {
        RebaseSessionError::NoRebaseInProgress => anyhow::Error::new(LibError::from(err)),
        RebaseSessionError::RebaseAlreadyInProgress => {
            let kind = repo.operation().blocking_switch()
                .ok()
                .flatten()
                .unwrap_or(grit_lib::state::InProgressOperation::Rebase);
            json_error::operation_in_progress_for_repo(repo, "rebase", kind)
        }
        RebaseSessionError::ForeignRebaseInProgress => anyhow::anyhow!(
            "this rebase was started by git; finish it with `git rebase --continue` or `git rebase --skip`, or cancel it with `grit rebase --abort`"
        ),
        RebaseSessionError::UnmergedPaths => anyhow::Error::new(LibError::from(err)),
        RebaseSessionError::NothingToCommit => {
            anyhow::anyhow!("nothing to commit after conflict resolution")
        }
        RebaseSessionError::ResolutionHeadChanged { expected } => anyhow::anyhow!(
            "HEAD moved away from the conflict resolution parent (expected {}); save commits on a branch or reset before `grit rebase --continue`",
            expected.to_short_hex()
        ),
        RebaseSessionError::NotOnBranch => {
            anyhow::anyhow!("rebase session requires HEAD on a branch")
        }
        RebaseSessionError::StaleExecPlan {
            name,
            expected,
            actual,
        } => anyhow::anyhow!(
            "{name} moved to {} while the rebase was stopped, so it cannot be continued or skipped; \
             run `grit rebase --abort` to restore {name} to {} (the moved tip stays in its reflog)",
            actual.map_or_else(|| "(deleted)".to_owned(), |oid| oid.to_short_hex()),
            expected.to_short_hex()
        ),
        RebaseSessionError::AbortRefMoved { name } => anyhow::anyhow!(
            "{name} moved while the rebase was being aborted; retry `grit rebase --abort`"
        ),
        RebaseSessionError::InvalidStateOid { value } => anyhow::anyhow!(
            "the saved rebase plan is corrupt ({value:?} is not an object id); remove .git/rebase-merge to discard it"
        ),
        _ => anyhow::Error::new(LibError::from(err)),
    }
}

fn run_rebase_check(
    repo: &Repository,
    request: &RebaseRequest,
    opts: &RebaseOntoOptions,
    on_err: impl Fn(LibError) -> anyhow::Error + Copy,
) -> Result<RebaseRunOutcome> {
    match repo
        .history()
        .plan_rebase_check(request, opts)
        .map_err(on_err)?
    {
        RebaseCheckPlan::UpToDate => Ok(RebaseRunOutcome::Check(RebaseCheckOutcome::up_to_date())),
        RebaseCheckPlan::Clean { commits, .. } => {
            Ok(RebaseRunOutcome::Check(RebaseCheckOutcome::clean(commits)))
        }
        RebaseCheckPlan::Conflict {
            commit,
            conflicts,
            refs,
            ..
        } => Ok(RebaseRunOutcome::Check(RebaseCheckOutcome::conflicts(
            commit, &conflicts, refs,
        ))),
        _ => bail!("rebase --check returned a result this version of grit cannot display"),
    }
}

fn resolve_rebase_upstream(repo: &Repository, upstream: Option<&str>) -> Result<ObjectId> {
    if upstream.is_none()
        && matches!(
            repo.references().head()?,
            grit_lib::state::HeadState::Detached { .. }
        )
    {
        anyhow::bail!("cannot rebase detached HEAD without an upstream argument");
    }
    let spec = upstream.unwrap_or("@{upstream}");
    crate::error_map::resolve_rebase_upstream_revision(
        spec,
        upstream.is_none(),
        repo.revwalk()
            .resolve(spec, &ResolveOptions::default().with_peel(Peel::Commit)),
    )
}

/// Restore refs (and the worktree when needed) from the last recorded history rewrite.
pub fn run_undo(shared: &HistoryRewriteArgs) -> Result<HistoryUndoOutcome> {
    let on_err = history_error("history undo");
    let repo = context::discover()?;
    let rewrite = Rewrite::new(repo, shared)?;
    let now = context::wall_clock_now(rewrite.repo.environment());
    let apply_opts = ApplyOptions::new(
        context::signature(
            &rewrite.repo,
            grit_lib::config::ident_resolve::IdentRole::Committer,
            now,
        )?,
        "history undo",
    )
    .with_hooks(PostOperationHooks::Skip);
    let _interrupt = crate::interrupt::InterruptCancel::install();
    let mode = if shared.dry_run {
        UndoMode::DryRun
    } else {
        UndoMode::Apply
    };
    let outcome = rewrite
        .repo
        .history()
        .undo_last(&apply_opts, mode, &crate::interrupt::process_cancel_token())
        .map_err(on_err)?;
    let rewrite = HistoryRewriteOutcome {
        operation: "undo".into(),
        rewritten: Vec::new(),
        refs: outcome
            .ref_updates
            .iter()
            .map(|u| RefPair {
                ref_name: u.name.to_string(),
                name: u.name.to_string(),
                old: u.old.to_hex(),
                new: u.new.to_hex(),
            })
            .collect(),
        head: outcome.head_change.map(|(old, new)| HeadPair {
            old: old.to_hex(),
            new: new.to_hex(),
        }),
        dry_run: outcome.dry_run,
        exec: Vec::new(),
    };
    Ok(HistoryUndoOutcome {
        rewrite,
        restored_paths: outcome.restored_paths,
        unrestored_edits: outcome.unrestored_edits.map(|oid| oid.to_hex()),
    })
}

/// Assign staged hunks into history and replay descendants once.
pub fn run_absorb(base: Option<&str>, shared: &HistoryRewriteArgs) -> Result<HistoryAbsorbOutcome> {
    let on_err = history_error("history absorb");
    let repo = context::discover()?;
    let base = base
        .map(|rev| resolve_commit(&repo, rev, "base commit"))
        .transpose()?;
    let rewrite = Rewrite::new(repo, shared)?;
    let absorb_plan = rewrite
        .repo
        .history()
        .absorb(&AbsorbOptions {
            base,
            rewrite: rewrite.options.clone(),
        })
        .map_err(on_err)?;
    if !shared.dry_run && !absorb_plan.plan.commits.is_empty() {
        let _interrupt = crate::interrupt::InterruptCancel::install();
        rewrite
            .repo
            .history()
            .apply_absorb(
                &absorb_plan,
                &rewrite.apply_options("absorb", "history absorb".into())?,
                &crate::interrupt::process_cancel_token(),
            )
            .map_err(on_err)?;
    }
    let summary = HistoryRewriteOutcome::new("absorb", &absorb_plan.plan, shared.dry_run);
    Ok(HistoryAbsorbOutcome {
        operation: summary.operation,
        assignments: absorb_plan.assignments.iter().map(Into::into).collect(),
        unassigned: absorb_plan.unassigned.iter().map(Into::into).collect(),
        excluded_refs: absorb_plan
            .excluded_refs
            .iter()
            .map(|r| AbsorbExcludedRefOut {
                ref_name: r.ref_name.clone(),
                commit: r.tip.to_hex(),
            })
            .collect(),
        rewritten: summary.rewritten,
        refs: summary.refs,
        head: summary.head,
        dry_run: summary.dry_run,
    })
}

pub(crate) fn history_error(operation: &'static str) -> impl Fn(LibError) -> anyhow::Error + Copy {
    move |err| match err {
        LibError::History(history) => map_history_error(history, operation),
        other => anyhow::Error::new(other),
    }
}

fn map_history_error(err: HistoryError, operation: &str) -> anyhow::Error {
    match err {
        HistoryError::Planning(planning) => {
            anyhow::anyhow!("history rewrite planning failed: {planning}")
        }
        HistoryError::EmptyMessage => anyhow::anyhow!("commit message cannot be empty"),
        HistoryError::NothingToReword => {
            anyhow::anyhow!("reword requires a new message or author")
        }
        HistoryError::InvalidAuthor(err) => anyhow::anyhow!("{err}"),
        HistoryError::BareRepository => {
            anyhow::anyhow!("cannot run {operation} in a bare repository")
        }
        HistoryError::NothingStaged => {
            anyhow::anyhow!("nothing to {operation}: no staged changes")
        }
        HistoryError::FixupMakesTargetEmpty { commit } => {
            let short = commit.to_short_hex();
            anyhow::anyhow!("{operation} makes commit {short} empty")
        }
        HistoryError::Conflict {
            commit,
            subject,
            conflicts,
            refs,
        } => json_error::replay_conflict(
            operation,
            commit,
            &subject,
            conflict_paths(conflicts.as_ref().clone()),
            refs,
        ),
        HistoryError::Signing {
            program,
            status,
            stderr,
        } => anyhow::anyhow!(
            "commit signing failed (program {program}, status {status:?}): {stderr}"
        ),
        HistoryError::InsertBeforeOnMerge { commit } => {
            let short = commit.to_short_hex();
            anyhow::anyhow!("insert-before on merge commit {short} is not supported")
        }
        HistoryError::MergeCommitInReplay { commit } => {
            let short = commit.to_short_hex();
            anyhow::anyhow!(
                "{short} is a merge commit — {operation} cannot drop or reorder merge commits yet"
            )
        }
        HistoryError::RootCommit { commit } => {
            let short = commit.to_short_hex();
            anyhow::anyhow!(
                "{short} is a root commit and cannot be dropped or replayed without a parent"
            )
        }
        HistoryError::BecameEmpty { commit } => {
            let short = commit.to_short_hex();
            anyhow::anyhow!("replay of {short} would become empty — refusing rewrite")
        }
        HistoryError::NotInHistory { commit } => {
            let short = commit.to_short_hex();
            anyhow::anyhow!("{short} is not reachable from HEAD for this rewrite scope")
        }
        HistoryError::MoveTargetNotInHistory { commit } => {
            let short = commit.to_short_hex();
            anyhow::anyhow!("{short} is not on the linear first-parent history of HEAD")
        }
        HistoryError::NoOp { commit } => {
            let short = commit.to_short_hex();
            anyhow::anyhow!("{short} is already at the requested position")
        }
        HistoryError::NotLinear => anyhow::anyhow!(
            "history move requires a linear first-parent range without merge commits"
        ),
        HistoryError::DivergentBranch { name } => anyhow::anyhow!(
            "branch {name} diverged before the rewritten commits and cannot be updated"
        ),
        HistoryError::BranchCheckedOutElsewhere { name, worktree } => {
            anyhow::anyhow!("branch {name} is checked out in worktree {worktree}")
        }
        HistoryError::LocalChangesWouldBeOverwritten { mut paths } => {
            paths.sort();
            paths.dedup();
            anyhow::anyhow!(
                "local changes would be overwritten:\n  {}",
                paths.join("\n  ")
            )
        }
        HistoryError::RefCasMismatch { name } => anyhow::anyhow!(
            "ref {name} changed since the rewrite plan was built — retry the command"
        ),
        HistoryError::HeadMovedSincePlan {
            planned_ref,
            planned_commit,
        } => {
            let oid = planned_commit.to_short_hex();
            match planned_ref {
                Some(name) => anyhow::anyhow!(
                    "HEAD moved since the rewrite plan was built (expected {name} at {oid})"
                ),
                None => {
                    anyhow::anyhow!("HEAD moved since the rewrite plan was built (expected {oid})")
                }
            }
        }
        HistoryError::RefApply(store) => anyhow::anyhow!("ref update failed: {store}"),
        HistoryError::WorktreeApply { source } => {
            anyhow::anyhow!("worktree update failed: {source}")
        }
        HistoryError::SplitSelectsNothing => {
            anyhow::anyhow!("pathspec did not select any changes from the commit")
        }
        HistoryError::SplitSelectsEverything => {
            anyhow::anyhow!("pathspec selected every change in the commit")
        }
        HistoryError::NoUndoRecord => anyhow::anyhow!("no history rewrite to undo"),
        HistoryError::UndoStale { name } => anyhow::anyhow!(
            "ref {name} changed since the last history rewrite — undo is no longer safe"
        ),
        HistoryError::UndoRecordRead { source } | HistoryError::UndoRecordWrite { source } => {
            anyhow::anyhow!("history undo record error: {source}")
        }
        HistoryError::HunkSelect(h) => anyhow::Error::new(h),
        HistoryError::ChangeId(h) => anyhow::Error::new(h),
        HistoryError::RangeNotConnected => {
            anyhow::anyhow!("the selected commits are not a connected revision range")
        }
        HistoryError::MultipleTips => {
            anyhow::anyhow!("the revision range contains more than one tip commit")
        }
        HistoryError::MarkerTargetOutsideRange { commit } => {
            let short = commit.to_short_hex();
            anyhow::anyhow!(
                "cannot squash {short}: its fixup/amend/squash target is outside the range"
            )
        }
        HistoryError::SquashDiscardsMessage { commit } => {
            let short = commit.to_short_hex();
            anyhow::anyhow!("squashing {short} would discard its message; use --combine or -m")
        }
        HistoryError::UncommittedChanges => anyhow::anyhow!(
            "you have uncommitted changes — commit them before rebasing with --exec"
        ),
        HistoryError::ExecDirty {
            commit,
            command,
            paths,
            runs: _,
        } => {
            let short = commit.to_short_hex();
            let mut paths = paths;
            paths.sort();
            paths.dedup();
            anyhow::anyhow!(
                "rebase --exec left tracked changes at commit {short} after `{command}`; save the edits before continuing, or abort the rebase:\n  {}",
                paths.join("\n  ")
            )
        }
        HistoryError::ExecConflict {
            commit,
            conflicts,
            runs: _,
        } => json_error::replay_conflict(
            operation,
            commit,
            "(during rebase --exec)",
            conflict_paths(conflicts.as_ref().clone()),
            Vec::new(),
        ),
        HistoryError::ExecFailed {
            commit,
            exit_code,
            stdout,
            stderr,
            completed_runs: _,
        } => {
            let short = commit.to_short_hex();
            let stdout = String::from_utf8_lossy(&stdout);
            let stderr = String::from_utf8_lossy(&stderr);
            anyhow::anyhow!(
                "rebase --exec command failed at commit {short} (exit {exit_code})\nstdout:\n{stdout}\nstderr:\n{stderr}"
            )
        }
        HistoryError::ExecHeadMoved { commit, head } => {
            let short = commit.to_short_hex();
            match head {
                Some(h) => anyhow::anyhow!(
                    "rebase --exec command moved HEAD away from commit {short}; HEAD is now {}",
                    h.to_short_hex()
                ),
                None => {
                    anyhow::anyhow!("rebase --exec command moved HEAD away from commit {short}")
                }
            }
        }
        HistoryError::RewriteApplyIncomplete => {
            anyhow::anyhow!("history rewrite checkout is incomplete")
        }
        HistoryError::ApplyRollbackIncomplete {
            checkout,
            ref_rollback,
            worktree_restore,
        } => {
            let mut parts = vec![format!(
                "checkout failed after refs were updated: {checkout}"
            )];
            if let Some(r) = ref_rollback {
                parts.push(format!("ref rollback also failed: {r}"));
            }
            if let Some(w) = worktree_restore {
                parts.push(format!("worktree restore also failed: {w}"));
            }
            anyhow::Error::msg(parts.join("; "))
        }
        other => anyhow::anyhow!("{other}"),
    }
}

/// Dispatch parsed CLI args for `History`.
pub fn dispatch(
    args: crate::args::HistoryArgs,
    env: crate::dispatch::CommandEnv<'_>,
) -> anyhow::Result<crate::exit_status::ExitStatus> {
    use crate::args::*;
    use crate::commands;

    use crate::dispatch::{emit_rebase_outcome, emit_success, finish_hunk_error, CommandEnv};

    let CommandEnv {
        opts,
        ctx,
        process_env: _,
    } = env;
    let HistoryArgs { action } = args;
    match action {
        HistoryAction::Drop { commit, shared } => {
            emit_success(commands::history::run_drop(&commit, &shared)?, opts, ctx)
        }
        HistoryAction::Move {
            commit,
            before,
            after,
            shared,
        } => emit_success(
            commands::history::run_move(&commit, before, after, &shared)?,
            opts,
            ctx,
        ),
        HistoryAction::Reword {
            commit,
            message,
            author,
            shared,
        } => emit_success(
            commands::history::run_reword(&commit, &message, &author, &shared)?,
            opts,
            ctx,
        ),
        HistoryAction::Fixup {
            commit,
            message,
            hunks,
            lines,
            pathspecs,
            shared,
        } => {
            let outcome = commands::history::run_fixup(
                &commit, &message, &hunks, &lines, &pathspecs, &shared,
            );
            match outcome {
                Ok(out) => emit_success(out, opts, ctx),
                Err(err) => {
                    let code = commands::add::hunks_exit_code(&err);
                    Ok(finish_hunk_error(err, opts, code))
                }
            }
        }
        HistoryAction::Rebase {
            upstream,
            onto,
            continue_rebase,
            abort,
            skip,
            rebase_flags,
            shared,
        } => {
            let outcome = commands::history::run_rebase(&commands::history::RebaseRunOptions {
                upstream,
                onto,
                continue_rebase,
                abort,
                skip,
                flags: &rebase_flags,
                shared: &shared,
            })?;
            emit_rebase_outcome(outcome, opts, ctx)
        }
        HistoryAction::Absorb { base, shared } => emit_success(
            commands::history::run_absorb(base.as_deref(), &shared)?,
            opts,
            ctx,
        ),
        HistoryAction::Undo { shared } => {
            emit_success(commands::history::run_undo(&shared)?, opts, ctx)
        }
        HistoryAction::Squash {
            range,
            message,
            combine,
            shared,
        } => emit_success(
            commands::history::run_squash(&range, &message, combine, &shared)?,
            opts,
            ctx,
        ),
        HistoryAction::Split {
            commit,
            message,
            rest_message,
            paths,
            shared,
        } => emit_success(
            commands::history::run_split(&commit, &message, rest_message, &paths, &shared)?,
            opts,
            ctx,
        ),
    }
}

/// Dispatch parsed CLI args for `Rebase`.
pub fn dispatch_rebase(
    args: crate::args::RebaseArgs,
    env: crate::dispatch::CommandEnv<'_>,
) -> anyhow::Result<crate::exit_status::ExitStatus> {
    use crate::args::*;
    use crate::commands;

    use crate::dispatch::{emit_rebase_outcome, CommandEnv};

    let CommandEnv {
        opts,
        ctx,
        process_env: _,
    } = env;
    let RebaseArgs {
        upstream,
        onto,
        continue_rebase,
        abort,
        skip,
        rebase_flags,
        shared,
    } = args;
    {
        let outcome = commands::history::run_rebase(&commands::history::RebaseRunOptions {
            upstream,
            onto,
            continue_rebase,
            abort,
            skip,
            flags: &rebase_flags,
            shared: &shared,
        })?;
        emit_rebase_outcome(outcome, opts, ctx)
    }
}

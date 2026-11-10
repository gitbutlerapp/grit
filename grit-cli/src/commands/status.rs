//! `grit status` (and bare `grit`) — the dashboard: where you are, the commits
//! you're ahead by, what's changed, and what to do next.

use anyhow::{Context, Result};
use grit_lib::diff::DiffEntry;
use grit_lib::gitbutler::{GitButlerWorkspace, WorkspaceStanding};
use grit_lib::objects::ObjectId;
use grit_lib::operation::Conflict;
use grit_lib::porcelain::status::{
    short_status_lines, ConflictGroup, IgnoredMode, InterruptedRewrite, StatusGuidance,
    UntrackedMode,
};
use grit_lib::progress::CancelToken;
use grit_lib::progress::NullProgress;
use grit_lib::refs::branch_tracking::{
    status_ahead_behind_enabled, BranchStanding, StandingOptions, StatusHeader,
    StatusIntegrationHint, TrackedComparison,
};
use grit_lib::revwalk::AbbrevLength;
use grit_lib::state::{HeadState, InProgressOperation, NextStep};
use grit_lib::status::{
    filter_guidance_for_case_collisions, worktree_case_collision_paths, StatusOptions,
    StatusSummaryOptions,
};
use schemars::JsonSchema;
use serde::Serialize;

use crate::context::{self, CommitSummary};
use crate::json_error::LockHolderJson;
use crate::output::{change_json_with_worktree, ChangeJson, CommitJson};
use crate::pathspec_args::PathspecArgs;
use crate::ui::PathDisplayOptions;
use clap::Args;

/// Maximum number of commits to list in the status shortlog before summarizing.
pub(crate) const SHORTLOG_LIMIT: usize = 10;

/// Case-insensitive tracked paths that share one work-tree file in `--json` output.
#[derive(Serialize, JsonSchema)]
pub struct WorktreeCaseCollisionJson {
    /// Index spellings in the collision group.
    pub paths: Vec<String>,
}

/// One unmerged path in `--json` output.
#[derive(Serialize, JsonSchema)]
pub struct ConflictJson {
    /// Repository-relative path.
    pub path: String,
    /// Conflict category (`both_modified`, `both_added`, …).
    pub kind: String,
}

/// Paused operation and suggested `grit` commands.
#[derive(Serialize, JsonSchema)]
pub struct OperationJson {
    /// Stable operation id (for example `merge`).
    pub kind: String,
    /// Suggested follow-up commands, in order. A `git …` command appears only for an
    /// operation `git` started that grit cannot resume (for example `git rebase --continue`).
    pub next_steps: Vec<String>,
    /// Mutually exclusive options (for example cancel the merge).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    #[schemars(default)]
    pub alternatives: Vec<String>,
}

/// Result of `grit status`.
#[derive(Serialize, JsonSchema)]
pub struct StatusOutcome {
    /// Current branch (short name), or `null` when detached / invalid. While a rebase is in
    /// progress, the branch being rebased (`null` when the rebase started detached).
    pub branch: Option<String>,
    pub detached: bool,
    /// HEAD commit (full oid), or `null` on an unborn / invalid HEAD.
    pub head: Option<String>,
    /// Collision-aware abbreviated HEAD oid for human and Markdown output.
    #[serde(skip)]
    pub head_short: Option<String>,
    /// Target branch name, or `null` when none applies / was found.
    pub target: Option<String>,
    /// Number of commits ahead of `target` (full count), or `null` when counts were skipped.
    pub ahead: Option<usize>,
    /// Number of commits on `target` not in the current branch (full count), or `null` when skipped.
    pub behind: Option<usize>,
    /// Upstream tracking comparison when it differs from `target`, or `null`.
    pub upstream: Option<StatusUpstreamJson>,
    /// Newest ahead-of-target commits (newest first), up to the status shortlog limit.
    pub commits: Vec<CommitJson>,
    pub staged: Vec<ChangeJson>,
    pub unstaged: Vec<ChangeJson>,
    pub untracked: Vec<String>,
    /// Untracked nested repositories that would be recorded as gitlinks if staged.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub embedded_repositories: Vec<String>,
    pub clean: bool,
    /// True when a merge is in progress (`MERGE_HEAD` exists). Deprecated: prefer `operation`.
    pub merging: bool,
    /// In-progress operations (e.g. `merge`, `rebase`), when any apply. Deprecated: prefer `operation`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    #[schemars(default)]
    pub in_progress: Vec<String>,
    /// Unmerged paths with conflict kind.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    #[schemars(default)]
    pub conflicts: Vec<ConflictJson>,
    /// Primary paused operation and suggested next commands.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(default)]
    pub operation: Option<OperationJson>,
    /// Ignored paths (only when `--ignored` is passed).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(default)]
    pub ignored: Option<Vec<String>>,
    /// Number of entries on the stash reflog.
    pub stash_count: usize,
    /// What status did about a rebase or history edit that was killed after moving refs, when one
    /// was pending.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(default)]
    pub interrupted_rewrite: Option<InterruptedRewriteJson>,
    /// The GitButler workspace `HEAD` is on, when it is on one. `head` is then the workspace
    /// commit, and `commits` and `ahead` leave it out.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(default)]
    pub gitbutler_workspace: Option<GitButlerWorkspaceJson>,
    /// Non-fatal warnings emitted while scanning the worktree (for example unreadable directories).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    #[schemars(default)]
    pub warnings: Vec<crate::diagnostics::WarningRecord>,
    /// Tracked path groups that collide on this case-insensitive work tree.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub worktree_case_collisions: Vec<WorktreeCaseCollisionJson>,
    /// When the branch is shared (`grit share`): the other members' heads, as last fetched.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(default)]
    pub shared: Option<StatusSharedJson>,

    // Human-only state, not part of the JSON schema.
    #[serde(skip)]
    #[schemars(skip)]
    pub(crate) header: StatusHeader,
    #[serde(skip)]
    #[schemars(skip)]
    pub(crate) commit_rows: Vec<CommitSummary>,
    #[serde(skip)]
    #[schemars(skip)]
    pub(crate) staged_entries: Vec<DiffEntry>,
    #[serde(skip)]
    #[schemars(skip)]
    pub(crate) unstaged_entries: Vec<DiffEntry>,
    #[serde(skip)]
    #[schemars(skip)]
    pub(crate) path_display: Option<PathDisplayOptions>,
    #[serde(skip)]
    #[schemars(skip)]
    pub(crate) show_counts: bool,
    #[serde(skip)]
    #[schemars(skip)]
    pub(crate) standing: BranchStanding,
    #[serde(skip)]
    #[schemars(skip)]
    pub(crate) conflict_groups: Vec<ConflictGroup>,
    #[serde(skip)]
    #[schemars(skip)]
    pub(crate) guidance: StatusGuidance,
    #[serde(skip)]
    #[schemars(skip)]
    pub(crate) integration_hint: Option<StatusIntegrationHint>,
    #[serde(skip)]
    #[schemars(skip)]
    pub(crate) short_format: bool,
    #[serde(skip)]
    #[schemars(skip)]
    pub(crate) short_lines: Vec<String>,
    #[serde(skip)]
    #[schemars(skip)]
    pub(crate) worktree_case_collision_paths: Vec<String>,
    #[serde(skip)]
    #[schemars(skip)]
    pub(crate) interrupted_rewrite_lock_holder: Option<LockHolderJson>,
    /// Rows for the workspace's merged heads, in the same order as `gitbutler_workspace.heads`.
    #[serde(skip)]
    #[schemars(skip)]
    pub(crate) workspace_rows: Vec<CommitSummary>,
}

/// A GitButler workspace in `grit status --json`: GitButler's merge of the applied branches.
#[derive(Serialize, JsonSchema)]
pub struct GitButlerWorkspaceJson {
    /// The workspace commit at the tip of `gitbutler/workspace` (full oid).
    pub commit: String,
    /// The heads merged into the workspace, in its parent order.
    pub heads: Vec<WorkspaceHeadJson>,
}

/// One head merged into a GitButler workspace.
#[derive(Serialize, JsonSchema)]
pub struct WorkspaceHeadJson {
    /// The head commit (full oid).
    pub commit: String,
    /// Local branches that point at it, sorted; empty when none does.
    pub branches: Vec<String>,
    /// The head commit's subject.
    pub subject: String,
    /// Commits on this head not on `target`, or `null` when counts were skipped or there is no
    /// target.
    pub ahead: Option<usize>,
}

/// Recovery of an interrupted rebase or history edit, in `--json` output.
#[derive(Clone, Copy, Serialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum InterruptedRewriteJson {
    /// Refs had already moved: the checkout was completed and the rewrite stands.
    FinishedCheckout,
    /// The rewrite was undone: refs, index and files are back to their earlier state.
    RolledBack,
    /// Another process holds `index.lock`; recovery runs on a later command.
    Pending,
}

impl From<InterruptedRewrite> for InterruptedRewriteJson {
    fn from(outcome: InterruptedRewrite) -> Self {
        match outcome {
            InterruptedRewrite::FinishedCheckout => Self::FinishedCheckout,
            InterruptedRewrite::RolledBack => Self::RolledBack,
            InterruptedRewrite::Pending => Self::Pending,
            // Treat recovery states this CLI does not know yet as still pending.
            _ => Self::Pending,
        }
    }
}

impl InterruptedRewriteJson {
    /// One line for human and Markdown output; `lock` is how the index lock path is shown.
    pub(crate) fn sentence(self, lock: &str, holder: Option<&LockHolderJson>) -> String {
        match self {
            Self::FinishedCheckout => "Finished the checkout of an interrupted rebase or history edit; HEAD stays on the rewritten commits.".to_owned(),
            Self::RolledBack => "Rolled back an interrupted rebase or history edit; refs, index and files are back to where they were.".to_owned(),
            Self::Pending => match holder {
                Some(h) if h.running => format!(
                    "An interrupted rebase or history edit is pending: process {} holds {lock}. Run grit status again once it exits.",
                    h.pid
                ),
                Some(h) => format!(
                    "An interrupted rebase or history edit is pending: {lock} is held but process {} has exited; run grit status again to recover.",
                    h.pid
                ),
                None => format!(
                    "An interrupted rebase or history edit is pending: {lock} is held and no grit process id was recorded; the lock may be stale (another tool or core.lockfilePid=false). Run grit status again once nothing else is using the repository."
                ),
            },
        }
    }
}

fn conflict_json(c: &Conflict) -> ConflictJson {
    ConflictJson {
        path: c.path.clone(),
        kind: c.kind.as_str().to_owned(),
    }
}

fn operation_json(op: &grit_lib::state::Operation) -> OperationJson {
    OperationJson {
        kind: in_progress_snapshot_kind(op.kind).to_owned(),
        next_steps: op.next_steps.iter().filter_map(NextStep::command).collect(),
        alternatives: op
            .alternatives
            .iter()
            .filter_map(NextStep::command)
            .collect(),
    }
}

/// A shared branch's other heads in `grit status`.
#[derive(Serialize, JsonSchema)]
pub struct StatusSharedJson {
    /// The remote they were fetched from.
    pub remote: String,
    /// Each other member's head, compared with this branch.
    pub heads: Vec<crate::render::sync::SharedHead>,
}

/// Upstream ahead/behind in `--json` / `--markdown` output.
#[derive(Serialize, JsonSchema)]
pub struct StatusUpstreamJson {
    /// Short upstream ref name (`origin/topic`).
    pub r#ref: String,
    /// Commits on this branch not on the upstream, or `null` when counts were skipped.
    pub ahead: Option<usize>,
    /// Commits on the upstream not on this branch, or `null` when counts were skipped.
    pub behind: Option<usize>,
}

/// The other members' heads of `branch`, when it is shared here.
fn shared_status(repo: &grit_lib::repo::Repository, branch: &str) -> Option<StatusSharedJson> {
    let env = crate::context::environment();
    let config = grit_lib::config::ConfigSet::load(
        &env,
        Some(repo.git_dir()),
        &grit_lib::config::LoadConfigOptions::standard(&env, Some(repo.git_dir())),
    )
    .ok()?;
    if !crate::commands::sync::is_shared(&config, branch) {
        return None;
    }
    let ours = repo
        .references()
        .resolve(format!("refs/heads/{branch}"))
        .ok();
    let mut remotes: Vec<String> =
        crate::context::config_string(&config, &format!("branch.{branch}.remote"))
            .into_iter()
            .collect();
    remotes.push(crate::commands::sync::SPACE_REMOTE.to_owned());
    remotes.push("origin".to_owned());
    for remote in remotes {
        let heads = crate::commands::sync::shared_heads(repo, &remote, branch).ok()?;
        if heads.is_empty() {
            continue;
        }
        return Some(StatusSharedJson {
            heads: heads
                .into_iter()
                .map(|(user, tip)| {
                    crate::commands::sync::SharedHead::new(repo, &remote, branch, user, ours, tip)
                })
                .collect(),
            remote,
        });
    }
    Some(StatusSharedJson {
        remote: "origin".to_owned(),
        heads: Vec::new(),
    })
}

fn format_short_lines(
    model: &grit_lib::porcelain::status::StatusModel,
    display: Option<&PathDisplayOptions>,
) -> Vec<String> {
    short_status_lines(model)
        .into_iter()
        .map(|line| {
            line.render_with(|path| {
                display
                    .map(|d| d.format_path_for_human(path))
                    .unwrap_or_else(|| path.to_owned())
            })
        })
        .collect()
}

/// Stable ids for status `--json` (`operation.kind`, `in_progress` entries).
fn in_progress_snapshot_kind(op: InProgressOperation) -> &'static str {
    match op {
        InProgressOperation::Merge => "merge",
        InProgressOperation::RebaseInteractive => "rebase-interactive",
        InProgressOperation::Rebase => "rebase",
        InProgressOperation::CherryPick => "cherry-pick",
        InProgressOperation::Revert => "revert",
        InProgressOperation::Bisect => "bisect",
        InProgressOperation::Am => "am",
        InProgressOperation::RewriteApply => "rewrite-apply",
        _ => "other",
    }
}

fn tracked_to_upstream_json(tracked: &TrackedComparison) -> StatusUpstreamJson {
    StatusUpstreamJson {
        r#ref: tracked.name.clone(),
        ahead: tracked.ahead,
        behind: tracked.behind,
    }
}

/// CLI spelling for [`UntrackedMode`] (`grit status --untracked`).
#[derive(Clone, Copy, Debug)]
pub enum UntrackedListing {
    No,
    Normal,
    All,
}

impl std::str::FromStr for UntrackedListing {
    type Err = String;

    fn from_str(raw: &str) -> Result<Self, Self::Err> {
        Self::parse(raw)
    }
}

impl UntrackedListing {
    /// Parse `no`, `normal`, or `all` (case-insensitive).
    pub fn parse(raw: &str) -> Result<Self, String> {
        match raw.to_ascii_lowercase().as_str() {
            "no" => Ok(Self::No),
            "normal" => Ok(Self::Normal),
            "all" => Ok(Self::All),
            _ => Err(format!(
                "invalid untracked mode {raw:?} (expected no, normal, or all)"
            )),
        }
    }

    fn into_lib(self) -> UntrackedMode {
        match self {
            Self::No => UntrackedMode::No,
            Self::Normal => UntrackedMode::Normal,
            Self::All => UntrackedMode::All,
        }
    }
}

/// CLI arguments for `grit status` (also the default when no subcommand is given).
#[derive(Debug, Clone, Default, Args)]
pub struct StatusCliArgs {
    /// How untracked paths are listed: `normal` (default) collapses wholly untracked
    /// directories; `all` lists every file; `no` omits untracked paths. When omitted,
    /// uses `status.showUntrackedFiles` from config.
    #[arg(long = "untracked", value_name = "MODE")]
    pub untracked: Option<UntrackedListing>,
    /// Skip ahead/behind rev-list walks (also when `status.aheadBehind=false`).
    #[arg(long = "no-ahead-behind")]
    pub no_ahead_behind: bool,
    /// List ignored paths (traditional mode; same set as `git status --ignored`).
    #[arg(long)]
    pub ignored: bool,
    /// One line per path: two-letter index/worktree code plus path (`??` / `!!` for untracked / ignored).
    #[arg(short = 's', long)]
    pub short: bool,
    #[command(flatten)]
    pub pathspecs: PathspecArgs,
}

pub fn run(args: &StatusCliArgs) -> Result<StatusOutcome> {
    let (repo, sink) = context::discover_with_warnings()?;
    let config = repo.config().snapshot().context("could not load config")?;
    let mut status_opts =
        StatusOptions::from_config(&repo.config()).context("could not load config")?;
    if let Some(mode) = args.untracked {
        status_opts.untracked = mode.into_lib();
    }
    if !args.pathspecs.paths.is_empty() || args.pathspecs.pathspec_from_file.is_some() {
        status_opts.pathspec = args.pathspecs.resolve_pathspec(&repo)?;
    }
    if args.ignored {
        status_opts.ignored = IgnoredMode::Traditional;
    }
    status_opts.count_stash = true;
    let show_counts = !args.no_ahead_behind && status_ahead_behind_enabled(&config);
    let model = repo
        .status()
        .run(&status_opts, &mut NullProgress, &CancelToken::never())
        .context("could not compute status")?;

    let summary = repo
        .status()
        .assemble_summary(
            model,
            StatusSummaryOptions::default().standing(StandingOptions {
                newest_ahead_limit: SHORTLOG_LIMIT,
                counts: show_counts,
            }),
        )
        .context("could not assemble status summary")?;
    let model = summary.model;
    let operations = summary.in_progress;
    let merging = model.state.merge_in_progress;
    let conflicts: Vec<ConflictJson> = model.conflict_details.iter().map(conflict_json).collect();
    let conflict_groups = summary.conflict_groups;
    let operation = model.operation.as_ref().map(operation_json);
    let standing = summary.standing;
    let header = summary.header;
    let branch = match &header {
        StatusHeader::RebaseInProgress { branch, .. } => branch.clone(),
        _ => standing.branch.clone(),
    };
    let now = context::wall_clock_now(repo.environment()).unix_timestamp();
    let workspace = GitButlerWorkspace::detect(&repo).context("could not read HEAD")?;
    let workspace_standing = match (&workspace, &standing.target) {
        (Some(workspace), Some(target)) => Some(
            workspace
                .standing(&repo, target)
                .context("could not compare a workspace branch with the target")?,
        ),
        _ => None,
    };
    let (ahead, newest_ahead) = match (&workspace_standing, &standing.target) {
        (Some(ws), _) => (ws.ahead, ws.newest_ahead.as_slice()),
        (None, Some(target)) => (target.ahead, target.newest_ahead.as_slice()),
        (None, None) => (None, &[][..]),
    };
    let commit_rows = context::commit_summaries(&repo, newest_ahead)?;
    let workspace_rows = match &workspace {
        Some(workspace) => context::commit_summaries(&repo, &workspace.head_commits())?,
        None => Vec::new(),
    };
    let gitbutler_workspace = workspace
        .as_ref()
        .map(|workspace| workspace_json(workspace, &workspace_rows, workspace_standing.as_ref()));
    let ahead_oids: Vec<_> = commit_rows.iter().map(|c| c.oid).collect();
    let commits = if ahead_oids.is_empty() {
        Vec::new()
    } else {
        crate::commits_json::CommitJsonLoader::for_listed_commits().load(&repo, &ahead_oids, now)?
    };

    let work_tree = repo.work_tree();
    let staged: Vec<ChangeJson> = model
        .staged
        .iter()
        .map(|e| change_json_with_worktree(e, work_tree))
        .collect();
    let unstaged: Vec<ChangeJson> = model
        .unstaged
        .iter()
        .map(|e| change_json_with_worktree(e, work_tree))
        .collect();
    let clean = model.is_clean();
    let worktree_case_collisions = repo
        .status()
        .worktree_case_collisions(&model.index)
        .context("could not detect path collisions")?;
    let collision_json: Vec<WorktreeCaseCollisionJson> = worktree_case_collisions
        .iter()
        .map(|g| WorktreeCaseCollisionJson {
            paths: g.paths.clone(),
        })
        .collect();
    let blocked = worktree_case_collision_paths(&worktree_case_collisions);
    let unstaged_paths: Vec<String> = model
        .unstaged
        .iter()
        .filter_map(|e| e.new_path.clone().or_else(|| e.old_path.clone()))
        .collect();
    let guidance = filter_guidance_for_case_collisions(summary.guidance, &unstaged_paths, &blocked);
    let worktree_case_collision_paths: Vec<String> = blocked.into_iter().collect();

    let path_display = PathDisplayOptions::from_repo(&repo).ok();
    let short_lines = if args.short {
        format_short_lines(&model, path_display.as_ref())
    } else {
        Vec::new()
    };
    let ignored_json = args.ignored.then(|| model.ignored.clone());

    let standing_for_outcome = standing.clone();
    let integration_hint = summary.integration_hint;
    let interrupted_rewrite_lock_holder = model
        .interrupted_rewrite
        .as_ref()
        .filter(|outcome| matches!(outcome, InterruptedRewrite::Pending))
        .and_then(|_| {
            let lock_path = repo.git_dir().join("index.lock");
            grit_lib::lockfile::LockHolder::read(&lock_path).map(|h| LockHolderJson {
                pid: h.pid,
                running: h.running,
            })
        });
    let head_short = model
        .head
        .oid()
        .map(|oid| repo.revwalk().abbreviate(*oid, AbbrevLength::Configured))
        .transpose()?;
    let shared = branch
        .as_deref()
        .and_then(|branch| shared_status(&repo, branch));
    Ok(StatusOutcome {
        shared,
        warnings: sink.warnings(),
        worktree_case_collisions: collision_json,
        branch,
        detached: matches!(model.head, HeadState::Detached { .. }),
        head: model.head.oid().map(ObjectId::to_hex),
        head_short,
        target: standing.target.as_ref().map(|t| t.name.clone()),
        ahead,
        behind: standing.target.as_ref().and_then(|t| t.behind),
        upstream: standing.upstream.as_ref().map(tracked_to_upstream_json),
        commits,
        staged,
        unstaged,
        untracked: model.untracked,
        embedded_repositories: model.embedded_repositories,
        clean,
        merging,
        in_progress: operations
            .into_iter()
            .map(|op| in_progress_snapshot_kind(op).to_owned())
            .collect(),
        conflicts,
        operation,
        ignored: ignored_json,
        stash_count: model.stash_count,
        interrupted_rewrite: model.interrupted_rewrite.map(InterruptedRewriteJson::from),
        gitbutler_workspace,
        header,
        commit_rows,
        staged_entries: model.staged,
        unstaged_entries: model.unstaged,
        path_display,
        show_counts,
        standing: standing_for_outcome,
        conflict_groups,
        guidance,
        integration_hint,
        short_format: args.short,
        short_lines,
        worktree_case_collision_paths,
        interrupted_rewrite_lock_holder,
        workspace_rows,
    })
}

/// `grit status --json`'s view of `workspace`; `rows` are its heads' summaries, in order.
fn workspace_json(
    workspace: &GitButlerWorkspace,
    rows: &[CommitSummary],
    standing: Option<&WorkspaceStanding>,
) -> GitButlerWorkspaceJson {
    let heads = workspace
        .heads
        .iter()
        .zip(rows)
        .enumerate()
        .map(|(i, (head, row))| WorkspaceHeadJson {
            commit: head.commit.to_hex(),
            branches: head.branches.clone(),
            subject: row.subject.clone(),
            ahead: standing.and_then(|s| s.heads_ahead.get(i).copied().flatten()),
        })
        .collect();
    GitButlerWorkspaceJson {
        commit: workspace.commit.to_hex(),
        heads,
    }
}

/// Dispatch parsed CLI args for `Status`.
pub fn dispatch(
    args: crate::args::StatusArgs,
    env: crate::dispatch::CommandEnv<'_>,
) -> anyhow::Result<crate::exit_status::ExitStatus> {
    use crate::commands;

    use crate::dispatch::{emit_success, CommandEnv};

    let CommandEnv {
        opts,
        ctx,
        process_env: _,
    } = env;
    let status = args;
    emit_success(commands::status::run(&status)?, opts, ctx)
}

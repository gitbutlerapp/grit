//! `grit history` — stateless history rewrite helpers.

use std::collections::HashMap;
use std::io::{self, Read, Write};

use anyhow::{bail, Context, Result};
use clap::ValueEnum;
use grit_lib::config::ident_resolve::IdentRole;
use grit_lib::config::ConfigSet;
use grit_lib::history::{
    apply_rewrite, reword, ApplyOptions, EmptyPolicy, HistoryError, RefScope, RewriteOptions,
    RewritePlan,
};
use grit_lib::objects::ObjectId;
use grit_lib::revwalk::rev_parse::resolve_revision_as_commit;
use grit_lib::state::{resolve_head, HeadState};
use schemars::JsonSchema;
use serde::Serialize;

use crate::context;
use crate::json_error;
use crate::markdown;
use crate::output::{outln, HumanRender, MarkdownRender, RenderContext};

/// Which local refs a history rewrite may move.
#[derive(Debug, Clone, Copy, ValueEnum, Default, PartialEq, Eq)]
pub enum UpdateRefs {
    /// Every local branch descending from the edited commit, plus `HEAD` when applicable.
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

/// Shared flags for history rewrite subcommands.
#[derive(Debug, clap::Args)]
pub struct HistoryRewriteArgs {
    /// Print planned ref updates without moving refs (new commit objects are still written).
    #[arg(long = "dry-run")]
    pub dry_run: bool,
    /// Which refs to rewrite: all descendant branches (default) or only `HEAD`.
    #[arg(long = "update-refs", value_enum, default_value_t = UpdateRefs::Branches)]
    pub update_refs: UpdateRefs,
}

/// Result of `grit history reword`.
#[derive(Debug, Serialize, JsonSchema)]
pub struct HistoryRewordOutcome {
    /// Operation name (`reword`).
    pub operation: String,
    /// Original and replacement commit OIDs for every rewritten commit.
    pub rewritten: Vec<RewrittenPair>,
    /// Planned or applied ref tip updates.
    pub refs: Vec<RefPair>,
    /// Previous and new `HEAD` commit OIDs when `HEAD`'s commit changes; otherwise omitted.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub head: Option<HeadPair>,
    /// Whether refs were left unchanged (`true` for `--dry-run`).
    pub dry_run: bool,
}

/// One commit OID before and after rewrite.
#[derive(Debug, Serialize, JsonSchema)]
pub struct RewrittenPair {
    /// Commit OID before rewrite (full hex).
    pub old: String,
    /// Replacement commit OID (full hex).
    pub new: String,
}

/// One ref tip before and after rewrite.
#[derive(Debug, Serialize, JsonSchema)]
pub struct RefPair {
    /// Full ref name.
    pub name: String,
    /// Previous tip OID (full hex).
    pub old: String,
    /// New tip OID (full hex).
    pub new: String,
}

/// `HEAD` commit before and after rewrite.
#[derive(Debug, Serialize, JsonSchema)]
pub struct HeadPair {
    /// Commit OID at `HEAD` before rewrite (full hex).
    pub old: String,
    /// Commit OID at `HEAD` after rewrite (full hex).
    pub new: String,
}

impl HumanRender for HistoryRewordOutcome {
    fn render_human(&self, w: &mut dyn Write, _ctx: &RenderContext) -> Result<()> {
        if self.dry_run {
            outln!(w, "Dry run — refs would change:");
        } else {
            outln!(w, "Rewrote {} commit(s)", self.rewritten.len());
        }
        for pair in &self.rewritten {
            let old_short = pair.old.get(..7).unwrap_or(&pair.old);
            let new_short = pair.new.get(..7).unwrap_or(&pair.new);
            outln!(w, "  commit {old_short} → {new_short}");
        }
        for update in &self.refs {
            let old_short = update.old.get(..7).unwrap_or(&update.old);
            let new_short = update.new.get(..7).unwrap_or(&update.new);
            outln!(w, "  {} {old_short} → {new_short}", update.name);
        }
        Ok(())
    }
}

impl MarkdownRender for HistoryRewordOutcome {
    fn render_markdown(&self, w: &mut dyn Write, _ctx: &RenderContext) -> Result<()> {
        markdown::print_history_reword_outcome(w, self)?;
        Ok(())
    }
}

/// Rewrite `commit`'s message to `message` and replay descendants.
pub fn run_reword(
    commit: &str,
    message: Option<String>,
    message_file: Option<String>,
    shared: &HistoryRewriteArgs,
) -> Result<HistoryRewordOutcome> {
    let message = read_message(message, message_file)?;
    let repo = context::discover()?;
    let commit_oid = resolve_revision_as_commit(&repo, commit)
        .with_context(|| format!("could not resolve commit '{commit}'"))?;
    let env = repo.environment();
    let config =
        ConfigSet::load(env, Some(repo.git_dir()), true).context("could not load config")?;
    let committer = context::identity(
        env,
        &config,
        IdentRole::Committer,
        "GIT_COMMITTER_DATE",
        context::wall_clock_now(env),
    )?;
    let plan = reword(
        &repo,
        commit_oid,
        message,
        &RewriteOptions {
            scope: shared.update_refs.into(),
            empty: EmptyPolicy::Drop,
            committer,
        },
    )
    .map_err(map_history_error)?;
    let subject = rewritten_subject(&repo, &plan, commit_oid)?;
    let mut outcome = outcome_from_plan(&repo, &plan, shared.dry_run)?;
    if !shared.dry_run {
        let identity = context::committer_reflog_identity(env, &config)?;
        apply_rewrite(
            &repo,
            &plan,
            &ApplyOptions {
                identity,
                reflog_message: format!("history reword: {subject}"),
            },
        )
        .map_err(map_history_error)?;
    }
    outcome.dry_run = shared.dry_run;
    Ok(outcome)
}

fn read_message(message: Option<String>, message_file: Option<String>) -> Result<String> {
    match (message, message_file) {
        (Some(_), Some(_)) => bail!("use either -m/--message or --message-file, not both"),
        (None, None) => bail!("a new message is required (-m/--message or --message-file)"),
        (Some(m), None) => Ok(m),
        (None, Some(path)) => {
            if path == "-" {
                let mut buf = String::new();
                io::stdin()
                    .read_to_string(&mut buf)
                    .context("could not read message from stdin")?;
                Ok(buf)
            } else {
                std::fs::read_to_string(&path)
                    .with_context(|| format!("could not read message file '{path}'"))
            }
        }
    }
}

fn rewritten_subject(
    repo: &grit_lib::repo::Repository,
    plan: &RewritePlan,
    target: ObjectId,
) -> Result<String> {
    let new_oid = plan
        .commits
        .iter()
        .find(|r| r.old == target)
        .and_then(|r| r.new)
        .ok_or_else(|| anyhow::anyhow!("rewritten commit missing from plan"))?;
    let commit = context::read_commit(repo, &new_oid)?;
    Ok(context::subject_line(&commit.message))
}

fn outcome_from_plan(
    repo: &grit_lib::repo::Repository,
    plan: &RewritePlan,
    dry_run: bool,
) -> Result<HistoryRewordOutcome> {
    let rewritten = plan
        .commits
        .iter()
        .filter_map(|row| {
            row.new.map(|new| RewrittenPair {
                old: row.old.to_hex(),
                new: new.to_hex(),
            })
        })
        .collect();
    let refs = plan
        .ref_updates
        .iter()
        .map(|u| RefPair {
            name: u.name.clone(),
            old: u.old.to_hex(),
            new: u.new.to_hex(),
        })
        .collect();
    let head = head_pair(repo, plan)?;
    Ok(HistoryRewordOutcome {
        operation: "reword".into(),
        rewritten,
        refs,
        head,
        dry_run,
    })
}

fn head_pair(repo: &grit_lib::repo::Repository, plan: &RewritePlan) -> Result<Option<HeadPair>> {
    if !plan.head.commit_changes {
        return Ok(None);
    }
    let old_oid = head_commit_oid(repo)?;
    let mapping: HashMap<ObjectId, ObjectId> = plan
        .commits
        .iter()
        .filter_map(|r| r.new.map(|n| (r.old, n)))
        .collect();
    let new_oid = mapping.get(&old_oid).copied().unwrap_or(old_oid);
    Ok(Some(HeadPair {
        old: old_oid.to_hex(),
        new: new_oid.to_hex(),
    }))
}

fn head_commit_oid(repo: &grit_lib::repo::Repository) -> Result<ObjectId> {
    match resolve_head(repo.git_dir())? {
        HeadState::Branch { oid: Some(oid), .. } | HeadState::Detached { oid } => Ok(oid),
        HeadState::Branch { oid: None, .. } | HeadState::Invalid => {
            bail!("HEAD does not point at a commit")
        }
    }
}

fn map_history_error(err: HistoryError) -> anyhow::Error {
    match err {
        HistoryError::EmptyMessage => anyhow::anyhow!("commit message cannot be empty"),
        HistoryError::Conflict { mut paths, .. } => {
            paths.sort();
            paths.dedup();
            json_error::operation_conflict("history reword", paths)
        }
        HistoryError::MergeCommitInReplay { commit } => {
            let short = &commit.to_hex()[..7];
            anyhow::anyhow!(
                "{short} is a merge commit — grit history reword cannot replay merges yet"
            )
        }
        HistoryError::RootCommit { commit } => {
            let short = &commit.to_hex()[..7];
            anyhow::anyhow!(
                "{short} is a root commit and cannot be dropped or replayed without a parent"
            )
        }
        HistoryError::BecameEmpty { commit } => {
            let short = &commit.to_hex()[..7];
            anyhow::anyhow!("replay of {short} would become empty — refusing rewrite")
        }
        HistoryError::NotInHistory { commit } => {
            let short = &commit.to_hex()[..7];
            anyhow::anyhow!("{short} is not reachable from HEAD for this rewrite scope")
        }
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
        HistoryError::RefApply { message } => {
            anyhow::anyhow!("ref update failed: {message}")
        }
        HistoryError::WorktreeApply { message } => {
            anyhow::anyhow!("worktree update failed: {message}")
        }
        _ => anyhow::anyhow!("{err}"),
    }
}

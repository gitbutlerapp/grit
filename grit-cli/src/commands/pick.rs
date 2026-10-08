//! `grit pick` — cherry-pick a single commit onto the current branch.
//!
//! Replays the change introduced by `<commit>` (its diff against its first
//! parent) on top of the current branch using a three-way merge, then records
//! a new commit that preserves the original author and commit message.
//!
//! `grit pick` is deliberately minimal: one commit at a time, no `--continue` /
//! `--abort` machinery, no merge-commit picking. Conflicts and other tricky
//! situations are reported up front, and `git cherry-pick` is the escape hatch.

use crate::context;
use crate::output::HumanRender;
use anyhow::{bail, Context, Result};
use grit_lib::config::ConfigSet;
use grit_lib::ident_resolve::IdentRole;
use grit_lib::merge_file::MergeFavor;
use grit_lib::merge_trees::{
    merge_trees_three_way, TreeMergeConflictPresentation, WhitespaceMergeOptions,
};
use grit_lib::objects::{serialize_commit, CommitData, ObjectId, ObjectKind};
use grit_lib::porcelain::checkout::checkout_between_trees;
use grit_lib::porcelain::worktree_guard::{ensure_worktree_clean_for_pick, prepare_tree_checkout};
use grit_lib::refs;
use grit_lib::repo::Repository;
use grit_lib::rev_parse::resolve_revision;
use grit_lib::state::{resolve_head, HeadState};
use grit_lib::write_tree::{write_tree_update_index, WriteTreeFlags};
use serde::Serialize;

/// Result of `grit pick`.
#[derive(Serialize)]
pub struct PickOutcome {
    /// The original commit that was picked (full hex oid).
    pub source: String,
    /// The new commit created on the current branch (full hex oid).
    pub oid: String,
    /// Subject line of the picked commit.
    pub subject: String,
}

impl HumanRender for PickOutcome {
    fn render_human(&self) {
        let new_short = self.oid.get(..7).unwrap_or(&self.oid);
        let src_short = self.source.get(..7).unwrap_or(&self.source);
        println!("Picked {src_short} → {new_short} {}", self.subject);
    }
}

/// Cherry-pick `commit` onto the current branch.
///
/// `commit` may be any revision spec resolvable by [`resolve_revision`]
/// (full / short oid, branch name, `HEAD~2`, etc.).
pub fn run(commit: &str) -> Result<PickOutcome> {
    let repo = context::discover()?;

    ensure_worktree_clean_for_pick(&repo)
        .map_err(anyhow::Error::new)
        .context("could not verify worktree is clean")?;

    let (refname, head_oid) = match resolve_head(&repo.git_dir)? {
        HeadState::Branch {
            refname,
            oid: Some(oid),
            ..
        } => (refname, oid),
        HeadState::Branch { .. } => bail!("no commits yet on this branch to pick onto"),
        HeadState::Detached { .. } => bail!("HEAD is detached; grit pick needs a branch"),
        HeadState::Invalid => bail!("HEAD is in an unknown state"),
    };

    let source_oid = resolve_revision(&repo, commit)
        .with_context(|| format!("could not resolve commit '{commit}'"))?;
    let source = context::read_commit(&repo, &source_oid)?;
    if source.parents.len() > 1 {
        bail!(
            "{} is a merge commit — grit pick only handles regular commits; use `git cherry-pick -m 1 {commit}`",
            &source_oid.to_hex()[..7]
        );
    }
    if source_oid == head_oid {
        bail!("nothing to pick — that commit is already the current HEAD");
    }

    let head_tree = context::commit_tree(&repo, &head_oid)?;
    let base_tree = if let Some(parent) = source.parents.first() {
        context::commit_tree(&repo, parent)?
    } else {
        // Root commit: base is the empty tree.
        repo.odb
            .write(ObjectKind::Tree, &[])
            .context("could not write empty tree")?
    };
    let source_tree = source.tree;

    if base_tree == source_tree {
        bail!(
            "{} is empty (its tree matches its parent) — nothing to pick",
            &source_oid.to_hex()[..7]
        );
    }

    let merged = merge_trees_three_way(
        &repo,
        base_tree,
        head_tree,
        source_tree,
        MergeFavor::default(),
        WhitespaceMergeOptions::default(),
        None,
        TreeMergeConflictPresentation::default(),
    )
    .context("could not replay the commit")?;

    if !merged.conflict_content.is_empty() || merged.index.entries().iter().any(|e| e.stage() != 0)
    {
        let mut paths: Vec<String> = merged
            .conflict_content
            .keys()
            .map(|k| String::from_utf8_lossy(k).into_owned())
            .collect();
        if paths.is_empty() {
            paths = merged
                .index
                .entries()
                .iter()
                .filter(|e| e.stage() != 0)
                .map(|e| String::from_utf8_lossy(&e.path).into_owned())
                .collect();
        }
        paths.sort();
        paths.dedup();
        bail!(
            "pick has conflicts in:\n  {}\n\nNothing was changed. grit can't resolve conflicts yet — run `git cherry-pick {}` to resolve them.",
            paths.join("\n  "),
            &source_oid.to_hex()[..7]
        );
    }

    let mut index = merged.index;
    let new_tree = write_tree_update_index(&repo.odb, &mut index, "", WriteTreeFlags::silent())
        .context("could not write picked tree")?;
    if new_tree == head_tree {
        bail!(
            "{} is already applied on this branch — nothing to pick",
            &source_oid.to_hex()[..7]
        );
    }

    prepare_tree_checkout(&repo, Some(&head_tree), &new_tree)
        .map_err(anyhow::Error::new)
        .context("could not verify working tree")?;
    checkout_between_trees(&repo, Some(&head_tree), &new_tree)
        .context("could not update the working tree")?;

    let env = repo.environment();
    let config =
        ConfigSet::load(env, Some(&repo.git_dir), true).context("could not load config")?;
    let now = context::wall_clock_now(env);
    // Preserve the original author (cherry-pick semantics); committer is the
    // current user. `author_raw` is empty so `serialize_commit` re-encodes from
    // the textual `author` field — matching `grit commit`.
    let author = source.author.clone();
    let committer = context::identity(
        env,
        &config,
        IdentRole::Committer,
        "GIT_COMMITTER_DATE",
        now,
    )?;

    let commit_data = CommitData {
        tree: new_tree,
        parents: vec![head_oid],
        author,
        committer,
        author_raw: Vec::new(),
        committer_raw: Vec::new(),
        encoding: None,
        message: source.message.clone(),
        raw_message: None,
    };
    let new_oid = repo
        .odb
        .write(ObjectKind::Commit, &serialize_commit(&commit_data))
        .context("could not store picked commit")?;

    move_branch(
        &repo,
        &refname,
        head_oid,
        new_oid,
        &format!("cherry-pick: {}", context::subject_line(&source.message)),
    )?;

    Ok(PickOutcome {
        source: source_oid.to_hex(),
        oid: new_oid.to_hex(),
        subject: context::subject_line(&source.message),
    })
}

/// Point a branch (and HEAD's reflog) at `new`, logging the transition.
///
/// Mirrors `grit merge`'s `move_branch`: both refs are updated and reflog entries
/// are written best-effort (failure to log doesn't fail the pick itself).
fn move_branch(
    repo: &Repository,
    refname: &str,
    old: ObjectId,
    new: ObjectId,
    reason: &str,
) -> Result<()> {
    refs::write_ref(&repo.git_dir, refname, &new).context("could not update branch")?;
    let env = repo.environment();
    let config = ConfigSet::load(env, Some(&repo.git_dir), true).unwrap_or_default();
    let who = context::reflog_identity(env, &config, context::wall_clock_now(env));
    let _ = refs::append_reflog(&repo.git_dir, refname, &old, &new, &who, reason, false);
    let _ = refs::append_reflog(&repo.git_dir, "HEAD", &old, &new, &who, reason, false);
    Ok(())
}

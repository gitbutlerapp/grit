//! `grit revert` — revert existing commits.
//!
//! Creates new commits that undo the changes introduced by the given commits.
//! Revert is essentially a reverse cherry-pick: it applies the inverse of a
//! commit's diff onto the current HEAD.
//!
//! For a commit C with parent P:
//!   - base  = C.tree   (the commit being reverted)
//!   - ours  = HEAD.tree (current state)
//!   - theirs = P.tree   (the state before the commit)
//!
//! This three-way merge produces the revert of C's changes.

use anyhow::{bail, Context, Result};
use clap::Args as ClapArgs;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::fs;
use std::io::{stdin, IsTerminal, Write};
use std::path::Path;
use std::process::Command;
use tempfile::NamedTempFile;

use grit_lib::config::ConfigSet;
use grit_lib::index::{Index, IndexEntry, MODE_EXECUTABLE, MODE_GITLINK, MODE_SYMLINK};
use grit_lib::merge_file::{ConflictStyle, MergeFavor};
use grit_lib::merge_trees::{
    index_tree_oid_matches_head, merge_trees_three_way, TheirsConflictLabel,
    TreeMergeConflictPresentation, WhitespaceMergeOptions,
};
use grit_lib::objects::{
    parse_commit, parse_tree, serialize_commit, CommitData, ObjectId, ObjectKind,
};
use grit_lib::porcelain::revert::{merge_commit_message_for_revert, revision_set_newest_first};
use grit_lib::refs::append_reflog;
use grit_lib::repo::Repository;
use grit_lib::rev_parse::resolve_revision;
use grit_lib::state::{resolve_head, HeadState};
use grit_lib::write_tree::write_tree_from_index;

use super::checkout::checkout_index_to_worktree_allowing_submodule_replacement;
use super::cherry_pick::{
    bail_if_df_merge_would_remove_cwd, preflight_cherry_pick_cwd_obstruction,
};
use super::merge::{
    cleanup_message, merge_touched_paths, refresh_index_stat_cache_from_worktree,
    staged_dirty_paths_vs_head,
};
use super::sequencer::{
    append_merge_msg_conflict_footer, rollback_is_safe, sequencer_is_pick_sequence,
    sequencer_is_revert_sequence, strip_first_sequencer_todo_line, unmerged_paths,
    write_abort_safety_file,
};

/// Arguments for `grit revert`.
#[derive(Debug, ClapArgs)]
#[command(about = "Revert some existing commits")]
pub struct Args {
    /// Commits to revert.
    #[arg(value_name = "COMMIT")]
    pub commits: Vec<String>,

    /// Apply revert to index and working tree without committing.
    #[arg(short = 'n', long = "no-commit")]
    pub no_commit: bool,

    /// Add Signed-off-by trailer to the message.
    #[arg(short = 's', long = "signoff")]
    pub signoff: bool,

    /// For reverting merge commits, specify which parent (1-based) is mainline.
    #[arg(short = 'm', long = "mainline")]
    pub mainline: Option<usize>,

    /// Continue a revert after resolving conflicts.
    #[arg(long = "continue")]
    pub r#continue: bool,

    /// Abort an in-progress revert.
    #[arg(long = "abort")]
    pub abort: bool,

    /// Skip the current commit and continue.
    #[arg(long = "skip")]
    pub skip: bool,

    /// Quit the revert sequence, keeping current changes.
    #[arg(long = "quit")]
    pub quit: bool,

    /// Merge strategy to use (e.g. recursive, ort).
    #[arg(long = "strategy")]
    pub strategy: Option<String>,

    /// Strategy option (e.g. "theirs", "ours", "patience").
    #[arg(short = 'X', long = "strategy-option")]
    pub strategy_option: Vec<String>,

    /// Use the given edit message without opening an editor.
    #[arg(long = "no-edit", conflicts_with = "edit")]
    pub no_edit: bool,

    /// Open an editor for the commit message.
    #[arg(short = 'e', long = "edit")]
    pub edit: bool,

    /// Refer to the reverted commit using the `reference` pretty format (like `git show --pretty=reference`).
    #[arg(long = "reference", conflicts_with = "no_reference")]
    pub reference: bool,

    /// Disable reference-format commit lines even if `revert.reference` is set.
    #[arg(long = "no-reference", conflicts_with = "reference")]
    pub no_reference: bool,

    /// Message cleanup mode for conflict `MERGE_MSG` (matches `git revert --cleanup`).
    #[arg(long = "cleanup", value_name = "MODE", hide = true)]
    pub cleanup: Option<String>,
}

/// Run the `revert` command.
pub fn run(args: Args) -> Result<()> {
    if args.abort {
        return super::cherry_pick::abort_cherry_pick_or_revert();
    }
    if args.skip {
        return do_skip(args);
    }
    if args.quit {
        return do_quit();
    }
    if args.r#continue {
        return do_continue();
    }
    if args.commits.is_empty() {
        bail!("nothing to revert; specify at least one commit");
    }
    do_revert(args)
}

// ── Main revert flow ────────────────────────────────────────────────

fn merge_revert_reference_config(git_dir: &Path, args: &mut Args) -> Result<()> {
    if args.reference || args.no_reference {
        return Ok(());
    }
    let config = ConfigSet::load(Some(git_dir), true)?;
    if config.get_bool("revert.reference") == Some(Ok(true)) {
        args.reference = true;
    }
    Ok(())
}

fn do_revert(mut args: Args) -> Result<()> {
    let repo = Repository::discover(None).context("not a git repository")?;
    let git_dir = &repo.git_dir;

    let expanded = expand_revert_specs(&repo, &args.commits)?;

    let seq_todo = git_dir.join("sequencer").join("todo");
    if seq_todo.exists() {
        if expanded.len() > 1 {
            if sequencer_is_pick_sequence(git_dir) {
                eprintln!("error: a cherry-pick is already in progress");
                eprintln!("hint: try \"git cherry-pick (--continue | --abort | --quit)\"");
                eprintln!("fatal: revert failed");
                std::process::exit(128);
            }
            if sequencer_is_revert_sequence(git_dir) {
                eprintln!("error: a revert is already in progress");
                eprintln!("hint: try \"git revert (--continue | --skip | --abort | --quit)\"");
                eprintln!("fatal: revert failed");
                std::process::exit(128);
            }
            bail!(
                "error: a revert is already in progress\n\
                 hint: use \"grit revert --continue\" to continue\n\
                 hint: or \"grit revert --abort\" to abort"
            );
        } else if sequencer_is_pick_sequence(git_dir) {
            eprintln!("error: a cherry-pick is already in progress");
            eprintln!("hint: try \"git cherry-pick (--continue | --abort | --quit)\"");
            eprintln!("fatal: revert failed");
            std::process::exit(128);
        }
    }

    if git_dir.join("CHERRY_PICK_HEAD").exists() {
        eprintln!("error: a cherry-pick is already in progress");
        eprintln!("hint: try \"git cherry-pick (--continue | --abort | --quit)\"");
        eprintln!("fatal: revert failed");
        std::process::exit(128);
    }

    if git_dir.join("REVERT_HEAD").exists() {
        bail!(
            "error: a revert is already in progress\n\
             hint: use \"grit revert --continue\" to continue\n\
             hint: or \"grit revert --abort\" to abort"
        );
    }

    let head = resolve_head(git_dir)?;
    let orig_head_oid = head
        .oid()
        .ok_or_else(|| anyhow::anyhow!("cannot revert: HEAD does not point to a commit"))?;

    if let Some(head_oid) = head.oid() {
        let _ = fs::write(
            git_dir.join("ORIG_HEAD"),
            format!("{}\n", head_oid.to_hex()),
        );
    }

    if expanded.len() > 1 && !args.no_commit {
        let seq_dir = git_dir.join("sequencer");
        fs::create_dir_all(&seq_dir)?;
        fs::write(
            seq_dir.join("head"),
            format!("{}\n", orig_head_oid.to_hex()),
        )?;
        let mut todo_lines = String::new();
        for spec in &expanded {
            let oid =
                resolve_revision(&repo, spec).with_context(|| format!("bad revision '{spec}'"))?;
            let obj = repo.odb.read(&oid)?;
            let commit = parse_commit(&obj.data)?;
            let subject = commit.message.lines().next().unwrap_or("");
            todo_lines.push_str(&format!("revert {} {}\n", &oid.to_hex()[..7], subject));
        }
        fs::write(seq_dir.join("todo"), &todo_lines)?;
        write_revert_sequencer_opts(git_dir, &args)?;
        write_abort_safety_file(git_dir)?;
    }

    merge_revert_reference_config(git_dir, &mut args)?;
    run_revert_sequence(&repo, &expanded, &args, None)
}

fn write_revert_sequencer_opts(git_dir: &Path, args: &Args) -> Result<()> {
    let seq_dir = git_dir.join("sequencer");
    fs::create_dir_all(&seq_dir)?;
    let mut opts = String::from("[options]\n");
    if args.signoff {
        opts.push_str("\tsignoff = true\n");
    }
    if let Some(m) = args.mainline {
        opts.push_str(&format!("\tmainline = {m}\n"));
    }
    if let Some(ref strat) = args.strategy {
        opts.push_str(&format!("\tstrategy = {strat}\n"));
    }
    for xopt in &args.strategy_option {
        opts.push_str(&format!("\tstrategy-option = {xopt}\n"));
    }
    if args.edit {
        opts.push_str("\tedit = true\n");
    }
    if args.no_edit {
        opts.push_str("\tedit = false\n");
    }
    if args.reference {
        opts.push_str("\treference = true\n");
    }
    if args.no_reference {
        opts.push_str("\treference = false\n");
    }
    if let Some(ref c) = args.cleanup {
        opts.push_str(&format!("\tcleanup = {c}\n"));
    }
    fs::write(seq_dir.join("opts"), &opts)?;
    Ok(())
}

fn merge_revert_sequencer_opts(git_dir: &Path, args: &mut Args) {
    let opts_path = git_dir.join("sequencer").join("opts");
    let content = match fs::read_to_string(&opts_path) {
        Ok(c) => c,
        Err(_) => return,
    };
    for raw_line in content.lines() {
        let line = raw_line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if line.starts_with('[') && line.ends_with(']') {
            continue;
        }
        if let Some((k, v)) = line.split_once('=') {
            let key = k.trim();
            let val = v.trim();
            match key {
                "signoff" if val == "true" => args.signoff = true,
                "mainline" => {
                    if let Ok(m) = val.parse::<usize>() {
                        args.mainline = Some(m);
                    }
                }
                "strategy" => args.strategy = Some(val.to_string()),
                "strategy-option" => args.strategy_option.push(val.to_string()),
                "edit" if val == "true" => {
                    args.edit = true;
                    args.no_edit = false;
                }
                "edit" if val == "false" => {
                    args.no_edit = true;
                    args.edit = false;
                }
                "reference" if val == "true" => {
                    args.reference = true;
                    args.no_reference = false;
                }
                "reference" if val == "false" => {
                    args.no_reference = true;
                    args.reference = false;
                }
                "cleanup" => args.cleanup = Some(val.to_string()),
                _ => {}
            }
        }
    }
}

fn parse_revert_todo_line(repo: &Repository, line: &str) -> Option<ObjectId> {
    let t = line.trim();
    if t.is_empty() || t.starts_with('#') {
        return None;
    }
    let after_cmd = t.strip_prefix("revert")?;
    if after_cmd.is_empty() || !after_cmd.starts_with(|c: char| c.is_whitespace()) {
        return None;
    }
    let after = after_cmd.trim_start();
    let token = after.split_whitespace().next()?;
    resolve_revision(repo, token).ok()
}

fn load_revert_sequencer_todo(repo: &Repository, git_dir: &Path) -> Vec<ObjectId> {
    let path = git_dir.join("sequencer").join("todo");
    let Ok(content) = fs::read_to_string(path) else {
        return Vec::new();
    };
    let mut oids = Vec::new();
    for line in content.lines() {
        if let Some(oid) = parse_revert_todo_line(repo, line) {
            oids.push(oid);
        }
    }
    oids
}

fn validate_revert_sequencer_todo(repo: &Repository, git_dir: &Path) -> Result<()> {
    let path = git_dir.join("sequencer").join("todo");
    let content = fs::read_to_string(path)?;
    for line in content.lines() {
        let t = line.trim();
        if t.is_empty() || t.starts_with('#') {
            continue;
        }
        if parse_revert_todo_line(repo, line).is_none() {
            eprintln!("error: invalid todo line in sequencer: {t}");
            std::process::exit(128);
        }
    }
    Ok(())
}

fn revert_todo_line_for_oid(repo: &Repository, oid: ObjectId) -> Result<String> {
    let obj = repo.odb.read(&oid)?;
    let commit = parse_commit(&obj.data)?;
    let subject = commit.message.lines().next().unwrap_or("");
    Ok(format!("revert {} {}", &oid.to_hex()[..7], subject))
}

fn save_revert_sequencer_after_failure(
    repo: &Repository,
    git_dir: &Path,
    orig_head: &ObjectId,
    remaining: &[ObjectId],
    args: &Args,
) -> Result<()> {
    let seq_dir = git_dir.join("sequencer");
    fs::create_dir_all(&seq_dir)?;
    fs::write(seq_dir.join("head"), format!("{}\n", orig_head.to_hex()))?;
    let mut todo = String::new();
    for oid in remaining {
        todo.push_str(&revert_todo_line_for_oid(repo, *oid)?);
        todo.push('\n');
    }
    fs::write(seq_dir.join("todo"), &todo)?;
    write_revert_sequencer_opts(git_dir, args)?;
    Ok(())
}

fn cleanup_revert_sequencer_only(git_dir: &Path) {
    let _ = fs::remove_dir_all(git_dir.join("sequencer"));
}

fn reset_revert_to_head_tree(repo: &Repository, git_dir: &Path) -> Result<()> {
    let head = resolve_head(git_dir)?;
    let head_oid = head
        .oid()
        .ok_or_else(|| anyhow::anyhow!("cannot resolve HEAD"))?;
    let obj = repo.odb.read(head_oid)?;
    let commit = parse_commit(&obj.data)?;
    let entries = tree_to_index_entries(repo, &commit.tree, "")?;
    let old_index = load_index(repo)?;
    let mut new_index = Index::new();
    new_index.entries = entries;
    new_index.sort();
    let index_path = repo.index_path();
    if let Some(wt) = &repo.work_tree {
        preflight_cherry_pick_cwd_obstruction(repo, wt, &new_index, &BTreeMap::new(), None)?;
    }
    repo.write_index_at(&index_path, &mut new_index)?;
    if let Some(wt) = &repo.work_tree {
        checkout_merged_index(repo, wt, &old_index, &new_index, &BTreeMap::new())?;
        refresh_index_stat_cache_from_worktree(repo, &mut new_index)?;
        repo.write_index_at(&index_path, &mut new_index)?;
    }
    Ok(())
}

fn run_revert_sequence(
    repo: &Repository,
    specs: &[String],
    args: &Args,
    orig_head_override: Option<ObjectId>,
) -> Result<()> {
    let git_dir = &repo.git_dir;
    let head_file_path = git_dir.join("sequencer").join("head");
    let orig_head_oid = if let Some(o) = orig_head_override {
        o
    } else if specs.len() > 1 && !args.no_commit {
        if let Ok(stored) = fs::read_to_string(&head_file_path) {
            if let Ok(parsed) = ObjectId::from_hex(stored.trim()) {
                parsed
            } else {
                let head = resolve_head(git_dir)?;
                *head.oid().ok_or_else(|| {
                    anyhow::anyhow!("cannot revert: HEAD does not point to a commit")
                })?
            }
        } else {
            let head = resolve_head(git_dir)?;
            *head
                .oid()
                .ok_or_else(|| anyhow::anyhow!("cannot revert: HEAD does not point to a commit"))?
        }
    } else {
        let head = resolve_head(git_dir)?;
        *head
            .oid()
            .ok_or_else(|| anyhow::anyhow!("cannot revert: HEAD does not point to a commit"))?
    };

    for (i, spec) in specs.iter().enumerate() {
        let remaining_specs = &specs[i + 1..];
        let remaining_oids: Result<Vec<ObjectId>> = remaining_specs
            .iter()
            .map(|s| resolve_revision(repo, s).with_context(|| format!("bad revision '{s}'")))
            .collect();
        let remaining_oids = remaining_oids?;

        match revert_one_commit(repo, spec, args) {
            Ok(()) => {
                if specs.len() > 1 && !args.no_commit {
                    strip_first_sequencer_todo_line(git_dir)?;
                    write_abort_safety_file(git_dir)?;
                }
            }
            Err(e) => {
                let err_msg = format!("{e}");
                if err_msg.contains("DIRTY_INDEX_REVERT") {
                    std::process::exit(128);
                }
                if err_msg.contains("CONFLICT_EXIT_REVERT") {
                    if specs.len() > 1 {
                        save_revert_sequencer_after_failure(
                            repo,
                            git_dir,
                            &orig_head_oid,
                            &remaining_oids,
                            args,
                        )?;
                        write_abort_safety_file(git_dir)?;
                    }
                    std::process::exit(1);
                }
                if err_msg.contains("EMPTY_REVERT_STOP") {
                    if specs.len() > 1 {
                        save_revert_sequencer_after_failure(
                            repo,
                            git_dir,
                            &orig_head_oid,
                            &remaining_oids,
                            args,
                        )?;
                        write_abort_safety_file(git_dir)?;
                    }
                    let user_msg = err_msg
                        .strip_prefix("EMPTY_REVERT_STOP: ")
                        .unwrap_or(&err_msg);
                    eprintln!("{user_msg}");
                    std::process::exit(1);
                }
                if specs.len() > 1 {
                    save_revert_sequencer_after_failure(
                        repo,
                        git_dir,
                        &orig_head_oid,
                        &remaining_oids,
                        args,
                    )?;
                }
                eprintln!("error: {e:#}");
                eprintln!("fatal: revert failed");
                std::process::exit(128);
            }
        }
    }

    cleanup_revert_sequencer_only(git_dir);
    Ok(())
}

/// Expand revert commit specs, handling A..B ranges.
/// For revert, A..B means revert commits from B down to (but not including) A,
/// in reverse order (newest first).
fn expand_revert_specs(repo: &Repository, specs: &[String]) -> Result<Vec<String>> {
    // `git revert` parses all arguments as a single revision set (setup_revisions):
    // `^X` and `A..B` contribute UNINTERESTING tips, bare refs are interesting tips.
    // When any range/exclusion is present, the reverts are ordered newest-first across
    // the whole reachable set (e.g. `revert ^first fourth` == `revert first..fourth`).
    let has_set_syntax = specs.iter().any(|s| s.starts_with('^') || s.contains(".."));
    if !has_set_syntax {
        return Ok(specs.to_vec());
    }

    let mut include: Vec<ObjectId> = Vec::new();
    let mut exclude: Vec<ObjectId> = Vec::new();
    for spec in specs {
        if let Some(neg) = spec.strip_prefix('^') {
            let oid =
                resolve_revision(repo, neg).with_context(|| format!("bad revision '{spec}'"))?;
            exclude.push(oid);
        } else if let Some((lhs, rhs)) = spec.split_once("..") {
            let lo =
                resolve_revision(repo, lhs).with_context(|| format!("bad revision '{lhs}'"))?;
            let hi =
                resolve_revision(repo, rhs).with_context(|| format!("bad revision '{rhs}'"))?;
            exclude.push(lo);
            include.push(hi);
        } else {
            let oid =
                resolve_revision(repo, spec).with_context(|| format!("bad revision '{spec}'"))?;
            include.push(oid);
        }
    }

    let ordered = revision_set_newest_first(repo, &include, &exclude)?;
    Ok(ordered.into_iter().map(|o| o.to_hex()).collect())
}

/// Walk commits reachable from `tip` but not from `base`, oldest first.
fn walk_commit_range(repo: &Repository, base: ObjectId, tip: ObjectId) -> Result<Vec<ObjectId>> {
    let mut result = Vec::new();
    let mut current = tip;
    loop {
        if current == base {
            break;
        }
        result.push(current);
        let obj = repo.odb.read(&current)?;
        let commit = parse_commit(&obj.data)?;
        if commit.parents.is_empty() {
            break;
        }
        current = commit.parents[0];
    }
    result.reverse(); // oldest first
    Ok(result)
}

fn append_signoff_revert(msg: &str, git_dir: &Path) -> Result<String> {
    let config = ConfigSet::load(Some(git_dir), true)?;
    let name = std::env::var("GIT_COMMITTER_NAME")
        .ok()
        .or_else(|| config.get("user.name"))
        .unwrap_or_else(|| "Unknown".to_owned());
    let email = std::env::var("GIT_COMMITTER_EMAIL")
        .ok()
        .or_else(|| config.get("user.email"))
        .unwrap_or_default();
    let signoff_line = format!("Signed-off-by: {name} <{email}>");
    if msg.contains(&signoff_line) {
        return Ok(msg.to_owned());
    }
    let trimmed = msg.trim_end();
    Ok(format!("{trimmed}\n\n{signoff_line}\n"))
}

fn should_use_reference_format(git_dir: &Path, args: &Args) -> Result<bool> {
    if args.no_reference {
        return Ok(false);
    }
    if args.reference {
        return Ok(true);
    }
    let config = ConfigSet::load(Some(git_dir), true)?;
    Ok(config.get_bool("revert.reference") == Some(Ok(true)))
}

fn merge_conflict_advice_enabled(git_dir: &Path) -> bool {
    let Ok(config) = ConfigSet::load(Some(git_dir), true) else {
        return true;
    };
    config.get_bool("advice.mergeConflict") != Some(Ok(false))
}

fn parse_strategy_options_revert(
    strategy_options: &[String],
) -> (MergeFavor, WhitespaceMergeOptions) {
    let mut favor = MergeFavor::None;
    let mut ws = WhitespaceMergeOptions::default();
    for opt in strategy_options {
        match opt.as_str() {
            "theirs" => favor = MergeFavor::Theirs,
            "ours" => favor = MergeFavor::Ours,
            "ignore-all-space" => ws.ignore_all_space = true,
            "ignore-space-change" => ws.ignore_space_change = true,
            "ignore-space-at-eol" => ws.ignore_space_at_eol = true,
            "ignore-cr-at-eol" => ws.ignore_cr_at_eol = true,
            _ => {}
        }
    }
    (favor, ws)
}

/// Abort the revert if local edits overlap paths the revert three-way merge would touch.
///
/// Revert's merge inputs are base = commit tree, ours = HEAD tree, theirs = parent tree.
/// Any staged-vs-HEAD or unstaged worktree change on a touched path would be overwritten,
/// so emit git's "Your local changes ... would be overwritten by merge" and fail (exit 128).
fn error_if_revert_would_clobber_worktree(
    repo: &Repository,
    head_oid: ObjectId,
    commit_tree: ObjectId,
    head_tree: ObjectId,
    parent_tree: ObjectId,
    work_tree: &Path,
) -> Result<()> {
    use std::collections::BTreeSet;
    let touched = merge_touched_paths(repo, commit_tree, head_tree, parent_tree)?;

    let staged_dirty = staged_dirty_paths_vs_head(repo, head_oid)?;
    let staged_overlap: BTreeSet<String> = staged_dirty.intersection(&touched).cloned().collect();
    if !staged_overlap.is_empty() {
        bail_local_changes_overwritten(&staged_overlap);
    }

    let index = repo.load_index()?;
    let gitlink_paths: BTreeSet<String> = index
        .entries
        .iter()
        .filter(|e| e.stage() == 0 && e.mode == MODE_GITLINK)
        .map(|e| String::from_utf8_lossy(&e.path).into_owned())
        .collect();
    let unstaged =
        grit_lib::diff::diff_index_to_worktree(&repo.odb, &index, work_tree, false, false)?;
    let unstaged_paths: BTreeSet<String> = unstaged
        .iter()
        .map(|e| e.path().to_string())
        .filter(|p| !gitlink_paths.contains(p))
        .collect();
    let unstaged_overlap: BTreeSet<String> =
        unstaged_paths.intersection(&touched).cloned().collect();
    if !unstaged_overlap.is_empty() {
        bail_local_changes_overwritten(&unstaged_overlap);
    }
    Ok(())
}

fn bail_local_changes_overwritten(paths: &std::collections::BTreeSet<String>) -> ! {
    eprintln!("error: Your local changes to the following files would be overwritten by merge:");
    for p in paths {
        eprintln!("\t{p}");
    }
    eprintln!("Please commit your changes or stash them before you merge.");
    eprintln!("Aborting");
    eprintln!("fatal: revert failed");
    std::process::exit(128);
}

fn error_dirty_index_revert(repo: &Repository, head_oid: ObjectId) -> Result<()> {
    if super::merge::index_matches_head_tree(repo, head_oid)? {
        return Ok(());
    }
    eprintln!("your local changes would be overwritten by revert.");
    let git_dir = &repo.git_dir;
    let Ok(config) = ConfigSet::load(Some(git_dir), true) else {
        return Ok(());
    };
    if config.get_bool("advice.commitBeforeMerge") != Some(Ok(false)) {
        eprintln!("commit your changes or stash them to proceed.");
    }
    bail!("DIRTY_INDEX_REVERT");
}

fn is_effective_editor_value(raw: &str) -> bool {
    let t = raw.trim();
    !t.is_empty() && t != ":"
}

fn resolve_revert_editor(git_dir: &Path) -> String {
    if let Ok(e) = std::env::var("GIT_EDITOR") {
        if is_effective_editor_value(&e) {
            return e;
        }
    }
    if let Ok(config) = ConfigSet::load(Some(git_dir), true) {
        if let Some(e) = config.get("core.editor") {
            if is_effective_editor_value(&e) {
                return e;
            }
        }
    }
    if let Ok(e) = std::env::var("VISUAL") {
        if is_effective_editor_value(&e) {
            return e;
        }
    }
    if let Ok(e) = std::env::var("EDITOR") {
        if is_effective_editor_value(&e) {
            return e;
        }
    }
    if std::env::var("VISUAL").is_ok() || std::env::var("EDITOR").is_ok() || !stdin().is_terminal()
    {
        "true".to_owned()
    } else {
        "vi".to_owned()
    }
}

fn comment_char_for_revert(git_dir: &Path) -> char {
    let Ok(config) = ConfigSet::load(Some(git_dir), true) else {
        return '#';
    };
    let Some(raw) = config.get("core.commentChar") else {
        return '#';
    };
    let t = raw.trim();
    if t.eq_ignore_ascii_case("auto") {
        return '#';
    }
    t.chars().next().unwrap_or('#')
}

fn launch_revert_editor(git_dir: &Path, path: &Path) -> Result<()> {
    let editor = resolve_revert_editor(git_dir);
    let status = Command::new("sh")
        .arg("-c")
        .arg(format!("{editor} \"$1\""))
        .arg("sh")
        .arg(path)
        .status()
        .with_context(|| format!("failed to launch editor '{editor}'"))?;
    if !status.success() {
        bail!("editor exited with non-zero status");
    }
    Ok(())
}

fn cleanup_edited_revert_message(message: &str, comment_prefix: char) -> String {
    let prefix = comment_prefix.to_string();
    let mut out = String::new();
    let mut empties = 0usize;
    let mut i = 0usize;
    while i < message.len() {
        let rest = &message[i..];
        let (line_with_nl, advance) = if let Some(pos) = rest.find('\n') {
            (&rest[..=pos], pos + 1)
        } else {
            (rest, rest.len())
        };
        i += advance;
        if line_with_nl.starts_with(&prefix) {
            continue;
        }
        let content_len = line_with_nl.trim_end_matches(['\r', '\n', ' ', '\t']).len();
        if content_len > 0 {
            if empties > 0 && !out.is_empty() {
                out.push('\n');
            }
            empties = 0;
            out.push_str(&line_with_nl[..content_len]);
            out.push('\n');
        } else {
            empties += 1;
        }
    }
    out
}

fn revert_one_commit(repo: &Repository, spec: &str, args: &Args) -> Result<()> {
    let git_dir = &repo.git_dir;

    // Resolve commit to revert.
    let commit_oid =
        resolve_revision(repo, spec).with_context(|| format!("bad revision '{spec}'"))?;
    let commit_obj = repo.odb.read(&commit_oid)?;
    if commit_obj.kind != ObjectKind::Commit {
        bail!("object {} is not a commit", commit_oid);
    }
    let commit = parse_commit(&commit_obj.data)?;

    // Parent tree for the original change (empty tree for root commits, matching Git).
    let parent_tree_oid = if commit.parents.len() > 1 {
        let m = args.mainline.ok_or_else(|| {
            anyhow::anyhow!(
                "commit {} is a merge but no -m option was given",
                commit_oid
            )
        })?;
        if m == 0 || m > commit.parents.len() {
            bail!("commit {} does not have parent {}", commit_oid, m);
        }
        let parent_oid = commit.parents[m - 1];
        let parent_obj = repo.odb.read(&parent_oid)?;
        let parent_commit = parse_commit(&parent_obj.data)?;
        parent_commit.tree
    } else if commit.parents.is_empty() {
        repo.odb.write(ObjectKind::Tree, &[])?
    } else {
        let parent_oid = commit.parents[0];
        let parent_obj = repo.odb.read(&parent_oid)?;
        let parent_commit = parse_commit(&parent_obj.data)?;
        parent_commit.tree
    };

    // The commit's own tree.
    let commit_tree_oid = commit.tree;

    // Resolve HEAD tree.
    let head = resolve_head(git_dir)?;
    let head_oid = head
        .oid()
        .ok_or_else(|| anyhow::anyhow!("cannot revert: HEAD does not point to a commit"))?
        .to_owned();
    let head_obj = repo.odb.read(&head_oid)?;
    let head_commit = parse_commit(&head_obj.data)?;
    let head_tree_oid = head_commit.tree;

    if !args.no_commit {
        error_dirty_index_revert(repo, head_oid)?;
    }

    if let Some(wt) = repo.work_tree.as_deref() {
        // Refuse to overwrite local (unstaged/staged) edits that overlap the reverted paths,
        // matching `git revert`'s unpack_trees check (t3507 "revert w/dirty tree ...").
        error_if_revert_would_clobber_worktree(
            repo,
            head_oid,
            commit_tree_oid,
            head_tree_oid,
            parent_tree_oid,
            wt,
        )?;

        let base_map = tree_entries_to_map(tree_to_index_entries(repo, &commit_tree_oid, "")?);
        let ours_map = tree_entries_to_map(tree_to_index_entries(repo, &head_tree_oid, "")?);
        let theirs_map = tree_entries_to_map(tree_to_index_entries(repo, &parent_tree_oid, "")?);
        bail_if_df_merge_would_remove_cwd(wt, &base_map, &ours_map, &theirs_map)?;
    }

    let config = ConfigSet::load(Some(git_dir), true)?;

    let use_reference = should_use_reference_format(git_dir, args)?;
    let comment_char = comment_char_for_revert(git_dir);
    let (favor, ws_opts) = parse_strategy_options_revert(&args.strategy_option);
    let short_oid = &commit_oid.to_hex()[..7];
    let subject = commit.message.lines().next().unwrap_or("");
    let label_theirs = format!("parent of {short_oid} ({subject})");
    let label_base = format!("{short_oid} ({subject})");

    let conflict_style = match config.get("merge.conflictstyle").as_deref() {
        Some("diff3") | Some("zdiff3") => ConflictStyle::Diff3,
        _ => ConflictStyle::Merge,
    };

    let merged = merge_trees_three_way(
        repo,
        commit_tree_oid,
        head_tree_oid,
        parent_tree_oid,
        favor,
        ws_opts,
        None,
        TreeMergeConflictPresentation {
            label_ours: "HEAD",
            label_theirs: TheirsConflictLabel::Fixed(label_theirs.as_str()),
            label_base: label_base.as_str(),
            style: conflict_style,
            checkout_merge: false,
        },
    )?;
    let mut merged_index = merged.index;
    let conflict_map = merged.conflict_content;

    // Check for conflicts (any entry with stage != 0).
    let has_conflicts = merged_index.entries.iter().any(|e| e.stage() != 0);

    // Check if the revert produces an empty commit (no changes).
    if !has_conflicts && index_tree_oid_matches_head(&repo.odb, &merged_index, &head_tree_oid)? {
        bail!("EMPTY_REVERT_STOP: error: The previous revert is now empty, possibly due to conflict resolution.");
    }

    // Load old index BEFORE writing new one (needed for worktree cleanup).
    let old_index = load_index(repo)?;

    // Update working tree.
    let work_tree = repo
        .work_tree
        .as_deref()
        .ok_or_else(|| anyhow::anyhow!("cannot revert in a bare repository"))?;
    preflight_cherry_pick_cwd_obstruction(repo, work_tree, &merged_index, &conflict_map, None)?;

    // Write index.
    let index_path = repo.index_path();
    repo.write_index_at(&index_path, &mut merged_index)
        .context("writing index")?;

    checkout_merged_index(repo, work_tree, &old_index, &merged_index, &conflict_map)?;
    refresh_index_stat_cache_from_worktree(repo, &mut merged_index)?;
    repo.write_index_at(&index_path, &mut merged_index)
        .context("refreshing index stat cache after revert checkout")?;

    let (title_line, body_suffix) =
        merge_commit_message_for_revert(&commit, commit_oid, use_reference, comment_char);
    let template_msg = if use_reference {
        format!("{title_line}\n\n{body_suffix}")
    } else {
        // git separates the `Revert "..."` subject from the `This reverts commit ...` body with
        // a blank line (sequencer.c's revert message; `title_line` already ends in one `\n`).
        format!("{title_line}\n{body_suffix}")
    };

    if has_conflicts {
        fs::write(
            git_dir.join("REVERT_HEAD"),
            format!("{}\n", commit_oid.to_hex()),
        )?;

        let commit_cleanup = config.get("commit.cleanup");
        let cleanup_mode = args
            .cleanup
            .as_deref()
            .or(commit_cleanup.as_deref())
            .unwrap_or("default");
        let is_scissors = cleanup_mode.eq_ignore_ascii_case("scissors");
        let mut merge_msg = cleanup_message(&template_msg, cleanup_mode);
        // Always append the `# Conflicts:` block; the scissors cut-line only for cleanup=scissors
        // (t3507 revert scissors / default-cleanup MERGE_MSG), matching append_conflicts_hint.
        let paths = unmerged_paths(&merged_index);
        append_merge_msg_conflict_footer(&mut merge_msg, &paths, is_scissors);
        fs::write(git_dir.join("MERGE_MSG"), &merge_msg)?;

        eprintln!("error: could not revert {short_oid}... {subject}");
        if let Ok(help) = std::env::var("GIT_REVERT_HELP") {
            eprintln!("hint: {help}");
        } else if merge_conflict_advice_enabled(git_dir) {
            eprintln!("hint: After resolving the conflicts, mark them with");
            eprintln!("hint: \"git add/rm <pathspec>\", then run");
            eprintln!("hint: \"git revert --continue\".");
            eprintln!("hint: You can instead skip this commit with \"git revert --skip\".");
            eprintln!("hint: To abort and get back to the state before \"git revert\",");
            eprintln!("hint: run \"git revert --abort\".");
            eprintln!(
                "hint: Disable this message with \"git config set advice.mergeConflict false\""
            );
        }
        return Err(anyhow::anyhow!("CONFLICT_EXIT_REVERT"));
    }

    if args.no_commit {
        fs::write(
            git_dir.join("REVERT_HEAD"),
            format!("{}\n", commit_oid.to_hex()),
        )?;
        return Ok(());
    }

    let mut msg = template_msg;
    if args.signoff {
        msg = append_signoff_revert(&msg, git_dir)?;
    }

    let should_edit = args.edit || (!args.no_edit && stdin().is_terminal());
    if should_edit {
        let mut tmp = NamedTempFile::new_in(git_dir).context("temp file for revert message")?;
        let p = tmp.path().to_path_buf();
        write!(tmp, "{msg}")?;
        tmp.flush()?;
        launch_revert_editor(git_dir, &p)?;
        let edited = fs::read_to_string(&p).unwrap_or_default();
        msg = cleanup_edited_revert_message(&edited, comment_char);
        if msg.trim().is_empty() {
            bail!("Aborting commit due to empty commit message.");
        }
    }

    create_revert_commit(repo, &head, &merged_index, &msg)?;

    // Print summary.
    let short_oid_new = {
        let new_head = resolve_head(git_dir)?;
        let new_oid = new_head
            .oid()
            .ok_or_else(|| anyhow::anyhow!("HEAD has no OID"))?;
        new_oid.to_hex()[..7].to_owned()
    };
    let branch = match &head {
        HeadState::Branch { short_name, .. } => short_name.as_str(),
        HeadState::Detached { .. } => "HEAD detached",
        HeadState::Invalid => "unknown",
    };
    let first_line = msg.lines().next().unwrap_or("");
    eprintln!("[{branch} {short_oid_new}] {first_line}");

    Ok(())
}

// ── --continue ──────────────────────────────────────────────────────

pub(crate) fn do_continue() -> Result<()> {
    let repo = Repository::discover(None).context("not a git repository")?;
    let git_dir = &repo.git_dir;

    let mut opts = Args {
        commits: vec![],
        no_commit: false,
        signoff: false,
        mainline: None,
        r#continue: true,
        abort: false,
        skip: false,
        quit: false,
        strategy: None,
        strategy_option: vec![],
        no_edit: false,
        edit: false,
        reference: false,
        no_reference: false,
        cleanup: None,
    };
    merge_revert_sequencer_opts(git_dir, &mut opts);

    let has_revert_head = git_dir.join("REVERT_HEAD").exists();
    let seq_todo = git_dir.join("sequencer").join("todo");
    let has_seq = seq_todo.exists();

    if !has_revert_head && !has_seq {
        bail!("error: no revert in progress");
    }

    if has_seq {
        validate_revert_sequencer_todo(&repo, git_dir)?;
    }

    if !has_revert_head && has_seq {
        let head_file = git_dir.join("sequencer").join("head");
        let stored_orig = if let Ok(s) = fs::read_to_string(&head_file) {
            ObjectId::from_hex(s.trim()).ok()
        } else {
            None
        };
        let remaining = load_revert_sequencer_todo(&repo, git_dir);
        cleanup_revert_sequencer_only(git_dir);
        let specs: Vec<String> = remaining.iter().map(|o| o.to_hex()).collect();
        if !specs.is_empty() {
            run_revert_sequence(&repo, &specs, &opts, stored_orig)?;
        }
        return Ok(());
    }

    let index = load_index(&repo)?;
    if index.entries.iter().any(|e| e.stage() != 0) {
        eprintln!(
            "error: commit is not possible because you have unmerged files\n\
             hint: fix conflicts and then commit the result with 'git revert --continue'"
        );
        std::process::exit(128);
    }

    let mut msg = match fs::read_to_string(git_dir.join("MERGE_MSG")) {
        Ok(m) => m,
        Err(_) => {
            let revert_oid = fs::read_to_string(git_dir.join("REVERT_HEAD"))?;
            let revert_oid = revert_oid.trim();
            let oid = ObjectId::from_hex(revert_oid)?;
            let obj = repo.odb.read(&oid)?;
            let commit = parse_commit(&obj.data)?;
            let subject = commit.message.lines().next().unwrap_or("");
            format!("Revert \"{subject}\"\n\nThis reverts commit {revert_oid}.\n")
        }
    };

    if opts.signoff {
        msg = append_signoff_revert(&msg, git_dir)?;
    }

    let head = resolve_head(git_dir)?;
    create_revert_commit(&repo, &head, &index, &msg)?;

    let new_head = resolve_head(git_dir)?;
    let new_oid = new_head
        .oid()
        .ok_or_else(|| anyhow::anyhow!("HEAD has no OID"))?;
    let short = &new_oid.to_hex()[..7];
    let branch = match &head {
        HeadState::Branch { short_name, .. } => short_name.as_str(),
        HeadState::Detached { .. } => "HEAD detached",
        HeadState::Invalid => "unknown",
    };
    let first_line = msg.lines().next().unwrap_or("");
    eprintln!("[{branch} {short}] {first_line}");

    let remaining = load_revert_sequencer_todo(&repo, git_dir);
    let _ = fs::remove_file(git_dir.join("REVERT_HEAD"));
    let _ = fs::remove_file(git_dir.join("CHERRY_PICK_HEAD"));
    let _ = fs::remove_file(git_dir.join("MERGE_MSG"));

    if !remaining.is_empty() {
        strip_first_sequencer_todo_line(git_dir)?;
        write_abort_safety_file(git_dir)?;
        let specs: Vec<String> = remaining.iter().map(|o| o.to_hex()).collect();
        run_revert_sequence(&repo, &specs, &opts, None)?;
    } else {
        cleanup_revert_sequencer_only(git_dir);
    }

    Ok(())
}

/// After a manual `git commit` finished the current revert, resume remaining `sequencer/todo` reverts.
/// NOTE: git does NOT auto-resume on a plain commit (only `revert --continue` advances); kept unused.
#[allow(dead_code)]
pub(crate) fn try_resume_revert_sequence_after_commit(repo: &Repository) -> Result<()> {
    let git_dir = &repo.git_dir;
    if !git_dir.join("sequencer").join("todo").exists() {
        return Ok(());
    }
    if !sequencer_is_revert_sequence(git_dir) {
        return Ok(());
    }
    if git_dir.join("REVERT_HEAD").exists() {
        return Ok(());
    }

    let mut opts = Args {
        commits: vec![],
        no_commit: false,
        signoff: false,
        mainline: None,
        r#continue: true,
        abort: false,
        skip: false,
        quit: false,
        strategy: None,
        strategy_option: vec![],
        no_edit: false,
        edit: false,
        reference: false,
        no_reference: false,
        cleanup: None,
    };
    merge_revert_sequencer_opts(git_dir, &mut opts);
    validate_revert_sequencer_todo(repo, git_dir)?;

    let remaining = load_revert_sequencer_todo(repo, git_dir);
    if !remaining.is_empty() {
        let specs: Vec<String> = remaining.iter().map(|o| o.to_hex()).collect();
        run_revert_sequence(repo, &specs, &opts, None)?;
    } else {
        cleanup_revert_sequencer_only(git_dir);
    }
    Ok(())
}

fn do_skip(mut args: Args) -> Result<()> {
    let repo = Repository::discover(None).context("not a git repository")?;
    let git_dir = &repo.git_dir;

    merge_revert_sequencer_opts(git_dir, &mut args);
    let args = &args;

    let has_rv = git_dir.join("REVERT_HEAD").exists();
    let seq_rev = sequencer_is_revert_sequence(git_dir);

    if has_rv {
        reset_revert_to_head_tree(&repo, git_dir)?;
        let remaining = load_revert_sequencer_todo(&repo, git_dir);
        let _ = fs::remove_file(git_dir.join("REVERT_HEAD"));
        let _ = fs::remove_file(git_dir.join("CHERRY_PICK_HEAD"));
        let _ = fs::remove_file(git_dir.join("MERGE_MSG"));
        if !remaining.is_empty() {
            strip_first_sequencer_todo_line(git_dir)?;
            write_abort_safety_file(git_dir)?;
            let specs: Vec<String> = remaining.iter().map(|o| o.to_hex()).collect();
            run_revert_sequence(&repo, &specs, args, None)?;
        } else {
            cleanup_revert_sequencer_only(git_dir);
        }
        return Ok(());
    }

    if seq_rev {
        if !rollback_is_safe(git_dir) {
            eprintln!("error: there is nothing to skip");
            eprintln!("hint: have you committed already?");
            eprintln!("hint: try \"git revert --continue\"");
            eprintln!("fatal: revert failed");
            std::process::exit(128);
        }
        reset_revert_to_head_tree(&repo, git_dir)?;
        let remaining = load_revert_sequencer_todo(&repo, git_dir);
        let _ = fs::remove_file(git_dir.join("REVERT_HEAD"));
        let _ = fs::remove_file(git_dir.join("CHERRY_PICK_HEAD"));
        let _ = fs::remove_file(git_dir.join("MERGE_MSG"));
        if !remaining.is_empty() {
            strip_first_sequencer_todo_line(git_dir)?;
            write_abort_safety_file(git_dir)?;
            let specs: Vec<String> = remaining.iter().map(|o| o.to_hex()).collect();
            run_revert_sequence(&repo, &specs, args, None)?;
        } else {
            cleanup_revert_sequencer_only(git_dir);
        }
        return Ok(());
    }

    eprintln!("error: no revert in progress");
    std::process::exit(1);
}

fn do_quit() -> Result<()> {
    let repo = Repository::discover(None).context("not a git repository")?;
    let git_dir = &repo.git_dir;
    let in_progress =
        git_dir.join("REVERT_HEAD").exists() || git_dir.join("sequencer").join("todo").exists();
    if !in_progress {
        return Ok(());
    }
    let _ = fs::remove_file(git_dir.join("REVERT_HEAD"));
    let _ = fs::remove_file(git_dir.join("CHERRY_PICK_HEAD"));
    let _ = fs::remove_file(git_dir.join("MERGE_MSG"));
    cleanup_revert_sequencer_only(git_dir);
    Ok(())
}

// ── Helpers ─────────────────────────────────────────────────────────

fn load_index(repo: &Repository) -> Result<Index> {
    Ok(repo.load_index()?)
}

fn create_revert_commit(
    repo: &Repository,
    head: &HeadState,
    index: &Index,
    message: &str,
) -> Result<()> {
    let tree_oid = write_tree_from_index(&repo.odb, index, "")?;
    let git_dir = &repo.git_dir;

    let mut parents = Vec::new();
    if let Some(head_oid) = head.oid() {
        parents.push(*head_oid);
    }

    let config = ConfigSet::load(Some(git_dir), true)?;
    let now = time::OffsetDateTime::now_utc();
    let author = resolve_identity(&config, "AUTHOR")?;
    let committer = resolve_identity(&config, "COMMITTER")?;

    let commit_enc = config
        .get("i18n.commitEncoding")
        .or_else(|| config.get("i18n.commitencoding"));
    let (stored_msg, encoding, raw_message) =
        grit_lib::commit_encoding::finalize_stored_commit_message(
            message.to_owned(),
            commit_enc.as_deref(),
        );

    let author_line = format_ident(&author, now);
    let committer_line = format_ident(&committer, now);
    let (author_raw, committer_raw) = grit_lib::commit_encoding::identity_raw_for_serialized_commit(
        &encoding,
        &author_line,
        &committer_line,
    );

    let commit_data = CommitData {
        tree: tree_oid,
        parents,
        author: author_line,
        committer: committer_line,
        author_raw,
        committer_raw,
        encoding,
        message: stored_msg,
        raw_message,
    };

    let commit_bytes = serialize_commit(&commit_data);
    let commit_oid = repo.odb.write(ObjectKind::Commit, &commit_bytes)?;

    let old_oid = head.oid().copied().unwrap_or_else(ObjectId::zero);
    update_head(git_dir, head, &commit_oid)?;

    let reflog_subject = message
        .lines()
        .find(|l| !l.trim().is_empty() && !l.trim_start().starts_with('#'))
        .unwrap_or_else(|| message.lines().next().unwrap_or(""));
    let reflog_msg = format!("revert: {reflog_subject}");
    let ident = &commit_data.committer;
    let _ = append_reflog(
        git_dir,
        "HEAD",
        &old_oid,
        &commit_oid,
        ident,
        &reflog_msg,
        false,
    );
    if let HeadState::Branch { refname, .. } = head {
        let _ = append_reflog(
            git_dir,
            refname,
            &old_oid,
            &commit_oid,
            ident,
            &reflog_msg,
            false,
        );
    }

    let _ = fs::remove_file(git_dir.join("REVERT_HEAD"));
    let _ = fs::remove_file(git_dir.join("CHERRY_PICK_HEAD"));
    let _ = fs::remove_file(git_dir.join("MERGE_MSG"));

    Ok(())
}

fn resolve_identity(config: &ConfigSet, kind: &str) -> Result<(String, String)> {
    let name_var = format!("GIT_{kind}_NAME");
    let email_var = format!("GIT_{kind}_EMAIL");

    let mut name = match crate::ident::read_git_identity_name_env(&name_var) {
        crate::ident::GitIdentityNameEnv::Set(s) => s,
        crate::ident::GitIdentityNameEnv::Unset => {
            if let Some(v) = config.get("user.name") {
                let t = v.trim();
                if !t.is_empty() {
                    t.to_owned()
                } else {
                    crate::ident::ident_default_name(config)
                }
            } else {
                crate::ident::ident_default_name(config)
            }
        }
    };
    if name.trim().is_empty() {
        name = "Unknown".to_owned();
    }
    let email = std::env::var(&email_var)
        .ok()
        .or_else(|| config.get("user.email"))
        .unwrap_or_default();

    Ok((name, email))
}

fn format_ident(ident: &(String, String), now: time::OffsetDateTime) -> String {
    let (name, email) = ident;
    let epoch = now.unix_timestamp();
    let offset = now.offset();
    let hours = offset.whole_hours();
    let minutes = offset.minutes_past_hour().unsigned_abs();

    let date_str = std::env::var(if name == "Unknown" {
        "GIT_COMMITTER_DATE"
    } else {
        "GIT_AUTHOR_DATE"
    })
    .ok();

    let timestamp = date_str
        .map(|d| grit_lib::commit::parse_date_to_git_timestamp(&d).unwrap_or(d))
        .unwrap_or_else(|| format!("{epoch} {hours:+03}{minutes:02}"));
    format!("{name} <{email}> {timestamp}")
}

fn update_head(git_dir: &Path, head: &HeadState, commit_oid: &ObjectId) -> Result<()> {
    match head {
        HeadState::Branch { refname, .. } => {
            let ref_path = git_dir.join(refname);
            if let Some(parent) = ref_path.parent() {
                fs::create_dir_all(parent)?;
            }
            fs::write(&ref_path, format!("{}\n", commit_oid.to_hex()))?;
        }
        HeadState::Detached { .. } | HeadState::Invalid => {
            fs::write(git_dir.join("HEAD"), format!("{}\n", commit_oid.to_hex()))?;
        }
    }
    Ok(())
}

// ── Tree → index helpers ────────────────────────────────────────────

fn tree_entries_to_map(entries: Vec<IndexEntry>) -> HashMap<Vec<u8>, IndexEntry> {
    let mut m = HashMap::new();
    for e in entries {
        m.insert(e.path.clone(), e);
    }
    m
}

fn tree_to_index_entries(
    repo: &Repository,
    oid: &ObjectId,
    prefix: &str,
) -> Result<Vec<IndexEntry>> {
    let obj = repo.odb.read(oid)?;
    if obj.kind != ObjectKind::Tree {
        bail!("expected tree, got {}", obj.kind);
    }
    let entries = parse_tree(&obj.data)?;
    let mut result = Vec::new();

    for te in entries {
        let name = String::from_utf8_lossy(&te.name).into_owned();
        let path = if prefix.is_empty() {
            name.clone()
        } else {
            format!("{prefix}/{name}")
        };

        if te.mode == 0o040000 {
            let sub = tree_to_index_entries(repo, &te.oid, &path)?;
            result.extend(sub);
        } else {
            let path_bytes = path.into_bytes();
            result.push(IndexEntry {
                ctime_sec: 0,
                ctime_nsec: 0,
                mtime_sec: 0,
                mtime_nsec: 0,
                dev: 0,
                ino: 0,
                mode: te.mode,
                uid: 0,
                gid: 0,
                size: 0,
                oid: te.oid,
                flags: path_bytes.len().min(0xFFF) as u16,
                flags_extended: None,
                path: path_bytes,
                base_index_pos: 0,
            });
        }
    }
    Ok(result)
}

/// Write merged index entries to the working tree.
fn checkout_merged_index(
    repo: &Repository,
    work_tree: &Path,
    old_index: &Index,
    index: &Index,
    conflict_content: &BTreeMap<Vec<u8>, ObjectId>,
) -> Result<()> {
    let has_unmerged = index.entries.iter().any(|e| e.stage() != 0);
    if !has_unmerged {
        return checkout_index_to_worktree_allowing_submodule_replacement(
            repo, old_index, index, work_tree, false, false, true,
        );
    }

    let mut stage0_only = Index::new();
    stage0_only.version = index.version;
    stage0_only.sparse_directories = index.sparse_directories;
    stage0_only.untracked_cache = index.untracked_cache.clone();
    stage0_only.entries = index
        .entries
        .iter()
        .filter(|e| e.stage() == 0)
        .cloned()
        .collect();
    stage0_only.sort();
    checkout_index_to_worktree_allowing_submodule_replacement(
        repo,
        old_index,
        &stage0_only,
        work_tree,
        false,
        false,
        true,
    )?;

    let mut written: HashSet<Vec<u8>> = HashSet::new();
    for entry in &index.entries {
        if entry.stage() != 2 || written.contains(&entry.path) {
            continue;
        }
        let has_stage0 = index
            .entries
            .iter()
            .any(|e| e.path == entry.path && e.stage() == 0);
        if has_stage0 {
            continue;
        }

        let mut e = entry.clone();
        if let Some(marker_oid) = conflict_content.get(&entry.path) {
            e.oid = *marker_oid;
        }
        let path_str = String::from_utf8_lossy(&entry.path).into_owned();
        let abs_path = work_tree.join(&path_str);
        write_entry_to_worktree(repo, work_tree, &path_str, &abs_path, &e)?;
        written.insert(entry.path.clone());
    }

    Ok(())
}

fn write_entry_to_worktree(
    repo: &Repository,
    work_tree: &Path,
    path_str: &str,
    abs_path: &Path,
    entry: &IndexEntry,
) -> Result<()> {
    if let Some(parent) = abs_path.parent() {
        fs::create_dir_all(parent)?;
    }

    if entry.mode == 0o160000 {
        if grit_lib::worktree_cwd::cwd_would_be_removed_with_repo_path(work_tree, path_str) {
            bail!("Refusing to remove the current working directory:\n{path_str}\n");
        }
        if abs_path.is_file() || abs_path.is_symlink() {
            let _ = fs::remove_file(abs_path);
        } else if abs_path.is_dir() {
            let _ = fs::remove_dir_all(abs_path);
        }
        fs::create_dir_all(abs_path)?;
        return Ok(());
    }

    let obj = repo
        .odb
        .read(&entry.oid)
        .context("reading object for checkout")?;

    if entry.mode == MODE_SYMLINK {
        let target =
            String::from_utf8(obj.data).map_err(|_| anyhow::anyhow!("symlink not UTF-8"))?;
        if abs_path.exists() || abs_path.is_symlink() {
            if grit_lib::worktree_cwd::cwd_would_be_removed_with_repo_path(work_tree, path_str) {
                bail!("Refusing to remove the current working directory:\n{path_str}\n");
            }
            let _ = fs::remove_file(abs_path);
        }
        std::os::unix::fs::symlink(target, abs_path)?;
    } else {
        if abs_path.is_dir() {
            if grit_lib::worktree_cwd::cwd_would_be_removed_with_repo_path(work_tree, path_str) {
                bail!("Refusing to remove the current working directory:\n{path_str}\n");
            }
            fs::remove_dir_all(abs_path)?;
        }
        fs::write(abs_path, &obj.data)?;
        if entry.mode == MODE_EXECUTABLE {
            use std::os::unix::fs::PermissionsExt;
            let mut perms = fs::metadata(abs_path)?.permissions();
            perms.set_mode(0o755);
            fs::set_permissions(abs_path, perms)?;
        }
    }

    Ok(())
}

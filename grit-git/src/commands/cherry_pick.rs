//! `grit cherry-pick` — apply the changes introduced by existing commits.
//!
//! Cherry-pick applies the diff of a commit onto the current HEAD using a
//! three-way merge:
//!   - base   = parent_tree  (state before the picked commit)
//!   - ours   = HEAD_tree    (current state)
//!   - theirs = commit_tree  (the commit being picked)

use anyhow::{bail, Context, Result};
use clap::Args as ClapArgs;
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::fs;
use std::path::Path;

use grit_lib::commit_trailers::{
    append_cherry_picked_from_line, append_signoff_trailer, finalize_cherry_pick_message,
    format_signoff_line,
};
use grit_lib::config::ConfigSet;
use grit_lib::diff::diff_index_to_worktree;
use grit_lib::hooks::{run_commit_hook, CommitHookEnv, HookResult};
use grit_lib::ident::parse_signature_times;
use grit_lib::index::{Index, IndexEntry, MODE_EXECUTABLE, MODE_GITLINK, MODE_SYMLINK, MODE_TREE};
use grit_lib::merge_file::{merge, ConflictStyle, MergeFavor, MergeInput};
use grit_lib::merge_trees::{
    merge_trees_three_way, TheirsConflictLabel, TreeMergeConflictPresentation,
    WhitespaceMergeOptions,
};
use grit_lib::objects::{parse_commit, serialize_commit, CommitData, ObjectId, ObjectKind};
use grit_lib::odb::Odb;
use grit_lib::reflog::read_reflog;
use grit_lib::refs::append_reflog;
use grit_lib::repo::Repository;
use grit_lib::rev_parse::resolve_revision;
use grit_lib::sparse_checkout::apply_sparse_checkout_skip_worktree;
use grit_lib::state::{resolve_head, HeadState};
use grit_lib::write_tree::write_tree_from_index;

use std::sync::OnceLock;

static CHERRY_PICK_REV_OPTS: OnceLock<(Option<usize>, Option<String>)> = OnceLock::new();

/// Log text used for cherry-pick message rewriting.
///
/// When the commit object stores a message without a trailing newline, [`CommitData::message`]
/// omits the final incomplete line (Git's `find_commit_subject` behaviour). Prefer the raw body
/// bytes in that case so `-x`/`-s` match Git (`t3511-cherry-pick-x`).
fn cherry_pick_source_message(commit: &CommitData) -> String {
    match &commit.raw_message {
        Some(raw) if !raw.ends_with(b"\n") => String::from_utf8_lossy(raw).into_owned(),
        _ => commit.message.clone(),
    }
}

/// Strip Git revision-walking options from argv before clap parsing.
///
/// Handles `-<n>` (max count) and `--author=<pat>` / `--author <pat>` like `git cherry-pick`.
pub fn preprocess_cherry_pick_argv(rest: &[String]) -> Vec<String> {
    let mut max_count: Option<usize> = None;
    let mut author: Option<String> = None;
    let mut out = Vec::new();
    let mut i = 0;
    while i < rest.len() {
        let a = &rest[i];
        if a == "--" {
            out.extend_from_slice(&rest[i..]);
            break;
        }
        if let Some(v) = a.strip_prefix("--author=") {
            author = Some(v.to_string());
            i += 1;
            continue;
        }
        if a == "--author" && i + 1 < rest.len() {
            author = Some(rest[i + 1].clone());
            i += 2;
            continue;
        }
        if let Some(digits) = a.strip_prefix('-') {
            if !digits.is_empty() && digits.chars().all(|c| c.is_ascii_digit()) {
                if let Ok(n) = digits.parse::<usize>() {
                    max_count = Some(n);
                    i += 1;
                    continue;
                }
            }
        }
        out.push(a.clone());
        i += 1;
    }
    let _ = CHERRY_PICK_REV_OPTS.set((max_count, author));
    out
}

fn cherry_pick_rev_max_count() -> Option<usize> {
    CHERRY_PICK_REV_OPTS.get().and_then(|(m, _)| *m)
}

fn cherry_pick_rev_author() -> Option<String> {
    CHERRY_PICK_REV_OPTS.get().and_then(|(_, a)| a.clone())
}

/// Whether the picked commit's tree matches its first parent's tree (or the empty tree for roots).
fn is_original_commit_empty(repo: &Repository, commit: &CommitData) -> Result<bool> {
    let parent_tree_oid = if commit.parents.is_empty() {
        repo.odb.write(ObjectKind::Tree, &[])?
    } else {
        let parent_obj = repo.odb.read(&commit.parents[0])?;
        parse_commit(&parent_obj.data)?.tree
    };
    Ok(parent_tree_oid == commit.tree)
}

/// Resolves how to handle a cherry-pick whose merged tree equals `HEAD` (index unchanged vs HEAD).
fn resolve_empty_pick_resolution(
    originally_empty: bool,
    args: &Args,
) -> Result<EmptyPickResolution> {
    let drop_redundant = matches!(args.empty.as_deref(), Some("drop"));
    let keep_redundant =
        args.keep_redundant_commits || matches!(args.empty.as_deref(), Some("keep"));
    let allow_initial_empty = args.allow_empty || args.keep_redundant_commits;

    if originally_empty {
        return Ok(if allow_initial_empty {
            EmptyPickResolution::Proceed
        } else {
            EmptyPickResolution::Stop
        });
    }
    if keep_redundant {
        return Ok(EmptyPickResolution::Proceed);
    }
    if drop_redundant {
        return Ok(EmptyPickResolution::Drop);
    }
    Ok(EmptyPickResolution::Stop)
}

fn verify_pick_flags_not_with_operation(args: &Args, operation: &str) {
    let mut incompatible: Option<&'static str> = None;
    if args.no_commit {
        incompatible = Some("--no-commit");
    } else if args.signoff {
        incompatible = Some("--signoff");
    } else if args.mainline.is_some() {
        incompatible = Some("-m");
    } else if args.strategy.is_some() {
        incompatible = Some("--strategy");
    } else if !args.strategy_option.is_empty() {
        incompatible = Some("--strategy-option");
    } else if args.append_source {
        incompatible = Some("-x");
    } else if args.ff {
        incompatible = Some("--ff");
    } else if args.rerere_autoupdate {
        incompatible = Some("--rerere-autoupdate");
    } else if args.no_rerere_autoupdate {
        incompatible = Some("--no-rerere-autoupdate");
    } else if args.keep_redundant_commits {
        incompatible = Some("--keep-redundant-commits");
    } else if args.empty.is_some() {
        incompatible = Some("--empty");
    } else if args.allow_empty {
        incompatible = Some("--allow-empty");
    } else if args.edit {
        incompatible = Some("--edit");
    }
    if let Some(flag) = incompatible {
        eprintln!("fatal: cherry-pick: {flag} cannot be used with {operation}");
        std::process::exit(128);
    }
}

use super::merge::{
    bail_if_resolve_index_not_clean_vs_head, cleanup_message, merge_touched_paths,
    refresh_index_stat_cache_from_worktree, staged_dirty_paths_vs_head,
};
use super::sequencer::{
    append_merge_msg_conflict_footer, rollback_is_safe, sequencer_is_pick_sequence,
    sequencer_is_revert_sequence, strip_first_sequencer_todo_line, unmerged_paths,
    write_abort_safety_file,
};

use grit_lib::porcelain::cherry_pick::{
    directory_renames_from_file_renames, parse_strategy_options, path_has_unmerged_entry,
    remap_path_by_directory_renames, same_blob, same_blob_renames, stage_entry_at,
    WhitespaceStrategyOptions,
};
use grit_lib::porcelain::merge::{tree_to_index_entries, tree_to_map};

/// Result of a three-way merge: the index plus any conflict content for working tree.
struct MergeResult {
    index: Index,
    /// For conflicted paths, the merged content with conflict markers (OID of blob).
    conflict_content: BTreeMap<Vec<u8>, ObjectId>,
}

/// How to handle a cherry-pick whose resulting tree matches `HEAD`.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum EmptyPickResolution {
    /// Create the commit (empty or redundant).
    Proceed,
    /// Skip without error (patch already upstream).
    Drop,
    /// Stop with `CHERRY_PICK_HEAD` set (user may `--skip` or `--allow-empty`).
    Stop,
}

/// Arguments for `grit cherry-pick`.
#[derive(Debug, ClapArgs)]
#[command(about = "Apply the changes introduced by existing commits")]
pub struct Args {
    /// Commits to cherry-pick (single commits or A..B ranges).
    #[arg(value_name = "COMMIT")]
    pub commits: Vec<String>,

    /// Append "(cherry picked from commit <sha>)" to the message.
    #[arg(short = 'x')]
    pub append_source: bool,

    /// Apply changes without committing.
    #[arg(short = 'n', long = "no-commit")]
    pub no_commit: bool,

    /// Add Signed-off-by trailer to the message.
    #[arg(short = 's', long = "signoff")]
    pub signoff: bool,

    /// For cherry-picking merge commits, specify which parent (1-based) is mainline.
    #[arg(short = 'm', long = "mainline")]
    pub mainline: Option<usize>,

    /// Continue cherry-pick after resolving conflicts.
    #[arg(long = "continue")]
    pub r#continue: bool,

    /// Abort an in-progress cherry-pick.
    #[arg(long = "abort")]
    pub abort: bool,

    /// Skip the current commit and continue.
    #[arg(long = "skip")]
    pub skip: bool,

    /// Quit the cherry-pick sequence, keeping current changes.
    #[arg(long = "quit")]
    pub quit: bool,

    /// Fast-forward if possible.
    #[arg(long = "ff")]
    pub ff: bool,

    /// Allow empty commits (already-applied content).
    #[arg(long = "allow-empty")]
    pub allow_empty: bool,

    /// Allow recording commits whose message is empty (matches `git cherry-pick`).
    #[arg(long = "allow-empty-message")]
    pub allow_empty_message: bool,

    /// Keep commits that become empty after replaying onto the current branch (deprecated alias for `--empty=keep`).
    #[arg(long = "keep-redundant-commits")]
    pub keep_redundant_commits: bool,

    /// Merge strategy to use (e.g. recursive, ort, resolve).
    #[arg(long = "strategy")]
    pub strategy: Option<String>,

    /// Strategy option (e.g. "theirs", "ours", "patience").
    #[arg(short = 'X', long = "strategy-option")]
    pub strategy_option: Vec<String>,

    /// What to do with empty commits: stop, drop, or keep.
    #[arg(long = "empty", value_name = "ACTION")]
    pub empty: Option<String>,

    /// Open an editor for the commit message.
    #[arg(short = 'e', long = "edit")]
    pub edit: bool,

    /// After a conflict, record conflict preimages / replay recorded resolutions and optionally stage.
    ///
    /// `--rerere-autoupdate` / `--no-rerere-autoupdate` form a last-wins tristate (Git's
    /// `OPT_RERERE_AUTOUPDATE`): they may be repeated and each overrides the other, so the final
    /// occurrence on the command line decides the mode (t3504 "more than once").
    #[arg(
        long = "rerere-autoupdate",
        overrides_with_all = ["rerere_autoupdate", "no_rerere_autoupdate"]
    )]
    pub rerere_autoupdate: bool,

    /// Do not update the index when a recorded rerere resolution is replayed.
    #[arg(
        long = "no-rerere-autoupdate",
        overrides_with_all = ["rerere_autoupdate", "no_rerere_autoupdate"]
    )]
    pub no_rerere_autoupdate: bool,

    /// Unsupported on cherry-pick (revert-only); accepted to print upstream usage.
    #[arg(long = "reference", hide = true)]
    pub reference: bool,

    /// Message cleanup mode (matches `git cherry-pick --cleanup`; used for conflict `MERGE_MSG`).
    #[arg(long = "cleanup", value_name = "MODE", hide = true)]
    pub cleanup: Option<String>,

    /// Read the list of commits to cherry-pick from standard input (one revision per line).
    #[arg(long = "stdin", hide = true)]
    pub stdin: bool,
}

/// Run the `cherry-pick` command.
pub fn run(args: Args) -> Result<()> {
    if args.reference {
        eprintln!("usage: git cherry-pick [--edit] [-n] [-m <parent-number>] [-s] [-x] [--ff]");
        eprintln!("                       [-S[<keyid>]] <commit>...");
        eprintln!("   or: git cherry-pick (--continue | --skip | --abort | --quit)");
        std::process::exit(129);
    }
    // Validate -m value early: 0 is invalid (1-based), exit 129 like git.
    if let Some(m) = args.mainline {
        if m == 0 {
            eprintln!("error: invalid mainline parent number: 0 (must be >= 1)");
            std::process::exit(129);
        }
    }

    if args.abort {
        verify_pick_flags_not_with_operation(&args, "--abort");
        return abort_cherry_pick_or_revert();
    }
    if args.quit {
        verify_pick_flags_not_with_operation(&args, "--quit");
        return do_quit();
    }
    if args.skip {
        return do_skip(args);
    }
    if args.r#continue {
        return do_continue(args);
    }

    let mut args = args;
    // `--stdin`: read revisions to pick from standard input, one per line (git accepts
    // the option as a hidden walker arg; e.g. `git rev-list ... | git cherry-pick --stdin`).
    if args.stdin {
        use std::io::Read;
        let mut input = String::new();
        std::io::stdin()
            .read_to_string(&mut input)
            .context("reading --stdin commit list")?;
        for line in input.lines() {
            let spec = line.trim();
            if !spec.is_empty() {
                args.commits.push(spec.to_string());
            }
        }
    }

    if args.commits.is_empty() {
        if args.stdin {
            // Match git's "empty commit set passed" for `--stdin` with no input.
            eprintln!("error: empty commit set passed");
            eprintln!("fatal: cherry-pick failed");
            std::process::exit(128);
        }
        bail!("nothing to cherry-pick; specify at least one commit");
    }

    if args.keep_redundant_commits || matches!(args.empty.as_deref(), Some("keep")) {
        args.allow_empty = true;
    }
    do_cherry_pick(args)
}

// ── Main cherry-pick flow ───────────────────────────────────────────

fn do_cherry_pick(mut args: Args) -> Result<()> {
    let repo = Repository::discover(None).context("not a git repository")?;
    let git_dir = &repo.git_dir;

    if args.commits.len() == 1 && args.commits[0] == "-" {
        args.commits = vec![resolve_previous_checkout_from_reflog(git_dir, 1)?];
    }

    let commit_oids = expand_commit_specs(&repo, &args.commits)?;

    if commit_oids.is_empty() {
        eprintln!("error: empty commit set passed");
        eprintln!("fatal: cherry-pick failed");
        std::process::exit(128);
    }

    if !args.no_commit && commit_oids.len() > 1 && git_dir.join("CHERRY_PICK_HEAD").exists() {
        eprintln!("error: cherry-pick is already in progress");
        eprintln!("hint: try \"git cherry-pick (--continue | --skip | --abort | --quit)\"");
        eprintln!("fatal: cherry-pick failed");
        std::process::exit(128);
    }

    let seq_todo = git_dir.join("sequencer").join("todo");
    if seq_todo.exists() {
        if commit_oids.len() > 1 {
            if sequencer_is_revert_sequence(git_dir) {
                eprintln!("error: a revert is already in progress");
                eprintln!("hint: try \"git revert (--continue | --abort | --quit)\"");
                eprintln!("fatal: cherry-pick failed");
                std::process::exit(128);
            }
            if sequencer_is_pick_sequence(git_dir) {
                let advise_skip = git_dir.join("CHERRY_PICK_HEAD").exists();
                eprintln!("error: cherry-pick is already in progress");
                if advise_skip {
                    eprintln!(
                        "hint: try \"git cherry-pick (--continue | --skip | --abort | --quit)\""
                    );
                } else {
                    eprintln!("hint: try \"git cherry-pick (--continue | --abort | --quit)\"");
                }
                eprintln!("fatal: cherry-pick failed");
                std::process::exit(128);
            }
            eprintln!("error: a cherry-pick is already in progress");
            eprintln!("hint: use \"grit cherry-pick --continue\" to continue");
            eprintln!("hint: or \"grit cherry-pick --abort\" to abort");
            std::process::exit(1);
        } else if sequencer_is_revert_sequence(git_dir) {
            eprintln!("error: a revert is already in progress");
            eprintln!("hint: try \"git revert (--continue | --abort | --quit)\"");
            eprintln!("fatal: cherry-pick failed");
            std::process::exit(128);
        }
    }

    if commit_oids.len() == 1
        && !args.no_commit
        && seq_todo.exists()
        && git_dir.join("CHERRY_PICK_HEAD").exists()
    {
        let cp_txt = fs::read_to_string(git_dir.join("CHERRY_PICK_HEAD"))?;
        if let Ok(cp_oid) = ObjectId::from_hex(cp_txt.trim()) {
            let new_oid = commit_oids[0];
            if new_oid != cp_oid {
                let cp_obj = repo.odb.read(&cp_oid)?;
                let cp_commit = parse_commit(&cp_obj.data)?;
                let blocks_nested = cp_commit.parents.len() == 1
                    && Some(new_oid) == cp_commit.parents.first().copied();
                if blocks_nested {
                    eprintln!("error: cherry-pick is already in progress");
                    eprintln!(
                        "hint: try \"git cherry-pick (--continue | --skip | --abort | --quit)\""
                    );
                    eprintln!("fatal: cherry-pick failed");
                    std::process::exit(128);
                }
            }
        }
    }

    if commit_oids.len() > 1 && !args.no_commit {
        save_orig_head(&repo)?;
    }

    run_commit_sequence(&repo, &commit_oids, &args, None)
}

/// Run a sequence of cherry-pick commits, saving sequencer state on conflict.
///
/// When `orig_head_override` is set (e.g. resuming after a manual commit mid-sequence),
/// it is used as the stored pre-sequence HEAD for `sequencer/head` and abort safety
/// instead of the current `HEAD`.
fn run_commit_sequence(
    repo: &Repository,
    oids: &[ObjectId],
    args: &Args,
    orig_head_override: Option<ObjectId>,
) -> Result<()> {
    let git_dir = &repo.git_dir;

    let head = resolve_head(git_dir)?;
    let head_file_path = git_dir.join("sequencer").join("head");
    let default_orig = || -> Result<ObjectId> {
        match head.oid() {
            Some(oid) => Ok(*oid),
            None => Ok(ObjectId::zero()),
        }
    };
    let orig_head_oid = if let Some(o) = orig_head_override {
        o
    } else if oids.len() > 1 && !args.no_commit {
        if let Ok(stored) = fs::read_to_string(&head_file_path) {
            if let Ok(parsed) = ObjectId::from_hex(stored.trim()) {
                parsed
            } else {
                default_orig()?
            }
        } else {
            default_orig()?
        }
    } else {
        default_orig()?
    };

    if oids.len() > 1 && !args.no_commit {
        let seq_dir = git_dir.join("sequencer");
        fs::create_dir_all(&seq_dir)?;
        fs::write(
            seq_dir.join("head"),
            format!("{}\n", orig_head_oid.to_hex()),
        )?;
        let mut full_todo = String::new();
        for oid in oids {
            full_todo.push_str(&format!("pick {}\n", oid.to_hex()));
        }
        fs::write(seq_dir.join("todo"), &full_todo)?;
        write_sequencer_opts(git_dir, args)?;
        write_abort_safety_file(git_dir)?;
    }

    // A bare `git cherry-pick -s <commit>` bakes the sign-off into the conflict MERGE_MSG so a
    // plain `git commit` finishing the resolution carries it (t3507 "failed cherry-pick does not
    // forget -s"). For a multi-pick sequence the sign-off is a persisted opt replayed per pick, so
    // the conflict MERGE_MSG is left without it (t3510 #46/#47 keep the manually resolved pick
    // unsigned). A continuation replay (`orig_head_override`) is likewise not a fresh single pick.
    let single_pick_signoff = oids.len() == 1 && orig_head_override.is_none() && !args.no_commit;

    for (i, commit_oid) in oids.iter().enumerate() {
        // When the sequence stops on `oids[i]` (conflict / empty / hook failure), git
        // keeps the *current* (failing) pick at the head of `sequencer/todo` so that a
        // later `--continue` / `--skip` operates on it. Include `oids[i]` in `remaining`.
        let remaining = &oids[i..];
        match cherry_pick_one_commit(repo, *commit_oid, args, single_pick_signoff) {
            Ok(()) => {
                if oids.len() > 1 && !args.no_commit {
                    strip_first_sequencer_todo_line(git_dir)?;
                    write_abort_safety_file(git_dir)?;
                }
            }
            Err(e) => {
                let err_msg = format!("{e}");
                if err_msg.contains("CHERRY_PICK_DIRTY_GENERIC") {
                    eprintln!("fatal: cherry-pick failed");
                    std::process::exit(128);
                }
                if err_msg.contains("CONFLICT_EXIT") {
                    if std::env::var_os("GIT_CHERRY_PICK_HELP").is_some() {
                        std::process::exit(1);
                    }
                    if oids.len() > 1 {
                        save_sequencer_state(git_dir, &orig_head_oid, remaining, args)?;
                        write_abort_safety_file(git_dir)?;
                    } else if oids.len() == 1 && head.oid().is_none() {
                        save_sequencer_state(git_dir, &ObjectId::zero(), &[], args)?;
                        write_abort_safety_file(git_dir)?;
                    }
                    std::process::exit(1);
                }
                if err_msg.contains("HOOK_FAILED") {
                    // The "'prepare-commit-msg' hook failed" line was already printed by the
                    // pick path (sequencer.c emits it once); just abort without re-reporting.
                    if oids.len() > 1 {
                        save_sequencer_state(git_dir, &orig_head_oid, remaining, args)?;
                        write_abort_safety_file(git_dir)?;
                    }
                    std::process::exit(1);
                }
                if err_msg.contains("EMPTY_CHERRY_PICK_STOP") {
                    let user_msg = err_msg
                        .strip_prefix("EMPTY_CHERRY_PICK_STOP: ")
                        .unwrap_or(&err_msg);
                    if oids.len() > 1 {
                        save_sequencer_state(git_dir, &orig_head_oid, remaining, args)?;
                        write_abort_safety_file(git_dir)?;
                    }
                    eprintln!("{user_msg}");
                    std::process::exit(1);
                }
                if oids.len() > 1 {
                    save_sequencer_state(git_dir, &orig_head_oid, remaining, args)?;
                }
                eprintln!("error: {e:#}");
                eprintln!("fatal: cherry-pick failed");
                std::process::exit(128);
            }
        }
    }

    // A continuation/skip replay (`orig_head_override` set) always finishes a sequence,
    // so it must clean up the sequencer state even when only one pick remained. A genuine
    // standalone `cherry-pick <single commit>` run while a sequence is in progress
    // (`orig_head_override` == None, head file present) must instead leave the state alone.
    let is_continuation = orig_head_override.is_some();
    let nested_single_in_sequence =
        !is_continuation && oids.len() == 1 && !args.no_commit && head_file_path.exists();
    if nested_single_in_sequence {
        // "git cherry-pick <single commit>" in the middle of a sequence is a plain
        // single_pick (sequencer.c:5565-5571): it commits on top of HEAD, clears
        // CHERRY_PICK_HEAD, and DOES NOT touch the sequencer state (todo/head/opts).
        // Leaving the todo intact lets a later `--continue` advance past the original
        // (still-listed) conflicting pick and replay the remaining commits correctly.
    } else {
        cleanup_sequencer_state(git_dir);
    }
    Ok(())
}

#[allow(dead_code)]
fn remove_pick_oid_from_sequencer_todo_if_present(git_dir: &Path, oid: ObjectId) -> Result<()> {
    let path = git_dir.join("sequencer").join("todo");
    let Ok(content) = fs::read_to_string(&path) else {
        return Ok(());
    };
    let mut out = Vec::new();
    let mut removed = false;
    for line in content.lines() {
        if !removed && parse_todo_pick_line(line) == Some(oid) {
            removed = true;
            continue;
        }
        out.push(line);
    }
    if removed {
        let new_content = if out.is_empty() {
            String::new()
        } else {
            out.join("\n") + "\n"
        };
        fs::write(path, new_content)?;
    }
    Ok(())
}

fn parse_checkout_moving_message(message: &str) -> Option<(String, String)> {
    let rest = message.strip_prefix("checkout: moving from ")?;
    let idx = rest.rfind(" to ")?;
    let from = rest[..idx].to_string();
    let to = rest[idx + 4..].to_string();
    Some((from, to))
}

/// Resolve the Nth previous branch/commit checked out from, using `logs/HEAD` (like `git cherry-pick -`).
fn resolve_previous_checkout_from_reflog(git_dir: &Path, n: usize) -> Result<String> {
    let entries = read_reflog(git_dir, "HEAD").context("read HEAD reflog")?;
    let mut count = 0usize;
    for entry in entries.iter().rev() {
        if let Some((from, _to)) = parse_checkout_moving_message(&entry.message) {
            count += 1;
            if count == n {
                return Ok(from);
            }
        }
    }
    bail!("bad revision '-'");
}

/// Expand commit specs, handling A..B ranges.
fn expand_commit_specs(repo: &Repository, specs: &[String]) -> Result<Vec<ObjectId>> {
    let max_count = cherry_pick_rev_max_count();
    let author = cherry_pick_rev_author();
    let use_rev_walk = max_count.is_some() || author.is_some();

    let mut oids = Vec::new();
    for spec in specs {
        if let Some((lhs, rhs)) = spec.split_once("..") {
            let exclude_oid =
                resolve_revision(repo, lhs).with_context(|| format!("bad revision '{lhs}'"))?;
            let include_oid =
                resolve_revision(repo, rhs).with_context(|| format!("bad revision '{rhs}'"))?;

            let range_oids = walk_commit_range(repo, exclude_oid, include_oid)?;
            oids.extend(range_oids);
        } else if use_rev_walk {
            let tip =
                resolve_revision(repo, spec).with_context(|| format!("bad revision '{spec}'"))?;
            let chain = walk_first_parent_filtered(repo, tip, max_count, author.as_deref())?;
            oids.extend(chain);
        } else {
            let oid =
                resolve_revision(repo, spec).with_context(|| format!("bad revision '{spec}'"))?;
            // Validate up front that each spec names a commit. `git cherry-pick` checks every
            // pending object before applying any pick (sequencer.c sequencer_pick_revisions),
            // so a non-commit (e.g. `two:` -> tree) fails the whole command with nothing
            // applied (t3508 "cherry-pick three one two: fails").
            ensure_commit_or_fail(repo, oid, spec)?;
            oids.push(oid);
        }
    }
    Ok(oids)
}

/// Fail like `git cherry-pick` if `oid` is not (or does not peel to) a commit.
///
/// Prints `error: <spec>: can't cherry-pick a <type>` and exits 128, matching
/// sequencer.c's pre-flight object-type validation. Tags that peel to a commit pass.
fn ensure_commit_or_fail(repo: &Repository, oid: ObjectId, spec: &str) -> Result<()> {
    let obj = repo
        .odb
        .read(&oid)
        .with_context(|| format!("bad revision '{spec}'"))?;
    let kind = match obj.kind {
        ObjectKind::Commit => return Ok(()),
        ObjectKind::Tag => {
            // Peel the tag; if it ultimately points at a commit, it's pickable.
            if tag_peels_to_commit(repo, &obj.data) {
                return Ok(());
            }
            "tag"
        }
        other => other.as_str(),
    };
    eprintln!("error: {spec}: can't cherry-pick a {kind}");
    eprintln!("fatal: cherry-pick failed");
    std::process::exit(128);
}

/// Whether an annotated tag object (recursively) targets a commit.
fn tag_peels_to_commit(repo: &Repository, tag_data: &[u8]) -> bool {
    let mut data = tag_data.to_vec();
    for _ in 0..16 {
        let Some(line) = data
            .split(|&b| b == b'\n')
            .find_map(|l| l.strip_prefix(b"object "))
        else {
            return false;
        };
        let Ok(target) = ObjectId::from_hex(&String::from_utf8_lossy(line)) else {
            return false;
        };
        let Ok(obj) = repo.odb.read(&target) else {
            return false;
        };
        match obj.kind {
            ObjectKind::Commit => return true,
            ObjectKind::Tag => data = obj.data,
            _ => return false,
        }
    }
    false
}

fn walk_first_parent_filtered(
    repo: &Repository,
    tip: ObjectId,
    max_count: Option<usize>,
    author_sub: Option<&str>,
) -> Result<Vec<ObjectId>> {
    let mut matches = Vec::new();
    let mut current = Some(tip);
    while let Some(c) = current {
        let obj = repo.odb.read(&c)?;
        let commit = parse_commit(&obj.data)?;
        let author_ok = author_sub.is_none_or(|sub| commit.author.contains(sub));
        if author_ok {
            matches.push(c);
            if let Some(limit) = max_count {
                if matches.len() >= limit {
                    break;
                }
            }
        }
        current = commit.parents.first().copied();
    }
    matches.reverse();
    Ok(matches)
}

/// Commits reachable from `tip` but not from `base`, in topological order (oldest first).
///
/// Matches Git's `A..B` for cherry-pick: `git rev-list --reverse A..B` selects every commit
/// reachable from `B` that is *not* reachable from `A`. Unlike a naive first-parent walk (which
/// would never stop when `A` is off the first-parent chain, e.g. `A` is a merge commit whose
/// second parent introduced shared history), this walks ALL parents and excludes the entire
/// ancestor closure of `A`, then emits the survivors oldest-first (git applies picks oldest-first).
fn walk_commit_range(repo: &Repository, base: ObjectId, tip: ObjectId) -> Result<Vec<ObjectId>> {
    // Everything reachable from `base` (inclusive) is excluded.
    let excluded = grit_lib::merge_base::ancestor_closure(repo, base)?;

    // Collect commits reachable from `tip` that are not in `excluded`, recording each commit's
    // generation depth (max distance to any selected root) so we can emit them oldest-first.
    let mut selected: HashSet<ObjectId> = HashSet::new();
    let mut order: Vec<ObjectId> = Vec::new();
    let mut stack = vec![tip];
    let mut parents_of: HashMap<ObjectId, Vec<ObjectId>> = HashMap::new();
    while let Some(current) = stack.pop() {
        if excluded.contains(&current) || !selected.insert(current) {
            continue;
        }
        order.push(current);
        let obj = repo.odb.read(&current)?;
        let commit = parse_commit(&obj.data)?;
        let parents: Vec<ObjectId> = commit
            .parents
            .iter()
            .copied()
            .filter(|p| !excluded.contains(p))
            .collect();
        for &p in &parents {
            stack.push(p);
        }
        parents_of.insert(current, parents);
    }

    // Topological sort (Kahn): a commit may be applied only after all its selected ancestors.
    // Emit oldest-first. Ties broken by commit date (newer applied later) to match git's
    // rev-list ordering closely enough for cherry-pick replay.
    let mut indegree: HashMap<ObjectId, usize> = HashMap::new();
    let mut children: HashMap<ObjectId, Vec<ObjectId>> = HashMap::new();
    for oid in &order {
        indegree.entry(*oid).or_insert(0);
        for &p in parents_of.get(oid).map(|v| v.as_slice()).unwrap_or(&[]) {
            *indegree.entry(*oid).or_insert(0) += 1;
            children.entry(p).or_default().push(*oid);
        }
    }
    // Ready set: commits whose selected parents are all already emitted (start with roots).
    let mut ready: Vec<ObjectId> = order
        .iter()
        .copied()
        .filter(|o| indegree.get(o).copied().unwrap_or(0) == 0)
        .collect();
    let mut result = Vec::with_capacity(order.len());
    let mut emitted: HashSet<ObjectId> = HashSet::new();
    while !ready.is_empty() {
        // Pick the ready commit with the oldest commit date so the apply order is oldest-first.
        ready.sort_by_key(|o| commit_date_for(repo, *o).unwrap_or(i64::MAX));
        let next = ready.remove(0);
        if !emitted.insert(next) {
            continue;
        }
        result.push(next);
        if let Some(kids) = children.get(&next) {
            for &k in kids {
                if let Some(e) = indegree.get_mut(&k) {
                    *e -= 1;
                    if *e == 0 {
                        ready.push(k);
                    }
                }
            }
        }
    }
    Ok(result)
}

/// Committer date (seconds since epoch) for ordering selected commits oldest-first.
fn commit_date_for(repo: &Repository, oid: ObjectId) -> Option<i64> {
    let obj = repo.odb.read(&oid).ok()?;
    let commit = parse_commit(&obj.data).ok()?;
    let committer = &commit.committer;
    // `Name <email> <secs> <tz>`: the timestamp is the second-to-last whitespace token.
    let toks: Vec<&str> = committer.split_whitespace().collect();
    if toks.len() >= 2 {
        toks[toks.len() - 2].parse::<i64>().ok()
    } else {
        None
    }
}

fn cherry_pick_one_commit(
    repo: &Repository,
    commit_oid: ObjectId,
    args: &Args,
    single_pick_signoff: bool,
) -> Result<()> {
    let git_dir = &repo.git_dir;

    let commit_obj = repo.odb.read(&commit_oid)?;
    if commit_obj.kind != ObjectKind::Commit {
        bail!("object {} is not a commit", commit_oid);
    }
    let commit = parse_commit(&commit_obj.data)?;

    let commit_tree_oid = commit.tree;

    let head = resolve_head(git_dir)?;
    let head_oid_opt = head.oid().map(|o| o.to_owned());

    // Cherry-pick onto an unborn branch records the picked content as the branch's first
    // (root) commit, mirroring `git cherry-pick <commit>` on an orphan/unborn HEAD: the
    // three-way merge below uses the empty tree as both base ("ours") and HEAD tree, and
    // `create_cherry_pick_commit` writes a parentless commit, updating the unborn branch ref
    // (t3501 "cherry-pick on unborn branch"). `git cherry-pick -n` still only stages the index.

    // Check for fast-forward possibility with --ff
    if args.ff {
        let ff_parent = if let Some(m) = args.mainline {
            if m == 0 || m > commit.parents.len() {
                bail!("commit {} does not have parent {}", commit_oid, m);
            }
            Some(commit.parents[m - 1])
        } else if commit.parents.len() > 1 {
            // Merge commit without -m: fall through to normal error handling
            bail!(
                "commit {} is a merge but no -m option was given",
                commit_oid
            );
        } else {
            commit.parents.first().copied()
        };

        let can_ff = match (&head_oid_opt, ff_parent) {
            // Unborn branch: always fast-forward
            (None, _) => true,
            // Normal: parent matches HEAD
            (Some(head_oid), Some(parent)) => parent == *head_oid,
            // Root commit with existing HEAD: cannot ff
            _ => false,
        };

        if can_ff {
            update_head(git_dir, &head, &commit_oid)?;
            let entries = tree_to_index_entries(repo, &commit_tree_oid, "")?;
            let old_index = load_index(repo)?;
            let mut new_index = Index::new();
            new_index.entries = entries;
            new_index.sort();
            repo.write_index(&mut new_index).context("writing index")?;
            if let Some(wt) = &repo.work_tree {
                checkout_merged_index(repo, wt, &old_index, &new_index, &BTreeMap::new())?;
            }

            let short = &commit_oid.to_hex()[..7];
            let branch = branch_name(&head);
            let first_line = commit.message.lines().next().unwrap_or("");
            eprintln!("[{branch} {short}] {first_line}");
            return Ok(());
        }
    }

    // Determine parent (base for the change).
    let parent_oid = if let Some(m) = args.mainline {
        if m == 0 || m > commit.parents.len() {
            bail!("commit {} does not have parent {}", commit_oid, m);
        }
        commit.parents[m - 1]
    } else if commit.parents.len() > 1 {
        bail!(
            "commit {} is a merge but no -m option was given",
            commit_oid
        );
    } else if commit.parents.is_empty() {
        // Root commit: use empty tree as base (sentinel, handled below)
        ObjectId::zero()
    } else {
        commit.parents[0]
    };

    // Read parent tree (base), commit tree (theirs), HEAD tree (ours).
    let parent_tree_oid = if commit.parents.is_empty() {
        // Root commit: base is empty tree
        repo.odb.write(ObjectKind::Tree, &[])?
    } else {
        let parent_obj = repo.odb.read(&parent_oid)?;
        let parent_commit = parse_commit(&parent_obj.data)?;
        parent_commit.tree
    };

    let head_tree_oid = if let Some(head_oid) = head_oid_opt {
        let head_obj = repo.odb.read(&head_oid)?;
        let head_commit = parse_commit(&head_obj.data)?;
        head_commit.tree
    } else {
        repo.odb.write(ObjectKind::Tree, &[])?
    };

    // Three-way merge
    // For --no-commit mode, use current index tree as "ours" when it has content.
    let ours_tree_oid = if args.no_commit {
        let cur_index = load_index(repo)?;
        let stage0: Vec<IndexEntry> = cur_index
            .entries
            .into_iter()
            .filter(|e| e.stage() == 0)
            .collect();
        if !stage0.is_empty() {
            let mut tmp = Index::new();
            tmp.entries = stage0;
            tmp.sort();
            write_tree_from_index(&repo.odb, &tmp, "")?
        } else {
            head_tree_oid
        }
    } else {
        head_tree_oid
    };

    let config = ConfigSet::load(Some(git_dir), true)?;

    if let (Some(head_oid), Some(wt)) = (head_oid_opt.as_ref(), repo.work_tree.as_deref()) {
        error_if_cherry_pick_would_clobber_worktree(
            repo,
            git_dir,
            *head_oid,
            parent_tree_oid,
            ours_tree_oid,
            commit_tree_oid,
            wt,
            args.no_commit,
        )?;
    }

    if let Some(head_oid) = head_oid_opt.as_ref() {
        if !args.no_commit
            && args
                .strategy
                .as_deref()
                .is_some_and(|s| s.eq_ignore_ascii_case("resolve"))
        {
            bail_if_resolve_index_not_clean_vs_head(repo, *head_oid, false)?;
        }
    }

    let (favor, ws_opts, mut diff_algorithm) = parse_strategy_options(&args.strategy_option);
    if diff_algorithm.is_none() {
        diff_algorithm = config.get("diff.algorithm");
    }
    let ws_merge = WhitespaceMergeOptions {
        ignore_all_space: ws_opts.ignore_all_space,
        ignore_space_change: ws_opts.ignore_space_change,
        ignore_space_at_eol: ws_opts.ignore_space_at_eol,
        ignore_cr_at_eol: ws_opts.ignore_cr_at_eol,
    };
    let short_oid = &commit_oid.to_hex()[..7];
    let subject = commit.message.lines().next().unwrap_or("");
    let label_theirs = format!("{short_oid} ({subject})");
    let label_base = format!("parent of {short_oid} ({subject})");

    let conflict_style = match config.get("merge.conflictstyle").as_deref() {
        Some("diff3") | Some("zdiff3") => ConflictStyle::Diff3,
        _ => ConflictStyle::Merge,
    };

    let base_entries = tree_to_map(tree_to_index_entries(repo, &parent_tree_oid, "")?);
    let ours_entries = tree_to_map(tree_to_index_entries(repo, &ours_tree_oid, "")?);
    let theirs_entries = tree_to_map(tree_to_index_entries(repo, &commit_tree_oid, "")?);

    if let Some(wt) = repo.work_tree.as_deref() {
        bail_if_df_merge_would_remove_cwd(wt, &base_entries, &ours_entries, &theirs_entries)?;
    }

    // The `resolve` strategy (git-merge-resolve) announces "Trying simple merge." on stdout
    // before each three-way merge (t3508 "output during multi-pick indicates merge strategy").
    if args
        .strategy
        .as_deref()
        .is_some_and(|s| s.eq_ignore_ascii_case("resolve"))
    {
        println!("Trying simple merge.");
    }

    let merged = merge_trees_three_way(
        repo,
        parent_tree_oid,
        ours_tree_oid,
        commit_tree_oid,
        favor,
        ws_merge,
        diff_algorithm.as_deref(),
        TreeMergeConflictPresentation {
            label_ours: "HEAD",
            label_theirs: TheirsConflictLabel::Fixed(label_theirs.as_str()),
            label_base: label_base.as_str(),
            style: conflict_style,
            checkout_merge: false,
        },
    )?;
    let mut merge_result = MergeResult {
        index: merged.index,
        conflict_content: merged.conflict_content,
    };

    apply_transitive_file_location_conflicts(
        &base_entries,
        &ours_entries,
        &theirs_entries,
        &mut merge_result.index,
        label_theirs.as_str(),
    );
    apply_sparse_checkout_skip_worktree(
        &repo.git_dir,
        repo.work_tree.as_deref(),
        &mut merge_result.index,
        false,
    );
    clear_skip_worktree_for_changed_entries(&mut merge_result.index, &ours_entries);

    let has_conflicts = merge_result.index.entries.iter().any(|e| e.stage() != 0);

    // Index matches HEAD: either drop, record an empty commit, or stop (Git's `allow_empty()`).
    if !has_conflicts {
        let new_tree_oid = write_tree_from_index(&repo.odb, &merge_result.index, "")?;
        if new_tree_oid == head_tree_oid {
            let originally_empty = is_original_commit_empty(repo, &commit)?;
            match resolve_empty_pick_resolution(originally_empty, args)? {
                EmptyPickResolution::Drop => {
                    let subject = commit.message.lines().next().unwrap_or("");
                    eprintln!(
                        "dropping {} {} -- patch contents already upstream",
                        commit_oid.to_hex(),
                        subject
                    );
                    return Ok(());
                }
                EmptyPickResolution::Proceed => { /* commit below */ }
                EmptyPickResolution::Stop => {
                    let config = ConfigSet::load(Some(git_dir), true)?;
                    let (cname, cemail) = committer_name_email(&config);
                    let msg = finalize_cherry_pick_message(
                        &cherry_pick_source_message(&commit),
                        args.append_source,
                        false,
                        &cname,
                        &cemail,
                        &config,
                        &commit_oid.to_hex(),
                    );
                    fs::write(
                        git_dir.join("CHERRY_PICK_HEAD"),
                        format!("{}\n", commit_oid.to_hex()),
                    )?;
                    fs::write(git_dir.join("MERGE_MSG"), &msg)?;
                    bail!("EMPTY_CHERRY_PICK_STOP: The previous cherry-pick is now empty, possibly due to conflict resolution.\nIf you wish to commit it anyway, use --allow-empty.\nhint: try \"git cherry-pick --skip\"");
                }
            }
        }
    }

    let old_index = load_index(repo)?;
    if let Some(wt) = &repo.work_tree {
        super::reset::check_untracked_cherry_pick_obstruction(wt, &old_index, &merge_result.index)?;
    }
    let work_tree = repo
        .work_tree
        .as_deref()
        .ok_or_else(|| anyhow::anyhow!("cannot cherry-pick in a bare repository"))?;
    preflight_cherry_pick_cwd_obstruction(
        repo,
        work_tree,
        &merge_result.index,
        &merge_result.conflict_content,
        None,
    )?;
    repo.write_index(&mut merge_result.index)
        .context("writing index")?;

    checkout_merged_index(
        repo,
        work_tree,
        &old_index,
        &merge_result.index,
        &merge_result.conflict_content,
    )?;

    // The merged index was built from tree entries with an empty stat cache. After writing the
    // working tree, refresh the stat cache for the resolved (stage-0) paths and persist the index
    // so `git diff-files` sees them as clean (matches `git merge`, which refreshes the index after
    // a conflicted checkout). Conflict stages (1/2/3) are left untouched. This also gives `rerere`
    // (invoked below on the conflict path) an index whose clean paths match the working tree, so a
    // replayed/auto-staged resolution leaves `diff-files` clean (t3504 `--rerere-autoupdate`).
    refresh_index_stat_cache_from_worktree(repo, &mut merge_result.index)?;
    repo.write_index(&mut merge_result.index)
        .context("writing index")?;

    // Build the cherry-pick message (Git: `sequencer.c` + `commit.cleanup` when `-x`).
    let (cname, cemail) = committer_name_email(&config);
    let mut msg = finalize_cherry_pick_message(
        &cherry_pick_source_message(&commit),
        args.append_source,
        false,
        &cname,
        &cemail,
        &config,
        &commit_oid.to_hex(),
    );
    // Note: signoff is NOT added to MERGE_MSG here.  When there is a conflict,
    // the user may manually `git commit` to resolve it, which reads MERGE_MSG.
    // Signoff should only be added by `cherry-pick --continue` (which re-reads
    // the opts from the sequencer), not by a manual commit that the user makes
    // without explicitly requesting signoff.
    if has_conflicts {
        let cherry_pick_help = std::env::var("GIT_CHERRY_PICK_HELP").ok();
        let short_oid = &commit_oid.to_hex()[..7];
        let subject = commit.message.lines().next().unwrap_or("");

        if let Some(ref help) = cherry_pick_help {
            eprintln!("error: could not apply {short_oid}... {subject}");
            eprintln!("hint: {help}");
            bail!("CONFLICT_EXIT");
        }

        fs::write(
            git_dir.join("CHERRY_PICK_HEAD"),
            format!("{}\n", commit_oid.to_hex()),
        )?;

        let commit_cleanup = config.get("commit.cleanup");
        let cleanup_mode = args
            .cleanup
            .as_deref()
            .or(commit_cleanup.as_deref())
            .unwrap_or("default");
        let is_scissors = cleanup_mode.eq_ignore_ascii_case("scissors");
        let mut merge_msg = cleanup_message(&msg, cleanup_mode);
        // A bare `cherry-pick -s <commit>` bakes the sign-off into MERGE_MSG (before the
        // `# Conflicts:` block) so a manual `git commit` finishing the resolution carries it
        // (t3507 "failed cherry-pick does not forget -s"). Sequence picks leave it out
        // (t3510 #46/#47) and `--continue` re-affirms it separately. Skip when the picked
        // message already ends with that exact sign-off (t3507 "does not add duplicated -s").
        if single_pick_signoff && args.signoff {
            let sob = format_signoff_line(&cname, &cemail);
            if merge_msg.trim_end().lines().last().map(str::trim_end) != Some(sob.trim_end()) {
                append_signoff_trailer(&mut merge_msg, &sob, &config);
            }
        }
        // git's append_conflicts_hint always appends the `# Conflicts:` comment block on a
        // conflicted pick; the scissors cut-line is added only for cleanup=scissors
        // (t3507 "ensure commit.cleanup = scissors ..." and the default-cleanup MERGE_MSG).
        let paths = unmerged_paths(&merge_result.index);
        append_merge_msg_conflict_footer(&mut merge_msg, &paths, is_scissors);
        // Do NOT bake `--signoff` into MERGE_MSG. The conflict-resolution commit
        // (the interrupted pick completed by `--continue`, or a manual `git commit`)
        // should NOT carry an automatic Signed-off-by: the user must re-affirm `-s`
        // on the continue/commit command line (t3510 #46/#47/#48). Note `-x`
        // (record-origin) IS already present via finalize_cherry_pick_message and is
        // intentionally kept (t3510 #45). The signoff is only re-applied to picks
        // that `--continue` replays *fresh* (done inside cherry_pick_one_commit).
        fs::write(git_dir.join("MERGE_MSG"), &merge_msg)?;

        eprintln!("error: could not apply {short_oid}... {subject}");
        if merge_conflict_advice_enabled(git_dir) {
            if args.no_commit {
                eprintln!("hint: after resolving the conflicts, mark the corrected paths");
                eprintln!("hint: with 'git add <paths>' or 'git rm <paths>'");
            } else {
                eprintln!("hint: After resolving the conflicts, mark them with");
                eprintln!("hint: \"git add/rm <pathspec>\", then run");
                eprintln!("hint: \"git cherry-pick --continue\".");
                eprintln!(
                    "hint: You can instead skip this commit with \"git cherry-pick --skip\"."
                );
                eprintln!("hint: To abort and get back to the state before \"git cherry-pick\",");
                eprintln!("hint: run \"git cherry-pick --abort\".");
            }
            eprintln!(
                "hint: Disable this message with \"git config set advice.mergeConflict false\""
            );
        }

        // Record conflict preimages / replay recorded resolutions, mirroring
        // sequencer.c:do_pick_commit -> repo_rerere(r, opts->allow_rerere_auto) on the
        // conflict path. The autoupdate mode follows the command line flags, falling back
        // to rerere.autoUpdate config (RerereAutoupdate::FromConfig).
        let rr = if args.no_rerere_autoupdate {
            grit_lib::rerere::RerereAutoupdate::No
        } else if args.rerere_autoupdate {
            grit_lib::rerere::RerereAutoupdate::Yes
        } else {
            grit_lib::rerere::RerereAutoupdate::FromConfig
        };
        let _ = grit_lib::rerere::repo_rerere(repo, rr);

        bail!("CONFLICT_EXIT");
    }

    if args.no_commit {
        return Ok(());
    }

    // Add signoff for the non-conflict case (the conflict case skips signoff in
    // MERGE_MSG so that manual `git commit` does not unexpectedly add it).
    if args.signoff {
        let sob = format_signoff_line(&cname, &cemail);
        append_signoff_trailer(&mut msg, &sob, &config);
    }

    if args.edit {
        // `cherry-pick -e`: upstream's sequencer (`should_edit()` true → `msg_file = NULL`)
        // delegates to `git commit -e` via `run_git_commit`. That `git commit` sees the
        // leftover `MERGE_MSG`/`CHERRY_PICK_HEAD` state, so the prepare-commit-msg hook runs
        // with `arg1 = "merge"` and the editor is launched. Mirror that by recording the
        // pick state and spawning `grit commit -n -e` on the staged index.
        fs::write(git_dir.join("MERGE_MSG"), &msg)?;
        fs::write(
            git_dir.join("CHERRY_PICK_HEAD"),
            format!("{}\n", commit_oid.to_hex()),
        )?;
        return finish_cherry_pick_via_commit(repo);
    }

    // Run prepare-commit-msg on the clean (non-conflict, non-amend) pick, mirroring
    // sequencer.c:run_prepare_commit_msg_hook (arg1 = "message", GIT_EDITOR=:). The hook may
    // rewrite the message; read it back before creating the commit.
    let msg = run_cherry_pick_prepare_commit_msg_hook(repo, msg)?;

    // Create the cherry-pick commit (preserving original author).
    create_cherry_pick_commit(repo, &head, &merge_result.index, &msg, &commit, commit_oid)?;

    let new_head = resolve_head(git_dir)?;
    let new_oid = new_head
        .oid()
        .ok_or_else(|| anyhow::anyhow!("HEAD has no OID"))?;
    let first_line = msg.lines().next().unwrap_or("");
    // Summarise on the pre-commit HEAD state so an unborn-branch pick reports `(root-commit)`.
    print_commit_summary(repo, &head, *new_oid, first_line);

    Ok(())
}

/// Finalise a clean `cherry-pick -e` by delegating to `grit commit -n -e`, mirroring
/// `sequencer.c:run_git_commit` (which spawns `git commit` for the editor path).
///
/// The staged index plus `MERGE_MSG`/`CHERRY_PICK_HEAD` have already been written, so the
/// spawned `commit` reads the pick message, runs `prepare-commit-msg` (arg1 = "merge", editor
/// used), launches the editor, and records the commit — preserving the picked author via
/// `CHERRY_PICK_HEAD`. The environment (including `GIT_EDITOR`) is inherited unchanged.
fn finish_cherry_pick_via_commit(repo: &Repository) -> Result<()> {
    let self_exe = std::env::current_exe().context("cannot determine grit binary path")?;
    let mut cmd = std::process::Command::new(&self_exe);
    // `-n` (no pre-commit/commit-msg), `-e` (editor), `--allow-empty` mirror the
    // `EDIT_MSG | ALLOW_EMPTY` flags upstream passes for a clean pick.
    cmd.args(["commit", "-n", "-e", "--allow-empty", "--no-gpg-sign"]);
    cmd.current_dir(repo.work_tree.as_deref().unwrap_or(&repo.git_dir));
    let status = cmd.status().context("run grit commit for cherry-pick -e")?;
    if !status.success() {
        bail!("HOOK_FAILED");
    }
    Ok(())
}

fn branch_name(head: &HeadState) -> &str {
    match head {
        HeadState::Branch { short_name, .. } => short_name.as_str(),
        HeadState::Detached { .. } => "HEAD detached",
        HeadState::Invalid => "unknown",
    }
}

/// Remove a single trailing sign-off line (`sob`, a full `Signed-off-by: ...` line possibly with a
/// trailing newline) and any blank lines its removal leaves behind.
fn strip_trailing_signoff_line(msg: &mut String, sob: &str) {
    let target = sob.trim_end();
    let mut lines: Vec<&str> = msg.lines().collect();
    // Find the last occurrence of the exact sign-off line and drop it.
    if let Some(pos) = lines.iter().rposition(|l| l.trim_end() == target) {
        lines.remove(pos);
        // Trim trailing blank lines created by the removal.
        while lines.last().is_some_and(|l| l.trim().is_empty()) {
            lines.pop();
        }
        *msg = if lines.is_empty() {
            String::new()
        } else {
            format!("{}\n", lines.join("\n"))
        };
    }
}

/// Render an ident string (`Name <email> <ts> <tz>`) as `Name <email>`.
fn ident_name_email(ident: &str) -> String {
    match (ident.find('<'), ident.find('>')) {
        (Some(lt), Some(gt)) if gt > lt => {
            let name = ident[..lt].trim_end();
            let email = &ident[lt + 1..gt];
            format!("{name} <{email}>")
        }
        _ => ident.to_string(),
    }
}

/// Format the author date of an ident as git's `DATE_NORMAL` (`Thu Apr 7 15:14:13 2005 -0700`),
/// using an un-padded day like git's `"%.3s %d "` (date.c).
fn format_normal_date(ident: &str) -> String {
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
        let sign = if tz_bytes[0] == b'-' { -1i64 } else { 1i64 };
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
        "{} {} {} {:02}:{:02}:{:02} {} {}",
        weekday,
        month,
        dt.day(),
        dt.hour(),
        dt.minute(),
        dt.second(),
        dt.year(),
        offset_str
    )
}

/// Print git's `print_commit_summary` for a freshly created pick/revert commit, to stdout.
///
/// Mirrors sequencer.c:print_commit_summary — always `[<branch>[ (root-commit)] <short>] <subject>`,
/// then ` Author: <ident>` only when the author ident differs from the committer ident, then
/// ` Date: <author-date>`, then a shortstat line. The cherry-pick/revert summary is emitted to
/// stdout (t3508 "output to keep user entertained during multi-pick").
fn print_commit_summary(repo: &Repository, head: &HeadState, new_oid: ObjectId, subject: &str) {
    let branch = branch_name(head);
    let short = &new_oid.to_hex()[..7];
    let root = head.is_unborn();
    if root {
        println!("[{branch} (root-commit) {short}] {subject}");
    } else {
        println!("[{branch} {short}] {subject}");
    }

    let Ok(obj) = repo.odb.read(&new_oid) else {
        return;
    };
    let Ok(commit) = parse_commit(&obj.data) else {
        return;
    };

    let author_ne = ident_name_email(&commit.author);
    let committer_ne = ident_name_email(&commit.committer);
    if author_ne != committer_ne {
        println!(" Author: {author_ne}");
    }
    let date = format_normal_date(&commit.author);
    if !date.is_empty() {
        println!(" Date: {date}");
    }

    // Shortstat against the first parent (or empty tree for a root commit).
    let parent_tree = if commit.parents.is_empty() {
        None
    } else if let Ok(po) = repo.odb.read(&commit.parents[0]) {
        parse_commit(&po.data).ok().map(|c| c.tree)
    } else {
        None
    };
    if let Ok(diff_entries) =
        grit_lib::diff::diff_trees(&repo.odb, parent_tree.as_ref(), Some(&commit.tree), "")
    {
        let zero_oid = ObjectId::zero();
        let mut total_files = 0usize;
        let mut total_ins = 0usize;
        let mut total_del = 0usize;
        for entry in &diff_entries {
            total_files += 1;
            let read_text = |oid: &ObjectId| -> String {
                if *oid == zero_oid {
                    String::new()
                } else {
                    repo.odb
                        .read(oid)
                        .map(|o| String::from_utf8_lossy(&o.data).into_owned())
                        .unwrap_or_default()
                }
            };
            let (a, d) = grit_lib::diff::count_changes(
                &read_text(&entry.old_oid),
                &read_text(&entry.new_oid),
            );
            total_ins += a;
            total_del += d;
        }
        if total_files > 0 {
            let mut summary = format!(
                " {} file{} changed",
                total_files,
                if total_files == 1 { "" } else { "s" }
            );
            if total_ins > 0 {
                summary.push_str(&format!(
                    ", {} insertion{}(+)",
                    total_ins,
                    if total_ins == 1 { "" } else { "s" }
                ));
            }
            if total_del > 0 {
                summary.push_str(&format!(
                    ", {} deletion{}(-)",
                    total_del,
                    if total_del == 1 { "" } else { "s" }
                ));
            }
            println!("{summary}");
        }
    }
}

fn merge_conflict_advice_enabled(git_dir: &Path) -> bool {
    let Ok(config) = ConfigSet::load(Some(git_dir), true) else {
        return true;
    };
    config.get_bool("advice.mergeConflict") != Some(Ok(false))
}

/// Refuse cherry-pick when local changes overlap the merge, matching Git's messages.
#[allow(clippy::too_many_arguments)]
fn error_if_cherry_pick_would_clobber_worktree(
    repo: &Repository,
    _git_dir: &Path,
    head_oid: ObjectId,
    parent_tree: ObjectId,
    head_tree: ObjectId,
    picked_tree: ObjectId,
    work_tree: &Path,
    no_commit: bool,
) -> Result<()> {
    let touched = merge_touched_paths(repo, parent_tree, head_tree, picked_tree)?;
    // A local worktree edit only blocks the pick when the *incoming* commit actually
    // rewrites that path. Paths that differ only between the parent and HEAD (e.g. a
    // local rename the pick does not touch) are kept as "ours" by the three-way merge,
    // so the worktree file is left untouched and must not abort the pick (t3501
    // "cherry-pick works with dirty renamed file"). Restrict the overwrite check to the
    // paths the picked commit changes relative to its parent.
    let theirs_touched: BTreeSet<String> =
        grit_lib::diff::diff_trees(&repo.odb, Some(&parent_tree), Some(&picked_tree), "")?
            .iter()
            .map(|e| e.path().to_string())
            .collect();
    let touched: BTreeSet<String> = touched.intersection(&theirs_touched).cloned().collect();
    // In `--no-commit` mode the index legitimately accumulates earlier picks of the same
    // sequence (`ours` is the evolving index tree, not HEAD). git's overwrite protection
    // there is `unpack_trees` against the index, so only genuine *unstaged* worktree edits
    // count — staged-vs-HEAD deltas are expected (t3508 "cherry-pick -n first..fourth").
    let staged_dirty = if no_commit {
        BTreeSet::new()
    } else {
        staged_dirty_paths_vs_head(repo, head_oid)?
    };

    if !staged_dirty.is_empty() {
        let overlap: BTreeSet<String> = staged_dirty.intersection(&touched).cloned().collect();
        if !overlap.is_empty() {
            let mut msg = String::from(
                "Your local changes to the following files would be overwritten by merge:\n",
            );
            for path in overlap {
                msg.push_str(&format!("\t{path}\n"));
            }
            msg.push_str("Please commit your changes or stash them before you merge.\nAborting");
            bail!("{msg}");
        }
        eprintln!("error: your local changes would be overwritten by cherry-pick.");
        eprintln!("hint: commit your changes or stash them to proceed.");
        bail!("CHERRY_PICK_DIRTY_GENERIC");
    }

    let index = repo.load_index()?;
    let unstaged = diff_index_to_worktree(&repo.odb, &index, work_tree, false, false)?;
    let unstaged_paths: BTreeSet<String> = unstaged.iter().map(|e| e.path().to_string()).collect();
    let overlap_u: BTreeSet<String> = unstaged_paths.intersection(&touched).cloned().collect();
    if !overlap_u.is_empty() {
        let mut msg = String::from(
            "Your local changes to the following files would be overwritten by merge:\n",
        );
        for path in overlap_u {
            msg.push_str(&format!("\t{path}\n"));
        }
        msg.push_str("Please commit your changes or stash them before you merge.\nAborting");
        bail!("{msg}");
    }

    Ok(())
}

// ── --continue ──────────────────────────────────────────────────────

fn do_continue(mut args: Args) -> Result<()> {
    let repo = Repository::discover(None).context("not a git repository")?;
    let git_dir = &repo.git_dir;

    if git_dir.join("REVERT_HEAD").exists()
        && !git_dir.join("CHERRY_PICK_HEAD").exists()
        && (!git_dir.join("sequencer").join("todo").exists()
            || super::sequencer::sequencer_is_revert_sequence(git_dir))
    {
        return super::revert::do_continue();
    }

    // Compatibility check uses command-line flags only (git's `verify_opt_compatible`
    // runs before `read_populate_opts`); merge persisted sequencer/opts afterwards.
    verify_pick_flags_not_with_operation(&args, "--continue");
    // Remember whether `-s`/`--signoff` was given on *this* command line, before the
    // persisted opts are merged in. The conflict-resolution commit only gets a
    // Signed-off-by when the user re-affirms `-s` on the `--continue` line itself.
    let cli_signoff = args.signoff;
    merge_sequencer_opts(git_dir, &mut args);
    let args = &args;

    let has_cherry_pick_head = git_dir.join("CHERRY_PICK_HEAD").exists();
    let sequencer_todo = git_dir.join("sequencer").join("todo");
    let sequencer_todo_exists = sequencer_todo.exists();

    if !has_cherry_pick_head && !sequencer_todo_exists {
        eprintln!("error: no cherry-pick or revert in progress");
        std::process::exit(128);
    }

    if sequencer_todo_exists {
        validate_sequencer_todo_pick_only(git_dir)?;
    }

    if !has_cherry_pick_head && sequencer_todo_exists {
        // CHERRY_PICK_HEAD is gone: the user resolved the conflict and committed it
        // manually (or the conflicting pick was already committed). Git's
        // `sequencer_continue` requires the index to match HEAD here, then advances
        // past the just-committed pick (`todo_list.current++`) and runs the rest.
        let index = load_index(&repo)?;
        if index.entries.iter().any(|e| e.stage() != 0) {
            eprintln!(
                "error: commit is not possible because you have unmerged files\n\
                 hint: fix conflicts and then commit the result with 'git cherry-pick --continue'"
            );
            std::process::exit(128);
        }
        let head = resolve_head(git_dir)?;
        let head_tree_oid = if let Some(h) = head.oid() {
            let ho = repo.odb.read(h)?;
            parse_commit(&ho.data)?.tree
        } else {
            repo.odb.write(ObjectKind::Tree, &[])?
        };
        let new_tree_oid = write_tree_from_index(&repo.odb, &index, "")?;
        if new_tree_oid != head_tree_oid {
            eprintln!(
                "error: your local changes would be overwritten by cherry-pick.\n\
                 hint: commit your changes or stash them to proceed.\n\
                 fatal: cherry-pick failed"
            );
            std::process::exit(128);
        }

        let head_file = git_dir.join("sequencer").join("head");
        let stored_orig = if let Ok(s) = fs::read_to_string(&head_file) {
            ObjectId::from_hex(s.trim()).ok()
        } else {
            None
        };
        // Drop the already-committed pick from the head of the todo (`current++`).
        strip_first_sequencer_todo_line(git_dir)?;
        let remaining = load_sequencer_todo(git_dir);
        cleanup_sequencer_state(git_dir);
        if !remaining.is_empty() {
            run_commit_sequence(&repo, &remaining, args, stored_orig)?;
        }
        return Ok(());
    }

    let index = load_index(&repo)?;
    if index.entries.iter().any(|e| e.stage() != 0) {
        eprintln!(
            "error: commit is not possible because you have unmerged files\n\
             hint: fix conflicts and then commit the result with 'git cherry-pick --continue'"
        );
        std::process::exit(128);
    }

    let cp_head_content = fs::read_to_string(git_dir.join("CHERRY_PICK_HEAD"))?;
    let cp_oid = ObjectId::from_hex(cp_head_content.trim())?;
    let cp_obj = repo.odb.read(&cp_oid)?;
    let cp_commit = parse_commit(&cp_obj.data)?;

    let config = ConfigSet::load(Some(git_dir), true)?;
    let (cname, cemail) = committer_name_email(&config);

    let mut msg = match fs::read_to_string(git_dir.join("MERGE_MSG")) {
        Ok(m) => m,
        Err(_) => finalize_cherry_pick_message(
            &cherry_pick_source_message(&cp_commit),
            args.append_source,
            false,
            &cname,
            &cemail,
            &config,
            &cp_oid.to_hex(),
        ),
    };

    // A single-pick `cherry-pick -s <commit>` baked the committer sign-off into MERGE_MSG (so a
    // manual `git commit` keeps it, t3507). But `--continue` must re-affirm `-s` on its own
    // command line (t3510 #48), so drop a trailing committer sign-off that the original commit
    // message did not carry unless `-s` was given on this `--continue`.
    if !cli_signoff {
        let sob = format_signoff_line(&cname, &cemail);
        let original_has_sob = cherry_pick_source_message(&cp_commit).contains(sob.trim_end());
        if !original_has_sob {
            strip_trailing_signoff_line(&mut msg, &sob);
        }
    }

    if args.append_source {
        let trailer = format!("(cherry picked from commit {})", cp_oid.to_hex());
        if !msg.contains(&trailer) {
            append_cherry_picked_from_line(&mut msg, &cp_oid.to_hex(), &config);
        }
    }

    // Only sign off the conflict-resolution commit when `-s` was re-affirmed on the
    // `--continue` command line; the persisted sequence signoff must NOT auto-apply
    // here (t3510 #47/#48). Fresh picks replayed afterwards still get the persisted -s.
    if cli_signoff {
        let sob = format_signoff_line(&cname, &cemail);
        append_signoff_trailer(&mut msg, &sob, &config);
    }

    let head = resolve_head(git_dir)?;
    let head_tree_oid = if let Some(h) = head.oid() {
        let ho = repo.odb.read(h)?;
        parse_commit(&ho.data)?.tree
    } else {
        repo.odb.write(ObjectKind::Tree, &[])?
    };

    let new_tree_oid = write_tree_from_index(&repo.odb, &index, "")?;
    if !args.allow_empty && new_tree_oid == head_tree_oid {
        eprintln!("The previous cherry-pick is now empty, possibly due to conflict resolution.");
        eprintln!("If you wish to commit it anyway, use --allow-empty.");
        eprintln!("hint: try \"git cherry-pick --skip\"");
        std::process::exit(1);
    }

    create_cherry_pick_commit(&repo, &head, &index, &msg, &cp_commit, cp_oid)?;

    let new_head = resolve_head(git_dir)?;
    let new_oid = new_head
        .oid()
        .ok_or_else(|| anyhow::anyhow!("HEAD has no OID"))?;
    let first_line = msg.lines().next().unwrap_or("");
    print_commit_summary(&repo, &head, *new_oid, first_line);

    cleanup_cherry_pick_state(git_dir);

    // The head of sequencer/todo is the pick we just committed (fix: the conflicting
    // commit is kept at the head on a stop). Advance past it (`todo_list.current++`)
    // and replay only the *remaining* picks.
    let stored_orig = {
        let head_file = git_dir.join("sequencer").join("head");
        fs::read_to_string(&head_file)
            .ok()
            .and_then(|s| ObjectId::from_hex(s.trim()).ok())
    };
    let full_todo = load_sequencer_todo(git_dir);
    if !full_todo.is_empty() {
        strip_first_sequencer_todo_line(git_dir)?;
    }
    let remaining = load_sequencer_todo(git_dir);
    if !remaining.is_empty() {
        write_abort_safety_file(git_dir)?;
        run_commit_sequence(&repo, &remaining, args, stored_orig)?;
    } else {
        cleanup_sequencer_state(git_dir);
    }

    Ok(())
}

/// After a manual `git commit` finished the current pick, resume any remaining `sequencer/todo`
/// picks. NOTE: git does NOT auto-resume on a plain commit (only `--continue` advances the
/// sequence), so this is currently unused; kept for reference/potential future use.
#[allow(dead_code)]
pub(crate) fn try_resume_pick_sequence_after_commit(repo: &Repository) -> Result<()> {
    let git_dir = &repo.git_dir;
    if !git_dir.join("sequencer").join("todo").exists() {
        return Ok(());
    }
    if sequencer_is_revert_sequence(git_dir) {
        return Ok(());
    }
    if git_dir.join("CHERRY_PICK_HEAD").exists() {
        return Ok(());
    }

    let mut args = Args {
        commits: vec![],
        append_source: false,
        no_commit: false,
        signoff: false,
        mainline: None,
        r#continue: true,
        abort: false,
        skip: false,
        quit: false,
        ff: false,
        allow_empty: false,
        allow_empty_message: false,
        keep_redundant_commits: false,
        strategy: None,
        strategy_option: vec![],
        empty: None,
        edit: false,
        rerere_autoupdate: false,
        no_rerere_autoupdate: false,
        reference: false,
        cleanup: None,
        stdin: false,
    };
    merge_sequencer_opts(git_dir, &mut args);
    if args.keep_redundant_commits || matches!(args.empty.as_deref(), Some("keep")) {
        args.allow_empty = true;
    }
    validate_sequencer_todo_pick_only(git_dir)?;

    let head_file = git_dir.join("sequencer").join("head");
    let stored_orig = if let Ok(s) = fs::read_to_string(&head_file) {
        ObjectId::from_hex(s.trim()).ok()
    } else {
        None
    };
    let remaining = load_sequencer_todo(git_dir);
    if !remaining.is_empty() {
        run_commit_sequence(repo, &remaining, &args, stored_orig)?;
    } else {
        cleanup_sequencer_state(git_dir);
    }
    Ok(())
}

// ── --abort ─────────────────────────────────────────────────────────

fn null_oid() -> ObjectId {
    ObjectId::zero()
}

pub(crate) fn abort_cherry_pick_or_revert() -> Result<()> {
    let repo = Repository::discover(None).context("not a git repository")?;
    let git_dir = &repo.git_dir;

    let head_file = git_dir.join("sequencer").join("head");
    let has_seq = head_file.exists();
    let has_cp = git_dir.join("CHERRY_PICK_HEAD").exists();
    let has_rv = git_dir.join("REVERT_HEAD").exists();

    if !has_seq && !has_cp && !has_rv {
        eprintln!("error: no cherry-pick or revert in progress");
        std::process::exit(128);
    }

    if has_seq {
        let stored = fs::read_to_string(&head_file)?;
        let stored_oid = ObjectId::from_hex(stored.trim())?;
        if stored_oid == null_oid() {
            eprintln!("error: cannot abort from a branch yet to be born");
            eprintln!("fatal: cherry-pick failed");
            std::process::exit(128);
        }
        if !rollback_is_safe(git_dir) {
            eprintln!("warning: You seem to have moved HEAD. Not rewinding, check your HEAD!");
            cleanup_cherry_pick_state(git_dir);
            let _ = fs::remove_file(git_dir.join("REVERT_HEAD"));
            cleanup_sequencer_state(git_dir);
            let _ = fs::remove_file(git_dir.join("ORIG_HEAD"));
            return Ok(());
        }
        super::reset::run(super::reset::Args {
            soft: false,
            mixed: false,
            hard: false,
            keep: false,
            merge: true,
            quiet: true,
            intent_to_add: false,
            no_refresh: false,
            refresh: true,
            patch: false,
            unified: None,
            inter_hunk_context: None,
            no_auto_advance: false,
            pathspec_from_file: None,
            pathspec_file_nul: false,
            recurse_submodules: None,
            no_recurse_submodules: false,
            rest: vec![stored_oid.to_hex()],
            skip_sequencer_head_cleanup: true,
            raw_argv_had_path_separator: false,
        })?;
        cleanup_cherry_pick_state(git_dir);
        cleanup_sequencer_state(git_dir);
        let _ = fs::remove_file(git_dir.join("ORIG_HEAD"));
        return Ok(());
    }

    super::reset::run(super::reset::Args {
        soft: false,
        mixed: false,
        hard: false,
        keep: false,
        merge: true,
        quiet: true,
        intent_to_add: false,
        no_refresh: false,
        refresh: true,
        patch: false,
        unified: None,
        inter_hunk_context: None,
        no_auto_advance: false,
        pathspec_from_file: None,
        pathspec_file_nul: false,
        recurse_submodules: None,
        no_recurse_submodules: false,
        rest: vec!["HEAD".to_owned()],
        skip_sequencer_head_cleanup: true,
        raw_argv_had_path_separator: false,
    })?;
    cleanup_cherry_pick_state(git_dir);
    let _ = fs::remove_file(git_dir.join("REVERT_HEAD"));
    cleanup_sequencer_state(git_dir);
    let _ = fs::remove_file(git_dir.join("ORIG_HEAD"));
    Ok(())
}

// ── --skip ──────────────────────────────────────────────────────────

fn reset_to_head_tree(repo: &Repository, git_dir: &Path) -> Result<()> {
    let head = resolve_head(git_dir)?;
    let old_index = load_index(repo)?;
    let mut new_index = Index::new();
    if let Some(head_oid) = head.oid() {
        let obj = repo.odb.read(head_oid)?;
        let commit = parse_commit(&obj.data)?;
        new_index.entries = tree_to_index_entries(repo, &commit.tree, "")?;
    }
    new_index.sort();
    repo.write_index(&mut new_index)?;
    if let Some(wt) = &repo.work_tree {
        checkout_merged_index(repo, wt, &old_index, &new_index, &BTreeMap::new())?;
    }
    Ok(())
}

fn do_skip(mut args: Args) -> Result<()> {
    let repo = Repository::discover(None).context("not a git repository")?;
    let git_dir = &repo.git_dir;

    // git's `verify_opt_compatible` (builtin/revert.c) runs against the command-line
    // options ONLY, *before* `read_populate_opts` loads the persisted sequencer/opts.
    // Run the compat check on the raw argv flags first, then merge persisted opts so
    // flags carried forward from the original pick don't spuriously trip the check.
    verify_pick_flags_not_with_operation(&args, "--skip");
    merge_sequencer_opts(git_dir, &mut args);
    let args = &args;

    if git_dir.join("REVERT_HEAD").exists() {
        eprintln!("error: no cherry-pick in progress");
        std::process::exit(1);
    }

    let has_cp = git_dir.join("CHERRY_PICK_HEAD").exists();
    let seq_pick = sequencer_is_pick_sequence(git_dir);

    if has_cp {
        reset_to_head_tree(&repo, git_dir)?;
        cleanup_cherry_pick_state(git_dir);
        skip_current_pick_and_continue(&repo, args)?;
        return Ok(());
    }

    if seq_pick {
        if !rollback_is_safe(git_dir) {
            eprintln!("error: there is nothing to skip");
            eprintln!("hint: have you committed already?");
            eprintln!("hint: try \"git cherry-pick --continue\"");
            eprintln!("fatal: cherry-pick failed");
            std::process::exit(128);
        }
        reset_to_head_tree(&repo, git_dir)?;
        cleanup_cherry_pick_state(git_dir);
        skip_current_pick_and_continue(&repo, args)?;
        return Ok(());
    }

    eprintln!("error: no cherry-pick in progress");
    std::process::exit(1);
}

/// Drop the current pick (the head of `sequencer/todo`) and replay the remaining picks,
/// matching git's `--skip` (`todo_list.current++` then `pick_commits`). Preserves the
/// stored pre-sequence HEAD so abort-safety stays valid across the resumed sequence.
fn skip_current_pick_and_continue(repo: &Repository, args: &Args) -> Result<()> {
    let git_dir = &repo.git_dir;
    let stored_orig = {
        let head_file = git_dir.join("sequencer").join("head");
        fs::read_to_string(&head_file)
            .ok()
            .and_then(|s| ObjectId::from_hex(s.trim()).ok())
    };
    if !load_sequencer_todo(git_dir).is_empty() {
        strip_first_sequencer_todo_line(git_dir)?;
    }
    let remaining = load_sequencer_todo(git_dir);
    if !remaining.is_empty() {
        write_abort_safety_file(git_dir)?;
        run_commit_sequence(repo, &remaining, args, stored_orig)?;
    } else {
        cleanup_sequencer_state(git_dir);
    }
    Ok(())
}

// ── --quit ──────────────────────────────────────────────────────────

fn do_quit() -> Result<()> {
    let repo = Repository::discover(None).context("not a git repository")?;
    let git_dir = &repo.git_dir;

    let in_progress = git_dir.join("CHERRY_PICK_HEAD").exists()
        || git_dir.join("sequencer").join("todo").exists();
    if !in_progress {
        return Ok(());
    }

    cleanup_cherry_pick_state(git_dir);
    cleanup_sequencer_state(git_dir);
    Ok(())
}

// ── Sequencer state management ──────────────────────────────────────

fn save_orig_head(repo: &Repository) -> Result<()> {
    let git_dir = &repo.git_dir;
    let head = resolve_head(git_dir)?;
    if let Some(oid) = head.oid() {
        fs::write(git_dir.join("ORIG_HEAD"), format!("{}\n", oid.to_hex()))?;
    }
    Ok(())
}

fn write_sequencer_opts(git_dir: &Path, args: &Args) -> Result<()> {
    let seq_dir = git_dir.join("sequencer");
    fs::create_dir_all(&seq_dir)?;
    let mut opts = String::from("[options]\n");
    if args.signoff {
        opts.push_str("\tsignoff = true\n");
    }
    if args.append_source {
        opts.push_str("\trecord-origin = true\n");
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
    // Persist the rerere autoupdate choice so `--continue` of a multi-commit sequence
    // replays it (sequencer.c writes `options.allow-rerere-auto` only when set).
    if args.rerere_autoupdate {
        opts.push_str("\tallow-rerere-auto = true\n");
    } else if args.no_rerere_autoupdate {
        opts.push_str("\tallow-rerere-auto = false\n");
    }
    if args.edit {
        opts.push_str("\tedit = true\n");
    }
    if args.allow_empty {
        opts.push_str("\tallow-empty = true\n");
    }
    if args.allow_empty_message {
        opts.push_str("\tallow-empty-message = true\n");
    }
    if args.keep_redundant_commits {
        opts.push_str("\tkeep-redundant-commits = true\n");
    }
    if let Some(ref empty) = args.empty {
        opts.push_str(&format!("\tempty = {empty}\n"));
    }
    if let Some(ref c) = args.cleanup {
        opts.push_str(&format!("\tcleanup = {c}\n"));
    }
    fs::write(seq_dir.join("opts"), &opts)?;
    Ok(())
}

fn save_sequencer_state(
    git_dir: &Path,
    head_oid: &ObjectId,
    remaining: &[ObjectId],
    args: &Args,
) -> Result<()> {
    let seq_dir = git_dir.join("sequencer");
    fs::create_dir_all(&seq_dir)?;

    fs::write(seq_dir.join("head"), format!("{}\n", head_oid.to_hex()))?;

    let mut todo = String::new();
    for oid in remaining {
        todo.push_str(&format!("pick {}\n", oid.to_hex()));
    }
    fs::write(seq_dir.join("todo"), &todo)?;

    write_sequencer_opts(git_dir, args)?;

    Ok(())
}

/// Load the sequencer opts and merge them into the provided args.
/// This allows `--continue` to re-apply flags from the original cherry-pick.
fn merge_sequencer_opts(git_dir: &Path, args: &mut Args) {
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
            let key = key.strip_prefix("options.").unwrap_or(key);
            let val = v.trim();
            match key {
                "signoff" if val == "true" => args.signoff = true,
                "append_source" if val == "true" => args.append_source = true,
                "record-origin" if val == "true" => args.append_source = true,
                "no_commit" if val == "true" => args.no_commit = true,
                "edit" if val == "true" => args.edit = true,
                "allow-empty" if val == "true" => args.allow_empty = true,
                "allow-empty-message" if val == "true" => args.allow_empty_message = true,
                "keep-redundant-commits" if val == "true" => {
                    args.keep_redundant_commits = true;
                    args.allow_empty = true;
                }
                "mainline" => {
                    if let Ok(m) = val.parse::<usize>() {
                        args.mainline = Some(m);
                    }
                }
                "strategy" => args.strategy = Some(val.to_string()),
                "strategy-option" => args.strategy_option.push(val.to_string()),
                "allow-rerere-auto" => {
                    if val == "true" {
                        args.rerere_autoupdate = true;
                    } else if val == "false" {
                        args.no_rerere_autoupdate = true;
                    }
                }
                "empty" => args.empty = Some(val.to_string()),
                "cleanup" => args.cleanup = Some(val.to_string()),
                _ => {}
            }
        }
    }
}

fn parse_todo_pick_line(line: &str) -> Option<ObjectId> {
    let t = line.trim();
    if t.is_empty() || t.starts_with('#') {
        return None;
    }
    let after_cmd = t.strip_prefix("pick")?;
    if after_cmd.is_empty() || !after_cmd.starts_with(|c: char| c.is_whitespace()) {
        return None;
    }
    let after_pick = after_cmd.trim_start();
    let token = after_pick.split_whitespace().next()?;
    if !(4..=40).contains(&token.len()) || !token.chars().all(|c| c.is_ascii_hexdigit()) {
        return None;
    }
    ObjectId::from_hex(token).ok()
}

fn load_sequencer_todo(git_dir: &Path) -> Vec<ObjectId> {
    let todo_path = git_dir.join("sequencer").join("todo");
    match fs::read_to_string(&todo_path) {
        Ok(content) => {
            let mut oids = Vec::new();
            for line in content.lines() {
                if let Some(oid) = parse_todo_pick_line(line) {
                    oids.push(oid);
                }
            }
            oids
        }
        Err(_) => Vec::new(),
    }
}

fn validate_sequencer_todo_pick_only(git_dir: &Path) -> Result<()> {
    let todo_path = git_dir.join("sequencer").join("todo");
    let content = fs::read_to_string(&todo_path)?;
    for line in content.lines() {
        let t = line.trim();
        if t.is_empty() || t.starts_with('#') {
            continue;
        }
        if parse_todo_pick_line(line).is_none() {
            eprintln!("error: invalid todo line in sequencer: {t}");
            std::process::exit(128);
        }
    }
    Ok(())
}

fn cleanup_sequencer_state(git_dir: &Path) {
    let seq_dir = git_dir.join("sequencer");
    let _ = fs::remove_dir_all(&seq_dir);
}

// ── Helpers ─────────────────────────────────────────────────────────

fn cleanup_cherry_pick_state(git_dir: &Path) {
    let _ = fs::remove_file(git_dir.join("CHERRY_PICK_HEAD"));
    let _ = fs::remove_file(git_dir.join("REVERT_HEAD"));
    let _ = fs::remove_file(git_dir.join("MERGE_MSG"));
}

fn load_index(repo: &Repository) -> Result<Index> {
    Ok(repo.load_index()?)
}

/// If `committer` would serialize to the same commit object as `source_oid`, advance the
/// committer timestamp by one second so the replayed commit gets a distinct OID (t3510).
#[allow(clippy::too_many_arguments)]
fn bump_committer_if_replay_matches_source(
    source_oid: ObjectId,
    tree_oid: ObjectId,
    parents: &[ObjectId],
    author: &str,
    committer: &mut String,
    encoding: &Option<String>,
    stored_msg: &str,
    raw_message: &Option<Vec<u8>>,
) -> Result<()> {
    let (author_raw, committer_raw) =
        grit_lib::commit_encoding::identity_raw_for_serialized_commit(encoding, author, committer);
    let trial = CommitData {
        tree: tree_oid,
        parents: parents.to_vec(),
        author: author.to_owned(),
        committer: committer.clone(),
        author_raw,
        committer_raw,
        encoding: encoding.clone(),
        message: stored_msg.to_owned(),
        raw_message: raw_message.clone(),
    };
    let bytes = serialize_commit(&trial);
    if Odb::hash_object_data(ObjectKind::Commit, &bytes) != source_oid {
        return Ok(());
    }
    *committer = bump_committer_ident_unix_seconds(committer)?;
    Ok(())
}

/// Increment the Unix seconds field in a Git author/committer identity line, preserving the
/// timezone suffix.
fn bump_committer_ident_unix_seconds(ident: &str) -> Result<String> {
    let Some(parsed) = parse_signature_times(ident) else {
        return Ok(ident.to_owned());
    };
    let bytes = ident.as_bytes();
    let gt = ident
        .rfind('>')
        .with_context(|| format!("malformed identity line (missing '>'): {ident}"))?;
    let mut i = gt + 1;
    while i < bytes.len() && matches!(bytes[i], b' ' | b'\t') {
        i += 1;
    }
    let tz = ident
        .get(parsed.tz_hhmm_range.clone())
        .with_context(|| format!("malformed identity timezone in: {ident}"))?;
    let new_unix = parsed.unix_seconds.saturating_add(1);
    let mut out = String::new();
    out.push_str(&ident[..i]);
    out.push_str(&new_unix.to_string());
    out.push(' ');
    out.push_str(tz);
    Ok(out)
}

/// Run `prepare-commit-msg` for a clean (non-conflict, non-amend) cherry-pick, mirroring
/// `sequencer.c:run_prepare_commit_msg_hook`.
///
/// The message is written to `COMMIT_EDITMSG`, the hook is invoked with `arg1 = "message"`
/// and `GIT_EDITOR=:` (the sequencer always passes `editor_is_used = 0` to `run_commit_hook`
/// for a clean pick), the (possibly hook-modified) message is read back, and the index is
/// re-read in case the hook touched it. A failing hook reports `'prepare-commit-msg' hook
/// failed` (matching `sequencer.c`) and bails so the pick aborts.
fn run_cherry_pick_prepare_commit_msg_hook(repo: &Repository, msg: String) -> Result<String> {
    let editmsg_path = repo.git_dir.join("COMMIT_EDITMSG");
    fs::write(&editmsg_path, msg.as_bytes())?;

    let index_path = repo.index_path();
    let editmsg_str = editmsg_path.to_string_lossy().to_string();
    let hook_env = CommitHookEnv {
        index_file: Some(index_path.as_path()),
        git_editor: Some(":"),
        git_prefix: None,
        extra_env: &[],
    };
    let r = run_commit_hook(
        repo,
        "prepare-commit-msg",
        &[editmsg_str.as_str(), "message"],
        None,
        &hook_env,
    )
    .map_err(|e| anyhow::anyhow!(e))?;
    if let HookResult::Failed(_) = r {
        // Matches sequencer.c: emit the hook-failure line exactly once, then abort the pick
        // without repeating the hook name in the top-level error (t7505 greps for a count of 1).
        eprintln!("error: 'prepare-commit-msg' hook failed");
        bail!("HOOK_FAILED");
    }
    if !r.was_executed() {
        // No hook ran; keep the original message untouched.
        return Ok(msg);
    }

    let edited = fs::read_to_string(&editmsg_path)?;
    Ok(edited)
}

fn create_cherry_pick_commit(
    repo: &Repository,
    head: &HeadState,
    index: &Index,
    message: &str,
    original_commit: &CommitData,
    source_commit_oid: ObjectId,
) -> Result<()> {
    let mut index_for_tree = index.clone();
    index_for_tree.expand_sparse_directory_placeholders(&repo.odb)?;
    let tree_oid = write_tree_from_index(&repo.odb, &index_for_tree, "")?;
    let git_dir = &repo.git_dir;

    let mut parents = Vec::new();
    if let Some(head_oid) = head.oid() {
        parents.push(*head_oid);
    }

    let config = ConfigSet::load(Some(git_dir), true)?;
    let now = time::OffsetDateTime::now_utc();

    let author = original_commit.author.clone();
    let mut committer = resolve_committer_ident(&config, now)?;

    let commit_enc = config
        .get("i18n.commitEncoding")
        .or_else(|| config.get("i18n.commitencoding"));
    let (stored_msg, encoding, raw_message) =
        grit_lib::commit_encoding::finalize_stored_commit_message(
            message.to_owned(),
            commit_enc.as_deref(),
        );

    bump_committer_if_replay_matches_source(
        source_commit_oid,
        tree_oid,
        &parents,
        &author,
        &mut committer,
        &encoding,
        &stored_msg,
        &raw_message,
    )?;

    let (author_raw, committer_raw) = grit_lib::commit_encoding::identity_raw_for_serialized_commit(
        &encoding, &author, &committer,
    );

    let commit_data = CommitData {
        tree: tree_oid,
        parents,
        author,
        committer,
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

    let subject = message.lines().next().unwrap_or("");
    let reflog_msg = format!("cherry-pick: {subject}");
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

    cleanup_cherry_pick_state(git_dir);

    Ok(())
}

fn committer_name_email(config: &ConfigSet) -> (String, String) {
    let mut name = match crate::ident::read_git_identity_name_env("GIT_COMMITTER_NAME") {
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
    let email = std::env::var("GIT_COMMITTER_EMAIL")
        .ok()
        .or_else(|| config.get("user.email"))
        .unwrap_or_default();
    (name, email)
}

fn resolve_committer_ident(config: &ConfigSet, now: time::OffsetDateTime) -> Result<String> {
    let (name, email) = committer_name_email(config);

    let epoch = now.unix_timestamp();
    let offset = now.offset();
    let hours = offset.whole_hours();
    let minutes = offset.minutes_past_hour().unsigned_abs();

    let timestamp = std::env::var("GIT_COMMITTER_DATE")
        .map(|d| grit_lib::commit::parse_date_to_git_timestamp(&d).unwrap_or(d))
        .unwrap_or_else(|_| format!("{epoch} {hours:+03}{minutes:02}"));

    Ok(format!("{name} <{email}> {timestamp}"))
}

fn append_signoff(msg: &str, git_dir: &Path) -> Result<String> {
    let config = ConfigSet::load(Some(git_dir), true)?;
    let mut name = match crate::ident::read_git_identity_name_env("GIT_COMMITTER_NAME") {
        crate::ident::GitIdentityNameEnv::Set(s) => s,
        crate::ident::GitIdentityNameEnv::Unset => {
            if let Some(v) = config.get("user.name") {
                let t = v.trim();
                if !t.is_empty() {
                    t.to_owned()
                } else {
                    crate::ident::ident_default_name(&config)
                }
            } else {
                crate::ident::ident_default_name(&config)
            }
        }
    };
    if name.trim().is_empty() {
        name = "Unknown".to_owned();
    }
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

fn path_has_tree_descendant(map: &HashMap<Vec<u8>, IndexEntry>, path: &[u8]) -> bool {
    map.keys()
        .any(|k| k.len() > path.len() && k.starts_with(path) && k.get(path.len()) == Some(&b'/'))
}

/// Directory/file obstruction relative to merge base: one side introduces a non-tree at `P` while
/// the merge base only had paths under `P/` (no blob at `P`). Matches the cases `merge.rs` handles
/// after rename flattening; `merge_trees_three_way` / rebase do not, so we preflight here
/// (`t2501-cwd-empty`).
pub(crate) fn bail_if_df_merge_would_remove_cwd(
    work_tree: &Path,
    base: &HashMap<Vec<u8>, IndexEntry>,
    ours: &HashMap<Vec<u8>, IndexEntry>,
    theirs: &HashMap<Vec<u8>, IndexEntry>,
) -> Result<()> {
    let mut all_paths = BTreeSet::new();
    all_paths.extend(base.keys().cloned());
    all_paths.extend(ours.keys().cloned());
    all_paths.extend(theirs.keys().cloned());
    for path in all_paths {
        let b = base.get(&path);
        let o = ours.get(&path);
        let t = theirs.get(&path);
        if let Some(te) = t {
            if te.mode != MODE_TREE && o.is_none() && path_has_tree_descendant(base, &path) {
                let anchor = String::from_utf8_lossy(&path);
                if grit_lib::worktree_cwd::cwd_would_be_removed_with_repo_path(
                    work_tree,
                    anchor.as_ref(),
                ) {
                    bail!("Refusing to remove the current working directory:\n{anchor}\n");
                }
            }
        }
        if let Some(oe) = o {
            if oe.mode != MODE_TREE && t.is_none() && path_has_tree_descendant(base, &path) {
                let anchor = String::from_utf8_lossy(&path);
                if grit_lib::worktree_cwd::cwd_would_be_removed_with_repo_path(
                    work_tree,
                    anchor.as_ref(),
                ) {
                    bail!("Refusing to remove the current working directory:\n{anchor}\n");
                }
            }
        }
        if let (Some(oe), Some(te)) = (o, t) {
            if oe.mode == MODE_TREE && te.mode != MODE_TREE {
                let anchor = String::from_utf8_lossy(&path);
                if grit_lib::worktree_cwd::cwd_would_be_removed_with_repo_path(
                    work_tree,
                    anchor.as_ref(),
                ) {
                    bail!("Refusing to remove the current working directory:\n{anchor}\n");
                }
            }
            if oe.mode != MODE_TREE && te.mode == MODE_TREE {
                let anchor = String::from_utf8_lossy(&path);
                if grit_lib::worktree_cwd::cwd_would_be_removed_with_repo_path(
                    work_tree,
                    anchor.as_ref(),
                ) {
                    bail!("Refusing to remove the current working directory:\n{anchor}\n");
                }
            }
        }
        if let (Some(be), Some(oe), Some(te)) = (b, o, t) {
            if be.mode == MODE_TREE && oe.mode == MODE_TREE && te.mode != MODE_TREE {
                let anchor = String::from_utf8_lossy(&path);
                if grit_lib::worktree_cwd::cwd_would_be_removed_with_repo_path(
                    work_tree,
                    anchor.as_ref(),
                ) {
                    bail!("Refusing to remove the current working directory:\n{anchor}\n");
                }
            }
            if be.mode == MODE_TREE && te.mode == MODE_TREE && oe.mode != MODE_TREE {
                let anchor = String::from_utf8_lossy(&path);
                if grit_lib::worktree_cwd::cwd_would_be_removed_with_repo_path(
                    work_tree,
                    anchor.as_ref(),
                ) {
                    bail!("Refusing to remove the current working directory:\n{anchor}\n");
                }
            }
        }
    }
    Ok(())
}

fn clear_skip_worktree_for_changed_entries(
    index: &mut Index,
    ours_entries: &HashMap<Vec<u8>, IndexEntry>,
) {
    for entry in &mut index.entries {
        if entry.stage() != 0 || !entry.skip_worktree() {
            continue;
        }
        let changed = match ours_entries.get(&entry.path) {
            Some(ours) => !same_blob(ours, entry),
            None => true,
        };
        if changed {
            entry.set_skip_worktree(false);
        }
    }
}

fn apply_transitive_file_location_conflicts(
    base: &HashMap<Vec<u8>, IndexEntry>,
    ours: &HashMap<Vec<u8>, IndexEntry>,
    theirs: &HashMap<Vec<u8>, IndexEntry>,
    index: &mut Index,
    theirs_label: &str,
) {
    let ours_dir_renames = directory_renames_from_file_renames(&same_blob_renames(base, ours));
    if ours_dir_renames.is_empty() {
        return;
    }

    for (old_path, new_path) in same_blob_renames(base, theirs) {
        if ours.contains_key(&old_path) || path_has_unmerged_entry(index, &old_path) {
            continue;
        }
        let Some(remapped) = remap_path_by_directory_renames(&new_path, &ours_dir_renames) else {
            continue;
        };
        if remapped != old_path {
            continue;
        }
        let (Some(base_entry), Some(theirs_entry)) = (base.get(&old_path), theirs.get(&new_path))
        else {
            continue;
        };

        index.remove(&old_path);
        stage_entry_at(index, &old_path, base_entry, 1);
        stage_entry_at(index, &old_path, theirs_entry, 3);

        let old_s = String::from_utf8_lossy(&old_path);
        let new_s = String::from_utf8_lossy(&new_path);
        println!(
            "CONFLICT (file location): {old_s} renamed to {new_s} in {theirs_label}, inside a directory that was renamed in HEAD, suggesting it should perhaps be moved to {old_s}."
        );
    }
    index.sort();
}

/// Check if two blobs have the same content modulo a trailing newline.
/// Returns true if the contents are equal after stripping a single trailing `\n`
/// from both sides (or if both are already equal).
fn same_blob_content_modulo_trailing_newline(
    repo: &Repository,
    a: &IndexEntry,
    b: &IndexEntry,
) -> bool {
    if a.mode != b.mode {
        return false;
    }
    if a.oid == b.oid {
        return true;
    }
    let a_data = match repo.odb.read(&a.oid) {
        Ok(obj) => obj.data,
        Err(_) => return false,
    };
    let b_data = match repo.odb.read(&b.oid) {
        Ok(obj) => obj.data,
        Err(_) => return false,
    };
    let a_stripped = a_data.strip_suffix(b"\n").unwrap_or(&a_data);
    let b_stripped = b_data.strip_suffix(b"\n").unwrap_or(&b_data);
    a_stripped == b_stripped
}

fn stage_entry(index: &mut Index, src: &IndexEntry, stage: u8) {
    let mut e = src.clone();
    e.flags = (e.flags & 0x0FFF) | ((stage as u16) << 12);
    index.entries.push(e);
}

/// Three-way merge with content-level merging.
fn three_way_merge_with_content(
    repo: &Repository,
    base: &HashMap<Vec<u8>, IndexEntry>,
    ours: &HashMap<Vec<u8>, IndexEntry>,
    theirs: &HashMap<Vec<u8>, IndexEntry>,
    favor: MergeFavor,
    ws_opts: WhitespaceStrategyOptions,
) -> Result<MergeResult> {
    let mut all_paths = BTreeSet::new();
    all_paths.extend(base.keys().cloned());
    all_paths.extend(ours.keys().cloned());
    all_paths.extend(theirs.keys().cloned());

    let mut out = Index::new();
    let mut conflict_content = BTreeMap::new();

    for path in all_paths {
        let b = base.get(&path);
        let o = ours.get(&path);
        let t = theirs.get(&path);

        match (b, o, t) {
            (_, Some(oe), Some(te)) if same_blob(oe, te) => {
                out.entries.push(oe.clone());
            }
            (Some(be), Some(oe), Some(te)) if same_blob(be, oe) => {
                out.entries.push(te.clone());
            }
            (Some(be), Some(oe), Some(te)) if same_blob(be, te) => {
                out.entries.push(oe.clone());
            }
            // If base and ours differ only in trailing newline (and ours == base
            // content), treat as "base unchanged on our side" and take theirs.
            // This handles the common case where a manual conflict resolution
            // adds/removes a trailing newline without changing content.
            (Some(be), Some(oe), Some(te))
                if !same_blob(be, te)
                    && same_blob_content_modulo_trailing_newline(repo, be, oe) =>
            {
                out.entries.push(te.clone());
            }
            (Some(be), Some(oe), Some(te))
                if be.mode == 0o160000 && oe.mode == 0o160000 && te.mode == 0o160000 =>
            {
                if same_blob(oe, te) {
                    out.entries.push(oe.clone());
                } else if same_blob(be, oe) {
                    out.entries.push(te.clone());
                } else if same_blob(be, te) {
                    out.entries.push(oe.clone());
                } else {
                    stage_entry(&mut out, be, 1);
                    stage_entry(&mut out, oe, 2);
                    stage_entry(&mut out, te, 3);
                }
            }
            (Some(be), Some(oe), Some(te)) => {
                content_merge_or_conflict(
                    repo,
                    &mut out,
                    &mut conflict_content,
                    &path,
                    be,
                    oe,
                    te,
                    favor,
                    ws_opts,
                )?;
            }
            (None, Some(oe), None) => {
                out.entries.push(oe.clone());
            }
            (None, None, Some(te)) => {
                out.entries.push(te.clone());
            }
            (None, Some(oe), Some(te)) if same_blob(oe, te) => {
                out.entries.push(oe.clone());
            }
            (None, Some(oe), Some(te)) => {
                stage_entry(&mut out, oe, 2);
                stage_entry(&mut out, te, 3);
            }
            (Some(_), None, None) => {}
            (Some(be), Some(oe), None) if same_blob(be, oe) => {}
            (Some(be), None, Some(te)) if same_blob(be, te) => {}
            (Some(be), Some(oe), None) => {
                stage_entry(&mut out, be, 1);
                stage_entry(&mut out, oe, 2);
            }
            (Some(be), None, Some(te)) => {
                stage_entry(&mut out, be, 1);
                stage_entry(&mut out, te, 3);
            }
            (None, None, None) => {}
        }
    }

    out.sort();
    Ok(MergeResult {
        index: out,
        conflict_content,
    })
}

#[expect(clippy::too_many_arguments)]
fn content_merge_or_conflict(
    repo: &Repository,
    index: &mut Index,
    conflict_content: &mut BTreeMap<Vec<u8>, ObjectId>,
    path: &[u8],
    base: &IndexEntry,
    ours: &IndexEntry,
    theirs: &IndexEntry,
    favor: MergeFavor,
    ws_opts: WhitespaceStrategyOptions,
) -> Result<()> {
    if base.mode == 0o160000 || ours.mode == 0o160000 || theirs.mode == 0o160000 {
        stage_entry(index, base, 1);
        stage_entry(index, ours, 2);
        stage_entry(index, theirs, 3);
        return Ok(());
    }

    if base.mode == MODE_TREE || ours.mode == MODE_TREE || theirs.mode == MODE_TREE {
        stage_entry(index, base, 1);
        stage_entry(index, ours, 2);
        stage_entry(index, theirs, 3);
        return Ok(());
    }

    let base_obj = repo.odb.read(&base.oid)?;
    let ours_obj = repo.odb.read(&ours.oid)?;
    let theirs_obj = repo.odb.read(&theirs.oid)?;

    if grit_lib::merge_file::is_binary(&base_obj.data)
        || grit_lib::merge_file::is_binary(&ours_obj.data)
        || grit_lib::merge_file::is_binary(&theirs_obj.data)
    {
        // With -Xtheirs or -Xours, resolve binary conflicts by taking one side
        match favor {
            MergeFavor::Theirs => {
                index.entries.push(theirs.clone());
                return Ok(());
            }
            MergeFavor::Ours => {
                index.entries.push(ours.clone());
                return Ok(());
            }
            _ => {
                stage_entry(index, base, 1);
                stage_entry(index, ours, 2);
                stage_entry(index, theirs, 3);
                return Ok(());
            }
        }
    }

    let path_str = String::from_utf8_lossy(path);
    let input = MergeInput {
        base: &base_obj.data,
        ours: &ours_obj.data,
        theirs: &theirs_obj.data,
        label_ours: "HEAD",
        label_base: "parent of picked commit",
        label_theirs: &path_str,
        favor,
        style: Default::default(),
        marker_size: 7,
        diff_algorithm: None,
        ignore_all_space: ws_opts.ignore_all_space,
        ignore_space_change: ws_opts.ignore_space_change,
        ignore_space_at_eol: ws_opts.ignore_space_at_eol,
        ignore_cr_at_eol: ws_opts.ignore_cr_at_eol,
    };

    let result = merge(&input)?;

    if result.conflicts > 0 {
        // Store the conflict-marker content blob for working tree checkout
        let conflict_oid = repo.odb.write(ObjectKind::Blob, &result.content)?;
        conflict_content.insert(path.to_vec(), conflict_oid);

        stage_entry(index, base, 1);
        stage_entry(index, ours, 2);
        stage_entry(index, theirs, 3);
    } else {
        let merged_oid = repo.odb.write(ObjectKind::Blob, &result.content)?;
        let mut entry = ours.clone();
        entry.oid = merged_oid;
        if base.mode == ours.mode && base.mode != theirs.mode {
            entry.mode = theirs.mode;
        }
        index.entries.push(entry);
    }

    Ok(())
}

/// Abort before index write when the cherry-pick/revert/rebase checkout would replace a directory
/// that contains the process cwd with a file (`t2501-cwd-empty`).
pub(crate) fn preflight_cherry_pick_cwd_obstruction(
    repo: &Repository,
    work_tree: &Path,
    index: &Index,
    conflict_content: &BTreeMap<Vec<u8>, ObjectId>,
    rebase_conflict_paths: Option<&[Vec<u8>]>,
) -> Result<()> {
    let old_index = load_index(repo)?;
    let old_stage0: HashMap<Vec<u8>, &IndexEntry> = old_index
        .entries
        .iter()
        .filter(|e| e.stage() == 0)
        .map(|e| (e.path.clone(), e))
        .collect();

    let new_paths: HashSet<Vec<u8>> = index
        .entries
        .iter()
        .filter(|e| e.stage() == 0)
        .map(|e| e.path.clone())
        .collect();
    let new_stage0: Vec<&IndexEntry> = index.entries.iter().filter(|e| e.stage() == 0).collect();

    for entry in &old_index.entries {
        if entry.stage() == 0 && !new_paths.contains(&entry.path) {
            let path_str = String::from_utf8_lossy(&entry.path).into_owned();
            if entry.mode == MODE_GITLINK {
                let mut prefix = entry.path.clone();
                prefix.push(b'/');
                let abs_path = work_tree.join(&path_str);
                if new_stage0
                    .iter()
                    .any(|new_entry| new_entry.path.starts_with(&prefix))
                    && (submodule_dir_has_non_dotgit_content(&abs_path)
                        || abs_path.join(".git").exists())
                {
                    bail!("cannot replace submodule directory {path_str}");
                }
            }
            if grit_lib::worktree_cwd::cwd_would_be_removed_with_repo_path(work_tree, &path_str) {
                bail!("Refusing to remove the current working directory:\n{path_str}\n");
            }
        }
    }

    let mut written = HashSet::new();
    for entry in &index.entries {
        if entry.stage() == 0 && (entry.skip_worktree() || entry.mode == MODE_TREE) {
            continue;
        }
        let path_str = String::from_utf8_lossy(&entry.path).into_owned();
        let abs_path = work_tree.join(&path_str);

        if entry.stage() == 0 {
            if entry.skip_worktree() {
                continue;
            }
            if let Some(prev) = old_stage0.get(&entry.path) {
                if same_blob(prev, entry) {
                    written.insert(entry.path.clone());
                    continue;
                }
                if prev.mode == MODE_GITLINK
                    && entry.mode != MODE_GITLINK
                    && abs_path.is_dir()
                    && (submodule_dir_has_non_dotgit_content(&abs_path)
                        || abs_path.join(".git").exists())
                {
                    bail!("cannot replace submodule directory {path_str}");
                }
            }
            preflight_blob_write_vs_cwd_dir(repo, work_tree, &path_str, &abs_path, entry)?;
            written.insert(entry.path.clone());
        } else if entry.stage() == 2 && !written.contains(&entry.path) {
            if rebase_conflict_paths.is_some_and(|paths| paths.iter().any(|p| p == &entry.path)) {
                if abs_path.is_dir()
                    && grit_lib::worktree_cwd::cwd_would_be_removed_with_repo_path(
                        work_tree, &path_str,
                    )
                {
                    bail!("Refusing to remove the current working directory:\n{path_str}\n");
                }
            } else {
                let effective = if let Some(marker_oid) = conflict_content.get(&entry.path) {
                    let mut marker_entry = entry.clone();
                    marker_entry.oid = *marker_oid;
                    marker_entry
                } else {
                    entry.clone()
                };
                preflight_blob_write_vs_cwd_dir(repo, work_tree, &path_str, &abs_path, &effective)?;
            }
            written.insert(entry.path.clone());
        }
    }
    Ok(())
}

fn preflight_blob_write_vs_cwd_dir(
    repo: &Repository,
    work_tree: &Path,
    path_str: &str,
    abs_path: &Path,
    entry: &IndexEntry,
) -> Result<()> {
    if entry.mode == 0o160000 {
        if abs_path.is_dir()
            && !abs_path.join(".git").exists()
            && grit_lib::worktree_cwd::cwd_would_be_removed_with_repo_path(work_tree, path_str)
        {
            bail!("Refusing to remove the current working directory:\n{path_str}\n");
        }
        return Ok(());
    }
    let obj = repo.odb.read(&entry.oid)?;
    if obj.kind != ObjectKind::Blob {
        return Ok(());
    }
    if abs_path.is_dir()
        && grit_lib::worktree_cwd::cwd_would_be_removed_with_repo_path(work_tree, path_str)
    {
        bail!("Refusing to remove the current working directory:\n{path_str}\n");
    }
    Ok(())
}

fn submodule_dir_has_non_dotgit_content(path: &Path) -> bool {
    let Ok(entries) = fs::read_dir(path) else {
        return false;
    };
    entries.flatten().any(|entry| entry.file_name() != ".git")
}

fn checkout_merged_index(
    repo: &Repository,
    work_tree: &Path,
    old_index: &Index,
    index: &Index,
    conflict_content: &BTreeMap<Vec<u8>, ObjectId>,
) -> Result<()> {
    let old_stage0: HashMap<Vec<u8>, &IndexEntry> = old_index
        .entries
        .iter()
        .filter(|e| e.stage() == 0)
        .map(|e| (e.path.clone(), e))
        .collect();

    let new_paths: HashSet<Vec<u8>> = index
        .entries
        .iter()
        .filter(|e| e.stage() == 0)
        .map(|e| e.path.clone())
        .collect();

    for entry in &old_index.entries {
        if entry.stage() == 0 && !new_paths.contains(&entry.path) {
            if entry.mode == MODE_GITLINK {
                continue;
            }
            let path_str = String::from_utf8_lossy(&entry.path).into_owned();
            if grit_lib::worktree_cwd::cwd_would_be_removed_with_repo_path(work_tree, &path_str) {
                bail!("Refusing to remove the current working directory:\n{path_str}\n");
            }
            let abs_path = work_tree.join(&path_str);
            if abs_path.exists() || abs_path.is_symlink() {
                if abs_path.is_dir() {
                    let _ = fs::remove_dir_all(&abs_path);
                } else {
                    let _ = fs::remove_file(&abs_path);
                }
                remove_empty_parent_dirs(work_tree, &abs_path);
            }
        }
    }
    let mut written = HashSet::new();
    for entry in &index.entries {
        if entry.stage() == 0 && (entry.skip_worktree() || entry.mode == MODE_TREE) {
            continue;
        }
        let path_str = String::from_utf8_lossy(&entry.path).into_owned();
        let abs_path = work_tree.join(&path_str);

        if entry.stage() == 0 {
            if let Some(prev) = old_stage0.get(&entry.path) {
                if same_blob(prev, entry) {
                    // Index OID unchanged — preserve local worktree modifications (t3501).
                    written.insert(entry.path.clone());
                    continue;
                }
            }
            write_entry_to_worktree(repo, work_tree, &path_str, &abs_path, entry)?;
            written.insert(entry.path.clone());
        } else if entry.stage() == 2 && !written.contains(&entry.path) {
            // For conflicts, prefer writing conflict-marker content if available
            if let Some(marker_oid) = conflict_content.get(&entry.path) {
                let mut marker_entry = entry.clone();
                marker_entry.oid = *marker_oid;
                write_entry_to_worktree(repo, work_tree, &path_str, &abs_path, &marker_entry)?;
            } else {
                write_entry_to_worktree(repo, work_tree, &path_str, &abs_path, entry)?;
            }
            written.insert(entry.path.clone());
        }
    }

    Ok(())
}

fn remove_empty_parent_dirs(work_tree: &Path, path: &Path) {
    let cwd_rel = grit_lib::worktree_cwd::process_cwd_repo_relative(work_tree);
    let mut current = path.parent();
    while let Some(dir) = current {
        if dir == work_tree {
            break;
        }
        if let Some(ref cr) = cwd_rel {
            if grit_lib::worktree_cwd::cwd_would_be_removed_with_dir(work_tree, dir, cr) {
                break;
            }
        }
        match fs::remove_dir(dir) {
            Ok(()) => current = dir.parent(),
            Err(_) => break,
        }
    }
}

fn write_entry_to_worktree(
    repo: &Repository,
    work_tree: &Path,
    path_str: &str,
    abs_path: &Path,
    entry: &IndexEntry,
) -> Result<()> {
    if let Some(parent) = abs_path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("creating parent directory for {path_str}"))?;
    }

    if entry.mode == 0o160000 {
        if grit_lib::worktree_cwd::cwd_would_be_removed_with_repo_path(work_tree, path_str) {
            bail!("Refusing to remove the current working directory:\n{path_str}\n");
        }
        if abs_path.is_file() || abs_path.is_symlink() {
            let _ = fs::remove_file(abs_path);
        }
        if !abs_path.is_dir() {
            fs::create_dir_all(abs_path)
                .with_context(|| format!("creating gitlink directory {path_str}"))?;
        }
        return Ok(());
    }

    let obj = repo
        .odb
        .read(&entry.oid)
        .with_context(|| format!("reading object for {path_str}"))?;

    if entry.mode == MODE_SYMLINK {
        let target =
            String::from_utf8(obj.data).map_err(|_| anyhow::anyhow!("symlink not UTF-8"))?;
        if abs_path.exists() || abs_path.is_symlink() {
            if grit_lib::worktree_cwd::cwd_would_be_removed_with_repo_path(work_tree, path_str) {
                bail!("Refusing to remove the current working directory:\n{path_str}\n");
            }
            let _ = fs::remove_file(abs_path);
        }
        std::os::unix::fs::symlink(target, abs_path)
            .with_context(|| format!("creating symlink {path_str}"))?;
    } else {
        if abs_path.is_dir() {
            if grit_lib::worktree_cwd::cwd_would_be_removed_with_repo_path(work_tree, path_str) {
                bail!("Refusing to remove the current working directory:\n{path_str}\n");
            }
            fs::remove_dir_all(abs_path)
                .with_context(|| format!("removing directory before writing {path_str}"))?;
        }
        fs::write(abs_path, &obj.data).with_context(|| format!("writing {path_str}"))?;
        if entry.mode == MODE_EXECUTABLE {
            use std::os::unix::fs::PermissionsExt;
            let mut perms = fs::metadata(abs_path)
                .with_context(|| format!("reading permissions for {path_str}"))?
                .permissions();
            perms.set_mode(0o755);
            fs::set_permissions(abs_path, perms)
                .with_context(|| format!("setting executable permissions on {path_str}"))?;
        }
    }

    Ok(())
}

//! `grit history` — history rewriting (reword, split, etc.).

use crate::commands::commit::{cleanup_edited_commit_message, comment_line_prefix_full};
use crate::commands::replay::replay_commits_onto;
use crate::commands::update_ref::resolve_reflog_identity;
use anyhow::{bail, Context, Result};
use clap::{Args as ClapArgs, Parser, Subcommand};
use grit_lib::config::ConfigSet;
use grit_lib::diff::{diff_trees, DiffEntry, DiffStatus};
use grit_lib::index::{Index, IndexEntry, MODE_GITLINK, MODE_REGULAR};
use grit_lib::merge_base::is_ancestor;
use grit_lib::objects::{
    parse_commit, parse_tag, parse_tree, serialize_commit, CommitData, ObjectId, ObjectKind,
};
use grit_lib::pathspec::matches_pathspec;
use grit_lib::refs::{append_reflog, list_refs, read_head, resolve_ref, write_ref};
use grit_lib::repo::Repository;
use grit_lib::rev_parse::resolve_revision;
use grit_lib::write_tree::write_tree_from_index;
use similar::{group_diff_ops, TextDiff};
use std::cmp::Ordering;
use std::collections::{BinaryHeap, HashMap, HashSet, VecDeque};
use std::fs;
use std::io::{self, BufRead, Write};
use std::path::Path;
use std::process::Command;
use time::OffsetDateTime;

const EMPTY_TREE_OID: &str = "4b825dc642cb6eb9a060e54bf8d69288fbee4904";

/// Launch the configured editor on `path`, matching Git's `launch_editor`.
///
/// Like [`crate::commands::commit::launch_commit_editor`], this resolves the editor with
/// `for_launch = true` so harness placeholders (`EDITOR=:` / `VISUAL=:`) are ignored and the
/// real fake editor installed via `test_set_editor` is honoured.
fn launch_history_editor(repo: &Repository, path: &Path) -> Result<()> {
    let config = ConfigSet::load(Some(&repo.git_dir), true).unwrap_or_default();
    let editor = match crate::editor::resolve_git_editor(&config, true) {
        Some(e) => e,
        None => return Ok(()),
    };

    // Git treats `:` as a no-op editor (`launch_specified_editor`).
    if editor.trim() == ":" {
        return Ok(());
    }

    // Match Git: the editor command is run under `sh -c` with the path as `$1` (not `$@`),
    // so `test_set_editor` patterns like `EDITOR='"$FAKE_EDITOR"'` expand and receive the file.
    let mut cmd = Command::new("sh");
    cmd.arg("-c")
        .arg(format!("{editor} \"$1\""))
        .arg("sh")
        .arg(path);
    // Run from the work tree so editor scripts that use relative paths (e.g. `fake-input` in
    // t3452-history-split) see the same cwd as `git commit`.
    if let Some(wt) = repo.work_tree.as_ref() {
        cmd.current_dir(wt);
    } else {
        cmd.current_dir(&repo.git_dir);
    }
    let status = cmd
        .status()
        .with_context(|| format!("failed to launch editor '{editor}'"))?;
    if !status.success() {
        bail!("editor exited with non-zero status");
    }
    Ok(())
}

/// Arguments for `grit history`.
#[derive(Debug, Parser)]
#[command(name = "grit-history", about = "Rewrite history")]
pub struct Args {
    #[command(subcommand)]
    pub command: HistoryCommand,
}

#[derive(Debug, Subcommand)]
pub enum HistoryCommand {
    /// Change a commit message and replay descendants.
    Reword(RewordArgs),
    /// Split one commit into two (interactive hunk selection).
    Split(SplitArgs),
}

#[derive(Debug, ClapArgs)]
pub struct RewordArgs {
    /// Print ref updates without modifying the repository.
    #[arg(short = 'n', long)]
    pub dry_run: bool,

    /// Limit which refs are updated: `branches` (default) or `head`.
    #[arg(long = "update-refs", value_name = "ACTION")]
    pub update_refs: Option<String>,

    /// Commit to reword.
    #[arg(value_name = "COMMIT")]
    pub commit: Option<String>,
}

#[derive(Debug, ClapArgs)]
pub struct SplitArgs {
    /// Print ref updates without modifying the repository.
    #[arg(short = 'n', long)]
    pub dry_run: bool,

    /// Limit which refs are updated: `branches` (default) or `head`.
    #[arg(long = "update-refs", value_name = "ACTION")]
    pub update_refs: Option<String>,

    /// Commit to split.
    #[arg(value_name = "COMMIT")]
    pub commit: Option<String>,

    /// Optional pathspecs limiting which changes can be split out.
    #[arg(
        value_name = "PATHSPEC",
        trailing_var_arg = true,
        allow_hyphen_values = true
    )]
    pub pathspec: Vec<String>,
}

/// Run `grit history`.
pub fn run(args: Args) -> Result<()> {
    match args.command {
        HistoryCommand::Reword(r) => run_reword(r),
        HistoryCommand::Split(s) => run_split(s),
    }
}

/// Parse `argv` after the `history` token (e.g. `["reword", "HEAD"]`) and run.
pub fn run_from_argv(rest: &[String]) -> Result<()> {
    // Match Git's `parse_options` subcommand handling so the error messages
    // line up with the upstream tests.
    let first = rest.iter().find(|a| !a.starts_with('-'));
    match first.map(String::as_str) {
        None => bail!("need a subcommand"),
        Some("reword") | Some("split") => {}
        Some(other) => bail!("unknown subcommand: `{other}'"),
    }
    let mut argv = vec!["grit-history".to_owned()];
    argv.extend(rest.iter().cloned());
    let args = Args::try_parse_from(&argv).map_err(|e| anyhow::anyhow!("{}", e))?;
    run(args)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum UpdateRefsMode {
    Branches,
    Head,
}

pub(crate) fn run_reword(args: RewordArgs) -> Result<()> {
    let repo = Repository::discover(None).context("not a git repository")?;
    let mode = match args.update_refs.as_deref() {
        None | Some("branches") => UpdateRefsMode::Branches,
        Some("head") => UpdateRefsMode::Head,
        Some(_) => {
            bail!("--update-refs expects one of 'branches' or 'head'");
        }
    };

    let commit_spec = args
        .commit
        .as_deref()
        .filter(|s| !s.is_empty())
        .ok_or_else(|| anyhow::anyhow!("command expects a single revision"))?;

    let original = resolve_revision(&repo, commit_spec)
        .map_err(|_| anyhow::anyhow!("commit cannot be found: {commit_spec}"))?;

    let original_obj = repo.odb.read(&original)?;
    let original_commit = parse_commit(&original_obj.data)?;

    if original_commit.parents.len() > 1 {
        let parent_obj = repo.odb.read(&original_commit.parents[0])?;
        let parent_c = parse_commit(&parent_obj.data)?;
        if parent_c.parents.len() > 1 {
            bail!("replaying merge commits is not supported yet!");
        }
    }

    let descendants = collect_descendants_to_replay(&repo, original, mode)?;
    if descendants.iter().any(|oid| {
        repo.odb
            .read(oid)
            .ok()
            .and_then(|o| parse_commit(&o.data).ok())
            .is_some_and(|c| c.parents.len() > 1)
    }) {
        bail!("replaying merge commits is not supported yet!");
    }

    let head_oid = resolve_revision(&repo, "HEAD").context("cannot look up HEAD")?;
    if mode == UpdateRefsMode::Head && !is_ancestor(&repo, original, head_oid)? {
        bail!("rewritten commit must be an ancestor of HEAD when using --update-refs=head");
    }

    let new_message = edit_reword_message(&repo, &original_commit)?;
    if new_message.trim().is_empty() {
        eprintln!("Aborting commit due to empty commit message.");
        bail!("empty commit message");
    }

    let rewritten = write_reworded_commit(
        &repo,
        &original_commit,
        &new_message,
        descendants.as_slice(),
    )?;

    let reflog_msg = format!("reword: updating {commit_spec}");
    apply_ref_updates(
        &repo,
        original,
        rewritten,
        &descendants,
        mode,
        args.dry_run,
        &reflog_msg,
    )?;

    Ok(())
}

pub(crate) fn run_split(args: SplitArgs) -> Result<()> {
    let repo = Repository::discover(None).context("not a git repository")?;
    let mode = match args.update_refs.as_deref() {
        None | Some("branches") => UpdateRefsMode::Branches,
        Some("head") => UpdateRefsMode::Head,
        Some(_) => {
            bail!("--update-refs expects one of 'branches' or 'head'");
        }
    };

    let commit_spec = args
        .commit
        .as_deref()
        .filter(|s| !s.is_empty())
        .ok_or_else(|| anyhow::anyhow!("command expects a committish"))?;

    let original = resolve_revision(&repo, commit_spec)
        .map_err(|_| anyhow::anyhow!("commit cannot be found: {commit_spec}"))?;

    let original_obj = repo.odb.read(&original)?;
    let original_commit = parse_commit(&original_obj.data)?;

    if original_commit.parents.len() > 1 {
        bail!("cannot split up merge commit");
    }

    if let Some(&p) = original_commit.parents.first() {
        let parent_obj = repo.odb.read(&p)?;
        let parent_c = parse_commit(&parent_obj.data)?;
        if parent_c.parents.len() > 1 {
            bail!("replaying merge commits is not supported yet!");
        }
    }

    let descendants = collect_descendants_to_replay(&repo, original, mode)?;
    if descendants.iter().any(|oid| {
        repo.odb
            .read(oid)
            .ok()
            .and_then(|o| parse_commit(&o.data).ok())
            .is_some_and(|c| c.parents.len() > 1)
    }) {
        bail!("replaying merge commits is not supported yet!");
    }

    let head_oid = resolve_revision(&repo, "HEAD").context("cannot look up HEAD")?;
    if mode == UpdateRefsMode::Head && !is_ancestor(&repo, original, head_oid)? {
        bail!("rewritten commit must be an ancestor of HEAD when using --update-refs=head");
    }

    let parent_tree = if let Some(&p) = original_commit.parents.first() {
        let po = repo.odb.read(&p)?;
        parse_commit(&po.data)?.tree
    } else {
        ObjectId::from_hex(EMPTY_TREE_OID).map_err(|_| anyhow::anyhow!("bad empty tree"))?
    };

    let split_tree_oid =
        split_commit_interactive(&repo, &parent_tree, &original_commit.tree, &args.pathspec)?;

    if split_tree_oid == parent_tree {
        bail!("split commit is empty");
    }
    if split_tree_oid == original_commit.tree {
        bail!("split commit tree matches original commit");
    }

    let first_commit = commit_split_out(
        &repo,
        &original_commit,
        original_commit.parents.clone(),
        &parent_tree,
        &split_tree_oid,
        "split-out",
    )?;

    let second_commit = commit_split_out(
        &repo,
        &original_commit,
        vec![first_commit],
        &split_tree_oid,
        &original_commit.tree,
        "split-out",
    )?;

    let reflog_msg = format!("split: updating {commit_spec}");
    apply_ref_updates(
        &repo,
        original,
        second_commit,
        &descendants,
        mode,
        args.dry_run,
        &reflog_msg,
    )?;

    Ok(())
}

fn path_matches_any_pathspec(path: &str, specs: &[String]) -> bool {
    if specs.is_empty() {
        return true;
    }
    specs.iter().any(|s| matches_pathspec(s, path))
}

fn index_from_tree(repo: &Repository, tree_oid: &ObjectId) -> Result<Index> {
    let obj = repo.odb.read(tree_oid)?;
    if obj.kind != ObjectKind::Tree {
        bail!("expected tree object");
    }
    let entries = parse_tree(&obj.data)?;
    let mut index = Index::new();
    for te in entries {
        let path = te.name;
        if te.mode == 0o040000 {
            let sub = index_from_tree(repo, &te.oid)?;
            for mut e in sub.entries {
                let mut full = path.clone();
                full.push(b'/');
                full.extend_from_slice(&e.path);
                e.path = full;
                let pl = e.path.len().min(0xFFF) as u16;
                e.flags = pl;
                index.add_or_replace(e);
            }
        } else if te.mode == MODE_GITLINK {
            continue;
        } else {
            let pl = path.len().min(0xFFF) as u16;
            index.add_or_replace(IndexEntry {
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
                flags: pl,
                flags_extended: None,
                path,
                base_index_pos: 0,
            });
        }
    }
    Ok(index)
}

fn read_blob_string(repo: &Repository, oid: &ObjectId) -> Result<String> {
    let obj = repo.odb.read(oid)?;
    if obj.kind != ObjectKind::Blob {
        bail!("expected blob");
    }
    String::from_utf8(obj.data).map_err(|_| anyhow::anyhow!("cannot split non-UTF-8 content"))
}

fn prompt_hunk_stdin() -> Result<bool> {
    let stdin = io::stdin();
    let mut stdin = stdin.lock();
    let mut line = String::new();
    stdin.read_line(&mut line)?;
    let c = line.trim().chars().next().unwrap_or('n');
    Ok(matches!(c, 'y' | 'Y'))
}

fn group_has_change(group: &[similar::DiffOp]) -> bool {
    group
        .iter()
        .any(|op| !matches!(op, similar::DiffOp::Equal { .. }))
}

fn print_hunk_group(old_text: &str, new_text: &str, group: &[similar::DiffOp]) {
    let old_lines: Vec<&str> = old_text.lines().collect();
    let new_lines: Vec<&str> = new_text.lines().collect();
    for op in group {
        match *op {
            similar::DiffOp::Equal { old_index, len, .. } => {
                for j in 0..len {
                    let line = old_lines.get(old_index + j).copied().unwrap_or("");
                    println!(" {line}");
                }
            }
            similar::DiffOp::Delete {
                old_index, old_len, ..
            } => {
                for j in 0..old_len {
                    let line = old_lines.get(old_index + j).copied().unwrap_or("");
                    println!("-{line}");
                }
            }
            similar::DiffOp::Insert {
                new_index, new_len, ..
            } => {
                for j in 0..new_len {
                    let line = new_lines.get(new_index + j).copied().unwrap_or("");
                    println!("+{line}");
                }
            }
            similar::DiffOp::Replace {
                old_index,
                old_len,
                new_index,
                new_len,
            } => {
                for j in 0..old_len {
                    let line = old_lines.get(old_index + j).copied().unwrap_or("");
                    println!("-{line}");
                }
                for j in 0..new_len {
                    let line = new_lines.get(new_index + j).copied().unwrap_or("");
                    println!("+{line}");
                }
            }
        }
    }
}

fn materialize_partial_apply(
    old_text: &str,
    new_text: &str,
    groups: &[Vec<similar::DiffOp>],
    apply_group: &[bool],
) -> String {
    let old_lines: Vec<&str> = old_text.lines().collect();
    let new_lines: Vec<&str> = new_text.lines().collect();
    let mut out: Vec<String> = Vec::new();

    for (gi, g) in groups.iter().enumerate() {
        let apply = apply_group.get(gi).copied().unwrap_or(false);
        for op in g {
            match *op {
                similar::DiffOp::Equal { old_index, len, .. } => {
                    for j in 0..len {
                        if let Some(line) = old_lines.get(old_index + j) {
                            out.push((*line).to_string());
                        }
                    }
                }
                similar::DiffOp::Delete {
                    old_index, old_len, ..
                } => {
                    if !apply {
                        for j in 0..old_len {
                            if let Some(line) = old_lines.get(old_index + j) {
                                out.push((*line).to_string());
                            }
                        }
                    }
                }
                similar::DiffOp::Insert {
                    new_index, new_len, ..
                } => {
                    if apply {
                        for j in 0..new_len {
                            if let Some(line) = new_lines.get(new_index + j) {
                                out.push((*line).to_string());
                            }
                        }
                    }
                }
                similar::DiffOp::Replace {
                    old_index,
                    old_len,
                    new_index,
                    new_len,
                } => {
                    if apply {
                        for j in 0..new_len {
                            if let Some(line) = new_lines.get(new_index + j) {
                                out.push((*line).to_string());
                            }
                        }
                    } else {
                        for j in 0..old_len {
                            if let Some(line) = old_lines.get(old_index + j) {
                                out.push((*line).to_string());
                            }
                        }
                    }
                }
            }
        }
    }

    let mut body = out.join("\n");
    let ends_with_nl =
        new_text.ends_with('\n') || (new_text.is_empty() && old_text.ends_with('\n'));
    if ends_with_nl && !body.is_empty() && !body.ends_with('\n') {
        body.push('\n');
    }
    body
}

fn format_mode_octal(mode: u32) -> String {
    format!("{mode:o}")
}

fn blob_oid_for_text(
    repo: &Repository,
    parent_blob: &ObjectId,
    old_text: &str,
    text: &str,
) -> Result<ObjectId> {
    if text == old_text {
        return Ok(*parent_blob);
    }
    Ok(repo
        .odb
        .write(ObjectKind::Blob, text.as_bytes())
        .context("write blob")?)
}

fn split_commit_interactive(
    repo: &Repository,
    parent_tree: &ObjectId,
    orig_tree: &ObjectId,
    pathspecs: &[String],
) -> Result<ObjectId> {
    let mut split_index = index_from_tree(repo, parent_tree)?;
    let entries = diff_trees(&repo.odb, Some(parent_tree), Some(orig_tree), "")?;

    for entry in entries {
        let path = entry
            .new_path
            .as_deref()
            .or(entry.old_path.as_deref())
            .unwrap_or(grit_lib::repo_path::RepoPath::from_str(""));
        if !path_matches_any_pathspec(&path.to_string_lossy(), pathspecs) {
            continue;
        }

        let path_bytes = path.as_bytes();

        match entry.status {
            DiffStatus::Added => {
                print!("diff --git a/{} b/{}\nnew file\n", path, path);
                let take = prompt_hunk_stdin()?;
                if take {
                    let pl = path_bytes.len().min(0xFFF) as u16;
                    split_index.add_or_replace(IndexEntry {
                        ctime_sec: 0,
                        ctime_nsec: 0,
                        mtime_sec: 0,
                        mtime_nsec: 0,
                        dev: 0,
                        ino: 0,
                        mode: u32::from_str_radix(&entry.new_mode, 8).unwrap_or(MODE_REGULAR),
                        uid: 0,
                        gid: 0,
                        size: 0,
                        oid: entry.new_oid,
                        flags: pl,
                        flags_extended: None,
                        path: path_bytes.to_vec(),
                        base_index_pos: 0,
                    });
                }
            }
            DiffStatus::Deleted => {
                print!("diff --git a/{path} b/{path}\ndeleted\n");
                let take = prompt_hunk_stdin()?;
                if take {
                    split_index.remove(path_bytes);
                }
            }
            DiffStatus::Modified | DiffStatus::TypeChanged => {
                let parent_mode = split_index
                    .get(path_bytes, 0)
                    .map(|e| e.mode)
                    .unwrap_or(MODE_REGULAR);
                let old_mode = u32::from_str_radix(&entry.old_mode, 8).unwrap_or(MODE_REGULAR);
                let new_mode = u32::from_str_radix(&entry.new_mode, 8).unwrap_or(MODE_REGULAR);
                let mode_only = entry.old_oid == entry.new_oid && old_mode != new_mode;

                let mut staged_mode = parent_mode;
                if mode_only {
                    println!("@@ {path} @@");
                    println!(
                        " mode change {} => {}",
                        format_mode_octal(old_mode),
                        format_mode_octal(new_mode)
                    );
                    if prompt_hunk_stdin()? {
                        staged_mode = new_mode;
                    }
                } else if old_mode != new_mode {
                    println!("@@ {path} @@");
                    println!(
                        " mode change {} => {}",
                        format_mode_octal(old_mode),
                        format_mode_octal(new_mode)
                    );
                    if prompt_hunk_stdin()? {
                        staged_mode = new_mode;
                    }
                }

                let old_data = read_blob_string(repo, &entry.old_oid)?;
                let new_data = read_blob_string(repo, &entry.new_oid)?;
                let diff = TextDiff::configure().diff_lines(&old_data, &new_data);
                let context_lines = 3usize;
                let groups = group_diff_ops(diff.ops().to_vec(), context_lines.saturating_mul(2));
                let mut apply_group = vec![false; groups.len()];
                for (gi, g) in groups.iter().enumerate() {
                    if !group_has_change(g) {
                        continue;
                    }
                    println!("@@ {path} @@");
                    print_hunk_group(&old_data, &new_data, g);
                    apply_group[gi] = prompt_hunk_stdin()?;
                }
                let merged = materialize_partial_apply(&old_data, &new_data, &groups, &apply_group);
                let blob = blob_oid_for_text(repo, &entry.old_oid, &old_data, &merged)?;
                if blob == entry.old_oid && staged_mode == parent_mode {
                    continue;
                }
                let pl = path_bytes.len().min(0xFFF) as u16;
                let oid_for_index = if blob == entry.old_oid {
                    entry.old_oid
                } else {
                    blob
                };
                split_index.add_or_replace(IndexEntry {
                    ctime_sec: 0,
                    ctime_nsec: 0,
                    mtime_sec: 0,
                    mtime_nsec: 0,
                    dev: 0,
                    ino: 0,
                    mode: staged_mode,
                    uid: 0,
                    gid: 0,
                    size: 0,
                    oid: oid_for_index,
                    flags: pl,
                    flags_extended: None,
                    path: path_bytes.to_vec(),
                    base_index_pos: 0,
                });
            }
            DiffStatus::Renamed | DiffStatus::Copied | DiffStatus::Unmerged => {
                bail!(
                    "unsupported diff status for history split: {:?}",
                    entry.status
                );
            }
        }
    }

    write_tree_from_index(&repo.odb, &split_index, "").context("write split tree")
}

fn find_commit_body_for_split_editor(message: &str) -> String {
    let mut lines = message.splitn(2, '\n');
    let _subject = lines.next().unwrap_or("");
    lines.next().unwrap_or("").to_string()
}

fn fill_split_commit_message(
    repo: &Repository,
    based_on: &CommitData,
    old_tree: &ObjectId,
    new_tree: &ObjectId,
) -> Result<String> {
    let body_default = find_commit_body_for_split_editor(&based_on.message);
    let subject = based_on.message.split('\n').next().unwrap_or("").to_owned();

    let mut buf = String::new();
    if body_default.trim().is_empty() {
        buf.push_str(&subject);
    } else {
        buf.push_str(body_default.trim_end_matches(['\n', '\r']));
    }
    buf.push('\n');
    buf.push('\n');
    buf.push_str(
        "# Please enter the commit message for the split-out changes. Lines starting\n\
         # with '#' will be ignored, and an empty message aborts the commit.\n",
    );

    let edit_path = repo.git_dir.join("COMMIT_EDITMSG");
    fs::write(&edit_path, &buf).context("writing COMMIT_EDITMSG")?;

    {
        let mut f = fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&edit_path)?;
        append_tree_diff_status(repo, old_tree, new_tree, &mut f)?;
    }

    launch_history_editor(repo, &edit_path)?;

    let edited = fs::read_to_string(&edit_path).context("reading COMMIT_EDITMSG")?;
    Ok(cleanup_edited_commit_message(&edited, "#"))
}

/// Resolve the committer identity line for a split-out commit.
///
/// Delegates to [`crate::commands::commit::resolve_committer`] so the split-out commits use the
/// same identity/date resolution as `git commit`. In particular this honours `GIT_COMMITTER_DATE`
/// (set by the test harness's `test_tick`) instead of always using the wall-clock time, matching
/// upstream `git history split` which passes `NULL` committer info to `commit_tree_extended` and so
/// lets `git_committer_info()` pick up the environment date.
fn resolve_committer_for_split(repo: &Repository, now: OffsetDateTime) -> Result<String> {
    let config = ConfigSet::load(Some(&repo.git_dir), true)?;
    crate::commands::commit::resolve_committer(&config, now)
}

fn commit_split_out(
    repo: &Repository,
    based_on: &CommitData,
    parents: Vec<ObjectId>,
    old_tree: &ObjectId,
    new_tree: &ObjectId,
    _action: &str,
) -> Result<ObjectId> {
    let message = fill_split_commit_message(repo, based_on, old_tree, new_tree)?;
    if message.trim().is_empty() {
        eprintln!("Aborting commit due to empty commit message.");
        bail!("empty commit message");
    }

    let mut body = message;
    if !body.ends_with('\n') {
        body.push('\n');
    }

    let committer = resolve_committer_for_split(repo, OffsetDateTime::now_utc())?;

    let commit = CommitData {
        tree: *new_tree,
        parents,
        author: based_on.author.clone(),
        committer,
        author_raw: based_on.author_raw.clone(),
        committer_raw: Vec::new(),
        encoding: None,
        message: body,
        raw_message: None,
    };
    let bytes = serialize_commit(&commit);
    repo.odb
        .write(ObjectKind::Commit, &bytes)
        .context("failed writing split commit")
}

fn collect_descendants_to_replay(
    repo: &Repository,
    original: ObjectId,
    mode: UpdateRefsMode,
) -> Result<Vec<ObjectId>> {
    let mut tip_set: HashSet<ObjectId> = HashSet::new();
    match mode {
        UpdateRefsMode::Branches => {
            if let Ok(h) = resolve_ref(&repo.git_dir, "HEAD") {
                tip_set.insert(h);
            }
            for (_, oid) in list_refs(&repo.git_dir, "refs/")? {
                if let Ok(c) = peel_to_commit(repo, oid) {
                    tip_set.insert(c);
                }
            }
        }
        UpdateRefsMode::Head => {
            tip_set.insert(resolve_ref(&repo.git_dir, "HEAD")?);
        }
    }

    let mut seen_walk: HashSet<ObjectId> = HashSet::new();
    let mut queue: VecDeque<ObjectId> = VecDeque::new();
    for t in tip_set {
        if seen_walk.insert(t) {
            queue.push_back(t);
        }
    }

    while let Some(oid) = queue.pop_front() {
        let obj = repo.odb.read(&oid)?;
        let c = parse_commit(&obj.data)?;
        for p in &c.parents {
            if seen_walk.insert(*p) {
                queue.push_back(*p);
            }
        }
    }

    let mut selected: HashSet<ObjectId> = HashSet::new();
    for &oid in &seen_walk {
        if oid != original && is_ancestor(repo, original, oid)? {
            selected.insert(oid);
        }
    }

    let mut order = topo_sort_descendants(repo, &selected)?;
    order.reverse();
    Ok(order)
}

fn peel_to_commit(repo: &Repository, mut oid: ObjectId) -> Result<ObjectId> {
    loop {
        let object = repo.odb.read(&oid)?;
        match object.kind {
            ObjectKind::Commit => return Ok(oid),
            ObjectKind::Tag => {
                let tag = parse_tag(&object.data)?;
                oid = tag.object;
            }
            _ => {
                bail!("peel_to_commit: not a commit");
            }
        }
    }
}

#[derive(Clone, Copy)]
struct TopoKey {
    oid: ObjectId,
    time: i64,
}

impl Eq for TopoKey {}

impl PartialEq for TopoKey {
    fn eq(&self, other: &Self) -> bool {
        self.oid == other.oid
    }
}

impl Ord for TopoKey {
    fn cmp(&self, other: &Self) -> Ordering {
        self.time
            .cmp(&other.time)
            .then_with(|| self.oid.cmp(&other.oid))
    }
}

impl PartialOrd for TopoKey {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

fn committer_timestamp(data: &CommitData) -> i64 {
    fn ts_from_ident(line: &str) -> i64 {
        let parts = line.split_whitespace().collect::<Vec<_>>();
        if parts.len() < 2 {
            return 0;
        }
        let ts = parts[parts.len().saturating_sub(2)];
        ts.parse::<i64>().unwrap_or(0)
    }
    ts_from_ident(&data.committer)
}

fn topo_sort_descendants(repo: &Repository, selected: &HashSet<ObjectId>) -> Result<Vec<ObjectId>> {
    let mut child_count: HashMap<ObjectId, usize> = selected.iter().map(|&oid| (oid, 0)).collect();
    for &oid in selected {
        let obj = repo.odb.read(&oid)?;
        let c = parse_commit(&obj.data)?;
        for p in &c.parents {
            if selected.contains(p) {
                if let Some(n) = child_count.get_mut(p) {
                    *n += 1;
                }
            }
        }
    }

    let mut heap = BinaryHeap::new();
    for (&oid, &cnt) in &child_count {
        if cnt == 0 {
            let obj = repo.odb.read(&oid)?;
            let c = parse_commit(&obj.data)?;
            heap.push(TopoKey {
                oid,
                time: committer_timestamp(&c),
            });
        }
    }

    let mut out = Vec::with_capacity(selected.len());
    while let Some(item) = heap.pop() {
        let oid = item.oid;
        out.push(oid);
        let obj = repo.odb.read(&oid)?;
        let c = parse_commit(&obj.data)?;
        for p in &c.parents {
            if !selected.contains(p) {
                continue;
            }
            if let Some(cnt) = child_count.get_mut(p) {
                *cnt = cnt.saturating_sub(1);
                if *cnt == 0 {
                    let po = repo.odb.read(p)?;
                    let pc = parse_commit(&po.data)?;
                    heap.push(TopoKey {
                        oid: *p,
                        time: committer_timestamp(&pc),
                    });
                }
            }
        }
    }

    Ok(out)
}

fn edit_reword_message(repo: &Repository, commit: &CommitData) -> Result<String> {
    let parent_tree = if let Some(&p) = commit.parents.first() {
        let po = repo.odb.read(&p)?;
        parse_commit(&po.data)?.tree
    } else {
        ObjectId::from_hex(EMPTY_TREE_OID).map_err(|_| anyhow::anyhow!("bad empty tree"))?
    };

    let subject = commit.message.split('\n').next().unwrap_or("").to_owned();

    let mut buf = String::new();
    buf.push_str(&subject);
    buf.push('\n');
    buf.push('\n');
    buf.push_str(
        "# Please enter the commit message for the reworded changes. Lines starting\n\
         # with '#' will be ignored, and an empty message aborts the commit.\n",
    );

    let edit_path = repo.git_dir.join("COMMIT_EDITMSG");
    fs::write(&edit_path, &buf).context("writing COMMIT_EDITMSG")?;

    {
        let mut f = fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&edit_path)?;
        append_tree_diff_status(repo, &parent_tree, &commit.tree, &mut f)?;
    }

    launch_history_editor(repo, &edit_path)?;

    let edited = fs::read_to_string(&edit_path).context("reading COMMIT_EDITMSG")?;
    let cfg = ConfigSet::load(Some(&repo.git_dir), true).unwrap_or_default();
    let prefix = comment_line_prefix_full(&cfg);
    Ok(cleanup_edited_commit_message(&edited, prefix.as_ref()))
}

fn append_tree_diff_status(
    repo: &Repository,
    old_tree: &ObjectId,
    new_tree: &ObjectId,
    w: &mut dyn Write,
) -> Result<()> {
    writeln!(w, "# Changes to be committed:")?;
    let entries = diff_trees(&repo.odb, Some(old_tree), Some(new_tree), "")?;
    let mut paths: Vec<&DiffEntry> = entries.iter().collect();
    paths.sort_by(|a, b| {
        let pa = a
            .new_path
            .as_deref()
            .or(a.old_path.as_deref())
            .unwrap_or(grit_lib::repo_path::RepoPath::from_str(""));
        let pb = b
            .new_path
            .as_deref()
            .or(b.old_path.as_deref())
            .unwrap_or(grit_lib::repo_path::RepoPath::from_str(""));
        pa.cmp(pb)
    });
    for e in paths {
        let path = e
            .new_path
            .as_deref()
            .or(e.old_path.as_deref())
            .unwrap_or(grit_lib::repo_path::RepoPath::from_str("?"));
        let label = match e.status {
            DiffStatus::Added => "new file",
            DiffStatus::Deleted => "deleted",
            DiffStatus::Modified => "modified",
            DiffStatus::Renamed => "renamed",
            DiffStatus::Copied => "copied",
            DiffStatus::TypeChanged => "typechange",
            DiffStatus::Unmerged => "unmerged",
        };
        // Match `git commit` short status: "new file:   path" (three spaces after colon).
        writeln!(w, "#\t{label}:   {path}")?;
    }
    writeln!(w, "#")?;
    Ok(())
}

fn epoch_from_ident_line(ident: &str) -> i64 {
    if let Some(gt) = ident.rfind('>') {
        let after = ident[gt + 1..].trim();
        if let Some(epoch_str) = after.split_whitespace().next() {
            return epoch_str.parse::<i64>().unwrap_or(0);
        }
    }
    0
}

fn min_committer_epoch_among(repo: &Repository, oids: &[ObjectId]) -> Result<i64> {
    let mut min_e = i64::MAX;
    for oid in oids {
        let obj = repo.odb.read(oid)?;
        let c = parse_commit(&obj.data)?;
        let e = epoch_from_ident_line(&c.committer);
        min_e = min_e.min(e);
    }
    if min_e == i64::MAX {
        Ok(0)
    } else {
        Ok(min_e)
    }
}

fn committer_with_epoch(base_ident: &str, epoch: i64) -> String {
    if let Some(gt) = base_ident.rfind('>') {
        let prefix = &base_ident[..=gt];
        let after = base_ident[gt + 1..].trim();
        let tz = after
            .split_whitespace()
            .nth(1)
            .unwrap_or("+0000")
            .to_owned();
        return format!("{prefix} {epoch} {tz}");
    }
    base_ident.to_owned()
}

fn write_reworded_commit(
    repo: &Repository,
    original: &CommitData,
    message: &str,
    descendants: &[ObjectId],
) -> Result<ObjectId> {
    let mut body = message.to_owned();
    if !body.ends_with('\n') {
        body.push('\n');
    }

    let mut committer = original.committer.clone();
    if !descendants.is_empty() {
        let min_desc = min_committer_epoch_among(repo, descendants)?;
        let orig_e = epoch_from_ident_line(&original.committer);
        let target = min_desc.min(orig_e).saturating_sub(1);
        committer = committer_with_epoch(&original.committer, target);
    }

    let committer_raw = if committer == original.committer {
        original.committer_raw.clone()
    } else {
        Vec::new()
    };
    let commit = CommitData {
        tree: original.tree,
        parents: original.parents.clone(),
        author: original.author.clone(),
        committer,
        author_raw: original.author_raw.clone(),
        committer_raw,
        encoding: None,
        message: body,
        raw_message: None,
    };
    let bytes = serialize_commit(&commit);
    repo.odb
        .write(ObjectKind::Commit, &bytes)
        .context("failed writing reworded commit")
}

fn apply_ref_updates(
    repo: &Repository,
    original: ObjectId,
    rewritten: ObjectId,
    descendants: &[ObjectId],
    mode: UpdateRefsMode,
    dry_run: bool,
    reflog_msg: &str,
) -> Result<()> {
    let detached_head = read_head(&repo.git_dir)?.is_none();
    let identity = resolve_reflog_identity(repo);

    let selected_set: HashSet<ObjectId> = descendants.iter().copied().collect();

    let mut updates: Vec<(String, ObjectId, ObjectId)> = Vec::new();

    for (refname, oid) in list_refs(&repo.git_dir, "refs/")? {
        if mode == UpdateRefsMode::Head {
            continue;
        }
        if oid == original {
            updates.push((refname, rewritten, original));
            continue;
        }
        if is_ancestor(repo, original, oid)? && oid != original {
            let subset: HashSet<ObjectId> = selected_set
                .iter()
                .copied()
                .filter(|&c| is_ancestor(repo, c, oid).unwrap_or(false))
                .collect();
            if subset.is_empty() {
                continue;
            }
            let chain: Vec<ObjectId> = descendants
                .iter()
                .copied()
                .filter(|c| subset.contains(c))
                .collect();
            let (new_oid, _) = replay_commits_onto(repo, &chain, rewritten)?;
            updates.push((refname, new_oid, oid));
        }
    }

    if mode == UpdateRefsMode::Branches {
        if detached_head {
            if let Ok(head_oid) = resolve_ref(&repo.git_dir, "HEAD") {
                if head_oid == original {
                    updates.push(("HEAD".to_owned(), rewritten, original));
                } else if is_ancestor(repo, original, head_oid)? {
                    let subset: HashSet<ObjectId> = selected_set
                        .iter()
                        .copied()
                        .filter(|&c| is_ancestor(repo, c, head_oid).unwrap_or(false))
                        .collect();
                    if !subset.is_empty() {
                        let chain: Vec<ObjectId> = descendants
                            .iter()
                            .copied()
                            .filter(|c| subset.contains(c))
                            .collect();
                        let (new_oid, _) = replay_commits_onto(repo, &chain, rewritten)?;
                        updates.push(("HEAD".to_owned(), new_oid, head_oid));
                    }
                }
            }
        }
    } else if mode == UpdateRefsMode::Head {
        if let Ok(head_oid) = resolve_ref(&repo.git_dir, "HEAD") {
            let head_leaf = read_head(&repo.git_dir)?.unwrap_or_else(|| "HEAD".to_owned());
            if head_oid == original {
                updates.push((head_leaf, rewritten, original));
            } else if is_ancestor(repo, original, head_oid)? {
                let subset: HashSet<ObjectId> = selected_set
                    .iter()
                    .copied()
                    .filter(|&c| is_ancestor(repo, c, head_oid).unwrap_or(false))
                    .collect();
                if !subset.is_empty() {
                    let chain: Vec<ObjectId> = descendants
                        .iter()
                        .copied()
                        .filter(|c| subset.contains(c))
                        .collect();
                    let (new_oid, _) = replay_commits_onto(repo, &chain, rewritten)?;
                    updates.push((head_leaf, new_oid, head_oid));
                }
            }
        }
    }

    let mut by_ref: HashMap<String, (ObjectId, ObjectId)> = HashMap::new();
    for (r, n, o) in updates {
        by_ref.insert(r, (n, o));
    }

    for (refname, (new_oid, old_oid)) in &by_ref {
        if dry_run {
            println!(
                "update {} {} {}",
                refname,
                new_oid.to_hex(),
                old_oid.to_hex()
            );
        } else {
            write_ref(&repo.git_dir, refname, new_oid)
                .with_context(|| format!("failed to update ref '{refname}'"))?;
            let _ = append_reflog(
                &repo.git_dir,
                refname,
                old_oid,
                new_oid,
                &identity,
                reflog_msg,
                false,
            );
        }
    }

    if !dry_run {
        if let Ok(Some(branch)) = read_head(&repo.git_dir) {
            if let Some((new_oid, old_oid)) = by_ref.get(&branch) {
                let _ = append_reflog(
                    &repo.git_dir,
                    "HEAD",
                    old_oid,
                    new_oid,
                    &identity,
                    reflog_msg,
                    false,
                );
            }
        }
    }

    Ok(())
}

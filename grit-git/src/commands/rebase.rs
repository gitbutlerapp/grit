//! `grit rebase` — reapply commits on top of another base tip.
//!
//! Non-interactive rebase replays a series of commits by cherry-picking each
//! one onto the new base.  For a commit C with parent P being replayed onto
//! current HEAD:
//!
//!   - base   = P.tree     (parent of the commit being replayed)
//!   - ours   = HEAD.tree  (current tip we're building on)
//!   - theirs = C.tree     (the commit being replayed)
//!
//! This three-way merge produces the replayed commit.

use anyhow::{bail, Context, Result};
use clap::Args as ClapArgs;
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::fs;
use std::fs::OpenOptions;
use std::io::{self, ErrorKind, Write};
use std::path::{Path, PathBuf};

use grit_lib::commit_trailers::{append_signoff_trailer, format_signoff_line};
use grit_lib::config::{ConfigScope, ConfigSet};
use grit_lib::diff::{self, count_changes, diff_index_to_tree, diff_index_to_worktree, DiffEntry};
use grit_lib::hooks::{run_commit_hook, run_hook, CommitHookEnv, HookResult};
use grit_lib::index::{Index, IndexEntry, MODE_EXECUTABLE, MODE_GITLINK, MODE_SYMLINK, MODE_TREE};
use grit_lib::interpret_trailers::{
    complete_line, process_trailers, NewTrailerArg, ProcessTrailerOptions,
};
use grit_lib::merge_base::{ancestor_closure, fork_point, is_ancestor, merge_bases_first_vs_rest};
use grit_lib::merge_file::{merge, ConflictStyle, MergeFavor, MergeInput};
use grit_lib::objects::{
    parse_commit, parse_tree, serialize_commit, CommitData, ObjectId, ObjectKind,
};
use grit_lib::patch_ids::compute_patch_id;
use grit_lib::porcelain::rebase::{
    append_commented, append_nth_squash_message, append_skipped_squash_message,
    commit_subject_single_line, fixup_replacement_message, format_autosquash_subject_for_match,
    message_body_after_subject, rebase_todo_command_for_display_abbrev, skip_fixupish_prefix,
    strip_fixupish_chain, update_squash_message_for_fixup, FixupMessageMode, RebaseTodoCmd,
};
use grit_lib::refs::{append_reflog, delete_ref, list_refs, resolve_ref, write_ref};
use grit_lib::repo::Repository;
use grit_lib::rev_list::{rev_list, split_revision_token, OrderingMode, RevListOptions};
use grit_lib::rev_parse::{
    abbreviate_object_id, peel_to_commit_for_merge_base, resolve_revision,
    resolve_revision_as_commit_without_index_dwim, resolve_revision_for_range_end,
    split_triple_dot_range, upstream_suffix_info,
};
use grit_lib::state::{resolve_head, HeadState};
use grit_lib::whitespace_rule::{fix_blob_bytes, parse_whitespace_rule, WS_DEFAULT_RULE};
use grit_lib::write_tree::write_tree_from_index;

use super::checkout::{
    check_dirty_worktree, checkout_index_to_worktree, refuse_populated_submodule_tree_replacement,
};
use super::cherry_pick::{
    bail_if_df_merge_would_remove_cwd, preflight_cherry_pick_cwd_obstruction,
};
use super::commit::{
    cleanup_edited_commit_message, comment_line_prefix_full, split_stored_author_line,
};
use super::merge::refresh_index_stat_cache_from_worktree;
use super::replay::merge_trees_for_single_cherry_pick;
use super::stash;
use crate::explicit_exit::ExplicitExit;
use crate::ident::{resolve_email, resolve_name, IdentRole};

#[derive(Clone, Copy, Debug)]
struct RebaseReplayCommitOpts {
    ignore_space_change: bool,
    committer_date_is_author_date: bool,
    ignore_date: bool,
}

#[derive(Clone, Copy)]
enum RebaseBackend {
    Merge,
    Apply,
}

#[derive(Clone, Copy)]
struct RebaseConflictContext<'a> {
    backend: RebaseBackend,
    /// Abbreviated object id of the commit being picked (for the diff3 ancestor label).
    picked_short_oid: &'a str,
    picked_subject: &'a str,
    ignore_space_change: bool,
}

impl<'a> RebaseConflictContext<'a> {
    fn style(self, repo: &Repository) -> ConflictStyle {
        let Ok(config) = ConfigSet::load(Some(&repo.git_dir), true) else {
            return ConflictStyle::Merge;
        };
        match config
            .get("merge.conflictstyle")
            .unwrap_or_default()
            .to_lowercase()
            .as_str()
        {
            "diff3" => ConflictStyle::Diff3,
            "zdiff3" => ConflictStyle::ZealousDiff3,
            _ => ConflictStyle::Merge,
        }
    }

    fn label_ours(self) -> &'static str {
        "HEAD"
    }

    fn label_base(self) -> String {
        match self.backend {
            // Matches Git's sequencer: out->parent_label = "parent of <abbrev> (<subject>)".
            RebaseBackend::Merge => format!(
                "parent of {} ({})",
                self.picked_short_oid, self.picked_subject
            ),
            RebaseBackend::Apply => "constructed fake ancestor".to_string(),
        }
    }
}

/// Arguments for `grit rebase`.
#[derive(Debug, Clone, ClapArgs)]
#[command(about = "Reapply commits on top of another base tip")]
pub struct Args {
    /// Set when the user supplied an upstream argument (not defaulted from @{upstream}).
    #[arg(skip)]
    pub upstream_explicit: bool,

    /// Upstream branch to rebase onto (default: upstream tracking branch).
    #[arg(value_name = "UPSTREAM")]
    pub upstream: Option<String>,

    /// Rebase onto a specific base (used with `--onto <newbase> <upstream>`).
    #[arg(long)]
    pub onto: Option<String>,

    /// Rebase all commits reachable from the branch tip, not just those after the merge-base with upstream.
    #[arg(long)]
    pub root: bool,

    /// Interactive rebase (write todo list only).
    #[arg(short = 'i', long = "interactive")]
    pub interactive: bool,

    /// Continue the rebase after resolving conflicts.
    #[arg(long = "continue")]
    pub r#continue: bool,

    /// Abort the in-progress rebase.
    #[arg(long = "abort", conflicts_with = "verbose")]
    pub abort: bool,

    /// Skip the current commit and continue.
    #[arg(long = "skip")]
    pub skip: bool,

    /// Run a shell command after each commit is applied. May be given multiple times; each `--exec`
    /// adds an `exec` line after every pick, in order.
    #[arg(short = 'x', long = "exec")]
    pub exec: Vec<String>,

    /// Use the merge backend for rebasing (default, accepted for compatibility).
    #[arg(long = "merge", short = 'm', conflicts_with = "apply")]
    pub merge: bool,

    /// Use the apply backend for rebasing (accepted for compatibility).
    #[arg(long = "apply", conflicts_with = "merge")]
    pub apply: bool,

    /// Rebase merge commits (`-r` / optional mode: `rebase-cousins`, `no-rebase-cousins`).
    /// Optional mode only as `--rebase-merges=rebase-cousins` (must use `=` so `A` in
    /// `rebase -i --rebase-merges A main` is not consumed as the mode).
    ///
    /// `--rebase-merges` and `--no-rebase-merges` are NOT mutually exclusive: git treats them as a
    /// negatable option where the later flag wins, and `--no-rebase-merges` countermands an earlier
    /// `--rebase-merges` (t3430). Precedence is resolved in `effective_rebase_merges_settings`,
    /// which gives `--no-rebase-merges` priority, so they must not `conflicts_with` each other.
    #[arg(
        long = "rebase-merges",
        value_name = "MODE",
        num_args = 0..=1,
        default_missing_value = "true",
        require_equals = true
    )]
    pub rebase_merges: Option<String>,

    /// Disable rebasing merges even when `rebase.rebaseMerges` is true.
    #[arg(long = "no-rebase-merges")]
    pub no_rebase_merges: bool,

    /// Force rebase even if the current branch is up to date
    /// (Git's `-f`/`--force-rebase`, also spelled `--no-ff`).
    #[arg(short = 'f', long = "no-ff", visible_alias = "force-rebase")]
    pub no_ff: bool,

    /// GPG/SSH-sign the replayed commits (Git's `-S`/`--gpg-sign`). The key id is
    /// optional and only attached (`-S<keyid>` / `--gpg-sign=<keyid>`).
    #[arg(short = 'S', long = "gpg-sign", value_name = "KEYID", num_args = 0..=1, default_missing_value = "", require_equals = true)]
    pub gpg_sign: Option<String>,

    /// Do not sign the replayed commits (overrides `commit.gpgsign`).
    #[arg(long = "no-gpg-sign")]
    pub no_gpg_sign: bool,

    /// Add Signed-off-by trailer to rebased commits.
    #[arg(long = "signoff", overrides_with = "no_signoff")]
    pub signoff: bool,

    /// Do not add Signed-off-by trailers (overrides an alias with --signoff).
    #[arg(long = "no-signoff", overrides_with = "signoff")]
    pub no_signoff: bool,

    /// Append a trailer to every rebased commit message.
    #[arg(long = "trailer", value_name = "TRAILER")]
    pub trailer: Vec<String>,

    /// Keep the base of the branch (rebase onto the merge-base of upstream and branch).
    /// May be passed multiple times for Git compatibility (`--keep-base --keep-base`).
    #[arg(long = "keep-base", action = clap::ArgAction::Count)]
    pub keep_base: u8,

    /// Use the fork-point algorithm to find the merge base.
    #[arg(long = "fork-point", overrides_with = "no_fork_point")]
    pub fork_point: bool,

    /// Do not use the fork-point algorithm.
    #[arg(long = "no-fork-point")]
    pub no_fork_point: bool,

    /// Replay every picked commit even when it matches upstream by patch-id (Git default off).
    #[arg(
        long = "reapply-cherry-picks",
        overrides_with = "no_reapply_cherry_picks"
    )]
    pub reapply_cherry_picks: bool,

    /// Omit commits that match upstream by patch-id (default unless `--keep-base`).
    #[arg(long = "no-reapply-cherry-picks")]
    pub no_reapply_cherry_picks: bool,

    /// Be verbose (show diffs).
    #[arg(short = 'v', long = "verbose", conflicts_with = "abort")]
    pub verbose: bool,

    /// Be quiet.
    #[arg(short = 'q', long = "quiet")]
    pub quiet: bool,

    /// Update stale tracking branches after rebase.
    #[arg(long = "update-refs", conflicts_with = "no_update_refs")]
    pub update_refs: bool,

    /// Do not update other branches even when `rebase.updateRefs` is true.
    #[arg(long = "no-update-refs", conflicts_with = "update_refs")]
    pub no_update_refs: bool,

    /// How to handle commits that become empty (merge backend; accepted for compatibility).
    #[arg(long = "empty", value_name = "mode")]
    pub empty: Option<String>,

    /// Merge strategy (merge backend; accepted for compatibility).
    #[arg(short = 's', long = "strategy", value_name = "strategy")]
    pub strategy: Option<String>,

    /// Options for the merge strategy (merge backend; accepted for compatibility).
    #[arg(
        short = 'X',
        short_alias = 'o',
        long = "strategy-option",
        value_name = "option"
    )]
    pub strategy_option: Vec<String>,

    /// Branch to rebase (checkout first, then rebase onto upstream).
    #[arg(value_name = "BRANCH")]
    pub branch: Option<String>,

    /// Show a diffstat of what would be replayed (also honors `rebase.stat` config).
    #[arg(long = "stat")]
    pub stat: bool,

    /// Do not show a diffstat (overrides `rebase.stat` config).
    #[arg(short = 'n', long = "no-stat")]
    pub no_stat: bool,

    /// Passed through for compatibility; validated when present.
    #[arg(short = 'C', value_name = "n")]
    pub context_lines: Option<String>,

    /// Passed through for compatibility; validated when present.
    #[arg(long = "whitespace", value_name = "action")]
    pub whitespace: Option<String>,

    /// Stash local changes before starting and restore after (or honor `rebase.autostash`).
    #[arg(long = "autostash")]
    pub autostash: bool,

    /// Do not stash local changes (overrides `rebase.autostash`).
    #[arg(long = "no-autostash")]
    pub no_autostash: bool,

    /// Quit an in-progress rebase, keeping HEAD and working tree as-is.
    #[arg(long = "quit")]
    pub quit: bool,

    /// Move fixup!/squash! commits next to their targets (also implied by `rebase.autosquash` with `-i`).
    #[arg(long = "autosquash")]
    pub autosquash: bool,

    /// Disable autosquash even when `rebase.autosquash` is true.
    #[arg(long = "no-autosquash")]
    pub no_autosquash: bool,

    /// Keep commits that do not change any file (empty patch).
    #[arg(short = 'k', long = "keep-empty", overrides_with = "no_keep_empty")]
    pub keep_empty: bool,

    /// Drop commits that start empty before the rebase.
    #[arg(long = "no-keep-empty", overrides_with = "keep_empty")]
    pub no_keep_empty: bool,

    /// Ignore whitespace when applying patches (merge backend: `ignore-space-change`).
    #[arg(long = "ignore-whitespace")]
    pub ignore_whitespace: bool,

    /// Update the index with reused recorded conflict resolutions.
    #[arg(long = "rerere-autoupdate", conflicts_with = "no_rerere_autoupdate")]
    pub rerere_autoupdate: bool,

    /// Do not update the index with reused recorded conflict resolutions.
    #[arg(long = "no-rerere-autoupdate", conflicts_with = "rerere_autoupdate")]
    pub no_rerere_autoupdate: bool,

    /// Reschedule failed exec commands in interactive rebase.
    #[arg(
        long = "reschedule-failed-exec",
        conflicts_with = "no_reschedule_failed_exec"
    )]
    pub reschedule_failed_exec: bool,

    /// Do not reschedule failed exec commands in interactive rebase.
    #[arg(
        long = "no-reschedule-failed-exec",
        conflicts_with = "reschedule_failed_exec"
    )]
    pub no_reschedule_failed_exec: bool,

    /// Set committer date to the author date of the replayed commit.
    #[arg(long = "committer-date-is-author-date")]
    pub committer_date_is_author_date: bool,

    /// Ignore author dates from replayed commits; use current time at +0000 (alias: `--ignore-date`).
    #[arg(long = "reset-author-date", alias = "ignore-date")]
    pub reset_author_date: bool,

    /// Edit the todo list of the current interactive rebase.
    #[arg(long = "edit-todo")]
    pub edit_todo: bool,

    /// Show the patch currently being replayed.
    #[arg(long = "show-current-patch")]
    pub show_current_patch: bool,

    /// Do not run the pre-rebase hook.
    #[arg(long = "no-verify")]
    pub no_verify: bool,
}

/// Expand combined short flags (`-ki`, `-ik`) before clap parsing.
///
/// Git's `-r` is an alias for `--rebase-merges` and must not consume a following revision (clap
/// `short = 'r'` with optional value would eat `A` in `rebase -i -r A main`).
pub fn preprocess_rebase_argv(rest: &[String]) -> Vec<String> {
    // Git's parse-options reports a value-less `--exec`/`-x` at the end of the command line as
    // `option 'exec' requires a value` / `switch 'x' requires a value` and exits 128, before any
    // rebase work (t3404 'rebase -i --exec without <CMD>'). Clap's default ("a value is required
    // for '--exec <EXEC>'…") does not contain "requires a value", so detect the trailing bare flag
    // here and emit Git's wording directly.
    if let Some(last) = rest.last() {
        if last == "--exec" {
            eprintln!("error: option `exec' requires a value");
            std::process::exit(128);
        }
        if last == "-x" {
            eprintln!("error: switch `x' requires a value");
            std::process::exit(128);
        }
    }
    let mut out = Vec::new();
    let mut i = 0usize;
    while i < rest.len() {
        let arg = &rest[i];
        // Clap does not accept glued `-C<n>`; Git's rebase passes this through to the apply backend.
        // Non-numeric glued forms (e.g. `-Cnot-a-number`) must still become `-C` + value so
        // `validate_compat_syntax` reports Git's "switch `C' expects a numerical value".
        if arg.len() > 2 && arg.starts_with("-C") && !arg.starts_with("--") {
            let suffix = &arg[2..];
            if !suffix.is_empty() {
                if suffix.chars().all(|c| c.is_ascii_digit()) {
                    out.push("-C".to_string());
                    out.push(suffix.to_string());
                    i += 1;
                    continue;
                }
                out.push("-C".to_string());
                out.push(suffix.to_string());
                i += 1;
                continue;
            }
        }
        if arg.len() > 2 && arg.starts_with("-X") && !arg.starts_with("--") {
            out.push("-X".to_string());
            out.push(arg[2..].to_string());
            i += 1;
            continue;
        }
        if arg.len() > 2 && arg.starts_with("-x") && !arg.starts_with("--") {
            out.push("-x".to_string());
            out.push(arg[2..].to_string());
            i += 1;
            continue;
        }
        if arg == "-r" {
            if i + 1 < rest.len() {
                let next = rest[i + 1].as_str();
                if next == "rebase-cousins" || next == "no-rebase-cousins" {
                    out.push(format!("--rebase-merges={next}"));
                    i += 2;
                    continue;
                }
            }
            out.push("--rebase-merges".to_string());
            i += 1;
            continue;
        }
        if arg.len() > 2 && arg.starts_with("-r") && !arg.starts_with("--") {
            let rest_s = &arg[2..];
            if rest_s == "rebase-cousins" || rest_s == "no-rebase-cousins" {
                out.push(format!("--rebase-merges={rest_s}"));
                i += 1;
                continue;
            }
        }
        if arg.len() > 2
            && arg.starts_with('-')
            && !arg.starts_with("--")
            && arg.chars().nth(1) != Some('-')
        {
            let flags: String = arg.chars().skip(1).collect();
            let mut expanded = Vec::new();
            for ch in flags.chars() {
                match ch {
                    'i' => expanded.push("-i".to_string()),
                    'k' => expanded.push("-k".to_string()),
                    'r' => expanded.push("--rebase-merges".to_string()),
                    _ => expanded.push(format!("-{ch}")),
                }
            }
            out.extend(expanded);
        } else {
            out.push(arg.clone());
        }
        i += 1;
    }
    out
}

/// Run the `rebase` command.
pub fn run(mut args: Args) -> Result<()> {
    validate_compat_syntax(&args)?;

    if let Ok(s) = std::env::var(INTERNAL_REBASE_PICK_ENV) {
        let line_idx: usize = s
            .parse()
            .map_err(|_| anyhow::anyhow!("invalid {INTERNAL_REBASE_PICK_ENV}"))?;
        return run_internal_rebase_pick_line(line_idx);
    }

    if args.show_current_patch {
        return do_show_current_patch();
    }

    if args.keep_base > 0 && args.onto.is_some() {
        bail!("options '--keep-base' and '--onto' cannot be used together");
    }

    let mut upstream_explicit = args.upstream.is_some();

    if args.root {
        if args.keep_base > 0 {
            bail!("options '--keep-base' and '--root' cannot be used together");
        }
        // `rebase.forkPoint` may be true in config; Git only rejects the explicit CLI combination.
        if args.fork_point {
            bail!("options '--root' and '--fork-point' cannot be used together");
        }
        if args.upstream.is_some() && args.branch.is_some() {
            bail!("git rebase: too many arguments");
        }
        if args.upstream.is_some() && args.branch.is_none() {
            args.branch = args.upstream.take();
        }
    }

    if args.abort {
        return do_abort();
    }
    if args.r#continue {
        if args.no_reschedule_failed_exec || args.reschedule_failed_exec {
            eprintln!("error: unknown option `no-reschedule-failed-exec'");
            std::process::exit(129);
        }
        return do_continue();
    }
    if args.skip {
        if args.verbose {
            eprintln!("error: option --skip cannot be used with other options");
            std::process::exit(129);
        }
        return do_skip();
    }
    if args.quit {
        return do_quit();
    }
    if args.edit_todo {
        if args.no_reschedule_failed_exec || args.reschedule_failed_exec {
            eprintln!("error: unknown option `no-reschedule-failed-exec'");
            std::process::exit(129);
        }
        return do_edit_todo();
    }

    let pre_rebase_hook_second = args.branch.clone();
    let mut pre_rebase_upstream_label: Option<String> = None;

    // If a branch argument is given, checkout that branch first.
    // Resolve `upstream` before checkout: `git rebase <upstream> <branch>` uses the pre-checkout
    // meaning of `HEAD` and other relative specs.
    let mut upstream_spec_before_branch_checkout: Option<String> = None;
    if args.branch.is_some() {
        let repo = Repository::discover(None).context("not a git repository")?;
        if let Some(branch) = args.branch.as_deref() {
            if resolve_ref(&repo.git_dir, &format!("refs/heads/{branch}")).is_ok() {
                if let Some(wt_path) =
                    crate::commands::worktree_refs::branch_occupied_by_other_worktree(&repo, branch)
                        .or_else(|| branch_checked_out_in_linked_worktree(&repo, branch))
                {
                    bail!("fatal: '{branch}' is already used by worktree at '{wt_path}'");
                }
            }
        }
        let uspec = args
            .upstream
            .as_deref()
            .map(|s| if s == "-" { "@{-1}" } else { s })
            .unwrap_or("HEAD");
        pre_rebase_upstream_label = Some(uspec.to_owned());
        let uoid = resolve_revision_as_commit_without_index_dwim(&repo, uspec)
            .with_context(|| format!("bad revision '{uspec}'"))?
            .to_hex();
        upstream_spec_before_branch_checkout = Some(uoid.clone());
        args.upstream = Some(uoid);
    }
    if args.upstream.as_deref() == Some("-") {
        args.upstream = Some("@{-1}".to_owned());
    }

    // Fix up the reflog so @{-N} isn't polluted by the internal checkout.
    if let Some(ref branch) = args.branch {
        let self_exe = std::env::current_exe().context("cannot determine own executable")?;
        let status = std::process::Command::new(&self_exe)
            .arg("checkout")
            .arg("--quiet")
            .arg(branch)
            .status()
            .context("failed to checkout branch")?;
        if !status.success() {
            bail!("checkout {} failed", branch);
        }
        // Replace the checkout reflog entry with a rebase message
        let repo = Repository::discover(None).context("not a git repository")?;
        let reflog_path = repo.git_dir.join("logs/HEAD");
        if let Ok(content) = std::fs::read_to_string(&reflog_path) {
            let lines: Vec<&str> = content.lines().collect();
            if let Some(last) = lines.last() {
                if last.contains("checkout: moving from ") {
                    if let Some(tab_idx) = last.rfind('\t') {
                        let upstream_name = args.upstream.as_deref().unwrap_or("HEAD");
                        let new_line = format!(
                            "{}\trebase (start): checkout {}",
                            &last[..tab_idx],
                            upstream_name
                        );
                        let mut new_lines: Vec<String> = lines[..lines.len() - 1]
                            .iter()
                            .map(|s| s.to_string())
                            .collect();
                        new_lines.push(new_line);
                        let _ = std::fs::write(&reflog_path, new_lines.join("\n") + "\n");
                    }
                }
            }
        }
        args.branch = None;
    }

    // Default upstream to the current branch's @{upstream} whenever it is omitted (including
    // `rebase --onto <newbase>` — Git still uses the configured upstream for fork-point / merge
    // base logic; t3431).
    if args.upstream.is_none() && !args.root {
        let repo = Repository::discover(None).context("not a git repository")?;
        let head = resolve_head(&repo.git_dir)?;
        let branch_name = match &head {
            HeadState::Branch { short_name, .. } => short_name.clone(),
            _ => bail!("no upstream configured for the current branch"),
        };
        match resolve_revision(&repo, &format!("{}@{{upstream}}", branch_name)) {
            Ok(_) => {
                args.upstream = Some(format!("{}@{{upstream}}", branch_name));
                upstream_explicit = false;
            }
            Err(_) => {
                if args.onto.is_none() {
                    bail!(
                        "There is no tracking information for the current branch.\n\
                         Please specify which branch you want to rebase against."
                    );
                }
            }
        }
    }

    args.upstream_explicit = upstream_explicit;
    do_rebase(
        args,
        pre_rebase_hook_second,
        upstream_spec_before_branch_checkout,
        pre_rebase_upstream_label,
    )
}

const INTERNAL_REBASE_PICK_ENV: &str = "GRIT_INTERNAL_REBASE_PICK_LINE";
const INTERNAL_REBASE_FORCE_FF_ENV: &str = "GRIT_INTERNAL_REBASE_FORCE_REWRITE";

fn run_internal_rebase_pick_line(line_index: usize) -> Result<()> {
    let repo = Repository::discover(None).context("not a git repository")?;
    let git_dir = &repo.git_dir;
    let rb_dir =
        active_rebase_dir(git_dir).ok_or_else(|| anyhow::anyhow!("no rebase in progress"))?;
    let force_rewrite = std::env::var(INTERNAL_REBASE_FORCE_FF_ENV).ok().as_deref() == Some("1")
        || rb_dir.join("force-rewrite").exists();
    let rebase_interactive = rb_dir.join("interactive").exists();
    let todo_content = read_rebase_todo_file(&repo, &rb_dir)?;
    let todo: Vec<&str> = rebase_todo_actionable_lines(&todo_content);
    let i = line_index;
    if i >= todo.len() {
        bail!("internal rebase pick: line index out of range");
    }
    let line = todo[i];
    let step = parse_rebase_replay_step(&repo, line, rebase_interactive)?
        .ok_or_else(|| anyhow::anyhow!("malformed rebase todo line: {line}"))?;
    match step {
        RebaseReplayStep::PickLike {
            oid: commit_oid,
            cmd: todo_cmd,
            fixup_message_mode,
        } => {
            let final_fixup = is_final_fixup_in_todo(&repo, &rb_dir, &todo, i, rebase_interactive);
            let next_after = peek_next_rebase_flush_hint(&repo, &todo, i + 1, rebase_interactive);
            cherry_pick_for_rebase(
                &repo,
                &rb_dir,
                &commit_oid,
                load_rebase_backend(&rb_dir),
                todo_cmd,
                final_fixup,
                next_after,
                true,
                force_rewrite,
                fixup_message_mode,
            )
        }
        RebaseReplayStep::Edit(commit_oid) => {
            let todo_cmd = RebaseTodoCmd::Pick;
            let final_fixup = is_final_fixup_in_todo(&repo, &rb_dir, &todo, i, rebase_interactive);
            let next_after = peek_next_rebase_flush_hint(&repo, &todo, i + 1, rebase_interactive);
            cherry_pick_for_rebase(
                &repo,
                &rb_dir,
                &commit_oid,
                load_rebase_backend(&rb_dir),
                todo_cmd,
                final_fixup,
                next_after,
                false,
                force_rewrite,
                None,
            )
        }
        _ => bail!("internal rebase pick: unsupported step at line {i}"),
    }
}

fn run_rebase_pick_in_clean_child_process(
    repo: &Repository,
    line_index: usize,
    force_rewrite_commits: bool,
) -> Result<()> {
    let self_exe = std::env::current_exe().context("cannot determine grit binary path")?;
    let wt = repo.work_tree.as_deref().unwrap_or_else(|| Path::new("."));
    let mut cmd = std::process::Command::new(&self_exe);
    cmd.arg("rebase")
        .env(INTERNAL_REBASE_PICK_ENV, line_index.to_string())
        .env(
            INTERNAL_REBASE_FORCE_FF_ENV,
            if force_rewrite_commits { "1" } else { "0" },
        )
        .current_dir(wt);
    let st = cmd.status().context("spawn internal rebase pick")?;
    if !st.success() {
        bail!(
            "internal rebase pick failed with status {}",
            st.code().unwrap_or(-1)
        );
    }
    Ok(())
}

// ── Rebase state directory layout ───────────────────────────────────
//
// .git/rebase-apply/
//   head-name   — original branch ref (e.g. refs/heads/topic)
//   orig-head   — original HEAD OID before rebase
//   onto        — OID of the new base
//   todo        — remaining commit OIDs to replay, one per line
//   current     — OID of the commit currently being replayed
//   msgnum      — 1-based index of current patch
//   end         — total number of patches

fn validate_compat_syntax(args: &Args) -> Result<()> {
    if let Some(ref c) = args.context_lines {
        if c.parse::<u32>().is_err() {
            bail!("switch `C' expects a numerical value");
        }
    }
    if let Some(ref ws) = args.whitespace {
        let allowed = ["warn", "nowarn", "error", "error-all", "fix", "strip"];
        if !allowed.contains(&ws.as_str()) {
            bail!("Invalid whitespace option: '{ws}'");
        }
    }
    if let Some(ref empty) = args.empty {
        let e = empty.to_ascii_lowercase();
        if !matches!(e.as_str(), "drop" | "keep" | "stop" | "ask") {
            bail!(
                "unrecognized empty type '{empty}'; valid values are \"drop\", \"keep\", and \"stop\"."
            );
        }
    }
    for exec_cmd in &args.exec {
        if exec_cmd.is_empty() || exec_cmd.trim().is_empty() {
            bail!("empty exec command");
        }
        if exec_cmd.contains('\n') {
            bail!("exec commands cannot contain newlines");
        }
    }
    Ok(())
}

/// True when the user requested the merge-style rebase backend via flags that Git treats as merge-only.
fn merge_backend_requested_by_flags(
    args: &Args,
    config: &ConfigSet,
    want_autosquash: bool,
) -> bool {
    if args.rebase_merges.is_some() {
        return true;
    }
    if effective_rebase_merges_settings(args, config).0 {
        return true;
    }
    if args.merge || args.interactive || !args.exec.is_empty() || args.keep_empty {
        return true;
    }
    if args.empty.is_some() {
        return true;
    }
    if want_autosquash {
        return true;
    }
    if args.strategy.is_some() || !args.strategy_option.is_empty() {
        return true;
    }
    let reapply_explicit = args.reapply_cherry_picks || args.no_reapply_cherry_picks;
    if reapply_explicit && args.keep_base == 0 {
        return true;
    }
    if args.root && args.onto.is_none() {
        return true;
    }
    false
}

/// True when options force the apply backend (`git am` style), matching Git's `git_am_opts` / `--apply`.
fn apply_backend_forced(args: &Args) -> bool {
    if args.apply {
        return true;
    }
    if args.context_lines.is_some() {
        return true;
    }
    args.whitespace
        .as_deref()
        .is_some_and(|w| w.eq_ignore_ascii_case("fix") || w.eq_ignore_ascii_case("strip"))
}

/// `rebase.rebaseMerges` from config: enabled flag and whether cousins are rebased.
fn config_rebase_merges_settings(config: &ConfigSet) -> (bool, bool) {
    let Some(raw) = config.get("rebase.rebaseMerges") else {
        return (false, false);
    };
    let s = raw.trim();
    if let Ok(b) = grit_lib::config::parse_bool(s) {
        return (b, false);
    }
    if s.eq_ignore_ascii_case("rebase-cousins") {
        return (true, true);
    }
    if s.eq_ignore_ascii_case("no-rebase-cousins") {
        return (true, false);
    }
    (true, false)
}

/// Effective `--rebase-merges` / `rebase.rebaseMerges` (Git `rebase.c` semantics).
fn effective_rebase_merges_settings(args: &Args, config: &ConfigSet) -> (bool, bool) {
    let (cfg_on, cfg_cousins) = config_rebase_merges_settings(config);
    if args.no_rebase_merges {
        return (false, false);
    }
    if let Some(ref v) = args.rebase_merges {
        let v = v.as_str();
        if v == "false" {
            return (false, false);
        }
        if v.eq_ignore_ascii_case("rebase-cousins") {
            return (true, true);
        }
        if v.eq_ignore_ascii_case("no-rebase-cousins") {
            return (true, false);
        }
        // `-r` / `--rebase-merges` / `--rebase-merges=true`
        return (true, false);
    }
    (cfg_on, cfg_cousins)
}

/// Reject mixing apply-only and merge-only options (and config) the same way as upstream `git rebase`.
fn validate_apply_merge_backend_combo(
    args: &Args,
    config: &ConfigSet,
    want_autosquash: bool,
) -> Result<()> {
    let apply_forced = apply_backend_forced(args);
    if !apply_forced {
        return Ok(());
    }

    let (effective_rebase_merges, _) = effective_rebase_merges_settings(args, config);

    let update_refs_cli = if args.no_update_refs {
        Some(false)
    } else if args.update_refs {
        Some(true)
    } else {
        None
    };
    let config_update_refs = config.get_bool("rebase.updateRefs").and_then(|r| r.ok());
    let effective_update_refs = update_refs_cli.unwrap_or(config_update_refs.unwrap_or(false));

    if effective_rebase_merges && args.rebase_merges.is_none() {
        bail!(
            "apply options are incompatible with rebase.rebaseMerges.  Consider adding --no-rebase-merges"
        );
    }
    if effective_update_refs {
        bail!(
            "apply options are incompatible with rebase.updateRefs.  Consider adding --no-update-refs"
        );
    }

    let merge_requested = merge_backend_requested_by_flags(args, config, want_autosquash);
    if merge_requested {
        bail!("apply options and merge options cannot be used together");
    }

    Ok(())
}

fn validate_rebase_trailer_options(args: &Args) -> Result<()> {
    if args.trailer.is_empty() {
        return Ok(());
    }
    if apply_backend_forced(args) {
        return Err(anyhow::Error::new(ExplicitExit {
            code: 128,
            message: "fatal: --trailer requires the merge backend".to_string(),
        }));
    }
    for trailer in &args.trailer {
        if trailer.is_empty() {
            return Err(anyhow::Error::new(ExplicitExit {
                code: 128,
                message: "empty --trailer".to_string(),
            }));
        }
        let trimmed = trailer.trim_start();
        if trimmed.starts_with(':') || trimmed.starts_with('=') {
            return Err(anyhow::Error::new(ExplicitExit {
                code: 128,
                message: "missing key before separator".to_string(),
            }));
        }
    }
    Ok(())
}

fn rebase_force_rewrite_requested(args: &Args) -> bool {
    // Git's `builtin/rebase.c` sets `REBASE_FORCE` (which clears `replay.allow_ff`) for `--no-ff`,
    // `--force-rebase`, signoff, trailers, and for the date-rewriting options
    // `--committer-date-is-author-date` / `--reset-author-date` (`--ignore-date`). With those date
    // options a tree-identical pick — including a root pick after `reset [new root]` in a
    // `rebase -r` script — must be rewritten rather than fast-forwarded so the new committer/author
    // timestamp is actually applied (t3436 `--committer-date-is-author-date`/`--reset-author-date`
    // with `rebase -r`).
    args.no_ff
        || args.signoff
        || !args.trailer.is_empty()
        || args.committer_date_is_author_date
        || args.reset_author_date
}

/// Apply autosquash (`fixup!`/`squash!`/`amend!`) rearrangement to a generated `--rebase-merges`
/// todo script, mirroring Git's `todo_list_rearrange_squash` (which `complete_action` runs over the
/// FULL merge script — picks AND merges — before the sequence editor opens).
///
/// Each `pick <oid> # …` and `merge … <oid> # …` line carries a commit; we match a fixup/squash
/// pick to its target by commit subject (or, for a one-token suffix, by commit name), convert the
/// matched line's command to `fixup`/`squash`, and move it directly after its target (threading
/// multiple fixups via a per-target tail). Non-commit lines (`label`/`reset`/`exec`/blank/comment)
/// are preserved in place. t3430 'with --autosquash and --exec'.
fn rearrange_autosquash_merge_script(repo: &Repository, script: &str) -> Result<String> {
    let lines: Vec<&str> = script.lines().collect();
    let n = lines.len();

    // Per-line: the commit oid (if this line is a pick/merge with a commit) and its subject.
    let mut line_oid: Vec<Option<ObjectId>> = vec![None; n];
    let mut subjects: Vec<Option<String>> = vec![None; n];
    // subject -> first line index that introduced it (target lookup by title).
    let mut subject_to_index: HashMap<String, usize> = HashMap::new();
    // commit oid -> line index (target lookup by commit name).
    let mut oid_to_index: HashMap<ObjectId, usize> = HashMap::new();
    let mut next: Vec<isize> = vec![-1; n];
    let mut tail: Vec<isize> = vec![-1; n];
    // The new command keyword for a rearranged line ("fixup"/"squash"); None = leave the line as-is.
    let mut new_cmd: Vec<Option<&'static str>> = vec![None; n];
    let mut rearranged = false;

    for i in 0..n {
        let line = lines[i].trim();
        // Determine the commit oid for pick/merge lines. `pick <abbrev> # subj` and the merge forms
        // `merge -C <abbrev> <label> # subj` / `merge -c <abbrev> …` both expose a commit oid.
        let oid = pick_or_merge_commit_oid(repo, line);
        let Some(oid) = oid else {
            continue;
        };
        let obj = repo.odb.read(&oid)?;
        let commit = parse_commit(&obj.data)?;
        let subj = format_autosquash_subject_for_match(&commit.message);
        line_oid[i] = Some(oid);
        subjects[i] = Some(subj.clone());

        let mut target_idx: Option<usize> = None;
        if let Some(rest) = skip_fixupish_prefix(&subj) {
            let key = strip_fixupish_chain(rest).trim();
            if let Some(&idx) = subject_to_index.get(key) {
                target_idx = Some(idx);
            } else if !key.contains(' ') {
                if let Ok(t) = resolve_revision(repo, key) {
                    if t != oid {
                        if let Some(&idx) = oid_to_index.get(&t) {
                            target_idx = Some(idx);
                        }
                    }
                }
            }
            if target_idx.is_none() {
                // Prefix match against an earlier subject (Git's final fallback).
                for (i2, s) in subjects.iter().enumerate().take(i) {
                    if let Some(s) = s {
                        if s.starts_with(key) {
                            target_idx = Some(i2);
                            break;
                        }
                    }
                }
            }
        }

        if let Some(i2) = target_idx {
            rearranged = true;
            if subj.starts_with("fixup!") || subj.starts_with("amend!") {
                new_cmd[i] = Some("fixup");
            } else {
                new_cmd[i] = Some("squash");
            }
            if tail[i2] < 0 {
                next[i] = next[i2];
                next[i2] = i as isize;
            } else {
                let t = tail[i2] as usize;
                next[i] = next[t];
                next[t] = i as isize;
            }
            tail[i2] = i as isize;
        } else {
            subject_to_index.entry(subj).or_insert(i);
            oid_to_index.entry(oid).or_insert(i);
        }
    }

    if !rearranged {
        return Ok(script.to_owned());
    }

    let mut out: Vec<String> = Vec::with_capacity(n);
    for i in 0..n {
        // Rearranged (fixup/squash) lines are emitted only by following a target's `next` chain.
        if new_cmd[i].is_some() {
            continue;
        }
        let mut cur = i as isize;
        while cur >= 0 {
            let ci = cur as usize;
            if let Some(cmd) = new_cmd[ci] {
                // Rewrite the leading command keyword (`pick`/`p`) to `fixup`/`squash`.
                out.push(rewrite_todo_command_keyword(lines[ci], cmd));
            } else {
                out.push(lines[ci].to_owned());
            }
            cur = next[ci];
        }
    }
    let mut body = out.join("\n");
    if script.ends_with('\n') {
        body.push('\n');
    }
    Ok(body)
}

/// The commit oid named by a `pick`/`merge -C`/`merge -c` todo line, or `None` for other lines.
fn pick_or_merge_commit_oid(repo: &Repository, line: &str) -> Option<ObjectId> {
    let t = line.trim();
    let mut parts = t.split_whitespace();
    let cmd = parts.next()?.to_ascii_lowercase();
    match cmd.as_str() {
        "pick" | "p" => {
            let tok = parts.next()?;
            resolve_revision(repo, tok).ok()
        }
        "merge" | "m" => {
            // `merge [-C <commit> | -c <commit>] <label> [# oneline]`. Only `-C`/`-c` name a commit.
            let opt = parts.next()?;
            if opt == "-C" || opt == "-c" {
                let tok = parts.next()?;
                resolve_revision(repo, tok).ok()
            } else {
                None
            }
        }
        _ => None,
    }
}

/// Replace the leading command word of a todo line with `new_cmd`, preserving the rest verbatim.
fn rewrite_todo_command_keyword(line: &str, new_cmd: &str) -> String {
    let leading_ws_len = line.len() - line.trim_start().len();
    let (lead, rest) = line.split_at(leading_ws_len);
    let mut it = rest.splitn(2, char::is_whitespace);
    let _old = it.next().unwrap_or("");
    let tail = it.next().unwrap_or("");
    if tail.is_empty() {
        format!("{lead}{new_cmd}")
    } else {
        format!("{lead}{new_cmd} {tail}")
    }
}

fn is_commit_tree_unchanged(repo: &Repository, oid: &ObjectId) -> Result<bool> {
    const GIT_EMPTY_TREE_HEX: &str = "4b825dc642cb6eb9a060e54bf8d69288fbee4904";
    let obj = repo.odb.read(oid)?;
    let commit = parse_commit(&obj.data)?;
    let parent_tree = if let Some(p) = commit.parents.first() {
        let pobj = repo.odb.read(p)?;
        let pc = parse_commit(&pobj.data)?;
        pc.tree
    } else {
        ObjectId::from_hex(GIT_EMPTY_TREE_HEX).map_err(|e| anyhow::anyhow!("{e}"))?
    };
    Ok(commit.tree == parent_tree)
}

/// Read (without consuming) the `obstructed-pick` marker and report whether it names `current_oid`.
///
/// `cherry_pick_for_rebase` writes this marker when a step aborts because materializing it would
/// overwrite an untracked working tree file: the step is never applied, so the worktree still equals
/// HEAD and a later `--continue` would otherwise mistake it for an empty no-op. A matching marker
/// means the step must instead be re-run so it actually produces its commit and records the real
/// old->new mapping for the post-rewrite hook (t5407 "git rebase with failed pick"). Because only
/// obstructions write the marker, resolved-conflict, empty-pick and submodule paths never match.
fn rebase_pick_was_obstructed(rb_dir: &Path, current_oid: &ObjectId) -> bool {
    fs::read_to_string(rb_dir.join("obstructed-pick"))
        .ok()
        .and_then(|s| {
            s.lines()
                .next()
                .map(str::trim)
                .and_then(|h| ObjectId::from_hex(h).ok())
        })
        .is_some_and(|oid| &oid == current_oid)
}

/// Git creates a synthetic empty root commit when `rebase --root` runs without `--onto`
/// (`commit_tree` with the empty tree and no parents).
fn synthetic_root_onto_commit_oid(repo: &Repository, config: &ConfigSet) -> Result<ObjectId> {
    const GIT_EMPTY_TREE_HEX: &str = "4b825dc642cb6eb9a060e54bf8d69288fbee4904";
    let tree = ObjectId::from_hex(GIT_EMPTY_TREE_HEX).map_err(|e| anyhow::anyhow!("{e}"))?;
    let now = time::OffsetDateTime::now_utc();
    let author = resolve_identity(config, "AUTHOR")?;
    let committer = resolve_identity(config, "COMMITTER")?;
    let commit_data = CommitData {
        tree,
        parents: Vec::new(),
        author: format_ident(&author, now),
        committer: format_ident(&committer, now),
        author_raw: Vec::new(),
        committer_raw: Vec::new(),
        encoding: None,
        message: String::new(),
        raw_message: None,
    };
    let bytes = serialize_commit(&commit_data);
    Ok(repo.odb.write(ObjectKind::Commit, &bytes)?)
}

/// Validates `rebase.instructionFormat` similarly to Git's `get_commit_format` for todo generation.
fn validate_rebase_instruction_format(config: &ConfigSet) -> Result<()> {
    let Some(fmt) = config.get("rebase.instructionFormat") else {
        return Ok(());
    };
    if fmt.trim().is_empty() {
        return Ok(());
    }
    if !fmt.contains('%') {
        bail!("invalid --pretty format: {fmt}");
    }
    let mut chars = fmt.chars().peekable();
    while let Some(ch) = chars.next() {
        if ch != '%' {
            continue;
        }
        let Some(spec) = chars.next() else {
            bail!("invalid --pretty format: {fmt}");
        };
        match spec {
            'n' | '%' => {}
            'H' | 'h' | 'T' | 't' | 's' | 'e' | 'b' | 'B' | 'N' => {}
            'P' | 'p' => {}
            'w' | 'W' => {}
            'a' | 'c' => {
                let Some(second) = chars.peek().copied() else {
                    bail!("invalid --pretty format: {fmt}");
                };
                match second {
                    'n' | 'e' | 'd' | 'i' => {
                        chars.next();
                    }
                    'r' if spec == 'a' || spec == 'c' => {
                        chars.next();
                    }
                    _ => bail!("invalid --pretty format: {fmt}"),
                }
            }
            '(' => {
                while let Some(c) = chars.next() {
                    if c == ')' {
                        break;
                    }
                }
            }
            _ => bail!("invalid --pretty format: {fmt}"),
        }
    }
    Ok(())
}

/// Minimum abbreviated-commit length for the rebase todo, from `core.abbrev` (default 7; `auto`/empty
/// -> 7; `false`/`no`/`off`/`0` -> full 40; numeric clamped to `[4, 40]`).
fn rebase_core_abbrev_len(config: &ConfigSet) -> usize {
    match config.get("core.abbrev") {
        None => 7,
        Some(raw) => match raw.trim().to_ascii_lowercase().as_str() {
            "false" | "no" | "off" | "0" => 40,
            "auto" | "" => 7,
            v => v.parse::<usize>().map(|n| n.clamp(4, 40)).unwrap_or(7),
        },
    }
}

fn format_rebase_todo_line(
    repo: &Repository,
    oid: &ObjectId,
    cmd: RebaseTodoCmd,
    config: &ConfigSet,
    short_oid_field: bool,
) -> Result<String> {
    let obj = repo.odb.read(oid)?;
    let commit = parse_commit(&obj.data)?;
    let subj = commit.message.lines().next().unwrap_or("");
    let empty = is_commit_tree_unchanged(repo, oid).unwrap_or(false);
    let abbrev_cmds = rebase_abbreviate_commands(config);
    let cmd_word = rebase_todo_command_for_display_abbrev(cmd, &commit, abbrev_cmds);
    let oid_field = if short_oid_field {
        // Honour `core.abbrev` for the minimum length; `abbreviate_object_id` still extends further
        // on collision (t3404 "respect core.abbrev" / "short commit ID collide").
        abbreviate_object_id(repo, *oid, rebase_core_abbrev_len(config))?
    } else {
        oid.to_hex()
    };
    let mut line = match config.get("rebase.instructionFormat") {
        None => format!("{cmd_word} {oid_field} # {subj}"),
        Some(raw) if raw.trim().is_empty() => format!("{cmd_word} {oid_field} # {subj}"),
        Some(tmpl) => {
            let mut t = tmpl.clone();
            if !t.starts_with('#') {
                t = format!("# {t}");
            }
            let rest = crate::commands::show::format_commit_placeholder(&t, oid, &commit);
            format!("{cmd_word} {oid_field} {rest}")
        }
    };
    if empty {
        line.push_str(" # empty");
    }
    Ok(line)
}

/// `rebase.abbreviateCommands` (Git `TODO_LIST_ABBREVIATE_CMDS`): when true the generated todo uses
/// single-letter command keywords (`p`/`f`/`s`/`r`/`x`/…). Default false.
fn rebase_abbreviate_commands(config: &ConfigSet) -> bool {
    config
        .get_bool("rebase.abbreviateCommands")
        .and_then(|r| r.ok())
        .unwrap_or(false)
}

#[derive(Debug, Default)]
struct RebaseMergeLabelState {
    commit_to_label: HashMap<ObjectId, String>,
    used_labels: HashSet<String>,
    max_label_length: usize,
}

fn rebase_max_label_length(config: &ConfigSet) -> usize {
    config
        .get("rebase.maxLabelLength")
        .or_else(|| config.get("rebase.maxlabellength"))
        .and_then(|s| s.trim().parse::<usize>().ok())
        .unwrap_or(usize::MAX)
}

fn branch_tip_names_by_oid(repo: &Repository) -> HashMap<ObjectId, String> {
    let mut m: HashMap<ObjectId, String> = HashMap::new();
    let Ok(refs) = list_refs(&repo.git_dir, "refs/heads/") else {
        return m;
    };
    for (name, oid) in refs {
        let short = name.strip_prefix("refs/heads/").unwrap_or(&name).to_owned();
        m.entry(oid).or_insert(short);
    }
    m
}

fn merge_subject_label_from_oneline(oneline: &str) -> String {
    // Mirror Git's `make_script_with_merges` `label_from_message`. The input is the bare merge
    // subject (Git pretty-prints `# %s`, then strips the comment prefix).
    let rest = oneline.trim_start_matches('#').trim_start();
    // `# Merge <anything> '<label>' …` → text inside the first pair of single quotes after
    // "Merge " (e.g. `Merge commit '<oid>' into labels` → `<oid>`; `Merge branch 'topic'` →
    // `topic`). Git uses `skip_prefix("# Merge ")` then the first `'`…`'` pair.
    if let Some(after_merge) = rest.strip_prefix("Merge ") {
        if let Some(open) = after_merge.find('\'') {
            let after_open = &after_merge[open + 1..];
            if let Some(end) = after_open.find('\'') {
                return after_open[..end].to_owned();
            }
        }
    }
    // `# Merge pull request … from <label>` → text after " from ".
    if let Some(i) = rest.find("Merge pull request ") {
        if let Some(from) = rest[i..].find(" from ") {
            return rest[i + from + " from ".len()..].trim().to_owned();
        }
    }
    // Otherwise the whole subject is the label seed.
    rest.trim().to_owned()
}

fn sanitize_merge_label(base: &str, max_len: usize) -> String {
    // Mirror Git's `label_oid` truncation (sequencer.c). The buffer length is measured in BYTES
    // (`buf->len`), and a non-ASCII UTF-8 character is kept whole only if appending all of its bytes
    // does not exceed `max_len` (`buf->len + (p - label) > max_len` → break). Git's loop guard is
    // `buf->len + 1 < max_len`, so it stops once one more byte would reach `max_len`. Using the
    // character's BYTE length (not its display width) is essential: e.g. `0123456789 我 123` truncates
    // to `0123456789-我` at maxLabelLength=14 (10 + 1 + 3 = 14, accepted) but to `0123456789-` at 13
    // (10 + 1 + 3 = 14 > 13, the 3-byte `我` is rejected). t3430 'truncate label names'.
    let mut out = String::new();
    for ch in base.chars() {
        if out.len() + 1 >= max_len {
            break;
        }
        if ch.is_ascii_alphanumeric() {
            out.push(ch);
        } else if !ch.is_ascii() {
            let nbytes = ch.len_utf8();
            if out.len() + nbytes > max_len {
                break;
            }
            out.push(ch);
        } else if !out.is_empty() && !out.ends_with('-') {
            out.push('-');
        }
    }
    if out.is_empty() {
        out.push_str("rev-unknown");
    }
    out
}

fn ensure_unique_label_name(name: String, used: &mut HashSet<String>) -> String {
    if name == "#"
        || name.len() == 40 && name.bytes().all(|b| b.is_ascii_hexdigit())
        || used.contains(&name)
    {
        let mut n = 2u32;
        loop {
            let candidate = format!("{name}-{n}");
            if !used.contains(&candidate) {
                used.insert(candidate.clone());
                return candidate;
            }
            n += 1;
        }
    }
    used.insert(name.clone());
    name
}

fn label_oid_for_merge_script(
    oid: &ObjectId,
    hint: Option<&str>,
    state: &mut RebaseMergeLabelState,
    repo: &Repository,
) -> Result<String> {
    if let Some(existing) = state.commit_to_label.get(oid) {
        return Ok(existing.clone());
    }
    let name = if let Some(h) = hint.filter(|s| !s.is_empty()) {
        let sanitized = sanitize_merge_label(h, state.max_label_length);
        ensure_unique_label_name(sanitized, &mut state.used_labels)
    } else {
        let abbrev = abbreviate_object_id(repo, *oid, 7)?;
        let mut candidate = abbrev.clone();
        let full = oid.to_hex();
        while state.used_labels.contains(&candidate) && candidate.len() < full.len() {
            candidate.push(full.chars().nth(candidate.len()).unwrap_or('0'));
        }
        if state.used_labels.contains(&candidate) {
            let mut n = 2u32;
            loop {
                let c = format!("{abbrev}-{n}");
                if !state.used_labels.contains(&c) {
                    candidate = c;
                    break;
                }
                n += 1;
            }
        }
        state.used_labels.insert(candidate.clone());
        candidate
    };
    state.commit_to_label.insert(*oid, name.clone());
    Ok(name)
}

fn commits_for_rebase_merge_walk(
    repo: &Repository,
    head_oid: ObjectId,
    upstream_oid: ObjectId,
    filter_cherry_equivalents: bool,
) -> Result<(Vec<ObjectId>, HashSet<ObjectId>)> {
    if filter_cherry_equivalents {
        // Use `--cherry-mark` only (not `--cherry-pick`): the walk must retain every commit so the
        // merge-script generator can decide per-commit. Cherry-equivalent commits are dropped there
        // only when non-empty (`generate_rebase_merge_script`); empty commits are never dropped by
        // cherry detection (all empty commits share the empty patch-id), so `--keep-empty` keeps
        // them even when already in upstream (t3421 --rebase-merges --keep-empty).
        let bases = merge_bases_first_vs_rest(repo, upstream_oid, &[head_oid])?;
        let negative: Vec<String> = bases.iter().map(|b| b.to_hex()).collect();
        let result = rev_list(
            repo,
            &[upstream_oid.to_hex(), head_oid.to_hex()],
            &negative,
            &RevListOptions {
                cherry_mark: true,
                right_only: true,
                left_right: true,
                symmetric_left: Some(upstream_oid),
                symmetric_right: Some(head_oid),
                ordering: OrderingMode::Topo,
                reverse: true,
                ..Default::default()
            },
        )
        .map_err(|e| anyhow::anyhow!("{e}"))?;
        let cherry_equivalent = result.cherry_equivalent.clone();
        let mut stream = result.commits;
        stream.reverse();
        return Ok((stream, cherry_equivalent));
    }

    let result = rev_list(
        repo,
        &[head_oid.to_hex()],
        &[upstream_oid.to_hex()],
        &RevListOptions {
            ordering: OrderingMode::Topo,
            reverse: true,
            ..Default::default()
        },
    )
    .map_err(|e| anyhow::anyhow!("{e}"))?;
    Ok((result.commits, HashSet::new()))
}

/// Subject text for a `reset <label> # <subject>` comment line. Extracts the trailing subject from
/// the `pick <hash> # <subject>` form produced by [`format_rebase_todo_line`], so the comment shows
/// the human-readable subject without re-encoding the abbreviated hash already implied by the label.
fn reset_comment_subject(repo: &Repository, oid: &ObjectId, config: &ConfigSet) -> Result<String> {
    let fmt = format_rebase_todo_line(repo, oid, RebaseTodoCmd::Pick, config, true)?;
    if let Some(idx) = fmt.find(" # ") {
        Ok(fmt[idx + 3..].to_owned())
    } else {
        let obj = repo.odb.read(oid)?;
        let commit = parse_commit(&obj.data)?;
        Ok(commit.message.lines().next().unwrap_or("").to_owned())
    }
}

fn generate_rebase_merge_script(
    repo: &Repository,
    head_oid: ObjectId,
    upstream_oid: ObjectId,
    onto_oid: ObjectId,
    filter_cherry_equivalents: bool,
    rebase_cousins: bool,
    root_with_onto: bool,
    keep_empty: bool,
    config: &ConfigSet,
) -> Result<String> {
    validate_rebase_instruction_format(config)?;
    let max_label_length = rebase_max_label_length(config);
    let mut label_state = RebaseMergeLabelState {
        max_label_length,
        ..Default::default()
    };
    label_state.used_labels.insert("onto".to_owned());
    // Git labels the BOTTOM rev of the `upstream...head` symmetric walk as "onto"
    // (sequencer.c: `revs->cmdline.rev[0].flags & BOTTOM`). For `base...head` that bottom is the
    // merge-base of upstream and head, which differs from the `--onto` target when upstream is not
    // an ancestor of head. The reset-target lookup in the third phase relies on this: a boundary
    // that equals the merge-base resolves to "onto" (→ `reset onto`), while a true cousin boundary
    // gets a fresh `label_oid` (→ `reset <abbrev> # <subj>`). t3421 keep-empty resets to onto;
    // t3430 'do not rebase cousins' resets to the branch point.
    let onto_label_oid = merge_bases_first_vs_rest(repo, upstream_oid, &[head_oid])?
        .into_iter()
        .next()
        .unwrap_or(onto_oid);
    label_state
        .commit_to_label
        .insert(onto_label_oid, "onto".to_owned());

    let (walk_order, cherry_equiv) =
        commits_for_rebase_merge_walk(repo, head_oid, upstream_oid, filter_cherry_equivalents)?;
    let mut interesting: HashSet<ObjectId> = HashSet::new();
    let mut commit_to_todo_line: HashMap<ObjectId, String> = HashMap::new();
    let decorations = branch_tip_names_by_oid(repo);

    for &oid in &walk_order {
        interesting.insert(oid);
        let obj = repo.odb.read(&oid)?;
        let commit = parse_commit(&obj.data)?;
        let empty = is_commit_tree_unchanged(repo, &oid).unwrap_or(false);
        if !empty && cherry_equiv.contains(&oid) {
            continue;
        }
        if empty && !keep_empty {
            continue;
        }
        let oneline = format_rebase_todo_line(repo, &oid, RebaseTodoCmd::Pick, config, true)?;
        if commit.parents.len() <= 1 {
            commit_to_todo_line.insert(oid, oneline);
            continue;
        }
        let merge_c = abbreviate_object_id(repo, oid, 7)?;
        let mut merge_line = format!("merge -C {merge_c}");
        // Git's `make_script_with_merges` label hint for a merged tip: the branch ref decoration
        // (`refs/heads/<name>`) if present, otherwise a label derived from THIS merge's subject
        // (`label_from_message` — strips `Merge branch '<x>'`/`Merge pull request ... from <x>`,
        // else the whole subject). `label_oid` only uses this hint for *interesting* (rebased)
        // parents; uninteresting ones fall back to an abbreviated hash. t3430 'truncate label
        // names' expects `label 0123456789-我` from the subject `0123456789 我 123`.
        // `oneline` is the formatted `pick <hash> # <subject>` line; the label hint derives from the
        // bare subject (text after ` # `), matching Git which pretty-prints the merge with `# %s`.
        let merge_subject = oneline
            .find(" # ")
            .map(|idx| &oneline[idx + 3..])
            .unwrap_or_else(|| commit.message.lines().next().unwrap_or(""));
        let subject_label = merge_subject_label_from_oneline(merge_subject);
        for p in commit.parents.iter().skip(1) {
            let tip_name = decorations
                .get(p)
                .map(String::as_str)
                .or_else(|| (!subject_label.is_empty()).then_some(subject_label.as_str()));
            let lbl = label_oid_for_merge_script(p, tip_name, &mut label_state, repo)?;
            merge_line.push(' ');
            merge_line.push_str(&lbl);
        }
        // Append ` # <subject>` exactly once (no trailing space before the comment).
        if let Some(idx) = oneline.find(" # ") {
            merge_line.push_str(&oneline[idx..]);
        } else {
            let subj = commit.message.lines().next().unwrap_or("");
            merge_line.push_str(&format!(" # {subj}"));
        }
        commit_to_todo_line.insert(oid, merge_line);
    }

    let mut child_seen: HashSet<ObjectId> = HashSet::new();
    for &oid in &walk_order {
        let obj = match repo.odb.read(&oid) {
            Ok(o) => o,
            Err(_) => continue,
        };
        let commit = match parse_commit(&obj.data) {
            Ok(c) => c,
            Err(_) => continue,
        };
        for p in &commit.parents {
            if interesting.contains(p) && !child_seen.insert(*p) {
                let _ =
                    label_oid_for_merge_script(p, Some("branch-point"), &mut label_state, repo)?;
            }
        }
    }

    let mut tips: Vec<ObjectId> = Vec::new();
    let mut seen_tip: HashSet<ObjectId> = HashSet::new();
    for &oid in walk_order.iter().rev() {
        if !interesting.contains(&oid) {
            continue;
        }
        let obj = match repo.odb.read(&oid) {
            Ok(o) => o,
            Err(_) => continue,
        };
        let commit = match parse_commit(&obj.data) {
            Ok(c) => c,
            Err(_) => continue,
        };
        if commit.parents.len() > 1 {
            for p in commit.parents.iter().skip(1) {
                if interesting.contains(p) && seen_tip.insert(*p) {
                    tips.push(*p);
                }
            }
        }
    }
    // Git adds the branch tip (HEAD of the rebased branch) as an implicit tip — not
    // `walk_order.last()` (that is oldest-first when using `reverse` + topo).
    if interesting.contains(&head_oid) && seen_tip.insert(head_oid) {
        tips.push(head_oid);
    }

    let mut out = String::new();
    out.push_str("label onto\n");
    let mut shown: HashSet<ObjectId> = HashSet::new();

    for tip in tips {
        if shown.contains(&tip) {
            continue;
        }
        if let Some(lbl) = label_state.commit_to_label.get(&tip) {
            out.push('\n');
            out.push_str("# Branch ");
            out.push_str(lbl);
            out.push('\n');
        } else {
            out.push('\n');
        }

        // Match `make_script_with_merges` in Git's `sequencer.c`: walk first-parent links while the
        // commit is still "interesting" and not yet emitted on another tip's chain. The commit
        // that stops the walk (already `shown`, outside `interesting`, or absent) becomes the
        // `reset` target — e.g. `main`'s chain is H→E→D and stops at C already labeled from
        // `second`, yielding `reset branch-point # C`.
        let mut chain: Vec<ObjectId> = Vec::new();
        let mut cur = tip;
        let mut stopped_at_root = false;
        loop {
            if !interesting.contains(&cur) || shown.contains(&cur) {
                break;
            }
            let obj = repo.odb.read(&cur)?;
            let c = parse_commit(&obj.data)?;
            chain.push(cur);
            if c.parents.is_empty() {
                stopped_at_root = true;
                break;
            }
            cur = c.parents[0];
        }

        if chain.is_empty() {
            continue;
        }

        let boundary = if stopped_at_root { None } else { Some(cur) };

        if stopped_at_root {
            out.push_str(if rebase_cousins || root_with_onto {
                "reset onto\n"
            } else {
                "reset [new root]\n"
            });
        } else if let Some(b) = boundary {
            // Mirror Git's `make_script_with_merges` (sequencer.c) exactly. The first-parent walk
            // stopped at `b` because it is no longer interesting or was already shown. Pick the
            // reset target: prefer an existing label (the merge-base is pre-labeled "onto", branch
            // points are "branch-point"); otherwise, when *not* rebasing cousins, synthesize a label
            // from `b`'s abbreviated hash via `label_oid(oid, NULL)`. Cousins mode leaves an
            // unlabeled boundary as `None`, falling back to `reset onto`.
            let to: Option<String> = if let Some(lbl) = label_state.commit_to_label.get(&b) {
                Some(lbl.clone())
            } else if !rebase_cousins {
                Some(label_oid_for_merge_script(
                    &b,
                    None,
                    &mut label_state,
                    repo,
                )?)
            } else {
                None
            };
            match to {
                None => out.push_str("reset onto\n"),
                Some(ref lbl) if lbl == "onto" => out.push_str("reset onto\n"),
                Some(lbl) => {
                    // Git's `reset <label> # <subject>` comment uses the commit subject only, not
                    // the abbreviated hash (which is already encoded in the label). t3430 expects
                    // `reset branch-point # C`, not `reset branch-point # <hash> # C`.
                    let subj = reset_comment_subject(repo, &b, config)?;
                    out.push_str(&format!("reset {lbl} # {subj}\n"));
                }
            }
        }

        for &coid in chain.iter().rev() {
            if let Some(line) = commit_to_todo_line.get(&coid) {
                out.push_str(line);
                out.push('\n');
            }
            if let Some(lbl) = label_state.commit_to_label.get(&coid) {
                out.push_str(&format!("label {lbl}\n"));
            }
            shown.insert(coid);
        }
    }

    Ok(out)
}

/// A single entry in a generated interactive todo, before help text is appended.
///
/// `Commit` items participate in autosquash matching; `UpdateRef` / `RefComment` items are
/// branch decorations inserted by `--update-refs` and are inert during the rearrange (they
/// hold their original array position, exactly like Git's `TODO_UPDATE_REF` / `TODO_COMMENT`
/// items in `sequencer.c`).
#[derive(Debug, Clone)]
enum TodoBuildItem {
    /// A `pick`/`fixup`/`squash` step for a commit.
    Commit(ObjectId, RebaseTodoCmd),
    /// An `update-ref <ref>` step tracking a branch tip.
    UpdateRef(String),
    /// A `# Ref <ref> checked out at <path>` comment (branch is checked out elsewhere).
    RefComment(String),
    /// An `exec <command>` step inserted by `--exec`/`-x` after a pick (or merge) chain.
    Exec(String),
}

/// Insert `exec` items after every pick/merge chain, mirroring Git's `todo_list_add_exec_commands`:
/// fixup/squash chains count as part of the preceding pick, so the exec commands are inserted only
/// once the next non-fixup command (or end of list) is reached. The decoration items
/// (`UpdateRef`/`RefComment`) are inert and do not reset the pending insert.
/// Insert `exec <cmd>` lines into a generated `--rebase-merges` todo script after every `pick`/
/// `merge` (treating a fixup/squash chain as part of its pick — exec lands after the chain), and
/// append trailing exec commands if the last command was a pick/merge. Mirrors Git's
/// `todo_list_add_exec_commands`, which runs over the whole merge script after autosquash and before
/// the editor. The inserted `exec` lines are then driven by the normal replay `RebaseReplayStep::Exec`
/// path (with `done`-recording, reschedule, etc.). t3430 'truncate label names', 'with --autosquash
/// and --exec'.
fn insert_exec_into_merge_script(script: &str, exec_cmds: &[String]) -> String {
    if exec_cmds.is_empty() {
        return script.to_owned();
    }
    let trailing_newline = script.ends_with('\n');
    let mut out: Vec<String> = Vec::new();
    let mut pending = false;
    let flush = |out: &mut Vec<String>| {
        for c in exec_cmds {
            out.push(format!("exec {c}"));
        }
    };
    for line in script.lines() {
        let t = line.trim();
        let first = t.split_whitespace().next().map(str::to_ascii_lowercase);
        let is_comment_or_blank = t.is_empty() || t.starts_with('#');
        let is_fixup = matches!(first.as_deref(), Some("fixup" | "f" | "squash" | "s"));
        let is_pick_or_merge = matches!(first.as_deref(), Some("pick" | "p" | "merge" | "m"));
        // Flush the pending exec before the next non-fixup, non-comment command (Git treats
        // comments/blank lines as transparent: they don't end a fixup chain).
        if pending && !is_fixup && !is_comment_or_blank {
            flush(&mut out);
            pending = false;
        }
        out.push(line.to_owned());
        if is_pick_or_merge {
            pending = true;
        }
    }
    if pending {
        flush(&mut out);
    }
    let mut body = out.join("\n");
    if trailing_newline {
        body.push('\n');
    }
    body
}

fn insert_exec_commands(items: Vec<TodoBuildItem>, exec_cmds: &[String]) -> Vec<TodoBuildItem> {
    if exec_cmds.is_empty() {
        return items;
    }
    let mut out: Vec<TodoBuildItem> = Vec::with_capacity(items.len() + exec_cmds.len());
    let mut insert = false;
    for item in items {
        // Git's `is_fixup(command)` is true for both fixup and squash; the pending exec is flushed
        // before any non-fixup command.
        let is_fixup = matches!(
            item,
            TodoBuildItem::Commit(_, RebaseTodoCmd::Fixup | RebaseTodoCmd::Squash)
        );
        // Only a `pick` (Git also: `merge`) arms the pending insert. `reword` does not.
        let is_pick = matches!(item, TodoBuildItem::Commit(_, RebaseTodoCmd::Pick));
        // Decoration items (update-ref / ref-comment) are not real commands and must not trigger an
        // early exec insert; keep the pending state across them so exec lands after the pick chain.
        let is_decoration = matches!(
            item,
            TodoBuildItem::UpdateRef(_) | TodoBuildItem::RefComment(_)
        );
        if insert && !is_fixup && !is_decoration {
            for cmd in exec_cmds {
                out.push(TodoBuildItem::Exec(cmd.clone()));
            }
            insert = false;
        }
        out.push(item);
        if is_pick {
            insert = true;
        }
    }
    if insert {
        for cmd in exec_cmds {
            out.push(TodoBuildItem::Exec(cmd.clone()));
        }
    }
    out
}

/// Build the autosquash-rearranged todo item list, optionally interleaving `--update-refs`
/// decorations, mirroring Git's ordering in `complete_action` (`sequencer.c`).
///
/// Git inserts the `update-ref`/comment items *before* the autosquash rearrange
/// ([`todo_list_add_update_ref_commands`] runs before [`todo_list_rearrange_squash`]). The
/// rearrange threads `fixup!`/`squash!` commits directly after their target `pick` via a
/// `next`/`tail` linked list keyed on the *combined* item array, while the decoration items
/// keep their original positions. This reproduces the exact interleaving the t3404 update-refs
/// tests expect.
///
/// # Parameters
/// - `repo`: repository for reading commit objects / resolving fixup targets.
/// - `git_dir`: git directory, used to enumerate branch decorations per commit.
/// - `oids`: the commits to rebase, in todo order (oldest first).
/// - `autosquash`: whether to perform `fixup!`/`squash!` rearrangement.
/// - `update_refs`: whether to emit `update-ref` decoration items.
///
/// # Returns
/// The ordered list of [`TodoBuildItem`]s ready to be formatted into a todo body.
///
/// # Errors
/// Propagates object-read and ref-enumeration failures.
fn build_autosquash_with_update_refs(
    repo: &Repository,
    git_dir: &Path,
    oids: &[ObjectId],
    autosquash: bool,
    update_refs: bool,
) -> Result<Vec<TodoBuildItem>> {
    // Phase 1: build the flat item array with update-ref decorations interleaved after each
    // commit (Git's `todo_list_add_update_ref_commands`, which runs before autosquash).
    let mut items: Vec<TodoBuildItem> = Vec::new();
    // commit_item_index[k] = array index of the k-th commit (the k-th element of `oids`).
    let mut commit_item_index: Vec<usize> = Vec::with_capacity(oids.len());
    for &oid in oids {
        commit_item_index.push(items.len());
        items.push(TodoBuildItem::Commit(oid, RebaseTodoCmd::Pick));
        if update_refs {
            for refname in update_ref_decorations(repo, git_dir, oid)? {
                items.push(refname);
            }
        }
    }

    if !autosquash {
        return Ok(items);
    }

    let n = items.len();
    let mut subjects: Vec<Option<String>> = vec![None; n];
    let mut next: Vec<isize> = vec![-1; n];
    let mut tail: Vec<isize> = vec![-1; n];
    let mut subject_to_index: HashMap<String, usize> = HashMap::new();
    let mut oid_to_index: HashMap<ObjectId, usize> = HashMap::new();
    for (&idx, &oid) in commit_item_index.iter().zip(oids.iter()) {
        oid_to_index.insert(oid, idx);
    }
    let mut rearranged = false;

    for &i in &commit_item_index {
        let TodoBuildItem::Commit(oid_i, _) = items[i].clone() else {
            continue;
        };
        let obj = repo.odb.read(&oid_i)?;
        let commit = parse_commit(&obj.data)?;
        let subj = format_autosquash_subject_for_match(&commit.message);
        subjects[i] = Some(subj.clone());

        let mut target_idx: Option<usize> = None;
        if let Some(rest) = skip_fixupish_prefix(&subj) {
            let key = strip_fixupish_chain(rest).trim();
            if subject_to_index.contains_key(key) {
                target_idx = subject_to_index.get(key).copied();
            } else if !key.contains(' ') {
                if let Ok(oid) = resolve_revision(repo, key) {
                    if oid != oid_i {
                        if let Some(&idx) = oid_to_index.get(&oid) {
                            target_idx = Some(idx);
                        }
                    }
                }
            }
            if target_idx.is_none() {
                for &j in &commit_item_index {
                    if j >= i {
                        break;
                    }
                    if let Some(ref sj) = subjects[j] {
                        if sj.starts_with(key) {
                            target_idx = Some(j);
                            break;
                        }
                    }
                }
            }
        }

        if let Some(i2) = target_idx {
            rearranged = true;
            let t = subj.trim_start();
            let cmd = if t.starts_with("fixup!") || t.starts_with("amend!") {
                RebaseTodoCmd::Fixup
            } else {
                RebaseTodoCmd::Squash
            };
            if let TodoBuildItem::Commit(_, c) = &mut items[i] {
                *c = cmd;
            }
            if tail[i2] < 0 {
                next[i] = next[i2];
                next[i2] = i as isize;
            } else {
                let t = tail[i2] as usize;
                next[i] = next[t];
                next[t] = i as isize;
            }
            tail[i2] = i as isize;
        }

        if target_idx.is_none() && !subject_to_index.contains_key(&subj) {
            subject_to_index.insert(subj.clone(), i);
        }
    }

    if !rearranged {
        return Ok(items);
    }

    // Phase 2: walk the linked list to produce the final ordering. Non-fixup items are emitted
    // in array order; a `pick` whose `next` points at threaded `fixup`/`squash` commits emits
    // those immediately after it.
    let mut ordered: Vec<TodoBuildItem> = Vec::with_capacity(n);
    for i in 0..n {
        if matches!(items[i], TodoBuildItem::Commit(_, RebaseTodoCmd::Fixup))
            || matches!(items[i], TodoBuildItem::Commit(_, RebaseTodoCmd::Squash))
        {
            continue;
        }
        let mut cur = Some(i);
        while let Some(ci) = cur {
            ordered.push(items[ci].clone());
            let nxt = next[ci];
            cur = if nxt >= 0 { Some(nxt as usize) } else { None };
        }
    }
    debug_assert_eq!(ordered.len(), n);
    Ok(ordered)
}

/// Enumerate the `--update-refs` decoration items for a commit, in Git's decoration order.
///
/// Git's `add_decorations_to_list` walks `get_name_decoration`, which lists branches in the
/// reverse of ref-iteration (sorted) order because each is prepended; the current `HEAD`
/// branch is skipped (it is updated by the default rebase behaviour). Branches checked out in
/// another worktree become `# Ref ... checked out` comments rather than `update-ref` steps.
///
/// # Parameters
/// - `repo`: repository handle for worktree-occupancy checks.
/// - `git_dir`: git directory for ref enumeration.
/// - `oid`: the commit whose branch decorations are wanted.
///
/// # Returns
/// The decoration items (already typed as [`TodoBuildItem::UpdateRef`] / `RefComment`).
///
/// # Errors
/// Propagates ref-enumeration failures.
fn update_ref_decorations(
    repo: &Repository,
    git_dir: &Path,
    oid: ObjectId,
) -> Result<Vec<TodoBuildItem>> {
    let head_branch = current_head_branch_ref(git_dir);
    let mut refs = branch_refs_at_commit(git_dir, oid, None)?;
    // `branch_refs_at_commit` returns refs sorted ascending; Git's decoration list is the
    // reverse (prepend semantics), so iterate in reverse.
    refs.reverse();
    let mut out = Vec::new();
    // Git's `add_decorations_to_list` coalesces the current branch with the synthetic `HEAD`
    // decoration: the current branch is normally skipped because the default rebase updates it.
    // That skip is consumed by the *first* decoration only — once a higher-sorting branch
    // decoration is emitted ahead of the current branch (which happens when that branch is checked
    // out in another worktree and so still decorates the tip), the current branch is no longer
    // treated as the HEAD decoration and instead becomes a `# Ref ... checked out` comment (it is
    // checked out in the current worktree). t3400 "rebase --update-ref with core.commentChar and
    // branch on worktree" exercises this: both the worktree branch and the rebased branch appear
    // as comments.
    let mut emitted_decoration_before_head = false;
    for refname in refs {
        let is_head_branch = head_branch.as_deref() == Some(refname.as_str());
        if is_head_branch && !emitted_decoration_before_head {
            continue;
        }
        if let Some(short) = refname.strip_prefix("refs/heads/") {
            if let Some(path) =
                crate::commands::worktree_refs::branch_occupied_any_worktree(repo, short)
            {
                out.push(TodoBuildItem::RefComment(format!(
                    "Ref {refname} checked out at '{path}'"
                )));
                emitted_decoration_before_head = true;
                continue;
            }
        }
        if is_head_branch {
            // The current branch is never written as an `update-ref` line; it is always either
            // skipped (above) or commented (it is checked out in the current worktree).
            continue;
        }
        out.push(TodoBuildItem::UpdateRef(refname));
        emitted_decoration_before_head = true;
    }
    Ok(out)
}

/// Resolve the symbolic ref `HEAD` points at, as a full `refs/heads/<name>` ref name.
///
/// Returns `None` if `HEAD` is detached or unborn / unreadable.
fn current_head_branch_ref(git_dir: &Path) -> Option<String> {
    let head = fs::read_to_string(git_dir.join("HEAD")).ok()?;
    let head = head.trim();
    head.strip_prefix("ref: ").map(ToOwned::to_owned)
}

fn rearrange_autosquash(
    repo: &Repository,
    oids: Vec<ObjectId>,
) -> Result<Vec<(ObjectId, RebaseTodoCmd)>> {
    let n = oids.len();
    if n == 0 {
        return Ok(Vec::new());
    }

    let mut subjects: Vec<Option<String>> = vec![None; n];
    let mut cmds: Vec<RebaseTodoCmd> = vec![RebaseTodoCmd::Pick; n];
    let mut next: Vec<isize> = vec![-1; n];
    let mut tail: Vec<isize> = vec![-1; n];

    let mut subject_to_index: HashMap<String, usize> = HashMap::new();
    let mut oid_to_index: HashMap<ObjectId, usize> = HashMap::new();
    let mut rearranged = false;
    for (idx, oid) in oids.iter().copied().enumerate() {
        oid_to_index.insert(oid, idx);
    }

    for i in 0..n {
        let obj = repo.odb.read(&oids[i])?;
        let commit = parse_commit(&obj.data)?;
        let subj = format_autosquash_subject_for_match(&commit.message);
        subjects[i] = Some(subj.clone());

        let mut target_idx: Option<usize> = None;
        if let Some(rest) = skip_fixupish_prefix(&subj) {
            let key = strip_fixupish_chain(rest).trim();
            if subject_to_index.contains_key(key) {
                target_idx = subject_to_index.get(key).copied();
            } else if !key.contains(' ') {
                if let Ok(oid) = resolve_revision(repo, key) {
                    // A branch can point at the fixup commit itself (e.g. `fixup! self-cycle` on
                    // branch `self-cycle`); Git does not treat that as a valid autosquash target.
                    if oid != oids[i] {
                        if let Some(&idx) = oid_to_index.get(&oid) {
                            target_idx = Some(idx);
                        }
                    }
                }
            }
            if target_idx.is_none() {
                for j in 0..i {
                    if let Some(ref sj) = subjects[j] {
                        if sj.starts_with(key) {
                            target_idx = Some(j);
                            break;
                        }
                    }
                }
            }
        }
        if let Some(i2) = target_idx {
            rearranged = true;
            // Git uses `starts_with(subject, "fixup!")` / `"amend!"` vs else → squash, so
            // `squash! squash! …` becomes squash commands (not fixup).
            let t = subj.trim_start();
            cmds[i] = if t.starts_with("fixup!") || t.starts_with("amend!") {
                RebaseTodoCmd::Fixup
            } else {
                RebaseTodoCmd::Squash
            };
            if tail[i2] < 0 {
                next[i] = next[i2];
                next[i2] = i as isize;
            } else {
                let t = tail[i2] as usize;
                next[i] = next[t];
                next[t] = i as isize;
            }
            tail[i2] = i as isize;
        }

        if target_idx.is_none() && !subject_to_index.contains_key(&subj) {
            subject_to_index.insert(subj.clone(), i);
        }
    }

    if !rearranged {
        return Ok(oids.into_iter().map(|o| (o, RebaseTodoCmd::Pick)).collect());
    }

    let mut ordered: Vec<(ObjectId, RebaseTodoCmd)> = Vec::with_capacity(n);
    for i in 0..n {
        if matches!(cmds[i], RebaseTodoCmd::Fixup | RebaseTodoCmd::Squash) {
            continue;
        }
        let mut cur = Some(i);
        while let Some(ci) = cur {
            ordered.push((oids[ci], cmds[ci]));
            let nxt = next[ci];
            cur = if nxt >= 0 { Some(nxt as usize) } else { None };
        }
    }
    debug_assert_eq!(ordered.len(), n);
    Ok(ordered)
}

fn parse_todo_line_with_repo(
    repo: Option<&Repository>,
    line: &str,
) -> Result<Option<(ObjectId, RebaseTodoCmd)>> {
    let t = line.trim();
    if t.is_empty() || t.starts_with('#') {
        return Ok(None);
    }
    let mut parts = t.split_whitespace();
    let Some(cmd_word) = parts.next() else {
        return Ok(None);
    };
    let Some(cmd) = RebaseTodoCmd::parse_word(cmd_word) else {
        return Ok(None);
    };
    let Some(hex) = parts.next() else {
        return Ok(None);
    };
    if hex.len() < 4 || hex.len() > 40 || !hex.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Ok(None);
    }
    let oid = if hex.len() == 40 {
        ObjectId::from_hex(hex)?
    } else {
        let Some(r) = repo else {
            return Ok(None);
        };
        resolve_revision(r, hex).with_context(|| format!("todo: bad revision '{hex}'"))?
    };
    Ok(Some((oid, cmd)))
}

/// Normalized rebase todo step for [`replay_remaining`].
#[derive(Debug)]
enum RebaseReplayStep {
    PickLike {
        oid: ObjectId,
        cmd: RebaseTodoCmd,
        fixup_message_mode: Option<FixupMessageMode>,
    },
    Exec(String),
    Edit(ObjectId),
    Noop,
    /// Interactive-only: pause before the next command (`break` / `b`).
    Break,
    Label(String),
    Reset(String),
    MergeReuseMessage {
        merge_oid: ObjectId,
        merge_args: String,
        edit_message: bool,
    },
    MergePlain {
        merge_args: String,
    },
    /// `update-ref <ref>`: record the current HEAD as the ref's pending new value.
    UpdateRef(String),
}

fn parse_rebase_replay_step(
    repo: &Repository,
    line: &str,
    interactive: bool,
) -> Result<Option<RebaseReplayStep>> {
    let t = line.trim();
    if t.is_empty() || t.starts_with('#') {
        return Ok(None);
    }
    if interactive {
        return Ok(
            parse_interactive_rebase_todo_line(repo, line)?.map(|p| match p {
                ParsedRebaseTodoLine::Commit {
                    oid,
                    cmd,
                    fixup_message_mode,
                } => RebaseReplayStep::PickLike {
                    oid,
                    cmd,
                    fixup_message_mode,
                },
                ParsedRebaseTodoLine::Exec(s) => RebaseReplayStep::Exec(s),
                ParsedRebaseTodoLine::Edit(oid) => RebaseReplayStep::Edit(oid),
                ParsedRebaseTodoLine::MergeReuseMessage {
                    merge_oid,
                    merge_args,
                    edit_message,
                } => RebaseReplayStep::MergeReuseMessage {
                    merge_oid,
                    merge_args,
                    edit_message,
                },
                ParsedRebaseTodoLine::MergePlain { merge_args } => {
                    RebaseReplayStep::MergePlain { merge_args }
                }
                ParsedRebaseTodoLine::Noop => RebaseReplayStep::Noop,
                ParsedRebaseTodoLine::Break => RebaseReplayStep::Break,
                ParsedRebaseTodoLine::Label(s) => RebaseReplayStep::Label(s),
                ParsedRebaseTodoLine::Reset(s) => RebaseReplayStep::Reset(s),
                ParsedRebaseTodoLine::UpdateRef(s) => RebaseReplayStep::UpdateRef(s),
            }),
        );
    }
    if let Ok(Some(prefix)) = parse_interactive_rebase_todo_line(repo, line) {
        match prefix {
            ParsedRebaseTodoLine::Exec(s) => return Ok(Some(RebaseReplayStep::Exec(s))),
            ParsedRebaseTodoLine::Edit(oid) => return Ok(Some(RebaseReplayStep::Edit(oid))),
            ParsedRebaseTodoLine::MergeReuseMessage {
                merge_oid,
                merge_args,
                edit_message,
            } => {
                return Ok(Some(RebaseReplayStep::MergeReuseMessage {
                    merge_oid,
                    merge_args,
                    edit_message,
                }));
            }
            ParsedRebaseTodoLine::UpdateRef(s) => {
                return Ok(Some(RebaseReplayStep::UpdateRef(s)));
            }
            ParsedRebaseTodoLine::Commit { .. } => {}
            ParsedRebaseTodoLine::Noop
            | ParsedRebaseTodoLine::Break
            | ParsedRebaseTodoLine::Label(_)
            | ParsedRebaseTodoLine::Reset(_)
            | ParsedRebaseTodoLine::MergePlain { .. } => {}
        }
    }
    Ok(
        parse_todo_line_with_repo(Some(repo), line)?.map(|(oid, cmd)| RebaseReplayStep::PickLike {
            oid,
            cmd,
            fixup_message_mode: None,
        }),
    )
}

/// One line from an interactive rebase todo (after the user edits it).
#[derive(Debug)]
enum ParsedRebaseTodoLine {
    /// `pick` / `p` / `fixup` / `f` / `squash` / `s` with an object id.
    Commit {
        oid: ObjectId,
        cmd: RebaseTodoCmd,
        fixup_message_mode: Option<FixupMessageMode>,
    },
    /// `exec <shell command>` (rest of line after the command word).
    Exec(String),
    /// `edit` / `e` with an object id.
    Edit(ObjectId),
    Noop,
    /// `break` / `b` — stop for `rebase --continue`.
    Break,
    Label(String),
    Reset(String),
    /// `update-ref <ref>` — track a branch tip to update at rebase completion.
    UpdateRef(String),
    /// `merge -C <ref> ...` — replay merge commit message from `merge_oid`, merge heads from the rest.
    MergeReuseMessage {
        merge_oid: ObjectId,
        merge_args: String,
        edit_message: bool,
    },
    /// `merge <labels...> # subject` (no `-C`/`-c`).
    MergePlain {
        merge_args: String,
    },
}

fn parse_interactive_rebase_todo_line(
    repo: &Repository,
    line: &str,
) -> Result<Option<ParsedRebaseTodoLine>> {
    let t = line.trim();
    if t.is_empty() || t.starts_with('#') {
        return Ok(None);
    }
    let mut parts = t.split_whitespace();
    let Some(cmd_word) = parts.next() else {
        return Ok(None);
    };
    let cmd_lower = cmd_word.to_ascii_lowercase();
    if cmd_lower == "exec" || cmd_lower == "x" {
        let rest = t[cmd_word.len()..].trim_start();
        return Ok(Some(ParsedRebaseTodoLine::Exec(rest.to_owned())));
    }
    if cmd_lower == "break" || cmd_lower == "b" {
        return Ok(Some(ParsedRebaseTodoLine::Break));
    }
    if cmd_lower == "edit" || cmd_lower == "e" {
        let Some(hex) = parts.next() else {
            bail!("malformed rebase todo line: {line}");
        };
        let oid = if hex.len() == 40 && hex.bytes().all(|b| b.is_ascii_hexdigit()) {
            ObjectId::from_hex(hex)?
        } else {
            resolve_revision(repo, hex).with_context(|| format!("todo: bad revision '{hex}'"))?
        };
        return Ok(Some(ParsedRebaseTodoLine::Edit(oid)));
    }
    if cmd_lower == "noop" || cmd_lower == "drop" || cmd_lower == "d" {
        return Ok(Some(ParsedRebaseTodoLine::Noop));
    }
    if cmd_lower == "update-ref" || cmd_lower == "u" {
        let rest = t[cmd_word.len()..].trim_start();
        let refname = rest.split_whitespace().next().unwrap_or("");
        if refname.is_empty() {
            bail!("malformed update-ref todo line: {line}");
        }
        return Ok(Some(ParsedRebaseTodoLine::UpdateRef(refname.to_owned())));
    }
    if cmd_lower == "break" || cmd_lower == "b" {
        return Ok(Some(ParsedRebaseTodoLine::Break));
    }
    if cmd_lower == "label" || cmd_lower == "l" {
        let rest = t[cmd_word.len()..].trim_start();
        if rest.is_empty() || rest == "#" {
            bail!("malformed label todo line: {line}");
        }
        let name = rest.split_whitespace().next().unwrap_or("").to_owned();
        if name == "#" {
            bail!("illegal label name: '{name}'");
        }
        return Ok(Some(ParsedRebaseTodoLine::Label(name)));
    }
    if cmd_lower == "reset" || cmd_lower == "t" {
        let rest = t[cmd_word.len()..].trim_start();
        if rest.is_empty() {
            bail!("malformed reset todo line: {line}");
        }
        return Ok(Some(ParsedRebaseTodoLine::Reset(rest.to_owned())));
    }
    if cmd_lower == "merge" || cmd_lower == "m" {
        let mut rest = parts;
        let Some(flag) = rest.next() else {
            bail!("malformed merge todo line: {line}");
        };
        if flag.eq_ignore_ascii_case("-C") || flag.eq_ignore_ascii_case("-c") {
            // Distinguish `-C` vs `-c` by actual case (`eq_ignore_ascii_case` conflates them).
            let edit_message = flag.as_bytes().get(1) == Some(&b'c');
            let Some(merge_ref) = rest.next() else {
                bail!("merge -C missing commit: {line}");
            };
            let merge_oid = resolve_revision(repo, merge_ref)
                .with_context(|| format!("todo merge: bad revision '{merge_ref}'"))?;
            let tail: Vec<&str> = rest.take_while(|tok| *tok != "#").collect();
            let merge_args = tail.join(" ");
            return Ok(Some(ParsedRebaseTodoLine::MergeReuseMessage {
                merge_oid,
                merge_args,
                edit_message,
            }));
        }
        let merge_args = t[cmd_word.len()..].trim_start().to_owned();
        if merge_args.is_empty() {
            bail!("malformed merge todo line: {line}");
        }
        return Ok(Some(ParsedRebaseTodoLine::MergePlain { merge_args }));
    }
    if let Some(cmd) = RebaseTodoCmd::parse_word(&cmd_lower) {
        let mut fixup_message_mode = None;
        let mut next = parts.next();
        if cmd == RebaseTodoCmd::Fixup {
            if let Some(flag) = next {
                if flag.eq_ignore_ascii_case("-C") || flag.eq_ignore_ascii_case("-c") {
                    fixup_message_mode = Some(if flag.as_bytes().get(1) == Some(&b'c') {
                        FixupMessageMode::EditCommit
                    } else {
                        FixupMessageMode::UseCommit
                    });
                    next = parts.next();
                } else if flag.len() > 2 && flag.starts_with("-C") {
                    fixup_message_mode = Some(FixupMessageMode::UseCommit);
                    next = Some(&flag[2..]);
                } else if flag.len() > 2 && flag.starts_with("-c") {
                    fixup_message_mode = Some(FixupMessageMode::EditCommit);
                    next = Some(&flag[2..]);
                }
            }
        }
        let Some(hex) = next else {
            bail!("malformed rebase todo line: {line}");
        };
        let oid = if hex.len() == 40 && hex.bytes().all(|b| b.is_ascii_hexdigit()) {
            ObjectId::from_hex(hex)?
        } else {
            resolve_revision(repo, hex).with_context(|| format!("todo: bad revision '{hex}'"))?
        };
        return Ok(Some(ParsedRebaseTodoLine::Commit {
            oid,
            cmd,
            fixup_message_mode,
        }));
    }
    bail!("unknown rebase todo command: {line}");
}

/// For post-rewrite pending flush: next line's command category (Git `peek_command` + `is_fixup`).
fn first_interactive_todo_pick_oid(repo: &Repository, todo_lines: &[&str]) -> Option<ObjectId> {
    for line in todo_lines {
        let t = line.trim();
        if t.is_empty() || rebase_todo_line_is_comment(t) {
            continue;
        }
        if let Ok(Some(RebaseReplayStep::PickLike {
            oid,
            cmd: RebaseTodoCmd::Pick,
            ..
        })) = parse_rebase_replay_step(repo, t, true)
        {
            return Some(oid);
        }
    }
    None
}

fn peek_next_rebase_flush_hint(
    repo: &Repository,
    todo_lines: &[&str],
    start: usize,
    interactive: bool,
) -> Option<RebaseTodoCmd> {
    let mut j = start;
    while j < todo_lines.len() {
        let t = todo_lines[j].trim();
        if t.is_empty() || t.starts_with('#') {
            j += 1;
            continue;
        }
        if let Ok(Some(step)) = parse_rebase_replay_step(repo, t, interactive) {
            return Some(match step {
                RebaseReplayStep::PickLike { cmd, .. } => cmd,
                RebaseReplayStep::Exec(_)
                | RebaseReplayStep::Edit(_)
                | RebaseReplayStep::MergeReuseMessage { .. }
                | RebaseReplayStep::MergePlain { .. } => RebaseTodoCmd::Pick,
                RebaseReplayStep::Noop
                | RebaseReplayStep::Break
                | RebaseReplayStep::Label(_)
                | RebaseReplayStep::Reset(_)
                | RebaseReplayStep::UpdateRef(_) => RebaseTodoCmd::Pick,
            });
        }
        j += 1;
    }
    None
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum RebaseMergeReuseOutcome {
    Completed,
    Conflict,
    /// Merge aborted before `MERGE_HEAD` (e.g. untracked file would be overwritten); retry on
    /// `rebase --continue` after the user fixes the worktree.
    Blocked,
}

fn resolve_merge_head_token(repo: &Repository, token: &str) -> Result<ObjectId> {
    commit_oid_for_rebase_label(repo, token)
}

/// Split a commit ident (`Name <email> <secs> <tz>`) into `(name, email, "<secs> <tz>")` for
/// re-exporting as `GIT_AUTHOR_NAME`/`GIT_AUTHOR_EMAIL`/`GIT_AUTHOR_DATE`.
fn author_fields_from_ident(ident: &str) -> Option<(String, String, String)> {
    let lt = ident.find('<')?;
    let gt = ident[lt + 1..].find('>')? + lt + 1;
    let name = ident[..lt].trim().to_owned();
    let email = ident[lt + 1..gt].trim().to_owned();
    let date = ident[gt + 1..].trim().to_owned();
    Some((name, email, date))
}

fn run_rebase_merge_subprocess(
    repo: &Repository,
    git_dir: &Path,
    merge_oid: &ObjectId,
    merge_args: &str,
    edit_message: bool,
) -> Result<std::process::ExitStatus> {
    let merge_commit_oid = peel_to_commit_for_merge_base(repo, *merge_oid)?;
    let merge_obj = repo.odb.read(&merge_commit_oid)?;
    let merge_commit = parse_commit(&merge_obj.data)?;
    let msg_path = git_dir.join("rebase-merge-merge-msg");
    fs::write(&msg_path, &merge_commit.message)?;
    // An octopus merge step (more than one merge head → 3+ parents) cannot be replayed by the plain
    // recursive `merge`: Git's `do_merge` runs `git merge -s octopus --no-edit --no-ff --no-log
    // --no-stat -F <msg> <tips...>` and pulls the AUTHOR from the saved author-script
    // (`read_env_script`). The `--no-log`/`--no-stat` are essential — a bare octopus `merge -F`
    // ignores the message file and auto-generates `Merge branches '…'`. Mirror that here (t3430
    // 'octopus merges' checks both the `Tüntenfüsch` message and the `Hank` author).
    let (merge_heads, _) = parse_merge_todo_arg_list(merge_args);
    let is_octopus = merge_heads.len() > 1;
    let self_exe = std::env::current_exe().context("cannot determine grit binary path")?;
    let mut cmd = std::process::Command::new(&self_exe);
    // `--allow-unrelated-histories`: a `merge` step may join independent root commits (Git's
    // `do_merge` performs an in-process recursive merge with no unrelated-histories guard), e.g.
    // `merge first-branch` into a `[new root]` (t3430 'root commits').
    if is_octopus {
        // `--no-log`/`--no-stat` make the octopus merge honor `-F` instead of auto-generating
        // `Merge branches '…'`. The `-s octopus` (or an explicit override) is added by the
        // forwarding block below so a `git rebase -ir -s <strategy>` octopus still works.
        cmd.args([
            "merge",
            "--no-ff",
            "--no-log",
            "--no-stat",
            "--allow-unrelated-histories",
            "-F",
        ])
        .arg(msg_path.as_os_str());
    } else {
        cmd.args(["merge", "--no-ff", "--allow-unrelated-histories", "-F"])
            .arg(msg_path.as_os_str());
    }
    // Forward the rebase's merge strategy and strategy options to the child merge so a
    // `merge`/`merge -C`/`merge -c` honours `git rebase -s <strategy> -X<opt>` (t3430
    // 'merge -c rewords when a strategy is given', '--rebase-merges with strategies').
    let rb_dir = active_rebase_dir(git_dir);
    let mut explicit_strategy = false;
    if let Some(rb_dir) = rb_dir.as_deref() {
        if let Ok(strategy_raw) = fs::read_to_string(rb_dir.join("strategy")) {
            let strategy = strategy_raw.trim();
            if !strategy.is_empty() {
                cmd.arg("-s").arg(strategy);
                explicit_strategy = true;
            }
        }
        if let Ok(opts) = fs::read_to_string(rb_dir.join("strategy-opts")) {
            for encoded in opts.lines() {
                if encoded.is_empty() {
                    continue;
                }
                if let Some(opt) = hex_decode_string(encoded) {
                    let opt = opt.strip_prefix("--").unwrap_or(&opt);
                    cmd.arg(format!("-X{opt}"));
                }
            }
        }
    }
    // Octopus with no explicit strategy uses the `octopus` strategy (Git's `do_merge`).
    if is_octopus && !explicit_strategy {
        cmd.arg("-s").arg("octopus");
    }
    for tok in merge_args.split_whitespace() {
        let oid = resolve_merge_head_token(repo, tok)?;
        cmd.arg(oid.to_hex());
    }
    if edit_message {
        cmd.arg("--edit");
    }
    // Child `grit merge` must not inherit in-progress rebase env (e.g. `GIT_INDEX_FILE` from
    // tests or nested tooling); a polluted index breaks merge and surfaces bogus parse errors.
    let cmd = cmd.env_clear().envs(std::env::vars().filter(|(k, _)| {
        !k.starts_with("GIT_") || k == "GIT_CONFIG_NOSYSTEM" || k == "GIT_CONFIG_PARAMETERS"
    }));
    if let Ok(h) = std::env::var("HOME") {
        cmd.env("HOME", h);
    }
    if let Ok(p) = std::env::var("PATH") {
        cmd.env("PATH", p);
    }
    // Octopus: Git's `do_merge` sets the author from the saved author-script (`read_env_script`)
    // so the recreated octopus merge keeps the ORIGINAL merge commit's author (t3430 'octopus
    // merges' asserts `%an` == "Hank"). A 2-parent merge instead amends in-process and preserves the
    // author via `run_git_commit`; the child `grit merge` strips GIT_AUTHOR_* for octopus, so set
    // them explicitly here.
    if is_octopus {
        if let Some((name, email, date)) = author_fields_from_ident(&merge_commit.author) {
            cmd.env("GIT_AUTHOR_NAME", name);
            cmd.env("GIT_AUTHOR_EMAIL", email);
            if !date.is_empty() {
                cmd.env("GIT_AUTHOR_DATE", date);
            }
        }
    }
    let output = cmd
        .current_dir(repo.work_tree.as_deref().unwrap_or_else(|| Path::new(".")))
        .output()
        .context("run grit merge for rebase merge -C")?;
    let _ = fs::remove_file(&msg_path);
    let status = output.status;
    if !status.success() {
        let err_out = String::from_utf8_lossy(&output.stderr);
        let std_out = String::from_utf8_lossy(&output.stdout);
        if !err_out.trim().is_empty() {
            eprint!("{err_out}");
        }
        if !std_out.trim().is_empty() {
            eprint!("{std_out}");
        }
    }
    Ok(status)
}

fn rewrite_merge_head_for_replay_opts(
    repo: &Repository,
    git_dir: &Path,
    rb_dir: &Path,
    template_merge_oid: &ObjectId,
) -> Result<()> {
    let opts = load_rebase_replay_commit_opts(rb_dir);
    if !opts.committer_date_is_author_date && !opts.ignore_date {
        return Ok(());
    }
    let head_oid = resolve_head(git_dir)?
        .oid()
        .cloned()
        .ok_or_else(|| anyhow::anyhow!("HEAD has no OID after merge"))?;
    let head_obj = repo.odb.read(&head_oid)?;
    let head_commit = parse_commit(&head_obj.data)?;
    if head_commit.parents.len() < 2 {
        return Ok(());
    }
    // `merge -C <ref>` may name an annotated tag; peel to the underlying commit before parsing,
    // otherwise `parse_commit` on a tag object dies with "commit missing author header".
    let template_commit_oid = peel_to_commit_for_merge_base(repo, *template_merge_oid)?;
    let template_obj = repo.odb.read(&template_commit_oid)?;
    let template = parse_commit(&template_obj.data)?;
    let config = ConfigSet::load(Some(git_dir), true)?;
    let now = time::OffsetDateTime::now_utc();
    let author = rebase_replayed_author_line(&template.author, opts, now)?;
    let committer = rebase_replayed_committer_line(&config, &template.author, opts, now)?;
    let (message, encoding, raw_message) =
        finalize_message_for_commit_encoding(head_commit.message.clone(), &config);
    let commit_data = CommitData {
        tree: head_commit.tree,
        parents: head_commit.parents.clone(),
        author,
        committer,
        author_raw: Vec::new(),
        committer_raw: Vec::new(),
        encoding,
        message,
        raw_message,
    };
    let bytes = serialize_commit(&commit_data);
    let new_oid = repo.odb.write(ObjectKind::Commit, &bytes)?;
    fs::write(git_dir.join("HEAD"), format!("{}\n", new_oid.to_hex()))?;
    Ok(())
}

/// If a `merge -C <orig>` would reproduce the original merge commit exactly, return that original
/// commit so the caller can fast-forward to it (git `do_merge` can_fast_forward). The conditions:
/// current HEAD equals `orig`'s first parent, and the `merge_args` heads equal `orig`'s remaining
/// parents in order with the same count.
fn rebase_merge_fast_forward_target(
    repo: &Repository,
    git_dir: &Path,
    merge_oid: &ObjectId,
    merge_args: &str,
) -> Result<Option<ObjectId>> {
    let orig_commit_oid = peel_to_commit_for_merge_base(repo, *merge_oid)?;
    let orig_obj = repo.odb.read(&orig_commit_oid)?;
    let orig = parse_commit(&orig_obj.data)?;
    if orig.parents.len() < 2 {
        return Ok(None);
    }
    let head_oid = match resolve_head(git_dir)?.oid().cloned() {
        Some(oid) => oid,
        None => return Ok(None),
    };
    if orig.parents[0] != head_oid {
        return Ok(None);
    }
    let (heads, _oneline) = parse_merge_todo_arg_list(merge_args);
    if heads.len() != orig.parents.len() - 1 {
        return Ok(None);
    }
    for (tok, &parent) in heads.iter().zip(orig.parents.iter().skip(1)) {
        let head_commit = match resolve_merge_head_token(repo, tok) {
            Ok(oid) => oid,
            Err(_) => return Ok(None),
        };
        if head_commit != parent {
            return Ok(None);
        }
    }
    Ok(Some(orig_commit_oid))
}

/// Point HEAD at `target` and sync the index/worktree to its tree (a fast-forward during rebase).
fn reset_rebase_head_to_commit(repo: &Repository, git_dir: &Path, target: &ObjectId) -> Result<()> {
    let head_state = resolve_head(git_dir)?;
    reset_worktree_to_commit(repo, git_dir, &head_state, *target, "merge")
}

/// Reword a merge commit that was just fast-forwarded into (`merge -c` with `fast_forward_edit`).
/// Opens the commit-message editor seeded with the original merge message, then rewrites the merge
/// commit in place (same tree and parents, edited message) and advances HEAD. Mirrors Git's
/// `do_merge` `fast_forward_edit` (run_git_commit with AMEND_MSG|EDIT_MSG).
fn reword_fast_forwarded_merge(
    repo: &Repository,
    git_dir: &Path,
    rb_dir: &Path,
    ff_oid: &ObjectId,
    author_template_oid: Option<&ObjectId>,
) -> Result<()> {
    let config = ConfigSet::load(Some(git_dir), true)?;
    let ff_commit_oid = peel_to_commit_for_merge_base(repo, *ff_oid)?;
    let obj = repo.odb.read(&ff_commit_oid)?;
    let merge_commit = parse_commit(&obj.data)?;
    // Author for the reworded merge. For a fast-forward reword the fast-forwarded commit *is* the
    // original merge, so its author line is already correct. For a non-fast-forward `merge -c`, the
    // merge was produced by a child `grit merge` (with GIT_AUTHOR_* stripped), so its author can be
    // wrong; replay the ORIGINAL merge commit's author instead (Git keeps the rebased merge's
    // author). t3430 'merge -c ... reloads todo-list' compares author lines.
    let author = match author_template_oid {
        Some(oid) => {
            let aoid = peel_to_commit_for_merge_base(repo, *oid)?;
            let aobj = repo.odb.read(&aoid)?;
            parse_commit(&aobj.data)?.author
        }
        None => merge_commit.author.clone(),
    };
    let template = merge_commit.message.clone();
    let after_editor = run_commit_editor_for_reword(repo, git_dir, &template)?;
    let cleaned =
        message_from_reword_editor(&after_editor, rebase_commit_msg_cleanup(&config), &config)?;
    let cleaned = run_reword_commit_msg_hook(repo, git_dir, cleaned, &config)?;
    let (message, encoding, raw_message) = finalize_message_for_commit_encoding(cleaned, &config);
    let now = time::OffsetDateTime::now_utc();
    let opts = load_rebase_replay_commit_opts(rb_dir);
    let author = rebase_replayed_author_line(&author, opts, now)?;
    let committer = rebase_replayed_committer_line(&config, &author, opts, now)?;
    let (author_raw, committer_raw_final) =
        grit_lib::commit_encoding::identity_raw_for_serialized_commit(
            &encoding, &author, &committer,
        );
    let commit_data = CommitData {
        tree: merge_commit.tree,
        parents: merge_commit.parents.clone(),
        author,
        committer,
        author_raw,
        committer_raw: committer_raw_final,
        encoding,
        message: message.clone(),
        raw_message,
    };
    let head_before = resolve_head(git_dir)?
        .oid()
        .cloned()
        .unwrap_or_else(diff::zero_oid);
    let bytes = serialize_commit(&commit_data);
    let new_oid = write_replayed_commit(repo, rb_dir, &config, &commit_data.committer, bytes)?;
    fs::write(git_dir.join("HEAD"), format!("{}\n", new_oid.to_hex()))?;
    let ra = load_rebase_reflog_action(rb_dir);
    let ident = reflog_identity(repo);
    let subject = message.lines().next().unwrap_or("");
    let msg = format!("{ra} (reword): {subject}");
    let _ = append_reflog(git_dir, "HEAD", &head_before, &new_oid, &ident, &msg, false);
    Ok(())
}

fn rebase_merge_reuse_message(
    repo: &Repository,
    git_dir: &Path,
    rb_dir: &Path,
    merge_oid: &ObjectId,
    merge_args: &str,
    edit_message: bool,
    next_after_line: Option<RebaseTodoCmd>,
    allow_ff: bool,
) -> Result<RebaseMergeReuseOutcome> {
    // Git `do_merge` fast-forward: if `allow_ff` (no `--force-rebase`/`--no-ff`) and the original
    // merge's first parent already equals HEAD and every merge head equals the original merge's
    // remaining parents (in order, same count), reuse the ORIGINAL merge commit instead of building
    // a new one. This keeps unchanged merges stable (t3430 'do not rebase cousins', config tests).
    if allow_ff {
        if let Some(orig_oid) =
            rebase_merge_fast_forward_target(repo, git_dir, merge_oid, merge_args)?
        {
            reset_rebase_head_to_commit(repo, git_dir, &orig_oid)?;
            if edit_message {
                // Git's `do_merge` `fast_forward_edit`: a `merge -c <commit>` that fast-forwards
                // still opens the editor to reword the message (and re-reads the todo afterwards).
                // Amend the fast-forwarded merge commit with the edited message, keeping its tree
                // and parents (t3430 'merge -c ... reloads todo-list', 'reword fast-forwarded empty
                // merge commit').
                reword_fast_forwarded_merge(repo, git_dir, rb_dir, &orig_oid, None)?;
            }
            record_rebase_in_rewritten_pending(git_dir, rb_dir, merge_oid, next_after_line)?;
            return Ok(RebaseMergeReuseOutcome::Completed);
        }
    }
    fs::write(
        rb_dir.join("rebase-merge-source"),
        format!("{}\n", merge_oid.to_hex()),
    )?;
    fs::write(rb_dir.join("rebase-merge-args"), format!("{merge_args}\n"))?;
    fs::write(
        rb_dir.join("rebase-merge-edit-msg"),
        if edit_message { "1\n" } else { "0\n" },
    )?;
    // Run the merge itself with the original message (`--no-edit`). For a `merge -c` reword we open
    // the editor *after* the merge commit lands, not inside the merge subprocess: passing `--edit`
    // would launch the editor while MERGE_HEAD is still live, and a reword editor that runs
    // `git rebase --edit-todo` then trips over the in-progress merge (t3430 'merge -c ... reloads
    // todo-list'). Rewording afterwards mirrors Git's `do_merge` committing then re-running the
    // commit with EDIT_MSG.
    let status = run_rebase_merge_subprocess(repo, git_dir, merge_oid, merge_args, false)?;
    if !status.success() {
        if git_dir.join("MERGE_HEAD").exists() {
            return Ok(RebaseMergeReuseOutcome::Conflict);
        }
        return Ok(RebaseMergeReuseOutcome::Blocked);
    }
    rewrite_merge_head_for_replay_opts(repo, git_dir, rb_dir, merge_oid)?;
    if edit_message {
        let merged_head = resolve_head(git_dir)?
            .oid()
            .cloned()
            .ok_or_else(|| anyhow::anyhow!("HEAD has no OID after merge"))?;
        reword_fast_forwarded_merge(repo, git_dir, rb_dir, &merged_head, Some(merge_oid))?;
    }
    let _ = fs::remove_file(rb_dir.join("rebase-merge-source"));
    let _ = fs::remove_file(rb_dir.join("rebase-merge-args"));
    record_rebase_in_rewritten_pending(git_dir, rb_dir, merge_oid, next_after_line)?;
    Ok(RebaseMergeReuseOutcome::Completed)
}

fn rebase_state_todo_lines(
    repo: &Repository,
    config: &ConfigSet,
    entries: &[(ObjectId, RebaseTodoCmd)],
) -> Result<Vec<String>> {
    entries
        .iter()
        .map(|(oid, cmd)| format_rebase_todo_line(repo, oid, *cmd, config, false))
        .collect()
}

/// Build the non-interactive todo for a plain merge-backend rebase with `--update-refs`.
///
/// Each commit becomes a `pick` line; after a commit, any local branch pointing at it is
/// interleaved as an `update-ref <ref>` step (or a `# Ref ... checked out` comment when the
/// branch is occupied by a worktree), mirroring Git's `todo_list_add_update_ref_commands`. This
/// lets a plain `git rebase --update-refs <upstream>` populate the `rebase-merge/update-refs`
/// state file even though no sequence editor was opened, so the scheduled branches become
/// "used by worktree" while the rebase is paused on a conflict (t2407 #6).
///
/// # Parameters
/// - `repo`: repository handle (for worktree-occupancy checks and object reads).
/// - `git_dir`: git directory for branch-decoration enumeration.
/// - `config`: config set controlling the comment prefix and todo-line formatting.
/// - `commits`: the commits to rebase, in todo order (oldest first).
///
/// # Errors
/// Propagates object-read and ref-enumeration failures.
fn rebase_update_refs_todo_lines(
    repo: &Repository,
    git_dir: &Path,
    config: &ConfigSet,
    commits: &[ObjectId],
) -> Result<Vec<String>> {
    let items = build_autosquash_with_update_refs(repo, git_dir, commits, false, true)?;
    let comment_prefix = comment_line_prefix_full(config);
    let mut lines = Vec::with_capacity(items.len());
    for item in &items {
        match item {
            TodoBuildItem::Commit(oid, cmd) => {
                lines.push(format_rebase_todo_line(repo, oid, *cmd, config, false)?);
            }
            TodoBuildItem::UpdateRef(refname) => {
                lines.push(format!("update-ref {refname}"));
            }
            TodoBuildItem::RefComment(text) => {
                lines.push(format!("{comment_prefix} {text}"));
            }
            TodoBuildItem::Exec(cmd) => {
                lines.push(format!("exec {cmd}"));
            }
        }
    }
    Ok(lines)
}

/// Index of the next todo line that is not a `fixup`/`squash` pick, if any.
fn next_non_fixup_index(
    repo: &Repository,
    todo_lines: &[&str],
    from: usize,
    interactive: bool,
) -> Option<usize> {
    let mut j = from + 1;
    while j < todo_lines.len() {
        if let Ok(Some(step)) = parse_rebase_replay_step(repo, todo_lines[j], interactive) {
            match step {
                RebaseReplayStep::PickLike { cmd, .. } => {
                    if matches!(cmd, RebaseTodoCmd::Fixup | RebaseTodoCmd::Squash) {
                        j += 1;
                        continue;
                    }
                    return Some(j);
                }
                RebaseReplayStep::Exec(_)
                | RebaseReplayStep::Edit(_)
                | RebaseReplayStep::MergeReuseMessage { .. }
                | RebaseReplayStep::MergePlain { .. }
                | RebaseReplayStep::Break
                | RebaseReplayStep::Label(_)
                | RebaseReplayStep::Reset(_) => return Some(j),
                RebaseReplayStep::Noop | RebaseReplayStep::UpdateRef(_) => {
                    j += 1;
                }
            }
        } else {
            j += 1;
        }
    }
    None
}

fn rebase_todo_first_word(line: &str) -> Option<&str> {
    let t = line.trim();
    if t.is_empty() || t.starts_with('#') {
        return None;
    }
    t.split_whitespace().next()
}

fn rebase_todo_word_is_fixup_or_squash(word: &str) -> bool {
    matches!(
        word.to_ascii_lowercase().as_str(),
        "fixup" | "f" | "squash" | "s"
    )
}

/// Return whether the current fixup/squash is the last item in its contiguous fixup chain.
fn is_final_fixup_in_todo(
    repo: &Repository,
    _rb_dir: &Path,
    todo_lines: &[&str],
    idx: usize,
    interactive: bool,
) -> bool {
    let Ok(Some(step)) = parse_rebase_replay_step(repo, todo_lines[idx], interactive) else {
        return false;
    };
    let RebaseReplayStep::PickLike { cmd, .. } = step else {
        return false;
    };
    if !matches!(cmd, RebaseTodoCmd::Fixup | RebaseTodoCmd::Squash) {
        return false;
    }

    let mut j = idx + 1;
    while j < todo_lines.len() {
        let t = todo_lines[j].trim();
        if t.is_empty() || t.starts_with('#') {
            j += 1;
            continue;
        }
        let Some(w) = rebase_todo_first_word(todo_lines[j]) else {
            j += 1;
            continue;
        };
        if w.eq_ignore_ascii_case("noop") {
            j += 1;
            continue;
        }
        if rebase_todo_word_is_fixup_or_squash(w) {
            return false;
        }
        break;
    }

    true
}

#[derive(Clone, Copy, Default)]
struct SquashChainCtx {
    count: usize,
    seen_squash: bool,
}

fn squash_ctx_path(rb_dir: &Path) -> PathBuf {
    rb_dir.join("squash-chain-ctx")
}

fn read_squash_ctx(rb_dir: &Path) -> SquashChainCtx {
    let Ok(s) = fs::read_to_string(squash_ctx_path(rb_dir)) else {
        return SquashChainCtx::default();
    };
    let mut count = 0usize;
    let mut seen = false;
    for line in s.lines() {
        if let Some(v) = line.strip_prefix("count=") {
            count = v.parse().unwrap_or(0);
        }
        if line.trim() == "seen_squash=1" {
            seen = true;
        }
    }
    SquashChainCtx {
        count,
        seen_squash: seen,
    }
}

fn write_squash_ctx(rb_dir: &Path, ctx: SquashChainCtx) -> Result<()> {
    fs::write(
        squash_ctx_path(rb_dir),
        format!(
            "count={}\nseen_squash={}\n",
            ctx.count,
            if ctx.seen_squash { 1 } else { 0 }
        ),
    )?;
    Ok(())
}

fn clear_squash_ctx(rb_dir: &Path) {
    let _ = fs::remove_file(squash_ctx_path(rb_dir));
    let _ = fs::remove_file(rb_dir.join("message-squash"));
    let _ = fs::remove_file(rb_dir.join("message-fixup"));
    let _ = fs::remove_file(rb_dir.join("message-fixup-edit"));
    let _ = fs::remove_file(rb_dir.join("message-fixup-active-section"));
}

fn drop_last_squash_message_section(rb_dir: &Path) -> Result<()> {
    let mut ctx = read_squash_ctx(rb_dir);
    if ctx.count == 0 {
        return Ok(());
    }
    ctx.count -= 1;
    write_squash_ctx(rb_dir, ctx)?;
    let path = rb_dir.join("message-squash");
    let Ok(mut msg) = fs::read_to_string(&path) else {
        return Ok(());
    };
    if let Some(pos) = msg.rfind("\n# This is the commit message #") {
        msg.truncate(pos);
        msg.push('\n');
    }
    if let Some(first_nl) = msg.find('\n') {
        let count = ctx.count + 1;
        if msg.starts_with("# This is a combination of") {
            msg.replace_range(
                ..first_nl + 1,
                &format!("# This is a combination of {count} commits.\n"),
            );
        }
    }
    fs::write(path, msg)?;
    Ok(())
}

fn cleanup_head_message_after_fixup_skip(
    repo: &Repository,
    git_dir: &Path,
    raw_override: Option<String>,
) -> Result<()> {
    let head = resolve_head(git_dir)?;
    let Some(head_oid) = head.oid().copied() else {
        return Ok(());
    };
    let obj = repo.odb.read(&head_oid)?;
    let commit = parse_commit(&obj.data)?;
    let raw_message = raw_override.unwrap_or_else(|| commit.message.clone());
    if !raw_message.starts_with("# This is a combination of") {
        return Ok(());
    }
    let message = String::from_utf8_lossy(&grit_lib::stripspace::process(
        raw_message.as_bytes(),
        &grit_lib::stripspace::Mode::StripComments("#".to_string()),
    ))
    .into_owned();
    let commit_data = CommitData {
        tree: commit.tree,
        parents: commit.parents,
        author: commit.author.clone(),
        committer: commit.committer.clone(),
        author_raw: commit.author_raw.clone(),
        committer_raw: commit.committer_raw.clone(),
        encoding: commit.encoding,
        message,
        raw_message: None,
    };
    let bytes = serialize_commit(&commit_data);
    let new_oid = repo.odb.write(ObjectKind::Commit, &bytes)?;
    fs::write(git_dir.join("HEAD"), format!("{}\n", new_oid.to_hex()))?;
    Ok(())
}

fn run_prepare_commit_msg_hook(
    repo: &Repository,
    path: &Path,
    arg1: &str,
    arg2: Option<&str>,
    git_editor: Option<&str>,
) -> Result<()> {
    let p = path.to_string_lossy().into_owned();
    let hook_args: Vec<String> = match arg2 {
        Some(second) => vec![p.clone(), arg1.to_string(), second.to_string()],
        None => vec![p.clone(), arg1.to_string()],
    };
    let hook_arg_refs: Vec<&str> = hook_args.iter().map(String::as_str).collect();
    let hook_result = if let Some(editor) = git_editor {
        let index_path = repo.index_path();
        let hook_env = CommitHookEnv {
            index_file: Some(index_path.as_path()),
            git_editor: Some(editor),
            git_prefix: None,
            extra_env: &[],
        };
        run_commit_hook(repo, "prepare-commit-msg", &hook_arg_refs, None, &hook_env)
            .map_err(|e| anyhow::anyhow!(e))?
    } else {
        match arg2 {
            Some(second) => run_hook(
                repo,
                "prepare-commit-msg",
                &[p.as_ref(), arg1, second],
                None,
            ),
            None => run_hook(repo, "prepare-commit-msg", &[p.as_ref(), arg1], None),
        }
    };
    if let HookResult::Failed(code) = hook_result {
        bail!("prepare-commit-msg hook exited with status {code}");
    }
    Ok(())
}

/// Writes `text` to `COMMIT_EDITMSG`, runs `prepare-commit-msg`, returns the file contents afterward.
///
/// When `git_editor` is `Some(":")`, matches Git's non-interactive sequencer pick hook environment
/// (`t7505` `(no editor)` suffix).
fn commit_message_after_prepare_hook(
    repo: &Repository,
    git_dir: &Path,
    text: &str,
    arg1: &str,
    git_editor: Option<&str>,
) -> Result<String> {
    let editmsg = git_dir.join("COMMIT_EDITMSG");
    fs::write(&editmsg, text)?;
    run_prepare_commit_msg_hook(repo, &editmsg, arg1, None, git_editor)?;
    fs::read_to_string(&editmsg).context("read COMMIT_EDITMSG after prepare-commit-msg hook")
}

/// Run the `commit-msg` hook on a reworded message, matching upstream `sequencer.c` reword
/// (`run_git_commit` with `VERIFY_MSG`): writes `COMMIT_EDITMSG`, runs `commit-msg <COMMIT_EDITMSG>`,
/// and on success re-reads the file (so a hook that rewrites the message is honoured) and re-applies
/// `rebase_commit_msg_cleanup`. The reword pipeline always verifies, so this is unconditional.
fn run_reword_commit_msg_hook(
    repo: &Repository,
    git_dir: &Path,
    message: String,
    config: &ConfigSet,
) -> Result<String> {
    let editmsg = git_dir.join("COMMIT_EDITMSG");
    fs::write(&editmsg, &message)?;
    let editmsg_str = editmsg.to_string_lossy().to_string();
    let index_path = repo.index_path();
    let hook_env = CommitHookEnv {
        index_file: Some(index_path.as_path()),
        git_editor: Some(":"),
        git_prefix: None,
        extra_env: &[],
    };
    let result = run_commit_hook(repo, "commit-msg", &[editmsg_str.as_str()], None, &hook_env)
        .map_err(|e| anyhow::anyhow!(e))?;
    match result {
        HookResult::Failed(code) => bail!("commit-msg hook exited with status {code}"),
        HookResult::Success => {
            let new = fs::read_to_string(&editmsg)
                .context("read COMMIT_EDITMSG after commit-msg hook")?;
            Ok(apply_commit_msg_cleanup(
                &new,
                rebase_commit_msg_cleanup(config),
            ))
        }
        _ => Ok(message),
    }
}

fn strip_comment_lines_template(msg: &str) -> String {
    let mut out = String::new();
    for line in msg.lines() {
        let t = line.trim_start();
        if t.starts_with('#') {
            continue;
        }
        out.push_str(line);
        out.push('\n');
    }
    out
}

fn default_commit_msg_cleanup(config: &ConfigSet) -> &'static str {
    match config
        .get("commit.cleanup")
        .map(|s| s.to_lowercase())
        .as_deref()
    {
        Some("strip") => "strip",
        Some("verbatim") => "verbatim",
        Some("whitespace") => "whitespace",
        Some("scissors") => "scissors",
        _ => "default",
    }
}

/// Message cleanup for rebase replay after `prepare-commit-msg`, matching Git's sequencer
/// `try_to_commit`: when `commit.cleanup` is unset, `default_msg_cleanup` is `COMMIT_MSG_CLEANUP_NONE`
/// and `strbuf_stripspace` does not strip `#` lines (unlike `git commit`'s default).
fn rebase_commit_msg_cleanup(config: &ConfigSet) -> &'static str {
    match config
        .get("commit.cleanup")
        .map(|s| s.to_lowercase())
        .as_deref()
    {
        Some("strip") => "strip",
        Some("verbatim") => "verbatim",
        Some("whitespace") => "whitespace",
        Some("scissors") => "scissors",
        _ => "verbatim",
    }
}

fn apply_commit_msg_cleanup(msg: &str, mode: &str) -> String {
    match mode {
        "verbatim" => msg.to_string(),
        "whitespace" => {
            let s = msg.replace("\r\n", "\n");
            let lines: Vec<&str> = s.lines().collect();
            let mut start = 0usize;
            while start < lines.len() && lines[start].trim().is_empty() {
                start += 1;
            }
            let mut end = lines.len();
            while end > start && lines[end - 1].trim().is_empty() {
                end -= 1;
            }
            lines[start..end].join("\n") + "\n"
        }
        "strip" | "default" | "scissors" => {
            let mut s = msg.replace("\r\n", "\n");
            let cut = if mode == "scissors" {
                s.find("\n------------------------ >8 ------------------------\n")
            } else {
                None
            };
            if let Some(i) = cut {
                s.truncate(i);
            }
            let lines: Vec<&str> = s.lines().collect();
            let mut out: Vec<&str> = Vec::new();
            for line in lines {
                let t = line.trim_start();
                if t.starts_with('#') {
                    continue;
                }
                out.push(line);
            }
            let mut start = 0usize;
            while start < out.len() && out[start].trim().is_empty() {
                start += 1;
            }
            let mut end = out.len();
            while end > start && out[end - 1].trim().is_empty() {
                end -= 1;
            }
            out[start..end].join("\n") + "\n"
        }
        _ => strip_comment_lines_template(msg),
    }
}

fn cleanup_squash_editor_message(msg: &str, config: &ConfigSet) -> String {
    let normalized = msg.replace("\r\n", "\n");
    let mut kept = Vec::new();
    let mut uncomment_section = false;
    for line in normalized.lines() {
        let trimmed = line.trim_start();
        if trimmed.starts_with("# The commit message #") && trimmed.contains("will be skipped") {
            uncomment_section = false;
        } else if trimmed.starts_with("# This is the 1st commit message:")
            || trimmed.starts_with("# This is the commit message #")
        {
            uncomment_section = true;
        }
        let skip = trimmed.starts_with("# This is a combination of ")
            || trimmed.starts_with("# This is the ")
            || trimmed.starts_with("# The commit message #")
            || (trimmed.starts_with('#') && !uncomment_section)
            || trimmed
                .strip_prefix("# ")
                .is_some_and(|rest| rest.starts_with("fixup!") || rest.starts_with("squash!"));
        if !skip {
            if uncomment_section {
                if let Some(rest) = trimmed.strip_prefix("# ") {
                    kept.push(rest);
                    continue;
                }
            }
            kept.push(line);
        }
    }
    let mut collapsed: Vec<&str> = Vec::new();
    for line in kept {
        if line.trim().is_empty() && collapsed.last().is_none_or(|prev| prev.trim().is_empty()) {
            continue;
        }
        collapsed.push(line);
    }
    while collapsed.last().is_some_and(|line| line.trim().is_empty()) {
        collapsed.pop();
    }
    let mut cleaned = collapsed.join("\n");
    if !cleaned.is_empty() {
        cleaned.push('\n');
    }
    let cleaned = apply_commit_msg_cleanup(&cleaned, "whitespace");
    apply_commit_msg_cleanup(&cleaned, rebase_commit_msg_cleanup(config))
}

fn final_squash_template_after_fixups(fixup_message: &str, squash_body: &str) -> String {
    let mut out = String::new();
    out.push_str("# This is a combination of 2 commits.\n");
    out.push_str("# This is the 1st commit message:\n\n");
    out.push_str(fixup_message.trim_end_matches('\n'));
    out.push_str("\n\n# This is the commit message #2:\n\n");
    out.push_str(squash_body.trim_end_matches('\n'));
    out.push('\n');
    out
}

fn fixup_edit_template(rb_dir: &Path, fixup_message: &str) -> String {
    let ctx = read_squash_ctx(rb_dir);
    let n = ctx.count + 1;
    let mut out = String::new();
    out.push_str(&format!("# This is a combination of {n} commits.\n"));
    out.push_str("# This is the 1st commit message:\n\n");
    out.push_str("\n# This is the commit message #");
    out.push_str(&n.to_string());
    out.push_str(":\n\n");
    out.push_str(fixup_message.trim_end_matches('\n'));
    out.push_str("\n\n");
    out
}

fn message_with_editor_separator(message: &str) -> String {
    let mut out = message.trim_end_matches('\n').to_string();
    out.push_str("\n\n");
    out
}

fn write_fixup_message_mode(rb_dir: &Path, mode: Option<FixupMessageMode>) -> Result<()> {
    let edit_path = rb_dir.join("message-fixup-edit");
    match mode {
        Some(FixupMessageMode::EditCommit) => fs::write(edit_path, "1\n")?,
        Some(FixupMessageMode::UseCommit) => {
            let _ = fs::remove_file(edit_path);
        }
        None => {}
    }
    Ok(())
}

fn fixup_message_opens_editor(rb_dir: &Path) -> bool {
    rb_dir.join("message-fixup-edit").exists()
}

fn update_squash_message_file(
    repo: &Repository,
    rb_dir: &Path,
    git_dir: &Path,
    cmd: RebaseTodoCmd,
    fixup_message_mode: Option<FixupMessageMode>,
    picked: &CommitData,
    ctx: &mut SquashChainCtx,
) -> Result<()> {
    let squash_path = rb_dir.join("message-squash");
    let fixup_path = rb_dir.join("message-fixup");
    let mut display_message = picked.message.clone();
    if rebase_signoff(rb_dir)
        && cmd == RebaseTodoCmd::Fixup
        && fixup_message_mode.is_some()
        && !ctx.seen_squash
    {
        let config = ConfigSet::load(Some(git_dir), true)?;
        let committer = rebase_replayed_committer_line(
            &config,
            &picked.author,
            load_rebase_replay_commit_opts(rb_dir),
            time::OffsetDateTime::now_utc(),
        )?;
        let (name, email, _) = split_stored_author_line(&committer)?;
        let sob = format_signoff_line(&name, &email);
        append_signoff_trailer(&mut display_message, &sob, &config);
    }
    let body = if cmd == RebaseTodoCmd::Squash || cmd == RebaseTodoCmd::Fixup {
        display_message.as_str()
    } else {
        message_body_after_subject(&display_message)
    };

    if ctx.count == 0 {
        let head_oid = resolve_head(git_dir)?
            .oid()
            .cloned()
            .ok_or_else(|| anyhow::anyhow!("HEAD is unborn during squash"))?;
        let hobj = repo.odb.read(&head_oid)?;
        let head_commit = parse_commit(&hobj.data)?;
        let hsubj = head_commit.message.lines().next().unwrap_or("");
        let hbody = message_body_after_subject(&head_commit.message);
        // `is_fixup_flag` mirrors Git's `is_fixup_flag(command, flag)`: only the
        // `fixup -C` / `fixup -c` amend modes (a fixup with a message mode set) replace and
        // skip the prior accumulated message. A *plain* `fixup` keeps the 1st commit message
        // uncommented, exactly like a squash; only the fixup commit's own message (#2) is
        // skipped (handled below). Earlier this branch gated on `cmd == Fixup` alone, which
        // wrongly commented out the pick target's message for plain fixups (t3404 #35).
        let is_fixup_flag = cmd == RebaseTodoCmd::Fixup && fixup_message_mode.is_some();
        let mut buf = String::new();
        buf.push_str("# This is a combination of 2 commits.\n");
        if is_fixup_flag {
            buf.push_str("# The 1st commit message will be skipped:\n\n");
        } else {
            buf.push_str("# This is the 1st commit message:\n\n");
        }
        // A plain `fixup` records the accumulated message to FIXUP_MSG (Git writes HEAD's body
        // there for `command == TODO_FIXUP && !flag`).
        if cmd == RebaseTodoCmd::Fixup {
            write_fixup_message_mode(rb_dir, fixup_message_mode)?;
            let fixup_msg = if fixup_message_mode.is_some() {
                fixup_replacement_message(&picked.message)
            } else {
                let mut fixup_msg = String::new();
                fixup_msg.push_str("# This is a combination of 2 commits.\n");
                fixup_msg.push_str("# This is the 1st commit message:\n\n");
                fixup_msg.push_str(hsubj);
                fixup_msg.push('\n');
                fixup_msg.push_str(hbody);
                if !hbody.is_empty() && !hbody.ends_with('\n') {
                    fixup_msg.push('\n');
                }
                fixup_msg.push_str("\n# The commit message #2 will be skipped:\n\n");
                append_commented(&mut fixup_msg, picked.message.lines().next().unwrap_or(""));
                fixup_msg
            };
            fs::write(&fixup_path, fixup_msg)?;
        }
        if is_fixup_flag {
            append_commented(&mut buf, hsubj);
            if !hbody.is_empty() {
                append_commented(&mut buf, hbody.trim_end_matches('\n'));
            }
        } else {
            buf.push_str(hsubj);
            buf.push('\n');
            buf.push_str(hbody);
            if !hbody.is_empty() && !hbody.ends_with('\n') {
                buf.push('\n');
            }
        }
        if cmd == RebaseTodoCmd::Fixup && fixup_message_mode.is_none() {
            append_skipped_squash_message(&mut buf, body, 2);
        } else {
            append_nth_squash_message(&mut buf, body, cmd, ctx.seen_squash, 2);
        }
        fs::write(&squash_path, buf)?;
    } else {
        let mut buf = fs::read_to_string(&squash_path)?;
        let is_fixup_flag = cmd == RebaseTodoCmd::Fixup && fixup_message_mode.is_some();
        let n = ctx.count + 2;
        // Splice the combination-count header first (Git rewrites the leading line in place),
        // matching `update_squash_messages`' `strbuf_splice` of `combined_commit_msg_fmt`.
        if let Some(pos) = buf.find('\n') {
            if buf.starts_with("# This is a combination of") {
                buf.replace_range(
                    ..pos + 1,
                    &format!("# This is a combination of {n} commits.\n"),
                );
            }
        }
        // A `fixup -C`/`fixup -c` that replaces the message (and is not after a squash) drops every
        // section accumulated so far by commenting them out and flipping their headers to
        // "will be skipped" — Git's `update_squash_message_for_fixup`.
        if is_fixup_flag && !ctx.seen_squash {
            update_squash_message_for_fixup(&mut buf);
        }
        if is_fixup_flag {
            write_fixup_message_mode(rb_dir, fixup_message_mode)?;
            fs::write(&fixup_path, fixup_replacement_message(&picked.message))?;
        }
        if cmd == RebaseTodoCmd::Fixup && fixup_message_mode.is_none() {
            append_skipped_squash_message(&mut buf, body, n);
        } else {
            append_nth_squash_message(&mut buf, body, cmd, ctx.seen_squash, n);
        }
        fs::write(&squash_path, buf)?;
    }

    if cmd == RebaseTodoCmd::Squash {
        let _ = fs::remove_file(rb_dir.join("message-fixup-active-section"));
        ctx.seen_squash = true;
    }
    ctx.count += 1;
    write_squash_ctx(rb_dir, *ctx)?;
    Ok(())
}

fn commit_from_merged_index(
    repo: &Repository,
    _git_dir: &Path,
    merged_index: &Index,
    config: &ConfigSet,
    parents: Vec<ObjectId>,
    source_author_line: &str,
    message: String,
    replay_opts: RebaseReplayCommitOpts,
    now: time::OffsetDateTime,
) -> Result<ObjectId> {
    let tree_oid = write_tree_from_index(&repo.odb, merged_index, "")?;
    let author = rebase_replayed_author_line(source_author_line, replay_opts, now)?;
    let committer = rebase_replayed_committer_line(config, source_author_line, replay_opts, now)?;
    let (message, encoding, raw_message) = finalize_message_for_commit_encoding(message, config);
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
        message,
        raw_message,
    };
    let bytes = serialize_commit(&commit_data);
    Ok(repo.odb.write(ObjectKind::Commit, &bytes)?)
}

fn rebase_reflog_action() -> String {
    std::env::var("GIT_REFLOG_ACTION").unwrap_or_else(|_| "rebase".to_owned())
}

fn run_post_checkout_hook(repo: &Repository, old_oid: &ObjectId, new_oid: &ObjectId) -> Result<()> {
    let old_hex = old_oid.to_hex();
    let new_hex = new_oid.to_hex();
    let args = [old_hex.as_str(), new_hex.as_str(), "1"];
    if let HookResult::Failed(code) = run_hook(repo, "post-checkout", &args, None) {
        bail!("post-checkout hook exited with status {code}");
    }
    Ok(())
}

fn print_branch_up_to_date(head: &HeadState) {
    if let Some(name) = head.branch_name() {
        println!("Current branch {name} is up to date.");
    } else {
        println!("HEAD is up to date.");
    }
}

fn reflog_identity(repo: &Repository) -> String {
    let config = ConfigSet::load(Some(&repo.git_dir), true).unwrap_or_default();
    let (name, email) = crate::ident::resolve_loose_committer_parts(&config);
    let now = time::OffsetDateTime::now_utc();
    let epoch = now.unix_timestamp();
    let offset = now.offset();
    let hours = offset.whole_hours();
    let minutes = offset.minutes_past_hour().unsigned_abs();
    format!("{name} <{email}> {epoch} {hours:+03}{minutes:02}")
}

fn rebase_apply_dir(git_dir: &Path) -> std::path::PathBuf {
    git_dir.join("rebase-apply")
}

fn rebase_merge_dir(git_dir: &Path) -> std::path::PathBuf {
    git_dir.join("rebase-merge")
}

/// Whether `rebase --update-refs` is active (CLI flag or `rebase.updateRefs`).
fn rebase_update_refs_enabled(args: &Args, config: &ConfigSet) -> bool {
    if args.no_update_refs {
        return false;
    }
    if args.update_refs {
        return true;
    }
    config
        .get_bool("rebase.updateRefs")
        .and_then(|r| r.ok())
        .unwrap_or(false)
}

/// Write `rebase-merge/update-refs` for branches pointing at commits being replayed.
/// Branch refs (full name) pointing at `oid`, excluding `skip_ref` (usually HEAD).
fn branch_refs_at_commit(
    git_dir: &Path,
    oid: ObjectId,
    skip_ref: Option<&str>,
) -> Result<Vec<String>> {
    let mut out = Vec::new();
    for (refname, target) in list_refs(git_dir, "refs/heads/")? {
        if target == oid && skip_ref != Some(refname.as_str()) {
            out.push(refname);
        }
    }
    out.sort();
    Ok(out)
}

/// Read a ref's current object id, returning `None` if it does not exist or is unreadable.
fn read_ref_oid(git_dir: &Path, refname: &str) -> Option<ObjectId> {
    resolve_ref(git_dir, refname).ok()
}

/// A single record in the `rebase-merge/update-refs` state file.
///
/// Mirrors Git's `struct update_ref_record`: `before` is the ref value captured when the
/// rebase started; `after` is the new tip recorded when the corresponding `update-ref` todo
/// command runs (null/zero until then). At completion the ref is moved from `before` to `after`.
#[derive(Debug, Clone)]
struct UpdateRefRecord {
    before: String,
    after: String,
}

/// Read and parse the `rebase-merge/update-refs` state file into ordered `(ref, record)` pairs.
///
/// The file format is three lines per ref: `<refname>\n<before-oid>\n<after-oid>\n`. The order
/// is preserved from the file so re-writes stay stable; callers that need alphabetical output
/// (e.g. the completion message) sort separately. Returns an empty list when the file is absent.
///
/// # Errors
/// Propagates filesystem read errors (other than not-found).
fn read_update_refs_state(rb_dir: &Path) -> Result<Vec<(String, UpdateRefRecord)>> {
    let path = rb_dir.join("update-refs");
    let content = match fs::read_to_string(&path) {
        Ok(c) => c,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(e.into()),
    };
    let mut lines = content.lines();
    let mut out = Vec::new();
    while let Some(refname) = lines.next() {
        if refname.is_empty() {
            continue;
        }
        let before = lines.next().unwrap_or("").to_owned();
        let after = lines.next().unwrap_or("").to_owned();
        out.push((refname.to_owned(), UpdateRefRecord { before, after }));
    }
    Ok(out)
}

/// Serialize `(ref, record)` pairs back to the `rebase-merge/update-refs` state file.
///
/// Writes three lines per ref (`<ref>\n<before>\n<after>\n`); removes the file when the list is
/// empty (matching Git's `write_update_refs_state`).
///
/// # Errors
/// Propagates filesystem write/remove errors.
fn write_update_refs_state(rb_dir: &Path, refs: &[(String, UpdateRefRecord)]) -> Result<()> {
    let path = rb_dir.join("update-refs");
    if refs.is_empty() {
        if let Err(e) = fs::remove_file(&path) {
            if e.kind() != std::io::ErrorKind::NotFound {
                return Err(e.into());
            }
        }
        return Ok(());
    }
    let mut body = String::new();
    for (refname, rec) in refs {
        body.push_str(refname);
        body.push('\n');
        body.push_str(&rec.before);
        body.push('\n');
        body.push_str(&rec.after);
        body.push('\n');
    }
    fs::write(&path, body)?;
    Ok(())
}

/// Initialize the `update-refs` state from the `update-ref` lines in the generated todo.
///
/// For each distinct `update-ref <ref>` line, record the ref's current value as `before` and a
/// null `after` (zero OID). Refs that do not exist yet keep a zero `before`. This is Git's
/// `todo_list_add_update_ref_commands` writing the initial state before any command runs.
///
/// # Parameters
/// - `git_dir`: git directory (for ref lookups and locating the rebase state dir).
/// - `todo_body`: the finalized todo text containing `update-ref` lines.
///
/// # Errors
/// Propagates ref-read / state-write failures.
fn write_rebase_update_refs(git_dir: &Path, todo_body: &str) -> Result<()> {
    let rb_dir = rebase_merge_dir(git_dir);
    let zero = "0".repeat(40);
    let mut seen: HashSet<String> = HashSet::new();
    let mut refs: Vec<(String, UpdateRefRecord)> = Vec::new();
    for line in todo_body.lines() {
        let t = line.trim();
        let mut parts = t.split_whitespace();
        let Some(cmd) = parts.next() else { continue };
        if cmd != "update-ref" && cmd != "u" {
            continue;
        }
        let Some(refname) = parts.next() else {
            continue;
        };
        if !seen.insert(refname.to_owned()) {
            continue;
        }
        let before = read_ref_oid(git_dir, refname)
            .map(|o| o.to_hex())
            .unwrap_or_else(|| zero.clone());
        refs.push((
            refname.to_owned(),
            UpdateRefRecord {
                before,
                after: zero.clone(),
            },
        ));
    }
    write_update_refs_state(&rb_dir, &refs)
}

/// Record the current HEAD as the pending new value for `refname` in the update-refs state.
///
/// Implements Git's `do_update_ref`: locate the matching record and set its `after` to the
/// resolved HEAD oid, then persist. A no-op when the ref is not tracked.
///
/// # Errors
/// Propagates state read/write and HEAD-resolution failures.
fn do_update_ref(repo: &Repository, rb_dir: &Path, refname: &str) -> Result<()> {
    let mut refs = read_update_refs_state(rb_dir)?;
    let head = resolve_head(&repo.git_dir)?;
    let Some(head_oid) = head.oid() else {
        return Ok(());
    };
    for (name, rec) in refs.iter_mut() {
        if name == refname {
            rec.after = head_oid.to_hex();
            break;
        }
    }
    write_update_refs_state(rb_dir, &refs)
}

/// Apply all recorded `--update-refs` ref updates at rebase completion (Git's `do_update_refs`).
///
/// For each tracked ref whose `after` differs from its `before` and is non-null, move the ref
/// from `before` to `after` (verifying the ref still points at `before`). Successful refs are
/// listed under "Updated the following refs with --update-refs:" (alphabetically, on stderr);
/// refs that no longer match `before` are reported as failures. Quiet mode suppresses output.
///
/// # Parameters
/// - `git_dir`: git directory (state dir + ref store).
/// - `quiet`: suppress the human-readable summary.
///
/// # Errors
/// Propagates state-read and ref-write failures.
fn apply_update_refs(git_dir: &Path, quiet: bool) -> Result<()> {
    let rb_dir = rebase_merge_dir(git_dir);
    let refs = read_update_refs_state(&rb_dir)?;
    if refs.is_empty() {
        return Ok(());
    }
    let zero = "0".repeat(40);
    let mut sorted: Vec<&(String, UpdateRefRecord)> = refs.iter().collect();
    sorted.sort_by(|a, b| a.0.cmp(&b.0));
    let mut updated: Vec<String> = Vec::new();
    let mut failed: Vec<String> = Vec::new();
    for (refname, rec) in sorted {
        // Skip refs whose `after` was never recorded (their update-ref command did not run) or
        // is identical to `before` (no movement).
        if rec.after == zero || rec.after == rec.before {
            continue;
        }
        let current = read_ref_oid(git_dir, refname).map(|o| o.to_hex());
        let before_matches = match (&current, rec.before == zero) {
            (None, true) => true,
            (Some(cur), false) => *cur == rec.before,
            _ => false,
        };
        if !before_matches {
            failed.push(refname.clone());
            continue;
        }
        match ObjectId::from_hex(&rec.after) {
            Ok(after_oid) => {
                write_ref(git_dir, refname, &after_oid)?;
                updated.push(refname.clone());
            }
            Err(_) => failed.push(refname.clone()),
        }
    }
    if !quiet && (!updated.is_empty() || !failed.is_empty()) {
        let mut msg = String::from("Updated the following refs with --update-refs:\n");
        for r in &updated {
            msg.push('\t');
            msg.push_str(r);
            msg.push('\n');
        }
        eprint!("{msg}");
        if !failed.is_empty() {
            let mut emsg =
                String::from("Failed to update the following refs with --update-refs:\n");
            for r in &failed {
                emsg.push('\t');
                emsg.push_str(r);
                emsg.push('\n');
            }
            eprint!("{emsg}");
        }
    }
    Ok(())
}

fn rebase_todo_line_is_comment(line: &str) -> bool {
    let t = line.trim_start();
    t.starts_with('#') || t.starts_with('%')
}

fn rebase_todo_actionable_lines(content: &str) -> Vec<&str> {
    content
        .lines()
        .map(str::trim)
        .filter(|t| !t.is_empty() && !rebase_todo_line_is_comment(t))
        .collect()
}

fn count_rebase_todo_actionable_lines(content: &str) -> usize {
    rebase_todo_actionable_lines(content).len()
}

fn append_ref_to_delete_list(rb_dir: &Path, refname: &str) -> Result<()> {
    let path = rb_dir.join("refs-to-delete");
    let mut f = OpenOptions::new().create(true).append(true).open(&path)?;
    writeln!(f, "{refname}")?;
    Ok(())
}

fn parse_merge_todo_arg_list(arg: &str) -> (Vec<String>, Option<String>) {
    let mut heads: Vec<String> = Vec::new();
    let mut oneline: Option<String> = None;
    let mut cur = arg.trim();
    while !cur.is_empty() {
        if cur.starts_with('#') {
            let rest = cur[1..].trim_start();
            if !rest.is_empty() {
                oneline = Some(rest.to_owned());
            }
            break;
        }
        let token_end = cur
            .find(|c: char| c.is_whitespace() || c == '#')
            .unwrap_or(cur.len());
        let tok = cur[..token_end].trim();
        if !tok.is_empty() {
            heads.push(tok.to_owned());
        }
        cur = cur[token_end..].trim_start();
        if cur.starts_with('#') {
            let rest = cur[1..].trim_start();
            if !rest.is_empty() {
                oneline = Some(rest.to_owned());
            }
            break;
        }
    }
    (heads, oneline)
}

fn resolve_rebase_merge_label(repo: &Repository, label: &str) -> Result<ObjectId> {
    // Mirror git `lookup_label`: try `refs/rewritten/<label>` first, then fall back to a plain
    // ref/commit-ish resolution. On failure git reports `could not resolve '<label>'`, NOT the
    // rev-parse "ambiguous argument" path-vs-revision hint (t3430 `reset` only looks under
    // refs/rewritten). We still allow full revision syntax for the fallback so `A^{tree}` resolves
    // to a tree object (t3430 `reset` rejects trees).
    let rewritten = format!("refs/rewritten/{label}");
    if let Ok(oid) = grit_lib::refs::resolve_ref(&repo.git_dir, &rewritten) {
        return Ok(oid);
    }
    resolve_revision(repo, label).map_err(|_| anyhow::anyhow!("could not resolve '{label}'"))
}

fn commit_oid_for_rebase_label(repo: &Repository, label: &str) -> Result<ObjectId> {
    let oid = resolve_rebase_merge_label(repo, label)?;
    let obj = repo.odb.read(&oid)?;
    if obj.kind == ObjectKind::Tree {
        bail!("object {} is a tree, not a commit", oid.to_hex());
    }
    if obj.kind != ObjectKind::Commit {
        bail!("object {} is not a commit", oid.to_hex());
    }
    Ok(oid)
}

fn ensure_squash_onto_fake_root(
    repo: &Repository,
    git_dir: &Path,
    rb_dir: &Path,
) -> Result<ObjectId> {
    let path = rb_dir.join("squash-onto");
    if path.exists() {
        let hex = fs::read_to_string(&path)?;
        return ObjectId::from_hex(hex.trim()).map_err(|e| anyhow::anyhow!("{e}"));
    }
    const GIT_EMPTY_TREE_HEX: &str = "4b825dc642cb6eb9a060e54bf8d69288fbee4904";
    let tree = ObjectId::from_hex(GIT_EMPTY_TREE_HEX).map_err(|e| anyhow::anyhow!("{e}"))?;
    let config = ConfigSet::load(Some(git_dir), true)?;
    let now = time::OffsetDateTime::now_utc();
    let committer = resolve_identity(&config, "COMMITTER")?;
    let commit_data = CommitData {
        tree,
        parents: Vec::new(),
        author: format_ident(&committer, now),
        committer: format_ident(&committer, now),
        author_raw: Vec::new(),
        committer_raw: Vec::new(),
        encoding: None,
        message: String::new(),
        raw_message: None,
    };
    let bytes = serialize_commit(&commit_data);
    let oid = repo.odb.write(ObjectKind::Commit, &bytes)?;
    fs::write(&path, format!("{}\n", oid.to_hex()))?;
    Ok(oid)
}

fn reset_worktree_to_commit(
    repo: &Repository,
    git_dir: &Path,
    head_before: &HeadState,
    target: ObjectId,
    reflog_suffix: &str,
) -> Result<()> {
    let old_oid = head_before.oid().cloned().unwrap_or_else(diff::zero_oid);
    let obj = repo.odb.read(&target)?;
    let commit = parse_commit(&obj.data)?;
    let entries = tree_to_index_entries(repo, &commit.tree, "")?;
    let mut idx = Index::new();
    idx.entries = entries;
    idx.sort();
    let old_index = load_index(repo)?;
    if let Some(wt) = &repo.work_tree {
        check_dirty_worktree(repo, &old_index, &idx, wt, head_before)?;
    }
    repo.write_index(&mut idx)?;
    if let Some(wt) = &repo.work_tree {
        checkout_merged_index(repo, wt, &old_index, &idx, true)?;
    }
    fs::write(git_dir.join("HEAD"), format!("{}\n", target.to_hex()))?;
    run_post_checkout_hook(repo, &old_oid, &target)?;
    let ident = reflog_identity(repo);
    let ra = std::env::var("GIT_REFLOG_ACTION").unwrap_or_else(|_| "rebase".to_owned());
    let msg = format!("{ra} (reset): {reflog_suffix}");
    let _ = append_reflog(git_dir, "HEAD", &old_oid, &target, &ident, &msg, false);
    Ok(())
}

fn first_token_reset_arg(arg: &str) -> &str {
    let s = arg.trim();
    if s.starts_with('[') {
        if let Some(end) = s.find(']') {
            return &s[..=end];
        }
    }
    s.split_whitespace().next().unwrap_or("")
}

fn resolve_reset_target(repo: &Repository, rb_dir: &Path, arg: &str) -> Result<ObjectId> {
    let tok = first_token_reset_arg(arg);
    if tok == "[new root]" {
        return ensure_squash_onto_fake_root(repo, &repo.git_dir, rb_dir);
    }
    if tok == "onto" {
        let hex = fs::read_to_string(rb_dir.join("onto"))?;
        return ObjectId::from_hex(hex.trim()).map_err(|e| anyhow::anyhow!("{e}"));
    }
    commit_oid_for_rebase_label(repo, tok)
}

fn run_plain_merge_for_rebase(
    repo: &Repository,
    merge_args: &str,
) -> Result<std::process::ExitStatus> {
    let (heads, oneline) = parse_merge_todo_arg_list(merge_args);
    if heads.is_empty() {
        bail!("nothing to merge: '{merge_args}'");
    }
    let oids: Vec<ObjectId> = heads
        .iter()
        .map(|h| commit_oid_for_rebase_label(repo, h))
        .collect::<Result<_>>()?;
    let msg = oneline.unwrap_or_else(|| {
        if oids.len() > 1 {
            format!("Merge branches {}", heads.join(" "))
        } else {
            format!("Merge branch '{}'", heads[0])
        }
    });
    let self_exe = std::env::current_exe().context("cannot determine grit binary path")?;
    let mut cmd = std::process::Command::new(&self_exe);
    // `--allow-unrelated-histories`: a plain `merge <label>` step may join independent roots
    // (Git's in-process `do_merge` has no unrelated-histories guard; t3430 'root commits').
    cmd.args([
        "merge",
        "--no-ff",
        "--allow-unrelated-histories",
        "--no-edit",
        "-m",
    ])
    .arg(&msg);
    for o in &oids {
        cmd.arg(o.to_hex());
    }
    let cmd = cmd.env_clear().envs(std::env::vars().filter(|(k, _)| {
        !k.starts_with("GIT_") || k == "GIT_CONFIG_NOSYSTEM" || k == "GIT_CONFIG_PARAMETERS"
    }));
    if let Ok(h) = std::env::var("HOME") {
        cmd.env("HOME", h);
    }
    if let Ok(p) = std::env::var("PATH") {
        cmd.env("PATH", p);
    }
    cmd.current_dir(repo.work_tree.as_deref().unwrap_or_else(|| Path::new(".")))
        .status()
        .context("run grit merge for rebase merge (plain)")
}

fn rebase_dir(git_dir: &Path) -> std::path::PathBuf {
    if rebase_merge_dir(git_dir).exists() {
        rebase_merge_dir(git_dir)
    } else {
        rebase_apply_dir(git_dir)
    }
}

/// Directory holding in-progress rebase state (`.git/rebase-apply` or `.git/rebase-merge`).
fn active_rebase_dir(git_dir: &Path) -> Option<PathBuf> {
    let merge = rebase_merge_dir(git_dir);
    if merge.exists() {
        return Some(merge);
    }
    let apply = rebase_apply_dir(git_dir);
    if apply.exists() {
        return Some(apply);
    }
    None
}

/// Canonical on-disk todo path for an in-progress rebase (Git uses
/// `.git/rebase-merge/git-rebase-todo`). When `GIT_REBASE_TODO` is set, it overrides the default
/// file; relative paths are resolved against the repository work tree.
fn rebase_todo_file_path(repo: &Repository, rb_dir: &Path) -> PathBuf {
    if let Ok(raw) = std::env::var("GIT_REBASE_TODO") {
        let trimmed = raw.trim();
        if !trimmed.is_empty() {
            let p = PathBuf::from(trimmed);
            if p.is_absolute() {
                return p;
            }
            if let Some(wt) = repo.work_tree.as_deref() {
                return wt.join(p);
            }
            return p;
        }
    }
    rb_dir.join("git-rebase-todo")
}

fn read_rebase_todo_file(repo: &Repository, rb_dir: &Path) -> Result<String> {
    let primary = rebase_todo_file_path(repo, rb_dir);
    if primary.exists() {
        return fs::read_to_string(&primary)
            .with_context(|| format!("failed to read rebase todo {}", primary.display()));
    }
    let legacy = rb_dir.join("todo");
    fs::read_to_string(&legacy)
        .with_context(|| format!("failed to read rebase todo {}", legacy.display()))
}

fn write_rebase_todo_file(repo: &Repository, rb_dir: &Path, content: &str) -> Result<()> {
    let primary = rebase_todo_file_path(repo, rb_dir);
    fs::write(&primary, content)
        .with_context(|| format!("failed to write rebase todo {}", primary.display()))?;
    let legacy = rb_dir.join("todo");
    if primary != legacy {
        fs::write(&legacy, content)
            .with_context(|| format!("failed to write rebase todo {}", legacy.display()))?;
    }
    Ok(())
}

fn rebase_state_dir_for_backend(git_dir: &Path, backend: RebaseBackend) -> std::path::PathBuf {
    match backend {
        RebaseBackend::Apply => rebase_apply_dir(git_dir),
        RebaseBackend::Merge => rebase_merge_dir(git_dir),
    }
}

fn is_rebase_in_progress(git_dir: &Path) -> bool {
    rebase_apply_dir(git_dir).exists() || rebase_merge_dir(git_dir).exists()
}

fn choose_rebase_backend(args: &Args) -> RebaseBackend {
    if apply_backend_forced(args) {
        RebaseBackend::Apply
    } else {
        // `git rebase --merge` and `git rebase --interactive` both use `.git/rebase-merge/`.
        RebaseBackend::Merge
    }
}

/// Git creates an ephemeral empty-tree root when `rebase --root` is used without `--onto`.
fn create_squash_onto_root_commit(
    repo: &Repository,
    git_dir: &Path,
    reset_author_date: bool,
    committer_date_is_author_date: bool,
) -> Result<ObjectId> {
    let config = ConfigSet::load(Some(git_dir), true)?;
    let now = time::OffsetDateTime::now_utc();
    let author_ident = resolve_identity(&config, "AUTHOR")?;
    let committer_ident = resolve_identity(&config, "COMMITTER")?;
    let epoch = now.unix_timestamp();
    let (aname, aemail) = &author_ident;
    let (cname, cemail) = &committer_ident;
    let (author, committer) = if reset_author_date {
        let a = format!("{aname} <{aemail}> {epoch} +0000");
        let c = if committer_date_is_author_date {
            a.clone()
        } else {
            format!("{cname} <{cemail}> {epoch} +0000")
        };
        (a, c)
    } else {
        (
            format_ident(&author_ident, now),
            format_ident(&committer_ident, now),
        )
    };
    let empty_tree = repo.odb.write(ObjectKind::Tree, &[])?;
    let commit_data = CommitData {
        tree: empty_tree,
        parents: vec![],
        author,
        committer,
        author_raw: Vec::new(),
        committer_raw: Vec::new(),
        encoding: None,
        message: "squash-onto\n".to_string(),
        raw_message: None,
    };
    let bytes = serialize_commit(&commit_data);
    Ok(repo.odb.write(ObjectKind::Commit, &bytes)?)
}

fn load_rebase_ignore_whitespace(rb_dir: &Path) -> bool {
    rb_dir.join("ignore-whitespace").exists()
}

fn load_rebase_rerere_autoupdate(rb_dir: &Path) -> grit_lib::rerere::RerereAutoupdate {
    if rb_dir.join("rerere-autoupdate").exists() {
        grit_lib::rerere::RerereAutoupdate::Yes
    } else if rb_dir.join("no-rerere-autoupdate").exists() {
        grit_lib::rerere::RerereAutoupdate::No
    } else {
        grit_lib::rerere::RerereAutoupdate::FromConfig
    }
}

fn hex_encode_bytes(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for &b in bytes {
        out.push(HEX[(b >> 4) as usize] as char);
        out.push(HEX[(b & 0x0f) as usize] as char);
    }
    out
}

fn hex_decode_string(hex: &str) -> Option<String> {
    if hex.len() % 2 != 0 {
        return None;
    }
    let mut bytes = Vec::with_capacity(hex.len() / 2);
    let mut iter = hex.as_bytes().chunks_exact(2);
    for pair in &mut iter {
        let hi = (pair[0] as char).to_digit(16)? as u8;
        let lo = (pair[1] as char).to_digit(16)? as u8;
        bytes.push((hi << 4) | lo);
    }
    String::from_utf8(bytes).ok()
}

fn load_rebase_merge_favor(rb_dir: &Path) -> MergeFavor {
    let Ok(opts) = fs::read_to_string(rb_dir.join("strategy-opts")) else {
        return MergeFavor::None;
    };
    for encoded in opts.lines() {
        let opt = hex_decode_string(encoded).unwrap_or_else(|| encoded.to_owned());
        match opt.trim_start_matches('-') {
            "ours" => return MergeFavor::Ours,
            "theirs" => return MergeFavor::Theirs,
            _ => {}
        }
    }
    MergeFavor::None
}

fn maybe_run_rebase_merge_strategy(
    repo: &Repository,
    git_dir: &Path,
    rb_dir: &Path,
    commit_oid: ObjectId,
) -> Result<()> {
    let Ok(strategy_raw) = fs::read_to_string(rb_dir.join("strategy")) else {
        return Ok(());
    };
    let strategy = strategy_raw.trim();
    if strategy.is_empty() || matches!(strategy, "ort" | "recursive" | "resolve") {
        return Ok(());
    }
    let commit_obj = repo.odb.read(&commit_oid)?;
    let commit = parse_commit(&commit_obj.data)?;
    let Some(parent_oid) = commit.parents.first().copied() else {
        return Ok(());
    };
    let head_oid = resolve_head(git_dir)?
        .oid()
        .copied()
        .ok_or_else(|| anyhow::anyhow!("HEAD has no OID"))?;
    let mut cmd = std::process::Command::new(format!("git-merge-{strategy}"));
    if let Ok(opts) = fs::read_to_string(rb_dir.join("strategy-opts")) {
        for encoded in opts.lines() {
            if encoded.is_empty() {
                continue;
            }
            let Some(opt) = hex_decode_string(encoded) else {
                continue;
            };
            if opt.starts_with("--") {
                cmd.arg(opt);
            } else {
                cmd.arg(format!("--{opt}"));
            }
        }
    }
    cmd.arg(parent_oid.to_hex())
        .arg("--")
        .arg(head_oid.to_hex())
        .arg(commit_oid.to_hex());
    let status = cmd
        .current_dir(repo.work_tree.as_deref().unwrap_or_else(|| Path::new(".")))
        .status();
    match status {
        Ok(_) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e).context("run rebase merge strategy"),
    }
}

fn load_rebase_replay_commit_opts(rb_dir: &Path) -> RebaseReplayCommitOpts {
    RebaseReplayCommitOpts {
        ignore_space_change: load_rebase_ignore_whitespace(rb_dir),
        committer_date_is_author_date: rb_dir.join("cdate_is_adate").exists(),
        ignore_date: rb_dir.join("ignore_date").exists(),
    }
}

/// Decide whether replayed commits should be signed, mirroring Git's rebase
/// `gpg_sign_opt`: `--no-gpg-sign` disables; `-S`/`--gpg-sign[=<key>]` enables
/// (with the optional key id); otherwise `commit.gpgsign` decides. Returns the
/// signing-key string to persist (empty = default key) or `None` to leave
/// commits unsigned.
fn resolve_rebase_gpg_sign_opt(args: &Args, config: &ConfigSet) -> Option<String> {
    if args.no_gpg_sign {
        return None;
    }
    if let Some(key) = &args.gpg_sign {
        return Some(key.clone());
    }
    if matches!(config.get_bool("commit.gpgsign"), Some(Ok(true))) {
        return Some(String::new());
    }
    None
}

/// Load the persisted signing-key string for replayed commits, if signing is
/// enabled for this rebase.
fn rebase_gpg_sign_opt(rb_dir: &Path) -> Option<String> {
    fs::read_to_string(rb_dir.join("gpg-sign-opt")).ok()
}

/// Render the `-S<key>` signing option for user-facing hints, shell-quoted like Git's
/// `gpg_sign_opt_quoted` (`sq_quotef("-S%s", key)`); empty when this rebase is not signing.
fn gpg_sign_opt_quoted(rb_dir: &Path) -> String {
    let Some(key) = rebase_gpg_sign_opt(rb_dir) else {
        return String::new();
    };
    let raw = format!("-S{key}");
    // Git's `sq_quote` only wraps the value when it contains shell-special characters.
    if raw.chars().all(|c| {
        c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.' | '/' | ':' | ',' | '+' | '=')
    }) {
        raw
    } else {
        shell_single_quote_author_script(&raw)
    }
}

/// Write a replayed commit object, signing it first when this rebase has signing
/// enabled. Mirrors `git commit`/sequencer signing for rebase.
fn write_replayed_commit(
    repo: &Repository,
    rb_dir: &Path,
    config: &ConfigSet,
    committer_line: &str,
    commit_bytes: Vec<u8>,
) -> Result<ObjectId> {
    let bytes = match rebase_gpg_sign_opt(rb_dir) {
        Some(key) => {
            let cfg = grit_lib::signing::GpgConfig::from_config(config)?;
            let committer_default = grit_lib::signing::committer_signing_default(committer_line);
            let key_override = if key.is_empty() {
                None
            } else {
                Some(key.as_str())
            };
            let signing_key = cfg.resolve_signing_key(key_override, &committer_default);
            let signature = grit_lib::signing::sign_buffer(&cfg, &commit_bytes, &signing_key)?;
            grit_lib::signing::add_header_signature(
                &commit_bytes,
                &signature,
                grit_lib::signing::GPG_SIG_HEADER_SHA1,
            )
        }
        None => commit_bytes,
    };
    Ok(repo.odb.write(ObjectKind::Commit, &bytes)?)
}

fn rebase_replayed_author_line(
    raw_author: &str,
    opts: RebaseReplayCommitOpts,
    now: time::OffsetDateTime,
) -> Result<String> {
    if opts.committer_date_is_author_date && !opts.ignore_date {
        let (name, email, date_tail) = split_stored_author_line(raw_author)
            .map_err(|_| anyhow::anyhow!("invalid author identity in replayed commit"))?;
        let tail = date_tail
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .ok_or_else(|| anyhow::anyhow!("corrupt author: missing date information"))?;
        let timestamp =
            grit_lib::commit::parse_date_to_git_timestamp(tail).unwrap_or_else(|| tail.to_string());
        return Ok(format!("{name} <{email}> {timestamp}"));
    }
    if !opts.ignore_date {
        return Ok(raw_author.to_string());
    }
    let (name, email, _) = split_stored_author_line(raw_author)
        .map_err(|_| anyhow::anyhow!("invalid author identity in replayed commit"))?;
    // Match `git am --ignore-date`: author timestamp is wall-clock seconds, timezone +0000.
    let epoch = now.unix_timestamp();
    Ok(format!("{name} <{email}> {epoch} +0000"))
}

fn rebase_replayed_committer_line(
    config: &ConfigSet,
    raw_author: &str,
    opts: RebaseReplayCommitOpts,
    now: time::OffsetDateTime,
) -> Result<String> {
    let committer = resolve_identity(config, "COMMITTER")?;
    if opts.committer_date_is_author_date {
        if opts.ignore_date {
            // With `--reset-author-date`, author is wall-clock epoch at +0000; committer must match
            // `%ci` to `%ai` (t3436 test_ctime_is_atime), not `GIT_COMMITTER_DATE`.
            let epoch = now.unix_timestamp();
            let (cname, cemail) = &committer;
            return Ok(format!("{cname} <{cemail}> {epoch} +0000"));
        }
        let (_, _, date_tail) = split_stored_author_line(raw_author)
            .map_err(|_| anyhow::anyhow!("invalid author identity in replayed commit"))?;
        let tail = date_tail
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .ok_or_else(|| anyhow::anyhow!("corrupt author: missing date information"))?;
        // Match `git am`: pass the author date through `parse_date` / ident formatting, not
        // `@epoch` (which would drop non-UTC zones and break t3436).
        let timestamp =
            grit_lib::commit::parse_date_to_git_timestamp(tail).unwrap_or_else(|| tail.to_string());
        let (cname, cemail) = &committer;
        return Ok(format!("{cname} <{cemail}> {timestamp}"));
    }
    Ok(format_ident(&committer, now))
}

fn load_ws_fix_rule_from_rebase_state(git_dir: &Path) -> Option<u32> {
    let rb_dir = rebase_dir(git_dir);
    let action = fs::read_to_string(rb_dir.join("whitespace-action")).ok()?;
    let a = action.trim();
    if a.eq_ignore_ascii_case("fix") || a.eq_ignore_ascii_case("strip") {
        let config = ConfigSet::load(Some(git_dir), true).unwrap_or_else(|_| ConfigSet::new());
        Some(
            config
                .get("core.whitespace")
                .map(|s| parse_whitespace_rule(&s))
                .unwrap_or(WS_DEFAULT_RULE),
        )
    } else {
        None
    }
}

fn load_rebase_backend(rb_dir: &Path) -> RebaseBackend {
    let marker = fs::read_to_string(rb_dir.join("backend")).unwrap_or_default();
    if marker.trim().eq_ignore_ascii_case("apply") {
        RebaseBackend::Apply
    } else {
        RebaseBackend::Merge
    }
}

fn load_rebase_reflog_action(rb_dir: &Path) -> String {
    fs::read_to_string(rb_dir.join("reflog-action"))
        .ok()
        .map(|s| s.trim().to_owned())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(rebase_reflog_action)
}

fn load_onto_name(rb_dir: &Path) -> Option<String> {
    fs::read_to_string(rb_dir.join("onto-name"))
        .ok()
        .map(|s| s.trim().to_owned())
        .filter(|s| !s.is_empty())
}

/// Append one `old_oid new_oid` line to `.git/<rebase-dir>/rewritten` for the post-rewrite hook.
fn append_rebase_rewrite_line(rb_dir: &Path, old_oid: &ObjectId, new_oid: &ObjectId) -> Result<()> {
    let path = rb_dir.join("rewritten");
    let mut f = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .with_context(|| format!("open {}", path.display()))?;
    writeln!(f, "{} {}", old_oid.to_hex(), new_oid.to_hex())?;
    Ok(())
}

fn rebase_rewritten_pending_path(rb_dir: &Path) -> PathBuf {
    rb_dir.join("rewritten-pending")
}

fn rebase_rewritten_list_path(rb_dir: &Path) -> PathBuf {
    rb_dir.join("rewritten")
}

/// Append `old_oid` to `rewritten-pending`, then flush pending lines to `rewritten` when
/// `next_command` is not `fixup`/`squash` (matches Git's `record_in_rewritten`).
///
/// After a successful pick, pass the next todo command (`peek_command(..., 1)`). When recording
/// from `stopped-sha` during `rebase --continue` after `--skip`, pass the current (skipped) line's
/// command (`peek_command(..., 0)`).
fn record_rebase_in_rewritten_pending(
    git_dir: &Path,
    rb_dir: &Path,
    old_oid: &ObjectId,
    next_command: Option<RebaseTodoCmd>,
) -> Result<()> {
    let pending = rebase_rewritten_pending_path(rb_dir);
    let mut f = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&pending)
        .with_context(|| format!("open {}", pending.display()))?;
    writeln!(f, "{}", old_oid.to_hex())?;

    let flush = match next_command {
        None => true,
        Some(RebaseTodoCmd::Fixup | RebaseTodoCmd::Squash) => false,
        Some(RebaseTodoCmd::Pick | RebaseTodoCmd::Reword) => true,
    };
    if flush {
        flush_rebase_rewritten_pending(git_dir, rb_dir)?;
    }
    Ok(())
}

fn flush_rebase_rewritten_pending(git_dir: &Path, rb_dir: &Path) -> Result<()> {
    let pending_path = rebase_rewritten_pending_path(rb_dir);
    let Ok(s) = fs::read_to_string(&pending_path) else {
        return Ok(());
    };
    if s.trim().is_empty() {
        let _ = fs::remove_file(&pending_path);
        return Ok(());
    }
    let new_oid = resolve_head(git_dir)?
        .oid()
        .cloned()
        .ok_or_else(|| anyhow::anyhow!("HEAD has no OID"))?;
    let list_path = rebase_rewritten_list_path(rb_dir);
    let mut out = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&list_path)
        .with_context(|| format!("open {}", list_path.display()))?;
    for line in s.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        writeln!(out, "{} {}", line, new_oid.to_hex())?;
    }
    let _ = fs::remove_file(&pending_path);
    Ok(())
}

/// If `rewritten` has content, run `post-rewrite rebase` with that file on stdin (matches `git am`).
fn run_post_rewrite_after_rebase(repo: &Repository, rb_dir: &Path) {
    let path = rebase_rewritten_list_path(rb_dir);
    let Ok(meta) = fs::metadata(&path) else {
        return;
    };
    if meta.len() == 0 {
        return;
    }
    let Ok(bytes) = fs::read(&path) else {
        return;
    };
    if let Ok(self_exe) = std::env::current_exe() {
        if let Ok(mut child) = std::process::Command::new(self_exe)
            .args(["notes", "copy", "--for-rewrite=rebase"])
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
        {
            if let Some(mut stdin) = child.stdin.take() {
                let _ = stdin.write_all(&bytes);
            }
            let _ = child.wait();
        }
    }
    let _ = run_hook(repo, "post-rewrite", &["rebase"], Some(&bytes));
}

/// Message to record when replaying `commit` during a root rebase.
///
/// For two-parent merges, Git records the second parent's subject (the merged branch tip), not the
/// default merge message, when flattening history onto a new base.
fn message_for_root_replayed_commit(
    repo: &Repository,
    commit: &CommitData,
    root_rebase: bool,
) -> String {
    if root_rebase && commit.parents.len() == 2 {
        if let Ok(p2_obj) = repo.odb.read(&commit.parents[1]) {
            if let Ok(p2) = parse_commit(&p2_obj.data) {
                return p2.message;
            }
        }
    }
    commit.message.clone()
}

fn read_autostash_oid(rb_dir: &Path) -> Result<Option<ObjectId>> {
    let p = rb_dir.join("autostash");
    if !p.exists() {
        return Ok(None);
    }
    let s = fs::read_to_string(&p).unwrap_or_default();
    let hex = s.trim();
    if hex.len() != 40 || !hex.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Ok(None);
    }
    Ok(Some(ObjectId::from_hex(hex)?))
}

fn reset_index_to_head(repo: &Repository, git_dir: &Path) -> Result<()> {
    let head_oid = resolve_head(git_dir)?
        .oid()
        .cloned()
        .ok_or_else(|| anyhow::anyhow!("cannot reset index: HEAD is unborn"))?;
    let obj = repo.odb.read(&head_oid)?;
    let commit = parse_commit(&obj.data)?;
    let entries = tree_to_index_entries(repo, &commit.tree, "")?;
    let mut index = Index::new();
    index.entries = entries;
    index.sort();
    // `tree_to_index_entries` zeroes the stat cache; populate it from the work tree so a following
    // `git diff-files` does not report every entry as modified (the index would otherwise carry no
    // mtime/size and diff would re-hash nothing). Matches Git refreshing the index after rebase
    // (t3426 `test_superproject_content`: `git diff-files --ignore-submodules` must be empty).
    refresh_index_stat_cache_from_worktree(repo, &mut index)?;
    repo.write_index(&mut index)?;
    Ok(())
}

/// Matches Git's `git_editor()` (see [`crate::editor::resolve_git_editor`]).
fn git_editor_cmd(config: &ConfigSet) -> Result<String> {
    if std::env::var("GIT_EDITOR").is_err()
        && config.get("core.editor").is_none()
        && std::env::var("VISUAL")
            .ok()
            .is_some_and(|visual| visual.trim() == ":")
    {
        if let Ok(editor) = std::env::var("EDITOR") {
            if !editor.trim().is_empty() && editor.trim() != ":" {
                return Ok(editor);
            }
        }
    }
    // Preserve `EDITOR=:` as Git's no-op editor while still ignoring placeholder `VISUAL=:`
    // when tests install a fake editor in `EDITOR`.
    crate::editor::resolve_commit_launch_editor(config)
        .ok_or_else(|| anyhow::anyhow!("Terminal is dumb, but EDITOR unset"))
}

fn branch_checked_out_in_linked_worktree(repo: &Repository, branch: &str) -> Option<String> {
    let common = grit_lib::refs::common_dir(&repo.git_dir).unwrap_or_else(|| repo.git_dir.clone());
    let worktrees = common.join("worktrees");
    let target = format!("ref: refs/heads/{branch}");
    let entries = fs::read_dir(worktrees).ok()?;
    for entry in entries.flatten() {
        let admin = entry.path();
        let Ok(head) = fs::read_to_string(admin.join("HEAD")) else {
            continue;
        };
        if head.trim() != target {
            continue;
        }
        let path = fs::read_to_string(admin.join("gitdir"))
            .ok()
            .and_then(|gitdir| {
                Path::new(gitdir.trim())
                    .parent()
                    .map(|p| p.display().to_string())
            })
            .unwrap_or_else(|| admin.display().to_string());
        if !crate::commands::worktree_refs::worktree_paths_equal_pub(
            &path,
            &crate::commands::worktree_refs::current_worktree_path_for_repo(repo),
        ) {
            return Some(path);
        }
    }
    None
}

/// Resolves the program to run for `rebase -i` / autosquash todo editing (`git_sequence_editor`).
///
/// `GIT_SEQUENCE_EDITOR` and `sequence.editor` take precedence; otherwise falls back to
/// [`git_editor_cmd`].
fn sequence_editor_cmd(config: &ConfigSet) -> Result<String> {
    if let Ok(seq) = std::env::var("GIT_SEQUENCE_EDITOR") {
        let s = seq.trim();
        if !s.is_empty() {
            return Ok(seq);
        }
    }
    if let Some(seq) = config.get("sequence.editor") {
        let s = seq.trim();
        if !s.is_empty() {
            return Ok(seq);
        }
    }
    git_editor_cmd(config)
}

/// Warn (once per process) that `core.commentChar=auto` / `core.commentString=auto` is deprecated,
/// matching Git's `check_auto_comment_char_config` (config.c). The warning and the follow-up advice
/// are emitted to stderr just before the rebase sequence editor opens (the comment string is needed
/// to render the todo help block). Git uses the `GIT_AUTO_COMMENT_CHAR_CONFIG_WARNING_GIVEN`
/// environment variable to suppress repeats in subprocesses; we honor the same guard.
///
/// The advice's `git config ...` lines carry a scope flag (`--global`, `--system`, `--worktree`)
/// for non-local origins, mirroring `add_config_scope_arg`; repository-local config gets no flag.
fn warn_auto_comment_char_once(config: &ConfigSet) {
    const WARN_ENV: &str = "GIT_AUTO_COMMENT_CHAR_CONFIG_WARNING_GIVEN";

    // Find the last (highest-priority) entry for commentChar/commentString that is set to "auto".
    // Git tracks `last_key_id`/`auto_set_in_file`; the effective value is the highest-priority one.
    let mut auto_entry: Option<(&str, ConfigScope)> = None;
    for entry in config.entries() {
        let key_name = match entry.key.as_str() {
            "core.commentchar" => "core.commentChar",
            "core.commentstring" => "core.commentString",
            _ => continue,
        };
        let is_auto = entry
            .value
            .as_deref()
            .is_some_and(|v| v.trim().eq_ignore_ascii_case("auto"));
        if is_auto {
            auto_entry = Some((key_name, entry.scope));
        } else {
            // A later non-auto override for the same key clears the auto state.
            auto_entry = None;
        }
    }
    let Some((key_name, scope)) = auto_entry else {
        return;
    };

    if std::env::var("GIT_AUTO_COMMENT_CHAR_CONFIG_WARNING_GIVEN")
        .ok()
        .as_deref()
        .is_some_and(|v| matches!(v, "1" | "true" | "yes" | "on"))
    {
        return;
    }
    // Single-threaded CLI process; set before spawning the editor subprocess so it does not repeat.
    std::env::set_var(WARN_ENV, "true");

    let scope_flag = match scope {
        ConfigScope::Global => "--global ",
        ConfigScope::System => "--system ",
        ConfigScope::Worktree => "--worktree ",
        ConfigScope::Local | ConfigScope::Command => "",
    };

    eprintln!(
        "warning: Support for '{key_name}=auto' is deprecated and will be removed in Git 3.0"
    );
    eprintln!("hint: ");
    eprintln!("hint: To use the default comment string (#) please run");
    eprintln!("hint: ");
    eprintln!("hint:     git config unset {scope_flag}{key_name}");
    eprintln!("hint: ");
    eprintln!("hint: To set a custom comment string please run");
    eprintln!("hint: ");
    eprintln!("hint:     git config set {scope_flag}{key_name} <comment string>");
    eprintln!("hint: ");
    eprintln!("hint: where '<comment string>' is the string you wish to use.");
}

fn run_shell_editor(editor: &str, path: &Path) -> Result<std::process::ExitStatus> {
    let status = if editor.trim() == ":" {
        std::process::Command::new("true").status()
    } else {
        let mut cmd = std::process::Command::new("sh");
        cmd.arg("-c")
            .arg(format!("{} \"$@\"", editor))
            .arg(editor)
            .arg(path)
            .env_remove(INTERNAL_REBASE_PICK_ENV)
            .env_remove(INTERNAL_REBASE_FORCE_FF_ENV)
            .stdin(std::process::Stdio::inherit())
            .stdout(std::process::Stdio::inherit())
            .stderr(std::process::Stdio::inherit());
        cmd.status()
    }
    .context("failed to run editor")?;
    Ok(status)
}

/// Opens `GIT_EDITOR` on `COMMIT_EDITMSG` after seeding it and running `prepare-commit-msg`.
///
/// `prepare_arg1` / optional `prepare_arg2` are the hook arguments after the message file path
/// (`reword`, `squash`, `message`, or `commit` + `HEAD`, …), matching Git's `git commit -e` path
/// during interactive rebase.
fn run_commit_editor_for_template(
    repo: &Repository,
    git_dir: &Path,
    template: &str,
    prepare_arg1: &str,
    prepare_arg2: Option<&str>,
) -> Result<String> {
    let editmsg = git_dir.join("COMMIT_EDITMSG");
    fs::write(&editmsg, template)?;
    run_prepare_commit_msg_hook(repo, &editmsg, prepare_arg1, prepare_arg2, None)?;
    let config = ConfigSet::load(Some(git_dir), true)?;
    let editor = git_editor_cmd(&config)?;
    let status = run_shell_editor(&editor, &editmsg)?;
    if !status.success() {
        bail!("there was a problem with the editor");
    }
    fs::read_to_string(&editmsg).context("read COMMIT_EDITMSG after editor")
}

/// Post-`COMMIT_EDITMSG` handling for `reword`: strip comment-prefixed lines like `git commit` (using
/// `core.commentChar` / `core.commentString`), then apply [`rebase_commit_msg_cleanup`].
/// Whitespace-only messages abort the rebase, matching Git's sequencer (`t3405-rebase-malformed`).
fn message_from_reword_editor(raw: &str, msg_cleanup: &str, config: &ConfigSet) -> Result<String> {
    let comment_prefix = comment_line_prefix_full(config);
    let stripped = cleanup_edited_commit_message(raw, comment_prefix.as_ref());
    if stripped.trim().is_empty() {
        eprintln!("Aborting commit due to empty commit message.");
        bail!("empty commit message");
    }
    Ok(apply_commit_msg_cleanup(&stripped, msg_cleanup))
}

fn run_commit_editor_for_reword(
    repo: &Repository,
    git_dir: &Path,
    template: &str,
) -> Result<String> {
    let mut edit_template = template.to_owned();
    if !edit_template.ends_with("\n\n") {
        if !edit_template.ends_with('\n') {
            edit_template.push('\n');
        }
        edit_template.push('\n');
    }
    run_commit_editor_for_template(repo, git_dir, &edit_template, "commit", Some("HEAD"))
}

/// Opens the editor for a `git rebase --continue` after an `edit` stop.
///
/// Git's `commit_staged_changes` amends the edited commit with
/// `run_git_commit(rebase_path_message(), …, AMEND_MSG | EDIT_MSG)`, i.e. `git commit --amend -e -F
/// <message>`. Because the message is supplied via `-F`, the `prepare-commit-msg` hook receives
/// arg1="message" (`builtin/commit.c` precedence), unlike `reword` which passes "commit"/"HEAD".
fn run_edit_continue_editor(repo: &Repository, git_dir: &Path, template: &str) -> Result<String> {
    let mut edit_template = template.to_owned();
    if !edit_template.ends_with("\n\n") {
        if !edit_template.ends_with('\n') {
            edit_template.push('\n');
        }
        edit_template.push('\n');
    }
    run_commit_editor_for_template(repo, git_dir, &edit_template, "message", None)
}

fn worktree_matches_head(repo: &Repository, git_dir: &Path) -> Result<bool> {
    let Some(wt) = repo.work_tree.as_deref() else {
        return Ok(true);
    };
    let idx = repo.load_index().context("failed to read index")?;
    let head_tree = resolve_head(git_dir)?.oid().and_then(|oid| {
        let obj = repo.odb.read(oid).ok()?;
        parse_commit(&obj.data).ok().map(|c| c.tree)
    });
    let staged = grit_lib::diff::diff_index_to_tree(&repo.odb, &idx, head_tree.as_ref(), true)?;
    let mut unstaged = grit_lib::diff::diff_index_to_worktree(&repo.odb, &idx, wt, false, false)?;
    unstaged.retain(|e| e.old_mode != "160000" && e.new_mode != "160000");
    Ok(staged.is_empty() && unstaged.is_empty())
}

/// `rebase.missingCommitsCheck` level (Git's `get_missing_commit_check_level`).
#[derive(Clone, Copy, PartialEq, Eq)]
enum MissingCommitCheck {
    Ignore,
    Warn,
    Error,
}

/// Reads `rebase.missingCommitsCheck`; unknown values warn and behave as `ignore`.
fn missing_commit_check_level(config: &ConfigSet) -> MissingCommitCheck {
    match config.get("rebase.missingCommitsCheck") {
        None => MissingCommitCheck::Ignore,
        Some(v) => match v.trim().to_ascii_lowercase().as_str() {
            "ignore" => MissingCommitCheck::Ignore,
            "warn" => MissingCommitCheck::Warn,
            "error" => MissingCommitCheck::Error,
            other => {
                eprintln!(
                    "warning: unrecognized setting {other} for option rebase.missingCommitsCheck. Ignoring."
                );
                MissingCommitCheck::Ignore
            }
        },
    }
}

/// Lower-cased first whitespace-delimited token (the todo command keyword) of a non-comment line.
fn todo_command_word(line: &str) -> Option<String> {
    let t = line.trim();
    if t.is_empty() || rebase_todo_line_is_comment(t) {
        return None;
    }
    t.split_whitespace().next().map(|w| w.to_ascii_lowercase())
}

/// True if the keyword is a recognized interactive-rebase command (Git's `parse_insn_line`).
fn is_known_todo_command(word: &str) -> bool {
    matches!(
        word,
        "pick"
            | "p"
            | "reword"
            | "r"
            | "edit"
            | "e"
            | "squash"
            | "s"
            | "fixup"
            | "f"
            | "drop"
            | "d"
            | "exec"
            | "x"
            | "break"
            | "b"
            | "label"
            | "l"
            | "reset"
            | "t"
            | "merge"
            | "m"
            | "update-ref"
            | "u"
            | "noop"
    )
}

/// True if the command operates on a commit object (and so participates in the missing-commit check).
fn todo_command_is_pick_like(word: &str) -> bool {
    matches!(
        word,
        "pick"
            | "p"
            | "reword"
            | "r"
            | "edit"
            | "e"
            | "squash"
            | "s"
            | "fixup"
            | "f"
            | "drop"
            | "d"
    )
}

/// True if the command is a fixup/squash (which may not be the first actionable command).
fn todo_command_is_fixup(word: &str) -> bool {
    matches!(word, "squash" | "s" | "fixup" | "f")
}

/// Validate a `label` / `update-ref` argument, returning the error message if invalid.
///
/// Mirrors Git's `check_label_or_ref_arg` (sequencer.c): a `label` may not be `#` (reserved by
/// the merge command) and must pass `check_refname_format` with one-level allowed; an
/// `update-ref` refname must pass one-level validation and additionally be fully qualified
/// (multi-component), else the `update-ref requires a fully qualified refname …` message.
///
/// # Parameters
/// - `word`: the lowercased command word (`label`/`l` or `update-ref`/`u`).
/// - `arg`: the argument token.
///
/// # Returns
/// `Some(message)` describing the violation, or `None` when the argument is valid.
fn check_label_or_ref_arg(word: &str, arg: &str) -> Option<String> {
    use grit_lib::check_ref_format::{check_refname_format, RefNameOptions};
    let onelevel = RefNameOptions {
        allow_onelevel: true,
        refspec_pattern: false,
        normalize: false,
    };
    let fully_qualified = RefNameOptions {
        allow_onelevel: false,
        refspec_pattern: false,
        normalize: false,
    };
    if word == "label" || word == "l" {
        if arg == "#" || check_refname_format(arg, &onelevel).is_err() {
            return Some(format!("'{arg}' is not a valid label"));
        }
        return None;
    }
    // update-ref / u
    if check_refname_format(arg, &onelevel).is_err() {
        return Some(format!("'{arg}' is not a valid refname"));
    }
    if check_refname_format(arg, &fully_qualified).is_err() {
        return Some(format!(
            "update-ref requires a fully qualified refname e.g. refs/heads/{arg}"
        ));
    }
    None
}

/// Report a non-merge todo command targeting a merge commit (Git's `check_merge_commit_insn`).
///
/// `pick`/`reword`/`edit` print `'<cmd>' does not accept merge commits` plus a per-command
/// `hint:` advice block (suppressed when `advice_enabled` is false); `fixup`/`squash` print
/// `cannot squash merge commit into another commit`. `drop`/`merge`/`label`/etc. are accepted.
///
/// # Parameters
/// - `repo`: repository to look up the commit's parent count.
/// - `word`: lowercased command word.
/// - `oid`: the targeted commit.
/// - `advice_enabled`: whether `advice.rebaseTodoError` advice should be emitted.
///
/// # Returns
/// `true` when the command rejects this (merge) commit (caller emits `invalid line N`), else
/// `false`.
fn report_merge_commit_rejection(
    repo: &Repository,
    word: &str,
    oid: ObjectId,
    advice_enabled: bool,
) -> bool {
    let is_merge = repo
        .odb
        .read(&oid)
        .ok()
        .and_then(|obj| parse_commit(&obj.data).ok())
        .map(|c| c.parents.len() > 1)
        .unwrap_or(false);
    if !is_merge {
        return false;
    }
    let disable = "hint: Disable this message with \"git config set advice.rebaseTodoError false\"";
    match word {
        "pick" | "p" => {
            eprintln!("error: 'pick' does not accept merge commits");
            if advice_enabled {
                eprintln!("hint: 'pick' does not take a merge commit. If you wanted to");
                eprintln!("hint: replay the merge, use 'merge -C' on the commit.");
                eprintln!("{disable}");
            }
            true
        }
        "reword" | "r" => {
            eprintln!("error: 'reword' does not accept merge commits");
            if advice_enabled {
                eprintln!("hint: 'reword' does not take a merge commit. If you wanted to");
                eprintln!("hint: replay the merge and reword the commit message, use");
                eprintln!("hint: 'merge -c' on the commit");
                eprintln!("{disable}");
            }
            true
        }
        "edit" | "e" => {
            eprintln!("error: 'edit' does not accept merge commits");
            if advice_enabled {
                eprintln!("hint: 'edit' does not take a merge commit. If you wanted to");
                eprintln!("hint: replay the merge, use 'merge -C' on the commit, and then");
                eprintln!("hint: 'break' to give the control back to you so that you can");
                eprintln!("hint: do 'git commit --amend && git rebase --continue'.");
                eprintln!("{disable}");
            }
            true
        }
        "fixup" | "f" | "squash" | "s" => {
            eprintln!("error: cannot squash merge commit into another commit");
            true
        }
        _ => false,
    }
}

/// Validates the edited interactive-rebase todo before execution (Git's `todo_list_parse_insn_buffer`
/// + `todo_list_check`).
///
/// On any structural problem (unknown command, bad SHA-1, leading fixup, or — with
/// `rebase.missingCommitsCheck = error` — a dropped commit) this writes the `git-rebase-todo.backup`
/// and returns an error carrying the same advice Git prints. With `missingCommitsCheck = warn` it
/// prints the warning but allows the rebase to proceed.
fn validate_edited_interactive_todo(
    repo: &Repository,
    rb_merge: &Path,
    original_todo: &str,
    edited: &str,
    config: &ConfigSet,
) -> Result<()> {
    const ADVICE: &str = "You can fix this with 'git rebase --edit-todo' and then run 'git rebase --continue'.\nOr you can abort the rebase with 'git rebase --abort'.";

    // Backup the original todo (with help) so `--edit-todo` recovery and tests can restore it.
    let _ = fs::create_dir_all(rb_merge);
    let _ = fs::write(
        rb_merge.join("git-rebase-todo.backup"),
        original_todo.as_bytes(),
    );

    // Honour `core.commentchar`: the editor may comment lines with the configured prefix (e.g. `\`),
    // which must be skipped rather than parsed as commands (t3404 "respects core.commentchar").
    let comment_prefix = comment_line_prefix_full(config);
    let is_comment = |t: &str| -> bool {
        t.starts_with(comment_prefix.as_ref()) || rebase_todo_line_is_comment(t)
    };

    // Structural validation: unknown commands, bad SHAs, bad labels/refnames, merge commits on
    // non-merge commands, and a leading fixup all abort with advice. Git's
    // `todo_list_parse_insn_buffer` continues past a per-line error (printing `error: invalid line
    // N` for each), then aborts once at the end, so we accumulate rather than early-return.
    let advice_enabled = config
        .get_bool("advice.rebaseTodoError")
        .and_then(|r| r.ok())
        .unwrap_or(true);
    let mut fixup_okay = fs::read_to_string(rb_merge.join("done"))
        .map(|s| !s.trim().is_empty())
        .unwrap_or(false);
    let mut had_error = false;
    for (idx, raw) in edited.lines().enumerate() {
        let t = raw.trim();
        if t.is_empty() || is_comment(t) {
            continue;
        }
        let Some(word) = todo_command_word(t) else {
            continue;
        };
        if !is_known_todo_command(&word) {
            // Git prints both the offending command word and the full line (parse_insn_line then
            // todo_list_parse_insn_buffer); the original-case word is what the user typed.
            let original_word = t.split_whitespace().next().unwrap_or(&word);
            eprintln!("error: invalid command '{original_word}'");
            eprintln!("error: invalid line {}: {t}", idx + 1);
            had_error = true;
            continue;
        }
        // Validate `label`/`update-ref` argument format (Git `check_label_or_ref_arg`).
        if word == "label" || word == "l" || word == "update-ref" || word == "u" {
            let arg = t
                .split_whitespace()
                .nth(1)
                .map(str::to_owned)
                .unwrap_or_default();
            if let Some(msg) = check_label_or_ref_arg(&word, &arg) {
                eprintln!("error: {msg}");
                eprintln!("error: invalid line {}: {t}", idx + 1);
                had_error = true;
            }
            continue;
        }
        // Validate that pick-like commands carry a resolvable commit and (for non-merge commands)
        // do not target a merge commit.
        if todo_command_is_pick_like(&word) {
            match todo_line_commit_oid(repo, t) {
                Some(oid) => {
                    if report_merge_commit_rejection(repo, &word, oid, advice_enabled) {
                        eprintln!("error: invalid line {}: {t}", idx + 1);
                        had_error = true;
                        continue;
                    }
                }
                None => {
                    eprintln!("error: invalid line {}: {t}", idx + 1);
                    had_error = true;
                    continue;
                }
            }
        }
        let is_noop = word == "noop" || word == "drop" || word == "d";
        if !fixup_okay && todo_command_is_fixup(&word) {
            eprintln!("error: cannot '{word}' without a previous commit");
            had_error = true;
        }
        if !is_noop && !todo_command_is_fixup(&word) {
            fixup_okay = true;
        }
    }
    if had_error {
        eprintln!("{ADVICE}");
        return Err(crate::explicit_exit::SilentNonZeroExit { code: 1 }.into());
    }

    // Missing-commit check (`rebase.missingCommitsCheck`).
    let level = missing_commit_check_level(config);
    if level == MissingCommitCheck::Ignore {
        return Ok(());
    }

    // Collect the OIDs that survive into the edited todo. An explicit `drop` still counts the commit
    // as "seen" (Git marks dropped commits in `commit_seen`), so only commits removed entirely from
    // the list are reported as accidentally dropped.
    let mut kept: std::collections::HashSet<ObjectId> = std::collections::HashSet::new();
    for line in rebase_todo_actionable_lines(edited) {
        let Some(word) = todo_command_word(line) else {
            continue;
        };
        if !todo_command_is_pick_like(&word) {
            continue;
        }
        if let Some(oid) = todo_line_commit_oid(repo, line) {
            kept.insert(oid);
        }
    }
    // Commits already replayed (recorded in `done`) are likewise "seen" and must not be reported as
    // dropped when re-checking the todo mid-rebase (`--edit-todo` after a `break`/conflict).
    if let Ok(done) = fs::read_to_string(rb_merge.join("done")) {
        for line in rebase_todo_actionable_lines(&done) {
            if let Some(oid) = todo_line_commit_oid(repo, line) {
                kept.insert(oid);
            }
        }
    }

    // Walk the original todo newest-to-oldest; report commits dropped (not present, not `drop`ped).
    let mut missing = String::new();
    let mut reported: std::collections::HashSet<ObjectId> = std::collections::HashSet::new();
    let original_lines: Vec<&str> = rebase_todo_actionable_lines(original_todo);
    for line in original_lines.iter().rev() {
        let Some(word) = todo_command_word(line) else {
            continue;
        };
        if !todo_command_is_pick_like(&word) || word == "drop" || word == "d" {
            continue;
        }
        let Some(oid) = todo_line_commit_oid(repo, line) else {
            continue;
        };
        if kept.contains(&oid) || reported.contains(&oid) {
            continue;
        }
        reported.insert(oid);
        // Honour `core.abbrev` for the dropped-commit short hashes (Git uses `%h` semantics);
        // tests compute the expectation with `git log --format=%h`, which respects the config.
        let abbrev = abbreviate_object_id(repo, oid, rebase_core_abbrev_len(config))
            .unwrap_or_else(|_| oid.to_hex());
        let subject = commit_subject_for_oid(repo, oid);
        missing.push_str(&format!(" - {abbrev} # {subject}\n"));
    }

    if missing.is_empty() {
        return Ok(());
    }

    eprintln!("Warning: some commits may have been dropped accidentally.");
    eprintln!("Dropped commits (newer to older):");
    eprint!("{missing}");
    eprintln!("To avoid this message, use \"drop\" to explicitly remove a commit.");
    if level == MissingCommitCheck::Error {
        eprintln!();
        eprintln!("Use 'git config rebase.missingCommitsCheck' to change the level of warnings.");
        eprintln!("The possible behaviours are: ignore, warn, error.");
        eprintln!();
        eprintln!("{ADVICE}");
        return Err(crate::explicit_exit::SilentNonZeroExit { code: 1 }.into());
    }
    Ok(())
}

/// Extracts the commit OID a pick-like todo line refers to (`pick`/`reword`/`edit`/`squash`/`fixup`/
/// `drop` <commit> ...), resolving short SHAs and the `-C`/`-c` fixup flag form.
fn todo_line_commit_oid(repo: &Repository, line: &str) -> Option<ObjectId> {
    let t = line.trim();
    let mut parts = t.split_whitespace();
    let word = parts.next()?.to_ascii_lowercase();
    if !todo_command_is_pick_like(&word) {
        return None;
    }
    let mut next = parts.next()?;
    // Skip a leading `-C`/`-c[ ]<commit>` flag on fixup lines.
    if todo_command_is_fixup(&word) {
        if next.eq_ignore_ascii_case("-C") || next.eq_ignore_ascii_case("-c") {
            next = parts.next()?;
        } else if next.len() > 2 && (next.starts_with("-C") || next.starts_with("-c")) {
            next = &next[2..];
        }
    }
    if next.len() == 40 && next.bytes().all(|b| b.is_ascii_hexdigit()) {
        ObjectId::from_hex(next).ok()
    } else {
        resolve_revision(repo, next).ok()
    }
}

/// Single-line subject for a commit OID (empty string if it cannot be read).
fn commit_subject_for_oid(repo: &Repository, oid: ObjectId) -> String {
    repo.odb
        .read(&oid)
        .ok()
        .and_then(|obj| parse_commit(&obj.data).ok())
        .map(|c| commit_subject_single_line(&c.message))
        .unwrap_or_default()
}

/// Returns trimmed non-comment todo lines as edited (for replay), and pick/fixup/squash entries for
/// empty-list / up-to-date checks, plus the original (with-help) and raw-edited todo bodies for the
/// post-state static check.
#[allow(clippy::too_many_arguments, clippy::type_complexity)]
fn run_interactive_rebase(
    repo: &Repository,
    git_dir: &Path,
    commits: &[ObjectId],
    config: &ConfigSet,
    autostash_oid: Option<&ObjectId>,
    autosquash: bool,
    update_refs: bool,
    exec_cmds: &[String],
    revs_onto: Option<(&str, &str)>,
) -> Result<(
    Vec<String>,
    Vec<(ObjectId, RebaseTodoCmd)>,
    bool,
    String,
    String,
)> {
    if autosquash {
        validate_rebase_instruction_format(config)?;
    }
    let items = build_autosquash_with_update_refs(repo, git_dir, commits, autosquash, update_refs)?;
    // Git's `complete_action` runs `todo_list_add_exec_commands` after the autosquash rearrange, so
    // `--exec` lines are interleaved into the visible todo after each pick chain (t3404 #70, #107).
    let items = insert_exec_commands(items, exec_cmds);
    let comment_prefix = comment_line_prefix_full(config);
    let mut todo = String::new();
    for item in &items {
        match item {
            TodoBuildItem::Commit(oid, cmd) => {
                todo.push_str(&format_rebase_todo_line(repo, oid, *cmd, config, true)?);
                todo.push('\n');
            }
            TodoBuildItem::UpdateRef(refname) => {
                todo.push_str(&format!("update-ref {refname}\n"));
            }
            TodoBuildItem::RefComment(text) => {
                todo.push_str(&format!("{comment_prefix} {text}\n"));
            }
            TodoBuildItem::Exec(cmd) => {
                let kw = if rebase_abbreviate_commands(config) {
                    "x"
                } else {
                    "exec"
                };
                todo.push_str(&format!("{kw} {cmd}\n"));
            }
        }
    }
    let rb_merge = rebase_merge_dir(git_dir);
    let _ = fs::remove_dir_all(&rb_merge);
    fs::create_dir_all(&rb_merge)?;
    fs::write(rb_merge.join("interactive"), "")?;
    let todo_path = rb_merge.join("git-rebase-todo");
    // Git appends the instruction help (a comment block preceded by a blank line) to the todo
    // before opening the editor, so the file the user/test sees ends with `<todo>\n\n# Rebase ...`.
    // After `grep -v '^#'`, the separating blank line remains, which several t3404 `--exec` tests
    // depend on when calibrating `sed 1,Nd`.
    let command_count = count_rebase_todo_actionable_lines(&todo);
    let todo_with_help = append_rebase_todo_help(&todo, command_count, revs_onto, config);
    fs::write(&todo_path, todo_with_help.as_bytes())?;
    warn_auto_comment_char_once(config);
    let editor = sequence_editor_cmd(config)?;
    let status = run_shell_editor(&editor, &todo_path)?;
    let edited = fs::read_to_string(&todo_path)?;
    let _ = fs::remove_dir_all(&rb_merge);
    if !status.success() {
        if worktree_matches_head(repo, git_dir)? {
            if let Some(oid) = autostash_oid {
                let _ = stash::pop_autostash_if_top(repo, oid);
            }
        }
        bail!("there was a problem with the editor");
    }
    let mut lines: Vec<String> = Vec::new();
    let mut pick_like: Vec<(ObjectId, RebaseTodoCmd)> = Vec::new();
    for line in edited.lines() {
        let t = line.trim();
        if t.is_empty() || rebase_todo_line_is_comment(t) {
            continue;
        }
        lines.push(t.to_owned());
        if let Some(pair) = parse_todo_line_with_repo(Some(repo), t)? {
            pick_like.push(pair);
        }
    }
    let original_lines = todo
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty() && !rebase_todo_line_is_comment(line))
        .map(ToOwned::to_owned)
        .collect::<Vec<_>>();
    let changed = lines != original_lines;
    Ok((lines, pick_like, changed, todo, edited))
}

fn flush_rebase_stdout() {
    let _ = io::stdout().flush();
}

/// Interactive rebase with a pre-generated todo script (e.g. `--rebase-merges`).
/// Append the interactive-rebase instruction help (Git's `append_todo_help`) to a todo body.
///
/// Mirrors `rebase-interactive.c`: a blank line, the `Rebase <revs> onto <onto> (N commands)`
/// header (only when `revs_onto` is provided, i.e. not an `--edit-todo` re-edit), then the
/// commands reference and a final warning, all as comment lines. The exact wording is only
/// observable through `grep`/`sed` on `ORIGINAL-TODO` in tests; what matters everywhere is the
/// leading blank line separating the todo body from the comments.
fn append_rebase_todo_help(
    body: &str,
    command_count: usize,
    revs_onto: Option<(&str, &str)>,
    config: &ConfigSet,
) -> String {
    fn add_commented(out: &mut String, cp: &str, text: &str) {
        for line in text.split('\n') {
            if line.is_empty() {
                out.push_str(cp);
                out.push('\n');
            } else {
                out.push_str(cp);
                out.push(' ');
                out.push_str(line);
                out.push('\n');
            }
        }
    }
    let cp = comment_line_prefix_full(config);
    let mut out = String::from(body);
    if !out.ends_with('\n') {
        out.push('\n');
    }
    if let Some((revs, onto)) = revs_onto {
        out.push('\n');
        let noun = if command_count == 1 {
            "command"
        } else {
            "commands"
        };
        let header = format!("Rebase {revs} onto {onto} ({command_count} {noun})");
        out.push_str(&cp);
        out.push(' ');
        out.push_str(&header);
        out.push('\n');
    }
    let commands = "\nCommands:\n\
p, pick <commit> = use commit\n\
r, reword <commit> = use commit, but edit the commit message\n\
e, edit <commit> = use commit, but stop for amending\n\
s, squash <commit> = use commit, but meld into previous commit\n\
f, fixup [-C | -c] <commit> = like \"squash\" but keep only the previous\n\
                   commit's log message, unless -C is used, in which case\n\
                   keep only this commit's message; -c is same as -C but\n\
                   opens the editor\n\
x, exec <command> = run command (the rest of the line) using shell\n\
b, break = stop here (continue rebase later with 'git rebase --continue')\n\
d, drop <commit> = remove commit\n\
l, label <label> = label current HEAD with a name\n\
t, reset <label> = reset HEAD to a label\n\
m, merge [-C <commit> | -c <commit>] <label> [# <oneline>]\n\
        create a merge commit using the original merge commit's\n\
        message (or the oneline, if no original merge commit was\n\
        specified); use -c <commit> to reword the commit message\n\
u, update-ref <ref> = track a placeholder for the <ref> to be updated\n\
                      to this position in the new commits. The <ref> is\n\
                      updated at the end of the rebase\n\
\n\
These lines can be re-ordered; they are executed from top to bottom.\n";
    add_commented(&mut out, &cp, commands);
    add_commented(
        &mut out,
        &cp,
        "\nIf you remove a line here THAT COMMIT WILL BE LOST.\n",
    );
    if revs_onto.is_some() {
        add_commented(
            &mut out,
            &cp,
            "\nHowever, if you remove everything, the rebase will be aborted.\n\n",
        );
    } else {
        add_commented(
            &mut out,
            &cp,
            "\nYou are editing the todo file of an ongoing interactive rebase.\n\
To continue rebase after editing, run:\n    git rebase --continue\n\n",
        );
    }
    out
}

fn run_interactive_rebase_with_initial_todo(
    repo: &Repository,
    git_dir: &Path,
    initial_script: &str,
    config: &ConfigSet,
    autostash_oid: Option<&ObjectId>,
    help_revs_onto: Option<(&str, &str)>,
) -> Result<(Vec<String>, Vec<(ObjectId, RebaseTodoCmd)>)> {
    let rb_merge = rebase_merge_dir(git_dir);
    let _ = fs::remove_dir_all(&rb_merge);
    fs::create_dir_all(&rb_merge)?;
    fs::write(rb_merge.join("interactive"), "")?;
    // Git appends the instruction help (a comment block, preceded by a blank line) to the todo
    // before showing it. The editor file the user/test sees ends with `<todo>\n\n# Rebase ...`,
    // so after `grep -v '^#'` a trailing blank line remains (t3430 'generate correct todo list').
    let count = count_rebase_todo_actionable_lines(initial_script);
    let todo_with_help = append_rebase_todo_help(initial_script, count, help_revs_onto, config);
    let todo_path = rb_merge.join("git-rebase-todo");
    fs::write(&todo_path, todo_with_help.as_bytes())?;
    let orig_path = git_dir.join("ORIGINAL-TODO");
    fs::write(&orig_path, todo_with_help.as_bytes())?;
    warn_auto_comment_char_once(config);
    let editor = sequence_editor_cmd(config)?;
    let status = run_shell_editor(&editor, &todo_path)?;
    let edited = fs::read_to_string(&todo_path)?;
    let _ = fs::remove_dir_all(&rb_merge);
    if !status.success() {
        let _ = fs::remove_file(&orig_path);
        if worktree_matches_head(repo, git_dir)? {
            if let Some(oid) = autostash_oid {
                let _ = stash::pop_autostash_if_top(repo, oid);
            }
        }
        bail!("there was a problem with the editor");
    }
    let mut lines: Vec<String> = Vec::new();
    let mut pick_like: Vec<(ObjectId, RebaseTodoCmd)> = Vec::new();
    for line in edited.lines() {
        let t = line.trim();
        if t.is_empty() || t.starts_with('#') {
            continue;
        }
        lines.push(t.to_owned());
        if let Some(pair) = parse_todo_line_with_repo(Some(repo), t)? {
            pick_like.push(pair);
        }
    }
    Ok((lines, pick_like))
}

fn apply_pending_autostash(repo: &Repository, rb_dir: &Path) -> Result<()> {
    let Some(oid) = read_autostash_oid(rb_dir)? else {
        return Ok(());
    };
    flush_rebase_stdout();
    reset_index_to_head(repo, &repo.git_dir)?;
    let had_conflict = stash::apply_autostash_for_rebase(repo, &oid)?;
    if had_conflict {
        eprintln!("Applying autostash resulted in conflicts.");
        eprintln!("Your changes are safe in the stash.");
        eprintln!("You can run \"git stash pop\" or \"git stash drop\" at any time.");
    } else {
        eprintln!("Applied autostash.");
        let _ = stash::drop_stash_tip_if_matches(repo, &oid);
    }
    let _ = fs::remove_file(rb_dir.join("autostash"));
    Ok(())
}

fn apply_autostash_after_ff(repo: &Repository, autostash_oid: &ObjectId) -> Result<()> {
    flush_rebase_stdout();
    reset_index_to_head(repo, &repo.git_dir)?;
    let had_conflict = stash::apply_autostash_for_rebase(repo, autostash_oid)?;
    if had_conflict {
        eprintln!("Applying autostash resulted in conflicts.");
        eprintln!("Your changes are safe in the stash.");
        eprintln!("You can run \"git stash pop\" or \"git stash drop\" at any time.");
    } else {
        eprintln!("Applied autostash.");
        let _ = stash::drop_stash_tip_if_matches(repo, autostash_oid);
    }
    Ok(())
}

/// True when `name` is both a local branch and a tag pointing at different commits.
///
/// `git rebase --fork-point` fails in this case (t3431) while a plain revision parse would pick
/// the branch and warn.
fn fork_point_upstream_name_is_ambiguous(repo: &Repository, name: &str) -> Result<bool> {
    if name.contains('@')
        || name.starts_with("refs/")
        || name == "HEAD"
        || name.contains('*')
        || name.contains(':')
    {
        return Ok(false);
    }
    let head_ref = format!("refs/heads/{name}");
    let tag_ref = format!("refs/tags/{name}");
    let head_oid = grit_lib::refs::resolve_ref(&repo.git_dir, &head_ref).ok();
    let tag_oid = grit_lib::refs::resolve_ref(&repo.git_dir, &tag_ref).ok();
    Ok(matches!((head_oid, tag_oid), (Some(h), Some(t)) if h != t))
}

/// Open the interactive rebase todo in the sequence editor (`GIT_SEQUENCE_EDITOR` / `sequence.editor`).
fn do_edit_todo() -> Result<()> {
    let repo = Repository::discover(None).context("not a git repository")?;
    let git_dir = &repo.git_dir;

    if !is_rebase_in_progress(git_dir) {
        bail!("no rebase in progress");
    }

    let rb_dir = active_rebase_dir(git_dir)
        .ok_or_else(|| anyhow::anyhow!("internal: no rebase state directory"))?;
    if !rb_dir.join("interactive").exists() {
        bail!("interactive rebase is not in progress; cannot edit todo");
    }

    let config = ConfigSet::load(Some(git_dir), true).unwrap_or_else(|_| ConfigSet::new());
    let content = read_rebase_todo_file(&repo, &rb_dir)?;
    write_rebase_todo_file(&repo, &rb_dir, &content)?;

    let path = rebase_todo_file_path(&repo, &rb_dir);
    warn_auto_comment_char_once(&config);
    let editor = sequence_editor_cmd(&config)?;
    let status = run_shell_editor(&editor, &path)?;
    if !status.success() {
        bail!("there was a problem with the editor");
    }
    let edited = fs::read_to_string(&path)
        .with_context(|| format!("read rebase todo after edit {}", path.display()))?;
    write_rebase_todo_file(&repo, &rb_dir, &edited)?;
    // Re-run the static check (Git's `edit_todo_list` with `initial=false`): unknown commands, bad
    // SHAs, a leading fixup, and — comparing against `git-rebase-todo.backup` — dropped commits all
    // re-fail here so the user must fix the list before `--continue` (t3404 missingCommitsCheck +
    // "first command cannot be a fixup" tests).
    let original_for_check = fs::read_to_string(rb_dir.join("git-rebase-todo.backup"))
        .unwrap_or_else(|_| content.clone());
    validate_edited_interactive_todo(&repo, &rb_dir, &original_for_check, &edited, &config)?;
    Ok(())
}

fn do_rebase(
    args: Args,
    pre_rebase_hook_second: Option<String>,
    upstream_spec_before_branch_checkout: Option<String>,
    pre_rebase_upstream_label: Option<String>,
) -> Result<()> {
    let repo = Repository::discover(None).context("not a git repository")?;
    let git_dir = &repo.git_dir;

    if is_rebase_in_progress(git_dir) {
        bail!(
            "error: a rebase is already in progress\n\
             hint: use \"grit rebase --continue\" to continue\n\
             hint: or \"grit rebase --abort\" to abort"
        );
    }

    let config = ConfigSet::load(Some(git_dir), true).unwrap_or_else(|_| ConfigSet::new());
    let (rebase_merges_on, rebase_cousins) = effective_rebase_merges_settings(&args, &config);
    let config_autostash = config
        .get_bool("rebase.autostash")
        .and_then(|r| r.ok())
        .unwrap_or(false);
    let want_autostash = (args.autostash || config_autostash) && !args.no_autostash;
    let config_autosquash = config
        .get_bool("rebase.autosquash")
        .and_then(|r| r.ok())
        .unwrap_or(false);
    let want_autosquash =
        (args.autosquash || (config_autosquash && args.interactive)) && !args.no_autosquash;

    if want_autosquash {
        validate_rebase_instruction_format(&config)?;
    }

    validate_rebase_trailer_options(&args)?;
    validate_apply_merge_backend_combo(&args, &config, want_autosquash)?;
    let force_rewrite_requested = rebase_force_rewrite_requested(&args);

    let mut autostash_oid: Option<ObjectId> = None;
    let mut had_rebase_autostash = false;

    // Check for dirty worktree/index (optional autostash)
    {
        let work_tree = repo
            .work_tree
            .as_deref()
            .ok_or_else(|| anyhow::anyhow!("this operation must be run in a work tree"))?;
        let idx = repo.load_index().context("failed to read index")?;
        let head_tree = resolve_head(git_dir)?.oid().and_then(|oid| {
            let obj = repo.odb.read(oid).ok()?;
            parse_commit(&obj.data).ok().map(|c| c.tree)
        });
        let ignore_submodules = true;
        let staged = diff_index_to_tree(&repo.odb, &idx, head_tree.as_ref(), ignore_submodules)?;
        let mut unstaged = diff_index_to_worktree(&repo.odb, &idx, work_tree, false, false)?;
        unstaged.retain(|e| e.old_mode != "160000" && e.new_mode != "160000");
        let dirty = !staged.is_empty() || !unstaged.is_empty();
        if dirty {
            if !want_autostash {
                if !staged.is_empty() {
                    bail!(
                        "cannot rebase: your index contains uncommitted changes.\n\
                   Please commit or stash them."
                    );
                }
                bail!(
                    "error: cannot rebase: You have unstaged changes.\n\
                   Please commit or stash them."
                );
            }
            autostash_oid = stash::autostash_for_rebase(&repo)?;
            had_rebase_autostash = autostash_oid.is_some();
            if autostash_oid.is_none() {
                if !staged.is_empty() {
                    bail!(
                        "cannot rebase: your index contains uncommitted changes.\n\
                   Please commit or stash them."
                    );
                }
                bail!(
                    "error: cannot rebase: You have unstaged changes.\n\
                   Please commit or stash them."
                );
            }
        }
    }

    // Resolve upstream / onto / HEAD
    let head_state = resolve_head(git_dir)?;
    let head_oid_early = head_state
        .oid()
        .ok_or_else(|| anyhow::anyhow!("cannot rebase: HEAD is unborn"))?
        .to_owned();

    let upstream_spec_str = upstream_spec_before_branch_checkout
        .clone()
        .or_else(|| args.upstream.clone())
        .unwrap_or_else(|| "HEAD".to_owned());

    // Git applies fork-point for the default upstream (`@{upstream}`) when `rebase.forkPoint` is
    // true, but not when the user names the upstream explicitly (`rebase main`), unless
    // `--fork-point` is passed (t3431).
    let fork_point_effective = if args.root {
        false
    } else if args.fork_point {
        true
    } else if args.no_fork_point {
        false
    } else if args.keep_base > 0 {
        false
    } else {
        let cfg_default = config
            .get_bool("rebase.forkPoint")
            .or_else(|| config.get_bool("rebase.forkpoint"))
            .and_then(|r| r.ok())
            .unwrap_or(true);
        let upstream_arg = upstream_spec_before_branch_checkout
            .as_deref()
            .or_else(|| args.upstream.as_deref())
            .unwrap_or("HEAD");
        let implicit_upstream = upstream_suffix_info(upstream_arg).is_some();
        cfg_default && implicit_upstream
    };

    // Fork-point scans the upstream ref's reflog. The spec string is the same as the upstream
    // argument (`side@{upstream}` when implicit, or `main` / `refs/heads/main` when explicit).
    let fork_point_reflog_spec: Option<String> = if fork_point_effective {
        Some(upstream_spec_str.clone())
    } else {
        None
    };

    if !args.root {
        let us_for_ambiguous = upstream_spec_before_branch_checkout
            .as_deref()
            .or_else(|| args.upstream.as_deref())
            .unwrap_or("HEAD");
        if args.fork_point && fork_point_upstream_name_is_ambiguous(&repo, us_for_ambiguous)? {
            bail!(
                "fatal: ambiguous argument '{us_for_ambiguous}': unknown revision or path not in the working tree.\n\
Use '--' to separate paths from revisions, like this:\n\
'git <command> [<revision>...] -- [<file>...]'"
            );
        }
    }

    let (upstream_spec, upstream_oid, upstream_tip_oid, onto_oid, onto_name_for_state) = if args
        .root
    {
        let (onto, onto_label) = if let Some(ref onto_spec) = args.onto {
            let oid = resolve_revision_as_commit_without_index_dwim(&repo, onto_spec)
                .with_context(|| format!("bad revision '{onto_spec}'"))?;
            (oid, onto_spec.clone())
        } else {
            let oid = create_squash_onto_root_commit(
                &repo,
                git_dir,
                args.reset_author_date,
                args.committer_date_is_author_date,
            )?;
            (oid, oid.to_hex())
        };
        ("--root".to_owned(), onto, onto, onto, onto_label)
    } else {
        let upstream_spec = upstream_spec_str.clone();
        let up_oid = resolve_revision_as_commit_without_index_dwim(&repo, &upstream_spec)
            .with_context(|| format!("bad revision '{upstream_spec}'"))?;
        let upstream_tip_oid = up_oid;

        let mut effective_upstream_for_range = up_oid;
        if let Some(ref fp_spec) = fork_point_reflog_spec {
            match fork_point(&repo, fp_spec, up_oid, head_oid_early) {
                Ok(fp_oid) => effective_upstream_for_range = fp_oid,
                Err(e) if e.to_string().contains("no merge base") => {}
                Err(e) => {
                    return Err(e)
                        .with_context(|| format!("fork-point resolution failed for '{fp_spec}'"));
                }
            }
        }

        let (onto, onto_label) = if let Some(ref onto_spec) = args.onto {
            if split_triple_dot_range(onto_spec).is_some() {
                let oid =
                    merge_base_from_triple_dot_onto(&repo, onto_spec, false, onto_spec.as_str())?;
                (oid, onto_spec.clone())
            } else {
                let oid = resolve_revision_as_commit_without_index_dwim(&repo, onto_spec)
                    .with_context(|| format!("bad revision '{onto_spec}'"))?;
                (oid, onto_spec.clone())
            }
        } else if args.keep_base > 0 {
            let branch_name = head_state
                .branch_name()
                .map(|s| s.to_string())
                .unwrap_or_else(|| "HEAD".to_string());
            let onto_spec = format!("{upstream_spec}...{branch_name}");
            let oid =
                merge_base_from_triple_dot_onto(&repo, &onto_spec, true, upstream_spec.as_str())?;
            (oid, onto_spec)
        } else {
            (up_oid, upstream_spec.clone())
        };
        (
            upstream_spec,
            effective_upstream_for_range,
            upstream_tip_oid,
            onto,
            onto_label,
        )
    };
    let head = head_state;
    let head_oid = head_oid_early;

    let root_rebase_no_onto = args.root && args.onto.is_none();

    let want_stat =
        args.stat || (config.get("rebase.stat").as_deref() == Some("true") && !args.no_stat);

    let branch_base = if root_rebase_no_onto {
        None
    } else {
        let branch_base_merge = merge_bases_first_vs_rest(&repo, onto_oid, &[head_oid])?;
        if branch_base_merge.len() == 1 {
            Some(branch_base_merge[0])
        } else {
            None
        }
    };

    let whitespace_forces_replay = args
        .whitespace
        .as_deref()
        .is_some_and(|w| w.eq_ignore_ascii_case("fix") || w.eq_ignore_ascii_case("strip"));
    // Git sets `REBASE_FORCE` for these options so a preemptive fast-forward cannot skip replay
    // (tree-identical commits still need committer/author timestamp rewriting).
    let date_options_force_replay = args.committer_date_is_author_date || args.reset_author_date;
    let allow_preemptive_ff = !args.interactive
        && !rebase_merges_on
        && args.exec.is_empty()
        && !whitespace_forces_replay
        && !args.autosquash
        && !date_options_force_replay;

    // `rebase --fork-point --keep-base` can use different commits for the replay range and the
    // new base. If the fork point is not the same as `onto`, preemptive fast-forward detection
    // would incorrectly treat the branch as up-to-date and skip required replay (t3431). When
    // `onto` is the fork point, the preemptive noop path is valid (t3432).
    let skip_preemptive_ff_for_keep_base_fork_point =
        args.keep_base > 0 && upstream_tip_oid != upstream_oid && onto_oid != upstream_oid;

    // `upstream_oid` carries the fork-point commit when fork-point selection was active (it equals
    // `upstream_tip_oid` otherwise). Git passes this as `restrict_revision` to `can_fast_forward`
    // so a rewound upstream cannot be mistaken for an up-to-date branch.
    let restrict_revision = if fork_point_effective {
        Some(upstream_oid)
    } else {
        None
    };
    if allow_preemptive_ff
        && !root_rebase_no_onto
        && !skip_preemptive_ff_for_keep_base_fork_point
        && rebase_can_preemptive_ff(
            &repo,
            onto_oid,
            upstream_tip_oid,
            restrict_revision,
            head_oid,
        )?
    {
        if !force_rewrite_requested {
            print_branch_up_to_date(&head);
            if let Some(ref oid) = autostash_oid {
                apply_autostash_after_ff(&repo, oid)?;
            }
            return Ok(());
        }
        if let Some(name) = head.branch_name() {
            println!("Current branch {name} is up to date, rebase forced.");
        } else {
            println!("HEAD is up to date, rebase forced.");
        }
    }

    if want_stat {
        if args.verbose {
            match branch_base {
                Some(bb) => println!(
                    "Changes from {} to {}:",
                    &bb.to_hex()[..7],
                    &onto_oid.to_hex()[..7]
                ),
                None => println!("Changes to {}:", onto_oid.to_hex()),
            }
        }
        if !root_rebase_no_onto {
            print_rebase_diffstat(&repo, branch_base, onto_oid)?;
        }
    }

    let reapply_cherry_picks = if args.reapply_cherry_picks {
        true
    } else if args.no_reapply_cherry_picks {
        false
    } else {
        args.keep_base > 0
    };
    // Git's `sequencer_make_script` always drops clean cherry-picks of upstream commits as a
    // preliminary step unless `--reapply-cherry-picks` (or `--keep-base`) is given — this holds for
    // every backend, including interactive (`-i`) and `--rebase-merges`. Begin-empty commits are
    // never dropped here (see `collect_rebase_todo_commits`); only true patch-equivalents are.
    let filter_cherry_equivalents = !reapply_cherry_picks;
    // `--keep-base` commit selection: when reapplying cherry-picks, Git uses `onto` as upstream for
    // commit collection (`options.upstream = options.onto`). Otherwise default fork-point behavior
    // still uses the upstream *tip* for the replay list (t3431.4); explicit `--fork-point
    // --keep-base` uses the fork-point commit (t3431.12).
    let commits_upstream = if args.keep_base > 0 {
        if fork_point_effective && args.fork_point {
            upstream_oid
        } else if reapply_cherry_picks {
            onto_oid
        } else {
            upstream_tip_oid
        }
    } else {
        upstream_oid
    };
    let mut commits = if args.root {
        // Upstream `sequencer_make_script` sets `revs.max_parents = 1` unless `--rebase-merges`,
        // so a plain `rebase -i --root` (or any non-merge backend that would build a pick list)
        // excludes merge commits from the todo rather than emitting `pick <merge>` lines, which
        // `pick` rejects. The sequential/apply root backend keeps merges and flattens them via the
        // first-parent walk (`message_for_root_replayed_commit`), so only exclude merges for the
        // interactive, non-rebase-merges case (t3412 `rebase -i --root with conflict`).
        let exclude_merges = args.interactive && !rebase_merges_on;
        collect_commits_for_root_rebase(
            &repo,
            head_oid,
            onto_oid,
            args.onto.is_some(),
            exclude_merges,
        )?
    } else {
        collect_rebase_todo_commits(&repo, head_oid, commits_upstream, filter_cherry_equivalents)?
    };
    if filter_cherry_equivalents && upstream_tip_oid != upstream_oid {
        commits = filter_redundant_patch_commits(&repo, upstream_tip_oid, &commits)?;
    }
    if !args.root && !rebase_merges_on {
        commits = filter_merge_commits(&repo, &commits)?;
    }

    // Commits that start empty are kept by default. `--no-keep-empty` drops them as a preliminary
    // step for every backend (Git's `sequencer_make_script`: `if (is_empty && !keep_empty)
    // continue;`), including interactive (`-i --no-keep-empty`, t3421). `--reset-author-date` /
    // `--ignore-date` must still replay them even with `--no-keep-empty` so author timestamps are
    // rewritten (t3436). Merge-replay scripts handle empty filtering in the script generator.
    if args.no_keep_empty && !rebase_merges_on && !args.reset_author_date {
        commits.retain(|oid| !is_commit_tree_unchanged(&repo, oid).unwrap_or(false));
    }

    if args.interactive
        && args.onto.is_none()
        && !args.no_ff
        && !root_rebase_no_onto
        && commits.len() == 1
        && !is_commit_tree_unchanged(&repo, &commits[0]).unwrap_or(false)
        && commit_tree_oid_for_rebase(&repo, commits[0])?
            == commit_tree_oid_for_rebase(&repo, onto_oid)?
    {
        fast_forward_rebase(
            &repo,
            &head,
            head_oid,
            onto_oid,
            onto_name_for_state.as_str(),
            Some(head_oid),
            head_oid,
        )?;
        if let Some(ref oid) = autostash_oid {
            apply_autostash_after_ff(&repo, oid)?;
        }
        return Ok(());
    }

    let hook_upstream = pre_rebase_upstream_label
        .as_deref()
        .unwrap_or_else(|| upstream_spec.as_str());
    let hook_arg1: &str = if args.root { "--root" } else { hook_upstream };
    let hook_arg2: Option<&str> = pre_rebase_hook_second.as_deref();
    let hook_args: Vec<&str> = match hook_arg2 {
        Some(s) => vec![hook_arg1, s],
        None => vec![hook_arg1],
    };
    if !args.no_verify {
        if let HookResult::Failed(_) = run_hook(&repo, "pre-rebase", &hook_args, None) {
            if let Some(ref oid) = autostash_oid {
                let _ = stash::pop_autostash_if_top(&repo, oid);
            }
            bail!("The pre-rebase hook refused to rebase.");
        }
    }

    let mut generated_merge_script: Option<String> = None;
    // For a plain `rebase -i`, captures (original todo with help, raw edited todo) so the static
    // todo check (`todo_list_check`) can run after the rebase state dir is established.
    let mut interactive_validation: Option<(String, String)> = None;
    let (rebase_todo_lines, rebase_interactive) = if rebase_merges_on {
        // Do not use `collect_rebase_todo_commits` emptiness here: it walks first-parent chains only
        // and misses merge topology, which would incorrectly report "up to date" for `main` over `A`.
        // For interactive rebases we must NOT short-circuit: git still opens the sequence editor so a
        // script can add `merge`/`exec`/`label` lines even when there is nothing to pick (t3430
        // `rebase -ir HEAD` with a `merge -C H G` script).
        if head_oid == upstream_oid && !args.interactive {
            print_branch_up_to_date(&head);
            if let Some(ref oid) = autostash_oid {
                apply_autostash_after_ff(&repo, oid)?;
            }
            return Ok(());
        }
        let script = generate_rebase_merge_script(
            &repo,
            head_oid,
            upstream_oid,
            onto_oid,
            filter_cherry_equivalents,
            rebase_cousins,
            // `root_with_onto`: only a root rebase that has an explicit `--onto` resets to `onto`. A
            // bare `--root -r` resets each branch base to `[new root]` (a `[new root]` reset makes the
            // following pick a root commit, which lets an unchanged `--root -r` fast-forward as a whole
            // -graph no-op). t3430 'root commits' second `git rebase -i --root -r` must be a no-op.
            args.root && args.onto.is_some(),
            !args.no_keep_empty || args.keep_empty,
            &config,
        )?;
        // Git's `complete_action` runs `todo_list_rearrange_squash` over the whole merge script
        // (picks and merges) before the editor opens, so `--autosquash` works with `--rebase-merges`
        // (t3430 'with --autosquash and --exec').
        let script = if want_autosquash {
            rearrange_autosquash_merge_script(&repo, &script)?
        } else {
            script
        };
        // Then weave `--exec` commands into the merge todo after each pick/merge (Git's
        // `todo_list_add_exec_commands`, run after the rearrange). The merge path is "interactive",
        // so the global `rebase-merge/exec` file is NOT written; the exec must live in the todo
        // (t3430 'truncate label names' / 'with --autosquash and --exec').
        let script = if !args.exec.is_empty() {
            insert_exec_into_merge_script(&script, &args.exec)
        } else {
            script
        };
        generated_merge_script = Some(script.clone());
        if args.interactive {
            let pre_nonempty = count_rebase_todo_actionable_lines(&script);
            let short_revs = if args.root {
                "(root)".to_owned()
            } else {
                upstream_spec.clone()
            };
            let short_onto =
                abbreviate_object_id(&repo, onto_oid, 7).unwrap_or_else(|_| onto_oid.to_hex());
            let (edited, _) = run_interactive_rebase_with_initial_todo(
                &repo,
                git_dir,
                &script,
                &config,
                autostash_oid.as_ref(),
                Some((short_revs.as_str(), short_onto.as_str())),
            )?;
            if edited.is_empty() {
                if pre_nonempty > 0 {
                    if worktree_matches_head(&repo, git_dir)? {
                        if let Some(ref oid) = autostash_oid {
                            let _ = stash::pop_autostash_if_top(&repo, oid);
                        }
                    }
                    bail!("there was a problem with the editor");
                }
                print_branch_up_to_date(&head);
                if let Some(ref oid) = autostash_oid {
                    apply_autostash_after_ff(&repo, oid)?;
                }
                return Ok(());
            }
            (edited, true)
        } else {
            (
                script.lines().map(|s| s.to_owned()).collect::<Vec<_>>(),
                true,
            )
        }
    } else if args.interactive {
        // Even when the computed pick list is empty (`git rebase -i A A`), Git still runs the
        // sequence editor so the user can add `merge`/`exec` lines (t3436).
        let pre_editor_len = commits.len();
        // Git's todo help header reads `Rebase <short-upstream>..<short-orig-head> onto <short-onto>`.
        let short_onto =
            abbreviate_object_id(&repo, onto_oid, 7).unwrap_or_else(|_| onto_oid.to_hex());
        let help_revs = if args.root {
            "(root)".to_owned()
        } else {
            let short_upstream = abbreviate_object_id(&repo, upstream_tip_oid, 7)
                .unwrap_or_else(|_| upstream_tip_oid.to_hex());
            let short_head =
                abbreviate_object_id(&repo, head_oid, 7).unwrap_or_else(|_| head_oid.to_hex());
            format!("{short_upstream}..{short_head}")
        };
        let (edited_lines, _, todo_changed, original_todo_with_help, raw_edited_todo) =
            run_interactive_rebase(
                &repo,
                git_dir,
                &commits,
                &config,
                autostash_oid.as_ref(),
                want_autosquash,
                rebase_update_refs_enabled(&args, &config),
                &args.exec,
                Some((help_revs.as_str(), short_onto.as_str())),
            )?;
        interactive_validation = Some((original_todo_with_help, raw_edited_todo));
        if !todo_changed
            && !want_autosquash
            && !force_rewrite_requested
            && args.onto.is_none()
            && args.exec.is_empty()
            && !rebase_merges_on
            && commits.is_empty()
            && is_ancestor(&repo, upstream_oid, head_oid)?
        {
            print_branch_up_to_date(&head);
            if let Some(ref oid) = autostash_oid {
                apply_autostash_after_ff(&repo, oid)?;
            }
            return Ok(());
        }
        if edited_lines.is_empty() {
            if pre_editor_len > 0 {
                if worktree_matches_head(&repo, git_dir)? {
                    if let Some(ref oid) = autostash_oid {
                        let _ = stash::pop_autostash_if_top(&repo, oid);
                    }
                }
                bail!("nothing to do");
            }
            // The pick list is empty: either the branch is behind the upstream (a linear ancestor
            // of `onto`) or every commit was dropped as a clean cherry-pick of the upstream. In
            // both cases Git resets the branch to `onto` and reports "Successfully rebased"
            // (t3421 "rebase -i fast-forwards from ancestor of upstream"; t3404 "do noop when there
            // is nothing to cherry-pick"). Only when the branch already points at `onto` is this a
            // true "up to date" no-op.
            if !onto_oid.is_zero() && head_oid != onto_oid {
                rebase_reset_to_onto_noop(&repo, &head, head_oid, onto_oid)?;
                if let Some(ref oid) = autostash_oid {
                    apply_autostash_after_ff(&repo, oid)?;
                }
                return Ok(());
            }
            print_branch_up_to_date(&head);
            if let Some(ref oid) = autostash_oid {
                apply_autostash_after_ff(&repo, oid)?;
            }
            return Ok(());
        }
        (edited_lines, true)
    } else if want_autosquash {
        let entries = rearrange_autosquash(&repo, commits)?;
        (rebase_state_todo_lines(&repo, &config, &entries)?, false)
    } else if rebase_update_refs_enabled(&args, &config)
        && matches!(choose_rebase_backend(&args), RebaseBackend::Merge)
    {
        // A plain merge-backend `rebase --update-refs` still schedules branch updates even though
        // no sequence editor opens: Git's `todo_list_add_update_ref_commands` interleaves
        // `update-ref` steps so the scheduled branches are recorded in `rebase-merge/update-refs`
        // and become "used by worktree" while the rebase is paused (t2407 #6).
        (
            rebase_update_refs_todo_lines(&repo, git_dir, &config, &commits)?,
            false,
        )
    } else {
        let entries: Vec<(ObjectId, RebaseTodoCmd)> = commits
            .into_iter()
            .map(|o| (o, RebaseTodoCmd::Pick))
            .collect();
        (rebase_state_todo_lines(&repo, &config, &entries)?, false)
    };

    if !force_rewrite_requested && rebase_todo_lines.is_empty() {
        if !onto_oid.is_zero() && head_oid == onto_oid {
            print_branch_up_to_date(&head);
            if let Some(ref oid) = autostash_oid {
                apply_autostash_after_ff(&repo, oid)?;
            }
            return Ok(());
        }
        if !onto_oid.is_zero() && can_fast_forward(&repo, head_oid, onto_oid)? {
            let ff_base = merge_bases_first_vs_rest(&repo, onto_oid, &[head_oid])?
                .into_iter()
                .next();
            fast_forward_rebase(
                &repo,
                &head,
                head_oid,
                onto_oid,
                onto_name_for_state.as_str(),
                ff_base,
                head_oid,
            )?;
            if let Some(ref oid) = autostash_oid {
                apply_autostash_after_ff(&repo, oid)?;
            }
            return Ok(());
        }
    }

    if rebase_todo_lines.is_empty() {
        if let HeadState::Branch { refname, .. } = &head {
            let ident = reflog_identity(&repo);
            let msg = format!("rebase (no-ff): checkout {}", onto_oid.to_hex());
            let _ = append_reflog(git_dir, refname, &head_oid, &head_oid, &ident, &msg, false);
            let _ = append_reflog(git_dir, "HEAD", &head_oid, &head_oid, &ident, &msg, false);
        }
        // Git still records ORIG_HEAD when the rebase is a no-op because every commit was skipped
        // as a cherry-pick equivalent (t3418-rebase-continue ORIG_HEAD tests).
        fs::write(
            git_dir.join("ORIG_HEAD"),
            format!("{}\n", head_oid.to_hex()),
        )?;
        if let Some(ref oid) = autostash_oid {
            apply_autostash_after_ff(&repo, oid)?;
        }
        return Ok(());
    }

    let backend = choose_rebase_backend(&args);
    if matches!(backend, RebaseBackend::Apply) {
        prepare_rebased_patches_writable(git_dir)?;
    }
    // Remove any stale rebase state from either backend so `active_rebase_dir` cannot pick the
    // wrong directory (merge is checked before apply).
    cleanup_rebase_state(git_dir);
    let rb_dir = rebase_state_dir_for_backend(git_dir, backend);
    fs::create_dir_all(&rb_dir)?;

    let head_name = match &head {
        HeadState::Branch { refname, .. } => refname.clone(),
        _ => "detached HEAD".to_string(),
    };
    fs::write(rb_dir.join("head-name"), &head_name)?;
    fs::write(rb_dir.join("orig-head"), head_oid.to_hex())?;
    fs::write(
        git_dir.join("ORIG_HEAD"),
        format!("{}\n", head_oid.to_hex()),
    )?;
    fs::write(rb_dir.join("onto"), onto_oid.to_hex())?;
    if !args.root {
        fs::write(rb_dir.join("upstream"), upstream_oid.to_hex())?;
        fs::write(rb_dir.join("upstream-tip"), upstream_tip_oid.to_hex())?;
    }
    fs::write(rb_dir.join("onto-name"), format!("{onto_name_for_state}\n"))?;
    fs::write(
        rb_dir.join("reflog-action"),
        format!("{}\n", rebase_reflog_action()),
    )?;
    fs::write(
        rb_dir.join("backend"),
        match backend {
            RebaseBackend::Merge => "merge\n",
            RebaseBackend::Apply => "apply\n",
        },
    )?;
    fs::write(rb_dir.join("rebasing"), "")?;
    if args.root {
        fs::write(rb_dir.join("root"), "")?;
    }
    if force_rewrite_requested {
        fs::write(rb_dir.join("force-rewrite"), "")?;
    }
    if args.keep_empty || want_autosquash {
        fs::write(rb_dir.join("keep-empty"), "")?;
    }
    if args.no_keep_empty {
        fs::write(rb_dir.join("no-keep-empty"), "")?;
    }
    // Persist the become-empty policy so each pick (incl. those after --continue/--skip) honours
    // the resolved `--empty` mode (git `drop_redundant_commits` / `keep_redundant_commits`).
    match resolve_become_empty_mode(&args) {
        BecomeEmptyMode::Drop => {
            fs::write(rb_dir.join("drop_redundant_commits"), "")?;
        }
        BecomeEmptyMode::Keep => {
            fs::write(rb_dir.join("keep_redundant_commits"), "")?;
        }
        BecomeEmptyMode::Stop => {}
    }
    // Persist the GPG/SSH signing decision so each pick (including those run from
    // a clean child process or after `--continue`) signs the replayed commit
    // when `-S`/`--gpg-sign` was given or `commit.gpgsign` is set.
    if let Some(opt) = resolve_rebase_gpg_sign_opt(&args, &config) {
        fs::write(rb_dir.join("gpg-sign-opt"), opt)?;
    }
    if args.signoff && !args.no_signoff {
        fs::write(rb_dir.join("signoff"), "")?;
    }
    write_rebase_trailer_args(&rb_dir, &args.trailer)?;

    let todo = rebase_todo_lines;
    let todo_body = todo.join("\n") + "\n";
    let total_cmds = count_rebase_todo_actionable_lines(&todo_body);
    if rebase_interactive {
        fs::write(rb_dir.join("interactive"), "")?;
    }
    // Initialize the `--update-refs` state from the finalized todo's `update-ref` lines (Git's
    // `todo_list_add_update_ref_commands` writes the before/after-null records here).
    if rebase_update_refs_enabled(&args, &config) && matches!(backend, RebaseBackend::Merge) {
        write_rebase_update_refs(git_dir, &todo_body)?;
    }
    write_rebase_todo_file(&repo, &rb_dir, &todo_body)?;
    if rebase_merges_on && !args.interactive {
        if let Some(ref s) = generated_merge_script {
            fs::write(git_dir.join("ORIGINAL-TODO"), s.as_bytes())?;
        }
    }
    fs::write(rb_dir.join("end"), total_cmds.to_string())?;
    fs::write(rb_dir.join("total-cmds"), total_cmds.to_string())?;
    fs::write(rb_dir.join("completed-cmds"), "0")?;
    if args.verbose {
        fs::write(rb_dir.join("verbose"), "")?;
    }
    if args.quiet {
        fs::write(rb_dir.join("quiet"), "")?;
    }
    fs::write(rb_dir.join("msgnum"), "1")?;
    fs::write(rb_dir.join("last"), total_cmds.to_string())?;
    fs::write(rb_dir.join("next"), "1")?;

    if let Some(ref ws) = args.whitespace {
        if ws.eq_ignore_ascii_case("fix") || ws.eq_ignore_ascii_case("strip") {
            fs::write(rb_dir.join("whitespace-action"), format!("{ws}\n"))?;
        }
    }

    if args.ignore_whitespace {
        fs::write(rb_dir.join("ignore-whitespace"), "")?;
    }
    if args.rerere_autoupdate {
        fs::write(rb_dir.join("rerere-autoupdate"), "")?;
    }
    if args.no_rerere_autoupdate {
        fs::write(rb_dir.join("no-rerere-autoupdate"), "")?;
    }
    if args.reschedule_failed_exec {
        fs::write(rb_dir.join("reschedule-failed-exec"), "")?;
    } else if !args.no_reschedule_failed_exec
        && !args.exec.is_empty()
        && config
            .get_bool("rebase.rescheduleFailedExec")
            .and_then(|r| r.ok())
            .unwrap_or(false)
    {
        fs::write(rb_dir.join("reschedule-failed-exec"), "")?;
    }
    if args.no_reschedule_failed_exec {
        fs::write(rb_dir.join("no-reschedule-failed-exec"), "")?;
    }
    if args.committer_date_is_author_date {
        fs::write(rb_dir.join("cdate_is_adate"), "")?;
    }
    if args.reset_author_date {
        fs::write(rb_dir.join("ignore_date"), "")?;
    }

    if !args.exec.is_empty() && !rebase_interactive {
        // For a *non-interactive* `rebase --exec`, persist every command (one per line, in order);
        // the replay runs each after each pick (t3404 "rebase -ix with several instances of
        // --exec", "rebase --exec works without -i"). In interactive mode the `--exec` commands are
        // already woven into the visible todo as `exec` lines by `insert_exec_commands`, so the
        // replay executes them from the todo; writing the global file too would double-run them.
        fs::write(rb_dir.join("exec"), args.exec.join("\n"))?;
    }
    if let Some(ref strategy) = args.strategy {
        fs::write(rb_dir.join("strategy"), format!("{strategy}\n"))?;
    }
    if !args.strategy_option.is_empty() {
        fs::write(
            rb_dir.join("strategy-opts"),
            args.strategy_option
                .iter()
                .map(|opt| hex_encode_bytes(opt.as_bytes()))
                .collect::<Vec<_>>()
                .join("\n"),
        )?;
    }

    if let Some(ref oid) = autostash_oid {
        fs::write(rb_dir.join("autostash"), format!("{}\n", oid.to_hex()))?;
    }

    // Branches that pointed at the onto tip before rebase must keep that label after finish when
    // the rebased branch ends at the same OID (t3420 `never change active branch` after submodule
    // flows that pack refs and can leave unrelated labels tracking the wrong tip).
    if head_name != "detached HEAD" {
        let mut refs_at_onto: Vec<String> = Vec::new();
        if let Ok(all) = list_refs(git_dir, "refs/heads/") {
            for (refname, oid) in all {
                if oid == onto_oid && refname != head_name {
                    refs_at_onto.push(refname);
                }
            }
        }
        if !refs_at_onto.is_empty() {
            fs::write(
                rb_dir.join("preserve-onto-refs"),
                refs_at_onto.join("\n") + "\n",
            )?;
        }
    }

    let ident = reflog_identity(&repo);
    let ra = rebase_reflog_action();
    let start_msg = format!("{ra} (start): checkout {onto_name_for_state}");

    let checkout_onto = || -> Result<()> {
        let empty_tree: ObjectId = "4b825dc642cb6eb9a060e54bf8d69288fbee4904"
            .parse()
            .map_err(|_| anyhow::anyhow!("internal: empty tree object id"))?;
        let tree_oid = if args.root && args.onto.is_none() {
            empty_tree
        } else {
            let onto_obj = repo.odb.read(&onto_oid)?;
            let onto_commit = parse_commit(&onto_obj.data)?;
            onto_commit.tree
        };
        let entries = tree_to_index_entries(&repo, &tree_oid, "")?;
        let mut idx = Index::new();
        idx.entries = entries;
        idx.sort();
        let old_index = load_index(&repo)?;
        if let Some(wt) = &repo.work_tree {
            check_dirty_worktree(&repo, &old_index, &idx, wt, &head)?;
            // Fail before touching the work tree: `checkout_index_to_worktree` may apply partial
            // updates before refusal deep inside (t3426 `test_superproject_content` after failed rebase).
            refuse_populated_submodule_tree_replacement(&old_index, &idx, wt)?;
            preflight_cherry_pick_cwd_obstruction(&repo, wt, &idx, &BTreeMap::new(), None)?;
        }

        // Update the work tree before moving HEAD or writing the new index so a failure (e.g.
        // submodule replacement refusal) does not strand HEAD on `onto` with a mismatched tree
        // (breaks `reset_work_tree_to` / `rm -rf` in t3426).
        if let Some(wt) = &repo.work_tree {
            checkout_merged_index(&repo, wt, &old_index, &idx, true)?;
            refresh_index_stat_cache_from_worktree(&repo, &mut idx)?;
        }

        let head_after_checkout = if args.root && args.onto.is_none() {
            ObjectId::zero()
        } else {
            onto_oid
        };
        fs::write(
            git_dir.join("HEAD"),
            format!("{}\n", head_after_checkout.to_hex()),
        )?;
        fs::write(
            git_dir.join("ORIG_HEAD"),
            format!("{}\n", head_oid.to_hex()),
        )?;
        repo.write_index(&mut idx)?;
        run_post_checkout_hook(&repo, &head_oid, &head_after_checkout)?;
        Ok(())
    };

    if let Err(e) = checkout_onto() {
        if let Some(ref oid) = autostash_oid {
            let _ = stash::pop_autostash_if_top(&repo, oid);
        }
        let _ = fs::remove_dir_all(&rb_dir);
        return Err(e);
    }

    let checkout_target_oid = if args.root && args.onto.is_none() {
        ObjectId::zero()
    } else {
        onto_oid
    };
    // Record `(start)` only after HEAD/index/worktree successfully match `onto` (t3426: failed
    // rewind must not append a misleading HEAD move to the reflog).
    let _ = append_reflog(
        git_dir,
        "HEAD",
        &head_oid,
        &checkout_target_oid,
        &ident,
        &start_msg,
        false,
    );

    // Static check of the edited todo, mirroring Git: the rebase is already set up and HEAD has been
    // rewound onto the new base, so on failure the in-progress rebase (incl. `git-rebase-todo.backup`)
    // is left in place for `git rebase --edit-todo` + `--continue` recovery (t3404 static-check +
    // missingCommitsCheck=error tests).
    if let Some((ref original_todo_with_help, ref raw_edited_todo)) = interactive_validation {
        validate_edited_interactive_todo(
            &repo,
            &rb_dir,
            original_todo_with_help,
            raw_edited_todo,
            &config,
        )?;
    }

    replay_remaining(
        &repo,
        &rb_dir,
        autostash_oid,
        backend,
        had_rebase_autostash,
        force_rewrite_requested,
    )?;

    Ok(())
}

fn fast_forward_rebase(
    repo: &Repository,
    head: &HeadState,
    head_oid: ObjectId,
    onto_oid: ObjectId,
    onto_name: &str,
    branch_base: Option<ObjectId>,
    orig_head: ObjectId,
) -> Result<()> {
    let git_dir = &repo.git_dir;
    if branch_base != Some(orig_head) {
        bail!("internal: fast-forward branch base mismatch");
    }

    eprintln!("First, rewinding head to replay your work on top of it...");

    let ident = reflog_identity(repo);
    let ra = rebase_reflog_action();
    let start_msg = format!("{ra} (start): checkout {onto_name}");

    let onto_obj = repo.odb.read(&onto_oid)?;
    let onto_commit = parse_commit(&onto_obj.data)?;
    let entries = tree_to_index_entries(repo, &onto_commit.tree, "")?;
    let mut idx = Index::new();
    idx.entries = entries;
    idx.sort();
    let old_index = load_index(repo)?;
    if let Some(wt) = &repo.work_tree {
        preflight_cherry_pick_cwd_obstruction(repo, wt, &idx, &BTreeMap::new(), None)?;
        refuse_populated_submodule_tree_replacement(&old_index, &idx, wt)?;
        checkout_merged_index(repo, wt, &old_index, &idx, true)?;
        refresh_index_stat_cache_from_worktree(repo, &mut idx)?;
    }

    fs::write(git_dir.join("HEAD"), format!("{}\n", onto_oid.to_hex()))?;
    fs::write(
        git_dir.join("ORIG_HEAD"),
        format!("{}\n", head_oid.to_hex()),
    )?;
    repo.write_index(&mut idx)?;

    let _ = append_reflog(
        git_dir, "HEAD", &head_oid, &onto_oid, &ident, &start_msg, false,
    );

    run_post_checkout_hook(repo, &head_oid, &onto_oid)?;

    let branch_disp = head
        .branch_name()
        .map(|s| s.to_string())
        .unwrap_or_else(|| "HEAD".to_owned());

    if let HeadState::Branch { refname, .. } = head {
        let finish_branch = format!("{ra} (finish): {refname} onto {}", onto_oid.to_hex());
        let finish_head = format!("{ra} (finish): returning to {refname}");
        let _ = append_reflog(
            git_dir,
            refname,
            &head_oid,
            &onto_oid,
            &ident,
            &finish_branch,
            false,
        );
        let _ = append_reflog(
            git_dir,
            "HEAD",
            &onto_oid,
            &onto_oid,
            &ident,
            &finish_head,
            false,
        );

        let ref_path = git_dir.join(refname);
        if let Some(parent) = ref_path.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::write(&ref_path, format!("{}\n", onto_oid.to_hex()))?;
        fs::write(git_dir.join("HEAD"), format!("ref: {refname}\n"))?;
    }

    println!("Fast-forwarded {branch_disp} to {onto_name}.");
    Ok(())
}

/// Reset HEAD (and the current branch ref) to `onto` for a rebase whose pick list is empty but
/// where the branch is not already at `onto`. This happens when every commit was dropped as a
/// clean cherry-pick of the upstream (and the branch diverges from `onto`), or when the branch is
/// strictly behind `onto`. Git resets the branch to `onto` and reports "Successfully rebased and
/// updated <ref>" in both cases (t3404 "do noop when there is nothing to cherry-pick", t3421
/// "rebase -i fast-forwards from ancestor of upstream"). Output goes to stderr to match Git's
/// interactive/merge backend finish message.
fn rebase_reset_to_onto_noop(
    repo: &Repository,
    head: &HeadState,
    head_oid: ObjectId,
    onto_oid: ObjectId,
) -> Result<()> {
    let git_dir = &repo.git_dir;
    let ident = reflog_identity(repo);
    let ra = rebase_reflog_action();

    let onto_obj = repo.odb.read(&onto_oid)?;
    let onto_commit = parse_commit(&onto_obj.data)?;
    let entries = tree_to_index_entries(repo, &onto_commit.tree, "")?;
    let mut idx = Index::new();
    idx.entries = entries;
    idx.sort();
    let old_index = load_index(repo)?;
    if let Some(wt) = &repo.work_tree {
        preflight_cherry_pick_cwd_obstruction(repo, wt, &idx, &BTreeMap::new(), None)?;
        refuse_populated_submodule_tree_replacement(&old_index, &idx, wt)?;
        checkout_merged_index(repo, wt, &old_index, &idx, true)?;
        refresh_index_stat_cache_from_worktree(repo, &mut idx)?;
    }

    fs::write(
        git_dir.join("ORIG_HEAD"),
        format!("{}\n", head_oid.to_hex()),
    )?;
    repo.write_index(&mut idx)?;

    let success_target = if let HeadState::Branch { refname, .. } = head {
        let finish_branch = format!("{ra} (finish): {refname} onto {}", onto_oid.to_hex());
        let finish_head = format!("{ra} (finish): returning to {refname}");
        let _ = append_reflog(
            git_dir,
            refname,
            &head_oid,
            &onto_oid,
            &ident,
            &finish_branch,
            false,
        );
        let _ = append_reflog(
            git_dir,
            "HEAD",
            &onto_oid,
            &onto_oid,
            &ident,
            &finish_head,
            false,
        );
        let ref_path = git_dir.join(refname);
        if let Some(parent) = ref_path.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::write(&ref_path, format!("{}\n", onto_oid.to_hex()))?;
        fs::write(git_dir.join("HEAD"), format!("ref: {refname}\n"))?;
        refname.clone()
    } else {
        let finish_head = format!("{ra} (finish): returning to {}", onto_oid.to_hex());
        let _ = append_reflog(
            git_dir,
            "HEAD",
            &head_oid,
            &onto_oid,
            &ident,
            &finish_head,
            false,
        );
        fs::write(git_dir.join("HEAD"), format!("{}\n", onto_oid.to_hex()))?;
        "HEAD".to_owned()
    };

    run_post_checkout_hook(repo, &head_oid, &onto_oid)?;
    eprintln!("Successfully rebased and updated {success_target}.");
    Ok(())
}

/// Resolve `A...B` in an `--onto` or synthesized `--keep-base` onto name to a single merge base.
///
/// When `keep_base` is true, Git reports `'<upstream>': need exactly one merge base with branch`;
/// otherwise `'<onto>': need exactly one merge base`.
fn merge_base_from_triple_dot_onto(
    repo: &Repository,
    onto_spec: &str,
    keep_base: bool,
    upstream_label: &str,
) -> Result<ObjectId> {
    let Some((left_raw, right_raw)) = split_triple_dot_range(onto_spec) else {
        bail!("internal: expected symmetric-diff revision in onto spec");
    };
    let left_tip = if left_raw.is_empty() {
        resolve_revision_for_range_end(repo, "HEAD")?
    } else {
        resolve_revision_for_range_end(repo, left_raw)?
    };
    let right_tip = if right_raw.is_empty() {
        resolve_revision_for_range_end(repo, "HEAD")?
    } else {
        resolve_revision_for_range_end(repo, right_raw)?
    };
    let left_c = peel_to_commit_for_merge_base(repo, left_tip)?;
    let right_c = peel_to_commit_for_merge_base(repo, right_tip)?;
    let bases = merge_bases_first_vs_rest(repo, left_c, &[right_c])?;
    if bases.len() != 1 {
        if keep_base {
            bail!("'{upstream_label}': need exactly one merge base with branch");
        } else {
            bail!("'{onto_spec}': need exactly one merge base");
        }
    }
    Ok(bases[0])
}

/// Commits to replay for a non-interactive rebase, oldest-first.
///
/// When `filter_cherry_equivalents` is true (Git's default without `--keep-base`), commits whose
/// patch-id matches a commit on the upstream side of the symmetric range `upstream...head` are
/// omitted, matching `sequencer_make_script` with `--cherry-pick --right-only`.
fn collect_rebase_todo_commits(
    repo: &Repository,
    head: ObjectId,
    upstream: ObjectId,
    filter_cherry_equivalents: bool,
) -> Result<Vec<ObjectId>> {
    if !filter_cherry_equivalents {
        return collect_commits_to_replay(repo, head, upstream);
    }

    // Mirror Git's `sequencer_make_script`: walk `upstream..head` with `--cherry-mark`, then drop
    // only commits that are patch-equivalent (`PATCHSAME`) *and not empty*. Commits that start
    // empty must never be dropped by cherry detection — every empty commit has the same (empty)
    // patch-id, so a plain `--cherry-pick` would wrongly drop begin-empty commits (t3421 -m/-i
    // "keeps begin-empty commits"). Begin-empty handling is left to the keep-empty logic instead.
    let bases = merge_bases_first_vs_rest(repo, upstream, &[head])?;
    let negative: Vec<String> = bases.iter().map(|b| b.to_hex()).collect();
    let result = rev_list(
        repo,
        &[upstream.to_hex(), head.to_hex()],
        &negative,
        &RevListOptions {
            cherry_mark: true,
            right_only: true,
            left_right: true,
            symmetric_left: Some(upstream),
            symmetric_right: Some(head),
            ordering: OrderingMode::Topo,
            ..Default::default()
        },
    )?;

    let cherry_equivalent = result.cherry_equivalent;
    let mut commits = result.commits;
    commits.retain(|oid| {
        if !cherry_equivalent.contains(oid) {
            return true;
        }
        // Equivalent: drop unless the commit started empty.
        is_commit_tree_unchanged(repo, oid).unwrap_or(false)
    });
    commits.reverse();
    Ok(commits)
}

/// Collect commits to replay: ancestors of `head` that are not ancestors of `upstream`.
/// Returns them oldest-first.
fn collect_commits_to_replay(
    repo: &Repository,
    head: ObjectId,
    upstream: ObjectId,
) -> Result<Vec<ObjectId>> {
    let result = rev_list(
        repo,
        &[head.to_hex()],
        &[upstream.to_hex()],
        &RevListOptions {
            ordering: OrderingMode::Topo,
            ..Default::default()
        },
    )
    .map_err(|e| anyhow::anyhow!("{e}"))?;
    let mut commits = result.commits;
    commits.reverse();
    Ok(commits)
}

/// Commits to replay for `rebase --root --onto <onto>`: same set as `git rev-list <onto>..<head>`.
///
/// When `exclude_merges` is false (the default, sequential/apply backend), the walk follows
/// first-parent and *includes* merge commits, which are later flattened by recording the second
/// parent's message ([`message_for_root_replayed_commit`]). When `exclude_merges` is true
/// (interactive `-i` without `--rebase-merges`), it mirrors upstream `sequencer_make_script`, which
/// sets `revs.max_parents = 1`: the full topological history is walked but merge commits are
/// omitted entirely (their non-merge ancestors on every side are still picked individually). This
/// keeps `git rebase -i --root` from emitting `pick <merge>` lines, which `pick` rejects
/// (t3412 `rebase -i --root with conflict`).
///
/// Order matches `git rev-list` default output reversed (oldest first).
fn collect_commits_for_root_rebase(
    repo: &Repository,
    head: ObjectId,
    onto: ObjectId,
    filter_redundant: bool,
    exclude_merges: bool,
) -> Result<Vec<ObjectId>> {
    let mut opts = RevListOptions::default();
    opts.reverse = true;
    if exclude_merges {
        opts.max_parents = Some(1);
        opts.ordering = OrderingMode::Topo;
    } else {
        opts.first_parent = true;
        opts.ordering = OrderingMode::Default;
    }
    let listed = if onto.is_zero() {
        // `rebase --root` without `--onto`: replay the full first-parent chain from the branch tip
        // (Git uses an empty lower bound; we cannot pass the null OID through rev-list).
        rev_list(repo, &[head.to_hex()], &[], &opts).map_err(|e| anyhow::anyhow!("{e}"))?
    } else {
        let range = format!("{}..{}", onto.to_hex(), head.to_hex());
        let (positive, negative) = split_revision_token(&range);
        rev_list(repo, &positive, &negative, &opts).map_err(|e| anyhow::anyhow!("{e}"))?
    };
    if filter_redundant {
        filter_redundant_patch_commits(repo, onto, &listed.commits)
    } else {
        Ok(listed.commits)
    }
}

/// Drop commits whose patch-id already exists on `onto` or earlier in the replay list.
///
/// Matches Git's "skipped previously applied commit" behaviour during `rebase --root`.
fn filter_redundant_patch_commits(
    repo: &Repository,
    onto: ObjectId,
    ordered: &[ObjectId],
) -> Result<Vec<ObjectId>> {
    let mut seen_patch_ids: HashSet<ObjectId> = HashSet::new();
    if !onto.is_zero() {
        for oid in ancestor_closure(repo, onto)? {
            let obj = match repo.odb.read(&oid) {
                Ok(o) => o,
                Err(_) => continue,
            };
            if obj.kind != ObjectKind::Commit {
                continue;
            }
            let commit = match parse_commit(&obj.data) {
                Ok(c) => c,
                Err(_) => continue,
            };
            if commit.parents.len() > 1 {
                continue;
            }
            if let Some(pid) = compute_patch_id(&repo.odb, &oid)? {
                seen_patch_ids.insert(pid);
            }
        }
    }

    let mut out = Vec::new();
    for &oid in ordered {
        let obj = repo.odb.read(&oid)?;
        if obj.kind != ObjectKind::Commit {
            continue;
        }
        let commit = parse_commit(&obj.data)?;
        if commit.parents.len() > 1 {
            out.push(oid);
            continue;
        }
        // Commits that start empty are never dropped by cherry-pick detection (all empty commits
        // share the same empty patch-id); their fate is decided by keep-empty handling instead.
        if is_commit_tree_unchanged(repo, &oid).unwrap_or(false) {
            out.push(oid);
            continue;
        }
        let Some(pid) = compute_patch_id(&repo.odb, &oid)? else {
            out.push(oid);
            continue;
        };
        if seen_patch_ids.contains(&pid) {
            continue;
        }
        seen_patch_ids.insert(pid);
        out.push(oid);
    }
    Ok(out)
}

fn filter_merge_commits(repo: &Repository, ordered: &[ObjectId]) -> Result<Vec<ObjectId>> {
    let mut out = Vec::new();
    for &oid in ordered {
        let obj = repo.odb.read(&oid)?;
        if obj.kind != ObjectKind::Commit {
            continue;
        }
        let commit = parse_commit(&obj.data)?;
        if commit.parents.len() <= 1 {
            out.push(oid);
        }
    }
    Ok(out)
}

/// Whether `onto` is a strict fast-forward of `head` (linear single-parent history from `head` to `onto`).
fn can_fast_forward(repo: &Repository, head: ObjectId, onto: ObjectId) -> Result<bool> {
    if head == onto {
        return Ok(false);
    }
    if !is_ancestor(repo, head, onto)? {
        return Ok(false);
    }
    let bases = merge_bases_first_vs_rest(repo, onto, &[head])?;
    if bases.len() != 1 || bases[0] != head {
        return Ok(false);
    }
    is_linear_history(repo, head, onto)
}

fn is_linear_history(repo: &Repository, from: ObjectId, to: ObjectId) -> Result<bool> {
    let mut current = to;
    loop {
        if current == from {
            return Ok(true);
        }
        let obj = repo.odb.read(&current)?;
        let commit = parse_commit(&obj.data)?;
        if commit.parents.len() != 1 {
            return Ok(false);
        }
        current = commit.parents[0];
    }
}

/// Git's `can_fast_forward` for preemptive up-to-date / noop detection.
fn rebase_can_preemptive_ff(
    repo: &Repository,
    onto: ObjectId,
    upstream: ObjectId,
    restrict_revision: Option<ObjectId>,
    head: ObjectId,
) -> Result<bool> {
    let bases = merge_bases_first_vs_rest(repo, onto, &[head])?;
    if bases.len() != 1 || bases[0] != onto {
        return Ok(false);
    }
    // When fork-point is active, Git's `restrict_revision` holds the fork-point commit and the
    // preemptive fast-forward only fires if it coincides with the branch base (`onto`). If the
    // fork-point is a descendant of `onto` (the upstream branch was rewound after `head` forked
    // off it), the branch carries commits that are no longer in upstream and must be replayed onto
    // the new base instead of being treated as up-to-date (t3400 "default to common base in
    // @{upstream}s reflog if no upstream arg"). Mirrors `can_fast_forward()` in
    // `git/builtin/rebase.c`.
    if let Some(restrict) = restrict_revision {
        if restrict != onto {
            return Ok(false);
        }
    }
    let up_bases = merge_bases_first_vs_rest(repo, upstream, &[head])?;
    if up_bases.len() != 1 || up_bases[0] != onto {
        return Ok(false);
    }
    is_linear_history(repo, onto, head)
}

fn print_rebase_diffstat(
    repo: &Repository,
    branch_base: Option<ObjectId>,
    onto_oid: ObjectId,
) -> Result<()> {
    let old_tree = if let Some(bb) = branch_base {
        let obj = repo.odb.read(&bb)?;
        let c = parse_commit(&obj.data)?;
        Some(c.tree)
    } else {
        None
    };
    let empty_tree: ObjectId = "4b825dc642cb6eb9a060e54bf8d69288fbee4904"
        .parse()
        .map_err(|_| anyhow::anyhow!("internal: empty tree object id"))?;
    let new_tree = if onto_oid.is_zero() {
        empty_tree
    } else {
        let new_obj = repo.odb.read(&onto_oid)?;
        let new_commit = parse_commit(&new_obj.data)?;
        new_commit.tree
    };
    let entries = diff::diff_trees(&repo.odb, old_tree.as_ref(), Some(&new_tree), "")?;
    print_diffstat_from_entries(repo, &entries);
    Ok(())
}

fn print_diffstat_from_entries(repo: &Repository, entries: &[DiffEntry]) {
    if entries.is_empty() {
        return;
    }

    struct StatEntry {
        path: String,
        insertions: usize,
        deletions: usize,
        is_new: bool,
        is_deleted: bool,
        new_mode: Option<u32>,
    }

    let mut stats: Vec<StatEntry> = Vec::new();
    let mut total_ins = 0usize;
    let mut total_del = 0usize;

    for entry in entries {
        let path = entry
            .new_path
            .as_deref()
            .or(entry.old_path.as_deref())
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_else(|| "unknown".to_string());
        let is_new = entry.old_oid == diff::zero_oid();
        let is_deleted = entry.new_oid == diff::zero_oid();

        let old_content = if !is_new {
            repo.odb
                .read(&entry.old_oid)
                .ok()
                .map(|o| String::from_utf8_lossy(&o.data).to_string())
        } else {
            None
        };
        let new_content = if !is_deleted {
            repo.odb
                .read(&entry.new_oid)
                .ok()
                .map(|o| String::from_utf8_lossy(&o.data).to_string())
        } else {
            None
        };

        let (ins, del) = count_changes(
            old_content.as_deref().unwrap_or(""),
            new_content.as_deref().unwrap_or(""),
        );

        total_ins += ins;
        total_del += del;

        let mode_num = u32::from_str_radix(&entry.new_mode, 8).unwrap_or(0o100644);
        stats.push(StatEntry {
            path: path.to_owned(),
            insertions: ins,
            deletions: del,
            is_new,
            is_deleted,
            new_mode: if is_new { Some(mode_num) } else { None },
        });
    }

    let display_names: Vec<String> = stats.iter().map(|s| s.path.clone()).collect();
    let max_path_len = display_names.iter().map(|s| s.len()).max().unwrap_or(0);
    let max_change = stats
        .iter()
        .map(|s| s.insertions + s.deletions)
        .max()
        .unwrap_or(0);
    let count_width = if max_change == 0 {
        1
    } else {
        format!("{}", max_change).len()
    };

    for (i, s) in stats.iter().enumerate() {
        let total = s.insertions + s.deletions;
        let plus = "+".repeat(s.insertions.min(50));
        let minus = "-".repeat(s.deletions.min(50));
        println!(
            " {:<width$} | {:>cw$} {}{}",
            display_names[i],
            total,
            plus,
            minus,
            width = max_path_len,
            cw = count_width
        );
    }

    let files_changed = stats.len();
    let mut parts = Vec::new();
    parts.push(format!(
        "{} file{} changed",
        files_changed,
        if files_changed != 1 { "s" } else { "" }
    ));
    if total_ins > 0 {
        parts.push(format!(
            "{} insertion{}",
            total_ins,
            if total_ins != 1 { "s(+)" } else { "(+)" }
        ));
    }
    if total_del > 0 {
        parts.push(format!(
            "{} deletion{}",
            total_del,
            if total_del != 1 { "s(-)" } else { "(-)" }
        ));
    }
    println!(" {}", parts.join(", "));
}

/// Materialize `rebase-merge/done` when a *non-interactive* merge-backend rebase stops on a
/// conflict. The merge backend always leaves a `done` file listing the picks already applied; the
/// non-interactive backend keeps the conflicting pick in `git-rebase-todo`, so `done` holds only the
/// strictly-earlier picks (t5520 "pull --rebase does not reapply old patches" reads both files).
/// Interactive rebases already build `done` incrementally via `append_interactive_rebase_done_line`.
fn write_non_interactive_done_on_stop(
    rb_dir: &Path,
    todo: &[&str],
    current: usize,
    rebase_interactive: bool,
) {
    if rebase_interactive {
        return;
    }
    let body = todo[..current].join("\n");
    let body = if body.is_empty() {
        String::new()
    } else {
        format!("{body}\n")
    };
    let _ = fs::write(rb_dir.join("done"), body);
}

/// Append a completed interactive todo line to `rebase-merge/done` (Git `sequencer.c:save_todo`).
fn append_interactive_rebase_done_line(rb_dir: &Path, todo_line: &str) -> Result<()> {
    if !rb_dir.join("interactive").exists() {
        return Ok(());
    }
    let mut line = todo_line.trim_end().to_string();
    line.push('\n');
    let mut f = OpenOptions::new()
        .create(true)
        .append(true)
        .open(rb_dir.join("done"))?;
    f.write_all(line.as_bytes())?;
    Ok(())
}

fn write_rebase_todo_slice(rb_dir: &Path, lines: &[&str]) -> Result<()> {
    let body = if lines.is_empty() {
        String::new()
    } else {
        lines.join("\n") + "\n"
    };
    let n = count_rebase_todo_actionable_lines(&body);
    fs::write(rb_dir.join("todo"), &body)?;
    fs::write(rb_dir.join("git-rebase-todo"), &body)?;
    fs::write(rb_dir.join("end"), n.to_string())?;
    fs::write(rb_dir.join("msgnum"), "1")?;
    Ok(())
}

/// Run the persisted global `--exec` commands (from `rebase-merge/exec`) after a *merge* step.
///
/// Git inserts an `exec` instruction after every `pick` AND `merge` command (`todo_list_add_exec_commands`).
/// grit's pick path runs the persisted exec inline; merges share the same `rebase-merge/exec`
/// file but did not run it, so a `done`-capturing `-x` (t3430 'truncate label names', which greps
/// the post-merge `done` for the merge-tip label) never fired after the trailing merge. This runs
/// each exec command in order; on failure it reschedules the failed exec (and the rest of the todo)
/// and exits with the command's status, mirroring the pick-path behavior.
fn run_global_exec_after_merge(
    repo: &Repository,
    rb_dir: &Path,
    todo: &[&str],
    i: usize,
) -> Result<()> {
    let Ok(global_exec_raw) = fs::read_to_string(rb_dir.join("exec")) else {
        return Ok(());
    };
    let exec_cmds: Vec<String> = global_exec_raw
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .map(ToOwned::to_owned)
        .collect();
    for (exec_idx, global_exec) in exec_cmds.iter().enumerate() {
        if !rebase_quiet(rb_dir) {
            eprintln!("Executing: {}", global_exec);
        }
        let status = std::process::Command::new("sh")
            .arg("-c")
            .arg(global_exec)
            .current_dir(repo.work_tree.as_deref().unwrap_or_else(|| Path::new(".")))
            .status()
            .with_context(|| format!("failed to execute: {}", global_exec))?;
        if !status.success() {
            let code = status.code().filter(|code| *code != 127).unwrap_or(1);
            let reschedule = rb_dir.join("reschedule-failed-exec").exists()
                && !rb_dir.join("no-reschedule-failed-exec").exists();
            eprintln!(
                "warning: execution failed for: {}\n\
                 hint: You can fix the problem, and then run\n\
                 hint:   grit rebase --continue",
                global_exec
            );
            if reschedule {
                eprintln!("hint: '{}' has been rescheduled", global_exec);
            }
            let mut remaining: Vec<String> = exec_cmds[exec_idx..]
                .iter()
                .map(|c| format!("exec {c}"))
                .collect();
            remaining.extend(todo[i + 1..].iter().map(|line| (*line).to_owned()));
            let remaining_refs: Vec<&str> = remaining.iter().map(String::as_str).collect();
            write_rebase_todo_slice(rb_dir, &remaining_refs)?;
            std::process::exit(code);
        }
    }
    Ok(())
}

/// Replay all remaining commits from the todo list.
///
/// After each successful step, the todo file is re-read so hooks and `exec` lines can append work
/// (matching Git's `git-rebase-todo` behavior, including `GIT_REBASE_TODO`).
fn replay_remaining(
    repo: &Repository,
    rb_dir: &Path,
    autostash_oid: Option<ObjectId>,
    backend: RebaseBackend,
    had_rebase_autostash: bool,
    force_rewrite_commits: bool,
) -> Result<()> {
    let git_dir = &repo.git_dir;
    let ra = load_rebase_reflog_action(rb_dir);
    let ident = reflog_identity(repo);

    let _ = fs::remove_file(rb_dir.join("stopped-sha"));

    let rebase_interactive = rb_dir.join("interactive").exists();

    let total_rebase_cmds_seed: usize = fs::read_to_string(rb_dir.join("total-cmds"))
        .ok()
        .and_then(|s| s.trim().parse().ok())
        .unwrap_or(0);
    let mut completed_cmds: usize = fs::read_to_string(rb_dir.join("completed-cmds"))
        .ok()
        .and_then(|s| s.trim().parse().ok())
        .unwrap_or(0);
    let rebase_verbose = rb_dir.join("verbose").exists();

    let print_am_style_progress = matches!(backend, RebaseBackend::Apply);
    let rewind_marker = rb_dir.join("rewind-notice");
    let mut rewind_done = false;

    // `core.commentchar` may differ from `#`; lines using the configured prefix must be skipped
    // during replay too (t3404 "respects core.commentchar").
    let replay_comment_prefix = ConfigSet::load(Some(git_dir), true)
        .ok()
        .map(|c| comment_line_prefix_full(&c).into_owned())
        .unwrap_or_else(|| "#".to_string());

    'rebase_loop: loop {
        let todo_content = read_rebase_todo_file(repo, rb_dir)?;
        let todo: Vec<&str> = todo_content
            .lines()
            .map(str::trim)
            .filter(|l| {
                !l.is_empty()
                    && !l.starts_with(replay_comment_prefix.as_str())
                    && !rebase_todo_line_is_comment(l)
            })
            .collect();
        let total_rebase_cmds: usize = fs::read_to_string(rb_dir.join("total-cmds"))
            .ok()
            .and_then(|s| s.trim().parse().ok())
            .unwrap_or_else(|| todo.len().max(total_rebase_cmds_seed));
        let _end_file: usize = fs::read_to_string(rb_dir.join("end"))?.trim().parse()?;
        let msgnum: usize = fs::read_to_string(rb_dir.join("msgnum"))?.trim().parse()?;

        if !rewind_done && !todo.is_empty() {
            if print_am_style_progress && !rewind_marker.exists() && !rebase_quiet(rb_dir) {
                println!("First, rewinding head to replay your work on top of it...");
                flush_rebase_stdout();
                let _ = fs::write(&rewind_marker, "");
            } else if !print_am_style_progress && !rewind_marker.exists() {
                let _ = fs::write(&rewind_marker, "");
            }
            rewind_done = true;
        }

        for i in (msgnum - 1)..todo.len() {
            let line = todo[i];
            let step = parse_rebase_replay_step(repo, line, rebase_interactive)?
                .ok_or_else(|| anyhow::anyhow!("malformed rebase todo line: {line}"))?;

            completed_cmds += 1;
            fs::write(rb_dir.join("completed-cmds"), completed_cmds.to_string())?;
            if rebase_interactive {
                eprint!(
                    "Rebasing ({}/{}){}",
                    completed_cmds,
                    total_rebase_cmds,
                    if rebase_verbose { "\n" } else { "\r" }
                );
            }

            fs::write(rb_dir.join("msgnum"), (i + 1).to_string())?;
            fs::write(rb_dir.join("next"), (i + 1).to_string())?;

            match step {
                RebaseReplayStep::Noop => {
                    // Git consumes `drop`/`noop` lines into `done` like any other command, so a later
                    // `--edit-todo` missing-commit check treats explicitly dropped commits as "seen"
                    // (t3404 "rebase.missingCommitsCheck = error after resolving conflicts").
                    if rebase_interactive {
                        append_interactive_rebase_done_line(rb_dir, todo[i])?;
                    }
                }
                RebaseReplayStep::UpdateRef(refname) => {
                    // Record the current HEAD as the ref's pending new tip; the ref itself is
                    // moved at completion by `apply_update_refs` (Git's `do_update_ref`).
                    if rebase_interactive {
                        append_interactive_rebase_done_line(rb_dir, todo[i])?;
                    }
                    do_update_ref(repo, rb_dir, &refname)?;
                }
                RebaseReplayStep::Break => {
                    let _ = fs::remove_file(rb_dir.join("current"));
                    let _ = fs::remove_file(rb_dir.join("current-cmd"));
                    let _ = fs::remove_file(rb_dir.join("current-final-fixup"));
                    if rebase_interactive {
                        append_interactive_rebase_done_line(rb_dir, todo[i])?;
                    }
                    let remaining: Vec<&str> = todo[i + 1..].to_vec();
                    write_rebase_todo_slice(rb_dir, &remaining)?;
                    std::process::exit(0);
                }
                RebaseReplayStep::Label(name) => {
                    // Git consumes the todo item into `done` before executing it; mirror that so
                    // `done` reflects `label`/`reset` lines even on later failure (t3430).
                    if rebase_interactive {
                        append_interactive_rebase_done_line(rb_dir, todo[i])?;
                    }
                    let head_state = resolve_head(git_dir)?;
                    let head_oid_label = head_state
                        .oid()
                        .cloned()
                        .ok_or_else(|| anyhow::anyhow!("could not read HEAD"))?;
                    let refname = format!("refs/rewritten/{name}");
                    write_ref(git_dir, &refname, &head_oid_label)?;
                    append_ref_to_delete_list(rb_dir, &refname)?;
                }
                RebaseReplayStep::Reset(arg) => {
                    // Append to `done` before the reset runs: a failed `reset` (e.g. untracked file
                    // would be overwritten) must still leave the `reset` line in `done` (t3430).
                    if rebase_interactive {
                        append_interactive_rebase_done_line(rb_dir, todo[i])?;
                    }
                    let head_state = resolve_head(git_dir)?;
                    let target = resolve_reset_target(repo, rb_dir, arg.trim())?;
                    let suffix = first_token_reset_arg(arg.trim());
                    if let Err(e) =
                        reset_worktree_to_commit(repo, git_dir, &head_state, target, suffix)
                    {
                        // Persist the remaining todo starting at the failed reset so
                        // `rebase --continue` re-runs it after the user fixes the worktree. Git
                        // keeps the failed command at the head of the todo (it is re-consumed into
                        // `done` on the retry, yielding the duplicated `reset` line in t3430).
                        let remaining: Vec<&str> = todo[i..].to_vec();
                        write_rebase_todo_slice(rb_dir, &remaining)?;
                        eprintln!("{e:#}");
                        std::process::exit(1);
                    }
                }
                RebaseReplayStep::MergePlain { merge_args } => {
                    let head_before = resolve_head(git_dir)?;
                    let old_head = head_before.oid().cloned().unwrap_or_else(diff::zero_oid);
                    if let Ok(sq_hex) = fs::read_to_string(rb_dir.join("squash-onto")) {
                        if let Ok(sq_oid) = ObjectId::from_hex(sq_hex.trim()) {
                            let (heads, _) = parse_merge_todo_arg_list(&merge_args);
                            if heads.len() == 1 {
                                if head_before.oid().copied() == Some(sq_oid) {
                                    let only = commit_oid_for_rebase_label(repo, &heads[0])?;
                                    reset_worktree_to_commit(
                                        repo,
                                        git_dir,
                                        &head_before,
                                        only,
                                        &heads[0],
                                    )?;
                                    let rest: Vec<&str> = todo[i + 1..].to_vec();
                                    write_rebase_todo_slice(rb_dir, &rest)?;
                                    continue 'rebase_loop;
                                }
                            }
                        }
                    }
                    let st = run_plain_merge_for_rebase(repo, merge_args.as_str())?;
                    if !st.success() {
                        if git_dir.join("MERGE_HEAD").exists() {
                            // Content conflict on a plain `merge <branch>` (no `-C`). Git's
                            // `error_with_patch` writes the synthesized merge message to
                            // `.git/rebase-merge/message` and drops the merge from the todo: it is
                            // now in progress via MERGE_HEAD, so `--continue` commits it through the
                            // staged-changes path rather than re-running `merge` (t3430 'failed
                            // `merge <branch>` does not crash'). No `patch` is written because there
                            // is no original merge commit to diff against.
                            let (heads, oneline) = parse_merge_todo_arg_list(&merge_args);
                            let merge_message = oneline.unwrap_or_else(|| {
                                if heads.len() > 1 {
                                    format!("Merge branches {}", heads.join(" "))
                                } else {
                                    format!(
                                        "Merge branch '{}'",
                                        heads.first().cloned().unwrap_or_default()
                                    )
                                }
                            });
                            if !rb_dir.join("message").exists() {
                                let _ =
                                    fs::write(rb_dir.join("message"), format!("{merge_message}\n"));
                            }
                            let rest: Vec<&str> = todo[i + 1..].to_vec();
                            write_rebase_todo_slice(rb_dir, &rest)?;
                            std::process::exit(1);
                        }
                        std::process::exit(st.code().unwrap_or(1));
                    }
                    let head = resolve_head(git_dir)?;
                    let new_oid = *head
                        .oid()
                        .ok_or_else(|| anyhow::anyhow!("HEAD has no OID"))?;
                    let (_, oneline) = parse_merge_todo_arg_list(&merge_args);
                    let subject = oneline.unwrap_or_else(|| "merge".to_owned());
                    let msg = format!("{ra} (merge): {subject}");
                    let _ =
                        append_reflog(git_dir, "HEAD", &old_head, &new_oid, &ident, &msg, false);
                    let rest: Vec<&str> = todo[i + 1..].to_vec();
                    write_rebase_todo_slice(rb_dir, &rest)?;
                    // Git inserts an `exec` after every merge too; run the persisted global exec.
                    run_global_exec_after_merge(repo, rb_dir, &todo, i)?;
                    continue 'rebase_loop;
                }
                RebaseReplayStep::Exec(exec_cmd) => {
                    let _ = fs::remove_file(rb_dir.join("current"));
                    let _ = fs::remove_file(rb_dir.join("current-cmd"));
                    let _ = fs::remove_file(rb_dir.join("current-final-fixup"));
                    if rebase_interactive {
                        append_interactive_rebase_done_line(rb_dir, todo[i])?;
                    }
                    let remaining_before: Vec<&str> = todo[i + 1..].to_vec();
                    let rem_body = if remaining_before.is_empty() {
                        String::new()
                    } else {
                        remaining_before.join("\n") + "\n"
                    };
                    write_rebase_todo_file(repo, rb_dir, &rem_body)?;
                    fs::write(rb_dir.join("msgnum"), "1")?;
                    let rem_n = count_rebase_todo_actionable_lines(&rem_body);
                    fs::write(rb_dir.join("end"), rem_n.to_string())?;
                    if !rebase_quiet(rb_dir) {
                        eprintln!("Executing: {}", exec_cmd);
                    }
                    let status = std::process::Command::new("sh")
                        .arg("-c")
                        .arg(&exec_cmd)
                        .current_dir(repo.work_tree.as_deref().unwrap_or_else(|| Path::new(".")))
                        .status()
                        .with_context(|| format!("failed to execute: {}", exec_cmd))?;
                    if !status.success() {
                        let code = status.code().filter(|code| *code != 127).unwrap_or(1);
                        let reschedule = rb_dir.join("reschedule-failed-exec").exists()
                            && !rb_dir.join("no-reschedule-failed-exec").exists();
                        eprintln!(
                            "warning: execution failed for: {}\n\
                         hint: You can fix the problem, and then run\n\
                         hint:   grit rebase --continue",
                            exec_cmd
                        );
                        if reschedule {
                            eprintln!("hint: '{}' has been rescheduled", exec_cmd);
                            let mut remaining = vec![format!("exec {exec_cmd}")];
                            remaining.extend(todo[i + 1..].iter().map(|line| (*line).to_owned()));
                            let refs: Vec<&str> = remaining.iter().map(String::as_str).collect();
                            write_rebase_todo_slice(rb_dir, &refs)?;
                        }
                        // The todo was already written as `todo[i+1..]` above (matching Git's
                        // `save_todo` with `!reschedule`, which drops the executing command). With
                        // `rebase.rescheduleFailedExec` unset (Git's default), a failed `exec` is
                        // NOT rescheduled: `rebase --continue` must resume at the NEXT command, not
                        // re-run the failed exec. Leave the todo as-is and exit; re-writing
                        // `todo[i..]` here would re-add the exec and loop forever (t5407 `-i (exec)`).
                        std::process::exit(code);
                    }
                    if !worktree_matches_head(repo, git_dir)? {
                        eprintln!(
                            "execution succeeded: {} but left changes to the index and/or the working tree",
                            exec_cmd
                        );
                        eprintln!("warning: execution succeeded: {}", exec_cmd);
                        eprintln!(
                            "hint: You can fix the problem, and then run\n\
                         hint:   grit rebase --continue"
                        );
                        std::process::exit(1);
                    }
                    continue 'rebase_loop;
                }
                RebaseReplayStep::MergeReuseMessage {
                    merge_oid,
                    merge_args,
                    edit_message,
                } => {
                    let _ = fs::remove_file(rb_dir.join("current"));
                    let _ = fs::remove_file(rb_dir.join("current-cmd"));
                    let _ = fs::remove_file(rb_dir.join("current-final-fixup"));
                    // Consume the merge line into `done` before running it: git records the merge
                    // command in `done` even when it fails (untracked-file block or conflict), while
                    // a rescheduled failure also keeps it at the head of the todo (t3430 'failed
                    // merge -C writes patch'). The commit being replayed is exposed as REBASE_HEAD.
                    if rebase_interactive {
                        append_interactive_rebase_done_line(rb_dir, todo[i])?;
                    }
                    // A `merge -c` (reword) opens the commit editor, and the test/user editor may run
                    // `git rebase --edit-todo` to reload the remaining list. Write the remaining todo
                    // (`todo[i+1..]`) to disk *before* the merge so the editor sees the correct list;
                    // the Completed arm then re-reads the (possibly edited) file rather than
                    // clobbering it with this stale in-memory slice (t3430 'merge -c ... reloads
                    // todo-list').
                    if edit_message {
                        let rest_before: Vec<&str> = todo[i + 1..].to_vec();
                        write_rebase_todo_slice(rb_dir, &rest_before)?;
                    }
                    let merge_head_commit = peel_to_commit_for_merge_base(repo, merge_oid)?;
                    let old_head = resolve_head(git_dir)?
                        .oid()
                        .cloned()
                        .unwrap_or_else(diff::zero_oid);
                    let next_after =
                        peek_next_rebase_flush_hint(repo, &todo, i + 1, rebase_interactive);
                    match rebase_merge_reuse_message(
                        repo,
                        git_dir,
                        rb_dir,
                        &merge_oid,
                        merge_args.as_str(),
                        edit_message,
                        next_after,
                        !force_rewrite_commits,
                    ) {
                        Ok(RebaseMergeReuseOutcome::Completed) => {
                            let head = resolve_head(git_dir)?;
                            let new_oid = *head
                                .oid()
                                .ok_or_else(|| anyhow::anyhow!("HEAD has no OID"))?;
                            // `merge -C <ref>` may name an annotated tag (e.g. `merge -C H`); peel
                            // to the underlying commit before parsing, else `parse_commit` on a tag
                            // object dies with "commit missing author header".
                            let merge_commit_oid = peel_to_commit_for_merge_base(repo, merge_oid)?;
                            let merge_obj = repo.odb.read(&merge_commit_oid)?;
                            let mc = parse_commit(&merge_obj.data)?;
                            let subject = mc.message.lines().next().unwrap_or("");
                            if print_am_style_progress && !rebase_quiet(rb_dir) {
                                println!("Applying: {}", subject);
                                flush_rebase_stdout();
                            } else if !rebase_quiet(rb_dir) {
                                eprintln!("Applying: {}", subject);
                            }
                            let msg = format!("{ra} (merge): {subject}");
                            let _ = append_reflog(
                                git_dir, "HEAD", &old_head, &new_oid, &ident, &msg, false,
                            );
                            // For `merge -c` (reword) the remaining todo was written before the
                            // merge and possibly re-edited by the editor's `git rebase --edit-todo`;
                            // leave the on-disk file alone and let the loop re-read it. For a plain
                            // `merge -C` (no reword) drop the just-completed merge from the in-memory
                            // slice as before.
                            if !edit_message {
                                let rest: Vec<&str> = todo[i + 1..].to_vec();
                                write_rebase_todo_slice(rb_dir, &rest)?;
                            }
                            // Git inserts an `exec` after every merge; run the persisted global
                            // exec so a `-x` command captures the post-merge `done` (t3430
                            // 'truncate label names').
                            run_global_exec_after_merge(repo, rb_dir, &todo, i)?;
                            continue 'rebase_loop;
                        }
                        Ok(RebaseMergeReuseOutcome::Conflict) => {
                            let _ = fs::remove_file(rb_dir.join("current"));
                            let _ = fs::remove_file(rb_dir.join("current-cmd"));
                            let _ = fs::remove_file(rb_dir.join("current-final-fixup"));
                            // The conflicted merge stays at the head of the todo so the
                            // `rebase --continue` merge path can commit it once resolved. Write a
                            // `patch` for the conflicted merge and expose it as REBASE_HEAD (t3430).
                            let remaining_merge: Vec<&str> = todo[i..].to_vec();
                            let rem_body = remaining_merge.join("\n") + "\n";
                            let rem_n = count_rebase_todo_actionable_lines(&rem_body);
                            fs::write(rb_dir.join("todo"), &rem_body)?;
                            fs::write(rb_dir.join("git-rebase-todo"), &rem_body)?;
                            fs::write(rb_dir.join("msgnum"), "1")?;
                            fs::write(rb_dir.join("end"), rem_n.to_string())?;
                            let _ = fs::write(
                                rb_dir.join("stopped-sha"),
                                format!("{}\n", merge_oid.to_hex()),
                            );
                            let merge_commit_oid_cf =
                                peel_to_commit_for_merge_base(repo, merge_oid)?;
                            fs::write(
                                git_dir.join("REBASE_HEAD"),
                                format!("{}\n", merge_commit_oid_cf.to_hex()),
                            )?;
                            let _ = write_rebase_patch_file(repo, rb_dir, &merge_commit_oid_cf);
                            let merge_obj_cf = repo.odb.read(&merge_commit_oid_cf)?;
                            let mc_cf = parse_commit(&merge_obj_cf.data)?;
                            let subj_cf = mc_cf.message.lines().next().unwrap_or("");
                            eprintln!(
                                "error: could not apply {}... {}\n\
                             hint: Resolve all conflicts manually, mark them as resolved with\n\
                             hint: \"grit add <pathspec>\", then run \"grit rebase --continue\".\n\
                             hint: To skip this commit, run \"grit rebase --skip\".\n\
                             hint: To abort, run \"grit rebase --abort\".",
                                &merge_oid.to_hex()[..7],
                                subj_cf
                            );
                            std::process::exit(1);
                        }
                        Ok(RebaseMergeReuseOutcome::Blocked) => {
                            // Reschedule: keep the failed merge at the head of the todo so
                            // `rebase --continue` retries it after the user fixes the worktree, and
                            // expose the replayed merge commit as REBASE_HEAD (t3430).
                            let remaining_blk: Vec<&str> = todo[i..].to_vec();
                            let blk_body = remaining_blk.join("\n") + "\n";
                            let blk_n = count_rebase_todo_actionable_lines(&blk_body);
                            fs::write(rb_dir.join("todo"), &blk_body)?;
                            fs::write(rb_dir.join("git-rebase-todo"), &blk_body)?;
                            fs::write(rb_dir.join("msgnum"), "1")?;
                            fs::write(rb_dir.join("end"), blk_n.to_string())?;
                            fs::write(
                                git_dir.join("REBASE_HEAD"),
                                format!("{}\n", merge_head_commit.to_hex()),
                            )?;
                            std::process::exit(1);
                        }
                        Err(e) => {
                            let _ = fs::remove_file(rb_dir.join("rebase-merge-source"));
                            let _ = fs::remove_file(rb_dir.join("rebase-merge-args"));
                            eprintln!("{e:#}");
                            std::process::exit(1);
                        }
                    }
                }
                RebaseReplayStep::Edit(commit_oid) => {
                    let todo_cmd = RebaseTodoCmd::Pick;

                    let commit_hex = commit_oid.to_hex();
                    fs::write(rb_dir.join("current"), format!("{commit_hex}\n"))?;
                    fs::write(
                        rb_dir.join("current-cmd"),
                        format!("{}\n", todo_cmd.as_str()),
                    )?;
                    let _ = fs::remove_file(rb_dir.join("current-final-fixup"));

                    let old_head = resolve_head(git_dir)?
                        .oid()
                        .cloned()
                        .unwrap_or_else(diff::zero_oid);

                    append_interactive_rebase_done_line(rb_dir, todo[i])?;
                    let pick_result = if todo_cmd == RebaseTodoCmd::Reword {
                        let final_fixup =
                            is_final_fixup_in_todo(repo, rb_dir, &todo, i, rebase_interactive);
                        let next_after =
                            peek_next_rebase_flush_hint(repo, &todo, i + 1, rebase_interactive);
                        cherry_pick_for_rebase(
                            repo,
                            rb_dir,
                            &commit_oid,
                            backend,
                            todo_cmd,
                            final_fixup,
                            next_after,
                            true,
                            force_rewrite_commits,
                            None,
                        )
                    } else {
                        run_rebase_pick_in_clean_child_process(repo, i, force_rewrite_commits)
                    };
                    match pick_result {
                        Ok(()) => {
                            let head = resolve_head(git_dir)?;
                            let new_oid = *head
                                .oid()
                                .ok_or_else(|| anyhow::anyhow!("HEAD has no OID"))?;
                            let obj = repo.odb.read(&commit_oid)?;
                            let commit = parse_commit(&obj.data)?;
                            let root_rebase = rb_dir.join("root").exists();
                            let msg_for_log =
                                message_for_root_replayed_commit(repo, &commit, root_rebase);
                            let subject = msg_for_log.lines().next().unwrap_or("");
                            if print_am_style_progress && !rebase_quiet(rb_dir) {
                                println!("Applying: {}", subject);
                                flush_rebase_stdout();
                            }
                            let msg = format!("{ra} (pick): {subject}");
                            let _ = append_reflog(
                                git_dir, "HEAD", &old_head, &new_oid, &ident, &msg, false,
                            );

                            let remaining: Vec<&str> = todo[i + 1..].to_vec();
                            write_rebase_todo_slice(rb_dir, &remaining)?;
                            if rebase_interactive {
                                fs::write(rb_dir.join("end"), remaining.len().to_string())?;
                                fs::write(rb_dir.join("msgnum"), "1")?;
                            }
                            let _ = fs::write(
                                rb_dir.join("stopped-sha"),
                                format!("{}\n", commit_oid.to_hex()),
                            );
                            // Git's `intend_to_amend()` records the *current* HEAD (the commit just
                            // replayed for this `edit`) in `rebase-merge/amend`. On `--continue`,
                            // `commit_staged_changes` amends only when HEAD still equals that OID;
                            // if the user committed their own work (HEAD moved), the staged changes
                            // are folded into a fresh commit instead. The original commit OID lives
                            // in `stopped-sha`, so the two must be tracked separately.
                            let _ =
                                fs::write(rb_dir.join("amend"), format!("{}\n", new_oid.to_hex()));
                            let _ = write_rebase_patch_file(&repo, &rb_dir, &commit_oid);
                            let _ = fs::write(rb_dir.join("rebase-amend-continue"), "1\n");
                            let _ = fs::write(rb_dir.join("rebase-edit-continue"), "1\n");
                            // Git's `do_pick_commit` prints the amend hint when stopping for `edit`,
                            // echoing the `-S<key>` signing option (t3404 "rebase -i --gpg-sign").
                            let gpg_opt = gpg_sign_opt_quoted(&rb_dir);
                            eprintln!(
                                "You can amend the commit now, with\n\n  git commit --amend {gpg_opt}\n\nOnce you are satisfied with your changes, run\n\n  git rebase --continue"
                            );
                            std::process::exit(0);
                        }
                        Err(_e) => {
                            let remaining: Vec<&str> = if rebase_interactive {
                                todo[i + 1..].to_vec()
                            } else {
                                todo[i..].to_vec()
                            };
                            write_rebase_todo_slice(rb_dir, &remaining)?;
                            write_non_interactive_done_on_stop(
                                rb_dir,
                                &todo,
                                i,
                                rebase_interactive,
                            );
                            let ff =
                                is_final_fixup_in_todo(repo, rb_dir, &todo, i, rebase_interactive);
                            fs::write(
                                rb_dir.join("current-final-fixup"),
                                if ff { "1\n" } else { "0\n" },
                            )?;
                            let _ = fs::write(
                                rb_dir.join("stopped-sha"),
                                format!("{}\n", commit_oid.to_hex()),
                            );
                            let _ = write_rebase_patch_file(&repo, &rb_dir, &commit_oid);

                            let obj = repo.odb.read(&commit_oid)?;
                            let commit = parse_commit(&obj.data)?;
                            let root_rebase = rb_dir.join("root").exists();
                            let msg_for_log =
                                message_for_root_replayed_commit(repo, &commit, root_rebase);
                            let subject = msg_for_log.lines().next().unwrap_or("");

                            eprintln!(
                                "error: could not apply {}... {}\n\
                             hint: Resolve all conflicts manually, mark them as resolved with\n\
                             hint: \"grit add <pathspec>\", then run \"grit rebase --continue\".\n\
                             hint: To skip this commit, run \"grit rebase --skip\".\n\
                             hint: To abort, run \"grit rebase --abort\".",
                                &commit_oid.to_hex()[..7],
                                subject
                            );
                            std::process::exit(1);
                        }
                    }
                }
                RebaseReplayStep::PickLike {
                    oid: commit_oid,
                    cmd: todo_cmd,
                    fixup_message_mode: _,
                } => {
                    let commit_hex = commit_oid.to_hex();
                    fs::write(rb_dir.join("current"), format!("{commit_hex}\n"))?;
                    fs::write(
                        rb_dir.join("current-cmd"),
                        format!("{}\n", todo_cmd.as_str()),
                    )?;
                    let _ = fs::remove_file(rb_dir.join("current-final-fixup"));

                    let old_head = resolve_head(git_dir)?
                        .oid()
                        .cloned()
                        .unwrap_or_else(diff::zero_oid);

                    append_interactive_rebase_done_line(rb_dir, todo[i])?;
                    match run_rebase_pick_in_clean_child_process(repo, i, force_rewrite_commits) {
                        Ok(()) => {
                            let head = resolve_head(git_dir)?;
                            let new_oid = *head
                                .oid()
                                .ok_or_else(|| anyhow::anyhow!("HEAD has no OID"))?;
                            let obj = repo.odb.read(&commit_oid)?;
                            let commit = parse_commit(&obj.data)?;
                            let root_rebase = rb_dir.join("root").exists();
                            let msg_for_log =
                                message_for_root_replayed_commit(repo, &commit, root_rebase);
                            let subject = msg_for_log.lines().next().unwrap_or("");
                            if print_am_style_progress && !rebase_quiet(rb_dir) {
                                println!("Applying: {}", subject);
                                flush_rebase_stdout();
                            }
                            let msg = format!("{ra} (pick): {subject}");
                            let _ = append_reflog(
                                git_dir, "HEAD", &old_head, &new_oid, &ident, &msg, false,
                            );

                            if let Ok(global_exec_raw) = fs::read_to_string(rb_dir.join("exec")) {
                                // Each `--exec` is stored on its own line; run them all, in order,
                                // after this pick (t3404 "rebase -ix with several instances of
                                // --exec").
                                let exec_cmds: Vec<String> = global_exec_raw
                                    .lines()
                                    .map(str::trim)
                                    .filter(|l| !l.is_empty())
                                    .map(ToOwned::to_owned)
                                    .collect();
                                for (exec_idx, global_exec) in exec_cmds.iter().enumerate() {
                                    if !rebase_quiet(rb_dir) {
                                        eprintln!("Executing: {}", global_exec);
                                    }
                                    let status = std::process::Command::new("sh")
                                        .arg("-c")
                                        .arg(global_exec)
                                        .current_dir(
                                            repo.work_tree
                                                .as_deref()
                                                .unwrap_or_else(|| Path::new(".")),
                                        )
                                        .status()
                                        .with_context(|| {
                                            format!("failed to execute: {}", global_exec)
                                        })?;
                                    if !status.success() {
                                        let code =
                                            status.code().filter(|code| *code != 127).unwrap_or(1);
                                        let reschedule = rb_dir
                                            .join("reschedule-failed-exec")
                                            .exists()
                                            && !rb_dir.join("no-reschedule-failed-exec").exists();
                                        eprintln!(
                                            "warning: execution failed for: {}\n\
                                         hint: You can fix the problem, and then run\n\
                                         hint:   grit rebase --continue",
                                            global_exec
                                        );
                                        // The failed exec (and any later ones for this pick) plus the
                                        // remaining todo are re-scheduled so `--continue` resumes them.
                                        let mut remaining: Vec<String> = exec_cmds[exec_idx..]
                                            .iter()
                                            .map(|c| format!("exec {c}"))
                                            .collect();
                                        if reschedule {
                                            eprintln!(
                                                "hint: '{}' has been rescheduled",
                                                global_exec
                                            );
                                        }
                                        remaining.extend(
                                            todo[i + 1..].iter().map(|line| (*line).to_owned()),
                                        );
                                        let remaining_refs: Vec<&str> =
                                            remaining.iter().map(String::as_str).collect();
                                        write_rebase_todo_slice(rb_dir, &remaining_refs)?;
                                        std::process::exit(code);
                                    }
                                }
                            }

                            let todo_after_step = read_rebase_todo_file(repo, rb_dir).ok();
                            let remaining_body = if let Some(ref edited_todo) = todo_after_step {
                                let edited_lines = rebase_todo_actionable_lines(edited_todo);
                                let remaining = if edited_lines == todo {
                                    todo[i + 1..].join("\n")
                                } else if edited_lines.first().copied() == Some(todo[i]) {
                                    edited_lines[1..].join("\n")
                                } else {
                                    let mut remaining_lines = edited_lines.clone();
                                    if let Some(pos) =
                                        remaining_lines.iter().position(|line| *line == todo[i])
                                    {
                                        remaining_lines.remove(pos);
                                    }
                                    remaining_lines.join("\n")
                                };
                                if remaining.is_empty() {
                                    String::new()
                                } else {
                                    remaining + "\n"
                                }
                            } else {
                                let remaining: Vec<&str> = todo[i + 1..].to_vec();
                                remaining.join("\n") + "\n"
                            };
                            write_rebase_todo_file(repo, rb_dir, &remaining_body)?;
                            fs::write(rb_dir.join("msgnum"), "1")?;
                            let rem_n = count_rebase_todo_actionable_lines(&remaining_body);
                            fs::write(rb_dir.join("end"), rem_n.to_string())?;
                            continue 'rebase_loop;
                        }
                        Err(_e) => {
                            // An untracked-file obstruction (the commit never applied) sets the
                            // `obstructed-pick` marker in the child. Git keeps such a pick pending and
                            // re-consumes it into `done` on retry, so the obstructed line appears in
                            // `done` *twice* (t3404 "rebase -i commits that overwrite untracked
                            // files"). The line was already appended to `done` above; remember it
                            // verbatim so the obstructed-pick re-apply on `--continue` can append it a
                            // second time. A genuine merge conflict (no marker) keeps the single record.
                            if rebase_interactive && rb_dir.join("obstructed-pick").exists() {
                                let _ = fs::write(
                                    rb_dir.join("obstructed-pick-line"),
                                    format!("{}\n", todo[i]),
                                );
                            }
                            let remaining: Vec<&str> = if rebase_interactive {
                                todo[i + 1..].to_vec()
                            } else {
                                todo[i..].to_vec()
                            };
                            write_rebase_todo_slice(rb_dir, &remaining)?;
                            write_non_interactive_done_on_stop(
                                rb_dir,
                                &todo,
                                i,
                                rebase_interactive,
                            );
                            let ff =
                                is_final_fixup_in_todo(repo, rb_dir, &todo, i, rebase_interactive);
                            fs::write(
                                rb_dir.join("current-final-fixup"),
                                if ff { "1\n" } else { "0\n" },
                            )?;
                            let _ = fs::write(
                                rb_dir.join("stopped-sha"),
                                format!("{}\n", commit_oid.to_hex()),
                            );
                            // Git always exposes the commit being replayed as REBASE_HEAD when a pick
                            // stops (conflict *or* untracked-file obstruction). The conflict path sets
                            // it inside `cherry_pick_for_rebase`; ensure it is set for the obstruction
                            // path too (t3404 "rebase -i commits that overwrite untracked files").
                            let _ = fs::write(
                                git_dir.join("REBASE_HEAD"),
                                format!("{}\n", commit_oid.to_hex()),
                            );

                            let obj = repo.odb.read(&commit_oid)?;
                            let commit = parse_commit(&obj.data)?;
                            let root_rebase = rb_dir.join("root").exists();
                            let msg_for_log =
                                message_for_root_replayed_commit(repo, &commit, root_rebase);
                            let subject = msg_for_log.lines().next().unwrap_or("");

                            eprintln!(
                                "error: could not apply {}... {}\n\
                             hint: Resolve all conflicts manually, mark them as resolved with\n\
                             hint: \"grit add <pathspec>\", then run \"grit rebase --continue\".\n\
                             hint: To skip this commit, run \"grit rebase --skip\".\n\
                             hint: To abort, run \"grit rebase --abort\".",
                                &commit_oid.to_hex()[..7],
                                subject
                            );
                            std::process::exit(1);
                        }
                    }
                }
            }
        }
        break;
    }

    // Rebase complete — restore branch ref
    finish_rebase(repo, rb_dir, autostash_oid, backend, had_rebase_autostash)?;
    Ok(())
}
fn rebase_keep_empty(rb_dir: &Path) -> bool {
    rb_dir.join("keep-empty").exists()
}

fn rebase_keep_start_empty(rb_dir: &Path) -> bool {
    !rb_dir.join("no-keep-empty").exists()
}

/// How the rebase should treat a commit that *becomes* empty during replay (its change is already
/// present, e.g. cherry-equivalent upstream). Mirrors git's `enum empty_type` and the
/// `drop_redundant_commits` / `keep_redundant_commits` sequencer options.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BecomeEmptyMode {
    /// Drop the redundant commit (git `--empty=drop`, default for plain `--merge`).
    Drop,
    /// Keep it as an empty commit (git `--empty=keep`).
    Keep,
    /// Halt so the user can decide (git `--empty=stop`/`ask`).
    Stop,
}

/// Resolve the effective `--empty` mode, applying git's default rules
/// (`builtin/rebase.c`): explicit `--interactive` -> stop, `--exec` -> keep, otherwise drop.
fn resolve_become_empty_mode(args: &Args) -> BecomeEmptyMode {
    if let Some(ref e) = args.empty {
        match e.to_ascii_lowercase().as_str() {
            "drop" => return BecomeEmptyMode::Drop,
            "keep" => return BecomeEmptyMode::Keep,
            // "stop" and "ask" both halt.
            _ => return BecomeEmptyMode::Stop,
        }
    }
    if args.interactive {
        BecomeEmptyMode::Stop
    } else if !args.exec.is_empty() {
        BecomeEmptyMode::Keep
    } else {
        BecomeEmptyMode::Drop
    }
}

/// Read the persisted become-empty policy from rebase state files. Git stores
/// `drop_redundant_commits` / `keep_redundant_commits`; neither present means "stop".
fn rebase_become_empty_mode(rb_dir: &Path) -> BecomeEmptyMode {
    if rb_dir.join("keep_redundant_commits").exists() {
        BecomeEmptyMode::Keep
    } else if rb_dir.join("drop_redundant_commits").exists() {
        BecomeEmptyMode::Drop
    } else {
        BecomeEmptyMode::Stop
    }
}

fn rebase_signoff(rb_dir: &Path) -> bool {
    rb_dir.join("signoff").exists()
}

fn rebase_trailer_args(rb_dir: &Path) -> Vec<String> {
    let Ok(bytes) = fs::read(rb_dir.join("trailers")) else {
        return Vec::new();
    };
    if bytes.is_empty() {
        return Vec::new();
    }
    bytes
        .split(|b| *b == 0)
        .map(|part| String::from_utf8_lossy(part).into_owned())
        .collect()
}

fn write_rebase_trailer_args(rb_dir: &Path, trailers: &[String]) -> Result<()> {
    if trailers.is_empty() {
        return Ok(());
    }
    let mut bytes = Vec::new();
    for (idx, trailer) in trailers.iter().enumerate() {
        if idx > 0 {
            bytes.push(0);
        }
        bytes.extend_from_slice(trailer.as_bytes());
    }
    fs::write(rb_dir.join("trailers"), bytes)?;
    Ok(())
}

fn rebase_has_explicit_trailers(rb_dir: &Path) -> bool {
    rb_dir.join("trailers").exists()
}

fn apply_rebase_explicit_trailers_to_message(
    message: String,
    rb_dir: &Path,
    config: &ConfigSet,
) -> String {
    let trailer_args = rebase_trailer_args(rb_dir);
    if trailer_args.is_empty() {
        return message;
    }
    let specs: Vec<NewTrailerArg> = trailer_args
        .into_iter()
        .map(|text| NewTrailerArg {
            text,
            where_: Default::default(),
            if_exists: Default::default(),
            if_missing: Default::default(),
        })
        .collect();
    let opts = ProcessTrailerOptions {
        no_divider: true,
        ..Default::default()
    };
    let message = apply_commit_msg_cleanup(&message, rebase_commit_msg_cleanup(config));
    let input = complete_line(&message);
    let git_dir = rb_dir.parent().unwrap_or(rb_dir);
    process_trailers(&input, &opts, &specs, Some(git_dir))
}

fn maybe_apply_rebase_message_trailers_to_commit_data(
    commit_data: &mut CommitData,
    rb_dir: &Path,
    config: &ConfigSet,
    todo_cmd: RebaseTodoCmd,
) -> Result<()> {
    let mut message = commit_data.message.clone();
    let mut changed = false;

    if rebase_signoff(rb_dir) && matches!(todo_cmd, RebaseTodoCmd::Pick | RebaseTodoCmd::Reword) {
        let (name, email, _) = split_stored_author_line(&commit_data.committer)?;
        let sob = format_signoff_line(&name, &email);
        append_signoff_trailer(&mut message, &sob, config);
        changed = true;
    }

    if rebase_has_explicit_trailers(rb_dir) {
        message = apply_rebase_explicit_trailers_to_message(message, rb_dir, config);
        changed = true;
    }

    if !changed {
        return Ok(());
    }

    let (message, encoding, raw_message) = finalize_message_for_commit_encoding(message, config);
    commit_data.message = message;
    commit_data.encoding = encoding;
    commit_data.raw_message = raw_message;
    let (author_raw, committer_raw) = grit_lib::commit_encoding::identity_raw_for_serialized_commit(
        &commit_data.encoding,
        &commit_data.author,
        &commit_data.committer,
    );
    commit_data.author_raw = author_raw;
    commit_data.committer_raw = committer_raw;
    Ok(())
}

fn rebase_quiet(rb_dir: &Path) -> bool {
    rb_dir.join("quiet").exists()
}

fn rebase_orig_head_oid(rb_dir: &Path) -> Option<ObjectId> {
    let s = fs::read_to_string(rb_dir.join("orig-head")).ok()?;
    ObjectId::from_hex(s.trim()).ok()
}

fn rebase_initial_todo_count(rb_dir: &Path) -> Option<usize> {
    let s = fs::read_to_string(rb_dir.join("end")).ok()?;
    s.trim().parse().ok()
}

fn rebase_upstream_oid(rb_dir: &Path) -> Option<ObjectId> {
    let s = fs::read_to_string(rb_dir.join("upstream")).ok()?;
    ObjectId::from_hex(s.trim()).ok()
}

fn rebase_upstream_tip_oid(rb_dir: &Path) -> Option<ObjectId> {
    let s = fs::read_to_string(rb_dir.join("upstream-tip")).ok()?;
    ObjectId::from_hex(s.trim()).ok()
}

fn rebase_onto_oid_from_state(rb_dir: &Path) -> Option<ObjectId> {
    let s = fs::read_to_string(rb_dir.join("onto")).ok()?;
    ObjectId::from_hex(s.trim()).ok()
}

fn commit_tree_oid_for_rebase(repo: &Repository, oid: ObjectId) -> Result<ObjectId> {
    let obj = repo.odb.read(&oid)?;
    let commit = parse_commit(&obj.data)?;
    Ok(commit.tree)
}

fn write_rebase_patch_file(repo: &Repository, rb_dir: &Path, commit_oid: &ObjectId) -> Result<()> {
    let commit_obj = repo.odb.read(commit_oid)?;
    let commit = parse_commit(&commit_obj.data)?;
    let Some(parent_oid) = commit.parents.first().copied() else {
        fs::write(rb_dir.join("patch"), "")?;
        return Ok(());
    };
    let parent_obj = repo.odb.read(&parent_oid)?;
    let parent = parse_commit(&parent_obj.data)?;
    let old_entries = tree_to_map(tree_to_index_entries(repo, &parent.tree, "")?);
    let new_entries = tree_to_map(tree_to_index_entries(repo, &commit.tree, "")?);
    let mut paths = BTreeSet::new();
    paths.extend(old_entries.keys().cloned());
    paths.extend(new_entries.keys().cloned());

    let mut out = String::new();
    for path in paths {
        let old = old_entries.get(&path);
        let new = new_entries.get(&path);
        if old
            .zip(new)
            .is_some_and(|(old, new)| old.oid == new.oid && old.mode == new.mode)
        {
            continue;
        }
        let path_str = String::from_utf8_lossy(&path);
        let old_oid = old.map(|entry| entry.oid).unwrap_or_else(diff::zero_oid);
        let new_oid = new.map(|entry| entry.oid).unwrap_or_else(diff::zero_oid);
        let old_mode = old.map(|entry| entry.mode).unwrap_or(0);
        let new_mode = new.map(|entry| entry.mode).unwrap_or(0);
        let old_bytes = old
            .and_then(|entry| repo.odb.read(&entry.oid).ok())
            .map(|obj| obj.data)
            .unwrap_or_default();
        let new_bytes = new
            .and_then(|entry| repo.odb.read(&entry.oid).ok())
            .map(|obj| obj.data)
            .unwrap_or_default();
        let old_text = String::from_utf8_lossy(&old_bytes);
        let new_text = String::from_utf8_lossy(&new_bytes);
        out.push_str(&format!("diff --git a/{path_str} b/{path_str}\n"));
        if old.is_none() {
            out.push_str(&format!("new file mode {new_mode:06o}\n"));
        } else if new.is_none() {
            out.push_str(&format!("deleted file mode {old_mode:06o}\n"));
        } else if old_mode != new_mode {
            out.push_str(&format!(
                "old mode {old_mode:06o}\nnew mode {new_mode:06o}\n"
            ));
        }
        let old_abbrev = abbreviate_object_id(repo, old_oid, 7)?;
        let new_abbrev = abbreviate_object_id(repo, new_oid, 7)?;
        out.push_str(&format!("index {old_abbrev}..{new_abbrev}"));
        if old.is_some() && new.is_some() && old_mode == new_mode {
            out.push_str(&format!(" {new_mode:06o}"));
        }
        out.push('\n');
        let old_label = if old.is_some() {
            format!("a/{path_str}")
        } else {
            "/dev/null".to_string()
        };
        let new_label = if new.is_some() {
            format!("b/{path_str}")
        } else {
            "/dev/null".to_string()
        };
        out.push_str(&diff::unified_diff_with_prefix(
            old_text.as_ref(),
            new_text.as_ref(),
            &old_label,
            &new_label,
            3,
            0,
            "",
            "",
            true,
            false,
        ));
    }
    fs::write(rb_dir.join("patch"), out)?;
    Ok(())
}

/// Cherry-pick a single commit onto current HEAD for rebase purposes.
///
/// `rb_dir` is the active state directory (`rebase-apply` or `rebase-merge`), not `rebase_dir()`
/// (which wrongly prefers `rebase-merge` whenever that path exists).
fn cherry_pick_for_rebase(
    repo: &Repository,
    rb_dir: &Path,
    commit_oid: &ObjectId,
    backend: RebaseBackend,
    todo_cmd: RebaseTodoCmd,
    final_fixup: bool,
    next_after_line: Option<RebaseTodoCmd>,
    record_rewrite: bool,
    force_rewrite_commits: bool,
    fixup_message_mode: Option<FixupMessageMode>,
) -> Result<()> {
    let git_dir = &repo.git_dir;
    let keep_empty = rebase_keep_empty(rb_dir);
    let become_empty_mode = rebase_become_empty_mode(rb_dir);
    let keep_start_empty = rebase_keep_start_empty(rb_dir);
    let replay_opts = load_rebase_replay_commit_opts(rb_dir);
    let now = time::OffsetDateTime::now_utc();

    let commit_obj = repo.odb.read(commit_oid)?;
    let commit = parse_commit(&commit_obj.data)?;
    let config = ConfigSet::load(Some(git_dir), true)?;

    let head = resolve_head(git_dir)?;
    let head_oid = head
        .oid()
        .ok_or_else(|| anyhow::anyhow!("HEAD is unborn during rebase"))?
        .to_owned();
    const GIT_EMPTY_TREE_HEX: &str = "4b825dc642cb6eb9a060e54bf8d69288fbee4904";
    let empty_tree_oid = ObjectId::from_hex(GIT_EMPTY_TREE_HEX)
        .map_err(|e| anyhow::anyhow!("invalid empty tree oid: {e}"))?;
    let head_at_empty_tree = head_oid.is_zero();
    // After `reset [new root]`, HEAD points at the squash-onto sentinel (a fake empty-tree root).
    // A pick on top of it must become a *root* commit (no parent), not carry the sentinel as a
    // parent — otherwise the graph gains a spurious commit below it (t3430 'root commits').
    let head_is_squash_onto = !head_at_empty_tree
        && fs::read_to_string(rb_dir.join("squash-onto"))
            .ok()
            .and_then(|s| ObjectId::from_hex(s.trim()).ok())
            == Some(head_oid);
    let pick_is_root = head_at_empty_tree || head_is_squash_onto;

    if keep_start_empty
        && todo_cmd == RebaseTodoCmd::Pick
        && is_commit_tree_unchanged(repo, commit_oid)?
    {
        let (tree_oid, parents) = if head_at_empty_tree {
            (empty_tree_oid, Vec::new())
        } else {
            let head_obj = repo.odb.read(&head_oid)?;
            let head_commit = parse_commit(&head_obj.data)?;
            (head_commit.tree, vec![head_oid])
        };
        let (message, encoding, raw_message) = transcoded_replayed_message(&commit, &config);
        let author = rebase_replayed_author_line(&commit.author, replay_opts, now)?;
        let committer = rebase_replayed_committer_line(&config, &commit.author, replay_opts, now)?;
        let (author_raw, committer_raw) =
            grit_lib::commit_encoding::identity_raw_for_serialized_commit(
                &encoding, &author, &committer,
            );
        let mut commit_data = CommitData {
            tree: tree_oid,
            parents,
            author,
            committer,
            author_raw,
            committer_raw,
            encoding,
            message,
            raw_message,
        };
        maybe_apply_rebase_message_trailers_to_commit_data(
            &mut commit_data,
            rb_dir,
            &config,
            todo_cmd,
        )?;
        let bytes = serialize_commit(&commit_data);
        let new_oid = write_replayed_commit(repo, rb_dir, &config, &commit_data.committer, bytes)?;
        fs::write(git_dir.join("HEAD"), format!("{}\n", new_oid.to_hex()))?;
        if record_rewrite {
            record_rebase_in_rewritten_pending(git_dir, rb_dir, commit_oid, next_after_line)?;
        }
        return Ok(());
    }

    // Parent tree (base for the cherry-pick). Root commits use Git's empty tree as base.
    let parent_tree_oid = if let Some(parent_oid) = commit.parents.first() {
        let parent_obj = repo.odb.read(parent_oid)?;
        let parent_commit = parse_commit(&parent_obj.data)?;
        parent_commit.tree
    } else {
        ObjectId::from_hex(GIT_EMPTY_TREE_HEX)
            .map_err(|e| anyhow::anyhow!("invalid empty tree oid: {e}"))?
    };

    // Commit's tree (theirs — the changes we want)
    let commit_tree_oid = commit.tree;

    // HEAD tree (ours — the current state). `rebase --root` without `--onto` leaves HEAD at the
    // null OID until the first replayed commit exists (Git-compatible).
    let head_tree_oid = if head_at_empty_tree {
        empty_tree_oid
    } else {
        let head_obj = repo.odb.read(&head_oid)?;
        let head_commit = parse_commit(&head_obj.data)?;
        head_commit.tree
    };
    let root_rebase = rb_dir.join("root").exists();
    let ws_fix_rule = load_ws_fix_rule_from_rebase_state(git_dir);

    // Fast-forward a root-commit pick when HEAD is unborn (`rebase --root` without `--onto`) OR
    // sitting on the `[new root]` squash-onto sentinel (`reset [new root]` then `pick <root>` in a
    // `--rebase-merges` script), with fast-forward allowed. Git's sequencer fast-forwards picks where
    // `!parent && unborn` (sequencer.c `do_pick_commit`), so an unchanged `rebase --root -r` reuses
    // the original root commits instead of rewriting them with fresh committer dates — making the
    // second `git rebase -i --root -r` a whole-graph no-op (t3421 "rebase --root … is a no-op",
    // t3430 'root commits'). Forced rebases (`-f`/`--no-ff`/signoff/trailers) and whitespace fixups
    // must still rewrite the root.
    if todo_cmd == RebaseTodoCmd::Pick
        && commit.parents.is_empty()
        && pick_is_root
        && !force_rewrite_commits
        && ws_fix_rule.is_none()
        && !rebase_signoff(rb_dir)
        && !rebase_has_explicit_trailers(rb_dir)
    {
        let old_index = load_index(repo)?;
        let mut idx = Index::new();
        idx.entries = tree_to_index_entries(repo, &commit_tree_oid, "")?;
        idx.sort();
        if let Some(wt) = &repo.work_tree {
            preflight_cherry_pick_cwd_obstruction(repo, wt, &idx, &BTreeMap::new(), None)?;
        }
        repo.write_index(&mut idx)?;
        if let Some(wt) = &repo.work_tree {
            checkout_merged_index(repo, wt, &old_index, &idx, true)?;
            refresh_index_stat_cache_from_worktree(repo, &mut idx)?;
            repo.write_index(&mut idx)?;
        }
        fs::write(git_dir.join("HEAD"), format!("{}\n", commit_oid.to_hex()))?;
        if record_rewrite {
            record_rebase_in_rewritten_pending(git_dir, rb_dir, commit_oid, next_after_line)?;
        }
        return Ok(());
    }

    // Already at the picked commit's parent tip — nothing to replay (matches Git's noop pick).
    // Fixup/squash must still run merge + message folding even when parent == HEAD.
    // Reword still needs the commit editor even when the tree is already applied.
    if todo_cmd == RebaseTodoCmd::Pick {
        if let Some(p) = commit.parents.first() {
            if head_oid == *p {
                let old_index = load_index(repo)?;
                let mut idx = Index::new();
                idx.entries = tree_to_index_entries(repo, &commit_tree_oid, "")?;
                idx.sort();
                if let Some(rule) = ws_fix_rule {
                    apply_ws_fix_to_index(repo, &mut idx, rule)?;
                }
                if let Some(wt) = &repo.work_tree {
                    // A newly added path that would overwrite an untracked working tree file must abort
                    // even on this fast-forward-pick shortcut (git's unpack-trees `verify_absent`).
                    // The cwd preflight below does not cover plain untracked-file collisions
                    // (t3404 "rebase -i commits that overwrite untracked files").
                    if let Err(e) =
                        super::reset::check_untracked_cherry_pick_obstruction(wt, &old_index, &idx)
                    {
                        let _ = fs::write(
                            rb_dir.join("obstructed-pick"),
                            format!("{}\n", commit_oid.to_hex()),
                        );
                        return Err(e);
                    }
                    preflight_cherry_pick_cwd_obstruction(repo, wt, &idx, &BTreeMap::new(), None)?;
                }
                repo.write_index(&mut idx)?;
                if let Some(wt) = &repo.work_tree {
                    checkout_merged_index(repo, wt, &old_index, &idx, true)?;
                    refresh_index_stat_cache_from_worktree(repo, &mut idx)?;
                    repo.write_index(&mut idx)?;
                }
                let upstream_matches_onto = match (
                    rebase_upstream_oid(rb_dir),
                    rebase_onto_oid_from_state(rb_dir),
                ) {
                    (Some(u), Some(o)) => u == o,
                    _ => false,
                };
                let rebase_range_starts_at_upstream_tip =
                    match (rebase_upstream_oid(rb_dir), rebase_upstream_tip_oid(rb_dir)) {
                        (Some(u), Some(tip)) => u == tip,
                        _ => true,
                    };
                let single_noop_same_tip = force_rewrite_commits
                    && !rebase_signoff(rb_dir)
                    && !rebase_has_explicit_trailers(rb_dir)
                    && ws_fix_rule.is_none()
                    && upstream_matches_onto
                    && rebase_range_starts_at_upstream_tip
                    && rebase_initial_todo_count(rb_dir) == Some(1)
                    && rebase_orig_head_oid(rb_dir).as_ref() == Some(commit_oid);

                if ws_fix_rule.is_some() {
                    let tree_oid = write_tree_from_index(&repo.odb, &idx, "")?;
                    let (message, encoding, raw_message) = if root_rebase {
                        let msg = message_for_root_replayed_commit(repo, &commit, true);
                        (msg, commit.encoding.clone(), None)
                    } else {
                        transcoded_replayed_message(&commit, &config)
                    };
                    let raw_msg = commit_message_after_prepare_hook(
                        repo,
                        git_dir,
                        &message,
                        "message",
                        Some(":"),
                    )?;
                    let message =
                        apply_commit_msg_cleanup(&raw_msg, rebase_commit_msg_cleanup(&config));
                    let author = rebase_replayed_author_line(&commit.author, replay_opts, now)?;
                    let committer =
                        rebase_replayed_committer_line(&config, &commit.author, replay_opts, now)?;
                    let (author_raw, committer_raw) =
                        grit_lib::commit_encoding::identity_raw_for_serialized_commit(
                            &encoding, &author, &committer,
                        );
                    let mut commit_data = CommitData {
                        tree: tree_oid,
                        parents: vec![head_oid],
                        author,
                        committer,
                        author_raw,
                        committer_raw,
                        encoding,
                        message,
                        raw_message,
                    };
                    maybe_apply_rebase_message_trailers_to_commit_data(
                        &mut commit_data,
                        rb_dir,
                        &config,
                        todo_cmd,
                    )?;
                    let commit_bytes = serialize_commit(&commit_data);
                    let new_oid = write_replayed_commit(
                        repo,
                        rb_dir,
                        &config,
                        &commit_data.committer,
                        commit_bytes,
                    )?;
                    fs::write(git_dir.join("HEAD"), format!("{}\n", new_oid.to_hex()))?;
                    append_rebase_rewrite_line(rb_dir, commit_oid, &new_oid)?;
                } else if replay_opts.committer_date_is_author_date || replay_opts.ignore_date {
                    let (message, encoding, raw_message) = if root_rebase {
                        let msg = message_for_root_replayed_commit(repo, &commit, true);
                        (msg, commit.encoding.clone(), None)
                    } else {
                        transcoded_replayed_message(&commit, &config)
                    };
                    let raw_msg = commit_message_after_prepare_hook(
                        repo,
                        git_dir,
                        &message,
                        "message",
                        Some(":"),
                    )?;
                    let message =
                        apply_commit_msg_cleanup(&raw_msg, rebase_commit_msg_cleanup(&config));
                    let author = rebase_replayed_author_line(&commit.author, replay_opts, now)?;
                    let committer =
                        rebase_replayed_committer_line(&config, &commit.author, replay_opts, now)?;
                    let (author_raw, committer_raw) =
                        grit_lib::commit_encoding::identity_raw_for_serialized_commit(
                            &encoding, &author, &committer,
                        );
                    let mut commit_data = CommitData {
                        tree: commit_tree_oid,
                        parents: vec![head_oid],
                        author,
                        committer,
                        author_raw,
                        committer_raw,
                        encoding,
                        message,
                        raw_message,
                    };
                    maybe_apply_rebase_message_trailers_to_commit_data(
                        &mut commit_data,
                        rb_dir,
                        &config,
                        todo_cmd,
                    )?;
                    let commit_bytes = serialize_commit(&commit_data);
                    let new_oid = write_replayed_commit(
                        repo,
                        rb_dir,
                        &config,
                        &commit_data.committer,
                        commit_bytes,
                    )?;
                    fs::write(git_dir.join("HEAD"), format!("{}\n", new_oid.to_hex()))?;
                    append_rebase_rewrite_line(rb_dir, commit_oid, &new_oid)?;
                } else if force_rewrite_commits && !single_noop_same_tip {
                    let tree_oid = commit_tree_oid;
                    let (message, encoding, raw_message) = if root_rebase {
                        let msg = message_for_root_replayed_commit(repo, &commit, true);
                        (msg, commit.encoding.clone(), None)
                    } else {
                        transcoded_replayed_message(&commit, &config)
                    };
                    let raw_msg = commit_message_after_prepare_hook(
                        repo,
                        git_dir,
                        &message,
                        "message",
                        Some(":"),
                    )?;
                    let message =
                        apply_commit_msg_cleanup(&raw_msg, rebase_commit_msg_cleanup(&config));
                    let author = rebase_replayed_author_line(&commit.author, replay_opts, now)?;
                    let committer =
                        rebase_replayed_committer_line(&config, &commit.author, replay_opts, now)?;
                    let (author_raw, committer_raw) =
                        grit_lib::commit_encoding::identity_raw_for_serialized_commit(
                            &encoding, &author, &committer,
                        );
                    let mut commit_data = CommitData {
                        tree: tree_oid,
                        parents: vec![head_oid],
                        author,
                        committer,
                        author_raw,
                        committer_raw,
                        encoding,
                        message,
                        raw_message,
                    };
                    maybe_apply_rebase_message_trailers_to_commit_data(
                        &mut commit_data,
                        rb_dir,
                        &config,
                        todo_cmd,
                    )?;
                    let commit_bytes = serialize_commit(&commit_data);
                    let new_oid = write_replayed_commit(
                        repo,
                        rb_dir,
                        &config,
                        &commit_data.committer,
                        commit_bytes,
                    )?;
                    fs::write(git_dir.join("HEAD"), format!("{}\n", new_oid.to_hex()))?;
                    if record_rewrite {
                        record_rebase_in_rewritten_pending(
                            git_dir,
                            rb_dir,
                            commit_oid,
                            next_after_line,
                        )?;
                    } else {
                        append_rebase_rewrite_line(rb_dir, commit_oid, &new_oid)?;
                    }
                } else {
                    // True fast-forward: HEAD already matches the picked commit's parent and we
                    // keep the original commit OID. Git still records this in the rewritten list
                    // (sequencer.c `record_in_rewritten` after every successful pick, including the
                    // fast-forward at `allow_ff && !is_fixup`). Recording is essential when a
                    // following fixup/squash will amend this commit: the pending entry then flushes
                    // to the amended OID, so both the picked commit and the fixup map to the final
                    // commit in the post-rewrite hook input (t5407).
                    fs::write(git_dir.join("HEAD"), format!("{}\n", commit_oid.to_hex()))?;
                    if record_rewrite {
                        record_rebase_in_rewritten_pending(
                            git_dir,
                            rb_dir,
                            commit_oid,
                            next_after_line,
                        )?;
                    }
                }
                return Ok(());
            }
        }
    }
    if todo_cmd == RebaseTodoCmd::Reword {
        if let Some(p) = commit.parents.first() {
            if head_oid == *p {
                let old_index = load_index(repo)?;
                let mut idx = Index::new();
                idx.entries = tree_to_index_entries(repo, &commit_tree_oid, "")?;
                idx.sort();
                if let Some(wt) = &repo.work_tree {
                    preflight_cherry_pick_cwd_obstruction(repo, wt, &idx, &BTreeMap::new(), None)?;
                }
                repo.write_index(&mut idx)?;
                if let Some(wt) = &repo.work_tree {
                    checkout_merged_index(repo, wt, &old_index, &idx, true)?;
                    refresh_index_stat_cache_from_worktree(repo, &mut idx)?;
                    repo.write_index(&mut idx)?;
                }
                let (template, _enc, _raw) = if root_rebase {
                    let msg = message_for_root_replayed_commit(repo, &commit, true);
                    (msg, commit.encoding.clone(), None)
                } else {
                    transcoded_replayed_message(&commit, &config)
                };
                let raw_pick = commit_message_after_prepare_hook(
                    repo,
                    git_dir,
                    &template,
                    "message",
                    Some(":"),
                )?;
                let pick_msg =
                    apply_commit_msg_cleanup(&raw_pick, rebase_commit_msg_cleanup(&config));
                let (pick_message, enc_pick, raw_pick) =
                    finalize_message_for_commit_encoding(pick_msg, &config);
                let author = rebase_replayed_author_line(&commit.author, replay_opts, now)?;
                let committer =
                    rebase_replayed_committer_line(&config, &commit.author, replay_opts, now)?;
                let (author_raw, committer_raw) =
                    grit_lib::commit_encoding::identity_raw_for_serialized_commit(
                        &enc_pick, &author, &committer,
                    );
                let mut pick_data = CommitData {
                    tree: commit_tree_oid,
                    parents: vec![head_oid],
                    author: author.clone(),
                    committer: committer.clone(),
                    author_raw,
                    committer_raw,
                    encoding: enc_pick,
                    message: pick_message.clone(),
                    raw_message: raw_pick,
                };
                maybe_apply_rebase_message_trailers_to_commit_data(
                    &mut pick_data,
                    rb_dir,
                    &config,
                    RebaseTodoCmd::Pick,
                )?;
                let pick_bytes = serialize_commit(&pick_data);
                let pick_oid =
                    write_replayed_commit(repo, rb_dir, &config, &pick_data.committer, pick_bytes)?;
                fs::write(git_dir.join("HEAD"), format!("{}\n", pick_oid.to_hex()))?;
                let ra = load_rebase_reflog_action(rb_dir);
                let ident = reflog_identity(repo);
                let pick_subject = pick_data.message.lines().next().unwrap_or("");
                let pick_reflog_msg = format!("{ra} (pick): {pick_subject}");
                let _ = append_reflog(
                    git_dir,
                    "HEAD",
                    &head_oid,
                    &pick_oid,
                    &ident,
                    &pick_reflog_msg,
                    false,
                );

                let after_editor = run_commit_editor_for_reword(repo, git_dir, &pick_message)?;
                let cleaned = message_from_reword_editor(
                    &after_editor,
                    rebase_commit_msg_cleanup(&config),
                    &config,
                )?;
                let cleaned = run_reword_commit_msg_hook(repo, git_dir, cleaned, &config)?;
                let (message, encoding, raw_message) =
                    finalize_message_for_commit_encoding(cleaned, &config);
                let author = rebase_replayed_author_line(&commit.author, replay_opts, now)?;
                let committer =
                    rebase_replayed_committer_line(&config, &commit.author, replay_opts, now)?;
                let (author_raw, committer_raw) =
                    grit_lib::commit_encoding::identity_raw_for_serialized_commit(
                        &encoding, &author, &committer,
                    );
                let mut commit_data = CommitData {
                    tree: commit_tree_oid,
                    parents: vec![head_oid],
                    author,
                    committer,
                    author_raw,
                    committer_raw,
                    encoding,
                    message,
                    raw_message,
                };
                maybe_apply_rebase_message_trailers_to_commit_data(
                    &mut commit_data,
                    rb_dir,
                    &config,
                    todo_cmd,
                )?;
                let commit_bytes = serialize_commit(&commit_data);
                let new_oid = write_replayed_commit(
                    repo,
                    rb_dir,
                    &config,
                    &commit_data.committer,
                    commit_bytes,
                )?;
                fs::write(git_dir.join("HEAD"), format!("{}\n", new_oid.to_hex()))?;
                append_rebase_rewrite_line(rb_dir, commit_oid, &new_oid)?;
                return Ok(());
            }
        }
    }

    if matches!(todo_cmd, RebaseTodoCmd::Fixup | RebaseTodoCmd::Squash) {
        let mut ctx = read_squash_ctx(rb_dir);
        update_squash_message_file(
            repo,
            rb_dir,
            git_dir,
            todo_cmd,
            fixup_message_mode,
            &commit,
            &mut ctx,
        )?;
    }

    // Three-way merge: base=parent_tree, ours=HEAD_tree, theirs=commit_tree
    //
    // Standard cherry-pick semantics (Git's sequencer `do_recursive_merge`): the
    // merge base is always the *picked commit's parent tree*. ours = the current
    // HEAD tree (the series replayed so far), theirs = the picked commit's tree.
    // When the replayed predecessor matches the original (the common case) this
    // makes base == ours so the picked diff applies cleanly; when a force-rebase
    // replays a chain of dependent commits (e.g. `rebase -f HEAD^^`) it keeps the
    // base correct so sequential edits to the same file do not spuriously
    // conflict.
    let base_tree_oid = if ws_fix_rule.is_some() {
        // After an earlier replay, HEAD can differ from the picked commit's parent tree in the ODB
        // (e.g. `rebase --whitespace=fix`). Use the current tip tree as the merge base so the
        // merge sees ours==base and applies the commit's tree as the new result.
        head_tree_oid
    } else {
        let onto_hex = fs::read_to_string(rb_dir.join("onto"))?;
        let onto_oid_state = ObjectId::from_hex(onto_hex.trim())?;
        // When the replayed predecessor's tree exactly reproduces the picked
        // commit's parent tree, the parent tree is the correct cherry-pick base
        // (base==ours, so the picked diff applies cleanly). This covers:
        //   * a clean dependent-chain replay under force-rewrite (`rebase -f
        //     HEAD^^`), where the already-replayed predecessor reproduced its
        //     original tree, so sequential edits to the same file do not
        //     spuriously conflict; and
        //   * a `--continue` that resumes after a resolved conflict whose
        //     resolution matches the original parent content — the next commit
        //     must NOT be merged against the onto tree (which would conflict
        //     spuriously on lines the resolved commit already settled).
        // Matches Git's sequencer, which always uses the picked commit's parent
        // tree as the base. The first pick (HEAD still at onto) likewise uses
        // the picked commit's parent tree.
        let _ = onto_oid_state;
        parent_tree_oid
    };
    let base_entries = tree_to_map(tree_to_index_entries(repo, &base_tree_oid, "")?);
    let ours_entries = tree_to_map(tree_to_index_entries(repo, &head_tree_oid, "")?);
    let theirs_entries = tree_to_map(tree_to_index_entries(repo, &commit_tree_oid, "")?);
    for entry in base_entries.values() {
        if repo.odb.read(&entry.oid).is_err() {
            let _ =
                crate::commands::promisor_hydrate::try_lazy_fetch_promisor_object(repo, entry.oid);
        }
    }
    if let Some(wt) = repo.work_tree.as_deref() {
        bail_if_df_merge_would_remove_cwd(wt, &base_entries, &ours_entries, &theirs_entries)?;
    }
    maybe_run_rebase_merge_strategy(repo, git_dir, rb_dir, *commit_oid)?;
    let picked_short_oid = commit_oid.to_hex()[..7].to_string();
    let conflict_ctx = RebaseConflictContext {
        backend,
        picked_short_oid: &picked_short_oid,
        picked_subject: commit.message.lines().next().unwrap_or("replayed commit"),
        ignore_space_change: replay_opts.ignore_space_change,
    };
    let merge_favor = load_rebase_merge_favor(rb_dir);
    // Conflict-marker base label for the ancestor block (diff3/zdiff3 style). For the merge
    // backend this is "parent of <abbrev> (<subject>)"; for the apply backend it is
    // "constructed fake ancestor" — matching Git's sequencer / git-am behavior (t6427).
    let conflict_base_label = conflict_ctx.label_base();

    // With `--ignore-whitespace`/`--ignore-space-change`, route the content merge through
    // `three_way_merge_with_content`, which threads `ignore_space_change` into the per-file
    // 3-way merge (Git's apply backend runs `git am --ignore-whitespace`; the merge backend passes
    // `--ignore-space-change` to the merge strategy). The tree-merge cherry-pick path does not honor
    // this flag, so a whitespace-only divergence would spuriously conflict (t3436).
    let want_ignore_ws_merge = replay_opts.ignore_space_change;
    let (mut merged_index, mut merge_conflict_files) =
        if ws_fix_rule.is_none() && !want_ignore_ws_merge {
            if commit.parents.is_empty() && merge_favor == MergeFavor::Theirs {
                let mut idx = Index::new();
                idx.entries = theirs_entries.values().cloned().collect();
                idx.sort();
                (idx, Vec::new())
            } else if commit.parents.is_empty() {
                let merge_result = three_way_merge_with_content(
                    repo,
                    &base_entries,
                    &ours_entries,
                    &theirs_entries,
                    &conflict_ctx,
                    merge_favor,
                )?;
                (merge_result.index, merge_result.conflict_files)
            } else {
                let tree_merge = merge_trees_for_single_cherry_pick(
                    repo,
                    base_tree_oid,
                    head_tree_oid,
                    commit_tree_oid,
                    commit_oid,
                    commit.parents.first().ok_or_else(|| {
                        anyhow::anyhow!("cherry-pick of root commit not supported")
                    })?,
                    &head_oid,
                    merge_favor,
                    Some(conflict_base_label.as_str()),
                )?;
                if tree_merge.has_conflicts && merge_favor == MergeFavor::Theirs {
                    let mut idx = Index::new();
                    idx.entries = theirs_entries.values().cloned().collect();
                    idx.sort();
                    (idx, Vec::new())
                } else {
                    let short = commit_oid.to_hex()[..7].to_string();
                    let subject = commit.message.lines().next().unwrap_or("");
                    let from = format!(">>>>>>> {short}\n");
                    let to = format!(">>>>>>> {short} ({subject})\n");
                    let cf = tree_merge
                        .conflict_files
                        .into_iter()
                        .map(|(p, c)| {
                            let content = String::from_utf8(c)
                                .map(|s| s.replace(&from, &to).into_bytes())
                                .unwrap_or_else(|e| e.into_bytes());
                            (p.into_bytes(), content)
                        })
                        .collect::<Vec<_>>();
                    (tree_merge.index, cf)
                }
            }
        } else {
            let merge_result = three_way_merge_with_content(
                repo,
                &base_entries,
                &ours_entries,
                &theirs_entries,
                &conflict_ctx,
                merge_favor,
            )?;
            (merge_result.index, merge_result.conflict_files)
        };

    if let Some(rule) = ws_fix_rule {
        apply_ws_fix_to_index(repo, &mut merged_index, rule)?;
    }
    if merge_conflict_files.is_empty()
        && ws_fix_rule.is_none()
        && !want_ignore_ws_merge
        && merge_favor == MergeFavor::None
    {
        merge_conflict_files.extend(overlapping_content_changes(
            repo,
            &base_entries,
            &ours_entries,
            &theirs_entries,
        )?);
    }

    let has_conflicts =
        merged_index.entries.iter().any(|e| e.stage() != 0) || !merge_conflict_files.is_empty();

    if !has_conflicts {
        let merged_tree_oid = write_tree_from_index(&repo.odb, &merged_index, "")?;
        let fixup_replacement_empty =
            todo_cmd == RebaseTodoCmd::Fixup && fixup_message_mode.is_some();
        if matches!(todo_cmd, RebaseTodoCmd::Fixup | RebaseTodoCmd::Squash)
            && !fixup_replacement_empty
            && !keep_empty
        {
            let head_obj = repo.odb.read(&head_oid)?;
            let head_commit = parse_commit(&head_obj.data)?;
            if let Some(amend_parent) = head_commit.parents.first() {
                let parent_tree = commit_tree_oid_for_rebase(repo, *amend_parent)?;
                if merged_tree_oid == parent_tree {
                    eprintln!(
                        "The previous cherry-pick is now empty, possibly due to conflict resolution."
                    );
                    eprintln!("hint: try \"git rebase --skip\"");
                    bail!("nothing to commit");
                }
            }
        }
        // A commit whose result is empty. Its handling depends on whether it *started* empty or
        // *became* empty during the replay (its change is now redundant). `--keep-empty` keeps
        // commits that started empty (handled earlier); here we decide the redundant case using the
        // resolved become-empty policy (git `allow_empty()` -> drop/keep/halt). A commit that
        // became empty under `--empty=keep` falls through and is committed as an empty commit.
        let became_empty = commit_tree_oid != parent_tree_oid;
        let keep_this_empty = if became_empty {
            become_empty_mode == BecomeEmptyMode::Keep
        } else {
            keep_empty
        };
        if merged_tree_oid == head_tree_oid && !fixup_replacement_empty && !keep_this_empty {
            if became_empty && become_empty_mode == BecomeEmptyMode::Drop {
                // Index/worktree already equal HEAD's tree (the merge produced no change), so the
                // commit is dropped simply by not creating it; the replay loop advances.
                return Ok(());
            }
            fs::write(rb_dir.join("current"), format!("{}\n", commit_oid.to_hex()))?;
            fs::write(
                rb_dir.join("current-cmd"),
                format!("{}\n", todo_cmd.as_str()),
            )?;
            fs::write(
                rb_dir.join("stopped-sha"),
                format!("{}\n", commit_oid.to_hex()),
            )?;
            fs::write(
                git_dir.join("REBASE_HEAD"),
                format!("{}\n", commit_oid.to_hex()),
            )?;
            // Git records CHERRY_PICK_HEAD for a halted `pick` (matching REBASE_HEAD) so a
            // subsequent `git commit` detects an in-progress rebase pick (t3404 118/119). Restrict
            // to plain picks: a fixup/squash stop must not leave CHERRY_PICK_HEAD, which would make
            // the fixup continuation amend with the wrong message (t3404 76 `--root fixup`).
            if matches!(todo_cmd, RebaseTodoCmd::Pick | RebaseTodoCmd::Reword) {
                fs::write(
                    git_dir.join("CHERRY_PICK_HEAD"),
                    format!("{}\n", commit_oid.to_hex()),
                )?;
            }
            let (message, _encoding, _raw_message) = transcoded_replayed_message(&commit, &config);
            fs::write(git_dir.join("COMMIT_EDITMSG"), message)?;
            eprintln!(
                "The previous cherry-pick is now empty, possibly due to conflict resolution."
            );
            eprintln!("hint: try \"git rebase --skip\"");
            bail!("nothing to commit");
        }
    }

    // Write index
    let old_index = load_index(repo)?;
    let rebase_conflict_paths: Vec<Vec<u8>> = merge_conflict_files
        .iter()
        .map(|(p, _)| p.clone())
        .collect();
    if let Some(wt) = &repo.work_tree {
        // Refuse to materialize a newly-added path over an untracked working tree file. Git's
        // sequencer applies the pick through unpack-trees, whose `verify_absent` aborts the merge
        // when an added entry would overwrite an untracked file (`t5407` `git rebase with failed
        // pick`: an `exec >G` creates an untracked `G`, then `pick G` must abort). The cwd/submodule
        // preflight below does not cover this case, so mirror the standalone cherry-pick check.
        let obstruction =
            super::reset::check_untracked_cherry_pick_obstruction(wt, &old_index, &merged_index)
                .and_then(|()| {
                    preflight_cherry_pick_cwd_obstruction(
                        repo,
                        wt,
                        &merged_index,
                        &BTreeMap::new(),
                        Some(&rebase_conflict_paths),
                    )
                });
        if let Err(e) = obstruction {
            // The step was not applied (the worktree is untouched), so a later `--continue` would
            // otherwise see worktree == HEAD and mistake it for an empty no-op. Record which commit
            // was obstructed so the continue path re-runs it once the obstruction is cleared, and
            // records the real old->new mapping for the post-rewrite hook (t5407 "git rebase with
            // failed pick"). This does NOT alter the halt state Git leaves, so resolved-conflict and
            // submodule paths are unaffected.
            let _ = fs::write(
                rb_dir.join("obstructed-pick"),
                format!("{}\n", commit_oid.to_hex()),
            );
            return Err(e);
        }
    }
    repo.write_index(&mut merged_index)?;

    // Update worktree
    if let Some(wt) = &repo.work_tree {
        if let Err(e) = checkout_merged_index(repo, wt, &old_index, &merged_index, true) {
            let mut restore = old_index.clone();
            let _ = repo.write_index(&mut restore);
            return Err(e);
        }
        if has_conflicts {
            write_rebase_conflict_files(wt, &merge_conflict_files)?;
        }
        refresh_index_stat_cache_from_worktree(repo, &mut merged_index)?;
        repo.write_index(&mut merged_index)?;
    }

    if has_conflicts {
        // A `--root` rebase whose first pick conflicts while HEAD is still unborn must leave HEAD at
        // the squash-onto sentinel (an empty-tree root commit), so `git cat-file commit HEAD` and
        // `git rebase --abort` work during the halt (t3404 "rebase -i --root temporary sentinel
        // commit"). Without this, HEAD stays unborn and those commands fail.
        if head_at_empty_tree && root_rebase {
            if let Ok(sentinel) = ensure_squash_onto_fake_root(repo, git_dir, rb_dir) {
                let _ = fs::write(git_dir.join("HEAD"), format!("{}\n", sentinel.to_hex()));
            }
        }
        let _ = grit_lib::rerere::repo_rerere(repo, load_rebase_rerere_autoupdate(rb_dir));
        let _ = write_rebase_patch_file(repo, rb_dir, commit_oid);
        let _ = fs::write(
            git_dir.join("REBASE_HEAD"),
            format!("{}\n", commit_oid.to_hex()),
        );
        write_rebase_conflict_message(git_dir, &commit, &config)?;
        if todo_cmd != RebaseTodoCmd::Reword {
            // Keep MERGE_MSG in the commit output encoding (same as `rebase-merge/message`,
            // written above by `write_rebase_conflict_message`). Using the UTF-8 `commit.message`
            // here would make `rebase --continue` decode UTF-8 bytes as the configured non-UTF-8
            // encoding and corrupt the message (t3434 eucJP -> ISO-2022-JP).
            let (unicode, _enc, raw_opt) = transcoded_replayed_message(&commit, &config);
            let mut merge_msg = raw_opt.unwrap_or_else(|| unicode.into_bytes());
            append_rebase_conflicts_comment_block_bytes(
                &mut merge_msg,
                &merge_conflict_files,
                &config,
            );
            fs::write(git_dir.join("MERGE_MSG"), merge_msg)?;
        }
        eprint_submodule_merge_conflict_advice(repo, &merged_index);
        if rb_dir == rebase_merge_dir(git_dir) {
            write_rebase_author_script_for_commit(git_dir, &commit)?;
        }
        bail!("conflicts during cherry-pick of {}", commit_oid.to_hex());
    }

    if matches!(todo_cmd, RebaseTodoCmd::Fixup | RebaseTodoCmd::Squash) {
        let head_obj = repo.odb.read(&head_oid)?;
        let hc = parse_commit(&head_obj.data)?;
        let amend_parent = hc.parents.first().copied().unwrap_or(head_oid);
        // When the commit being amended is the rebased root, the fixup/squash result must itself be a
        // root commit with no parent. The root being amended is HEAD with no parents, or HEAD whose
        // sole parent is the `--root` squash-onto sentinel (t3404 "rebase -i --root fixup root
        // commit").
        let squash_onto = fs::read_to_string(rb_dir.join("squash-onto"))
            .ok()
            .and_then(|s| ObjectId::from_hex(s.trim()).ok());
        // Preserve ALL of HEAD's parents: a `fixup`/`squash` that folds into a recreated MERGE commit
        // (HEAD has 2+ parents) must keep the merge structure, not collapse to the first parent
        // (t3430 'post-rewrite hook and fixups work for merges' — `git rebase -ir --autosquash` of a
        // `fixup! <merge>` keeps the merge's second parent so `HEAD^2` still resolves).
        let amend_parents: Vec<ObjectId> = if hc.parents.is_empty()
            || (hc.parents.len() == 1 && squash_onto == Some(amend_parent))
        {
            Vec::new()
        } else {
            hc.parents.clone()
        };

        if todo_cmd == RebaseTodoCmd::Fixup && !final_fixup {
            // `message-fixup` holds the non-comment message Git would pass for intermediate
            // fixups; run the hook and use `rebase_commit_msg_cleanup` so `prepare-commit-msg`
            // can append lines that must survive when `commit.cleanup` is unset (t3415).
            let fixup_path = rb_dir.join("message-fixup");
            let tmpl = if fixup_path.exists() {
                fs::read_to_string(&fixup_path)?
            } else {
                hc.message.clone()
            };
            let raw =
                commit_message_after_prepare_hook(repo, git_dir, &tmpl, "message", Some(":"))?;
            let cleaned = apply_commit_msg_cleanup(&raw, rebase_commit_msg_cleanup(&config));
            let new_oid = commit_from_merged_index(
                repo,
                git_dir,
                &merged_index,
                &config,
                amend_parents.clone(),
                &hc.author,
                cleaned,
                replay_opts,
                now,
            )?;
            fs::write(git_dir.join("HEAD"), format!("{}\n", new_oid.to_hex()))?;
            record_rebase_in_rewritten_pending(git_dir, rb_dir, commit_oid, next_after_line)?;
            return Ok(());
        }
        if todo_cmd == RebaseTodoCmd::Squash && !final_fixup {
            let tmpl = fs::read_to_string(rb_dir.join("message-squash"))?;
            let raw =
                commit_message_after_prepare_hook(repo, git_dir, &tmpl, "message", Some(":"))?;
            let cleaned = apply_commit_msg_cleanup(&raw, rebase_commit_msg_cleanup(&config));
            let new_oid = commit_from_merged_index(
                repo,
                git_dir,
                &merged_index,
                &config,
                amend_parents.clone(),
                &hc.author,
                cleaned,
                replay_opts,
                now,
            )?;
            fs::write(git_dir.join("HEAD"), format!("{}\n", new_oid.to_hex()))?;
            if record_rewrite {
                record_rebase_in_rewritten_pending(git_dir, rb_dir, commit_oid, next_after_line)?;
            }
            return Ok(());
        }

        if todo_cmd == RebaseTodoCmd::Fixup {
            let fixup_path = rb_dir.join("message-fixup");
            let squash_ctx = read_squash_ctx(rb_dir);
            let mut cleaned = if fixup_path.exists() && !squash_ctx.seen_squash {
                let tmpl = fs::read_to_string(&fixup_path)?;
                if fixup_message_opens_editor(rb_dir) {
                    let edit_tmpl = fixup_edit_template(rb_dir, &tmpl);
                    let raw =
                        run_commit_editor_for_template(repo, git_dir, &edit_tmpl, "squash", None)?;
                    cleanup_squash_editor_message(&raw, &config)
                } else {
                    // Git's sequencer writes only the plain folded message body (no `# This is a
                    // combination …` headers) to `message-fixup`, then `try_to_commit` runs
                    // `prepare-commit-msg` on *that* body and applies only the configured
                    // `commit.cleanup` (NONE when unset). Mirror that here: collapse the squash
                    // template down to its message body first, run the hook on the body, then apply
                    // `commit.cleanup`. Running the squash editor cleanup *after* the hook would
                    // strip hook-appended comment lines such as a trailing `# Prepared` that must
                    // survive when `commit.cleanup` is unset (t3415 `pick and fixup respect
                    // commit.cleanup`).
                    let body = cleanup_squash_editor_message(&tmpl, &config);
                    let raw = commit_message_after_prepare_hook(
                        repo,
                        git_dir,
                        &body,
                        "message",
                        Some(":"),
                    )?;
                    apply_commit_msg_cleanup(&raw, rebase_commit_msg_cleanup(&config))
                }
            } else {
                let tmpl = fs::read_to_string(rb_dir.join("message-squash"))?;
                let after_editor =
                    run_commit_editor_for_template(repo, git_dir, &tmpl, "message", None)?;
                cleanup_squash_editor_message(&after_editor, &config)
            };
            cleaned = apply_rebase_explicit_trailers_to_message(cleaned, rb_dir, &config);
            let new_oid = commit_from_merged_index(
                repo,
                git_dir,
                &merged_index,
                &config,
                amend_parents.clone(),
                &hc.author,
                cleaned,
                replay_opts,
                now,
            )?;
            fs::write(git_dir.join("HEAD"), format!("{}\n", new_oid.to_hex()))?;
            clear_squash_ctx(rb_dir);
            if record_rewrite {
                record_rebase_in_rewritten_pending(git_dir, rb_dir, commit_oid, next_after_line)?;
            }
            return Ok(());
        }

        let squash_path = rb_dir.join("message-squash");
        let fixup_path = rb_dir.join("message-fixup");
        let mut cleaned = if fixup_path.exists() {
            let fixup_msg = fs::read_to_string(&fixup_path)?;
            let body = message_body_after_subject(&commit.message);
            let tmpl = final_squash_template_after_fixups(&fixup_msg, body);
            let after_editor =
                run_commit_editor_for_template(repo, git_dir, &tmpl, "message", None)?;
            cleanup_squash_editor_message(&after_editor, &config)
        } else {
            let tmpl = fs::read_to_string(&squash_path)?;
            let after_editor =
                run_commit_editor_for_template(repo, git_dir, &tmpl, "message", None)?;
            cleanup_squash_editor_message(&after_editor, &config)
        };
        cleaned = apply_rebase_explicit_trailers_to_message(cleaned, rb_dir, &config);
        let _ = fs::remove_file(git_dir.join("MERGE_MSG"));
        let new_oid = commit_from_merged_index(
            repo,
            git_dir,
            &merged_index,
            &config,
            amend_parents.clone(),
            &hc.author,
            cleaned,
            replay_opts,
            now,
        )?;
        fs::write(git_dir.join("HEAD"), format!("{}\n", new_oid.to_hex()))?;
        clear_squash_ctx(&rb_dir);
        if record_rewrite {
            record_rebase_in_rewritten_pending(git_dir, rb_dir, commit_oid, next_after_line)?;
        }
        return Ok(());
    }

    // Create the rebased commit, preserving the original author (normal pick / reword)
    let tree_oid = write_tree_from_index(&repo.odb, &merged_index, "")?;

    if todo_cmd == RebaseTodoCmd::Reword {
        let template = if root_rebase {
            message_for_root_replayed_commit(repo, &commit, true)
        } else {
            transcoded_replayed_message(&commit, &config).0
        };
        let raw_pick =
            commit_message_after_prepare_hook(repo, git_dir, &template, "message", Some(":"))?;
        let pick_msg = apply_commit_msg_cleanup(&raw_pick, rebase_commit_msg_cleanup(&config));
        let (pick_message, enc_pick, raw_pick) =
            finalize_message_for_commit_encoding(pick_msg, &config);
        let author = rebase_replayed_author_line(&commit.author, replay_opts, now)?;
        let committer = rebase_replayed_committer_line(&config, &commit.author, replay_opts, now)?;
        let fast_forward_reword = commit.parents.first().copied() == Some(head_oid)
            && !rebase_signoff(rb_dir)
            && !rebase_has_explicit_trailers(rb_dir)
            && !replay_opts.committer_date_is_author_date
            && !replay_opts.ignore_date;
        let pick_oid = if fast_forward_reword {
            *commit_oid
        } else {
            let (author_raw, committer_raw) =
                grit_lib::commit_encoding::identity_raw_for_serialized_commit(
                    &enc_pick, &author, &committer,
                );
            let mut pick_data = CommitData {
                tree: tree_oid,
                parents: if pick_is_root {
                    Vec::new()
                } else {
                    vec![head_oid]
                },
                author: author.clone(),
                committer: committer.clone(),
                author_raw,
                committer_raw,
                encoding: enc_pick,
                message: pick_message.clone(),
                raw_message: raw_pick,
            };
            maybe_apply_rebase_message_trailers_to_commit_data(
                &mut pick_data,
                rb_dir,
                &config,
                RebaseTodoCmd::Pick,
            )?;
            let pick_bytes = serialize_commit(&pick_data);
            write_replayed_commit(repo, rb_dir, &config, &pick_data.committer, pick_bytes)?
        };
        fs::write(git_dir.join("HEAD"), format!("{}\n", pick_oid.to_hex()))?;
        let ra = load_rebase_reflog_action(rb_dir);
        let ident = reflog_identity(repo);
        let pick_subject = pick_message.lines().next().unwrap_or("");
        let pick_reflog_msg = format!("{ra} (pick): {pick_subject}");
        let _ = append_reflog(
            git_dir,
            "HEAD",
            &head_oid,
            &pick_oid,
            &ident,
            &pick_reflog_msg,
            false,
        );

        let after_editor = run_commit_editor_for_reword(repo, git_dir, &pick_message)?;
        let cleaned =
            message_from_reword_editor(&after_editor, rebase_commit_msg_cleanup(&config), &config)?;
        let cleaned = run_reword_commit_msg_hook(repo, git_dir, cleaned, &config)?;
        let (message, encoding, raw_message) =
            finalize_message_for_commit_encoding(cleaned, &config);
        let (author_raw, committer_raw) =
            grit_lib::commit_encoding::identity_raw_for_serialized_commit(
                &encoding, &author, &committer,
            );
        let mut commit_data = CommitData {
            tree: tree_oid,
            parents: if pick_is_root {
                Vec::new()
            } else {
                vec![head_oid]
            },
            author,
            committer,
            author_raw,
            committer_raw,
            encoding,
            message,
            raw_message,
        };
        maybe_apply_rebase_message_trailers_to_commit_data(
            &mut commit_data,
            rb_dir,
            &config,
            todo_cmd,
        )?;
        let commit_bytes = serialize_commit(&commit_data);
        let new_oid =
            write_replayed_commit(repo, rb_dir, &config, &commit_data.committer, commit_bytes)?;
        fs::write(git_dir.join("HEAD"), format!("{}\n", new_oid.to_hex()))?;
        if record_rewrite {
            record_rebase_in_rewritten_pending(git_dir, rb_dir, commit_oid, next_after_line)?;
        }
        return Ok(());
    }

    let (message, encoding, raw_message) = {
        let (msg_base, _enc_base, _raw_base) = if root_rebase {
            let msg = message_for_root_replayed_commit(repo, &commit, true);
            (msg, commit.encoding.clone(), None)
        } else {
            transcoded_replayed_message(&commit, &config)
        };
        let raw_msg =
            commit_message_after_prepare_hook(repo, git_dir, &msg_base, "message", Some(":"))?;
        let message = apply_commit_msg_cleanup(&raw_msg, rebase_commit_msg_cleanup(&config));
        finalize_message_for_commit_encoding(message, &config)
    };

    let author = rebase_replayed_author_line(&commit.author, replay_opts, now)?;
    let committer = rebase_replayed_committer_line(&config, &commit.author, replay_opts, now)?;
    let (author_raw, committer_raw) = grit_lib::commit_encoding::identity_raw_for_serialized_commit(
        &encoding, &author, &committer,
    );
    let mut commit_data = CommitData {
        tree: tree_oid,
        parents: if pick_is_root {
            Vec::new()
        } else {
            vec![head_oid]
        },
        author,
        committer,
        author_raw,
        committer_raw,
        encoding,
        message,
        raw_message,
    };
    maybe_apply_rebase_message_trailers_to_commit_data(
        &mut commit_data,
        rb_dir,
        &config,
        todo_cmd,
    )?;

    let commit_bytes = serialize_commit(&commit_data);
    let new_oid =
        write_replayed_commit(repo, rb_dir, &config, &commit_data.committer, commit_bytes)?;

    // Update HEAD (detached)
    fs::write(git_dir.join("HEAD"), format!("{}\n", new_oid.to_hex()))?;

    if record_rewrite {
        record_rebase_in_rewritten_pending(git_dir, rb_dir, commit_oid, next_after_line)?;
    }

    Ok(())
}

/// Finish the rebase: point the original branch at the new HEAD.
fn finish_rebase(
    repo: &Repository,
    rb_dir: &Path,
    autostash_oid: Option<ObjectId>,
    backend: RebaseBackend,
    had_rebase_autostash: bool,
) -> Result<()> {
    let git_dir = &repo.git_dir;

    let head_name = fs::read_to_string(rb_dir.join("head-name"))?;
    let head_name = head_name.trim();

    let onto_hex = fs::read_to_string(rb_dir.join("onto"))?;
    let onto_hex = onto_hex.trim();
    let onto_oid = ObjectId::from_hex(onto_hex)?;

    let ra = load_rebase_reflog_action(rb_dir);
    let ident = reflog_identity(repo);

    let head = resolve_head(git_dir)?;
    let new_tip = head
        .oid()
        .ok_or_else(|| anyhow::anyhow!("HEAD has no OID"))?
        .to_owned();

    let autostash_oid_finish = autostash_oid.or_else(|| read_autostash_oid(rb_dir).ok().flatten());
    let had_autostash_finish = had_rebase_autostash || autostash_oid_finish.is_some();

    if head_name != "detached HEAD" {
        let ref_path = git_dir.join(head_name);
        // The reflog "old" value for the `rebase (finish)` entry must be the branch's pre-rebase
        // tip (so `branch@{1}` resolves to the state before rebase, per t3404 "reflog for the
        // branch shows state before rebase"). The picks run on a detached HEAD, but the branch
        // ref file may already point at `new_tip` by the time we reach finish, so reading it back
        // would record old==new and corrupt `@{N}` resolution. Use the recorded original head.
        let old_branch_oid = rebase_orig_head_oid(rb_dir)
            .or_else(|| {
                fs::read_to_string(&ref_path)
                    .ok()
                    .and_then(|s| ObjectId::from_hex(s.trim()).ok())
            })
            .unwrap_or(new_tip);

        let finish_branch = format!("{ra} (finish): {head_name} onto {}", onto_oid.to_hex());
        let finish_head = format!("{ra} (finish): returning to {head_name}");
        let reflog_tip = fs::read_to_string(git_dir.join("logs").join(head_name))
            .ok()
            .and_then(|content| {
                let line = content.lines().last()?;
                let mut fields = line.split_whitespace();
                let _old = fields.next()?;
                let new_hex = fields.next()?;
                ObjectId::from_hex(new_hex).ok()
            });
        if old_branch_oid != new_tip || reflog_tip != Some(new_tip) {
            let _ = append_reflog(
                git_dir,
                head_name,
                &old_branch_oid,
                &new_tip,
                &ident,
                &finish_branch,
                false,
            );
        }
        let _ = append_reflog(
            git_dir,
            "HEAD",
            &new_tip,
            &new_tip,
            &ident,
            &finish_head,
            false,
        );

        if let Some(parent) = ref_path.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::write(&ref_path, format!("{}\n", new_tip.to_hex()))?;
        fs::write(git_dir.join("HEAD"), format!("ref: {head_name}\n"))?;
    }

    let preserve_path = rb_dir.join("preserve-onto-refs");
    if preserve_path.exists() {
        if let Ok(s) = fs::read_to_string(&preserve_path) {
            for line in s.lines() {
                let r = line.trim();
                if r.is_empty() || r == head_name {
                    continue;
                }
                if let Ok(cur) = resolve_ref(git_dir, r) {
                    if cur == new_tip && new_tip != onto_oid {
                        let _ = write_ref(git_dir, r, &onto_oid);
                    }
                }
            }
        }
    }

    let success_target = if head_name == "detached HEAD" {
        "HEAD"
    } else {
        head_name
    };

    flush_rebase_rewritten_pending(git_dir, rb_dir)?;
    run_post_rewrite_after_rebase(repo, rb_dir);
    let quiet = rebase_quiet(rb_dir);

    // Leave the index matching the new tip (matches Git; avoids spurious "dirty index" on the next command).
    let _ = reset_index_to_head(repo, git_dir);

    match backend {
        RebaseBackend::Merge => {
            if autostash_oid_finish.is_some() {
                apply_pending_autostash(repo, rb_dir)?;
            }
            flush_rebase_stdout();
            if !quiet {
                eprintln!("Successfully rebased and updated {success_target}.");
            }
            // Apply `--update-refs` after the success message and before clearing rebase state
            // (Git: `do_update_refs` runs after "Successfully rebased", before cleanup).
            apply_update_refs(git_dir, quiet)?;
            cleanup_rebase_state(git_dir);
        }
        RebaseBackend::Apply => {
            cleanup_rebase_state(git_dir);
            if let Some(oid) = autostash_oid_finish {
                apply_autostash_after_ff(repo, &oid)?;
            }
            // With `--apply`, Git omits the "Successfully rebased" line on stdout when autostash
            // was used (see t3420 `create_expected_success_apply`).
            if !had_autostash_finish && !quiet {
                println!("Successfully rebased and updated {success_target}.");
            }
        }
    }

    Ok(())
}

// ── --continue ──────────────────────────────────────────────────────

fn do_show_current_patch() -> Result<()> {
    let repo = Repository::discover(None).context("not a git repository")?;
    let git_dir = &repo.git_dir;
    let rb_dir =
        active_rebase_dir(git_dir).ok_or_else(|| anyhow::anyhow!("no rebase in progress"))?;
    let oid_hex = fs::read_to_string(git_dir.join("REBASE_HEAD"))
        .or_else(|_| fs::read_to_string(rb_dir.join("current")))
        .or_else(|_| fs::read_to_string(rb_dir.join("stopped-sha")))?;
    let oid_hex = oid_hex.trim();
    let trace_arg = if rb_dir.ends_with("rebase-merge") && git_dir.join("REBASE_HEAD").exists() {
        "REBASE_HEAD"
    } else {
        oid_hex
    };
    if std::env::var("GIT_TRACE").is_ok() {
        eprintln!("trace: built-in: git show {trace_arg}");
    }
    if let Ok(oid) = ObjectId::from_hex(oid_hex) {
        let obj = repo.odb.read(&oid)?;
        let commit = parse_commit(&obj.data)?;
        print!("{}", commit.message);
    }
    Ok(())
}

fn read_current_rebase_todo_cmd(rb_dir: &Path) -> RebaseTodoCmd {
    let Ok(s) = fs::read_to_string(rb_dir.join("current-cmd")) else {
        return RebaseTodoCmd::Pick;
    };
    match s.trim() {
        "reword" => RebaseTodoCmd::Reword,
        "fixup" => RebaseTodoCmd::Fixup,
        "squash" => RebaseTodoCmd::Squash,
        _ => RebaseTodoCmd::Pick,
    }
}

fn read_current_final_fixup(rb_dir: &Path) -> bool {
    fs::read_to_string(rb_dir.join("current-final-fixup"))
        .map(|s| s.trim() == "1")
        .unwrap_or(false)
}

fn pop_first_nonempty_todo_line(repo: &Repository, rb_dir: &Path) -> Result<()> {
    let s = read_rebase_todo_file(repo, rb_dir)?;
    let mut lines: Vec<String> = s.lines().map(|l| l.to_owned()).collect();
    // Pop only the first non-empty, non-comment line (the `break` made this single-shot already).
    if let Some(idx) = lines.iter().position(|l| {
        let t = l.trim();
        !t.is_empty() && !t.starts_with('#')
    }) {
        lines.remove(idx);
    }
    let mut out = lines.join("\n");
    if !out.is_empty() {
        out.push('\n');
    }
    let remaining = count_rebase_todo_actionable_lines(&out);
    write_rebase_todo_file(repo, rb_dir, &out)?;
    fs::write(rb_dir.join("msgnum"), "1")?;
    fs::write(rb_dir.join("end"), remaining.to_string())?;
    Ok(())
}

fn trim_completed_merge_line_from_rebase_todo(repo: &Repository, rb_dir: &Path) -> Result<()> {
    let s = read_rebase_todo_file(repo, rb_dir)?;
    let mut lines: Vec<String> = s.lines().map(|l| l.to_owned()).collect();
    let mut i = 0usize;
    while i < lines.len() {
        let t = lines[i].trim();
        if t.is_empty() || t.starts_with('#') {
            i += 1;
            continue;
        }
        if t.split_whitespace()
            .next()
            .is_some_and(|w| w.eq_ignore_ascii_case("merge"))
        {
            lines.remove(i);
            break;
        }
        break;
    }
    let mut out = lines.join("\n");
    if !out.is_empty() {
        out.push('\n');
    }
    let remaining = count_rebase_todo_actionable_lines(&out);
    write_rebase_todo_file(repo, rb_dir, &out)?;
    fs::write(rb_dir.join("msgnum"), "1")?;
    fs::write(rb_dir.join("end"), remaining.to_string())?;
    Ok(())
}

fn do_continue() -> Result<()> {
    let repo = Repository::discover(None).context("not a git repository")?;
    let git_dir = &repo.git_dir;

    if !is_rebase_in_progress(git_dir) {
        bail!("no rebase in progress");
    }

    let rb_dir = active_rebase_dir(git_dir)
        .ok_or_else(|| anyhow::anyhow!("internal: no rebase state directory"))?;
    let autostash_continue = read_autostash_oid(&rb_dir)?;
    let had_autostash_continue = autostash_continue.is_some();
    let backend_continue = load_rebase_backend(&rb_dir);
    let force_rewrite_continue = rb_dir.join("force-rewrite").exists();

    let interactive_continue = rb_dir.join("interactive").exists();
    let replay_opts_continue = load_rebase_replay_commit_opts(&rb_dir);
    let now_continue = time::OffsetDateTime::now_utc();

    if rb_dir.join("rebase-merge-source").exists()
        && rb_dir.join("rebase-merge-args").exists()
        && !git_dir.join("MERGE_HEAD").exists()
    {
        // Git's `commit_staged_changes` gate (sequencer.c). A merge that was *blocked* before it
        // could touch the index (e.g. an untracked working-tree file would be overwritten) leaves
        // no `.git/rebase-merge/message`. If the user then stages unrelated changes and runs
        // `--continue`, Git refuses with "you have staged changes in your working tree" rather than
        // silently retrying the merge with a dirty index (t3430 'failed `merge -C` writes patch').
        // After `git reset --hard` cleans the worktree, the index matches HEAD and we proceed to
        // retry the merge. The MERGE_HEAD conflict-resolution continue takes the other branch below
        // (it always has a `message` file), so this gate never fires for genuine conflict resolves.
        if !rb_dir.join("message").exists() && !worktree_matches_head(&repo, git_dir)? {
            bail!("error: you have staged changes in your working tree");
        }
        let src_hex = fs::read_to_string(rb_dir.join("rebase-merge-source"))?;
        let merge_src_oid = ObjectId::from_hex(src_hex.trim())?;
        let merge_args = fs::read_to_string(rb_dir.join("rebase-merge-args"))?;
        let merge_args = merge_args.trim();
        let todo_retry = read_rebase_todo_file(&repo, &rb_dir)?;
        let todo_lines_retry: Vec<&str> = rebase_todo_actionable_lines(&todo_retry);
        let next_after_retry =
            peek_next_rebase_flush_hint(&repo, &todo_lines_retry, 1, interactive_continue);
        let edit_merge = fs::read_to_string(rb_dir.join("rebase-merge-edit-msg"))
            .map(|s| s.trim() == "1")
            .unwrap_or(false);
        let st =
            run_rebase_merge_subprocess(&repo, git_dir, &merge_src_oid, merge_args, edit_merge)?;
        if st.success() {
            rewrite_merge_head_for_replay_opts(&repo, git_dir, &rb_dir, &merge_src_oid)?;
            let _ = fs::remove_file(rb_dir.join("rebase-merge-source"));
            let _ = fs::remove_file(rb_dir.join("rebase-merge-args"));
            let _ = fs::remove_file(rb_dir.join("rebase-merge-edit-msg"));
            record_rebase_in_rewritten_pending(git_dir, &rb_dir, &merge_src_oid, next_after_retry)?;
            pop_first_nonempty_todo_line(&repo, &rb_dir)?;
            trim_completed_merge_line_from_rebase_todo(&repo, &rb_dir)?;
            return replay_remaining(
                &repo,
                &rb_dir,
                autostash_continue,
                backend_continue,
                had_autostash_continue,
                force_rewrite_continue,
            );
        }
        if git_dir.join("MERGE_HEAD").exists() {
            // The retried merge produced a *content* conflict (Git's `do_merge` returns >0). Unlike
            // a blocked retry, Git drops the merge from the todo (it is now in progress via
            // MERGE_HEAD) and writes `patch` + REBASE_HEAD via `error_with_patch`. The next
            // `--continue` then commits the resolved merge through the MERGE_HEAD path below.
            let merge_commit_oid = peel_to_commit_for_merge_base(&repo, merge_src_oid)?;
            let rest: Vec<String> = todo_lines_retry
                .iter()
                .skip(1)
                .map(|s| s.to_string())
                .collect();
            let rest_body = if rest.is_empty() {
                String::new()
            } else {
                rest.join("\n") + "\n"
            };
            fs::write(rb_dir.join("git-rebase-todo"), &rest_body)?;
            fs::write(rb_dir.join("todo"), &rest_body)?;
            let _ = fs::remove_file(rb_dir.join("rebase-merge-source"));
            let _ = fs::remove_file(rb_dir.join("rebase-merge-args"));
            let _ = fs::remove_file(rb_dir.join("rebase-merge-edit-msg"));
            let _ = fs::write(
                rb_dir.join("stopped-sha"),
                format!("{}\n", merge_commit_oid.to_hex()),
            );
            fs::write(
                git_dir.join("REBASE_HEAD"),
                format!("{}\n", merge_commit_oid.to_hex()),
            )?;
            let _ = write_rebase_patch_file(&repo, &rb_dir, &merge_commit_oid);
            bail!(
                "merge conflicts during rebase merge; resolve, then run 'grit rebase --continue'"
            );
        }
        bail!("merge still blocked; fix the reported issue and run 'grit rebase --continue'");
    }

    if git_dir.join("MERGE_HEAD").exists() && rb_dir.join("rebase-merge-source").exists() {
        let src_hex = fs::read_to_string(rb_dir.join("rebase-merge-source"))?;
        let merge_src_oid = ObjectId::from_hex(src_hex.trim())?;
        let self_exe = std::env::current_exe().context("cannot determine grit binary path")?;
        let mut merge_cont = std::process::Command::new(&self_exe);
        merge_cont
            .args(["merge", "--continue"])
            .env_clear()
            .envs(std::env::vars().filter(|(k, _)| {
                !k.starts_with("GIT_") || k == "GIT_CONFIG_NOSYSTEM" || k == "GIT_CONFIG_PARAMETERS"
            }));
        if let Ok(h) = std::env::var("HOME") {
            merge_cont.env("HOME", h);
        }
        if let Ok(p) = std::env::var("PATH") {
            merge_cont.env("PATH", p);
        }
        let st = merge_cont
            .current_dir(repo.work_tree.as_deref().unwrap_or_else(|| Path::new(".")))
            .status()
            .context("run grit merge --continue during rebase")?;
        if !st.success() {
            bail!("merge --continue failed");
        }
        rewrite_merge_head_for_replay_opts(&repo, git_dir, &rb_dir, &merge_src_oid)?;
        let _ = fs::remove_file(rb_dir.join("rebase-merge-source"));
        let _ = fs::remove_file(rb_dir.join("rebase-merge-args"));
        let _ = fs::remove_file(rb_dir.join("rebase-merge-edit-msg"));
        pop_first_nonempty_todo_line(&repo, &rb_dir)?;
        trim_completed_merge_line_from_rebase_todo(&repo, &rb_dir)?;
        let todo_after = read_rebase_todo_file(&repo, &rb_dir)?;
        let todo_lines_after: Vec<&str> = rebase_todo_actionable_lines(&todo_after);
        let next_peek =
            peek_next_rebase_flush_hint(&repo, &todo_lines_after, 0, interactive_continue);
        record_rebase_in_rewritten_pending(git_dir, &rb_dir, &merge_src_oid, next_peek)?;
        return replay_remaining(
            &repo,
            &rb_dir,
            autostash_continue,
            backend_continue,
            had_autostash_continue,
            force_rewrite_continue,
        );
    }

    let todo_content_continue = read_rebase_todo_file(&repo, &rb_dir)?;
    let todo_lines_continue: Vec<&str> = rebase_todo_actionable_lines(&todo_content_continue);

    // After `break` in an interactive rebase, there is no `current` commit yet — resume the todo.
    if interactive_continue && !rb_dir.join("current").exists() && !todo_lines_continue.is_empty() {
        return replay_remaining(
            &repo,
            &rb_dir,
            autostash_continue,
            backend_continue,
            had_autostash_continue,
            force_rewrite_continue,
        );
    }

    if rb_dir.join("rebase-amend-continue").exists() {
        let index = load_index(&repo)?;
        if index.entries.iter().any(|e| e.stage() != 0) {
            bail!(
                "error: commit is not possible because you have unmerged files\n\
                 hint: fix conflicts and then run 'grit rebase --continue'"
            );
        }
        let config = ConfigSet::load(Some(git_dir), true)?;
        let head = resolve_head(git_dir)?;
        let head_oid = head
            .oid()
            .cloned()
            .ok_or_else(|| anyhow::anyhow!("HEAD has no OID"))?;
        let head_obj = repo.odb.read(&head_oid)?;
        let hc = parse_commit(&head_obj.data)?;
        let amend_parent = hc.parents.first().copied().unwrap_or(head_oid);
        let amend_stopped_hex = fs::read_to_string(rb_dir.join("stopped-sha")).unwrap_or_default();
        let amend_stopped_hex = amend_stopped_hex.trim();
        let amend_old_oid = if amend_stopped_hex.len() == 40 {
            ObjectId::from_hex(amend_stopped_hex)?
        } else {
            head_oid
        };
        let edit_continue = rb_dir.join("rebase-edit-continue").exists();
        // Git's `commit_staged_changes` compares HEAD against the OID written by `intend_to_amend()`
        // (the commit replayed when `edit` stopped), recorded in `rebase-merge/amend`. After an
        // `edit`, the replayed commit's OID differs from the original commit (`stopped-sha`), so the
        // amend check must use the recorded HEAD-at-stop, not the original commit OID. When `amend`
        // is absent (older state, or non-edit amend flows), fall back to the original OID.
        let head_at_stop = fs::read_to_string(rb_dir.join("amend"))
            .ok()
            .and_then(|s| ObjectId::from_hex(s.trim()).ok())
            .unwrap_or(amend_old_oid);
        let user_made_own_commit = edit_continue && head_oid != head_at_stop;
        if user_made_own_commit {
            // The user already made their own commit after `edit` stopped (HEAD moved), so the edit
            // commit must NOT be auto-amended.
            if worktree_matches_head(&repo, git_dir)? {
                let next_peek_amend = peek_next_rebase_flush_hint(
                    &repo,
                    &todo_lines_continue,
                    0,
                    interactive_continue,
                );
                record_rebase_in_rewritten_pending(
                    git_dir,
                    &rb_dir,
                    &amend_old_oid,
                    next_peek_amend,
                )?;
                let _ = fs::remove_file(rb_dir.join("stopped-sha"));
                let _ = fs::remove_file(rb_dir.join("amend"));
                let _ = fs::remove_file(rb_dir.join("rebase-amend-continue"));
                let _ = fs::remove_file(rb_dir.join("rebase-edit-continue"));
                return replay_remaining(
                    &repo,
                    &rb_dir,
                    autostash_continue,
                    backend_continue,
                    had_autostash_continue,
                    force_rewrite_continue,
                );
            }
            // HEAD already advanced past the edited commit but the index still carries staged
            // changes: Git refuses to silently fold them into a new commit
            // (t3404 "auto-amend only edited commits after edit").
            bail!("error: you have staged changes in your working tree");
        }
        // Git's `commit_staged_changes`: when the worktree is clean (`is_clean` and not a final
        // fixup), an `edit` continue does NOT re-commit — it removes CHERRY_PICK_HEAD/MERGE_MSG and
        // returns, leaving HEAD at the edited commit, then proceeds to the next todo item. Amending
        // an unchanged `edit` here would rewrite the commit with a fresh committer (new OID) and could
        // even pick up a stale message, so a subsequent obstruction would leave HEAD on the rewritten
        // commit instead of the original (t3404 "rebase -i commits that overwrite untracked files
        // (squash)"/"(no ff)", whose `test_cmp_rev HEAD F` must still hold after the failed continue).
        if edit_continue
            && !read_current_final_fixup(&rb_dir)
            && worktree_matches_head(&repo, git_dir)?
        {
            let _ = fs::remove_file(git_dir.join("CHERRY_PICK_HEAD"));
            let _ = fs::remove_file(git_dir.join("MERGE_MSG"));
            let next_peek_amend =
                peek_next_rebase_flush_hint(&repo, &todo_lines_continue, 0, interactive_continue);
            record_rebase_in_rewritten_pending(git_dir, &rb_dir, &amend_old_oid, next_peek_amend)?;
            let _ = fs::remove_file(rb_dir.join("stopped-sha"));
            let _ = fs::remove_file(rb_dir.join("amend"));
            let _ = fs::remove_file(rb_dir.join("rebase-amend-continue"));
            let _ = fs::remove_file(rb_dir.join("rebase-edit-continue"));
            return replay_remaining(
                &repo,
                &rb_dir,
                autostash_continue,
                backend_continue,
                had_autostash_continue,
                force_rewrite_continue,
            );
        }
        let amend_src_commit = repo
            .odb
            .read(&amend_old_oid)
            .ok()
            .and_then(|o| parse_commit(&o.data).ok());
        let source_author_amend = amend_src_commit
            .as_ref()
            .map(|c| c.author.clone())
            .unwrap_or_else(|| hc.author.clone());
        // For an `edit` continue, Git amends HEAD via `git commit --amend -F rebase-merge/message`,
        // i.e. it reuses the *edited commit's* message (which is HEAD's message, since `edit` applied
        // the commit and left HEAD at it). A leftover `.git/COMMIT_EDITMSG` from an unrelated earlier
        // commit must NOT be used here (t3404 "rebase -i commits that overwrite untracked files
        // (squash)", where a stale COMMIT_EDITMSG held "P" and corrupted the amended `edit` commit's
        // message). `COMMIT_EDITMSG` is only the message source for reword/user-edited flows.
        let msg_src = if !edit_continue && git_dir.join("COMMIT_EDITMSG").exists() {
            fs::read_to_string(git_dir.join("COMMIT_EDITMSG"))?
        } else {
            hc.message.clone()
        };
        let mut message = if user_made_own_commit {
            let raw_msg = commit_message_after_prepare_hook(
                &repo,
                git_dir,
                msg_src.trim(),
                "message",
                Some(":"),
            )?;
            let mut template = raw_msg.clone();
            if !template.ends_with('\n') {
                template.push('\n');
            }
            template.push_str("\n# Changes to be committed\n");
            let raw_msg =
                run_commit_editor_for_template(&repo, git_dir, &template, "message", Some(":"))?;
            apply_commit_msg_cleanup(&raw_msg, rebase_commit_msg_cleanup(&config))
        } else if edit_continue {
            // Git's `commit_staged_changes` continues an `edit` stop via
            // `run_git_commit(rebase_path_message(), …, AMEND_MSG | EDIT_MSG)`, i.e.
            // `git commit --amend -e -F <message>`. Because `-F` supplies the message, the
            // `prepare-commit-msg` hook receives arg1="message" (builtin/commit.c precedence),
            // not the "commit"/"HEAD" pair used by `reword` (which has no `-F`).
            let after_editor = run_edit_continue_editor(&repo, git_dir, msg_src.trim())?;
            message_from_reword_editor(&after_editor, rebase_commit_msg_cleanup(&config), &config)?
        } else {
            let raw_msg = commit_message_after_prepare_hook(
                &repo,
                git_dir,
                msg_src.trim(),
                "message",
                Some(":"),
            )?;
            apply_commit_msg_cleanup(&raw_msg, rebase_commit_msg_cleanup(&config))
        };
        if rebase_signoff(&rb_dir) {
            let committer = rebase_replayed_committer_line(
                &config,
                &source_author_amend,
                replay_opts_continue,
                now_continue,
            )?;
            let (name, email, _) = split_stored_author_line(&committer)?;
            let sob = format_signoff_line(&name, &email);
            append_signoff_trailer(&mut message, &sob, &config);
        }
        message = apply_rebase_explicit_trailers_to_message(message, &rb_dir, &config);
        let message_subject = message.lines().next().unwrap_or("").to_owned();
        let new_oid = commit_from_merged_index(
            &repo,
            git_dir,
            &index,
            &config,
            vec![amend_parent],
            &source_author_amend,
            message,
            replay_opts_continue,
            now_continue,
        )?;
        // Record the amend on HEAD's reflog. Git's `commit_staged_changes` continues via
        // `git commit --amend` with `GIT_REFLOG_ACTION="<action> (continue)"`, so the amended
        // commit gets its own reflog entry `<action> (continue): <subject>` before the rebase
        // "finish" entry. Without this, an `edit` continue would leave HEAD pointing at the
        // amended commit but the previous reflog line still showing the un-amended pick
        // (t7505 `message [edit rebase-13]` at HEAD@{1}).
        fs::write(git_dir.join("HEAD"), format!("{}\n", new_oid.to_hex()))?;
        {
            let ra = load_rebase_reflog_action(&rb_dir);
            let ident = reflog_identity(&repo);
            let msg = format!("{ra} (continue): {message_subject}");
            let _ = append_reflog(git_dir, "HEAD", &head_oid, &new_oid, &ident, &msg, false);
        }
        let next_peek_amend =
            peek_next_rebase_flush_hint(&repo, &todo_lines_continue, 0, interactive_continue);
        record_rebase_in_rewritten_pending(git_dir, &rb_dir, &amend_old_oid, next_peek_amend)?;
        let _ = fs::remove_file(rb_dir.join("stopped-sha"));
        let _ = fs::remove_file(rb_dir.join("amend"));
        let _ = fs::remove_file(rb_dir.join("rebase-amend-continue"));
        let _ = fs::remove_file(rb_dir.join("rebase-edit-continue"));
        let _ = fs::remove_file(git_dir.join("MERGE_MSG"));
        return replay_remaining(
            &repo,
            &rb_dir,
            autostash_continue,
            backend_continue,
            had_autostash_continue,
            force_rewrite_continue,
        );
    }

    // Git's `commit_staged_changes` gate (sequencer.c): a pick that was *blocked* before it could
    // touch the index (an untracked working-tree file would have been overwritten) leaves no
    // `.git/rebase-merge/message` — the commit never started. If the user then stages unrelated
    // changes and runs `--continue`, Git refuses with "you have staged changes in your working
    // tree" rather than silently re-running the pick against a dirty index. This must run BEFORE the
    // `stopped-sha`/`patch` files are consumed below: deleting them would disarm the obstructed-pick
    // re-apply path so the subsequent clean `--continue` (after `git reset --hard`) would rewrite the
    // commit instead of fast-forwarding it (t3404 "rebase -i commits that overwrite untracked
    // files", whose final `HEAD` must stay byte-identical to the original commit `D`).
    if rb_dir.join("current").exists() && !rb_dir.join("message").exists() {
        if let Some(obstructed_oid) = fs::read_to_string(rb_dir.join("current"))
            .ok()
            .and_then(|s| ObjectId::from_hex(s.trim()).ok())
        {
            if rebase_pick_was_obstructed(&rb_dir, &obstructed_oid)
                && !worktree_matches_head(&repo, git_dir)?
            {
                // main.rs prefixes `error: `, so emit the bare message to match Git exactly.
                bail!("you have staged changes in your working tree");
            }
        }
    }

    let stopped_path = rb_dir.join("stopped-sha");
    let stopped_oid = fs::read_to_string(&stopped_path).ok().and_then(|s| {
        let hex = s.lines().next()?.trim();
        ObjectId::from_hex(hex).ok()
    });
    let _ = fs::remove_file(&stopped_path);
    let _ = fs::remove_file(rb_dir.join("patch"));

    if !rb_dir.join("current").exists() {
        let first_line = todo_lines_continue.iter().copied().find(|l| {
            let t = l.trim();
            !t.is_empty() && !t.starts_with('#')
        });
        if let Some(line) = first_line {
            if let Ok(Some(step)) = parse_rebase_replay_step(&repo, line, interactive_continue) {
                if matches!(step, RebaseReplayStep::Exec(_) | RebaseReplayStep::Break) {
                    return replay_remaining(
                        &repo,
                        &rb_dir,
                        autostash_continue,
                        backend_continue,
                        had_autostash_continue,
                        force_rewrite_continue,
                    );
                }
            }
        } else if !worktree_matches_head(&repo, git_dir)? {
            bail!("error: you have staged changes in your working tree");
        } else {
            return replay_remaining(
                &repo,
                &rb_dir,
                autostash_continue,
                backend_continue,
                had_autostash_continue,
                force_rewrite_continue,
            );
        }
    }

    // Check for unresolved conflicts
    let index = load_index(&repo)?;
    if index.entries.iter().any(|e| e.stage() != 0) {
        bail!(
            "error: commit is not possible because you have unmerged files\n\
             hint: fix conflicts and then run 'grit rebase --continue'"
        );
    }
    let _ = grit_lib::rerere::repo_rerere(&repo, load_rebase_rerere_autoupdate(&rb_dir));

    // Commit the current cherry-pick
    let current_hex = fs::read_to_string(rb_dir.join("current"))?;
    let current_hex = current_hex.trim();
    let mut current_oid = ObjectId::from_hex(current_hex)?;
    maybe_run_rebase_merge_strategy(&repo, git_dir, &rb_dir, current_oid)?;

    if interactive_continue {
        if let Some(first_pick) = first_interactive_todo_pick_oid(&repo, &todo_lines_continue) {
            if first_pick != current_oid {
                if let Ok(pick_obj) = repo.odb.read(&first_pick) {
                    if let Ok(pick_commit) = parse_commit(&pick_obj.data) {
                        if diff_index_to_tree(&repo.odb, &index, Some(&pick_commit.tree), false)
                            .map(|d| d.is_empty())
                            .unwrap_or(false)
                        {
                            current_oid = first_pick;
                            fs::write(
                                rb_dir.join("current"),
                                format!("{}\n", current_oid.to_hex()),
                            )?;
                            fs::write(rb_dir.join("current-cmd"), "pick\n")?;
                        }
                    }
                }
            }
        }
    }

    let commit_obj = repo.odb.read(&current_oid)?;
    let original_commit = parse_commit(&commit_obj.data)?;

    let config = ConfigSet::load(Some(git_dir), true)?;
    let todo_cmd = read_current_rebase_todo_cmd(&rb_dir);
    let final_fixup = read_current_final_fixup(&rb_dir);

    let head = resolve_head(git_dir)?;
    let head_oid = head
        .oid()
        .ok_or_else(|| anyhow::anyhow!("HEAD has no OID"))?
        .to_owned();

    if interactive_continue && matches!(todo_cmd, RebaseTodoCmd::Fixup | RebaseTodoCmd::Squash) {
        let head_tree = commit_tree_oid_for_rebase(&repo, head_oid)?;
        // A fixup/squash whose worktree matches HEAD's tree usually means its change is already
        // folded in, so it is treated as a no-op and skipped. But a fixup that *aborted* on an
        // obstruction (its untracked file would be overwritten) was never applied and also leaves
        // the worktree equal to HEAD — yet re-applying it still amends the commit (t5407 "git rebase
        // with failed pick", whose trailing `fixup I` must map its old oid to the amended commit).
        // Only the obstruction sets the `obstructed-pick` marker, so fall through to re-apply only
        // then; genuine empty fixups (no marker) keep the no-op shortcut. Squash is left on the
        // baseline path: its editor-driven message folding does not re-run cleanly here.
        let obstructed_fixup =
            todo_cmd == RebaseTodoCmd::Fixup && rebase_pick_was_obstructed(&rb_dir, &current_oid);
        if !obstructed_fixup
            && diff_index_to_tree(&repo.odb, &index, Some(&head_tree), false)
                .map(|diffs| diffs.is_empty())
                .unwrap_or(false)
        {
            let _ = drop_last_squash_message_section(&rb_dir);
            if todo_lines_continue
                .first()
                .and_then(|line| {
                    parse_rebase_replay_step(&repo, line, interactive_continue)
                        .ok()
                        .flatten()
                })
                .and_then(|step| match step {
                    RebaseReplayStep::PickLike { oid, .. } | RebaseReplayStep::Edit(oid) => {
                        Some(oid)
                    }
                    _ => None,
                })
                == Some(current_oid)
            {
                pop_first_nonempty_todo_line(&repo, &rb_dir)?;
            }
            let remaining = read_rebase_todo_file(&repo, &rb_dir)?;
            if rebase_todo_actionable_lines(&remaining).is_empty() {
                cleanup_head_message_after_fixup_skip(&repo, git_dir, None)?;
                clear_squash_ctx(&rb_dir);
            }
            let _ = fs::remove_file(rb_dir.join("current"));
            let _ = fs::remove_file(rb_dir.join("current-cmd"));
            let _ = fs::remove_file(rb_dir.join("current-final-fixup"));
            let _ = fs::remove_file(git_dir.join("MERGE_MSG"));
            return replay_remaining(
                &repo,
                &rb_dir,
                autostash_continue,
                backend_continue,
                had_autostash_continue,
                force_rewrite_continue,
            );
        }
    }

    if stopped_oid.is_some() && worktree_matches_head(&repo, git_dir)? {
        // The worktree equals HEAD here for two very different reasons:
        //   * a conflict the user resolved to *nothing* (the commit's change is already present), or
        //   * a pick/fixup/squash that aborted on an obstruction (untracked file would be
        //     overwritten) and so was never applied at all (t5407 "git rebase with failed pick").
        // Only the obstruction sets the `obstructed-pick` marker. When it matches, re-run the step so
        // it actually produces its commit and records the real old->new mapping for the post-rewrite
        // hook; otherwise the empty no-op record stands. `cherry_pick_for_rebase` reproduces picks
        // and fixups (amending HEAD); squash keeps the baseline no-op path (its editor-driven message
        // folding does not re-run cleanly here).
        let needs_reapply = matches!(todo_cmd, RebaseTodoCmd::Pick | RebaseTodoCmd::Fixup)
            && rebase_pick_was_obstructed(&rb_dir, &current_oid);
        if needs_reapply {
            // Git re-consumes the obstructed pick into `done` when it is retried, so the obstructed
            // line is recorded twice (t3404 "rebase -i commits that overwrite untracked files"). The
            // halt saved the verbatim line; append it again here, before re-applying.
            if interactive_continue {
                if let Ok(line) = fs::read_to_string(rb_dir.join("obstructed-pick-line")) {
                    let line = line.trim_end_matches('\n');
                    if !line.is_empty() {
                        append_interactive_rebase_done_line(&rb_dir, line)?;
                    }
                }
            }
            let _ = fs::remove_file(rb_dir.join("obstructed-pick-line"));
            // Consume the obstruction marker before re-running so a fresh obstruction on a later step
            // re-arms it cleanly and a stale value can never divert an unrelated continue.
            let _ = fs::remove_file(rb_dir.join("obstructed-pick"));
            let next_after_continue =
                peek_next_rebase_flush_hint(&repo, &todo_lines_continue, 0, interactive_continue);
            let _ = fs::remove_file(git_dir.join("REBASE_HEAD"));
            let _ = fs::remove_file(git_dir.join("MERGE_MSG"));
            cherry_pick_for_rebase(
                &repo,
                &rb_dir,
                &current_oid,
                backend_continue,
                todo_cmd,
                final_fixup,
                next_after_continue,
                true,
                force_rewrite_continue,
                None,
            )?;
            let _ = fs::remove_file(rb_dir.join("current"));
            let _ = fs::remove_file(rb_dir.join("current-cmd"));
            let _ = fs::remove_file(rb_dir.join("current-final-fixup"));
            return replay_remaining(
                &repo,
                &rb_dir,
                autostash_continue,
                backend_continue,
                had_autostash_continue,
                force_rewrite_continue,
            );
        }
        let _ = fs::remove_file(rb_dir.join("obstructed-pick"));
        let oid_for_rewrite = stopped_oid.as_ref().unwrap_or(&current_oid);
        // The conflict stop wrote the todo as `todo[i..]`, leaving the just-stopped commit as the
        // first actionable line with msgnum=1. The user resolved that conflict to a no-op (worktree
        // matches HEAD), so it is recorded as already-applied and must be dropped from the todo
        // before resuming; otherwise replay_remaining would re-apply the same pick and re-conflict
        // (t3418 "merge based rebase --continue removes .git/MERGE_MSG").
        let todo_still_starts_with_current = todo_lines_continue
            .first()
            .and_then(|line| {
                parse_rebase_replay_step(&repo, line, interactive_continue)
                    .ok()
                    .flatten()
            })
            .and_then(|step| match step {
                RebaseReplayStep::PickLike { oid, .. } | RebaseReplayStep::Edit(oid) => Some(oid),
                _ => None,
            })
            .is_some_and(|oid| oid == current_oid || stopped_oid.as_ref() == Some(&oid));
        let next_peek_index = if todo_still_starts_with_current { 1 } else { 0 };
        let next_after_continue = peek_next_rebase_flush_hint(
            &repo,
            &todo_lines_continue,
            next_peek_index,
            interactive_continue,
        );
        record_rebase_in_rewritten_pending(git_dir, &rb_dir, oid_for_rewrite, next_after_continue)?;
        let _ = fs::remove_file(rb_dir.join("current"));
        let _ = fs::remove_file(rb_dir.join("current-cmd"));
        let _ = fs::remove_file(rb_dir.join("current-final-fixup"));
        let _ = fs::remove_file(git_dir.join("MERGE_MSG"));
        let _ = fs::remove_file(git_dir.join("REBASE_HEAD"));
        if todo_still_starts_with_current {
            pop_first_nonempty_todo_line(&repo, &rb_dir)?;
        }
        return replay_remaining(
            &repo,
            &rb_dir,
            autostash_continue,
            backend_continue,
            had_autostash_continue,
            force_rewrite_continue,
        );
    }

    if interactive_continue {
        let head_tree = commit_tree_oid_for_rebase(&repo, head_oid)?;
        let current_tree = commit_tree_oid_for_rebase(&repo, current_oid)?;
        let clean_ignoring_submodules =
            diff_index_to_tree(&repo.odb, &index, Some(&head_tree), true)
                .map(|diffs| diffs.is_empty())
                .unwrap_or(false);
        let current_diffs = diff_index_to_tree(&repo.odb, &index, Some(&current_tree), false)?;
        let only_submodule_resolution_left = !current_diffs.is_empty()
            && current_diffs
                .iter()
                .all(|entry| entry.old_mode == "160000" || entry.new_mode == "160000");
        if clean_ignoring_submodules && only_submodule_resolution_left {
            if todo_lines_continue
                .first()
                .and_then(|line| {
                    parse_rebase_replay_step(&repo, line, interactive_continue)
                        .ok()
                        .flatten()
                })
                .and_then(|step| match step {
                    RebaseReplayStep::PickLike { oid, .. } | RebaseReplayStep::Edit(oid) => {
                        Some(oid)
                    }
                    _ => None,
                })
                == Some(current_oid)
            {
                pop_first_nonempty_todo_line(&repo, &rb_dir)?;
            }
            let _ = fs::remove_file(rb_dir.join("current"));
            let _ = fs::remove_file(rb_dir.join("current-cmd"));
            let _ = fs::remove_file(rb_dir.join("current-final-fixup"));
            return replay_remaining(
                &repo,
                &rb_dir,
                autostash_continue,
                backend_continue,
                had_autostash_continue,
                force_rewrite_continue,
            );
        }
    }

    let new_oid = if matches!(todo_cmd, RebaseTodoCmd::Fixup | RebaseTodoCmd::Squash) {
        let head_obj = repo.odb.read(&head_oid)?;
        let hc = parse_commit(&head_obj.data)?;
        let amend_parent = hc.parents.first().copied().unwrap_or(head_oid);

        if todo_cmd == RebaseTodoCmd::Fixup && !final_fixup {
            let fixup_path = rb_dir.join("message-fixup");
            let tmpl = if fixup_path.exists() {
                fs::read_to_string(&fixup_path)?
            } else {
                hc.message.clone()
            };
            let after_editor =
                run_commit_editor_for_template(&repo, git_dir, &tmpl, "message", None)?;
            let cleaned = cleanup_squash_editor_message(&after_editor, &config);
            let _ = fs::write(&fixup_path, &cleaned);
            commit_from_merged_index(
                &repo,
                git_dir,
                &index,
                &config,
                vec![amend_parent],
                &hc.author,
                cleaned,
                replay_opts_continue,
                now_continue,
            )?
        } else if todo_cmd == RebaseTodoCmd::Squash && !final_fixup {
            let tmpl = fs::read_to_string(rb_dir.join("message-squash"))?;
            let raw =
                commit_message_after_prepare_hook(&repo, git_dir, &tmpl, "message", Some(":"))?;
            let cleaned = apply_commit_msg_cleanup(&raw, rebase_commit_msg_cleanup(&config));
            commit_from_merged_index(
                &repo,
                git_dir,
                &index,
                &config,
                vec![amend_parent],
                &hc.author,
                cleaned,
                replay_opts_continue,
                now_continue,
            )?
        } else if todo_cmd == RebaseTodoCmd::Fixup {
            let fixup_path = rb_dir.join("message-fixup");
            let squash_ctx = read_squash_ctx(&rb_dir);
            let mut cleaned = if fixup_path.exists() && !squash_ctx.seen_squash {
                let tmpl = fs::read_to_string(&fixup_path)?;
                let edit_tmpl;
                let after_editor = if fixup_message_opens_editor(&rb_dir) {
                    edit_tmpl = fixup_edit_template(&rb_dir, &tmpl);
                    run_commit_editor_for_template(&repo, git_dir, &edit_tmpl, "squash", None)?
                } else {
                    edit_tmpl = message_with_editor_separator(&tmpl);
                    run_commit_editor_for_template(&repo, git_dir, &edit_tmpl, "squash", None)?
                };
                cleanup_squash_editor_message(&after_editor, &config)
            } else {
                let tmpl = fs::read_to_string(rb_dir.join("message-squash"))?;
                let after_editor =
                    run_commit_editor_for_template(&repo, git_dir, &tmpl, "message", None)?;
                cleanup_squash_editor_message(&after_editor, &config)
            };
            cleaned = apply_rebase_explicit_trailers_to_message(cleaned, &rb_dir, &config);
            let oid = commit_from_merged_index(
                &repo,
                git_dir,
                &index,
                &config,
                vec![amend_parent],
                &hc.author,
                cleaned,
                replay_opts_continue,
                now_continue,
            )?;
            clear_squash_ctx(&rb_dir);
            oid
        } else {
            let squash_path = rb_dir.join("message-squash");
            let fixup_path = rb_dir.join("message-fixup");
            let mut cleaned = if fixup_path.exists() {
                let fixup_msg = fs::read_to_string(&fixup_path)?;
                let body = message_body_after_subject(&original_commit.message);
                let tmpl = final_squash_template_after_fixups(&fixup_msg, body);
                let after_editor =
                    run_commit_editor_for_template(&repo, git_dir, &tmpl, "message", None)?;
                cleanup_squash_editor_message(&after_editor, &config)
            } else {
                let tmpl = fs::read_to_string(&squash_path)?;
                let after_editor =
                    run_commit_editor_for_template(&repo, git_dir, &tmpl, "message", None)?;
                cleanup_squash_editor_message(&after_editor, &config)
            };
            cleaned = apply_rebase_explicit_trailers_to_message(cleaned, &rb_dir, &config);
            let oid = commit_from_merged_index(
                &repo,
                git_dir,
                &index,
                &config,
                vec![amend_parent],
                &hc.author,
                cleaned,
                replay_opts_continue,
                now_continue,
            )?;
            clear_squash_ctx(&rb_dir);
            oid
        }
    } else if todo_cmd == RebaseTodoCmd::Reword {
        let template = {
            let rb_msg = rb_dir.join("message");
            if rb_msg.exists() {
                fs::read_to_string(&rb_msg)?
            } else {
                let (unicode, _enc, _raw) = transcoded_replayed_message(&original_commit, &config);
                unicode
            }
        };
        let after_editor = run_commit_editor_for_reword(&repo, git_dir, &template)?;
        let cleaned =
            message_from_reword_editor(&after_editor, rebase_commit_msg_cleanup(&config), &config)?;
        let cleaned = run_reword_commit_msg_hook(&repo, git_dir, cleaned, &config)?;
        let (message, encoding, raw_message) =
            finalize_message_for_commit_encoding(cleaned, &config);
        let tree_oid = write_tree_from_index(&repo.odb, &index, "")?;
        let author = rebase_replayed_author_line(
            &original_commit.author,
            replay_opts_continue,
            now_continue,
        )?;
        let committer = rebase_replayed_committer_line(
            &config,
            &original_commit.author,
            replay_opts_continue,
            now_continue,
        )?;
        let (author_raw, committer_raw) =
            grit_lib::commit_encoding::identity_raw_for_serialized_commit(
                &encoding, &author, &committer,
            );
        let mut commit_data = CommitData {
            tree: tree_oid,
            parents: vec![head_oid],
            author,
            committer,
            author_raw,
            committer_raw,
            encoding,
            message,
            raw_message,
        };
        maybe_apply_rebase_message_trailers_to_commit_data(
            &mut commit_data,
            &rb_dir,
            &config,
            todo_cmd,
        )?;
        let commit_bytes = serialize_commit(&commit_data);
        write_replayed_commit(
            &repo,
            &rb_dir,
            &config,
            &commit_data.committer,
            commit_bytes,
        )?
    } else {
        let (message, _, _) = read_rebase_continue_message(git_dir, &original_commit, &config)?;
        let tree_oid = write_tree_from_index(&repo.odb, &index, "")?;
        let hook_arg1 = "message";
        let raw_msg =
            commit_message_after_prepare_hook(&repo, git_dir, &message, hook_arg1, Some(":"))?;
        let edited_message = stopped_oid.is_some();
        let raw_msg = if edited_message {
            let mut template = raw_msg.clone();
            if !template.ends_with('\n') {
                template.push('\n');
            }
            template.push_str("\n# Changes to be committed\n");
            run_commit_editor_for_template(&repo, git_dir, &template, hook_arg1, Some(":"))?
        } else {
            raw_msg
        };
        let raw_msg = if edited_message {
            let comment_prefix = comment_line_prefix_full(&config);
            cleanup_edited_commit_message(&raw_msg, comment_prefix.as_ref())
        } else {
            raw_msg
        };
        let cleaned = apply_commit_msg_cleanup(&raw_msg, rebase_commit_msg_cleanup(&config));
        let (message, encoding, raw_message) =
            finalize_message_for_commit_encoding(cleaned, &config);
        let author_script_path = rebase_merge_dir(git_dir).join("author-script");
        // The author-script preserves the picked commit's original author identity and date; fall
        // back to the picked commit's author line when it is absent. Either way, route the raw
        // author through `rebase_replayed_author_line` so `--reset-author-date` /
        // `--committer-date-is-author-date` still apply when committing a conflict resolution via
        // `--continue` (t3436). The committer line derives from the same raw author so
        // `--committer-date-is-author-date` keeps committer date == author date.
        let raw_author_for_replay = if author_script_path.exists() {
            match read_rebase_author_script(git_dir) {
                Ok(line) => line,
                Err(e) => {
                    eprintln!("error: could not parse author script");
                    eprintln!("{e:#}");
                    bail!("could not parse author script");
                }
            }
        } else {
            original_commit.author.clone()
        };
        let author = rebase_replayed_author_line(
            &raw_author_for_replay,
            replay_opts_continue,
            now_continue,
        )?;
        let committer = rebase_replayed_committer_line(
            &config,
            &raw_author_for_replay,
            replay_opts_continue,
            now_continue,
        )?;
        let (author_raw, committer_raw) =
            grit_lib::commit_encoding::identity_raw_for_serialized_commit(
                &encoding, &author, &committer,
            );
        let mut commit_data = CommitData {
            tree: tree_oid,
            parents: vec![head_oid],
            author,
            committer,
            author_raw,
            committer_raw,
            encoding,
            message,
            raw_message,
        };
        maybe_apply_rebase_message_trailers_to_commit_data(
            &mut commit_data,
            &rb_dir,
            &config,
            todo_cmd,
        )?;
        let commit_bytes = serialize_commit(&commit_data);
        write_replayed_commit(
            &repo,
            &rb_dir,
            &config,
            &commit_data.committer,
            commit_bytes,
        )?
    };

    // Update HEAD (detached)
    fs::write(git_dir.join("HEAD"), format!("{}\n", new_oid.to_hex()))?;
    let _ = fs::remove_file(git_dir.join("MERGE_MSG"));
    let _ = fs::remove_file(git_dir.join("REBASE_HEAD"));
    let _ = fs::remove_file(rb_dir.join("message"));
    let _ = fs::remove_file(rb_dir.join("current-final-fixup"));

    // The just-resolved conflicting commit is still `todo[0]` in the post-conflict todo (the
    // conflict stop wrote `todo[i..]`; we pop it only after recording, at the end of this
    // function). Its "next command" for the rewritten-pending flush decision is therefore the
    // command at index 1 — so a following `fixup`/`squash` defers the flush and both the resolved
    // commit and the fixup/squash map to the final amended commit in the post-rewrite hook input
    // (t5407 `-i (squash)`). Peeking index 0 here would see the resolved commit's own `pick` and
    // flush early, mapping it to the intermediate (pre-squash) commit.
    let todo_still_starts_with_current = todo_lines_continue
        .first()
        .and_then(|line| {
            parse_rebase_replay_step(&repo, line, interactive_continue)
                .ok()
                .flatten()
        })
        .and_then(|step| match step {
            RebaseReplayStep::PickLike { oid, .. } | RebaseReplayStep::Edit(oid) => Some(oid),
            _ => None,
        })
        .is_some_and(|oid| oid == current_oid || stopped_oid.as_ref() == Some(&oid));
    let next_peek_index = if todo_still_starts_with_current { 1 } else { 0 };
    let (oid_for_rewrite, next_after_continue) = if let Some(stopped) = stopped_oid.as_ref() {
        (
            stopped,
            peek_next_rebase_flush_hint(
                &repo,
                &todo_lines_continue,
                next_peek_index,
                interactive_continue,
            ),
        )
    } else {
        (
            &current_oid,
            peek_next_rebase_flush_hint(
                &repo,
                &todo_lines_continue,
                next_peek_index,
                interactive_continue,
            ),
        )
    };
    record_rebase_in_rewritten_pending(git_dir, &rb_dir, oid_for_rewrite, next_after_continue)?;

    let subject = original_commit.message.lines().next().unwrap_or("");
    if matches!(backend_continue, RebaseBackend::Apply) && !rebase_quiet(&rb_dir) {
        println!("Applying: {}", subject);
        flush_rebase_stdout();
    }

    let pick_backend = load_rebase_backend(&rb_dir);
    let ra = load_rebase_reflog_action(&rb_dir);
    let ident = reflog_identity(&repo);
    let verb = match pick_backend {
        RebaseBackend::Merge => "continue",
        RebaseBackend::Apply => "pick",
    };
    let msg = format!("{ra} ({verb}): {subject}");
    let _ = append_reflog(git_dir, "HEAD", &head_oid, &new_oid, &ident, &msg, false);

    if rb_dir.join("verbose").exists() {
        let head_tree = commit_tree_oid_for_rebase(&repo, head_oid)?;
        let new_tree = commit_tree_oid_for_rebase(&repo, new_oid)?;
        let entries = diff::diff_trees(&repo.odb, Some(&head_tree), Some(&new_tree), "")?;
        print_diffstat_from_entries(&repo, &entries);
    }

    // The conflict stop paths (RebaseReplayStep::PickLike / Pick `Err`) rewrite the todo as
    // `todo[i..]`, leaving the *conflicting* commit as the first actionable line with msgnum=1.
    // We have now committed that resolved commit and advanced HEAD, so drop it from the todo
    // before resuming; otherwise replay_remaining would re-apply it (double "Applying: …").
    if todo_still_starts_with_current {
        pop_first_nonempty_todo_line(&repo, &rb_dir)?;
    }

    // Continue with remaining
    replay_remaining(
        &repo,
        &rb_dir,
        autostash_continue,
        backend_continue,
        had_autostash_continue,
        force_rewrite_continue,
    )?;

    Ok(())
}

// ── --skip ──────────────────────────────────────────────────────────

fn do_skip() -> Result<()> {
    let repo = Repository::discover(None).context("not a git repository")?;
    let git_dir = &repo.git_dir;

    if !is_rebase_in_progress(git_dir) {
        bail!("no rebase in progress");
    }

    let rb_dir = active_rebase_dir(git_dir)
        .ok_or_else(|| anyhow::anyhow!("internal: no rebase state directory"))?;
    let autostash_skip = read_autostash_oid(&rb_dir)?;
    let had_autostash_skip = autostash_skip.is_some();
    let backend_skip = load_rebase_backend(&rb_dir);
    let force_rewrite_skip = rb_dir.join("force-rewrite").exists();

    let todo_content_skip = read_rebase_todo_file(&repo, &rb_dir)?;
    let todo_lines_skip: Vec<&str> = rebase_todo_actionable_lines(&todo_content_skip);
    let interactive_skip = rb_dir.join("interactive").exists();
    let current_hex_skip = fs::read_to_string(rb_dir.join("current")).unwrap_or_default();
    let current_hex_skip = current_hex_skip.trim();
    if !interactive_skip {
        if let Ok(skipped_oid) = ObjectId::from_hex(current_hex_skip) {
            let next_cmd =
                peek_next_rebase_flush_hint(&repo, &todo_lines_skip, 0, interactive_skip);
            record_rebase_in_rewritten_pending(git_dir, &rb_dir, &skipped_oid, next_cmd)?;
        }
        // After a conflict stop the todo was rewritten as `todo[i..]`, so the skipped commit is
        // still the first actionable line. Drop it so replay_remaining resumes at the next commit
        // instead of re-applying the one we are skipping (which would conflict again and re-record
        // a spurious rewrite entry).
        pop_first_nonempty_todo_line(&repo, &rb_dir)?;
    }
    let _ = fs::remove_file(rb_dir.join("stopped-sha"));

    // Clean up any conflict state
    let _ = fs::remove_file(git_dir.join("MERGE_MSG"));

    // Reset index and worktree to HEAD
    let head = resolve_head(git_dir)?;
    if let Some(head_oid) = head.oid() {
        let obj = repo.odb.read(head_oid)?;
        let commit = parse_commit(&obj.data)?;
        let entries = tree_to_index_entries(&repo, &commit.tree, "")?;
        let mut index = Index::new();
        index.entries = entries;
        index.sort();
        let old_index = load_index(&repo)?;
        if let Some(wt) = &repo.work_tree {
            preflight_cherry_pick_cwd_obstruction(&repo, wt, &index, &BTreeMap::new(), None)?;
        }
        repo.write_index(&mut index)?;
        if let Some(wt) = &repo.work_tree {
            checkout_merged_index(&repo, wt, &old_index, &index, false)?;
            refresh_index_stat_cache_from_worktree(&repo, &mut index)?;
            repo.write_index(&mut index)?;
        }
    }

    if interactive_skip {
        let skipped_cmd = read_current_rebase_todo_cmd(&rb_dir);
        if matches!(skipped_cmd, RebaseTodoCmd::Fixup | RebaseTodoCmd::Squash) {
            let _ = drop_last_squash_message_section(&rb_dir);
        }
        let todo_raw = fs::read_to_string(rb_dir.join("todo"))?;
        let mut lines: Vec<&str> = todo_raw.lines().collect();
        if let Some(pos) = lines.iter().position(|l| {
            let t = l.trim();
            !t.is_empty() && !t.starts_with('#')
        }) {
            let first_is_current =
                ObjectId::from_hex(current_hex_skip)
                    .ok()
                    .is_some_and(|current_oid| {
                        parse_rebase_replay_step(&repo, lines[pos], interactive_skip)
                            .ok()
                            .flatten()
                            .and_then(|step| match step {
                                RebaseReplayStep::PickLike { oid, .. }
                                | RebaseReplayStep::Edit(oid) => Some(oid),
                                _ => None,
                            })
                            == Some(current_oid)
                    });
            if first_is_current {
                lines.remove(pos);
                write_rebase_todo_slice(&rb_dir, &lines)?;
            }
        }
        if lines
            .iter()
            .all(|line| line.trim().is_empty() || line.trim().starts_with('#'))
        {
            let edited_message = if skipped_cmd == RebaseTodoCmd::Squash {
                let squash_msg = fs::read_to_string(rb_dir.join("message-squash")).ok();
                if squash_msg
                    .as_deref()
                    .is_some_and(|msg| msg.contains("# This is the commit message #3:"))
                {
                    match squash_msg {
                        Some(msg) => Some(run_commit_editor_for_template(
                            &repo, git_dir, &msg, "squash", None,
                        )?),
                        None => None,
                    }
                } else {
                    None
                }
            } else {
                None
            };
            cleanup_head_message_after_fixup_skip(&repo, git_dir, edited_message)?;
        }
        fs::write(rb_dir.join("msgnum"), "1")?;
    }

    replay_remaining(
        &repo,
        &rb_dir,
        autostash_skip,
        backend_skip,
        had_autostash_skip,
        force_rewrite_skip,
    )?;

    Ok(())
}

// ── --quit ──────────────────────────────────────────────────────────

fn do_quit() -> Result<()> {
    let repo = Repository::discover(None).context("not a git repository")?;
    let git_dir = &repo.git_dir;
    if !is_rebase_in_progress(git_dir) {
        bail!("no rebase in progress");
    }
    let rb_dir = active_rebase_dir(git_dir)
        .ok_or_else(|| anyhow::anyhow!("internal: no rebase state directory"))?;
    if let Some(oid) = read_autostash_oid(&rb_dir)? {
        stash::save_autostash_for_rebase_quit(&repo, &oid)?;
        let _ = fs::remove_file(rb_dir.join("autostash"));
    }
    cleanup_rebase_state(git_dir);
    Ok(())
}

// ── --abort ─────────────────────────────────────────────────────────

fn do_abort() -> Result<()> {
    let repo = Repository::discover(None).context("not a git repository")?;
    let git_dir = &repo.git_dir;

    if !is_rebase_in_progress(git_dir) {
        bail!("no rebase in progress");
    }

    let rb_dir = active_rebase_dir(git_dir)
        .ok_or_else(|| anyhow::anyhow!("internal: no rebase state directory"))?;

    let autostash_oid = read_autostash_oid(&rb_dir)?;

    // Read original HEAD and branch name
    let orig_head_hex = fs::read_to_string(rb_dir.join("orig-head"))?;
    let orig_head_hex = orig_head_hex.trim();
    let orig_head_oid = ObjectId::from_hex(orig_head_hex)?;

    let head_name = fs::read_to_string(rb_dir.join("head-name"))?;
    let head_name = head_name.trim().to_string();

    // Before mutating anything, refuse the abort if laying down the orig-HEAD
    // tree would clobber an untracked working tree file/dir. Upstream
    // `rebase --abort` resets via unpack-trees twoway_merge, whose
    // `verify_absent` refuses to overwrite untracked paths. We perform the same
    // check up front so HEAD, the index, and the worktree are all left untouched
    // on failure (t2500: `git rm init.t` then an untracked init.t must survive).
    {
        let obj = repo.odb.read(&orig_head_oid)?;
        let commit = parse_commit(&obj.data)?;
        let entries = tree_to_index_entries(&repo, &commit.tree, "")?;
        let mut restore_index = Index::new();
        restore_index.entries = entries;
        restore_index.sort();
        let old_index = load_index(&repo)?;
        if let Some((path, is_dir)) =
            super::reset::find_untracked_obstruction(&repo, &old_index, &restore_index)?
        {
            if is_dir {
                bail!("Updating '{}' would lose untracked files in it", path);
            } else {
                bail!("Updating '{}' would lose untracked files.", path);
            }
        }
    }

    let ra = load_rebase_reflog_action(&rb_dir);
    let ident = reflog_identity(&repo);
    let cur_head = resolve_head(git_dir)?;
    let cur_oid = cur_head.oid().cloned().unwrap_or_else(diff::zero_oid);
    let abort_return = if head_name == "detached HEAD" {
        orig_head_oid.to_hex()
    } else {
        head_name.clone()
    };
    let abort_msg = format!("{ra} (abort): returning to {abort_return}");
    let _ = append_reflog(
        git_dir,
        "HEAD",
        &cur_oid,
        &orig_head_oid,
        &ident,
        &abort_msg,
        false,
    );

    // Restore HEAD
    if head_name != "detached HEAD" {
        // Update branch ref
        let ref_path = git_dir.join(&head_name);
        if let Some(parent) = ref_path.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::write(&ref_path, format!("{}\n", orig_head_oid.to_hex()))?;
        // Re-attach HEAD
        fs::write(git_dir.join("HEAD"), format!("ref: {}\n", head_name))?;
    } else {
        fs::write(
            git_dir.join("HEAD"),
            format!("{}\n", orig_head_oid.to_hex()),
        )?;
    }

    // Restore index and worktree to orig HEAD
    let obj = repo.odb.read(&orig_head_oid)?;
    let commit = parse_commit(&obj.data)?;
    let entries = tree_to_index_entries(&repo, &commit.tree, "")?;
    let mut index = Index::new();
    index.entries = entries;
    index.sort();

    let old_index = load_index(&repo)?;
    if let Some(wt) = &repo.work_tree {
        preflight_cherry_pick_cwd_obstruction(&repo, wt, &index, &BTreeMap::new(), None)?;
    }
    repo.write_index(&mut index)?;

    if let Some(wt) = &repo.work_tree {
        checkout_merged_index(&repo, wt, &old_index, &index, false)?;
        refresh_index_stat_cache_from_worktree(&repo, &mut index)?;
        repo.write_index(&mut index)?;
    }

    if let Some(oid) = autostash_oid {
        let _ = stash::pop_autostash_if_top(&repo, &oid);
    }

    cleanup_rebase_state(git_dir);
    eprintln!("Rebase aborted.");

    Ok(())
}

// ── Cleanup ─────────────────────────────────────────────────────────

fn cleanup_rebase_state(git_dir: &Path) {
    let rb_merge = rebase_merge_dir(git_dir);
    let refs_del = rb_merge.join("refs-to-delete");
    if let Ok(s) = fs::read_to_string(&refs_del) {
        for line in s.lines() {
            let r = line.trim();
            if r.is_empty() {
                continue;
            }
            let _ = delete_ref(git_dir, r);
        }
    }
    let _ = fs::remove_dir_all(rebase_apply_dir(git_dir));
    let _ = fs::remove_dir_all(rb_merge);
    // Git's `remove_branch_state` clears the in-progress-merge sentinels too. A `merge`/`merge -C`
    // step that conflicted leaves `MERGE_HEAD`/`MERGE_MODE`/`AUTO_MERGE`; without removing them on
    // abort/quit/finish they leak into the *next* rebase and make its first `grit merge` die with
    // "You have not concluded your merge (MERGE_HEAD exists)" (t3430 cascade: test 8 abort →
    // test 9 `merge -c`).
    let _ = fs::remove_file(git_dir.join("MERGE_MSG"));
    let _ = fs::remove_file(git_dir.join("MERGE_HEAD"));
    let _ = fs::remove_file(git_dir.join("MERGE_MODE"));
    let _ = fs::remove_file(git_dir.join("AUTO_MERGE"));
    let _ = fs::remove_file(git_dir.join("SQUASH_MSG"));
    let _ = fs::remove_file(git_dir.join("REBASE_HEAD"));
    // Git clears CHERRY_PICK_HEAD when a rebase finishes/aborts (the sequencer's
    // `sequencer_remove_state`); a halted interactive pick records it (t3404 97/118/119).
    let _ = fs::remove_file(git_dir.join("CHERRY_PICK_HEAD"));
}

fn commit_message_unicode(commit: &CommitData) -> String {
    if let Some(raw) = &commit.raw_message {
        return grit_lib::commit_encoding::decode_bytes(commit.encoding.as_deref(), raw);
    }
    commit.message.clone()
}

fn finalize_message_for_commit_encoding(
    unicode: String,
    config: &ConfigSet,
) -> (String, Option<String>, Option<Vec<u8>>) {
    let commit_enc = config
        .get("i18n.commitEncoding")
        .or_else(|| config.get("i18n.commitencoding"));
    let is_utf8 = match commit_enc.as_deref() {
        None => true,
        Some(e) => e.eq_ignore_ascii_case("utf-8") || e.eq_ignore_ascii_case("utf8"),
    };
    if is_utf8 {
        return (unicode, None, None);
    }
    let Some(label) = commit_enc else {
        return (unicode, None, None);
    };
    let Some(raw) = grit_lib::commit_encoding::encode_unicode(&label, &unicode) else {
        return (unicode, None, None);
    };
    (unicode, Some(label), Some(raw))
}

fn transcoded_replayed_message(
    commit: &CommitData,
    config: &ConfigSet,
) -> (String, Option<String>, Option<Vec<u8>>) {
    finalize_message_for_commit_encoding(commit_message_unicode(commit), config)
}

fn write_rebase_conflict_message(
    git_dir: &Path,
    commit: &CommitData,
    config: &ConfigSet,
) -> Result<()> {
    let (unicode, _enc, raw_opt) = transcoded_replayed_message(commit, config);
    let merge_msg = git_dir.join("MERGE_MSG");
    let bytes = raw_opt.unwrap_or_else(|| unicode.into_bytes());
    fs::write(&merge_msg, &bytes)?;
    if rebase_merge_dir(git_dir).exists() {
        fs::write(rebase_merge_dir(git_dir).join("message"), bytes)?;
    }
    Ok(())
}

fn append_rebase_conflicts_comment_block(
    msg: &mut String,
    conflict_files: &[(Vec<u8>, Vec<u8>)],
    config: &ConfigSet,
) {
    if conflict_files.is_empty() {
        return;
    }
    if !msg.ends_with('\n') {
        msg.push('\n');
    }
    msg.push('\n');
    let comment_prefix = comment_line_prefix_full(config);
    msg.push_str(comment_prefix.as_ref());
    msg.push_str(" Conflicts:\n");
    for (path, _) in conflict_files {
        msg.push_str(comment_prefix.as_ref());
        msg.push('\t');
        msg.push_str(String::from_utf8_lossy(path).as_ref());
        msg.push('\n');
    }
    msg.push('\n');
}

/// Append the `# Conflicts:` hint block to a message held as raw bytes.
///
/// MERGE_MSG must stay in the same charset as `rebase-merge/message` (the commit output
/// encoding), so that `rebase --continue` can decode it correctly. When the message bytes are
/// not UTF-8 (e.g. eucJP), appending the ASCII conflict block at the byte level avoids the
/// lossy UTF-8 round-trip that [`append_rebase_conflicts_comment_block`] would impose.
fn append_rebase_conflicts_comment_block_bytes(
    msg: &mut Vec<u8>,
    conflict_files: &[(Vec<u8>, Vec<u8>)],
    config: &ConfigSet,
) {
    if conflict_files.is_empty() {
        return;
    }
    if !msg.ends_with(b"\n") {
        msg.push(b'\n');
    }
    msg.push(b'\n');
    let comment_prefix = comment_line_prefix_full(config);
    msg.extend_from_slice(comment_prefix.as_bytes());
    msg.extend_from_slice(b" Conflicts:\n");
    for (path, _) in conflict_files {
        msg.extend_from_slice(comment_prefix.as_bytes());
        msg.push(b'\t');
        msg.extend_from_slice(path);
        msg.push(b'\n');
    }
    msg.push(b'\n');
}

fn read_rebase_continue_message(
    git_dir: &Path,
    original: &CommitData,
    config: &ConfigSet,
) -> Result<(String, Option<String>, Option<Vec<u8>>)> {
    let rb = rebase_dir(git_dir);
    // During a conflicted `pick` in the merge backend, Git writes the bare commit message to
    // `rebase-merge/message` *and* the same message with an `append_conflicts_hint()` footer
    // (the `# Conflicts:` block) to `MERGE_MSG`. On `--continue`, `git commit` uses `MERGE_MSG`
    // as the editor template, so the user sees (and `commit.cleanup` strips) that block (t3428
    // 'rebase -m --signoff'). Prefer `MERGE_MSG` when present; it carries the conflicts hint and
    // is removed after each successful commit, so a stale copy never leaks into a later pick.
    let merge_msg = git_dir.join("MERGE_MSG");
    let from_state = rb.join("message");
    let bytes = if merge_msg.exists() {
        fs::read(&merge_msg)?
    } else if from_state.exists() {
        fs::read(&from_state)?
    } else {
        return Ok(transcoded_replayed_message(original, config));
    };
    let enc_name = config
        .get("i18n.commitEncoding")
        .or_else(|| config.get("i18n.commitencoding"));
    let unicode = match enc_name.as_deref() {
        Some(e) if !e.eq_ignore_ascii_case("utf-8") && !e.eq_ignore_ascii_case("utf8") => {
            grit_lib::commit_encoding::decode_bytes(Some(e), &bytes)
        }
        _ => String::from_utf8(bytes.clone()).unwrap_or_else(|_| {
            grit_lib::commit_encoding::decode_bytes(enc_name.as_deref(), &bytes)
        }),
    };
    Ok(finalize_message_for_commit_encoding(unicode, config))
}

/// Ensure `.git/rebased-patches` can be created/truncated for the apply backend, matching Git's
/// `run_am` behavior (see t3438 `unwritable rebased-patches does not leak`).
fn prepare_rebased_patches_writable(git_dir: &Path) -> Result<()> {
    let path = git_dir.join("rebased-patches");
    match OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .open(&path)
    {
        Ok(_) => {
            let _ = fs::remove_file(&path);
            Ok(())
        }
        Err(e) if e.kind() == ErrorKind::PermissionDenied => {
            bail!("could not open '{}' for writing: {}", path.display(), e)
        }
        Err(e) => Err(e).with_context(|| format!("open {}", path.display())),
    }
}

/// Single-quote a string for `rebase-merge/author-script`, matching Git's `sq_quote_buf`.
fn shell_single_quote_author_script(value: &str) -> String {
    let mut out = String::with_capacity(value.len() + 2);
    out.push('\'');
    for ch in value.chars() {
        if ch == '\'' {
            out.push_str("'\\'");
            out.push(ch);
            out.push('\'');
        } else if ch == '!' {
            out.push_str("'\\!");
            out.push('\'');
        } else {
            out.push(ch);
        }
    }
    out.push('\'');
    out
}

/// Write `rebase-merge/author-script` from the picked commit's author line (Git `write_author_script`).
fn write_rebase_author_script_for_commit(git_dir: &Path, commit: &CommitData) -> Result<()> {
    let rb_merge = rebase_merge_dir(git_dir);
    if !rb_merge.exists() {
        return Ok(());
    }
    let author = commit.author.trim_end_matches(['\r', '\n']);
    let Some(lt) = author.find('<') else {
        let _ = fs::remove_file(rb_merge.join("author-script"));
        return Ok(());
    };
    let Some(gt) = author.rfind('>') else {
        let _ = fs::remove_file(rb_merge.join("author-script"));
        return Ok(());
    };
    if gt <= lt {
        let _ = fs::remove_file(rb_merge.join("author-script"));
        return Ok(());
    }
    let name = author[..lt].trim_end();
    let email = author[lt + 1..gt].trim();
    let after_gt = author[gt + 1..].trim_start();
    if name.is_empty() || email.is_empty() {
        let _ = fs::remove_file(rb_merge.join("author-script"));
        return Ok(());
    }
    let mut script = String::new();
    script.push_str("GIT_AUTHOR_NAME=");
    script.push_str(&shell_single_quote_author_script(name));
    script.push_str("\nGIT_AUTHOR_EMAIL=");
    script.push_str(&shell_single_quote_author_script(email));
    script.push_str("\nGIT_AUTHOR_DATE=");
    script.push('\'');
    if after_gt.is_empty() {
        script.push('\'');
    } else {
        for ch in after_gt.chars() {
            if ch == '\'' {
                script.push_str("'\\'");
                script.push(ch);
                script.push('\'');
            } else if ch == '!' {
                script.push_str("'\\!");
                script.push('\'');
            } else {
                script.push(ch);
            }
        }
        script.push('\'');
    }
    script.push('\n');
    fs::write(rb_merge.join("author-script"), script)?;
    Ok(())
}

/// Dequote one Git shell single-quoted value (see `sq_dequote` in Git's `quote.c`).
fn git_sq_dequote_single_quoted(value: &str) -> Result<String> {
    let b = value.trim_start().as_bytes();
    if b.first() != Some(&b'\'') {
        bail!("unable to dequote value");
    }
    let mut i = 1usize;
    let mut out = Vec::new();
    loop {
        let c = *b
            .get(i)
            .ok_or_else(|| anyhow::anyhow!("unable to dequote value"))?;
        i += 1;
        if c != b'\'' {
            out.push(c);
            continue;
        }
        match b.get(i).copied() {
            None => {
                return String::from_utf8(out)
                    .map_err(|_| anyhow::anyhow!("unable to dequote value"));
            }
            Some(b'\\') => {
                i += 1;
                let esc = *b
                    .get(i)
                    .ok_or_else(|| anyhow::anyhow!("unable to dequote value"))?;
                let close = *b
                    .get(i + 1)
                    .ok_or_else(|| anyhow::anyhow!("unable to dequote value"))?;
                if (esc == b'\'' || esc == b'!') && close == b'\'' {
                    out.push(esc);
                    i += 2;
                    continue;
                }
                bail!("unable to dequote value");
            }
            Some(_) => bail!("unable to dequote value"),
        }
    }
}

/// Parse `rebase-merge/author-script` into a single Git author identity line (`name <email> date`).
fn read_rebase_author_script(git_dir: &Path) -> Result<String> {
    let path = rebase_merge_dir(git_dir).join("author-script");
    let raw = fs::read_to_string(&path)
        .with_context(|| format!("could not open '{}' for reading", path.display()))?;

    let mut name: Option<String> = None;
    let mut email: Option<String> = None;
    let mut date: Option<String> = None;
    let mut unknown_err: Option<anyhow::Error> = None;

    for line in raw.lines() {
        let line = line.trim_end_matches('\r');
        if line.is_empty() {
            continue;
        }
        let Some((key, value_raw)) = line.split_once('=') else {
            bail!("no key present in '{}'", line);
        };
        let key = key.trim();
        let value_raw = value_raw.trim_start();
        let decoded = git_sq_dequote_single_quoted(value_raw)
            .with_context(|| format!("unable to dequote value of '{key}'"))?;

        match key {
            "GIT_AUTHOR_NAME" => {
                if name.is_some() {
                    bail!("'GIT_AUTHOR_NAME' already given");
                }
                name = Some(decoded);
            }
            "GIT_AUTHOR_EMAIL" => {
                if email.is_some() {
                    bail!("'GIT_AUTHOR_EMAIL' already given");
                }
                email = Some(decoded);
            }
            "GIT_AUTHOR_DATE" => {
                if date.is_some() {
                    bail!("'GIT_AUTHOR_DATE' already given");
                }
                date = Some(decoded);
            }
            other => {
                unknown_err = Some(anyhow::anyhow!("unknown variable '{other}'"));
            }
        }
    }

    let mut missing = false;
    if name.is_none() {
        eprintln!("error: missing 'GIT_AUTHOR_NAME'");
        missing = true;
    }
    if email.is_none() {
        eprintln!("error: missing 'GIT_AUTHOR_EMAIL'");
        missing = true;
    }
    if date.is_none() {
        eprintln!("error: missing 'GIT_AUTHOR_DATE'");
        missing = true;
    }

    if missing || unknown_err.is_some() {
        if let Some(e) = unknown_err {
            return Err(e);
        }
        bail!("invalid author script");
    }

    let (Some(name), Some(email), Some(date)) = (name, email, date) else {
        bail!("invalid author script");
    };
    Ok(format!("{name} <{email}> {date}"))
}

// ── Helpers (mirrored from revert.rs) ───────────────────────────────

fn load_index(repo: &Repository) -> Result<Index> {
    Ok(repo.load_index()?)
}

fn resolve_identity(config: &ConfigSet, kind: &str) -> Result<(String, String)> {
    let role = match kind {
        "AUTHOR" => IdentRole::Author,
        _ => IdentRole::Committer,
    };
    Ok((resolve_name(config, role)?, resolve_email(config, role)?))
}

fn format_ident(ident: &(String, String), now: time::OffsetDateTime) -> String {
    let (name, email) = ident;
    let epoch = now.unix_timestamp();
    let offset = now.offset();
    let hours = offset.whole_hours();
    let minutes = offset.minutes_past_hour().unsigned_abs();

    let date_str = std::env::var("GIT_COMMITTER_DATE").ok();
    let timestamp = date_str
        .map(|d| grit_lib::commit::parse_date_to_git_timestamp(&d).unwrap_or(d))
        .unwrap_or_else(|| format!("{epoch} {hours:+03}{minutes:02}"));
    format!("{name} <{email}> {timestamp}")
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

fn tree_to_map(entries: Vec<IndexEntry>) -> HashMap<Vec<u8>, IndexEntry> {
    let mut out = HashMap::new();
    for e in entries {
        out.insert(e.path.clone(), e);
    }
    out
}

fn overlapping_content_changes(
    repo: &Repository,
    base: &HashMap<Vec<u8>, IndexEntry>,
    ours: &HashMap<Vec<u8>, IndexEntry>,
    theirs: &HashMap<Vec<u8>, IndexEntry>,
) -> Result<Vec<(Vec<u8>, Vec<u8>)>> {
    let mut out = Vec::new();
    for (path, base_entry) in base {
        let (Some(ours_entry), Some(theirs_entry)) = (ours.get(path), theirs.get(path)) else {
            continue;
        };
        if base_entry.oid == ours_entry.oid
            || base_entry.oid == theirs_entry.oid
            || ours_entry.oid == theirs_entry.oid
        {
            continue;
        }
        let ours_data = repo
            .odb
            .read(&ours_entry.oid)
            .map(|o| o.data)
            .unwrap_or_default();
        let theirs_data = repo
            .odb
            .read(&theirs_entry.oid)
            .map(|o| o.data)
            .unwrap_or_default();
        let base_data = repo
            .odb
            .read(&base_entry.oid)
            .map(|o| o.data)
            .unwrap_or_default();
        let merged = merge(&MergeInput {
            base: &base_data,
            ours: &ours_data,
            theirs: &theirs_data,
            label_ours: "HEAD",
            label_base: "base",
            label_theirs: "REBASE_HEAD",
            favor: MergeFavor::None,
            style: ConflictStyle::Merge,
            marker_size: 7,
            diff_algorithm: None,
            ignore_all_space: false,
            ignore_space_change: false,
            ignore_space_at_eol: false,
            ignore_cr_at_eol: false,
        })?;
        let context_delete_conflict = !contains_subslice(&ours_data, &base_data)
            && theirs_data.starts_with(&base_data)
            && base_data != ours_data
            && base_data != theirs_data;
        if merged.conflicts > 0 || context_delete_conflict {
            out.push((path.clone(), merged.content));
        }
    }
    Ok(out)
}

fn contains_subslice(haystack: &[u8], needle: &[u8]) -> bool {
    needle.is_empty()
        || haystack
            .windows(needle.len())
            .any(|window| window == needle)
}

fn same_blob(a: &IndexEntry, b: &IndexEntry) -> bool {
    a.oid == b.oid && a.mode == b.mode
}

fn apply_ws_fix_to_index(repo: &Repository, index: &mut Index, rule: u32) -> Result<()> {
    for entry in &mut index.entries {
        if entry.stage() != 0 {
            continue;
        }
        if entry.mode == MODE_SYMLINK || entry.mode == 0o160000 {
            continue;
        }
        let obj = match repo.odb.read(&entry.oid) {
            Ok(o) => o,
            Err(_) => continue,
        };
        if grit_lib::merge_file::is_binary(&obj.data) {
            continue;
        }
        let fixed = fix_blob_bytes(&obj.data, rule);
        if fixed != obj.data {
            let new_oid = repo.odb.write(ObjectKind::Blob, &fixed)?;
            entry.oid = new_oid;
        }
    }
    Ok(())
}

fn stage_entry(index: &mut Index, src: &IndexEntry, stage: u8) {
    let mut e = src.clone();
    e.flags = (e.flags & 0x0FFF) | ((stage as u16) << 12);
    index.entries.push(e);
}

struct RebaseMergeResult {
    index: Index,
    conflict_files: Vec<(Vec<u8>, Vec<u8>)>,
}

fn three_way_merge_with_content(
    repo: &Repository,
    base: &HashMap<Vec<u8>, IndexEntry>,
    ours: &HashMap<Vec<u8>, IndexEntry>,
    theirs: &HashMap<Vec<u8>, IndexEntry>,
    conflict_ctx: &RebaseConflictContext,
    favor: MergeFavor,
) -> Result<RebaseMergeResult> {
    let mut all_paths = BTreeSet::new();
    all_paths.extend(base.keys().cloned());
    all_paths.extend(ours.keys().cloned());
    all_paths.extend(theirs.keys().cloned());

    let mut out = Index::new();
    let mut conflict_files: Vec<(Vec<u8>, Vec<u8>)> = Vec::new();

    for path in all_paths {
        let b = base.get(&path);
        let o = ours.get(&path);
        let t = theirs.get(&path);

        match (b, o, t) {
            (_, Some(oe), Some(te)) if same_blob(oe, te) => {
                out.entries.push(oe.clone());
            }
            // The replayed commit may change only the executable bit while `ours` already has the
            // same blob content (e.g. a cherry-picked content change exists upstream). Git applies
            // the picked mode in that case instead of attempting a textual merge of identical
            // sides, which can otherwise produce spurious conflicts on large patch-id tests.
            (_, Some(oe), Some(te)) if oe.oid == te.oid => {
                out.entries.push(te.clone());
            }
            // When base and ours differ, we must not take `theirs` without a real merge: that
            // silently drops the divergence between base and ours (t3407-rebase-abort: second pick
            // after `rebase --continue` must still conflict).
            (Some(be), Some(oe), Some(te)) if same_blob(be, oe) && !same_blob(oe, te) => {
                out.entries.push(te.clone());
            }
            (Some(be), Some(oe), Some(te)) if same_blob(be, te) && same_blob(be, oe) => {
                out.entries.push(oe.clone());
            }
            // Mode-only change: same blob OID on all three sides (Git tree can store 644 vs 755).
            (Some(be), Some(oe), Some(te))
                if be.oid == oe.oid
                    && oe.oid == te.oid
                    && (be.mode != te.mode || oe.mode != te.mode) =>
            {
                out.entries.push(te.clone());
            }
            // Submodule gitlinks: OIDs name commits in the submodule ODB, not blobs here.
            (Some(be), Some(oe), Some(te))
                if be.mode == 0o160000 && oe.mode == 0o160000 && te.mode == 0o160000 =>
            {
                if same_blob(oe, te) {
                    out.entries.push(oe.clone());
                } else if same_blob(be, oe) && !same_blob(oe, te) {
                    out.entries.push(te.clone());
                } else if same_blob(be, te) && same_blob(be, oe) {
                    out.entries.push(oe.clone());
                } else if be.oid == oe.oid
                    && oe.oid == te.oid
                    && (be.mode != te.mode || oe.mode != te.mode)
                {
                    out.entries.push(te.clone());
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
                    &mut conflict_files,
                    &path,
                    be,
                    oe,
                    te,
                    conflict_ctx,
                    favor,
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
    Ok(RebaseMergeResult {
        index: out,
        conflict_files,
    })
}

fn content_merge_or_conflict(
    repo: &Repository,
    index: &mut Index,
    conflict_files: &mut Vec<(Vec<u8>, Vec<u8>)>,
    path: &[u8],
    base: &IndexEntry,
    ours: &IndexEntry,
    theirs: &IndexEntry,
    ctx: &RebaseConflictContext<'_>,
    favor: MergeFavor,
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

    let base_obj = repo.odb.read(&base.oid).or_else(|_| {
        let _ = crate::commands::promisor_hydrate::try_lazy_fetch_promisor_object(repo, base.oid);
        repo.odb.read(&base.oid)
    })?;
    let ours_obj = repo.odb.read(&ours.oid).or_else(|_| {
        let _ = crate::commands::promisor_hydrate::try_lazy_fetch_promisor_object(repo, ours.oid);
        repo.odb.read(&ours.oid)
    })?;
    let theirs_obj = repo.odb.read(&theirs.oid).or_else(|_| {
        let _ = crate::commands::promisor_hydrate::try_lazy_fetch_promisor_object(repo, theirs.oid);
        repo.odb.read(&theirs.oid)
    })?;

    if grit_lib::merge_file::is_binary(&base_obj.data)
        || grit_lib::merge_file::is_binary(&ours_obj.data)
        || grit_lib::merge_file::is_binary(&theirs_obj.data)
    {
        stage_entry(index, base, 1);
        stage_entry(index, ours, 2);
        stage_entry(index, theirs, 3);
        return Ok(());
    }

    let path_str = String::from_utf8_lossy(path);
    let base_label = ctx.label_base();
    let input = MergeInput {
        base: &base_obj.data,
        ours: &ours_obj.data,
        theirs: &theirs_obj.data,
        label_ours: ctx.label_ours(),
        label_base: &base_label,
        label_theirs: &path_str,
        favor,
        style: ctx.style(repo),
        marker_size: 7,
        diff_algorithm: None,
        ignore_all_space: false,
        ignore_space_change: ctx.ignore_space_change,
        ignore_space_at_eol: false,
        ignore_cr_at_eol: false,
    };

    let result = merge(&input)?;

    if result.conflicts > 0 {
        stage_entry(index, base, 1);
        stage_entry(index, ours, 2);
        stage_entry(index, theirs, 3);
        conflict_files.push((path.to_vec(), result.content));
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

pub(crate) fn write_rebase_conflict_files(
    work_tree: &Path,
    conflict_files: &[(Vec<u8>, Vec<u8>)],
) -> Result<()> {
    for (path, content) in conflict_files {
        let rel = String::from_utf8_lossy(path);
        let abs = work_tree.join(rel.as_ref());
        if let Some(parent) = abs.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::write(abs, content)?;
    }
    Ok(())
}

/// Print Git's `merge-ort` submodule conflict advice when the index has a 3-way gitlink conflict.
///
/// Matches the message shape from upstream `submodule_merge_conflict_advice` (t7402 greps a line
/// containing `go to submodule (<name>), and either merge commit <abbrev>`).
fn eprint_submodule_merge_conflict_advice(repo: &Repository, index: &Index) {
    use std::collections::BTreeMap;

    let mut by_path: BTreeMap<&[u8], [Option<&IndexEntry>; 4]> = BTreeMap::new();
    for e in &index.entries {
        let st = e.stage() as usize;
        if st == 0 || st > 3 {
            continue;
        }
        by_path.entry(e.path.as_slice()).or_default()[st] = Some(e);
    }

    let mut subs: Vec<(String, String)> = Vec::new();
    for (path_bytes, stages) in by_path {
        let Some(s1) = stages[1] else { continue };
        let Some(s2) = stages[2] else { continue };
        let Some(s3) = stages[3] else { continue };
        if s1.mode != MODE_GITLINK || s2.mode != MODE_GITLINK || s3.mode != MODE_GITLINK {
            continue;
        }
        let name = String::from_utf8_lossy(path_bytes).into_owned();
        let abbrev = abbreviate_object_id(repo, s3.oid, 7)
            .unwrap_or_else(|_| s3.oid.to_hex()[..7].to_string());
        subs.push((name, abbrev));
    }

    if subs.is_empty() {
        return;
    }

    let names_joined: String = subs
        .iter()
        .map(|(n, _)| n.as_str())
        .collect::<Vec<_>>()
        .join(" ");
    let mut steps = String::new();
    for (name, abbrev) in &subs {
        steps.push_str(&format!(
            " - go to submodule ({name}), and either merge commit {abbrev}\n   or update to an existing commit which has merged those changes\n"
        ));
    }
    eprintln!(
        "Recursive merging with submodules currently only supports trivial cases.\n\
Please manually handle the merging of each conflicted submodule.\n\
This can be accomplished with the following steps:\n\
{steps}\
 - come back to superproject and run:\n\n\
      git add {names_joined}\n\n\
   to record the above merge or update\n\
 - resolve any other conflicts in the superproject\n\
 - commit the resulting index in the superproject\n"
    );
}

pub(crate) fn checkout_merged_index(
    repo: &Repository,
    work_tree: &Path,
    old_index: &Index,
    index: &Index,
    refuse_populated_submodule_replacement: bool,
) -> Result<()> {
    let has_unmerged = index.entries.iter().any(|e| e.stage() != 0);
    if refuse_populated_submodule_replacement && !has_unmerged {
        refuse_populated_submodule_tree_replacement(old_index, index, work_tree)?;
    }
    if !has_unmerged {
        return checkout_index_to_worktree(repo, old_index, index, work_tree, false, false, true);
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
    if refuse_populated_submodule_replacement {
        refuse_populated_submodule_tree_replacement(old_index, &stage0_only, work_tree)?;
    }
    checkout_index_to_worktree(repo, old_index, &stage0_only, work_tree, false, false, true)?;

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
        let path_str = String::from_utf8_lossy(&entry.path).into_owned();
        let abs_path = work_tree.join(&path_str);
        write_unmerged_stage2_to_worktree(repo, work_tree, &path_str, &abs_path, entry)?;
        written.insert(entry.path.clone());
    }

    Ok(())
}

/// True when `path` looks like a checked-out Git submodule (nested repo), not an empty placeholder.
fn worktree_dir_is_populated_submodule(path: &Path) -> bool {
    let git_meta = path.join(".git");
    git_meta.is_file() || git_meta.is_dir()
}

fn write_unmerged_stage2_to_worktree(
    repo: &Repository,
    work_tree: &Path,
    path_str: &str,
    abs_path: &Path,
    entry: &IndexEntry,
) -> Result<()> {
    if let Some(parent) = abs_path.parent() {
        fs::create_dir_all(parent)?;
    }
    if entry.mode == MODE_GITLINK {
        if grit_lib::worktree_cwd::cwd_would_be_removed_with_repo_path(work_tree, path_str) {
            bail!("Refusing to remove the current working directory:\n{path_str}\n");
        }
        if abs_path.is_file() || abs_path.is_symlink() {
            let _ = fs::remove_file(abs_path);
        } else if abs_path.is_dir() {
            // Replacing a populated submodule directory would delete its `.git` and corrupt the
            // nested repository (t7402). Git keeps the working tree and only records the gitlink
            // OID in the index.
            if !worktree_dir_is_populated_submodule(abs_path) {
                let _ = fs::remove_dir_all(abs_path);
            }
        }
        if !abs_path.exists() {
            fs::create_dir_all(abs_path)?;
        }
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

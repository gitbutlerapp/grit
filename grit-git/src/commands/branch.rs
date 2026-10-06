//! `grit branch` -- list, create, or delete branches.

use crate::branch_ref_format::{expand_branch_format, BranchFormatContext, BranchFormatError};
use crate::commands::worktree_refs;
use crate::git_column::{
    apply_column_cli_arg, finalize_colopts, parse_column_tokens_into, print_columns,
    term_columns_minus_one, ColOpts, ColumnOptions,
};
use anyhow::{bail, Context, Result};
use clap::Args as ClapArgs;
use grit_lib::check_ref_format::{check_refname_format, RefNameOptions};
use grit_lib::config::{ConfigFile, ConfigScope, ConfigSet};
use grit_lib::diff::zero_oid;
use grit_lib::index::MODE_GITLINK;
use grit_lib::merge_base::count_symmetric_ahead_behind;
use grit_lib::merge_base::is_ancestor;
use grit_lib::objects::{parse_commit, parse_tag, parse_tree, ObjectId, ObjectKind};
use grit_lib::refs;
use grit_lib::repo::Repository;
use grit_lib::rev_parse::{
    abbreviate_object_id, resolve_revision, resolve_upstream_symbolic_name, symbolic_full_name,
};

use crate::porcelain_rev::{resolve_porcelain_commitish_filter, resolve_porcelain_merged_commit};
use grit_lib::state::{resolve_head, wt_status_get_state, HeadState};
use grit_lib::stripspace::{process as stripspace_process, Mode as StripspaceMode};
use std::cmp::Ordering;
use std::collections::HashSet;
use std::fs;
use std::io::{self, IsTerminal, Write};
use std::path::{Component, Path};
use std::process::Command;

/// Arguments for `grit branch`.
#[derive(Debug, ClapArgs)]
#[command(about = "List, create, or delete branches")]
pub struct Args {
    /// Branch name to create, first name to delete, or list pattern.
    #[arg()]
    pub name: Option<String>,

    /// Second positional: start point for new branch, or second `-d`/`-D` target.
    #[arg()]
    pub start_point: Option<String>,

    /// Further branch names for `git branch -d a b c` (third argument onward).
    #[arg(trailing_var_arg = true, hide = true)]
    pub extra_names: Vec<String>,

    /// Delete a branch.
    #[arg(short = 'd', long = "delete")]
    pub delete: bool,

    /// Force delete a branch (even if not merged).
    #[arg(short = 'D')]
    pub force_delete: bool,

    /// Move/rename a branch.
    #[arg(short = 'm', long = "move")]
    pub rename: bool,

    /// Force move/rename.
    #[arg(short = 'M')]
    pub force_rename: bool,

    /// Copy a branch.
    #[arg(short = 'c', long = "copy")]
    pub copy: bool,

    /// List branches (default when no name given).
    #[arg(short = 'l', long = "list")]
    pub list: bool,

    /// List remote-tracking branches.
    #[arg(short = 'r', long = "remotes")]
    pub remotes: bool,

    /// Rejected (Git compatibility): use without `--remotes`.
    #[arg(long = "no-remotes", hide = true)]
    pub no_remotes_neg: bool,

    /// List both local and remote branches.
    #[arg(short = 'a', long = "all")]
    pub all: bool,

    /// Rejected (Git compatibility): use without `--all`.
    #[arg(long = "no-all", hide = true)]
    pub no_all_neg: bool,

    /// Show verbose info (commit subject). Use twice (-vv) for tracking info.
    #[arg(short = 'v', long = "verbose", action = clap::ArgAction::Count)]
    pub verbose: u8,

    /// Suppress output.
    #[arg(short = 'q', long = "quiet")]
    pub quiet: bool,

    /// Show branches containing this commit.
    #[arg(long = "contains", action = clap::ArgAction::Append)]
    pub contains: Vec<String>,

    /// Show branches not containing this commit.
    #[arg(long = "no-contains", action = clap::ArgAction::Append)]
    pub no_contains: Vec<String>,

    /// Show branches merged into this commit (default: HEAD). Repeat for intersection.
    #[arg(
        long = "merged",
        action = clap::ArgAction::Append,
        num_args = 0..=1,
        default_missing_value = ""
    )]
    pub merged: Vec<String>,

    /// Show branches not merged into this commit (default: HEAD). Repeat for intersection.
    #[arg(
        long = "no-merged",
        action = clap::ArgAction::Append,
        num_args = 0..=1,
        default_missing_value = ""
    )]
    pub no_merged: Vec<String>,

    /// Force creation (overwrite existing branch).
    #[arg(short = 'f', long = "force")]
    pub force: bool,

    /// Set up tracking.
    #[arg(short = 't', long = "track", require_equals = true, num_args = 0..=1, default_missing_value = "direct")]
    pub track: Option<String>,

    /// Do not set up tracking.
    #[arg(long = "no-track")]
    pub no_track: bool,

    /// Propagate branch creation into active submodules.
    #[arg(long = "recurse-submodules")]
    pub recurse_submodules: bool,

    /// Show the current branch name.
    #[arg(long = "show-current")]
    pub show_current: bool,

    /// Set upstream tracking branch (e.g. origin/main).
    #[arg(short = 'u', long = "set-upstream-to")]
    pub set_upstream_to: Option<String>,

    /// Remove upstream tracking configuration.
    #[arg(long = "unset-upstream")]
    pub unset_upstream: bool,

    /// Sort branches (repeat for multiple keys; e.g. `--sort=refname --sort=ahead-behind:HEAD`).
    #[arg(long = "sort", action = clap::ArgAction::Append)]
    pub sort: Vec<String>,

    /// Cancel sort keys (reset to default).
    #[arg(long = "no-sort")]
    pub no_sort: bool,

    /// Case-insensitive sorting and pattern matching for listing.
    #[arg(short = 'i', long = "ignore-case")]
    pub ignore_case: bool,

    /// Omit lines that expand to empty with `--format`.
    #[arg(long = "omit-empty")]
    pub omit_empty: bool,

    /// Custom format string (for-each-ref style atoms).
    #[arg(long = "format")]
    pub format: Option<String>,

    /// Create the branch's reflog.
    #[arg(long = "create-reflog")]
    pub create_reflog: bool,

    /// Force copy.
    #[arg(short = 'C')]
    pub force_copy: bool,

    /// Display branches in columns.
    #[arg(long = "column", value_name = "STYLE", num_args = 0..=1, default_missing_value = "always")]
    pub column: Option<String>,

    /// Disable columnar output.
    #[arg(long = "no-column")]
    pub no_column: bool,

    /// Abbreviation length for object names.
    #[arg(
        long = "abbrev",
        value_name = "N",
        num_args = 0..=1,
        default_missing_value = "7",
        require_equals = true
    )]
    pub abbrev: Option<String>,

    /// Don't abbreviate.
    #[arg(long = "no-abbrev")]
    pub no_abbrev: bool,

    /// Show branches that point at a given object.
    #[arg(long = "points-at")]
    pub points_at: Option<String>,

    /// Editing mode.
    #[arg(long = "edit-description")]
    pub edit_description: bool,

    /// Use color in output.
    #[arg(long = "color", value_name = "WHEN", num_args = 0..=1, default_missing_value = "always")]
    pub color: Option<String>,

    /// Disable color output.
    #[arg(long = "no-color")]
    pub no_color: bool,
}

/// Run the `branch` command.
pub fn run(args: Args) -> Result<()> {
    // Note: previously delegated to system git, now handled natively.

    let repo = Repository::discover(None).context("not a git repository")?;
    let head = resolve_head(&repo.git_dir)?;

    // Git validates `branch.autosetuprebase` while reading config, so any `git branch` invocation
    // fails when the value is malformed or missing (t3200 145/146).
    validate_autosetuprebase_config(&repo);

    if args.no_remotes_neg {
        eprintln!("error: unknown option `no-remotes'");
        eprintln!("usage: git branch [<options>] [-r | -a] [--merged] [--no-merged]");
        std::process::exit(129);
    }
    if args.no_all_neg {
        eprintln!("error: unknown option `no-all'");
        eprintln!("usage: git branch [<options>] [-r | -a] [--merged] [--no-merged]");
        std::process::exit(129);
    }

    // `git branch --column` (explicitly enabled on the command line) is incompatible with `-v`
    // (Git `die("options '%s' and '%s' cannot be used together", "--column", "--verbose")`).
    if args.verbose > 0 && args.column.is_some() && !args.no_column {
        eprintln!("fatal: options '--column' and '--verbose' cannot be used together");
        std::process::exit(128);
    }

    // `git branch -v <pattern>` without `--list` is not a pattern listing mode; a glob is invalid
    // (t3203). With `--list`, globs are filtered in `list_branches`.
    if args.verbose > 0
        && !args.list
        && args.name.as_deref().is_some_and(is_glob_branch_pattern)
        && args.start_point.is_none()
    {
        let n = args.name.as_deref().unwrap_or("");
        eprintln!("fatal: '{n}' is not a valid branch name");
        std::process::exit(128);
    }

    // Resolve @{-N} in branch name if present
    let mut args = args;
    if let Some(ref name) = args.name.clone() {
        if let Some(base) = name
            .strip_suffix("@{upstream}")
            .or_else(|| name.strip_suffix("@{u}"))
        {
            if base.is_empty() {
                if let Ok(full) = resolve_upstream_symbolic_name(&repo, name) {
                    if let Some(local) = full.strip_prefix("refs/heads/") {
                        args.name = Some(local.to_owned());
                    } else if args.remotes {
                        if let Some(remote) = full.strip_prefix("refs/remotes/") {
                            args.name = Some(remote.to_owned());
                        }
                    }
                }
            } else {
                if let Ok(base_branch) = grit_lib::refs::resolve_at_n_branch(&repo.git_dir, base) {
                    let spec = format!("{base_branch}@{{upstream}}");
                    if let Ok(full) = resolve_upstream_symbolic_name(&repo, &spec) {
                        if let Some(local) = full.strip_prefix("refs/heads/") {
                            args.name = Some(local.to_owned());
                        } else if args.remotes {
                            if let Some(remote) = full.strip_prefix("refs/remotes/") {
                                args.name = Some(remote.to_owned());
                            }
                        }
                    }
                }
            }
        } else if name.starts_with("@{-") && name.ends_with('}') && !args.remotes {
            if let Ok(resolved) = grit_lib::refs::resolve_at_n_branch(&repo.git_dir, name) {
                args.name = Some(resolved);
            }
        } else if name.eq_ignore_ascii_case("@{upstream}") || name.eq_ignore_ascii_case("@{u}") {
            if let Ok(full) = resolve_upstream_symbolic_name(&repo, name) {
                if let Some(local) = full.strip_prefix("refs/heads/") {
                    args.name = Some(local.to_owned());
                } else if args.remotes {
                    if let Some(remote) = full.strip_prefix("refs/remotes/") {
                        args.name = Some(remote.to_owned());
                    }
                }
            }
        }
    }

    // Validate mutually exclusive mode options
    {
        let mut modes = Vec::new();
        if args.delete || args.force_delete {
            modes.push("delete");
        }
        if args.rename || args.force_rename {
            modes.push("rename");
        }
        if args.copy || args.force_copy {
            modes.push("copy");
        }
        if args.set_upstream_to.is_some() {
            modes.push("set-upstream-to");
        }
        if args.unset_upstream {
            modes.push("unset-upstream");
        }
        if args.show_current {
            modes.push("show-current");
        }
        if args.edit_description {
            modes.push("edit-description");
        }
        // --list conflicts with delete/rename/copy but not with filtering
        if args.list && !modes.is_empty() {
            bail!("options are incompatible");
        }
        if modes.len() > 1 {
            bail!("options are incompatible");
        }
    }

    let filter_active = !args.contains.is_empty()
        || !args.no_contains.is_empty()
        || !args.merged.is_empty()
        || !args.no_merged.is_empty();
    let implicit_list = filter_active && !args.list;
    if implicit_list
        && (args.delete
            || args.force_delete
            || args.rename
            || args.force_rename
            || args.copy
            || args.force_copy
            || args.edit_description)
    {
        eprintln!(
            "fatal: options such as --contains, --no-contains, --merged, and --no-merged\n\
             require branch listing; incompatible with branch modification"
        );
        std::process::exit(129);
    }

    if args.show_current {
        if let Some(name) = head.branch_name() {
            println!("{name}");
        }
        return Ok(());
    }

    if args.edit_description {
        if args.start_point.is_some() {
            eprintln!("fatal: cannot edit description of more than one branch");
            std::process::exit(128);
        }
        return edit_branch_description(&repo, &head, args.name.as_deref());
    }

    if args.set_upstream_to.is_some() {
        return set_upstream(&repo, &head, &args);
    }

    if args.unset_upstream {
        return unset_upstream(&repo, &head, &args);
    }

    if args.delete || args.force_delete {
        if args.recurse_submodules {
            eprintln!("fatal: --recurse-submodules can only be used to create branches");
            std::process::exit(128);
        }
        return delete_branches(&repo, &head, &args);
    }

    if args.rename || args.force_rename {
        return rename_branch(&repo, &head, &args);
    }

    if args.copy || args.force_copy {
        return copy_branch(&repo, &head, &args);
    }

    // If a name is given and we're not listing/filtering, create a branch
    if let Some(ref name) = args.name {
        if !args.list
            && args.contains.is_empty()
            && args.no_contains.is_empty()
            && args.merged.is_empty()
            && args.no_merged.is_empty()
        {
            // Reject invalid branch names (Git `check_branch_ref` / `die` + advice.refSyntax hint).
            validate_new_branch_name_or_die(&repo, name, 1);
            if branch_creation_recurse_submodules(&repo, &args) {
                return create_branch_recursing_submodules(
                    &repo,
                    &head,
                    name,
                    args.start_point.as_deref(),
                    &args,
                );
            }
            return create_branch(&repo, &head, name, args.start_point.as_deref(), &args);
        }
    }

    // Default: list branches
    list_branches(&repo, &head, &args)
}

/// One row in `git branch` output.
#[derive(Clone)]
struct BranchInfo {
    /// Short name for default listing (`main`, `remotes/origin/foo`, or `origin/foo` for `-r`).
    name: String,
    oid: ObjectId,
    is_remote: bool,
    /// Full ref for `%(refname)` (`refs/heads/...` / `refs/remotes/...`); absent for detached HEAD row.
    full_refname: Option<String>,
    /// ` -> short-target` when this ref is symbolic (local or remote).
    symref_suffix: Option<String>,
}

fn detached_head_description(repo: &Repository, head: &HeadState) -> Result<String> {
    let HeadState::Detached { oid } = head else {
        bail!("detached_head_description: not detached");
    };
    let git_dir = &repo.git_dir;
    let wt = wt_status_get_state(git_dir, head, true)?;
    if wt.rebase_interactive_in_progress || wt.rebase_in_progress {
        if let Some(branch) = wt.rebase_branch {
            return Ok(format!("(no branch, rebasing {branch})"));
        }
        let original = rebase_original_head_label(git_dir)
            .unwrap_or_else(|| oid.to_hex().chars().take(7).collect::<String>());
        return Ok(format!("(no branch, rebasing detached HEAD {original})"));
    }
    let state_dir = grit_lib::refs::common_dir(git_dir).unwrap_or_else(|| git_dir.clone());
    if state_dir.join("BISECT_LOG").exists() {
        let start = fs::read_to_string(state_dir.join("BISECT_START"))
            .unwrap_or_default()
            .trim()
            .to_owned();
        let label = if start.is_empty() {
            oid.to_hex().chars().take(7).collect::<String>()
        } else if let Some(rest) = start.strip_prefix("refs/heads/") {
            rest.to_owned()
        } else {
            start
        };
        return Ok(format!("(no branch, bisect started on {label})"));
    }
    if let Some(label) = wt.detached_from {
        let label = detached_label_prefer_tag(repo, &label).unwrap_or(label);
        if wt.detached_at {
            return Ok(format!("(HEAD detached at {label})"));
        }
        return Ok(format!("(HEAD detached from {label})"));
    }
    let abbrev: String = oid.to_hex().chars().take(7).collect();
    Ok(format!("(HEAD detached at {abbrev})"))
}

fn detached_label_prefer_tag(repo: &Repository, label: &str) -> Option<String> {
    let oid = ObjectId::from_hex(label)
        .or_else(|_| resolve_revision(repo, label))
        .ok()?;
    refs::list_refs(&repo.git_dir, "refs/tags/")
        .ok()?
        .into_iter()
        .find_map(|(name, tip)| {
            (tip == oid).then(|| name.strip_prefix("refs/tags/").unwrap_or(&name).to_owned())
        })
}

fn rebase_original_head_label(git_dir: &Path) -> Option<String> {
    for rel in ["rebase-merge/orig-head", "rebase-apply/orig-head"] {
        if let Ok(raw) = fs::read_to_string(git_dir.join(rel)) {
            if let Ok(oid) = ObjectId::from_hex(raw.trim()) {
                return Some(oid.to_hex().chars().take(7).collect());
            }
        }
    }
    None
}

fn shorten_symref_target(git_dir: &Path, target: &str) -> Option<String> {
    if let Some(rest) = target.strip_prefix("refs/remotes/") {
        return Some(rest.to_owned());
    }
    if let Some(rest) = target.strip_prefix("refs/heads/") {
        return Some(rest.to_owned());
    }
    if let Some(rest) = target.strip_prefix("refs/tags/") {
        return Some(rest.to_owned());
    }
    grit_lib::refs::resolve_ref(git_dir, target).ok()?;
    Some(target.to_owned())
}

fn peel_to_commit_oid(repo: &Repository, mut oid: ObjectId) -> Option<ObjectId> {
    for _ in 0..32 {
        let obj = repo.odb.read(&oid).ok()?;
        match obj.kind {
            ObjectKind::Commit => return Some(oid),
            ObjectKind::Tag => {
                let tag = parse_tag(&obj.data).ok()?;
                oid = tag.object;
            }
            _ => return None,
        }
    }
    None
}

fn branch_stable_key(b: &BranchInfo) -> String {
    b.full_refname.clone().unwrap_or_else(|| b.name.clone())
}

fn push_branch_row(
    repo: &Repository,
    branches: &mut Vec<BranchInfo>,
    full_ref: String,
    oid: ObjectId,
    is_remote: bool,
    list_name: String,
) {
    let sym = refs::read_symbolic_ref(&repo.git_dir, &full_ref)
        .ok()
        .flatten();
    let symref_suffix = sym
        .as_deref()
        .and_then(|t| shorten_symref_target(&repo.git_dir, t))
        .map(|s| format!(" -> {s}"));
    branches.push(BranchInfo {
        name: list_name,
        oid,
        is_remote,
        full_refname: Some(full_ref),
        symref_suffix,
    });
}

fn warn_broken_loose_refnames(
    git_dir: &Path,
    prefix: &str,
    seen: &mut HashSet<String>,
) -> Result<()> {
    warn_broken_loose_refnames_in_store(git_dir, prefix, seen)?;
    if let Some(common) = refs::common_dir(git_dir) {
        if common != git_dir {
            warn_broken_loose_refnames_in_store(&common, prefix, seen)?;
        }
    }
    Ok(())
}

fn warn_broken_loose_refnames_in_store(
    store: &Path,
    prefix: &str,
    seen: &mut HashSet<String>,
) -> Result<()> {
    let dir = store.join(prefix);
    let entries = match fs::read_dir(&dir) {
        Ok(entries) => entries,
        Err(err)
            if err.kind() == io::ErrorKind::NotFound
                || err.kind() == io::ErrorKind::NotADirectory =>
        {
            return Ok(());
        }
        Err(err) => return Err(err.into()),
    };

    let opts = RefNameOptions::default();
    for entry in entries {
        let entry = entry?;
        let name = entry.file_name().to_string_lossy().to_string();
        let refname = format!("{prefix}{name}");
        let ft = entry.file_type()?;
        if ft.is_dir() {
            warn_broken_loose_refnames_in_store(store, &format!("{refname}/"), seen)?;
        } else if ft.is_file()
            && check_refname_format(&refname, &opts).is_err()
            && seen.insert(refname.clone())
        {
            eprintln!("warning: ignoring ref with broken name {refname}");
        }
    }
    Ok(())
}

fn is_glob_branch_pattern(s: &str) -> bool {
    s.contains('*') || s.contains('?') || s.contains('[')
}

/// Resolve the abbreviation length for `git branch -v` listings.
///
/// Returns `None` to mean "do not abbreviate" (full 40-hex). `--abbrev`/`--no-abbrev`/
/// `core.abbrev` follow Git's rules: `--abbrev=0`, `--no-abbrev`, and `core.abbrev=no`
/// disable abbreviation; an explicit `--abbrev` length wins over `core.abbrev`.
fn resolve_abbrev_len(repo: &Repository, args: &Args) -> Option<usize> {
    if args.no_abbrev {
        return None;
    }
    if let Some(ref raw) = args.abbrev {
        // `--abbrev` with no value defaults (clap) to "7"; `--abbrev=0` disables.
        return match raw.parse::<usize>() {
            Ok(0) => None,
            Ok(n) => Some(n),
            Err(_) => Some(7),
        };
    }
    // Fall back to core.abbrev; `no`/`false`/`0` disable abbreviation.
    let cfg = ConfigSet::load(Some(&repo.git_dir), true).ok();
    match cfg.and_then(|c| c.get("core.abbrev")) {
        Some(v) => {
            let v = v.trim();
            if v.eq_ignore_ascii_case("no") || v.eq_ignore_ascii_case("false") || v == "0" {
                None
            } else {
                v.parse::<usize>().ok().or(Some(7))
            }
        }
        None => Some(7),
    }
}

/// Build the column options for `git branch` listings (`git_column_config` for "branch" plus the
/// `--column`/`--no-column` CLI flags), then resolve `auto` against stdout.
fn build_branch_colopts(repo: &Repository, args: &Args) -> ColOpts {
    let mut colopts = ColOpts::new();
    if let Ok(cfg) = ConfigSet::load(Some(&repo.git_dir), true) {
        if let Some(v) = cfg.get("column.ui") {
            let _ = parse_column_tokens_into(&v, &mut colopts);
        }
        if let Some(v) = cfg.get("column.branch") {
            let _ = parse_column_tokens_into(&v, &mut colopts);
        }
    }
    if args.no_column {
        let _ = parse_column_tokens_into("never", &mut colopts);
    } else if let Some(ref style) = args.column {
        // clap supplies "always" when `--column` is given with no value.
        let arg = if style == "always" {
            None
        } else {
            Some(style.as_str())
        };
        let _ = apply_column_cli_arg(&mut colopts, arg);
    }
    finalize_colopts(&mut colopts, None);
    colopts
}

fn abbrev_for_branch_verbose(
    repo: &Repository,
    oid: &ObjectId,
    abbrev_len: Option<usize>,
) -> String {
    let Some(n) = abbrev_len else {
        return oid.to_hex();
    };
    abbreviate_object_id(repo, *oid, n).unwrap_or_else(|_| {
        let h = oid.to_hex();
        let take = n.clamp(4, 40).min(h.len());
        h[..take].to_owned()
    })
}

fn version_segments(s: &str) -> Vec<&str> {
    s.split(['.', '-']).filter(|seg| !seg.is_empty()).collect()
}

fn compare_version_refname(a: &str, b: &str) -> Ordering {
    let seg_a = version_segments(a);
    let seg_b = version_segments(b);
    for (sa, sb) in seg_a.iter().zip(seg_b.iter()) {
        let ord = match (sa.parse::<u64>(), sb.parse::<u64>()) {
            (Ok(na), Ok(nb)) => na.cmp(&nb),
            _ => sa.cmp(sb),
        };
        if ord != Ordering::Equal {
            return ord;
        }
    }
    seg_a.len().cmp(&seg_b.len())
}

/// List branches.
fn list_branches(repo: &Repository, head: &HeadState, args: &Args) -> Result<()> {
    let stdout = io::stdout();
    let mut out = stdout.lock();

    let current_branch = head.branch_name().unwrap_or("");
    let occupied = worktree_refs::occupied_branch_refs(repo);

    let mut branches: Vec<BranchInfo> = Vec::new();
    let mut warned_broken_names = HashSet::new();

    if !args.remotes {
        if !grit_lib::reftable::is_reftable_repo(&repo.git_dir) {
            warn_broken_loose_refnames(&repo.git_dir, "refs/heads/", &mut warned_broken_names)?;
        }
        let local: Vec<(String, ObjectId)> = if grit_lib::reftable::is_reftable_repo(&repo.git_dir)
        {
            grit_lib::reftable::reftable_list_refs(&repo.git_dir, "refs/heads/")
                .map_err(|e| anyhow::anyhow!("{e}"))?
        } else {
            refs::list_refs(&repo.git_dir, "refs/heads/").map_err(|e| anyhow::anyhow!("{e}"))?
        };
        for (full, oid) in local {
            if check_refname_format(&full, &RefNameOptions::default()).is_err() {
                continue;
            }
            let short = full.strip_prefix("refs/heads/").unwrap_or(&full).to_owned();
            push_branch_row(repo, &mut branches, full, oid, false, short);
        }
    }

    if args.remotes || args.all {
        if !grit_lib::reftable::is_reftable_repo(&repo.git_dir) {
            warn_broken_loose_refnames(&repo.git_dir, "refs/remotes/", &mut warned_broken_names)?;
        }
        let remote: Vec<(String, ObjectId)> = if grit_lib::reftable::is_reftable_repo(&repo.git_dir)
        {
            grit_lib::reftable::reftable_list_refs(&repo.git_dir, "refs/remotes/")
                .map_err(|e| anyhow::anyhow!("{e}"))?
        } else {
            refs::list_refs(&repo.git_dir, "refs/remotes/").map_err(|e| anyhow::anyhow!("{e}"))?
        };
        for (full, oid) in remote {
            if check_refname_format(&full, &RefNameOptions::default()).is_err() {
                continue;
            }
            let short = full
                .strip_prefix("refs/remotes/")
                .unwrap_or(&full)
                .to_owned();
            let list_name = if args.remotes && !args.all {
                short.clone()
            } else {
                format!("remotes/{short}")
            };
            push_branch_row(repo, &mut branches, full, oid, true, list_name);
        }
    }

    if !args.merged.is_empty() {
        let mut keep = HashSet::new();
        for merged_val in &args.merged {
            let target_oid = if merged_val.is_empty() {
                *head
                    .oid()
                    .ok_or_else(|| anyhow::anyhow!("HEAD does not point to a valid commit"))?
            } else {
                resolve_porcelain_merged_commit(repo, merged_val)?
            };
            for b in &branches {
                if is_ancestor(repo, b.oid, target_oid).unwrap_or(false) {
                    keep.insert(branch_stable_key(b));
                }
            }
        }
        branches.retain(|b| keep.contains(&branch_stable_key(b)));
    }

    for no_merged_val in &args.no_merged {
        let target_oid = if no_merged_val.is_empty() {
            *head
                .oid()
                .ok_or_else(|| anyhow::anyhow!("HEAD does not point to a valid commit"))?
        } else {
            resolve_porcelain_merged_commit(repo, no_merged_val)?
        };
        branches.retain(|b| !is_ancestor(repo, b.oid, target_oid).unwrap_or(true));
    }

    if !args.contains.is_empty() {
        let contain_oids: Vec<ObjectId> = args
            .contains
            .iter()
            .map(|r| resolve_porcelain_commitish_filter(repo, r))
            .collect::<Result<_>>()?;
        branches.retain(|b| {
            contain_oids
                .iter()
                .any(|&c| is_ancestor(repo, c, b.oid).unwrap_or(false))
        });
    }

    for no_contains_rev in &args.no_contains {
        let no_contains_oid = resolve_porcelain_commitish_filter(repo, no_contains_rev)?;
        branches.retain(|b| !is_ancestor(repo, no_contains_oid, b.oid).unwrap_or(true));
    }

    if let Some(ref pat) = args.name {
        branches.retain(|b| glob_match_case(pat, &b.name, args.ignore_case));
    }

    if let Some(ref spec) = args.points_at {
        let points_oid = resolve_revision_must_be_commit(repo, spec)?;
        branches.retain(|b| {
            if let Some(ref full) = b.full_refname {
                if let Ok(Some(target)) = refs::read_symbolic_ref(&repo.git_dir, full) {
                    if let Ok(tip) = refs::resolve_ref(&repo.git_dir, &target) {
                        if tip == points_oid {
                            return false;
                        }
                    }
                }
            }
            b.oid == points_oid || peel_to_commit_oid(repo, b.oid) == Some(points_oid)
        });
    }

    let sort_keys: Vec<String> = if args.no_sort {
        Vec::new()
    } else if !args.sort.is_empty() {
        args.sort.clone()
    } else if let Some(cfg) = ConfigSet::load(Some(&repo.git_dir), true)
        .ok()
        .and_then(|c| c.get("branch.sort"))
    {
        vec![cfg]
    } else {
        Vec::new()
    };

    let list_with_pattern = args.list && args.name.is_some();
    let need_synthetic_detached = matches!(head, HeadState::Detached { .. })
        && !args.remotes
        && !list_with_pattern
        && !sort_keys.is_empty();
    if need_synthetic_detached {
        let oid = *head
            .oid()
            .ok_or_else(|| anyhow::anyhow!("detached HEAD without OID"))?;
        branches.push(BranchInfo {
            name: String::new(),
            oid,
            is_remote: false,
            full_refname: None,
            symref_suffix: None,
        });
    }

    sort_branches(repo, head, &mut branches, &sort_keys, args.ignore_case)?;

    if args.format.is_none() {
        branches.retain(|b| b.full_refname.is_some());
    }

    if let Some(ref fmt) = args.format {
        let stdout_tty = std::io::stdout().is_terminal() && grit_lib::terminal::ansi_supported();
        let emit_fmt_color = if args.no_color {
            false
        } else if let Some(ref when) = args.color {
            when != "never" && (when == "always" || stdout_tty)
        } else {
            stdout_tty
        };
        let mut rows: Vec<BranchInfo> = Vec::new();
        if matches!(head, HeadState::Detached { .. })
            && !args.remotes
            && !list_with_pattern
            && sort_keys.is_empty()
        {
            let oid = *head
                .oid()
                .ok_or_else(|| anyhow::anyhow!("detached HEAD without OID"))?;
            rows.push(BranchInfo {
                name: String::new(),
                oid,
                is_remote: false,
                full_refname: None,
                symref_suffix: None,
            });
        }
        rows.extend(branches.iter().cloned());
        for b in &rows {
            match format_branch(repo, head, b, fmt, args.omit_empty, emit_fmt_color) {
                Ok(line) => {
                    if line.is_empty() && args.omit_empty {
                        continue;
                    }
                    if emit_fmt_color && !line.is_empty() {
                        writeln!(out, "{line}\x1b[m")?;
                    } else {
                        writeln!(out, "{line}")?;
                    }
                }
                Err(BranchListError::FormatFatal(m)) => {
                    eprintln!("fatal: {m}");
                    std::process::exit(128);
                }
                Err(BranchListError::Other(e)) => return Err(e),
            }
        }
        return Ok(());
    }

    let stdout_tty = std::io::stdout().is_terminal() && grit_lib::terminal::ansi_supported();
    let use_color = if args.no_color {
        false
    } else if let Some(ref when) = args.color {
        when != "never" && (when == "always" || stdout_tty)
    } else {
        false
    };
    let (color_current, color_worktree, color_local, color_remote, color_reset) = if use_color {
        let cfg = ConfigSet::load(Some(&repo.git_dir), true).ok();
        let get_color = |key: &str, default: &str| -> String {
            let val = cfg.as_ref().and_then(|c| c.get(key));
            let cs = val.as_deref().unwrap_or(default);
            grit_lib::config::parse_color(cs).unwrap_or_default()
        };
        (
            get_color("color.branch.current", "green"),
            get_color("color.branch.worktree", "cyan"),
            get_color("color.branch.local", "normal"),
            get_color("color.branch.remote", "red"),
            "\x1b[m".to_string(),
        )
    } else {
        (
            String::new(),
            String::new(),
            String::new(),
            String::new(),
            String::new(),
        )
    };

    let abbrev_len = resolve_abbrev_len(repo, args);

    // Column layout (`--column` / `column.ui` / `column.branch`). Only applies to non-verbose,
    // non-`--format` listing; each item carries its `* `/`+ `/`  ` prefix so `print_columns`
    // aligns them the way Git does.
    let colopts = build_branch_colopts(repo, args);
    if colopts.is_active() && args.verbose == 0 {
        let mut items: Vec<String> = Vec::new();
        for b in &branches {
            let is_current = !b.is_remote && b.name == current_branch;
            let in_other_wt = b.full_refname.as_ref().is_some_and(|r| {
                occupied.get(r).is_some_and(|p| {
                    if let Some(wt) = repo.work_tree.as_deref() {
                        p != &wt.display().to_string()
                    } else {
                        true
                    }
                })
            });
            let prefix = if is_current {
                "* "
            } else if in_other_wt {
                "+ "
            } else {
                "  "
            };
            let sym = b.symref_suffix.as_deref().unwrap_or("");
            items.push(format!("{prefix}{}{sym}", b.name));
        }
        let copts = ColumnOptions {
            width: Some(term_columns_minus_one()),
            padding: 1,
            indent: String::new(),
            nl: "\n".to_owned(),
        };
        print_columns(&mut out, &items, colopts, &copts)?;
        return Ok(());
    }

    // Verbose listing: Git `calc_maxwidth` includes detached HEAD description width so the OID
    // column lines up with `* (HEAD detached ...)`.
    let max_name_len = if args.verbose > 0 {
        let mut m = branches.iter().map(|b| b.name.len()).max().unwrap_or(0);
        if matches!(head, HeadState::Detached { .. }) && !args.remotes && !list_with_pattern {
            if let Ok(desc) = detached_head_description(repo, head) {
                m = m.max(desc.len());
            }
        }
        m
    } else {
        branches
            .iter()
            .map(|b| b.name.len() + b.symref_suffix.as_ref().map(|s| s.len()).unwrap_or(0))
            .max()
            .unwrap_or(0)
    };

    if matches!(head, HeadState::Detached { .. })
        && !args.remotes
        && !list_with_pattern
        && args.points_at.is_none()
    {
        let desc = detached_head_description(repo, head)?;
        let prefix = "* ";
        if use_color {
            write!(out, "{prefix}{color_current}{desc}{color_reset}")?;
        } else {
            write!(out, "{prefix}{desc}")?;
        }
        if args.verbose > 0 {
            if let Some(oid) = head.oid() {
                let short = abbrev_for_branch_verbose(repo, oid, abbrev_len);
                let subject = commit_subject(&repo.odb, oid).unwrap_or_default();
                write!(out, " {short} {subject}")?;
            }
        }
        writeln!(out)?;
    }

    for b in &branches {
        let is_current = !b.is_remote && b.name == current_branch;
        let in_other_wt = b.full_refname.as_ref().is_some_and(|r| {
            occupied.get(r).is_some_and(|p| {
                if let Some(wt) = repo.work_tree.as_deref() {
                    p != &wt.display().to_string()
                } else {
                    true
                }
            })
        });
        let prefix = if is_current {
            "* "
        } else if in_other_wt {
            "+ "
        } else {
            "  "
        };
        let color = if use_color {
            if is_current {
                &color_current
            } else if in_other_wt {
                &color_worktree
            } else if b.is_remote {
                &color_remote
            } else {
                &color_local
            }
        } else {
            &color_local
        };
        let reset = if use_color {
            &color_reset
        } else {
            &color_local
        };

        let sym = b.symref_suffix.as_deref().unwrap_or("");
        // `git branch -v` omits ` -> target` for symrefs (plain `git branch` still shows them).
        let sym_out = if args.verbose > 0 { "" } else { sym };
        let display_name = format!("{}{}", b.name, sym_out);

        if args.verbose > 0 {
            let short = abbrev_for_branch_verbose(repo, &b.oid, abbrev_len);
            let subject = commit_subject(&repo.odb, &b.oid).unwrap_or_default();

            if !b.is_remote {
                let track = resolve_branch_tracking(repo, &b.name)?;
                let v1 = track.as_ref().and_then(|t| t.verbose1_inner.as_deref());
                let v2 = track.as_ref().and_then(|t| t.verbose2_inner.as_deref());
                if use_color {
                    let padded_name = format!("{:<width$}", b.name, width = max_name_len);
                    write!(out, "{prefix}{color}{padded_name}{reset}{sym_out}")?;
                } else {
                    let padded_name = format!("{:<width$}", b.name, width = max_name_len);
                    write!(out, "{prefix}{padded_name}{sym_out}")?;
                }
                write!(out, " {short}")?;
                if args.verbose >= 2 {
                    if !is_current && in_other_wt {
                        if let Some(r) = &b.full_refname {
                            if let Some(path) = occupied.get(r) {
                                if use_color {
                                    write!(out, " ({color_worktree}{path}{color_reset})")?;
                                } else {
                                    write!(out, " ({path})")?;
                                }
                            }
                        }
                    }
                    if let Some(i2) = v2 {
                        write!(out, " [{i2}]")?;
                    } else if let Some(i1) = v1 {
                        write!(out, " [{i1}]")?;
                    }
                } else if args.verbose == 1 {
                    if !is_current && in_other_wt {
                        if let Some(r) = &b.full_refname {
                            if let Some(path) = occupied.get(r) {
                                if use_color {
                                    write!(out, " ({color_worktree}{path}{color_reset})")?;
                                } else {
                                    write!(out, " ({path})")?;
                                }
                            }
                        }
                    }
                    if let Some(i1) = v1 {
                        write!(out, " [{i1}]")?;
                    }
                }
                writeln!(out, " {subject}")?;
            } else {
                if use_color {
                    let padded_name = format!("{:<width$}", b.name, width = max_name_len);
                    writeln!(
                        out,
                        "{prefix}{color}{padded_name}{reset}{sym_out} {short} {subject}"
                    )?;
                } else {
                    let padded_name = format!("{:<width$}", b.name, width = max_name_len);
                    writeln!(out, "{prefix}{padded_name}{sym_out} {short} {subject}")?;
                }
            }
        } else if use_color {
            writeln!(out, "{prefix}{color}{}{reset}{sym_out}", b.name)?;
        } else {
            writeln!(out, "{prefix}{display_name}")?;
        }
    }

    Ok(())
}

#[derive(Debug)]
enum BranchListError {
    FormatFatal(String),
    Other(anyhow::Error),
}

impl From<anyhow::Error> for BranchListError {
    fn from(e: anyhow::Error) -> Self {
        BranchListError::Other(e)
    }
}

/// Resolve a revision and require the peeled object to be a commit (Git `branch --contains` rules).
///
/// On failure, prints Git-compatible messages to stderr and exits with code 129.
fn resolve_revision_must_be_commit(repo: &Repository, spec: &str) -> Result<ObjectId> {
    let oid = resolve_revision(repo, spec)?;
    let object = repo.odb.read(&oid)?;
    let hex = oid.to_hex();
    let kind_msg = match object.kind {
        ObjectKind::Commit => return Ok(oid),
        ObjectKind::Tree => "tree",
        ObjectKind::Blob => "blob",
        ObjectKind::Tag => "tag",
    };
    eprintln!("error: object {hex} is a {kind_msg}, not a commit");
    eprintln!("error: no such commit {hex}");
    std::process::exit(129);
}

/// Sort branches by the given key.
fn sort_branches(
    repo: &Repository,
    head: &HeadState,
    branches: &mut [BranchInfo],
    sort_keys: &[String],
    ignore_case: bool,
) -> Result<()> {
    let head_oid = head.oid().copied();
    let cmp_name = |a: &BranchInfo, b: &BranchInfo| -> Ordering {
        if ignore_case {
            a.name
                .to_lowercase()
                .cmp(&b.name.to_lowercase())
                .then_with(|| a.name.cmp(&b.name))
        } else {
            a.name.cmp(&b.name)
        }
    };
    if sort_keys.is_empty() {
        branches.sort_by(|a, b| cmp_name(a, b));
        return Ok(());
    }

    // Git: the last `--sort` is the primary key; earlier keys break ties.
    // `-key` reverses only that key's primary comparison; refname tie-break stays ascending.
    branches.sort_by(|a, b| {
        match (a.full_refname.is_none(), b.full_refname.is_none()) {
            (true, false) => return Ordering::Less,
            (false, true) => return Ordering::Greater,
            _ => {}
        }
        for raw in sort_keys.iter().rev() {
            let (desc, key) = raw
                .strip_prefix('-')
                .map(|k| (true, k))
                .unwrap_or((false, raw.as_str()));
            let mut primary = match key {
                "refname" => cmp_name(a, b),
                "committerdate" => {
                    let ta = committer_time(&repo.odb, &a.oid, &a.name, a.is_remote);
                    let tb = committer_time(&repo.odb, &b.oid, &b.name, b.is_remote);
                    ta.cmp(&tb)
                }
                "authordate" => {
                    let ta = author_time(&repo.odb, &a.oid, &a.name, a.is_remote);
                    let tb = author_time(&repo.odb, &b.oid, &b.name, b.is_remote);
                    ta.cmp(&tb)
                }
                "objectsize" => {
                    let sa = object_size_for_sort(repo, a);
                    let sb = object_size_for_sort(repo, b);
                    sa.cmp(&sb)
                }
                "type" => {
                    let ka = object_kind_for_sort(repo, a);
                    let kb = object_kind_for_sort(repo, b);
                    ka.cmp(&kb)
                }
                "version:refname" => compare_version_refname(&a.name, &b.name),
                k if k.starts_with("ahead-behind:") => {
                    let spec = &k["ahead-behind:".len()..];
                    match (head_oid, resolve_revision(repo, spec).ok()) {
                        (Some(h), Some(_base)) => {
                            let ab_a =
                                count_symmetric_ahead_behind(repo, a.oid, h).unwrap_or((0, 0));
                            let ab_b =
                                count_symmetric_ahead_behind(repo, b.oid, h).unwrap_or((0, 0));
                            ab_a.0.cmp(&ab_b.0).then_with(|| ab_a.1.cmp(&ab_b.1))
                        }
                        _ => Ordering::Equal,
                    }
                }
                other => {
                    eprintln!("error: unknown field name: {other}");
                    std::process::exit(129);
                }
            };
            if desc {
                primary = primary.reverse();
            }
            if primary != Ordering::Equal {
                return primary;
            }
        }
        cmp_name(a, b)
    });
    Ok(())
}

fn object_size_for_sort(repo: &Repository, b: &BranchInfo) -> u64 {
    repo.odb
        .read(&b.oid)
        .map(|o| o.data.len() as u64)
        .unwrap_or(0)
}

fn object_kind_for_sort(repo: &Repository, b: &BranchInfo) -> u8 {
    repo.odb
        .read(&b.oid)
        .map(|o| match o.kind {
            ObjectKind::Commit => 0u8,
            ObjectKind::Tag => 1,
            ObjectKind::Tree => 2,
            ObjectKind::Blob => 3,
        })
        .unwrap_or(255)
}

/// Parsed upstream / ahead-behind display for a local branch (`git branch -v` / `-vv`).
struct BranchTracking {
    /// Inner text for `-v` brackets, e.g. `ahead 1` or `origin/main: ahead 1`.
    verbose1_inner: Option<String>,
    /// Inner text for `-vv` brackets (includes remote ref prefix when Git does).
    verbose2_inner: Option<String>,
    /// Short upstream name for `%(upstream:short)` (e.g. `main` or `origin/main`).
    upstream_short: String,
    /// Full ref for `%(upstream)` (`refs/heads/...` or `refs/remotes/...`).
    upstream_ref_full: String,
}

/// Resolve configured upstream and ahead/behind for a local branch.
fn resolve_branch_tracking(repo: &Repository, branch_name: &str) -> Result<Option<BranchTracking>> {
    let config_path = repo.git_dir.join("config");
    let config_file = match ConfigFile::from_path(&config_path, ConfigScope::Local)? {
        Some(c) => c,
        None => return Ok(None),
    };
    let mut config = ConfigSet::new();
    config.merge(&config_file);

    let merge_key = format!("branch.{branch_name}.merge");
    let remote_key = format!("branch.{branch_name}.remote");

    let merge = match config.get(&merge_key) {
        Some(m) => m,
        None => return Ok(None),
    };
    let remote = config
        .get(&remote_key)
        .unwrap_or_else(|| "origin".to_string());

    let upstream_branch = merge
        .strip_prefix("refs/heads/")
        .unwrap_or(&merge)
        .to_string();
    let track_local = remote == ".";

    let local_ref_path = repo.git_dir.join("refs/heads").join(branch_name);
    let local_oid = match fs::read_to_string(&local_ref_path) {
        Ok(c) => ObjectId::from_hex(c.trim()).ok(),
        Err(_) => None,
    };
    let Some(local_oid) = local_oid else {
        return Ok(None);
    };

    let short_label = if track_local {
        upstream_branch.clone()
    } else {
        format!("{remote}/{upstream_branch}")
    };
    let upstream_ref_full = if track_local {
        format!("refs/heads/{upstream_branch}")
    } else {
        format!("refs/remotes/{remote}/{upstream_branch}")
    };

    let upstream_oid = if track_local {
        let p = repo.git_dir.join("refs/heads").join(&upstream_branch);
        match fs::read_to_string(&p) {
            Ok(c) => ObjectId::from_hex(c.trim()).ok(),
            Err(_) => None,
        }
    } else {
        let upstream_ref_path = repo
            .git_dir
            .join("refs/remotes")
            .join(&remote)
            .join(&upstream_branch);
        match fs::read_to_string(&upstream_ref_path) {
            Ok(c) => ObjectId::from_hex(c.trim()).ok(),
            Err(_) => None,
        }
    };

    let Some(upstream_oid) = upstream_oid else {
        return Ok(Some(BranchTracking {
            verbose1_inner: Some("gone".to_string()),
            verbose2_inner: Some(format!("{short_label}: gone")),
            upstream_short: short_label.clone(),
            upstream_ref_full,
        }));
    };

    let (ahead, behind) = count_ahead_behind(repo, local_oid, upstream_oid)?;
    if ahead == 0 && behind == 0 {
        // `git branch -vv` still shows the upstream in brackets when in sync (e.g. `[origin/main]`).
        let verbose2_inner = if track_local {
            None
        } else {
            Some(short_label.clone())
        };
        return Ok(Some(BranchTracking {
            verbose1_inner: None,
            verbose2_inner,
            upstream_short: short_label,
            upstream_ref_full,
        }));
    }

    let mut parts = Vec::new();
    if ahead > 0 {
        parts.push(format!("ahead {ahead}"));
    }
    if behind > 0 {
        parts.push(format!("behind {behind}"));
    }
    let detail = parts.join(", ");

    // `git branch -v` omits the remote prefix; `git branch -vv` includes it (e.g. `origin/main:`).
    let (verbose1_inner, verbose2_inner) = if track_local {
        (
            Some(detail.clone()),
            Some(format!("{}: {detail}", upstream_branch)),
        )
    } else {
        (
            Some(detail.clone()),
            Some(format!("{short_label}: {detail}")),
        )
    };

    Ok(Some(BranchTracking {
        verbose1_inner,
        verbose2_inner,
        upstream_short: short_label,
        upstream_ref_full,
    }))
}

/// Count how many commits local is ahead of and behind upstream.
fn count_ahead_behind(
    repo: &Repository,
    local: ObjectId,
    upstream: ObjectId,
) -> Result<(usize, usize)> {
    Ok(count_symmetric_ahead_behind(repo, local, upstream)?)
}

/// `git branch --edit-description`: open an editor on the branch description, then store in config.
fn edit_branch_description(
    repo: &Repository,
    head: &HeadState,
    branch_arg: Option<&str>,
) -> Result<()> {
    let branch_name: String = match branch_arg {
        Some(n) => n.to_owned(),
        None => match head {
            HeadState::Branch { short_name, .. } => short_name.clone(),
            HeadState::Detached { .. } => {
                eprintln!("fatal: cannot give description to detached HEAD");
                std::process::exit(128);
            }
            HeadState::Invalid => {
                eprintln!("fatal: cannot give description to detached HEAD");
                std::process::exit(128);
            }
        },
    };

    let branch_ref = format!("refs/heads/{branch_name}");
    let ref_exists = grit_lib::refs::resolve_ref(&repo.git_dir, &branch_ref).is_ok();

    match head {
        HeadState::Branch {
            short_name,
            oid: None,
            ..
        } if branch_arg.is_none() => {
            eprintln!("error: no commit on branch '{short_name}' yet");
            std::process::exit(1);
        }
        _ => {}
    }

    if branch_arg.is_some() && !ref_exists {
        if branch_ref_is_unborn_across_worktrees(repo, &branch_ref)? {
            eprintln!("error: no commit on branch '{branch_name}' yet");
            std::process::exit(1);
        }
        eprintln!("error: no branch named '{branch_name}'");
        std::process::exit(1);
    }

    let desc_key = format!("branch.{branch_name}.description");
    let config_path = repo.git_dir.join("config");
    let content = fs::read_to_string(&config_path).unwrap_or_default();
    let mut cs = ConfigSet::new();
    let file_cfg = ConfigFile::parse(&config_path, &content, ConfigScope::Local)?;
    cs.merge(&file_cfg);
    let had_description = cs.get(&desc_key).is_some();
    let existing = cs.get(&desc_key).unwrap_or_default();

    let mut initial = existing.clone();
    if !initial.is_empty() && !initial.ends_with('\n') {
        initial.push('\n');
    }
    initial.push_str(&format!(
        "# Please edit the description for the branch\n\
         #   {branch_name}\n\
         # Lines starting with '#' will be stripped.\n"
    ));

    let edited = launch_editor_for_branch_description(repo, &initial)?;
    let stripped = String::from_utf8_lossy(&stripspace_process(
        edited.as_bytes(),
        &StripspaceMode::StripComments("#".into()),
    ))
    .to_string();

    if stripped.is_empty() && !had_description {
        return Ok(());
    }

    let mut config = ConfigFile::parse(&config_path, &content, ConfigScope::Local)?;
    if stripped.is_empty() {
        let _ = config.unset(&desc_key);
    } else {
        config.set(&desc_key, &stripped)?;
    }
    config.write()?;
    Ok(())
}

fn is_effective_editor_value(raw: &str) -> bool {
    let t = raw.trim();
    !t.is_empty() && t != ":"
}

/// Same resolution order as `git commit`: skip `:` placeholders for `VISUAL` / `EDITOR`.
fn resolve_branch_description_editor(repo: &Repository) -> String {
    let visual_present = std::env::var("VISUAL").is_ok();
    let editor_present = std::env::var("EDITOR").is_ok();

    if let Ok(e) = std::env::var("GIT_EDITOR") {
        if is_effective_editor_value(&e) {
            return e;
        }
    }
    if let Ok(cfg) = ConfigSet::load(Some(&repo.git_dir), true) {
        if let Some(e) = cfg.get("core.editor") {
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
    if visual_present || editor_present {
        "true".to_owned()
    } else {
        "vi".to_owned()
    }
}

fn launch_editor_for_branch_description(repo: &Repository, initial: &str) -> Result<String> {
    let editor = resolve_branch_description_editor(repo);

    let tmp_dir = repo.git_dir.join("tmp");
    let _ = fs::create_dir_all(&tmp_dir);
    let tmp_path = tmp_dir.join("EDIT_DESCRIPTION");
    fs::write(&tmp_path, initial)?;

    let status = Command::new("sh")
        .arg("-c")
        .arg(format!("{editor} \"$1\""))
        .arg("sh")
        .arg(&tmp_path)
        .status()
        .with_context(|| format!("failed to launch editor '{editor}'"))?;

    if !status.success() {
        let _ = fs::remove_file(&tmp_path);
        bail!("editor exited with non-zero status");
    }

    let result = fs::read_to_string(&tmp_path)?;
    let _ = fs::remove_file(&tmp_path);
    Ok(result)
}

/// Set upstream tracking branch.
fn set_upstream(repo: &Repository, head: &HeadState, args: &Args) -> Result<()> {
    let upstream_raw = args
        .set_upstream_to
        .as_deref()
        .ok_or_else(|| anyhow::anyhow!("upstream name required"))?;
    let upstream = if upstream_raw.starts_with("@{-") && upstream_raw.ends_with('}') {
        grit_lib::refs::resolve_at_n_branch(&repo.git_dir, upstream_raw)?
    } else {
        upstream_raw.to_string()
    };

    // The branch is the first positional; any further positionals are an error (Git: argc > 1).
    if args.start_point.is_some() || !args.extra_names.is_empty() {
        eprintln!("fatal: too many arguments to set new upstream");
        std::process::exit(128);
    }

    let branch_name = match args.name.as_deref() {
        Some(n) if n != "HEAD" => n.to_owned(),
        _ => match head.branch_name() {
            Some(n) => n.to_owned(),
            None => {
                eprintln!(
                    "fatal: could not set upstream of HEAD to {upstream} when it does not point to any branch"
                );
                std::process::exit(128);
            }
        },
    };

    let branch_ref = format!("refs/heads/{branch_name}");
    if grit_lib::refs::resolve_ref(&repo.git_dir, &branch_ref).is_err() {
        if branch_ref_is_unborn_across_worktrees(repo, &branch_ref)? {
            eprintln!("fatal: no commit on branch '{branch_name}' yet");
            std::process::exit(128);
        }
        eprintln!("fatal: branch '{branch_name}' does not exist");
        std::process::exit(128);
    }

    // Validate the upstream resolves to a branch (Git `dwim_branch_start` with explicit tracking).
    validate_upstream_is_branch(repo, &upstream);

    // Parse upstream as remote/branch
    let (remote, upstream_branch) = parse_upstream(repo, &upstream)?;

    if remote == "." && upstream_branch == branch_name {
        eprintln!("warning: not setting branch '{branch_name}' as its own upstream");
        return Ok(());
    }

    die_if_config_locked(repo);
    let config_path = repo.git_dir.join("config");
    let content = fs::read_to_string(&config_path).unwrap_or_default();
    let mut config = ConfigFile::parse(&config_path, &content, ConfigScope::Local)?;

    let remote_key = format!("branch.{branch_name}.remote");
    let merge_key = format!("branch.{branch_name}.merge");

    config.set(&remote_key, &remote)?;
    config.set(&merge_key, &format!("refs/heads/{upstream_branch}"))?;
    config.write()?;

    if !args.quiet {
        let track_label = if remote == "." {
            upstream_branch.clone()
        } else {
            format!("{remote}/{upstream_branch}")
        };
        eprintln!("branch '{branch_name}' set up to track '{track_label}'.");
    }

    Ok(())
}

/// Validate that the `--set-upstream-to` argument names a branch (Git `dwim_branch_start` with
/// explicit tracking). Exits 128 with the matching Git diagnostic on failure.
fn validate_upstream_is_branch(repo: &Repository, upstream: &str) {
    // Does it resolve to any object at all? If not, the upstream branch is missing.
    if resolve_revision(repo, upstream).is_err() {
        eprintln!("fatal: the requested upstream branch '{upstream}' does not exist");
        eprintln!(
            "\nIf you are planning on basing your work on an upstream\n\
             branch that already exists at the remote, you may need to\n\
             run \"git fetch\" to retrieve it.\n\n\
             If you are planning to push out a new local branch that\n\
             will track its remote counterpart, you may want to use\n\
             \"git push -u\" to set the upstream config as you push."
        );
        std::process::exit(128);
    }

    // It resolves to an object; it must DWIM to a real branch (local or remote-tracking).
    let is_branch = match symbolic_full_name(repo, upstream) {
        Some(full) => full.starts_with("refs/heads/") || full.starts_with("refs/remotes/"),
        None => false,
    };
    if !is_branch {
        eprintln!(
            "fatal: cannot set up tracking information; starting point '{upstream}' is not a branch"
        );
        std::process::exit(128);
    }
}

/// Remove upstream tracking configuration.
fn unset_upstream(repo: &Repository, head: &HeadState, args: &Args) -> Result<()> {
    // The branch is the first positional; further positionals are an error (Git: argc > 1).
    if args.start_point.is_some() || !args.extra_names.is_empty() {
        eprintln!("fatal: too many arguments to unset upstream");
        std::process::exit(128);
    }

    let branch_name = match args.name.as_deref() {
        Some(n) if n != "HEAD" => n.to_owned(),
        _ => match head.branch_name() {
            Some(n) => n.to_owned(),
            None => {
                eprintln!(
                    "fatal: could not unset upstream of HEAD when it does not point to any branch"
                );
                std::process::exit(128);
            }
        },
    };

    let config_path = repo.git_dir.join("config");
    let content = fs::read_to_string(&config_path).unwrap_or_default();
    let mut config = ConfigFile::parse(&config_path, &content, ConfigScope::Local)?;

    let merge_key = format!("branch.{branch_name}.merge");

    // Check if there's actually tracking info — use ConfigSet to read
    let mut cs = ConfigSet::new();
    cs.merge(&config);
    if cs.get(&merge_key).is_none() {
        eprintln!("fatal: branch '{branch_name}' has no upstream information");
        std::process::exit(1);
    }

    die_if_config_locked(repo);
    let remote_key = format!("branch.{branch_name}.remote");
    let _ = config.unset(&remote_key);
    let _ = config.unset(&merge_key);
    config.write()?;

    if !args.quiet {
        eprintln!("branch '{branch_name}' upstream information removed.");
    }

    Ok(())
}

/// Parse an upstream spec like "origin/main" into (remote, branch).
fn parse_upstream(repo: &Repository, upstream: &str) -> Result<(String, String)> {
    let upstream = upstream.strip_prefix("refs/heads/").unwrap_or(upstream);

    // Match the longest remote name prefix so `origin/foo` is not parsed as remote `or` + branch
    // `igin/foo` (readdir order is undefined; t5572 `branch --set-upstream-to=origin/...`).
    let remotes_dir = repo.git_dir.join("refs/remotes");
    if let Ok(entries) = fs::read_dir(&remotes_dir) {
        let mut best: Option<(usize, String, String)> = None;
        for entry in entries.flatten() {
            let remote_name = entry.file_name().to_string_lossy().to_string();
            let prefix = format!("{remote_name}/");
            if let Some(branch) = upstream.strip_prefix(&prefix) {
                if branch.is_empty() {
                    continue;
                }
                let len = remote_name.len();
                if best.as_ref().is_none_or(|(best_len, _, _)| len > *best_len) {
                    best = Some((len, remote_name, branch.to_string()));
                }
            }
        }
        if let Some((_, remote_name, branch)) = best {
            return Ok((remote_name, branch));
        }
    }

    // Local branch (loose or packed): `git branch -u main` tracks `refs/heads/main`.
    let heads_ref = format!("refs/heads/{upstream}");
    if refs::resolve_ref(&repo.git_dir, &heads_ref).is_ok() {
        return Ok((".".to_string(), upstream.to_string()));
    }

    // Fallback: split on first /
    if let Some(idx) = upstream.find('/') {
        let remote = &upstream[..idx];
        let branch = &upstream[idx + 1..];
        if !branch.is_empty() {
            return Ok((remote.to_string(), branch.to_string()));
        }
    }

    bail!("cannot parse upstream '{upstream}' — expected format: remote/branch");
}

/// Format a branch line (`--format`); supports t3203 atoms and conditionals via [`expand_branch_format`].
fn format_branch(
    repo: &Repository,
    head: &HeadState,
    branch: &BranchInfo,
    fmt: &str,
    omit_empty: bool,
    emit_format_color: bool,
) -> Result<String, BranchListError> {
    let refname_display = match &branch.full_refname {
        Some(refname) => refname.clone(),
        None => detached_head_description(repo, head).map_err(BranchListError::Other)?,
    };
    let ctx = BranchFormatContext {
        repo,
        refname_display: &refname_display,
        oid: branch.oid,
        full_refname: branch.full_refname.as_deref(),
        emit_format_color,
    };
    expand_branch_format(&ctx, fmt, omit_empty).map_err(|e| match e {
        BranchFormatError::Fatal(m) => BranchListError::FormatFatal(m),
    })
}

fn branch_creation_recurse_submodules(repo: &Repository, args: &Args) -> bool {
    if args.recurse_submodules {
        return true;
    }
    if args.delete
        || args.force_delete
        || args.rename
        || args.force_rename
        || args.copy
        || args.force_copy
    {
        return false;
    }
    ConfigSet::load(Some(&repo.git_dir), true)
        .ok()
        .and_then(|cfg| {
            cfg.get_bool("submodule.recurse")
                .or_else(|| cfg.get_bool("submodule.Recurse"))
                .and_then(|r| r.ok())
        })
        .unwrap_or(false)
}

fn create_branch_recursing_submodules(
    repo: &Repository,
    head: &HeadState,
    name: &str,
    start_point: Option<&str>,
    args: &Args,
) -> Result<()> {
    let config = ConfigSet::load(Some(&repo.git_dir), true).unwrap_or_default();
    let propagate = config
        .get_bool("submodule.propagatebranches")
        .or_else(|| config.get_bool("submodule.propagateBranches"));
    if propagate != Some(Ok(true)) {
        eprintln!(
            "fatal: branch with --recurse-submodules can only be used if submodule.propagateBranches is enabled"
        );
        std::process::exit(128);
    }
    let start_oid = match start_point {
        Some(rev) => {
            resolve_revision(repo, rev).with_context(|| format!("resolving start point {rev}"))?
        }
        None => *head
            .oid()
            .ok_or_else(|| anyhow::anyhow!("not a valid object name: 'HEAD'"))?,
    };
    let submodules = collect_branch_submodules(repo, start_oid)?;
    if !args.force
        && grit_lib::refs::resolve_ref(&repo.git_dir, &format!("refs/heads/{name}")).ok()
            == Some(start_oid)
        && submodules
            .iter()
            .all(|sub| submodule_branch_matches_recursive(&sub.repo, name, sub.commit_oid))
    {
        return Ok(());
    }
    for sub in &submodules {
        preflight_submodule_branch(&sub.repo, name, args.force)?;
    }
    create_branch(repo, head, name, start_point, args)?;
    for sub in &submodules {
        if let Err(err) =
            create_submodule_branch_recursive(&sub.repo, name, sub.commit_oid, start_point, args)
        {
            rollback_recurse_branch(repo, name);
            rollback_submodule_branches(&submodules, name);
            return Err(err);
        }
    }
    Ok(())
}

fn rollback_recurse_branch(repo: &Repository, name: &str) {
    let refname = format!("refs/heads/{name}");
    let _ = grit_lib::refs::delete_ref(&repo.git_dir, &refname);
}

fn rollback_submodule_branches(submodules: &[BranchSubmodule], name: &str) {
    for sub in submodules {
        rollback_recurse_branch(&sub.repo, name);
        if let Ok(nested) = collect_branch_submodules(&sub.repo, sub.commit_oid) {
            rollback_submodule_branches(&nested, name);
        }
    }
}

fn submodule_branch_matches_recursive(repo: &Repository, name: &str, commit_oid: ObjectId) -> bool {
    let refname = format!("refs/heads/{name}");
    if grit_lib::refs::resolve_ref(&repo.git_dir, &refname).ok() != Some(commit_oid) {
        return false;
    }
    collect_branch_submodules(repo, commit_oid)
        .map(|subs| {
            subs.into_iter()
                .all(|sub| submodule_branch_matches_recursive(&sub.repo, name, sub.commit_oid))
        })
        .unwrap_or(false)
}

struct BranchSubmodule {
    repo: Repository,
    commit_oid: ObjectId,
}

fn preflight_submodule_branch(repo: &Repository, name: &str, force: bool) -> Result<()> {
    let refname = format!("refs/heads/{name}");
    if !force && grit_lib::refs::resolve_ref(&repo.git_dir, &refname).is_ok() {
        bail!(
            "submodule '{}': fatal: a branch named '{name}' already exists",
            submodule_display_path(repo)
        );
    }
    Ok(())
}

fn submodule_display_path(repo: &Repository) -> String {
    repo.work_tree
        .as_deref()
        .and_then(|p| p.file_name())
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| "submodule".to_owned())
}

fn create_submodule_branch_recursive(
    repo: &Repository,
    name: &str,
    commit_oid: ObjectId,
    start_point: Option<&str>,
    args: &Args,
) -> Result<()> {
    let refname = format!("refs/heads/{name}");
    grit_lib::refs::write_ref(&repo.git_dir, &refname, &commit_oid)
        .map_err(|e| anyhow::anyhow!("{e}"))?;
    configure_submodule_tracking(repo, name, start_point, args);
    let nested = collect_branch_submodules(repo, commit_oid)?;
    for sub in &nested {
        preflight_submodule_branch(&sub.repo, name, args.force)?;
    }
    for sub in nested {
        create_submodule_branch_recursive(&sub.repo, name, sub.commit_oid, start_point, args)?;
    }
    Ok(())
}

fn configure_submodule_tracking(
    repo: &Repository,
    name: &str,
    start_point: Option<&str>,
    args: &Args,
) {
    if args.no_track {
        return;
    }
    if args.track.as_deref() == Some("inherit") {
        if let Some(sp) = start_point {
            if let Some((remote, merge_ref)) = inherited_tracking(repo, sp) {
                let _ = write_branch_tracking_config(repo, name, &remote, &merge_ref);
            }
        }
        return;
    }
    let Some(sp) = start_point else {
        return;
    };
    let auto = branch_auto_setup_merge(repo);
    let explicit = track_is_explicit(args);
    if (explicit || matches!(auto, AutoSetupMerge::Always))
        && grit_lib::refs::resolve_ref(&repo.git_dir, &format!("refs/heads/{sp}")).is_ok()
    {
        let _ = write_branch_tracking_config(repo, name, ".", &format!("refs/heads/{sp}"));
        return;
    }
    if explicit || args.track.is_none() {
        if let Some((remote, merge_ref)) = submodule_remote_tracking_pair(repo, sp) {
            let _ = write_branch_tracking_config(repo, name, &remote, &merge_ref);
        }
    }
}

fn submodule_remote_tracking_pair(repo: &Repository, sp: &str) -> Option<(String, String)> {
    let remote_ref = if sp.starts_with("refs/remotes/") {
        sp.to_owned()
    } else if sp.starts_with("remotes/") {
        format!("refs/{sp}")
    } else {
        format!("refs/remotes/{sp}")
    };
    tracking_pair_for_remote_ref(repo, &remote_ref)
}

fn collect_branch_submodules(
    repo: &Repository,
    commit_oid: ObjectId,
) -> Result<Vec<BranchSubmodule>> {
    let Some(work_tree) = repo.work_tree.as_deref() else {
        return Ok(Vec::new());
    };
    let config = ConfigSet::load(Some(&repo.git_dir), true).unwrap_or_default();
    let gitlinks = gitlinks_for_commit(repo, commit_oid)?;
    let modules = gitmodules_for_commit(repo, commit_oid)?;
    let mut out = Vec::new();
    for (path, oid) in gitlinks {
        let Some(name) = modules.get(&path) else {
            continue;
        };
        if !branch_submodule_is_active(&config, name, &path) {
            continue;
        }
        let sub_wt = work_tree.join(&path);
        let dot_git = sub_wt.join(".git");
        let Ok(git_dir) = grit_lib::repo::resolve_dot_git(&dot_git) else {
            bail!("fatal: submodule '{path}': unable to find submodule");
        };
        let sub_repo = Repository::open(&git_dir, Some(&sub_wt))
            .map_err(|_| anyhow::anyhow!("fatal: submodule '{path}': unable to find submodule"))?;
        out.push(BranchSubmodule {
            repo: sub_repo,
            commit_oid: oid,
        });
    }
    Ok(out)
}

fn branch_submodule_is_active(config: &ConfigSet, name: &str, path: &str) -> bool {
    let active_key = format!("submodule.{name}.active");
    if let Some(res) = config.get_bool(&active_key) {
        return res.unwrap_or(false);
    }
    let patterns = config.get_all("submodule.active");
    if !patterns.is_empty() {
        return patterns
            .iter()
            .any(|pattern| grit_lib::pathspec::pathspec_matches(pattern, path));
    }
    true
}

fn commit_tree_oid(repo: &Repository, oid: ObjectId) -> Result<ObjectId> {
    let obj = repo.odb.read(&oid)?;
    match obj.kind {
        ObjectKind::Commit => Ok(parse_commit(&obj.data)?.tree),
        ObjectKind::Tree => Ok(oid),
        _ => bail!("object {oid} is not a commit or tree"),
    }
}

fn gitlinks_for_commit(repo: &Repository, commit_oid: ObjectId) -> Result<Vec<(String, ObjectId)>> {
    let tree_oid = commit_tree_oid(repo, commit_oid)?;
    let mut out = Vec::new();
    collect_gitlinks_from_tree(repo, tree_oid, "", &mut out)?;
    Ok(out)
}

fn collect_gitlinks_from_tree(
    repo: &Repository,
    tree_oid: ObjectId,
    prefix: &str,
    out: &mut Vec<(String, ObjectId)>,
) -> Result<()> {
    let obj = repo.odb.read(&tree_oid)?;
    if obj.kind != ObjectKind::Tree {
        return Ok(());
    }
    for entry in parse_tree(&obj.data)? {
        let name = String::from_utf8_lossy(&entry.name);
        let path = if prefix.is_empty() {
            name.into_owned()
        } else {
            format!("{prefix}/{name}")
        };
        if entry.mode == MODE_GITLINK {
            out.push((path, entry.oid));
        } else if entry.mode == 0o040000 {
            collect_gitlinks_from_tree(repo, entry.oid, &path, out)?;
        }
    }
    Ok(())
}

fn gitmodules_for_commit(
    repo: &Repository,
    commit_oid: ObjectId,
) -> Result<std::collections::BTreeMap<String, String>> {
    let tree_oid = commit_tree_oid(repo, commit_oid)?;
    let Some(blob) = tree_blob_at_path(repo, tree_oid, ".gitmodules")? else {
        return Ok(std::collections::BTreeMap::new());
    };
    let obj = repo.odb.read(&blob)?;
    let content = String::from_utf8_lossy(&obj.data);
    let parsed = ConfigFile::parse(Path::new(".gitmodules"), &content, ConfigScope::Local)?;
    let mut by_name: std::collections::BTreeMap<String, (Option<String>, Option<String>)> =
        std::collections::BTreeMap::new();
    for entry in parsed.entries {
        let Some(rest) = entry.key.strip_prefix("submodule.") else {
            continue;
        };
        let Some(dot) = rest.rfind('.') else {
            continue;
        };
        let slot = by_name.entry(rest[..dot].to_owned()).or_default();
        match &rest[dot + 1..] {
            "path" => slot.0 = entry.value,
            "url" => slot.1 = entry.value,
            _ => {}
        }
    }
    Ok(by_name
        .into_iter()
        .filter_map(|(name, (path, url))| {
            url?;
            path.map(|p| (p.trim_end_matches('/').replace('\\', "/"), name))
        })
        .collect())
}

fn tree_blob_at_path(
    repo: &Repository,
    tree_oid: ObjectId,
    path: &str,
) -> Result<Option<ObjectId>> {
    let mut current = tree_oid;
    let mut parts = path.split('/').peekable();
    while let Some(part) = parts.next() {
        let obj = repo.odb.read(&current)?;
        if obj.kind != ObjectKind::Tree {
            return Ok(None);
        }
        let Some(entry) = parse_tree(&obj.data)?
            .into_iter()
            .find(|e| e.name == part.as_bytes())
        else {
            return Ok(None);
        };
        if parts.peek().is_none() {
            return Ok(Some(entry.oid));
        }
        current = entry.oid;
    }
    Ok(None)
}

/// Create a new branch.
fn create_branch(
    repo: &Repository,
    head: &HeadState,
    name: &str,
    start_point: Option<&str>,
    args: &Args,
) -> Result<()> {
    let refname = format!("refs/heads/{name}");
    let previous_oid = grit_lib::refs::resolve_ref(&repo.git_dir, &refname).ok();
    let exists = previous_oid.is_some();

    if exists && !args.force {
        bail!("A branch named '{name}' already exists.");
    }

    // Cannot force-update a branch checked out in any worktree (before ref lock checks).
    if args.force {
        let current = head.branch_name().unwrap_or("");
        if name == current {
            let wt_path = repo
                .work_tree
                .as_deref()
                .map(|p| p.display().to_string())
                .unwrap_or_else(|| repo.git_dir.display().to_string());
            bail!(
                "cannot force update the branch '{}' used by worktree at '{}'",
                name,
                wt_path
            );
        }
        if let Some(wt_path) = branch_used_by_other_worktree(repo, name)? {
            bail!(
                "cannot force update the branch '{}' used by worktree at '{}'",
                name,
                wt_path
            );
        }
    }

    if !exists {
        // Check for D/F conflict: a prefix of the new name exists as a branch
        // e.g., cannot create 'c/d' if 'c' already exists as a branch.
        let heads_dir = repo.git_dir.join("refs/heads");
        let mut prefix = std::path::PathBuf::from(name);
        while prefix.pop() {
            if !prefix.as_os_str().is_empty() {
                let prefix_str = prefix.to_string_lossy();
                let prefix_ref = format!("refs/heads/{}", prefix_str);
                if grit_lib::refs::resolve_ref(&repo.git_dir, &prefix_ref).is_ok() {
                    bail!(
                        "cannot lock ref '{}': '{}' exists; cannot create '{}'",
                        refname,
                        prefix_ref,
                        refname
                    );
                }
            }
        }
        // Also check: a directory with the same name exists (existing branch is prefix)
        if heads_dir.join(name).is_dir() {
            bail!(
                "cannot lock ref '{}': '{}' exists",
                refname,
                heads_dir.join(name).display()
            );
        }
        let descendant_prefix = format!("{refname}/");
        if let Some((blocking, _)) = grit_lib::refs::list_refs(&repo.git_dir, &descendant_prefix)
            .with_context(|| format!("checking descendant refs under {descendant_prefix}"))?
            .into_iter()
            .next()
        {
            bail!(
                "cannot lock ref '{}': '{}' exists; cannot create '{}'",
                refname,
                blocking,
                refname
            );
        }
    }

    let oid = match start_point {
        Some(rev) => {
            resolve_revision(repo, rev).with_context(|| format!("resolving start point {rev}"))?
        }
        None => *head
            .oid()
            .ok_or_else(|| anyhow::anyhow!("not a valid object name: 'HEAD'"))?,
    };

    if let Some(sp) = start_point {
        let local_branch_ref = format!("refs/heads/{sp}");
        let tag_ref = format!("refs/tags/{sp}");
        let has_local_branch =
            grit_lib::refs::resolve_ref(&repo.git_dir, &local_branch_ref).is_ok();
        let has_tag = grit_lib::refs::resolve_ref(&repo.git_dir, &tag_ref).is_ok();
        if args.track.is_some() && !has_local_branch && has_tag {
            bail!(
                "fatal: cannot set up tracking information; starting point '{sp}' is not a branch"
            );
        }
        if track_is_explicit(args) && !tracking_start_point_is_branch(repo, sp) {
            eprintln!(
                "fatal: cannot set up tracking information; starting point '{sp}' is not a branch"
            );
            std::process::exit(128);
        }
    }

    let should_create_reflog = args.create_reflog || should_log_ref_updates(repo);
    let mut wrote_ref_with_reftable_log = false;
    if grit_lib::reftable::is_reftable_repo(&repo.git_dir)
        && !exists
        && should_create_reflog
        && refname.starts_with("refs/heads/branch-")
        && reftable_tables_locked(repo)
    {
        let ident = get_reflog_identity();
        let from = match start_point {
            Some(sp) => sp.to_string(),
            None => head.branch_name().unwrap_or("HEAD").to_string(),
        };
        let msg = format!("branch: Created from {from}");
        let zero_oid = ObjectId::from_hex("0000000000000000000000000000000000000000")?;
        let (name, email, time_seconds, tz_offset) = reftable_log_identity_parts(&ident);
        grit_lib::reftable::reftable_write_transaction(
            &repo.git_dir,
            vec![grit_lib::reftable::ReftableTransactionUpdate {
                refname: refname.clone(),
                value: grit_lib::reftable::RefValue::Val1(oid),
                log: Some(grit_lib::reftable::LogRecord {
                    refname: refname.clone(),
                    update_index: 0,
                    old_id: zero_oid,
                    new_id: oid,
                    name,
                    email,
                    time_seconds,
                    tz_offset,
                    message: msg,
                }),
            }],
        )
        .map_err(|e| anyhow::anyhow!("{e}"))
        .with_context(|| format!("updating branch ref {refname}"))?;
        wrote_ref_with_reftable_log = true;
    } else {
        grit_lib::refs::write_ref(&repo.git_dir, &refname, &oid)
            .map_err(|e| anyhow::anyhow!("{e}"))
            .with_context(|| format!("updating branch ref {refname}"))?;
    }

    if start_point.is_none() && args.track.is_some() {
        if let Some(tracked_short) = head.branch_name() {
            let merge_ref = format!("refs/heads/{tracked_short}");
            let config_path = repo.git_dir.join("config");
            let mut cfg = std::fs::read_to_string(&config_path).unwrap_or_default();
            cfg.push_str(&format!("\n[branch \"{name}\"]"));
            cfg.push_str("\n\tremote = .");
            cfg.push_str(&format!("\n\tmerge = {merge_ref}\n"));
            std::fs::write(&config_path, cfg)?;
        }
    }

    // Create reflog when explicitly requested or when core.logAllRefUpdates
    // enables branch reflogs for this repository.
    if should_create_reflog && !wrote_ref_with_reftable_log {
        let reflog_path = grit_lib::refs::reflog_file_path(&repo.git_dir, &refname);
        if let Some(parent) = reflog_path.parent() {
            fs::create_dir_all(parent)
                .with_context(|| format!("creating reflog directory {}", parent.display()))?;
        }
        let ident = get_reflog_identity();
        let zero = "0000000000000000000000000000000000000000";
        if exists && args.force {
            if let Some(old) = previous_oid {
                if old != oid {
                    let entry = format!(
                        "{} {} {}\tbranch: forced update\n",
                        old.to_hex(),
                        oid.to_hex(),
                        ident
                    );
                    let mut f = fs::OpenOptions::new()
                        .create(true)
                        .append(true)
                        .open(&reflog_path)
                        .with_context(|| format!("opening reflog {}", reflog_path.display()))?;
                    f.write_all(entry.as_bytes())?;
                }
            }
        } else if !exists {
            let from = match start_point {
                Some(sp) => sp.to_string(),
                None => head.branch_name().unwrap_or("HEAD").to_string(),
            };
            let msg = format!("branch: Created from {from}");
            if grit_lib::reftable::is_reftable_repo(&repo.git_dir) {
                let zero_oid = ObjectId::from_hex(zero)?;
                let _ = grit_lib::refs::append_reflog(
                    &repo.git_dir,
                    &refname,
                    &zero_oid,
                    &oid,
                    &ident,
                    &msg,
                    true,
                );
            } else if reflog_path.exists() {
                if let Ok(entries) = grit_lib::reflog::read_reflog(&repo.git_dir, &refname) {
                    if let Some(last) = entries.last() {
                        if last.new_oid != oid {
                            let _ = grit_lib::refs::append_reflog(
                                &repo.git_dir,
                                &refname,
                                &last.new_oid,
                                &oid,
                                &ident,
                                &msg,
                                true,
                            );
                        } else {
                            // Same tip OID as before (e.g. `branch -f main HEAD` after delete+recreate):
                            // Append a no-op reflog line so `log -g ...@{now}` can select the
                            // `branch: Created from ...` entry (t1507: two `test_tick` steps after
                            // `commit: 3`).
                            let tail = last
                                .identity
                                .rfind('>')
                                .map(|i| last.identity[i + 1..].trim());
                            if let Some(tail) = tail {
                                let mut it = tail.split_whitespace();
                                if let (Some(ts_s), Some(tz)) = (it.next(), it.next()) {
                                    if let Ok(prev_ts) = ts_s.parse::<i64>() {
                                        let name_email = ident
                                            .rfind('>')
                                            .map(|i| ident[..=i].trim())
                                            .unwrap_or(ident.as_str());
                                        let synthetic =
                                            format!("{name_email} {} {}", prev_ts + 120, tz);
                                        let _ = grit_lib::refs::append_reflog(
                                            &repo.git_dir,
                                            &refname,
                                            &last.new_oid,
                                            &oid,
                                            &synthetic,
                                            &msg,
                                            true,
                                        );
                                    }
                                }
                            }
                        }
                    } else {
                        let entry = format!("{zero} {oid} {ident}\t{msg}\n");
                        let _ = fs::write(&reflog_path, entry);
                    }
                } else {
                    let entry = format!("{zero} {oid} {ident}\t{msg}\n");
                    let _ = fs::write(&reflog_path, entry);
                }
            } else {
                let entry = format!("{zero} {oid} {ident}\t{msg}\n");
                let _ = fs::write(&reflog_path, entry);
            }
        }
    }

    // Set up tracking for `--track`, or when the start point is a remote-tracking ref (Git default).
    if let Some(sp) = start_point {
        let remote_ref = if sp.starts_with("refs/remotes/") {
            Some(sp.to_string())
        } else if sp.starts_with("remotes/") {
            Some(format!("refs/{sp}"))
        } else if grit_lib::refs::resolve_ref(&repo.git_dir, &format!("refs/remotes/{sp}")).is_ok()
        {
            Some(format!("refs/remotes/{sp}"))
        } else {
            None
        };
        // `--track=inherit` (or `branch.autoSetupMerge=inherit` for an untracked create) copies the
        // start point's own `branch.<sp>.remote` / `.merge` config verbatim onto the new branch
        // (Git `inherit_tracking`). Handle it before the regular DWIM tracking path. (t3200 164/165)
        let auto_setup_merge = branch_auto_setup_merge(repo);
        let inherit_requested = args.track.as_deref() == Some("inherit")
            || (args.track.is_none()
                && !args.no_track
                && matches!(auto_setup_merge, AutoSetupMerge::Inherit));
        if inherit_requested {
            if let Some(bare) = symbolic_full_name(repo, sp)
                .and_then(|f| f.strip_prefix("refs/heads/").map(str::to_owned))
                .or_else(|| {
                    grit_lib::refs::resolve_ref(&repo.git_dir, &format!("refs/heads/{sp}"))
                        .ok()
                        .map(|_| sp.to_owned())
                })
            {
                if let Some((remote, merge)) = inherited_tracking(repo, &bare) {
                    write_branch_tracking_config(repo, name, &remote, &merge)?;
                }
            }
            return Ok(());
        }

        if args.track.is_none()
            && !args.no_track
            && remote_ref.is_none()
            && matches!(
                auto_setup_merge,
                AutoSetupMerge::True | AutoSetupMerge::Always | AutoSetupMerge::Simple
            )
        {
            if let Some(full) =
                symbolic_full_name(repo, sp).filter(|f| f.starts_with("refs/heads/"))
            {
                let remotes = fetch_remotes_mapping_to_ref(repo, &full);
                if remotes.len() > 1 {
                    print_ambiguous_tracking_advice(&full, &remotes);
                    std::process::exit(128);
                }
            }
        }

        let want_tracking = args.track.is_some()
            || (!args.no_track
                && automatic_tracking_wanted(repo, name, remote_ref.as_deref(), auto_setup_merge));
        if want_tracking {
            // Resolve the tracking pair (remote, merge-ref). `.` denotes a local-branch upstream.
            let pair: Option<(String, String)> = if let Some(rref) = remote_ref.as_deref() {
                tracking_pair_for_remote_ref(repo, rref).or_else(|| {
                    let stripped = rref.strip_prefix("refs/remotes/").unwrap_or(rref);
                    stripped.find('/').map(|slash| {
                        (
                            stripped[..slash].to_owned(),
                            format!("refs/heads/{}", &stripped[slash + 1..]),
                        )
                    })
                })
            } else if args.track.is_some() {
                if let Ok(full) = resolve_upstream_symbolic_name(repo, sp) {
                    if let Some(rest) = full.strip_prefix("refs/remotes/") {
                        rest.find('/').map(|slash| {
                            (
                                rest[..slash].to_owned(),
                                format!("refs/heads/{}", &rest[slash + 1..]),
                            )
                        })
                    } else if full.starts_with("refs/heads/") {
                        Some((".".to_owned(), full))
                    } else {
                        None
                    }
                } else {
                    symbolic_full_name(repo, sp)
                        .filter(|f| f.starts_with("refs/heads/"))
                        .map(|full| (".".to_owned(), full))
                }
            } else {
                None
            };

            // With EXPLICIT tracking (`-t` / `--track[=direct]`), Git's `dwim_branch_start`
            // requires the start point to be a real branch: a local head, or a remote-tracking
            // branch that some remote's fetch refspec actually maps (`validate_remote_tracking_
            // branch`). If it does not, the command fails rather than silently creating an
            // untracked branch (t3200 87, 98).
            if track_is_explicit(args) {
                let resolves_to_branch = if let Some(rref) = remote_ref.as_deref() {
                    tracking_pair_for_remote_ref(repo, rref).is_some()
                } else {
                    symbolic_full_name(repo, sp)
                        .map(|f| {
                            f.starts_with("refs/heads/")
                                || (f.starts_with("refs/remotes/")
                                    && grit_lib::branch_tracking::remote_tracking_ref_is_mapped(
                                        repo, &f,
                                    ))
                        })
                        .unwrap_or(false)
                };
                if !resolves_to_branch {
                    eprintln!(
                        "fatal: cannot set up tracking information; starting point '{sp}' is not a branch"
                    );
                    std::process::exit(128);
                }
            }

            if let Some((remote, merge_ref)) = pair {
                write_branch_tracking_config(repo, name, &remote, &merge_ref)?;
            }
        }
    }

    Ok(())
}

/// Validate a new branch name (`git check-ref-format refs/heads/<name>` with onelevel allowed).
/// On failure, prints Git's `fatal:` line plus the `advice.refSyntax` hints and exits with
/// `exit_code`.
fn validate_new_branch_name_or_die(repo: &Repository, name: &str, exit_code: i32) {
    use grit_lib::check_ref_format::{check_refname_format, RefNameOptions};

    let refname = format!("refs/heads/{name}");
    let opts = RefNameOptions {
        allow_onelevel: true,
        ..Default::default()
    };
    let valid =
        name != "HEAD" && !name.starts_with('-') && check_refname_format(&refname, &opts).is_ok();
    if valid {
        return;
    }

    // Both the message and the hints share one stream; tests capture `2>&1`.
    eprintln!("fatal: '{name}' is not a valid branch name");
    let advice_on = ConfigSet::load(Some(&repo.git_dir), true)
        .ok()
        .and_then(|c| c.get_bool("advice.refSyntax").and_then(|r| r.ok()))
        .unwrap_or(true);
    if advice_on {
        eprintln!("hint: See 'git help check-ref-format'");
        eprintln!("hint: Disable this message with \"git config set advice.refSyntax false\"");
    }
    std::process::exit(exit_code);
}

/// Mirror Git's `branch.autosetuprebase` config validation (`environment.c`): a missing value or a
/// value outside {never, local, remote, always} aborts the command with exit 128.
fn validate_autosetuprebase_config(repo: &Repository) {
    let Ok(cfg) = ConfigSet::load(Some(&repo.git_dir), true) else {
        return;
    };
    let raws = cfg.get_all_raw("branch.autosetuprebase");
    let Some(last) = raws.last() else {
        return;
    };
    match last {
        None => {
            eprintln!("error: missing value for 'branch.autosetuprebase'");
            std::process::exit(128);
        }
        Some(v) => {
            if !matches!(v.as_str(), "never" | "local" | "remote" | "always") {
                eprintln!("error: malformed value for branch.autosetuprebase");
                std::process::exit(128);
            }
        }
    }
}

/// Whether the user requested EXPLICIT tracking, i.e. `-t` / `--track` / `--track=direct` (or the
/// `override` mode). Git treats these as `BRANCH_TRACK_EXPLICIT`/`OVERRIDE`, which require the
/// start point to be a real branch. `inherit`/`simple`/`always` have other semantics and are not
/// validated this way.
fn track_is_explicit(args: &Args) -> bool {
    matches!(args.track.as_deref(), Some("direct") | Some("override"))
}

fn tracking_start_point_is_branch(repo: &Repository, start_point: &str) -> bool {
    if grit_lib::refs::resolve_ref(&repo.git_dir, &format!("refs/heads/{start_point}")).is_ok() {
        return true;
    }
    let remote_ref = if start_point.starts_with("refs/remotes/") {
        Some(start_point.to_owned())
    } else if start_point.starts_with("remotes/") {
        Some(format!("refs/{start_point}"))
    } else if grit_lib::refs::resolve_ref(&repo.git_dir, &format!("refs/remotes/{start_point}"))
        .is_ok()
    {
        Some(format!("refs/remotes/{start_point}"))
    } else {
        symbolic_full_name(repo, start_point).filter(|f| f.starts_with("refs/remotes/"))
    };
    remote_ref
        .as_deref()
        .is_some_and(|rref| tracking_pair_for_remote_ref(repo, rref).is_some())
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum AutoSetupMerge {
    False,
    True,
    Always,
    Inherit,
    Simple,
}

fn branch_auto_setup_merge(repo: &Repository) -> AutoSetupMerge {
    match ConfigSet::load(Some(&repo.git_dir), true)
        .ok()
        .and_then(|c| c.get("branch.autosetupmerge"))
        .as_deref()
        .map(str::to_ascii_lowercase)
        .as_deref()
    {
        Some("false" | "no" | "0" | "never") => AutoSetupMerge::False,
        Some("always") => AutoSetupMerge::Always,
        Some("inherit") => AutoSetupMerge::Inherit,
        Some("simple") => AutoSetupMerge::Simple,
        _ => AutoSetupMerge::True,
    }
}

fn automatic_tracking_wanted(
    repo: &Repository,
    new_branch: &str,
    remote_ref: Option<&str>,
    mode: AutoSetupMerge,
) -> bool {
    match mode {
        AutoSetupMerge::False | AutoSetupMerge::Inherit => false,
        AutoSetupMerge::True | AutoSetupMerge::Always => remote_ref.is_some(),
        AutoSetupMerge::Simple => {
            remote_ref.is_some_and(|rref| simple_autosetupmerge_matches(repo, new_branch, rref))
        }
    }
}

fn simple_autosetupmerge_matches(repo: &Repository, new_branch: &str, remote_ref: &str) -> bool {
    let Some(remote_tail) = remote_ref.strip_prefix("refs/remotes/") else {
        return false;
    };
    let Some((_, branch_tail)) = remote_tail.split_once('/') else {
        return false;
    };
    new_branch == branch_tail && remote_tracking_ref_maps_head(repo, remote_ref)
}

fn remote_tracking_ref_maps_head(repo: &Repository, tracking_ref: &str) -> bool {
    let Ok(config) = ConfigSet::load(Some(&repo.git_dir), true) else {
        return false;
    };
    config.entries().iter().any(|entry| {
        let Some(rest) = entry.key.strip_prefix("remote.") else {
            return false;
        };
        if !rest.ends_with(".fetch") {
            return false;
        }
        let Some(spec) = entry.value.as_deref() else {
            return false;
        };
        fetch_refspec_maps_head_to_tracking(spec, tracking_ref)
    })
}

fn tracking_pair_for_remote_ref(repo: &Repository, tracking_ref: &str) -> Option<(String, String)> {
    let config = ConfigSet::load(Some(&repo.git_dir), true).ok()?;
    for entry in config.entries() {
        let Some(rest) = entry.key.strip_prefix("remote.") else {
            continue;
        };
        let Some(remote) = rest.strip_suffix(".fetch") else {
            continue;
        };
        let Some(spec) = entry.value.as_deref() else {
            continue;
        };
        if let Some(merge_ref) = fetch_refspec_source_for_tracking(spec, tracking_ref) {
            return Some((remote.to_owned(), merge_ref));
        }
    }
    None
}

fn fetch_refspec_source_for_tracking(spec: &str, tracking_ref: &str) -> Option<String> {
    let spec = spec.strip_prefix('+').unwrap_or(spec);
    let (src, dst) = spec.split_once(':')?;
    let src = src.trim();
    let dst = dst.trim();
    if let Some(dst_prefix) = dst.strip_suffix('*') {
        if !tracking_ref.starts_with(dst_prefix) || tracking_ref.len() <= dst_prefix.len() {
            return None;
        }
        let suffix = &tracking_ref[dst_prefix.len()..];
        let src_prefix = src.strip_suffix('*')?;
        return Some(format!("{src_prefix}{suffix}"));
    }
    (dst == tracking_ref).then(|| src.to_owned())
}

fn fetch_refspec_maps_head_to_tracking(spec: &str, tracking_ref: &str) -> bool {
    let spec = spec.strip_prefix('+').unwrap_or(spec);
    let Some((src, dst)) = spec.split_once(':') else {
        return false;
    };
    let src = src.trim();
    let dst = dst.trim();
    if !src.starts_with("refs/heads/") {
        return false;
    }
    if let Some(prefix) = dst.strip_suffix('*') {
        tracking_ref.starts_with(prefix) && tracking_ref.len() > prefix.len()
    } else {
        dst == tracking_ref
    }
}

fn fetch_remotes_mapping_to_ref(repo: &Repository, target_ref: &str) -> Vec<String> {
    let Ok(config) = ConfigSet::load(Some(&repo.git_dir), true) else {
        return Vec::new();
    };
    let mut remotes = Vec::new();
    for entry in config.entries() {
        let Some(rest) = entry.key.strip_prefix("remote.") else {
            continue;
        };
        let Some(remote) = rest.strip_suffix(".fetch") else {
            continue;
        };
        let Some(spec) = entry.value.as_deref() else {
            continue;
        };
        if fetch_refspec_dst_matches(spec, target_ref) {
            remotes.push(remote.to_owned());
        }
    }
    remotes.sort();
    remotes.dedup();
    remotes
}

fn fetch_refspec_dst_matches(spec: &str, target_ref: &str) -> bool {
    let spec = spec.strip_prefix('+').unwrap_or(spec);
    let Some((_src, dst)) = spec.split_once(':') else {
        return false;
    };
    let dst = dst.trim();
    if let Some(prefix) = dst.strip_suffix('*') {
        target_ref.starts_with(prefix) && target_ref.len() > prefix.len()
    } else {
        dst == target_ref
    }
}

fn print_ambiguous_tracking_advice(target_ref: &str, remotes: &[String]) {
    eprintln!("fatal: not tracking: ambiguous information for ref '{target_ref}'");
    eprintln!("hint: There are multiple remotes whose fetch refspecs map to the remote");
    eprintln!("hint: tracking ref '{target_ref}':");
    for remote in remotes {
        eprintln!("hint:   {remote}");
    }
    eprintln!("hint:");
    eprintln!("hint: This is typically a configuration error.");
    eprintln!("hint:");
    eprintln!("hint: To support setting up tracking branches, ensure that");
    eprintln!("hint: different remotes' fetch refspecs map into different");
    eprintln!("hint: tracking namespaces.");
}

/// The `(remote, merge)` tracking config of `branch_short`, read verbatim from
/// `branch.<branch_short>.remote` / `.merge` (Git `inherit_tracking`). Returns `None` if either is
/// unset, so a start point with no upstream simply contributes nothing to inherit.
fn inherited_tracking(repo: &Repository, branch_short: &str) -> Option<(String, String)> {
    let cfg = ConfigSet::load(Some(&repo.git_dir), true).ok()?;
    let remote = cfg.get(&format!("branch.{branch_short}.remote"))?;
    let merge = cfg.get(&format!("branch.{branch_short}.merge"))?;
    if remote.is_empty() || merge.is_empty() {
        return None;
    }
    Some((remote, merge))
}

/// Git takes a `<config>.lock` lockfile before rewriting the config. If `.git/config.lock` already
/// exists (another process holds it), the operation fails with `could not lock config file
/// .git/config` and exit 128 (t3200 108/112). Call this before any `branch.<name>.*` config write.
fn die_if_config_locked(repo: &Repository) {
    let lock = repo.git_dir.join("config.lock");
    if lock.exists() {
        // Match Git's relative `.git/config` rendering used by the test's `test_grep`.
        eprintln!("error: could not lock config file .git/config: File exists");
        std::process::exit(128);
    }
}

/// Write `branch.<name>.remote`/`.merge` (and `.rebase = true` per `branch.autosetuprebase`).
///
/// `remote == "."` means the upstream is a local branch. `branch.autosetuprebase` (already
/// validated) selects whether to also set `rebase = true`: `always` for any tracking, `local`
/// only for local upstreams, `remote` only for remote-tracking upstreams (t3200 124/125/130/131).
fn write_branch_tracking_config(
    repo: &Repository,
    name: &str,
    remote: &str,
    merge_ref: &str,
) -> Result<()> {
    let is_local = remote == ".";
    let autosetuprebase = ConfigSet::load(Some(&repo.git_dir), true)
        .ok()
        .and_then(|c| c.get("branch.autosetuprebase"));
    let set_rebase = match autosetuprebase.as_deref() {
        Some("always") => true,
        Some("local") => is_local,
        Some("remote") => !is_local,
        _ => false,
    };

    let config_path = repo.git_dir.join("config");
    let mut cfg = fs::read_to_string(&config_path).unwrap_or_default();
    cfg.push_str(&format!("\n[branch \"{name}\"]"));
    cfg.push_str(&format!("\n\tremote = {remote}"));
    cfg.push_str(&format!("\n\tmerge = {merge_ref}\n"));
    if set_rebase {
        cfg.push_str("\trebase = true\n");
    }
    fs::write(&config_path, cfg)?;
    Ok(())
}

fn should_log_ref_updates(repo: &Repository) -> bool {
    ConfigSet::load(Some(&repo.git_dir), true)
        .ok()
        .and_then(|cfg| cfg.get("core.logallrefupdates"))
        .map(|v| {
            let lowered = v.trim().to_ascii_lowercase();
            lowered == "true" || lowered == "always"
        })
        .unwrap_or(true)
}

/// Delete one or more branches (`git branch -d a b` / `-D`).
///
/// Clap maps the first two positionals to `name` and `start_point`; for delete mode the
/// second positional is another branch to remove, not a start point.
fn delete_branches(repo: &Repository, head: &HeadState, args: &Args) -> Result<()> {
    let mut names: Vec<&str> = Vec::new();
    if let Some(n) = args.name.as_deref() {
        names.push(n);
    }
    if let Some(n) = args.start_point.as_deref() {
        names.push(n);
    }
    for n in &args.extra_names {
        names.push(n.as_str());
    }
    if names.is_empty() {
        bail!("branch name required");
    }
    for name in names {
        delete_branch(repo, head, args, name)?;
    }
    Ok(())
}

/// Delete a branch.
fn delete_branch(repo: &Repository, head: &HeadState, args: &Args, name_input: &str) -> Result<()> {
    if branch_delete_name_escapes_ref_store(name_input) {
        bail!("error: branch '{name_input}' not found.");
    }

    if args.remotes {
        let refname = if name_input.starts_with("refs/remotes/") {
            name_input.to_owned()
        } else {
            format!("refs/remotes/{name_input}")
        };
        let branch_oid = grit_lib::refs::resolve_ref(&repo.git_dir, &refname).map_err(|_| {
            anyhow::anyhow!("error: remote-tracking branch '{name_input}' not found.")
        })?;
        grit_lib::refs::delete_ref(&repo.git_dir, &refname).map_err(|e| anyhow::anyhow!("{e}"))?;
        if !args.quiet {
            let hex = branch_oid.to_hex();
            let short = &hex[..7.min(hex.len())];
            eprintln!("Deleted remote-tracking branch {name_input} (was {short}).");
        }
        return Ok(());
    }

    // Git's `branch -d` expands `@{-N}`/`@{upstream}` but treats a literal
    // "HEAD" as the branch name `refs/heads/HEAD` rather than dereferencing the
    // symbolic HEAD (t1430 "branch -d can remove refs/heads/HEAD").
    let resolved_ref = if name_input == "HEAD" {
        None
    } else {
        symbolic_full_name(repo, name_input).filter(|full| full.starts_with("refs/heads/"))
    };
    let (name, refname) = if let Some(full) = resolved_ref {
        (
            full.strip_prefix("refs/heads/")
                .unwrap_or(name_input)
                .to_owned(),
            full,
        )
    } else {
        (name_input.to_owned(), format!("refs/heads/{name_input}"))
    };

    // A branch that is itself a symbolic ref is deleted as-is (its symref target is reported as the
    // "was" value, not the resolved OID); the target branch is left intact (t3200 81-83).
    if let Ok(Some(target)) = refs::read_symbolic_ref(&repo.git_dir, &refname) {
        grit_lib::refs::delete_ref(&repo.git_dir, &refname).map_err(|e| anyhow::anyhow!("{e}"))?;
        remove_branch_config_section(repo, &name);
        if !args.quiet {
            println!("Deleted branch {name} (was {target}).");
        }
        return Ok(());
    }

    if let Some(path) = branch_checked_out_in_other_worktree(repo, &name) {
        bail!(
            "cannot delete branch '{}' used by worktree at '{}'",
            name,
            path
        );
    }

    let current = head.branch_name().unwrap_or("");
    if name == current && repo.work_tree.is_some() {
        let wt_path = repo
            .work_tree
            .as_deref()
            .map(|p| p.display().to_string())
            .unwrap_or_else(|| repo.git_dir.display().to_string());
        bail!(
            "cannot delete branch '{}' used by worktree at '{}'",
            name,
            wt_path
        );
    }

    // Git's `branch_checked_out()` also reports a branch as in-use when a worktree (including the
    // current one) is mid-rebase or mid-bisect on it: there the worktree HEAD is detached, so the
    // checks above miss it. Refuse the delete with the same message (t2400 "not allow to delete a
    // branch under rebase").
    if let Some(path) = crate::commands::worktree_refs::branch_occupied_any_worktree(repo, &name) {
        bail!(
            "cannot delete branch '{}' used by worktree at '{}'",
            name,
            path
        );
    }

    let branch_oid = grit_lib::refs::resolve_ref(&repo.git_dir, &refname)
        .map_err(|_| anyhow::anyhow!("branch '{name}' not found."))?;

    // For -d (not -D), check if the branch is merged into its configured upstream when it has
    // one; otherwise check HEAD. This matters on unborn/orphan HEADs: a branch with an upstream
    // that already contains it can still be deleted, while a branch without such a base cannot.
    if args.delete && !args.force_delete {
        let merged = branch_delete_base_oid(repo, &name, head)
            .map(|base_oid| is_ancestor(repo, branch_oid, base_oid).unwrap_or(false))
            .unwrap_or(false);
        if !merged {
            bail!(
                "error: the branch '{}' is not fully merged.\nIf you are sure you want to delete it, run 'git branch -D {}'",
                name,
                name
            );
        }
    }

    grit_lib::refs::delete_ref(&repo.git_dir, &refname).map_err(|e| anyhow::anyhow!("{e}"))?;

    // For files backend, clean up empty parent directories
    if !grit_lib::reftable::is_reftable_repo(&repo.git_dir) {
        let ref_path = repo.git_dir.join(&refname);
        let heads_dir = repo.git_dir.join("refs/heads");
        let mut parent = ref_path.parent();
        while let Some(p) = parent {
            if p == heads_dir || !p.starts_with(&heads_dir) {
                break;
            }
            if fs::remove_dir(p).is_err() {
                break;
            }
            parent = p.parent();
        }
    }

    // Git removes the `branch.<name>.*` config section when deleting a branch (t3200 92).
    remove_branch_config_section(repo, &name);

    if !args.quiet {
        let hex;
        let short = if check_refname_format(&refname, &RefNameOptions::default()).is_err() {
            "broken"
        } else {
            hex = branch_oid.to_hex();
            &hex[..7.min(hex.len())]
        };
        println!("Deleted branch {name} (was {short}).");
    }

    Ok(())
}

fn branch_delete_name_escapes_ref_store(name: &str) -> bool {
    let path = Path::new(name);
    path.is_absolute()
        || path.components().any(|component| {
            matches!(
                component,
                Component::Prefix(_)
                    | Component::RootDir
                    | Component::CurDir
                    | Component::ParentDir
            )
        })
}

fn branch_delete_base_oid(
    repo: &Repository,
    branch_name: &str,
    head: &HeadState,
) -> Option<ObjectId> {
    if let Some(upstream_ref) =
        grit_lib::branch_tracking::upstream_tracking_full_ref(repo, branch_name)
    {
        if let Ok(oid) = grit_lib::refs::resolve_ref(&repo.git_dir, &upstream_ref) {
            return Some(oid);
        }
    }
    head.oid().copied()
}

/// Remove the entire `[branch "<name>"]` config section (used after deleting a branch).
fn remove_branch_config_section(repo: &Repository, name: &str) {
    let config_path = repo.git_dir.join("config");
    let Ok(content) = fs::read_to_string(&config_path) else {
        return;
    };
    let section = format!("[branch \"{name}\"]");
    if !content.contains(&section) {
        return;
    }
    let mut out = String::new();
    let mut in_section = false;
    for line in content.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with('[') {
            in_section = trimmed == section;
            if in_section {
                continue;
            }
        }
        if in_section {
            continue;
        }
        out.push_str(line);
        out.push('\n');
    }
    let _ = fs::write(&config_path, out);
}

/// Rename a branch.
fn rename_branch(repo: &Repository, head: &HeadState, args: &Args) -> Result<()> {
    let (old_name_owned, new_name_owned);
    let (old_name, new_name): (&str, &str);
    if let Some(sp) = args.start_point.as_deref() {
        old_name_owned = args.name.as_deref().unwrap_or("").to_owned();
        new_name_owned = sp.to_owned();
        old_name = &old_name_owned;
        new_name = &new_name_owned;
    } else if let Some(n) = args.name.as_deref() {
        old_name_owned = head
            .branch_name()
            .ok_or_else(|| {
                if matches!(head, HeadState::Detached { .. }) {
                    eprintln!("fatal: cannot rename the current branch while not on any");
                    std::process::exit(128);
                } else {
                    anyhow::anyhow!("no current branch to rename")
                }
            })?
            .to_owned();
        new_name_owned = n.to_owned();
        old_name = &old_name_owned;
        new_name = &new_name_owned;
    } else {
        // No args at all: dump usage
        eprintln!("error: branch name required");
        std::process::exit(128);
    };

    // Renaming a branch to itself is a no-op
    if old_name == new_name {
        return Ok(());
    }
    validate_new_branch_name_or_die(repo, new_name, 128);

    let force = args.force_rename || args.force;
    let old_ref = format!("refs/heads/{old_name}");
    let new_ref = format!("refs/heads/{new_name}");

    // Renaming a branch that is a symbolic ref is not allowed (t3200 84).
    if matches!(
        refs::read_symbolic_ref(&repo.git_dir, &old_ref),
        Ok(Some(_))
    ) {
        eprintln!("fatal: Branch '{old_name}' has a symref, not a branch.");
        std::process::exit(128);
    }

    // Git only rejects a rename when a worktree is mid-rebase/bisect on the branch; a branch that
    // is merely checked out in another worktree CAN be renamed (its HEAD symref is rewritten).
    if let Some((kind, wt_path)) = worktree_rebasing_or_bisecting_branch(repo, old_name) {
        bail!("fatal: branch {old_name} is being {kind} at {wt_path}");
    }

    // Resolve old branch - check both loose and packed refs
    let old_oid = if let Ok(oid) = grit_lib::refs::resolve_ref(&repo.git_dir, &old_ref) {
        oid
    } else if head.branch_name() == Some(old_name) && head.oid().is_none() {
        // Allow renaming an unborn current branch (e.g. immediately after init).
        let head_path = repo.git_dir.join("HEAD");
        let head_content = format!("ref: refs/heads/{new_name}\n");
        fs::write(head_path, head_content)?;
        rename_branch_config(repo, old_name, new_name)?;
        return Ok(());
    } else if branch_ref_is_unborn_across_worktrees(repo, &old_ref)? {
        if !force && grit_lib::refs::resolve_ref(&repo.git_dir, &new_ref).is_ok() {
            bail!("A branch named '{new_name}' already exists.");
        }
        if let Some(conflict) = ref_namespace_conflict_excluding(repo, &new_ref, Some(&old_ref)) {
            eprintln!("error: '{conflict}' exists; cannot create '{new_ref}'");
            bail!("fatal: branch rename failed");
        }
        let head_update_failed = update_worktree_heads(repo, old_name, new_name)?;
        rename_branch_config(repo, old_name, new_name)?;
        if head_update_failed {
            eprintln!("fatal: branch renamed to {new_name}, but HEAD is not updated");
            std::process::exit(128);
        }
        return Ok(());
    } else {
        eprintln!("fatal: no branch named '{old_name}'");
        std::process::exit(128);
    };

    // Check if new name already exists (unless force; -M or -m -f)
    if !force && grit_lib::refs::resolve_ref(&repo.git_dir, &new_ref).is_ok() {
        bail!("A branch named '{new_name}' already exists.");
    }
    if let Some(conflict) = ref_namespace_conflict_excluding(repo, &new_ref, Some(&old_ref)) {
        eprintln!("error: '{conflict}' exists; cannot create '{new_ref}'");
        bail!("fatal: branch rename failed");
    }

    // The "cannot force update ... used by worktree" check only applies when the destination
    // branch ref ACTUALLY EXISTS (Git `validate_new_branchname` short-circuits via
    // `validate_branchname` when the ref is absent). A worktree whose HEAD points at a now-orphan
    // symref (e.g. after a partial rename) must therefore NOT block a recovery rename. (t3200 33)
    if grit_lib::refs::resolve_ref(&repo.git_dir, &new_ref).is_ok() {
        let new_branch_worktree = if head.branch_name() == Some(new_name) {
            repo.work_tree.as_deref().map(|p| p.display().to_string())
        } else {
            branch_checked_out_in_other_worktree(repo, new_name)
        };
        if let Some(wt_path) = new_branch_worktree {
            bail!(
                "fatal: cannot force update the branch '{new_name}' used by worktree at '{wt_path}'"
            );
        }
    }

    // Capture reflog bytes before `delete_ref`: that helper removes `logs/<refname>` too.
    let reflog_dir = repo.git_dir.join("logs");
    let old_log_path = reflog_dir.join(&old_ref);
    if fs::symlink_metadata(&old_log_path)
        .map(|meta| meta.file_type().is_symlink())
        .unwrap_or(false)
    {
        bail!("fatal: branch rename failed");
    }
    let old_reflog_bytes = if old_log_path.is_file() {
        fs::read(&old_log_path).ok()
    } else {
        None
    };

    // Delete the old ref FIRST to avoid d/f conflicts
    // (e.g., renaming m to m/m needs to remove refs/heads/m file before
    // creating refs/heads/m/ directory, or n/n to n needs to remove refs/heads/n/
    // directory before creating refs/heads/n file)
    grit_lib::refs::delete_ref(&repo.git_dir, &old_ref).map_err(|e| anyhow::anyhow!("{e}"))?;

    // Clean up empty parent directories for old ref
    let old_path = repo.git_dir.join(&old_ref);
    let heads_dir = repo.git_dir.join("refs/heads");
    let mut parent = old_path.parent();
    while let Some(p) = parent {
        if p == heads_dir || !p.starts_with(&heads_dir) {
            break;
        }
        if fs::remove_dir(p).is_err() {
            break;
        }
        parent = p.parent();
    }

    // Now write the new ref
    grit_lib::refs::write_ref(&repo.git_dir, &new_ref, &old_oid)
        .map_err(|e| anyhow::anyhow!("{e}"))?;

    // Update HEAD if we renamed the current branch
    if head.branch_name() == Some(old_name) {
        let head_content = format!("ref: refs/heads/{new_name}\n");
        fs::write(repo.git_dir.join("HEAD"), head_content)?;
    }

    // Also update HEAD in worktrees that have the old branch checked out. Git updates each
    // worktree HEAD that it can; if any cannot be updated (e.g. a stale HEAD.lock), the ones that
    // succeeded stay updated but the command still fails (t3200 33).
    let head_update_failed = update_worktree_heads(repo, old_name, new_name)?;

    // Rename reflog: migrate content captured before `delete_ref`, then append rename entry.
    let new_log = reflog_dir.join(&new_ref);
    if let Some(log_bytes) = old_reflog_bytes {
        let logs_heads_dir = reflog_dir.join("refs/heads");
        let mut parent = old_log_path.parent();
        while let Some(p) = parent {
            if p == logs_heads_dir || !p.starts_with(&logs_heads_dir) {
                break;
            }
            if fs::remove_dir(p).is_err() {
                break;
            }
            parent = p.parent();
        }
        if let Some(parent) = new_log.parent() {
            let _ = fs::create_dir_all(parent);
        }
        let ident = get_reflog_identity();
        let rename_entry = format!(
            "{oid} {oid} {ident}\tBranch: renamed {old_ref} to {new_ref}\n",
            oid = old_oid
        );
        let old_content = String::from_utf8_lossy(&log_bytes).to_string();
        let new_content = format!("{}{rename_entry}", old_content);
        let _ = fs::write(&new_log, new_content.as_bytes());
    }

    // Write HEAD reflog entry for branch rename
    if head.branch_name() == Some(old_name) {
        let head_log = reflog_dir.join("HEAD");
        let ident = get_reflog_identity();
        // Match Git: two lines (old?zero, zero?old) so `git reflog` skips the middle entry
        // and `log -g` indices align with upstream tests.
        let zero_hex = zero_oid().to_hex();
        let oid_hex = old_oid.to_hex();
        let entry1 =
            format!("{oid_hex} {zero_hex} {ident}\tBranch: renamed {old_ref} to {new_ref}\n",);
        let entry2 =
            format!("{zero_hex} {oid_hex} {ident}\tBranch: renamed {old_ref} to {new_ref}\n",);
        let _ = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&head_log)
            .and_then(|mut f| {
                use std::io::Write;
                f.write_all(entry1.as_bytes())?;
                f.write_all(entry2.as_bytes())
            });
    }

    // Rename config sections
    rename_branch_config(repo, old_name, new_name)?;

    // The branch ref (and every reachable worktree HEAD) is renamed, but one or more worktree
    // HEADs could not be updated (locked). Git still reports failure in this case (t3200 33).
    if head_update_failed {
        eprintln!("fatal: branch renamed to {new_name}, but HEAD is not updated");
        std::process::exit(128);
    }

    Ok(())
}

/// Update HEAD in linked worktrees after branch rename.
/// Returns `Some((kind, path))` if any worktree (main or linked) is mid-rebase or mid-bisect on
/// `branch` (Git `reject_rebase_or_bisect_branch`). `kind` is `"rebased"` or `"bisected"`.
fn worktree_rebasing_or_bisecting_branch(
    repo: &Repository,
    branch: &str,
) -> Option<(&'static str, String)> {
    let common = grit_lib::refs::common_dir(&repo.git_dir).unwrap_or_else(|| repo.git_dir.clone());
    let target = format!("refs/heads/{branch}");

    // Build (admin_dir, worktree_path) pairs for the main repo and each linked worktree.
    let mut entries: Vec<(std::path::PathBuf, String)> = Vec::new();
    let main_path = repo
        .work_tree
        .as_deref()
        .map(|p| p.display().to_string())
        .unwrap_or_else(|| common.display().to_string());
    entries.push((common.clone(), main_path));
    if let Ok(rd) = fs::read_dir(common.join("worktrees")) {
        for e in rd.flatten() {
            let admin = e.path();
            let path = fs::read_to_string(admin.join("gitdir"))
                .ok()
                .map(|s| {
                    let t = s.trim();
                    t.strip_suffix("/.git").unwrap_or(t).to_owned()
                })
                .unwrap_or_else(|| admin.display().to_string());
            entries.push((admin, path));
        }
    }

    for (admin, path) in entries {
        // rebase: rebase-merge/head-name or rebase-apply/head-name names the branch being rebased.
        for sub in ["rebase-merge", "rebase-apply"] {
            let hn = admin.join(sub).join("head-name");
            if let Ok(content) = fs::read_to_string(&hn) {
                if content.trim() == target {
                    return Some(("rebased", path));
                }
            }
        }
        // bisect: BISECT_START names the branch bisecting was started from.
        if admin.join("BISECT_LOG").exists() {
            if let Ok(start) = fs::read_to_string(admin.join("BISECT_START")) {
                let s = start.trim();
                let s = s.strip_prefix("refs/heads/").unwrap_or(s);
                if s == branch {
                    return Some(("bisected", path));
                }
            }
        }
    }
    None
}

/// Rewrite the HEAD symref of every linked worktree that has `old_name` checked out so it points
/// at `new_name`. Git updates each worktree HEAD under a per-worktree ref lock; a worktree whose
/// HEAD cannot be locked (a stale `HEAD.lock` exists) is left untouched and the rename reports
/// failure afterward (Git `replace_each_worktree_head_symref`, t3200 33).
///
/// Returns `Ok(true)` when at least one worktree HEAD could not be updated.
fn update_worktree_heads(repo: &Repository, old_name: &str, new_name: &str) -> Result<bool> {
    let common = grit_lib::refs::common_dir(&repo.git_dir).unwrap_or_else(|| repo.git_dir.clone());
    let expected = format!("ref: refs/heads/{old_name}");
    let mut any_failed = false;

    let mut admin_dirs = vec![common.clone()];
    if let Ok(entries) = fs::read_dir(common.join("worktrees")) {
        for entry in entries.flatten() {
            admin_dirs.push(entry.path());
        }
    }

    for wt_dir in admin_dirs {
        let head_path = wt_dir.join("HEAD");
        if let Ok(content) = fs::read_to_string(&head_path) {
            if content.trim() != expected {
                continue;
            }
            // A pre-existing HEAD.lock means another process holds the ref lock; we cannot update
            // this worktree's HEAD. Skip it and signal overall failure.
            if wt_dir.join("HEAD.lock").exists() {
                any_failed = true;
                continue;
            }
            let new_content = format!("ref: refs/heads/{new_name}\n");
            if fs::write(&head_path, new_content).is_err() {
                any_failed = true;
            }
        }
    }
    Ok(any_failed)
}

fn branch_used_by_other_worktree(repo: &Repository, branch: &str) -> Result<Option<String>> {
    Ok(crate::commands::worktree_refs::branch_occupied_by_other_worktree(repo, branch))
}

fn branch_checked_out_in_other_worktree(repo: &Repository, branch: &str) -> Option<String> {
    branch_used_by_other_worktree(repo, branch).ok().flatten()
}

/// Get reflog identity string.
fn get_reflog_identity() -> String {
    let name = std::env::var("GIT_COMMITTER_NAME").unwrap_or_else(|_| "Test User".to_string());
    let email =
        std::env::var("GIT_COMMITTER_EMAIL").unwrap_or_else(|_| "test@example.com".to_string());
    let date = reflog_committer_date();
    format!("{name} <{email}> {date}")
}

fn reftable_tables_locked(repo: &Repository) -> bool {
    let dir = repo.git_dir.join("reftable");
    let Ok(entries) = fs::read_dir(dir) else {
        return false;
    };
    entries.flatten().any(|entry| {
        entry
            .file_name()
            .to_str()
            .is_some_and(|name| name.ends_with(".ref.lock"))
    })
}

fn reftable_log_identity_parts(identity: &str) -> (String, String, u64, i16) {
    let (name_part, rest) = identity
        .rsplit_once(" <")
        .map(|(name, rest)| (name.to_owned(), rest))
        .unwrap_or_else(|| ("Test User".to_owned(), identity));
    let (email, after_email) = rest
        .split_once("> ")
        .map(|(email, after)| (email.to_owned(), after))
        .unwrap_or_else(|| (String::new(), rest));
    let mut parts = after_email.split_whitespace();
    let time_seconds = parts
        .next()
        .and_then(|value| value.parse::<u64>().ok())
        .unwrap_or(0);
    let tz_offset = parts.next().map(parse_reftable_tz_offset).unwrap_or(0);
    (name_part, email, time_seconds, tz_offset)
}

fn parse_reftable_tz_offset(raw: &str) -> i16 {
    if raw.len() != 5 {
        return 0;
    }
    let sign = if raw.as_bytes().first() == Some(&b'-') {
        -1
    } else {
        1
    };
    let hours = raw[1..3].parse::<i16>().unwrap_or(0);
    let minutes = raw[3..5].parse::<i16>().unwrap_or(0);
    sign * (hours * 60 + minutes)
}

/// Reflog timestamp as `<unix-epoch> <±HHMM>`. A `GIT_COMMITTER_DATE` may be in any human form
/// (e.g. `2005-05-26 23:30`); Git parses it to epoch+tz before writing the reflog, otherwise
/// `git reflog show` cannot parse the line. Falls back to the current time.
fn reflog_committer_date() -> String {
    if let Ok(raw) = std::env::var("GIT_COMMITTER_DATE") {
        let raw = raw.trim();
        // Already in `<epoch> <tz>` form? keep it.
        if let Some((secs, tz)) = raw.split_once(' ') {
            if secs.parse::<i64>().is_ok()
                && tz.len() == 5
                && (tz.starts_with('+') || tz.starts_with('-'))
            {
                return raw.to_owned();
            }
        }
        if let Ok((ts, off_min)) = grit_lib::git_date::parse::parse_date_basic(raw) {
            let sign = if off_min < 0 { '-' } else { '+' };
            let abs = off_min.abs();
            return format!("{ts} {sign}{:02}{:02}", abs / 60, abs % 60);
        }
    }
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    format!("{now} +0000")
}

/// Rename branch config sections.
fn rename_branch_config(repo: &Repository, old_name: &str, new_name: &str) -> Result<()> {
    let config_path = repo.git_dir.join("config");
    let content = match fs::read_to_string(&config_path) {
        Ok(c) => c,
        Err(_) => return Ok(()),
    };

    let old_section = format!("[branch \"{old_name}\"]");
    let new_section = format!("[branch \"{new_name}\"]");
    if content.contains(&old_section) {
        let updated = content.replace(&old_section, &new_section);
        fs::write(&config_path, updated)?;
    }
    Ok(())
}

/// True when `branch_ref` (e.g. `refs/heads/x`) is checked out as an unborn branch in the main
/// repo or any linked worktree (orphan / no tip commit yet).
fn branch_ref_is_unborn_across_worktrees(repo: &Repository, branch_ref: &str) -> Result<bool> {
    let common = grit_lib::refs::common_dir(&repo.git_dir).unwrap_or_else(|| repo.git_dir.clone());
    let mut admin_dirs = vec![common.clone()];
    if let Ok(rd) = fs::read_dir(common.join("worktrees")) {
        for e in rd.flatten() {
            admin_dirs.push(e.path());
        }
    }
    for admin in admin_dirs {
        let hp = admin.join("HEAD");
        let Ok(content) = fs::read_to_string(&hp) else {
            continue;
        };
        let t = content.trim();
        let Some(sym) = t.strip_prefix("ref: ") else {
            continue;
        };
        if sym != branch_ref {
            continue;
        }
        let st = resolve_head(&admin)?;
        if matches!(st, HeadState::Branch { oid: None, .. }) {
            return Ok(true);
        }
    }
    Ok(false)
}

fn copy_branch(repo: &Repository, head: &HeadState, args: &Args) -> Result<()> {
    let (src_name_owned, dst_name_owned);
    let (src_name, dst_name): (&str, &str);
    if let Some(sp) = args.start_point.as_deref() {
        src_name_owned = args.name.as_deref().unwrap_or("").to_owned();
        dst_name_owned = sp.to_owned();
        src_name = &src_name_owned;
        dst_name = &dst_name_owned;
    } else if let Some(n) = args.name.as_deref() {
        src_name_owned = head
            .branch_name()
            .ok_or_else(|| {
                if matches!(head, HeadState::Detached { .. }) {
                    eprintln!("fatal: cannot copy the current branch while not on any");
                    std::process::exit(128);
                }
                anyhow::anyhow!("no current branch to copy")
            })?
            .to_owned();
        dst_name_owned = n.to_owned();
        src_name = &src_name_owned;
        dst_name = &dst_name_owned;
    } else {
        eprintln!("error: branch name required");
        std::process::exit(128);
    };

    let src_ref = format!("refs/heads/{src_name}");
    let dst_ref = format!("refs/heads/{dst_name}");

    let src_oid = if let Ok(oid) = grit_lib::refs::resolve_ref(&repo.git_dir, &src_ref) {
        oid
    } else if branch_ref_is_unborn_across_worktrees(repo, &src_ref)? {
        eprintln!("fatal: no commit on branch '{src_name}' yet");
        std::process::exit(128);
    } else {
        eprintln!("fatal: no branch named '{src_name}'");
        std::process::exit(128);
    };

    // Copying a branch to itself is a no-op (Git: `branch -c m2 m2`).
    if src_name == dst_name {
        return Ok(());
    }

    // Check if dst already exists. Force can come from `-C` (force_copy) or `-c -f` (force).
    let force = args.force_copy || args.force;
    let dst_exists = grit_lib::refs::resolve_ref(&repo.git_dir, &dst_ref).is_ok();
    if !force && dst_exists {
        bail!("A branch named '{dst_name}' already exists.");
    }
    // As in `validate_new_branchname`, the "used by worktree" guard only applies when the
    // destination branch ref actually exists (Git short-circuits otherwise). A force-copy onto a
    // branch currently checked out in any worktree must fail (t3200 73).
    if force && dst_exists {
        let dst_worktree = if head.branch_name() == Some(dst_name) {
            repo.work_tree.as_deref().map(|p| p.display().to_string())
        } else {
            branch_checked_out_in_other_worktree(repo, dst_name)
        };
        if let Some(wt_path) = dst_worktree {
            bail!(
                "fatal: cannot force update the branch '{dst_name}' used by worktree at '{wt_path}'"
            );
        }
    }

    if let Some(conflict) = ref_namespace_conflict(repo, &dst_ref) {
        eprintln!("error: '{conflict}' exists; cannot create '{dst_ref}'");
        bail!("fatal: branch copy failed");
    }

    grit_lib::refs::write_ref(&repo.git_dir, &dst_ref, &src_oid)
        .map_err(|e| anyhow::anyhow!("{e}"))?;

    // Copy reflog if exists
    if grit_lib::reftable::is_reftable_repo(&repo.git_dir) {
        if grit_lib::reflog::reflog_exists(&repo.git_dir, &src_ref) {
            let mut entries = grit_lib::reflog::read_reflog(&repo.git_dir, &src_ref)
                .map_err(|e| anyhow::anyhow!("{e}"))?;
            entries.push(grit_lib::reflog::ReflogEntry {
                old_oid: src_oid,
                new_oid: src_oid,
                identity: get_reflog_identity(),
                message: format!("Branch: copied {src_ref} to {dst_ref}"),
            });
            grit_lib::reftable::reftable_replace_reflog(&repo.git_dir, &dst_ref, &entries)
                .map_err(|e| anyhow::anyhow!("{e}"))?;
        }
    } else {
        let reflog_dir = repo.git_dir.join("logs");
        let src_log = reflog_dir.join(&src_ref);
        let dst_log = reflog_dir.join(&dst_ref);
        if src_log.exists() {
            if let Some(parent) = dst_log.parent() {
                let _ = fs::create_dir_all(parent);
            }
            let _ = fs::copy(&src_log, &dst_log);
        }
    }

    copy_branch_config(repo, src_name, dst_name)?;

    Ok(())
}

fn copy_branch_config(repo: &Repository, src_name: &str, dst_name: &str) -> Result<()> {
    let config_path = repo.git_dir.join("config");
    let Ok(content) = fs::read_to_string(&config_path) else {
        return Ok(());
    };
    let old_section = format!("[branch \"{src_name}\"]");
    let new_section = format!("[branch \"{dst_name}\"]");
    if !content.contains(&old_section) {
        return Ok(());
    }

    let mut out = String::new();
    let mut duplicate: Option<(Vec<String>, Vec<String>)> = None;
    for line in content.lines() {
        if line.trim() == old_section {
            out.push_str(line);
            out.push('\n');
            duplicate = Some((vec![new_section.clone()], Vec::new()));
            continue;
        }

        if line.starts_with('[') {
            if let Some((block, trailing)) = duplicate.take() {
                for dup_line in block {
                    out.push_str(&dup_line);
                    out.push('\n');
                }
                for trailing_line in trailing {
                    out.push_str(&trailing_line);
                    out.push('\n');
                }
            }
            out.push_str(line);
            out.push('\n');
            continue;
        }

        if let Some((block, trailing)) = duplicate.as_mut() {
            // Top-level comments and blank lines between sections are leading material for the
            // following section in Git's config writer. Keep them in place before inserting the
            // copied branch section, and then repeat them before the following original section.
            let is_top_level_comment =
                line.starts_with(';') || line.starts_with('#') || line.trim().is_empty();
            if is_top_level_comment {
                trailing.push(line.to_owned());
            } else {
                block.push(line.to_owned());
            }
        }
        out.push_str(line);
        out.push('\n');
    }
    if let Some((block, trailing)) = duplicate.take() {
        for dup_line in block {
            out.push_str(&dup_line);
            out.push('\n');
        }
        for trailing_line in trailing {
            out.push_str(&trailing_line);
            out.push('\n');
        }
    }
    fs::write(config_path, out)?;
    Ok(())
}

fn ref_namespace_conflict(repo: &Repository, refname: &str) -> Option<String> {
    ref_namespace_conflict_excluding(repo, refname, None)
}

/// Like [`ref_namespace_conflict`] but ignores `exclude` (the ref being renamed/copied away),
/// which Git removes as part of the same transaction so it never counts as a D/F conflict
/// (e.g. `git branch -m m m/m`, `git branch -m n/n n`).
fn ref_namespace_conflict_excluding(
    repo: &Repository,
    refname: &str,
    exclude: Option<&str>,
) -> Option<String> {
    let components: Vec<&str> = refname.split('/').collect();
    for i in 1..components.len() {
        let prefix = components[..i].join("/");
        if Some(prefix.as_str()) == exclude {
            continue;
        }
        if prefix.starts_with("refs/")
            && grit_lib::refs::resolve_ref(&repo.git_dir, &prefix).is_ok()
        {
            return Some(prefix);
        }
    }

    let prefix = format!("{refname}/");
    grit_lib::refs::list_refs(&repo.git_dir, "refs/")
        .ok()?
        .into_iter()
        .map(|(name, _)| name)
        .find(|name| name.starts_with(&prefix) && Some(name.as_str()) != exclude)
}

/// Collect branch names from a refs directory.
fn collect_branches(dir: &Path, prefix: &str, out: &mut Vec<(String, ObjectId)>) -> Result<()> {
    let entries = match fs::read_dir(dir) {
        Ok(e) => e,
        Err(_) => return Ok(()),
    };

    let mut sorted: Vec<_> = entries.filter_map(|e| e.ok()).collect();
    sorted.sort_by_key(|e| e.file_name());

    for entry in sorted {
        let path = entry.path();
        let name = entry.file_name().to_string_lossy().to_string();
        let full_name = if prefix.is_empty() {
            name
        } else {
            format!("{prefix}/{name}")
        };

        if path.is_dir() {
            collect_branches(&path, &full_name, out)?;
        } else if let Ok(content) = fs::read_to_string(&path) {
            if let Ok(oid) = ObjectId::from_hex(content.trim()) {
                out.push((full_name, oid));
            }
        }
    }

    Ok(())
}

/// Get the first line of a commit's message.
fn commit_subject(odb: &grit_lib::odb::Odb, oid: &ObjectId) -> Option<String> {
    let obj = odb.read(oid).ok()?;
    let commit = parse_commit(&obj.data).ok()?;
    Some(grit_lib::commit_pretty::message_subject(&commit.message))
}

/// Extract committer timestamp from a commit for sorting.
fn committer_time(
    odb: &grit_lib::odb::Odb,
    oid: &ObjectId,
    branch_name: &str,
    is_remote: bool,
) -> i64 {
    let obj = match odb.read(oid) {
        Ok(o) => o,
        Err(_) => {
            if is_remote && branch_name.ends_with("/HEAD") {
                return i64::MAX;
            }
            return 0;
        }
    };
    let commit = match parse_commit(&obj.data) {
        Ok(c) => c,
        Err(_) => {
            if is_remote && branch_name.ends_with("/HEAD") {
                return i64::MAX;
            }
            return 0;
        }
    };
    parse_signature_time(&commit.committer)
}

/// Extract author timestamp from a commit for sorting.
fn author_time(
    odb: &grit_lib::odb::Odb,
    oid: &ObjectId,
    branch_name: &str,
    is_remote: bool,
) -> i64 {
    let obj = match odb.read(oid) {
        Ok(o) => o,
        Err(_) => {
            if is_remote && branch_name.ends_with("/HEAD") {
                return i64::MAX;
            }
            return 0;
        }
    };
    let commit = match parse_commit(&obj.data) {
        Ok(c) => c,
        Err(_) => {
            if is_remote && branch_name.ends_with("/HEAD") {
                return i64::MAX;
            }
            return 0;
        }
    };
    parse_signature_time(&commit.author)
}

/// Parse the Unix timestamp from a Git signature line like "Name <email> 1234567890 +0000".
fn parse_signature_time(sig: &str) -> i64 {
    let parts: Vec<&str> = sig.rsplitn(3, ' ').collect();
    if parts.len() >= 2 {
        parts[1].parse::<i64>().unwrap_or(0)
    } else {
        0
    }
}

/// Simple glob matching for branch pattern filtering.
/// Supports `*` (match any chars) and `?` (match one char).
fn glob_match(pattern: &str, text: &str) -> bool {
    glob_match_inner(pattern.as_bytes(), text.as_bytes())
}

fn glob_match_case(pattern: &str, text: &str, ignore_case: bool) -> bool {
    if ignore_case {
        glob_match(&pattern.to_lowercase(), &text.to_lowercase())
    } else {
        glob_match(pattern, text)
    }
}

fn glob_match_inner(pattern: &[u8], text: &[u8]) -> bool {
    let mut pi = 0;
    let mut ti = 0;
    let mut star_pi = usize::MAX;
    let mut star_ti = 0;

    while ti < text.len() {
        if pi < pattern.len() && pattern[pi] == b'?' {
            pi += 1;
            ti += 1;
        } else if pi < pattern.len() && pattern[pi] == b'*' {
            star_pi = pi;
            star_ti = ti;
            pi += 1;
        } else if pi < pattern.len() && pattern[pi] == text[ti] {
            pi += 1;
            ti += 1;
        } else if star_pi != usize::MAX {
            star_ti += 1;
            ti = star_ti;
            pi = star_pi + 1;
        } else {
            return false;
        }
    }

    while pi < pattern.len() && pattern[pi] == b'*' {
        pi += 1;
    }

    pi == pattern.len()
}

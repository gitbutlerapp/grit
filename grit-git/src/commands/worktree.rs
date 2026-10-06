//! `grit worktree` — manage multiple working trees.
//!
//! Each linked worktree has its own HEAD, index, and working directory,
//! but shares the object database and refs with the main repository.
//! Worktree metadata is stored under `.git/worktrees/<name>/`.

use anyhow::{bail, Context, Result};
use clap::{Args as ClapArgs, Subcommand};
use grit_lib::config::ConfigSet;
use grit_lib::hooks::{run_hook_opts, HookResult, RunHookOptions};
use grit_lib::index::{Index, IndexEntry};
use grit_lib::objects::ObjectId;
use grit_lib::refs;
use grit_lib::repo::Repository;
use grit_lib::state::{resolve_head, HeadState};
use grit_lib::worktree::{self, WorktreeEntry};
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

/// Arguments for `grit worktree`.
#[derive(Debug, ClapArgs)]
#[command(about = "Manage multiple working trees")]
pub struct Args {
    #[command(subcommand)]
    pub command: WorktreeCommand,
}

#[derive(Debug, Subcommand)]
pub enum WorktreeCommand {
    /// Create a new working tree.
    Add(AddArgs),
    /// List linked working trees.
    List(ListArgs),
    /// Move a working tree to a new location.
    Move(MoveArgs),
    /// Remove a working tree.
    Remove(RemoveArgs),
    /// Repair worktree administrative files.
    Repair(RepairArgs),
    /// Remove stale worktree administrative files.
    Prune(PruneArgs),
    /// Prevent a working tree from being pruned.
    Lock(LockArgs),
    /// Allow a locked working tree to be pruned.
    Unlock(UnlockArgs),
}

#[derive(Debug, ClapArgs)]
pub struct AddArgs {
    /// Path for the new working tree.
    pub path: PathBuf,

    /// Branch to check out (or create). Defaults to basename of path.
    pub branch: Option<String>,

    /// Create a new branch with this name.
    #[arg(short = 'b', long)]
    pub new_branch: Option<String>,

    /// Detach HEAD in the new worktree.
    #[arg(long)]
    pub detach: bool,

    /// Force creation even if the branch is already checked out elsewhere.
    /// Twice: also override a missing locked worktree registration.
    #[arg(short, long, action = clap::ArgAction::Count)]
    pub force: u8,

    /// Create a new unborn/orphan branch in the worktree.
    #[arg(long)]
    pub orphan: bool,

    /// Lock the worktree after creation.
    #[arg(long)]
    pub lock: bool,

    /// Reason for locking.
    #[arg(long)]
    pub reason: Option<String>,

    /// Checkout from a specific commit or branch.
    #[arg(long)]
    pub checkout: bool,

    /// Don't checkout (bare-like).
    #[arg(long)]
    pub no_checkout: bool,

    /// Quiet mode.
    #[arg(short = 'q', long)]
    pub quiet: bool,

    /// Track a remote branch.
    #[arg(long)]
    pub track: bool,

    /// Guess remote branch.
    #[arg(long)]
    pub guess_remote: bool,

    /// Don't guess remote branch.
    #[arg(long)]
    pub no_guess_remote: bool,

    /// Do not set up tracking.
    #[arg(long)]
    pub no_track: bool,

    /// Store paths as relative to the main worktree.
    #[arg(long)]
    pub relative_paths: bool,

    /// Store paths as absolute.
    #[arg(long = "no-relative-paths")]
    pub no_relative_paths: bool,

    /// Create a new branch with -B (reset if exists).
    #[arg(short = 'B')]
    pub force_new_branch: Option<String>,
}

#[derive(Debug, ClapArgs)]
pub struct ListArgs {
    /// Machine-readable output.
    #[arg(long)]
    pub porcelain: bool,

    /// NUL-terminated output (for --porcelain).
    #[arg(short = 'z')]
    pub nul: bool,

    /// Show verbose output including lock/prune info.
    #[arg(short, long, conflicts_with = "porcelain")]
    pub verbose: bool,
}

#[derive(Debug, ClapArgs)]
pub struct RemoveArgs {
    /// Path of the worktree to remove.
    pub path: PathBuf,

    /// Force removal. Once: allow removing with dirty files. Twice: also allow removing locked worktrees.
    #[arg(short, long, action = clap::ArgAction::Count)]
    pub force: u8,
}

#[derive(Debug, ClapArgs)]
pub struct PruneArgs {
    /// Only report what would be done.
    #[arg(short = 'n', long)]
    pub dry_run: bool,

    /// Report pruned entries.
    #[arg(short, long)]
    pub verbose: bool,

    /// Prune entries older than a specific time.
    #[arg(long)]
    pub expire: Option<String>,
}

#[derive(Debug, ClapArgs)]
pub struct LockArgs {
    /// Path of the worktree to lock.
    pub path: PathBuf,

    /// Reason for locking.
    #[arg(long)]
    pub reason: Option<String>,
}

#[derive(Debug, ClapArgs)]
pub struct MoveArgs {
    /// Current path of the worktree.
    pub source: PathBuf,

    /// New path for the worktree.
    pub destination: PathBuf,

    /// Force move. Once: allow moving to existing path. Twice: also allow moving locked worktree.
    #[arg(short, long, action = clap::ArgAction::Count)]
    pub force: u8,

    /// Store paths as relative.
    #[arg(long)]
    pub relative_paths: bool,

    /// Store paths as absolute.
    #[arg(long = "no-relative-paths")]
    pub no_relative_paths: bool,
}

#[derive(Debug, ClapArgs)]
pub struct RepairArgs {
    /// Use relative paths when repairing gitfiles.
    #[arg(long = "relative-paths")]
    pub relative_paths: bool,
    /// Use absolute paths when repairing gitfiles.
    #[arg(long = "no-relative-paths")]
    pub no_relative_paths: bool,
    /// Paths to repair (defaults to all linked worktrees).
    pub paths: Vec<PathBuf>,
}

#[derive(Debug, ClapArgs)]
pub struct UnlockArgs {
    /// Path of the worktree to unlock.
    pub path: PathBuf,
}

pub fn run(args: Args) -> Result<()> {
    match args.command {
        WorktreeCommand::Add(a) => cmd_add(a),
        WorktreeCommand::List(a) => cmd_list(a),
        WorktreeCommand::Move(a) => cmd_move(a),
        WorktreeCommand::Remove(a) => cmd_remove(a),
        WorktreeCommand::Repair(a) => cmd_repair(a),
        WorktreeCommand::Prune(a) => cmd_prune(a),
        WorktreeCommand::Lock(a) => cmd_lock(a),
        WorktreeCommand::Unlock(a) => cmd_unlock(a),
    }
}

/// Shared git directory (main `.git` for linked worktrees).
fn common_dir(git_dir: &Path) -> Result<PathBuf> {
    Ok(worktree::common_git_dir(git_dir))
}

/// Resolve a commit-ish string to an ObjectId within the given repo.
fn resolve_commitish(repo: &Repository, spec: &str) -> Result<ObjectId> {
    // Try as a branch ref first
    let common = common_dir(&repo.git_dir)?;
    if let Ok(oid) = refs::resolve_ref(&common, &format!("refs/heads/{spec}")) {
        return Ok(oid);
    }
    if let Ok(oid) = refs::resolve_ref(&common, &format!("refs/tags/{spec}")) {
        return Ok(oid);
    }
    if let Ok(oid) = refs::resolve_ref(&common, spec) {
        return Ok(oid);
    }
    // Try as raw hex OID
    if let Ok(oid) = ObjectId::from_hex(spec) {
        return Ok(oid);
    }
    // Try as a revision with navigation (e.g., HEAD~1, main^2)
    if let Ok(oid) = grit_lib::rev_parse::resolve_revision(repo, spec) {
        // Ensure it's a commit or can be resolved to one
        if let Ok(obj) = repo.odb.read(&oid) {
            if obj.kind == grit_lib::objects::ObjectKind::Commit {
                return Ok(oid);
            }
        }
    }
    bail!("not a valid commit-ish: '{spec}'");
}

/// `remote/branch` start ref (not a full `refs/remotes/...` ref).
fn parse_explicit_remote_branch(spec: &str) -> Option<(&str, &str)> {
    if spec.starts_with("refs/") {
        return None;
    }
    let slash = spec.find('/')?;
    let remote = spec.get(..slash)?.trim();
    let branch = spec.get(slash + 1..)?.trim();
    if remote.is_empty() || branch.is_empty() {
        return None;
    }
    Some((remote, branch))
}

fn write_branch_tracking_config(common: &Path, branch: &str, remote: &str, merge_branch: &str) {
    let cfg_path = common.join("config");
    if let Ok(mut cfg_content) = std::fs::read_to_string(&cfg_path) {
        let section = format!(
            "\n[branch \"{branch}\"]\
\n\tremote = {remote}\
\n\tmerge = refs/heads/{merge_branch}\n"
        );
        cfg_content.push_str(&section);
        let _ = fs::write(&cfg_path, cfg_content);
    }
}

/// Resolve `branch` against remote-tracking refs; honor `checkout.defaultRemote` when ambiguous.
fn resolve_remote_branch_dwim(
    common: &Path,
    branch: &str,
    default_remote: Option<&str>,
) -> Result<Option<(ObjectId, String)>> {
    let remote_refs = refs::list_refs(common, "refs/remotes/").unwrap_or_default();
    let mut matching: Vec<(String, ObjectId)> = remote_refs
        .iter()
        .filter_map(|(r, oid)| {
            let rest = r.strip_prefix("refs/remotes/")?;
            let (remote, name) = rest.split_once('/')?;
            if name == branch {
                Some((remote.to_string(), *oid))
            } else {
                None
            }
        })
        .collect();
    if matching.is_empty() {
        return Ok(None);
    }
    if matching.len() > 1 {
        if let Some(def) = default_remote {
            matching.retain(|(remote, _)| remote == def);
        }
        if matching.len() != 1 {
            bail!("fatal: '{branch}' matched multiple (remote) tracking branches");
        }
    }
    let (remote, oid) = matching.swap_remove(0);
    Ok(Some((oid, remote)))
}

/// True when any ref exists under `refs/heads/` (Git: `refs_for_each_branch_ref`).
fn has_any_local_branch(common: &Path) -> bool {
    refs::list_refs(common, "refs/heads/")
        .map(|v| !v.is_empty())
        .unwrap_or(false)
}

/// Git's `can_use_local_refs`: we may use local refs as a worktree source when HEAD resolves
/// to a commit or at least one local branch exists.
fn can_use_local_refs(
    common: &Path,
    git_dir: &Path,
    head_state: &grit_lib::state::HeadState,
    quiet: bool,
) -> bool {
    if head_state.oid().is_some() {
        return true;
    }
    if !has_any_local_branch(common) {
        return false;
    }
    if !quiet {
        let head_path = git_dir.join("HEAD");
        let head_contents = fs::read_to_string(&head_path).unwrap_or_default();
        let head_display = head_path.canonicalize().unwrap_or(head_path);
        eprintln!(
            "warning: HEAD points to an invalid (or orphaned) reference.\n\
HEAD path: '{}'\n\
HEAD contents: '{}'",
            head_display.display(),
            head_contents.trim()
        );
    }
    true
}

fn remotes_configured(common: &Path) -> bool {
    let config = ConfigSet::load(Some(common), false).unwrap_or_default();
    config.entries().iter().any(|e| {
        let parts: Vec<&str> = e.key.splitn(3, '.').collect();
        parts.len() == 3 && parts[0] == "remote" && parts[2] == "url"
    })
}

/// Git's `can_use_remote_refs`: when `guess_remote` is on, remote-tracking refs count as a source.
fn can_use_remote_refs(
    common: &Path,
    guess_remote: bool,
    no_guess_remote: bool,
    force: u8,
) -> Result<bool> {
    if !guess_remote || no_guess_remote {
        return Ok(false);
    }
    if !refs::list_refs(common, "refs/remotes/")
        .unwrap_or_default()
        .is_empty()
    {
        return Ok(true);
    }
    if remotes_configured(common) && force == 0 {
        bail!(
            "fatal: No local or remote refs exist despite at least one remote\n\
present, stopping; use 'add -f' to override or fetch a remote first"
        );
    }
    Ok(false)
}

/// Run `post-checkout` for a newly populated linked worktree (null old OID, flag `1`).
/// Git `check_candidate_path`: reject or reclaim a registered worktree path.
fn check_worktree_add_destination(repo: &Repository, wt_path: &Path, force: u8) -> Result<()> {
    let wt_canon = wt_path
        .canonicalize()
        .unwrap_or_else(|_| wt_path.to_path_buf());
    for entry in worktree::list_worktrees(repo)? {
        let entry_canon = entry
            .path
            .canonicalize()
            .unwrap_or_else(|_| entry.path.clone());
        if entry_canon != wt_canon {
            continue;
        }
        if entry.path.exists() {
            bail!("'{path}' already exists", path = wt_path.display());
        }
        if (!entry.is_locked && force >= 1) || (entry.is_locked && force >= 2) {
            fs::remove_dir_all(&entry.admin_dir).with_context(|| {
                format!(
                    "cannot remove registered worktree '{}'",
                    entry.admin_dir.display()
                )
            })?;
            return Ok(());
        }
        if entry.is_locked {
            bail!(
                "fatal: '{}' is a missing but locked worktree;\n\
use 'git worktree add -f -f' to override, or 'unlock' and 'prune' or 'remove' to clear",
                wt_path.display()
            );
        }
        bail!(
            "fatal: '{}' is a missing but already registered worktree;\n\
use 'git worktree add -f' to override, or 'prune' or 'remove' to clear",
            wt_path.display()
        );
    }
    Ok(())
}

fn run_worktree_add_post_checkout_hook(
    repo: &Repository,
    wt_path: &Path,
    wt_admin: &Path,
    new_oid: &ObjectId,
) -> Result<()> {
    let config = ConfigSet::load(Some(&repo.git_dir), true).unwrap_or_default();
    let zero = ObjectId::from_bytes(&[0u8; 20]).map_err(|e| anyhow::anyhow!("{e}"))?;
    let git_dir_s = wt_admin.display().to_string();
    let wt_s = wt_path.display().to_string();
    let env = [
        ("GIT_DIR", git_dir_s.as_str()),
        ("GIT_WORK_TREE", wt_s.as_str()),
    ];
    let old_hex = zero.to_hex();
    let new_hex = new_oid.to_hex();
    let args = [old_hex.as_str(), new_hex.as_str(), "1"];
    if let HookResult::Failed(code) = run_hook_opts(
        Some(repo),
        "post-checkout",
        &args,
        &config,
        RunHookOptions {
            stdout_to_stderr: true,
            path_to_stdin: None,
            stdin_data: None,
            env_vars: &env,
            cwd: Some(wt_path),
            commit_env: None,
        },
        None,
    )
    .map_err(|e| anyhow::anyhow!("{e}"))?
    {
        bail!("post-checkout hook exited with status {code}");
    }
    Ok(())
}

fn print_orphan_worktree_hint(path: &Path, branch: Option<&str>) {
    eprintln!("hint: If you meant to create a worktree containing a new unborn branch");
    if let Some(branch) = branch {
        eprintln!("hint: named '{branch}', use the option '--orphan' as follows:");
        eprintln!("hint:");
        eprintln!(
            "hint:     git worktree add --orphan -b {branch} {}",
            path.display()
        );
    } else {
        eprintln!("hint:     git worktree add --orphan {}", path.display());
    }
}

/// Git's `dwim_orphan` for `worktree add`: infer `--orphan` when the repo has no usable refs.
///
/// When `check_remote` is true (path-only `add <path>`), Git skips inferring if `guess_remote`
/// is enabled and [`can_use_remote_refs`] applies — the caller should DWIM from a remote branch.
fn dwim_infer_orphan(
    common: &Path,
    git_dir: &Path,
    head_state: &grit_lib::state::HeadState,
    args: &AddArgs,
    guess_remote: bool,
    check_remote: bool,
) -> Result<bool> {
    if can_use_local_refs(common, git_dir, head_state, args.quiet) {
        return Ok(false);
    }

    if check_remote && can_use_remote_refs(common, guess_remote, args.no_guess_remote, args.force)?
    {
        return Ok(false);
    }

    if !args.quiet {
        eprintln!("No possible source branch, inferring '--orphan'");
    }
    if args.track {
        bail!("fatal: options '--orphan' and '--track' cannot be used together");
    }
    if args.no_checkout {
        bail!("fatal: options '--orphan' and '--no-checkout' cannot be used together");
    }
    Ok(true)
}

// ---------------------------------------------------------------------------
// worktree add
// ---------------------------------------------------------------------------

fn initialize_worktree_reftable_stack(
    wt_admin: &Path,
    commit_oid: Option<ObjectId>,
    branch_name: Option<&str>,
) -> Result<()> {
    let reftable_dir = wt_admin.join("reftable");
    fs::create_dir_all(&reftable_dir)
        .with_context(|| format!("cannot create '{}'", reftable_dir.display()))?;
    fs::write(reftable_dir.join("tables.list"), "")?;

    if let Some(oid) = commit_oid {
        refs::write_ref(wt_admin, "refs/worktree/HEAD", &oid)?;
    }
    if let Some(branch_name) = branch_name {
        grit_lib::reftable::reftable_write_symref(
            wt_admin,
            "refs/worktree/refs/heads",
            &format!("refs/heads/{branch_name}"),
            None,
            None,
        )
        .map_err(|e| anyhow::anyhow!("{e}"))?;
    }

    Ok(())
}

fn cmd_add(args: AddArgs) -> Result<()> {
    // Validate mutually exclusive options
    {
        let mut exclusive = Vec::new();
        if args.new_branch.is_some() {
            exclusive.push("-b");
        }
        if args.force_new_branch.is_some() {
            exclusive.push("-B");
        }
        if args.detach {
            exclusive.push("--detach");
        }
        // Note: --orphan is compatible with -b (provides branch name) but not -B or --detach
        if args.orphan && (args.force_new_branch.is_some() || args.detach) {
            exclusive.push("--orphan");
        }
        if exclusive.len() > 1 {
            eprintln!(
                "fatal: options '{}' and '{}' cannot be used together",
                exclusive[0], exclusive[1]
            );
            std::process::exit(1);
        }
        if args.orphan && args.no_checkout {
            eprintln!("fatal: options '--orphan' and '--no-checkout' cannot be used together");
            std::process::exit(1);
        }
        if args.orphan && args.branch.is_some() {
            eprintln!("fatal: options '--orphan' and '<branch>' cannot be used together");
            std::process::exit(1);
        }
        // Additional mutual exclusions not caught above
        if args.detach && args.orphan {
            eprintln!("fatal: options '--detach' and '--orphan' cannot be used together");
            std::process::exit(1);
        }
        if args.reason.is_some() && !args.lock {
            bail!("--reason requires --lock");
        }
    }

    let repo = Repository::discover(None)?;
    let git_dir = repo.git_dir.clone();
    let common = common_dir(&repo.git_dir)?;
    let config = ConfigSet::load(Some(&common), true).unwrap_or_default();
    let default_remote = config.get("checkout.defaultRemote");
    let mut guess_remote = args.guess_remote;
    if !args.no_guess_remote
        && !guess_remote
        && config
            .get("worktree.guessRemote")
            .is_some_and(|v| v == "true")
    {
        guess_remote = true;
    }

    // Determine the absolute path for the new worktree
    let wt_path = if args.path.is_absolute() {
        args.path.clone()
    } else {
        std::env::current_dir()?.join(&args.path)
    };

    // Check if path exists and is non-empty
    if wt_path.exists() {
        let is_empty = wt_path.is_dir()
            && fs::read_dir(&wt_path)
                .map(|mut d| d.next().is_none())
                .unwrap_or(false);
        if !is_empty {
            bail!("'{path}' already exists", path = wt_path.display());
        }
    }

    // Canonicalize the path (don't create it yet — that happens later after validation)
    let wt_path = if wt_path.exists() {
        wt_path.canonicalize().unwrap_or(wt_path.clone())
    } else {
        // Not created yet — use absolute path from cwd
        if wt_path.is_absolute() {
            wt_path.clone()
        } else {
            std::env::current_dir().unwrap_or_default().join(&wt_path)
        }
    };

    let display_path = worktree_add_display_path(&args.path, &wt_path);
    let wt_name = worktree::worktree_path_basename(&wt_path);
    check_worktree_add_destination(&repo, &wt_path, args.force)?;
    let wt_admin = worktree::allocate_worktree_admin_dir(&common, &wt_path);

    // HEAD for DWIM/orphan and invalid-HEAD warnings is per-worktree (`git_dir`), not `commondir`.
    let head_state = resolve_head(&git_dir)?;

    // Git infers `--orphan` when the repo has no commit on HEAD and no local branches (dwim_orphan),
    // before resolving the start ref for `-b` / path-only add.
    let mut orphan = args.orphan;
    let used_new_branch_options = args.new_branch.is_some() || args.force_new_branch.is_some();
    if !orphan {
        if args.branch.is_none() && used_new_branch_options {
            orphan = dwim_infer_orphan(&common, &git_dir, &head_state, &args, guess_remote, false)?;
        } else if args.branch.is_none() && !used_new_branch_options {
            orphan = dwim_infer_orphan(&common, &git_dir, &head_state, &args, guess_remote, true)?;
        }
    }

    if orphan {
        let orphan_branch = args
            .new_branch
            .as_deref()
            .or(args.force_new_branch.as_deref())
            .unwrap_or(&wt_name);
        setup_unborn_worktree(
            &common,
            &wt_admin,
            &wt_path,
            &display_path,
            orphan_branch,
            args.lock,
            args.reason.as_deref(),
        )?;
        return Ok(());
    }

    let head_oid = head_state.oid().copied();

    // Determine branch mode and starting commit.
    // `worktree add <path> <branch>` — if <branch> exists as a ref, check it out;
    //   otherwise create a new branch from HEAD.
    // `worktree add <path> <commit-ish>` — check out detached HEAD at that commit.
    // `worktree add -b <new> <path>` — always create a new branch from HEAD.
    let (branch_name, commit_oid, implicit_detach) = if let Some(ref new_b) = args.force_new_branch
    {
        // -B: create or reset branch (args.branch is the start point)
        let oid = if let Some(ref start_spec) = args.branch {
            resolve_commitish(&repo, start_spec)?
        } else {
            match head_oid {
                Some(oid) => oid,
                None => {
                    if !args.quiet {
                        print_orphan_worktree_hint(&args.path, Some(new_b));
                    }
                    bail!("fatal: invalid reference: HEAD");
                }
            }
        };
        (Some(new_b.clone()), Some(oid), false)
    } else if let Some(ref new_b) = args.new_branch {
        // -b: create branch (args.branch is the start point if given)
        let oid = if let Some(ref start_spec) = args.branch {
            resolve_commitish(&repo, start_spec)?
        } else {
            match head_oid {
                Some(oid) => oid,
                None => {
                    if !args.quiet {
                        print_orphan_worktree_hint(&args.path, Some(new_b));
                    }
                    bail!("fatal: invalid reference: HEAD");
                }
            }
        };
        (Some(new_b.clone()), Some(oid), false)
    } else if let Some(ref spec) = args.branch {
        // Handle "-" shorthand (previous branch)
        let spec = if spec == "-" {
            // Resolve @{-1} to get the previous branch
            refs::resolve_at_n_branch(&common, "@{-1}")
                .ok()
                .ok_or_else(|| anyhow::anyhow!("-: no previous branch"))?
        } else {
            spec.clone()
        };
        let spec = &spec;
        // Existing local branch: check out attached.
        if let Ok(oid) = refs::resolve_ref(&common, &format!("refs/heads/{spec}")) {
            (Some(spec.clone()), Some(oid), false)
        } else if matches!(
            resolve_head(&common)?,
            HeadState::Branch {
                refname,
                oid: None,
                ..
            } if refname == format!("refs/heads/{spec}")
        ) {
            // HEAD is unborn but already points at refs/heads/<spec> (no ref file yet). Git still
            // allows `worktree add <path> <branch>` once there are commits (t1500 setup).
            let oid =
                head_oid.ok_or_else(|| anyhow::anyhow!("fatal: invalid reference: '{spec}'"))?;
            (Some(spec.clone()), Some(oid), false)
        } else if let Some((remote, branch_on_remote)) = parse_explicit_remote_branch(spec) {
            let tracking = format!("refs/remotes/{remote}/{branch_on_remote}");
            let oid = refs::resolve_ref(&common, &tracking)
                .map_err(|_| anyhow::anyhow!("fatal: invalid reference: '{spec}'"))?;
            if let Ok(local_oid) =
                refs::resolve_ref(&common, &format!("refs/heads/{branch_on_remote}"))
            {
                (Some(branch_on_remote.to_string()), Some(local_oid), false)
            } else if can_use_local_refs(&common, &git_dir, &head_state, true) {
                if !args.no_track {
                    write_branch_tracking_config(
                        &common,
                        branch_on_remote,
                        remote,
                        branch_on_remote,
                    );
                }
                (Some(branch_on_remote.to_string()), Some(oid), false)
            } else {
                (None, Some(oid), true)
            }
        } else if let Some((oid, remote_name)) =
            resolve_remote_branch_dwim(&common, spec, default_remote.as_deref())?
        {
            if !args.no_track {
                write_branch_tracking_config(&common, spec, &remote_name, spec);
            }
            (Some(spec.clone()), Some(oid), false)
        } else {
            // Existing non-branch commit-ish (e.g. tag): check out detached.
            match resolve_commitish(&repo, spec) {
                Ok(oid) => (None, Some(oid), true),
                Err(_) => bail!("fatal: invalid reference: '{spec}'"),
            }
        }
    } else {
        // `worktree add <path>` only: Git `dwim_branch` prefers an existing local branch named
        // like the path basename, else `new_branch` = basename and start from HEAD / remote.
        if let Ok(oid) = refs::resolve_ref(&common, &format!("refs/heads/{wt_name}")) {
            (Some(wt_name.clone()), Some(oid), false)
        } else if guess_remote && !args.no_guess_remote {
            if let Some((oid, remote_name)) =
                resolve_remote_branch_dwim(&common, &wt_name, default_remote.as_deref())?
            {
                if !args.no_track {
                    write_branch_tracking_config(&common, &wt_name, &remote_name, &wt_name);
                }
                (Some(wt_name.clone()), Some(oid), false)
            } else if let Some(oid) = head_oid {
                (Some(wt_name.clone()), Some(oid), false)
            } else {
                if !args.quiet {
                    print_orphan_worktree_hint(&args.path, None);
                }
                bail!("fatal: invalid reference: HEAD");
            }
        } else if let Some(oid) = head_oid {
            (Some(wt_name.clone()), Some(oid), false)
        } else {
            if !args.quiet {
                print_orphan_worktree_hint(&args.path, None);
            }
            bail!("fatal: invalid reference: HEAD");
        }
    };

    // Check if the branch is already checked out in another worktree (including rebase/bisect).
    let detach_head_mode = args.detach || implicit_detach;
    if !detach_head_mode {
        if let Some(ref name) = branch_name {
            if args.force == 0 && repo.work_tree.is_some() {
                let branch_ref = format!("refs/heads/{name}");
                let main_head = resolve_head(&common).unwrap_or(HeadState::Invalid);
                if let HeadState::Branch { ref refname, .. } = main_head {
                    if *refname == branch_ref {
                        bail!(
                            "fatal: '{name}' is already checked out at '{}'",
                            common.parent().unwrap_or(&common).display()
                        );
                    }
                }
                let wt_dir = common.join("worktrees");
                if wt_dir.is_dir() {
                    for entry in std::fs::read_dir(&wt_dir).into_iter().flatten().flatten() {
                        let head_content =
                            crate::commands::worktree_refs::read_head_content(&entry.path());
                        if let Some(content) = head_content {
                            if let Some(refname) = content.trim().strip_prefix("ref: ") {
                                if refname.trim() == branch_ref {
                                    let gitdir_file = entry.path().join("gitdir");
                                    let wt_path_str =
                                        if let Ok(raw) = std::fs::read_to_string(&gitdir_file) {
                                            let p = std::path::Path::new(raw.trim());
                                            p.parent().unwrap_or(p).display().to_string()
                                        } else {
                                            entry.file_name().to_string_lossy().to_string()
                                        };
                                    bail!(
                                        "fatal: '{name}' is already checked out at '{wt_path_str}'"
                                    );
                                }
                            }
                        }
                    }
                }
                if let Some(wt_path) =
                    crate::commands::worktree_refs::branch_held_by_rebase_or_bisect_elsewhere(
                        &repo, name,
                    )
                {
                    bail!("fatal: '{name}' is already checked out at '{wt_path}'");
                }
            }
        }
    } // end detach_head_mode check

    // Create the working tree directory
    fs::create_dir_all(&wt_path)
        .with_context(|| format!("cannot create directory '{}'", wt_path.display()))?;

    // Create the admin directory: .git/worktrees/<name>/
    fs::create_dir_all(&wt_admin)
        .with_context(|| format!("cannot create '{}'", wt_admin.display()))?;
    // Per-worktree loose refs (`refs/worktree/...`) live here; Git always creates `refs/`.
    fs::create_dir_all(wt_admin.join("refs"))
        .with_context(|| format!("cannot create '{}'", wt_admin.join("refs").display()))?;

    let use_relative_paths =
        use_relative_worktree_paths(args.relative_paths, args.no_relative_paths, &config);
    if use_relative_paths {
        enable_relative_worktrees_extension(&common)?;
    }
    write_worktree_linking_files(&wt_path, &wt_admin, use_relative_paths)?;

    // Write commondir file — relative path from worktree admin to the common dir
    // Standard git uses relative paths like "../../"
    let commondir_rel = make_relative_path(&wt_admin, &common);
    fs::write(
        wt_admin.join("commondir"),
        format!("{}\n", commondir_rel.display()),
    )?;

    // Linked worktree admin `config` is minimal; the work tree path comes from the
    // gitdir file (Git does not store `core.worktree` in linked admin config).
    fs::write(
        wt_admin.join("config"),
        "[core]\n\trepositoryformatversion = 0\n",
    )?;
    if grit_lib::reftable::is_reftable_repo(&common) {
        initialize_worktree_reftable_stack(&wt_admin, commit_oid, branch_name.as_deref())?;
    }

    // Write HEAD — either branch or detached
    let detach_head = args.detach || implicit_detach;
    if detach_head {
        let commit_oid = commit_oid.ok_or_else(|| {
            anyhow::anyhow!("HEAD does not point to a valid commit; specify a branch")
        })?;
        fs::write(wt_admin.join("HEAD"), format!("{}\n", commit_oid.to_hex()))?;
    } else {
        let branch_name = branch_name
            .as_deref()
            .ok_or_else(|| anyhow::anyhow!("internal error: missing branch name"))?;
        let commit_oid = commit_oid.ok_or_else(|| {
            anyhow::anyhow!("HEAD does not point to a valid commit; specify a branch")
        })?;
        // Create the branch ref if it doesn't exist yet
        let branch_ref = format!("refs/heads/{}", branch_name);
        let ref_path = common.join(&branch_ref);
        let branch_exists = if grit_lib::reftable::is_reftable_repo(&common) {
            // With reftable there is no loose ref file; consult the stack instead.
            refs::resolve_ref(&common, &branch_ref).is_ok()
        } else {
            ref_path.exists()
        };
        if !branch_exists {
            let identity = crate::commands::update_ref::resolve_reflog_identity(&repo);
            if grit_lib::reftable::is_reftable_repo(&common) {
                // Write the branch ref and its reflog entry in a single reftable
                // transaction so only one table is appended to the main stack
                // (upstream git behaviour). Splitting these into write_ref +
                // append_reflog would append two tables instead.
                grit_lib::reftable::reftable_write_ref(
                    &common,
                    &branch_ref,
                    &commit_oid,
                    Some(&identity),
                    Some("branch: Created from HEAD"),
                )
                .map_err(|e| anyhow::anyhow!("{e}"))?;
            } else {
                refs::write_ref(&common, &branch_ref, &commit_oid)?;
                let _ = refs::append_reflog(
                    &common,
                    &branch_ref,
                    &ObjectId::zero(),
                    &commit_oid,
                    &identity,
                    "branch: Created from HEAD",
                    true,
                );
            }
        } else if args.force == 0 {
            // Branch already exists — check if it's checked out in another worktree
            // (For simplicity, allow it; git also warns but --force overrides)
        }
        if grit_lib::reftable::is_reftable_repo(&common) {
            fs::write(wt_admin.join("HEAD"), "ref: refs/heads/.invalid\n")?;
            fs::write(
                wt_admin.join("refs").join("heads"),
                format!("ref: refs/heads/{}\n", branch_name),
            )?;
            grit_lib::reftable::reftable_write_symref(
                &wt_admin,
                "refs/worktree/refs/heads",
                &format!("refs/heads/{branch_name}"),
                None,
                None,
            )
            .map_err(|e| anyhow::anyhow!("{e}"))?;
        } else {
            fs::write(
                wt_admin.join("HEAD"),
                format!("ref: refs/heads/{}\n", branch_name),
            )?;
        }
    }

    // Lock the worktree if --lock was used
    if args.lock {
        let reason = args.reason.as_deref().unwrap_or("");
        fs::write(wt_admin.join("locked"), format!("{reason}\n"))?;
    }

    if detach_head {
        let commit_oid = commit_oid.ok_or_else(|| {
            anyhow::anyhow!("HEAD does not point to a valid commit; specify a branch")
        })?;
        println!(
            "Preparing worktree (detached HEAD {}) at '{}'",
            &commit_oid.to_hex()[..7],
            display_path.display()
        );
    } else {
        let branch_name = branch_name
            .as_deref()
            .ok_or_else(|| anyhow::anyhow!("internal error: missing branch name"))?;
        println!(
            "Preparing worktree (new branch '{}') at '{}'",
            branch_name,
            display_path.display()
        );
    }

    // Populate the working tree by checking out the commit
    if !args.no_checkout {
        let commit_oid = commit_oid.ok_or_else(|| {
            anyhow::anyhow!("HEAD does not point to a valid commit; specify a branch")
        })?;
        populate_worktree(&repo, &commit_oid, &wt_path, &wt_admin)?;
    }

    crate::commands::sparse_checkout::copy_sparse_checkout_to_admin(&repo.git_dir, &wt_admin)?;
    let common_for_config = worktree::common_git_dir(&repo.git_dir);
    if grit_lib::repo::worktree_config_enabled(&common_for_config) {
        worktree::copy_filtered_worktree_config(&common_for_config, &wt_admin)?;
    } else {
        crate::commands::sparse_checkout::copy_worktree_config_to_admin(&repo.git_dir, &wt_admin)?;
    }

    // A new worktree inherits the main worktree's sparse-checkout patterns (copied above). Apply
    // them to the freshly checked-out tree so out-of-cone paths (e.g. `folder2`) are excluded,
    // matching Git's `worktree add` (t1091 'different sparse-checkouts with worktrees'). Only when
    // the worktree actually has a sparse-checkout file, to avoid touching admin files (and the
    // relative gitdir/commondir links) in the common non-sparse case.
    if !args.no_checkout && wt_admin.join("info").join("sparse-checkout").exists() {
        if let Ok(wt_repo) = Repository::open(&wt_admin, Some(&wt_path)) {
            let _ =
                crate::commands::sparse_checkout::reapply_sparse_checkout_if_configured(&wt_repo);
        }
    }

    if args.track && !args.no_track && !detach_head {
        if let Some(ref new_branch) = branch_name {
            if let Some(start) = args.branch.as_deref() {
                crate::commands::checkout::maybe_setup_tracking(
                    &repo,
                    new_branch,
                    Some(start),
                    Some("direct"),
                )?;
            }
        }
    }

    if !args.no_checkout && !orphan {
        if let Some(commit_oid) = commit_oid {
            run_worktree_add_post_checkout_hook(&repo, &wt_path, &wt_admin, &commit_oid)?;
        }
    }

    Ok(())
}

/// Resolve the path stored in a worktree admin `gitdir` file.
///
/// Relative paths are interpreted relative to the admin directory that contains the
/// `gitdir` file (matches Git's `resolve_gitdir_file`).
fn resolve_gitdir_file_target(gitdir_file: &Path, target_str: &str) -> PathBuf {
    let target_raw = PathBuf::from(target_str);
    let base = gitdir_file.parent().unwrap_or_else(|| Path::new("."));
    let joined = if target_raw.is_absolute() {
        target_raw
    } else {
        base.join(target_raw)
    };
    normalize_path(&joined)
}

/// Normalize a path by resolving `.` and `..` without requiring filesystem existence.
fn normalize_path(path: &std::path::Path) -> std::path::PathBuf {
    use std::path::Component;
    let mut out = std::path::PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                out.pop();
            }
            other => out.push(other.as_os_str()),
        }
    }
    out
}

fn setup_unborn_worktree(
    common: &Path,
    wt_admin: &Path,
    wt_path: &Path,
    display_path: &Path,
    branch_name: &str,
    lock: bool,
    reason: Option<&str>,
) -> Result<()> {
    // Check if the branch already exists
    let branch_ref = format!("refs/heads/{branch_name}");
    if refs::resolve_ref(common, &branch_ref).is_ok() {
        bail!("fatal: a branch named '{}' already exists", branch_name);
    }

    fs::create_dir_all(wt_path)
        .with_context(|| format!("cannot create directory '{}'", wt_path.display()))?;
    fs::create_dir_all(wt_admin)
        .with_context(|| format!("cannot create '{}'", wt_admin.display()))?;

    let gitdir_content = format!("{}\n", wt_path.join(".git").display());
    fs::write(wt_admin.join("gitdir"), &gitdir_content)?;
    let commondir_rel = make_relative_path(wt_admin, common);
    fs::write(
        wt_admin.join("commondir"),
        format!("{}\n", commondir_rel.display()),
    )?;
    fs::write(
        wt_admin.join("config"),
        "[core]\n\trepositoryformatversion = 0\n",
    )?;
    fs::write(
        wt_admin.join("HEAD"),
        format!("ref: refs/heads/{}\n", branch_name),
    )?;
    fs::write(
        wt_path.join(".git"),
        format!("gitdir: {}\n", wt_admin.display()),
    )?;

    if lock {
        fs::write(
            wt_admin.join("locked"),
            format!("{}\n", reason.unwrap_or("")),
        )?;
    }

    println!(
        "Preparing worktree (new branch '{}') at '{}'",
        branch_name,
        display_path.display()
    );
    Ok(())
}

fn worktree_add_display_path(input: &Path, resolved: &Path) -> PathBuf {
    if input.is_absolute() {
        resolved.to_path_buf()
    } else {
        input.to_path_buf()
    }
}

/// Populate a worktree directory with files from a commit.
fn populate_worktree(
    repo: &grit_lib::repo::Repository,
    commit_oid: &ObjectId,
    wt_path: &Path,
    admin_dir: &Path,
) -> Result<()> {
    use grit_lib::objects::parse_commit;
    let odb = &repo.odb;
    // Read the commit to get its tree
    let obj = odb.read(commit_oid).context("reading commit")?;
    let commit = parse_commit(&obj.data).context("parsing commit")?;
    let tree_oid = commit.tree;

    // Checkout files from the tree
    checkout_worktree_tree(odb, &tree_oid, wt_path, "")?;

    // Build and write the index for the new worktree
    let index_path = admin_dir.join("index");
    let mut index = Index::new();
    add_worktree_tree_to_index(odb, &tree_oid, "", &mut index, Some(wt_path))?;
    repo.write_index_at(&index_path, &mut index)
        .context("writing worktree index")?;

    Ok(())
}

/// Recursively check out tree entries to a working directory.
fn checkout_worktree_tree(
    odb: &grit_lib::odb::Odb,
    tree_oid: &ObjectId,
    work_tree: &Path,
    prefix: &str,
) -> Result<()> {
    use grit_lib::objects::parse_tree;

    let obj = odb.read(tree_oid).context("reading tree")?;
    let entries = parse_tree(&obj.data).context("parsing tree")?;

    for entry in &entries {
        let name = String::from_utf8_lossy(&entry.name);
        let path = if prefix.is_empty() {
            name.to_string()
        } else {
            format!("{prefix}/{name}")
        };
        let full_path = work_tree.join(&path);

        let is_tree = (entry.mode & 0o170000) == 0o040000;
        let is_gitlink = entry.mode == 0o160000;
        if is_gitlink {
            continue;
        } else if is_tree {
            fs::create_dir_all(&full_path)?;
            checkout_worktree_tree(odb, &entry.oid, work_tree, &path)?;
        } else {
            if let Some(parent) = full_path.parent() {
                fs::create_dir_all(parent)?;
            }
            let blob = odb
                .read(&entry.oid)
                .with_context(|| format!("reading blob for {path}"))?;
            fs::write(&full_path, &blob.data)?;

            #[cfg(unix)]
            if entry.mode == 0o100755 {
                use std::os::unix::fs::PermissionsExt;
                let perms = fs::Permissions::from_mode(0o755);
                fs::set_permissions(&full_path, perms)?;
            }
        }
    }

    Ok(())
}

/// Recursively add tree entries to an index.
fn add_worktree_tree_to_index(
    odb: &grit_lib::odb::Odb,
    tree_oid: &ObjectId,
    prefix: &str,
    index: &mut grit_lib::index::Index,
    work_tree: Option<&Path>,
) -> Result<()> {
    use grit_lib::objects::parse_tree;

    let obj = odb.read(tree_oid)?;
    let entries = parse_tree(&obj.data)?;

    for entry in &entries {
        let name = String::from_utf8_lossy(&entry.name);
        let path = if prefix.is_empty() {
            name.to_string()
        } else {
            format!("{prefix}/{name}")
        };

        let is_tree = (entry.mode & 0o170000) == 0o040000;
        let is_gitlink = entry.mode == 0o160000;
        if is_tree {
            add_worktree_tree_to_index(odb, &entry.oid, &path, index, work_tree)?;
        } else if is_gitlink {
            index.add_or_replace(IndexEntry {
                ctime_sec: 0,
                ctime_nsec: 0,
                mtime_sec: 0,
                mtime_nsec: 0,
                dev: 0,
                ino: 0,
                mode: 0o160000,
                uid: 0,
                gid: 0,
                size: 0,
                oid: entry.oid,
                flags: path.len().min(0xfff) as u16,
                flags_extended: None,
                path: path.into_bytes(),
                base_index_pos: 0,
            });
        } else {
            // Stat the file from the work tree if available
            let (mtime_sec, mtime_nsec, file_size) = if let Some(wt) = work_tree {
                let p = wt.join(&path);
                if let Ok(meta) = fs::metadata(&p) {
                    use std::time::UNIX_EPOCH;
                    let mtime = meta.modified().unwrap_or(UNIX_EPOCH);
                    let dur = mtime.duration_since(UNIX_EPOCH).unwrap_or_default();
                    (dur.as_secs() as u32, dur.subsec_nanos(), meta.len() as u32)
                } else {
                    (0, 0, 0)
                }
            } else {
                (0, 0, 0)
            };

            index.add_or_replace(IndexEntry {
                ctime_sec: mtime_sec,
                ctime_nsec: mtime_nsec,
                mtime_sec,
                mtime_nsec,
                dev: 0,
                ino: 0,
                mode: entry.mode,
                uid: 0,
                gid: 0,
                size: file_size,
                flags_extended: None,
                oid: entry.oid,
                flags: path.len().min(0xfff) as u16,
                path: path.into_bytes(),
                base_index_pos: 0,
            });
        }
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// worktree list
// ---------------------------------------------------------------------------

fn collect_worktrees(repo: &Repository) -> Result<Vec<WorktreeEntry>> {
    worktree::list_worktrees(repo).map_err(Into::into)
}

/// C-quote a path string when it contains non-ASCII characters (core.quotepath behavior).
fn quote_path_if_needed(path: &str, quotepath: bool) -> String {
    if !quotepath {
        return path.to_string();
    }
    let needs_quoting = path.bytes().any(|b| !(0x20..=0x7f).contains(&b));
    if !needs_quoting {
        return path.to_string();
    }
    let mut out = String::from('"');
    for b in path.bytes() {
        if b > 0x7f {
            out.push_str(&format!("\\{:03o}", b));
        } else if b < 0x20 {
            match b {
                b'\n' => out.push_str("\\n"),
                b'\t' => out.push_str("\\t"),
                _ => out.push_str(&format!("\\{:03o}", b)),
            }
        } else {
            out.push(b as char);
        }
    }
    out.push('"');
    out
}

fn cmd_list(args: ListArgs) -> Result<()> {
    let repo = Repository::discover(None)?;
    // -z requires --porcelain
    if args.nul && !args.porcelain {
        bail!("--null requires --porcelain");
    }
    let entries = collect_worktrees(&repo)?;

    // Read core.quotepath from config (default: true)
    // Use common_dir so we find config from linked worktrees too
    let quotepath = {
        let common = common_dir(&repo.git_dir).unwrap_or(repo.git_dir.clone());
        let cfg = grit_lib::config::ConfigSet::load(Some(&common), true).unwrap_or_default();
        cfg.get_bool("core.quotepath")
            .and_then(|r| r.ok())
            .unwrap_or(true)
    };

    let stdout = std::io::stdout();
    let mut out = stdout.lock();

    if args.porcelain {
        // With -z, use NUL between fields and between entries; without -z use newlines.
        let sep: u8 = if args.nul { 0 } else { b'\n' };
        let entry_sep: &[u8] = if args.nul { b"\0" } else { b"\n" };
        for entry in &entries {
            out.write_all(format!("worktree {}", entry.path.display()).as_bytes())?;
            out.write_all(&[sep])?;
            if !entry.is_bare {
                let head_oid = match &entry.head {
                    HeadState::Branch { oid: Some(oid), .. } => oid.to_hex(),
                    HeadState::Detached { oid } => oid.to_hex(),
                    _ => "0".repeat(40),
                };
                out.write_all(format!("HEAD {head_oid}").as_bytes())?;
                out.write_all(&[sep])?;
                match &entry.head {
                    HeadState::Branch { refname, .. } => {
                        out.write_all(format!("branch {refname}").as_bytes())?;
                        out.write_all(&[sep])?;
                    }
                    HeadState::Detached { .. } => {
                        out.write_all(b"detached")?;
                        out.write_all(&[sep])?;
                    }
                    _ => {}
                }
            }
            if entry.is_bare {
                out.write_all(b"bare")?;
                out.write_all(&[sep])?;
            }
            if entry.is_locked {
                if let Some(ref reason) = entry.lock_reason {
                    // Quote and escape the reason if it contains newlines
                    if reason.contains('\n') || reason.contains('\r') {
                        let escaped = reason.replace('\r', "\\r").replace('\n', "\\n");
                        out.write_all(format!("locked \"{escaped}\"").as_bytes())?;
                    } else {
                        out.write_all(format!("locked {reason}").as_bytes())?;
                    }
                } else {
                    out.write_all(b"locked")?;
                }
                out.write_all(&[sep])?;
            }
            // prunable: worktree path no longer exists on disk
            if !entry.is_bare && !entry.path.exists() {
                out.write_all(b"prunable gitdir file points to non-existent location")?;
                out.write_all(&[sep])?;
            }
            out.write_all(entry_sep)?;
        }
    } else {
        // Compute max path display width for column alignment (min 40)
        // Use quoted path length when quotepath is active
        let max_path_len = entries
            .iter()
            .map(|e| {
                quote_path_if_needed(&e.path.display().to_string(), quotepath)
                    .chars()
                    .count()
            })
            .max()
            .unwrap_or(0)
            .max(40);
        for entry in &entries {
            // Bare repos don't show a SHA in the non-porcelain output
            let sha = if entry.is_bare {
                String::new()
            } else {
                match &entry.head {
                    HeadState::Branch { oid: Some(oid), .. } => oid.to_hex()[..7].to_string(),
                    HeadState::Detached { oid } => oid.to_hex()[..7].to_string(),
                    _ => "0000000".to_string(),
                }
            };

            let branch_info = if entry.is_bare {
                "(bare)".to_string()
            } else {
                match &entry.head {
                    HeadState::Branch { short_name, .. } => {
                        format!("[{}]", short_name)
                    }
                    HeadState::Detached { .. } => "(detached HEAD)".to_string(),
                    HeadState::Invalid => "(error)".to_string(),
                }
            };

            // In verbose mode, locks with reasons are shown on separate lines (not as suffix)
            let lock_marker = if entry.is_locked {
                if args.verbose && entry.lock_reason.is_some() {
                    ""
                } else {
                    " locked"
                }
            } else {
                ""
            };
            // "prunable" annotation for worktrees whose path no longer exists
            // In verbose mode, prunable info goes on a separate line
            let is_prunable = !entry.is_bare && !entry.path.exists();
            let prunable_marker = if is_prunable && !args.verbose {
                " prunable"
            } else {
                ""
            };
            let path_str = quote_path_if_needed(&entry.path.display().to_string(), quotepath);
            if entry.is_bare {
                writeln!(
                    out,
                    "{:<width$} {}{}{}",
                    path_str,
                    branch_info,
                    lock_marker,
                    prunable_marker,
                    width = max_path_len,
                )?;
            } else {
                writeln!(
                    out,
                    "{:<width$} {} {}{}{}",
                    path_str,
                    sha,
                    branch_info,
                    lock_marker,
                    prunable_marker,
                    width = max_path_len,
                )?;
            }
            // In verbose mode, show lock reason and prunable details
            if args.verbose {
                if entry.is_locked {
                    if let Some(ref reason) = entry.lock_reason {
                        writeln!(out, "\tlocked: {reason}")?;
                    }
                }
                if is_prunable {
                    writeln!(
                        out,
                        "\tprunable: gitdir file points to non-existent location"
                    )?;
                }
            }
        }
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// worktree remove
// ---------------------------------------------------------------------------

fn cmd_remove(args: RemoveArgs) -> Result<()> {
    let repo = Repository::discover(None)?;
    let common = common_dir(&repo.git_dir)?;
    let worktrees_dir = common.join("worktrees");

    let wt_path = if args.path.is_absolute() {
        args.path.clone()
    } else {
        std::env::current_dir()?.join(&args.path)
    };
    let wt_path = wt_path.canonicalize().unwrap_or(wt_path);
    let display_path = worktree_add_display_path(&args.path, &wt_path);

    // Find the matching admin entry
    let wt_name = find_worktree_name(&worktrees_dir, &wt_path)?;
    let admin = worktrees_dir.join(&wt_name);

    // Check for lock
    // Locked: needs --force --force (force >= 2) to remove
    if admin.join("locked").exists() && args.force < 2 {
        if args.force >= 1 {
            bail!(
                "worktree '{}' is locked; use 'git worktree remove --force --force'",
                display_path.display()
            );
        }
        bail!(
            "worktree '{}' is locked; use --force or unlock it first",
            display_path.display()
        );
    }

    if args.force < 1 && has_initialized_submodule(&wt_path, &admin) {
        bail!("working trees containing submodules cannot be moved or removed");
    }

    // Check for dirty/untracked files unless --force >= 1
    if args.force < 1 && wt_path.exists() {
        // Load the linked worktree's index (stored in the admin directory). Open a Repository
        // SCOPED TO THE WORKTREE BEING REMOVED (git_dir = admin, work_tree = wt_path) rather than
        // reusing the discovered (main) repo: `load_index_at` runs
        // `clear_skip_worktree_from_present_files`, which clears the skip-worktree bit for any
        // sparse entry whose file is present in the repo's work_tree. With the main repo's work
        // tree those files exist, so the bits are wrongly cleared and a sparse worktree (whose
        // out-of-cone files are intentionally absent) is misreported as dirty
        // (t1091 'worktree: add copies sparse-checkout patterns'). Scoping to wt_path keeps the
        // skip-worktree bits, so `has_dirty_files` correctly skips those entries.
        let index_path = admin.join("index");
        if index_path.exists() {
            let wt_repo = Repository::open(&admin, Some(&wt_path)).ok();
            let load = match wt_repo {
                Some(ref r) => r.load_index_at(&index_path),
                None => repo.load_index_at(&index_path),
            };
            if let Ok(index) = load {
                // Check for untracked files
                if has_untracked_files(&wt_path, &index) {
                    bail!("worktree '{}' contains modified or untracked files; use --force to delete it", display_path.display());
                }
                // Check for dirty tracked files
                if has_dirty_files(&wt_path, &index, &repo) {
                    bail!("worktree '{}' contains modified or untracked files; use --force to delete it", display_path.display());
                }
            }
        }
    }

    // Remove the working tree directory
    if wt_path.exists() {
        fs::remove_dir_all(&wt_path)
            .with_context(|| format!("cannot remove '{}'", wt_path.display()))?;
    }

    // Remove the admin directory
    if admin.exists() {
        fs::remove_dir_all(&admin)
            .with_context(|| format!("cannot remove admin dir '{}'", admin.display()))?;
    }

    // If .git/worktrees is now empty, remove it too
    if worktrees_dir.exists() {
        if let Ok(mut entries) = fs::read_dir(&worktrees_dir) {
            if entries.next().is_none() {
                let _ = fs::remove_dir(&worktrees_dir);
            }
        }
    }

    Ok(())
}

/// Find a worktree admin directory name by matching the path recorded in its
/// `gitdir` file.
/// Whether a stale worktree (working `.git` gone) is past its expiry, matching
/// git's `should_prune_worktree`: prune when the admin `index` is missing or
/// its mtime is `<= expire`. `expire` is parsed with the full approxidate
/// grammar ("now", "1.week.ago", etc.).
fn worktree_index_expired(admin: &Path, expire: &str) -> bool {
    let expire_secs = grit_lib::git_date::approx::approxidate_careful(expire, None) as i64;
    match std::fs::metadata(admin.join("index")).and_then(|m| m.modified()) {
        Ok(mtime) => {
            let secs = mtime
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs() as i64)
                .unwrap_or(0);
            secs <= expire_secs
        }
        Err(_) => true,
    }
}

/// Check if a directory contains an initialized submodule (has .git directory inside).
fn has_initialized_submodule(wt_path: &Path, wt_git_dir: &Path) -> bool {
    if wt_git_dir.join("modules").is_dir() {
        return true;
    }
    walk_for_submodule(wt_path, wt_path)
}

fn walk_for_submodule(base: &Path, dir: &Path) -> bool {
    let Ok(read_dir) = std::fs::read_dir(dir) else {
        return false;
    };
    for entry in read_dir.flatten() {
        let path = entry.path();
        if path.file_name().map(|n| n == ".git").unwrap_or(false) && path != base.join(".git") {
            // Initialized submodules use a `.git` file or directory inside the submodule.
            return true;
        }
        if path.is_dir()
            && path.file_name().map(|n| n != ".git").unwrap_or(true)
            && walk_for_submodule(base, &path)
        {
            return true;
        }
    }
    false
}

/// Check if a worktree has untracked files.
fn has_untracked_files(work_tree: &Path, index: &grit_lib::index::Index) -> bool {
    let staged: std::collections::HashSet<Vec<u8>> = index
        .entries
        .iter()
        .filter(|e| e.stage() == 0)
        .map(|e| e.path.clone())
        .collect();
    walk_for_untracked(work_tree, work_tree, &staged)
}

fn walk_for_untracked(
    base: &Path,
    dir: &Path,
    staged: &std::collections::HashSet<Vec<u8>>,
) -> bool {
    let Ok(read_dir) = std::fs::read_dir(dir) else {
        return false;
    };
    for entry in read_dir.flatten() {
        let path = entry.path();
        let name = entry.file_name();
        if name == ".git" {
            continue;
        }
        if path.is_dir() {
            // Skip submodule directories (tracked as gitlinks in the index)
            if let Ok(rel) = path.strip_prefix(base) {
                let rel_bytes = rel.to_string_lossy().as_bytes().to_vec();
                if staged.contains(&rel_bytes) {
                    // This dir is a gitlink entry — skip it (submodule)
                    continue;
                }
            }
            if walk_for_untracked(base, &path, staged) {
                return true;
            }
        } else if path.is_file() {
            if let Ok(rel) = path.strip_prefix(base) {
                let rel_bytes = rel.to_string_lossy().as_bytes().to_vec();
                if !staged.contains(&rel_bytes) {
                    return true;
                }
            }
        }
    }
    false
}

/// Check if a worktree has dirty tracked files.
fn has_dirty_files(
    work_tree: &Path,
    index: &grit_lib::index::Index,
    _repo: &grit_lib::repo::Repository,
) -> bool {
    for entry in &index.entries {
        if entry.stage() != 0 {
            continue;
        }
        // Skip gitlinks (submodules) — they have special handling
        if entry.mode == 0o160000 {
            continue;
        }
        // Sparse-checkout entries (skip-worktree / sparse-directory placeholders) are intentionally
        // absent from the work tree; their missing files must not count as "dirty" or `worktree
        // remove` of a sparse worktree would always fail (t1091 'worktree: add copies patterns').
        if entry.skip_worktree() || entry.is_sparse_directory_placeholder() {
            continue;
        }
        let rel = String::from_utf8_lossy(&entry.path);
        let abs = work_tree.join(rel.as_ref());
        match std::fs::read(&abs) {
            Ok(data) => {
                let oid = grit_lib::odb::Odb::hash_object_data(
                    grit_lib::objects::ObjectKind::Blob,
                    &data,
                );
                if oid != entry.oid {
                    return true;
                }
            }
            Err(_) => return true, // missing file = dirty
        }
    }
    false
}

fn find_worktree_name(worktrees_dir: &Path, target: &Path) -> Result<String> {
    if !worktrees_dir.is_dir() {
        bail!("no linked worktrees found");
    }

    // Also try matching by basename directly
    if let Some(basename) = target.file_name().and_then(|n| n.to_str()) {
        let candidate = worktrees_dir.join(basename);
        if candidate.is_dir() {
            // Verify gitdir points to the right place
            let gitdir_file = candidate.join("gitdir");
            if gitdir_file.exists() {
                let raw = fs::read_to_string(&gitdir_file).unwrap_or_default();
                let recorded_raw = PathBuf::from(raw.trim());
                // Resolve relative paths against the admin dir
                let recorded = if recorded_raw.is_relative() {
                    candidate.join(&recorded_raw)
                } else {
                    recorded_raw
                };
                let recorded_normalized = normalize_path(&recorded);
                let recorded_wt = recorded_normalized
                    .parent()
                    .unwrap_or(&recorded_normalized)
                    .to_path_buf();
                let recorded_wt_canonical = recorded_wt.canonicalize().unwrap_or(recorded_wt);
                if recorded_wt_canonical == target {
                    return Ok(basename.to_string());
                }
            }
            // If gitdir doesn't match, still use basename as the name
            return Ok(basename.to_string());
        }
    }

    // Scan all entries
    for entry in fs::read_dir(worktrees_dir)? {
        let entry = entry?;
        if !entry.file_type()?.is_dir() {
            continue;
        }
        let admin_dir = entry.path();
        let gitdir_file = admin_dir.join("gitdir");
        if !gitdir_file.exists() {
            continue;
        }
        let raw = fs::read_to_string(&gitdir_file).unwrap_or_default();
        let recorded_raw = PathBuf::from(raw.trim());
        // Resolve relative paths against the admin dir
        let recorded = if recorded_raw.is_relative() {
            normalize_path(&admin_dir.join(&recorded_raw))
        } else {
            recorded_raw
        };
        let recorded_wt = recorded.parent().unwrap_or(&recorded).to_path_buf();
        let recorded_wt_canonical = recorded_wt.canonicalize().unwrap_or(recorded_wt);
        if recorded_wt_canonical == target {
            return Ok(entry.file_name().to_string_lossy().to_string());
        }
    }

    bail!("'{}' is not a working tree", target.display());
}

// ---------------------------------------------------------------------------
// worktree prune
// ---------------------------------------------------------------------------

fn cmd_prune(args: PruneArgs) -> Result<()> {
    let repo = Repository::discover(None)?;
    let common = common_dir(&repo.git_dir)?;
    let worktrees_dir = common.join("worktrees");

    if !worktrees_dir.is_dir() {
        return Ok(());
    }

    for entry in fs::read_dir(&worktrees_dir)? {
        let entry = entry?;
        let file_type = entry.file_type()?;
        let admin = entry.path();
        let name = entry.file_name().to_string_lossy().to_string();

        // Files inside worktrees/ are invalid
        if !file_type.is_dir() {
            if args.verbose || args.dry_run {
                eprintln!("Removing worktrees/{name}: not a valid directory");
            }
            if !args.dry_run {
                let _ = fs::remove_file(&admin);
            }
            continue;
        }

        // A worktree is stale if its gitdir target no longer exists
        let gitdir_file = admin.join("gitdir");
        let (is_stale, stale_reason) = if !gitdir_file.exists() {
            (true, "gitdir file does not exist")
        } else {
            match fs::read_to_string(&gitdir_file) {
                Err(_) => (true, "unable to read gitdir file"),
                Ok(raw) => {
                    let target_str = raw.trim();
                    if target_str.is_empty() {
                        (true, "invalid gitdir file")
                    } else {
                        let target = resolve_gitdir_file_target(&gitdir_file, target_str);
                        if !target.exists() {
                            // Check if the worktree path is the same as the main worktree path
                            // (e.g. after main repo was moved to where the linked wt was).
                            // In that case the linked worktree's gitdir now points to main's git dir.
                            let wt_path_file = admin.join("gitdir");
                            let _ = wt_path_file; // suppress warning
                                                  // The wt_name path: check if worktrees/<name> path matches main worktree
                            let main_wt = repo.work_tree.as_deref();
                            let wt_abs = admin
                                .parent()
                                .and_then(|p| p.parent())
                                .map(|p| p.join(&name));
                            let wt_abs_canonical =
                                wt_abs.as_ref().and_then(|p| p.canonicalize().ok());
                            let main_wt_canonical = main_wt.and_then(|p| p.canonicalize().ok());
                            let is_dup = match (wt_abs_canonical, main_wt_canonical) {
                                (Some(a), Some(b)) => a == b,
                                _ => false,
                            };
                            if is_dup {
                                (true, "duplicate entry")
                            } else {
                                (true, "gitdir file points to non-existent location")
                            }
                        } else {
                            (false, "")
                        }
                    }
                }
            }
        };

        if !is_stale {
            continue; // Not stale, keep it
        }

        // Stale: when the only problem is that the working `.git` is gone
        // ("non-existent location"), Git keeps the worktree until its admin
        // `index` is older than --expire (git should_prune_worktree). Other
        // stale reasons (missing/invalid gitdir, duplicate) are pruned
        // unconditionally.
        if stale_reason == "gitdir file points to non-existent location" {
            if let Some(ref expire_str) = args.expire {
                if !worktree_index_expired(&admin, expire_str) {
                    continue; // index is newer than the expiry; keep it
                }
            }
        }

        // Skip locked worktrees
        if admin.join("locked").exists() {
            if args.verbose {
                eprintln!("worktree '{name}' is locked; not pruning");
            }
            continue;
        }

        if args.verbose || args.dry_run {
            eprintln!("Removing worktrees/{name}: {stale_reason}");
        }

        if !args.dry_run {
            fs::remove_dir_all(&admin)
                .with_context(|| format!("cannot remove '{}'", admin.display()))?;
        }
    }

    // Check for duplicate entries (multiple admins pointing to same gitdir)
    if worktrees_dir.is_dir() {
        let mut gitdir_targets: std::collections::HashMap<PathBuf, String> =
            std::collections::HashMap::new();
        let mut all_entries: Vec<String> = fs::read_dir(&worktrees_dir)
            .map(|d| {
                d.filter_map(|e| e.ok())
                    .filter(|e| e.file_type().map(|t| t.is_dir()).unwrap_or(false))
                    .map(|e| e.file_name().to_string_lossy().to_string())
                    .collect()
            })
            .unwrap_or_default();
        all_entries.sort();
        for name in &all_entries {
            let admin = worktrees_dir.join(name);
            let gitdir_file = admin.join("gitdir");
            if let Ok(raw) = fs::read_to_string(&gitdir_file) {
                let target_normalized = resolve_gitdir_file_target(&gitdir_file, raw.trim());
                let target_canonical = target_normalized
                    .canonicalize()
                    .unwrap_or(target_normalized.clone());
                // Duplicate check: target points to same place as another linked worktree
                // or as the main git_dir (e.g. after main repo moved to old linked wt path)
                let main_gitdir_canonical =
                    repo.git_dir.canonicalize().unwrap_or(repo.git_dir.clone());
                let is_dup_of_main = target_canonical == main_gitdir_canonical;
                if is_dup_of_main {
                    if args.verbose || args.dry_run {
                        eprintln!("Removing worktrees/{name}: duplicate entry");
                    }
                    if !args.dry_run {
                        let _ = fs::remove_dir_all(&admin);
                    }
                    continue;
                }
                if let Some(first_name) = gitdir_targets.get(&target_canonical) {
                    if first_name != name {
                        // Duplicate! Remove this one.
                        if args.verbose || args.dry_run {
                            eprintln!("Removing worktrees/{name}: duplicate entry");
                        }
                        if !args.dry_run {
                            let _ = fs::remove_dir_all(&admin);
                        }
                    }
                } else {
                    gitdir_targets.insert(target_canonical, name.clone());
                }
            }
        }
    }

    // If .git/worktrees is now empty, remove it too
    if !args.dry_run && worktrees_dir.exists() {
        if let Ok(mut entries) = fs::read_dir(&worktrees_dir) {
            if entries.next().is_none() {
                let _ = fs::remove_dir(&worktrees_dir);
            }
        }
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// worktree move
// ---------------------------------------------------------------------------

fn cmd_move(args: MoveArgs) -> Result<()> {
    let repo = Repository::discover(None)?;
    let common = common_dir(&repo.git_dir)?;
    let worktrees_dir = common.join("worktrees");

    let src_path = if args.source.is_absolute() {
        args.source.clone()
    } else {
        std::env::current_dir()?.join(&args.source)
    };
    let src_path = src_path.canonicalize().unwrap_or(src_path);

    // Find the admin entry for the source worktree
    let wt_name = find_worktree_name(&worktrees_dir, &src_path)?;
    let admin = worktrees_dir.join(&wt_name);

    // Check for lock
    if admin.join("locked").exists() && args.force < 2 {
        if args.force >= 1 {
            bail!(
                "worktree '{}' is locked; use 'git worktree move --force --force' to force",
                src_path.display()
            );
        }
        bail!(
            "worktree '{}' is locked; use --force to move it anyway",
            src_path.display()
        );
    }

    // Determine the destination absolute path
    let dst_path = if args.destination.is_absolute() {
        args.destination.clone()
    } else {
        std::env::current_dir()?.join(&args.destination)
    };

    // If destination is an existing directory, append the source basename
    let dst_path = if dst_path.exists() && dst_path.is_dir() {
        let src_name = src_path
            .file_name()
            .ok_or_else(|| anyhow::anyhow!("cannot determine source name"))?;
        dst_path.join(src_name)
    } else {
        dst_path
    };

    // If destination is registered as a worktree (but possibly missing from disk), require --force
    let dst_canonical = dst_path.canonicalize().unwrap_or(dst_path.clone());
    let is_registered_wt = worktrees_dir.is_dir() && {
        std::fs::read_dir(&worktrees_dir)
            .ok()
            .map(|entries| {
                entries.flatten().any(|e| {
                    let gitdir_file = e.path().join("gitdir");
                    if let Ok(raw) = std::fs::read_to_string(&gitdir_file) {
                        let p = std::path::Path::new(raw.trim());
                        let wt = p.parent().unwrap_or(p);
                        wt.canonicalize().unwrap_or(wt.to_path_buf()) == dst_canonical
                    } else {
                        false
                    }
                })
            })
            .unwrap_or(false)
    };
    if !dst_path.exists() && is_registered_wt && args.force < 1 {
        bail!(
            "'{}' is a missing but registered worktree; use --force to overwrite",
            dst_path.display()
        );
    }

    if dst_path.exists() {
        bail!("target '{}' already exists", dst_path.display());
    }

    // Check for initialized submodules (cannot move a worktree with active submodules)
    let src_admin = worktrees_dir.join(&find_worktree_name(&worktrees_dir, &src_path)?);
    if args.force < 1 && has_initialized_submodule(&src_path, &src_admin) {
        bail!("working trees containing submodules cannot be moved or removed");
    }

    // Move the working tree directory
    fs::rename(&src_path, &dst_path).with_context(|| {
        format!(
            "cannot move '{}' to '{}'",
            src_path.display(),
            dst_path.display()
        )
    })?;

    let dst_path = dst_path.canonicalize().unwrap_or(dst_path);

    // Determine if we should use relative paths
    let use_relative = if args.relative_paths {
        true
    } else if args.no_relative_paths {
        false
    } else {
        let cfg = grit_lib::config::ConfigSet::load(Some(&common), true).unwrap_or_default();
        cfg.get_bool("worktree.useRelativePaths")
            .and_then(|r| r.ok())
            .unwrap_or(false)
    };

    // Update the gitdir file in the admin dir to point to the new location
    let new_gitdir_content = if use_relative {
        let rel = make_relative_path(&admin, &dst_path.join(".git"));
        format!("{}\n", rel.display())
    } else {
        format!("{}\n", dst_path.join(".git").display())
    };
    fs::write(admin.join("gitdir"), &new_gitdir_content)?;

    // Update the .git file in the moved worktree
    let dotgit_content = if use_relative {
        let rel = make_relative_path(&dst_path, &admin);
        format!("gitdir: {}\n", rel.display())
    } else {
        format!("gitdir: {}\n", admin.display())
    };
    fs::write(dst_path.join(".git"), &dotgit_content)?;

    Ok(())
}

fn use_relative_worktree_paths(
    args_relative: bool,
    args_no_relative: bool,
    config: &ConfigSet,
) -> bool {
    if args_relative {
        return true;
    }
    if args_no_relative {
        return false;
    }
    config
        .get_bool("worktree.useRelativePaths")
        .and_then(|r| r.ok())
        .unwrap_or(false)
}

fn enable_relative_worktrees_extension(common: &Path) -> Result<()> {
    let cfg_path = common.join("config");
    let mut content = fs::read_to_string(&cfg_path).unwrap_or_default();
    if content.contains("relativeWorktrees") || content.contains("relativeworktrees") {
        return Ok(());
    }
    if content.contains("repositoryformatversion = 0") {
        content = content.replace("repositoryformatversion = 0", "repositoryformatversion = 1");
    }
    content.push_str("\n[extensions]\n\trelativeWorktrees = true\n");
    fs::write(&cfg_path, content)?;
    Ok(())
}

fn write_worktree_linking_files(wt_path: &Path, wt_admin: &Path, use_relative: bool) -> Result<()> {
    let dot_git = wt_path.join(".git");
    if use_relative {
        let gitdir_rel = make_relative_path(wt_admin, &dot_git);
        fs::write(
            wt_admin.join("gitdir"),
            format!("{}\n", gitdir_rel.display()),
        )?;
        let dotgit_rel = make_relative_path(wt_path, wt_admin);
        fs::write(dot_git, format!("gitdir: {}\n", dotgit_rel.display()))?;
    } else {
        let wt_abs = path_for_git_storage(wt_path);
        let dot_git_abs = wt_abs.join(".git");
        let admin_abs = path_for_git_storage(wt_admin);
        fs::write(
            wt_admin.join("gitdir"),
            format!("{}\n", dot_git_abs.display()),
        )?;
        fs::write(dot_git, format!("gitdir: {}\n", admin_abs.display()))?;
    }
    Ok(())
}

/// Compute the relative path from `from` (a directory) to `to`.
fn make_relative_path(from: &std::path::Path, to: &std::path::Path) -> PathBuf {
    let from_abs = from.canonicalize().unwrap_or(from.to_path_buf());
    let to_abs = to.canonicalize().unwrap_or(to.to_path_buf());
    let from_comps: Vec<_> = from_abs.components().collect();
    let to_comps: Vec<_> = to_abs.components().collect();
    let common_len = from_comps
        .iter()
        .zip(to_comps.iter())
        .take_while(|(a, b)| a == b)
        .count();
    let up = from_comps.len() - common_len;
    let mut result = PathBuf::new();
    for _ in 0..up {
        result.push("..");
    }
    for comp in &to_comps[common_len..] {
        result.push(comp.as_os_str());
    }
    result
}

/// Canonicalize for storage in gitdir/gitfile paths (matches Git `strbuf_realpath` on macOS).
fn path_for_git_storage(path: &Path) -> PathBuf {
    let canon = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
    #[cfg(target_os = "macos")]
    {
        if let Ok(stripped) = canon.strip_prefix("/private") {
            let without_private = PathBuf::from("/").join(stripped);
            if without_private.exists() {
                return without_private;
            }
        }
    }
    canon
}

fn repair_use_relative_paths(args: &RepairArgs, common: &Path) -> bool {
    if args.relative_paths {
        return true;
    }
    if args.no_relative_paths {
        return false;
    }
    let cfg = ConfigSet::load(Some(common), true).unwrap_or_default();
    cfg.get_bool("worktree.useRelativePaths")
        .and_then(|r| r.ok())
        .unwrap_or(false)
}

/// Extract `worktrees/<id>` basename from an admin or gitdir path.
fn worktree_id_from_path(path: &Path) -> Option<String> {
    let mut saw_worktrees = false;
    let mut id = None;
    for comp in path.components() {
        if saw_worktrees {
            id = comp.as_os_str().to_str().map(String::from);
            break;
        }
        if comp.as_os_str() == "worktrees" {
            saw_worktrees = true;
        }
    }
    id
}

/// Infer `<common>/worktrees/<id>` from a worktree `.git` gitfile (Git `infer_backlink`).
fn infer_worktree_admin_from_gitfile(
    worktrees_dir: &Path,
    _gitfile: &Path,
    content: &str,
) -> Option<PathBuf> {
    let line = content.trim();
    let target = line.strip_prefix("gitdir: ")?.trim();
    let target_path = PathBuf::from(target);
    let id = worktree_id_from_path(&target_path)?;
    let admin = worktrees_dir.join(&id);
    if admin.is_dir() {
        Some(admin)
    } else {
        None
    }
}

/// Resolve the admin directory a worktree `.git` gitfile points at.
fn resolve_gitfile_backlink(gitfile: &Path, content: &str) -> Option<PathBuf> {
    let line = content.trim();
    let target = line.strip_prefix("gitdir: ")?.trim();
    if target.is_empty() {
        return None;
    }
    let target_path = PathBuf::from(target);
    let resolved = if target_path.is_absolute() {
        path_for_git_storage(&target_path)
    } else {
        let wt_root = gitfile.parent()?;
        path_for_git_storage(&wt_root.join(target_path))
    };
    Some(resolved)
}

fn repair_exit_error(path: &Path, msg: &str) -> Result<()> {
    eprintln!("error: '{}': {}", path.display(), msg);
    std::process::exit(1);
}

/// Repair a single worktree path (Git `repair_worktree_at_path`).
fn repair_worktree_at_path(
    common: &Path,
    worktrees_dir: &Path,
    path: &Path,
    args: &RepairArgs,
) -> Result<()> {
    let wt_root = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()?.join(path)
    };
    let wt_root = path_for_git_storage(&wt_root);
    let dot_git = wt_root.join(".git");

    if !wt_root.is_dir() {
        repair_exit_error(&wt_root, "not a valid path")?;
    }
    if dot_git.is_dir() {
        repair_exit_error(&dot_git, ".git is not a file")?;
    }
    if !dot_git.is_file() {
        repair_exit_error(&dot_git, ".git file broken")?;
    }

    let content = fs::read_to_string(&dot_git).unwrap_or_default();
    if !content.trim().starts_with("gitdir: ") {
        repair_exit_error(&dot_git, ".git file broken")?;
    }

    let inferred_admin = infer_worktree_admin_from_gitfile(worktrees_dir, &dot_git, &content);
    let mut backlink = resolve_gitfile_backlink(&dot_git, &content);
    if backlink.is_none() {
        if let Some(ref admin) = inferred_admin {
            backlink = Some(admin.clone());
        } else {
            repair_exit_error(&dot_git, ".git file broken")?;
        }
    }
    let backlink = backlink.ok_or_else(|| anyhow::anyhow!("internal error: missing backlink"))?;

    let backlink = if let Some(ref inferred) = inferred_admin {
        let inferred_canon = path_for_git_storage(inferred);
        let backlink_canon = path_for_git_storage(&backlink);
        if inferred_canon != backlink_canon {
            inferred_canon
        } else {
            backlink_canon
        }
    } else {
        path_for_git_storage(&backlink)
    };

    if !backlink.starts_with(worktrees_dir) {
        repair_exit_error(&dot_git, ".git file does not reference a repository")?;
    }

    let gitdir_file = backlink.join("gitdir");
    let use_relative = repair_use_relative_paths(args, common);
    let dot_git_expected = path_for_git_storage(&wt_root.join(".git"));

    let repair_reason = if !gitdir_file.is_file() {
        Some("gitdir unreadable")
    } else {
        let raw = fs::read_to_string(&gitdir_file).unwrap_or_default();
        let recorded = resolve_gitdir_file_target(&gitdir_file, raw.trim());
        let recorded_dotgit = path_for_git_storage(&recorded);
        if recorded_dotgit != dot_git_expected {
            Some("gitdir incorrect")
        } else {
            None
        }
    };

    if let Some(reason) = repair_reason {
        write_worktree_linking_files(&wt_root, &backlink, use_relative)?;
        eprintln!(
            "repair: {}: {reason}: {}",
            wt_root.display(),
            gitdir_file.display()
        );
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// worktree repair
// ---------------------------------------------------------------------------

fn cmd_repair(args: RepairArgs) -> Result<()> {
    let repo = Repository::discover(None)?;
    let common = path_for_git_storage(&common_dir(&repo.git_dir)?);
    let worktrees_dir = common.join("worktrees");
    let repo_git_dir = path_for_git_storage(&repo.git_dir);

    // Implicit repair: when running from a linked worktree without explicit paths,
    // detect if the admin dir's gitdir still points to the OLD path.
    if args.paths.is_empty() && repo_git_dir != common {
        // We're in a linked worktree (git_dir is under worktrees/)
        if repo_git_dir.starts_with(&worktrees_dir) {
            let admin = repo_git_dir.as_path();
            let gitdir_file = admin.join("gitdir");
            if let Some(ref wt) = repo.work_tree {
                let wt_canonical = path_for_git_storage(wt);
                if let Ok(raw) = fs::read_to_string(&gitdir_file) {
                    let recorded = resolve_gitdir_file_target(&gitdir_file, raw.trim());
                    let recorded_wt = recorded.parent().unwrap_or(&recorded).to_path_buf();
                    let recorded_canonical = path_for_git_storage(&recorded_wt);
                    if recorded_canonical != wt_canonical {
                        // Admin gitdir points to wrong location — repair it
                        let use_rel = repair_use_relative_paths(&args, &common);
                        write_worktree_linking_files(wt, admin, use_rel)?;
                        eprintln!(
                            "repair: {}: gitdir incorrect: {}",
                            wt.display(),
                            gitdir_file.display()
                        );
                    }
                }
            }
        }
    }

    if !args.paths.is_empty() {
        for p in &args.paths {
            repair_worktree_at_path(&common, &worktrees_dir, p, &args)?;
        }
        return Ok(());
    }

    if !worktrees_dir.is_dir() {
        return Ok(());
    }

    let entries_to_repair: Vec<String> = fs::read_dir(&worktrees_dir)?
        .filter_map(|e| e.ok())
        .filter(|e| e.file_type().map(|t| t.is_dir()).unwrap_or(false))
        .map(|e| e.file_name().to_string_lossy().to_string())
        .collect();

    for name in &entries_to_repair {
        let admin = worktrees_dir.join(name);
        let gitdir_file = admin.join("gitdir");

        if !gitdir_file.exists() {
            continue;
        }

        let raw = fs::read_to_string(&gitdir_file).unwrap_or_default();
        let recorded = resolve_gitdir_file_target(&gitdir_file, raw.trim());
        let wt_dotgit = path_for_git_storage(&recorded);
        let wt_path = wt_dotgit
            .parent()
            .map(path_for_git_storage)
            .unwrap_or_else(|| path_for_git_storage(&recorded));
        let use_relative = repair_use_relative_paths(&args, &common);

        // Repair 1: If the worktree .git file exists and points to the correct admin dir, it's fine.
        // If it exists but points to an EXISTING but different admin dir, repair the pointer.
        // If it exists but points to a NON-EXISTENT location, fall through to Repair 2.
        if !wt_path.exists() {
            continue;
        }
        if !wt_path.is_dir() {
            eprintln!("error: {}: not a directory", wt_path.display());
            std::process::exit(1);
        }
        let dotgit_path = wt_path.join(".git");
        if dotgit_path.is_dir() {
            eprintln!("error: {}: .git is not a file", wt_path.display());
            std::process::exit(1);
        }

        let admin_stored = path_for_git_storage(&admin);
        let mut repair_msg: Option<&str> = None;

        if dotgit_path.is_file() {
            if let Ok(content) = fs::read_to_string(&dotgit_path) {
                if let Some(backlink) = resolve_gitfile_backlink(&dotgit_path, &content) {
                    if !backlink.exists() {
                        repair_msg = Some(".git file broken");
                    } else if path_for_git_storage(&backlink) != admin_stored {
                        repair_msg = Some(".git file incorrect");
                    }
                } else {
                    repair_msg = Some(".git file broken");
                }
            }
        } else {
            repair_msg = Some(".git file broken");
        }

        let dot_git_expected = path_for_git_storage(&wt_path.join(".git"));
        if path_for_git_storage(&recorded) != dot_git_expected {
            repair_msg = Some("gitdir incorrect");
        }

        if let Some(msg) = repair_msg {
            write_worktree_linking_files(&wt_path, &admin, use_relative)?;
            eprintln!("repair: {}: {msg}", wt_path.display());
        }
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// worktree lock / unlock
// ---------------------------------------------------------------------------

fn cmd_lock(args: LockArgs) -> Result<()> {
    let repo = Repository::discover(None)?;
    let common = common_dir(&repo.git_dir)?;
    let worktrees_dir = common.join("worktrees");

    let wt_path = if args.path.is_absolute() {
        args.path.clone()
    } else {
        std::env::current_dir()?.join(&args.path)
    };
    let wt_path = wt_path.canonicalize().unwrap_or(wt_path);

    let wt_name = find_worktree_name(&worktrees_dir, &wt_path)?;
    let admin = worktrees_dir.join(&wt_name);

    if admin.join("locked").exists() {
        bail!("worktree '{}' is already locked", wt_path.display());
    }

    let reason = args.reason.as_deref().unwrap_or("");
    // Write reason with trailing newline (to match `echo reason > locked`)
    let content = if reason.is_empty() {
        String::new()
    } else {
        format!("{reason}\n")
    };
    fs::write(admin.join("locked"), content)?;

    Ok(())
}

fn cmd_unlock(args: UnlockArgs) -> Result<()> {
    let repo = Repository::discover(None)?;
    let common = common_dir(&repo.git_dir)?;
    let worktrees_dir = common.join("worktrees");

    let wt_path = if args.path.is_absolute() {
        args.path.clone()
    } else {
        std::env::current_dir()?.join(&args.path)
    };
    let wt_path = wt_path.canonicalize().unwrap_or(wt_path);

    let wt_name = find_worktree_name(&worktrees_dir, &wt_path)?;
    let admin = worktrees_dir.join(&wt_name);

    let lock_file = admin.join("locked");
    if !lock_file.exists() {
        bail!("worktree '{}' is not locked", wt_path.display());
    }

    fs::remove_file(&lock_file)?;

    Ok(())
}

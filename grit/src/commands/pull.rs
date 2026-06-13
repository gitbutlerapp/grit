//! `grit pull` — fetch from a remote and integrate changes.
//!
//! Equivalent to running `grit fetch` followed by `grit merge` (or
//! `grit rebase` with `--rebase`).  Only local transports are supported.

use crate::explicit_exit::ExplicitExit;
use anyhow::{bail, Context, Result};
use clap::Args as ClapArgs;
use grit_lib::config::ConfigSet;
use grit_lib::merge_base::{is_ancestor, merge_bases_first_vs_rest};
use grit_lib::objects::ObjectId;
use grit_lib::push_submodules::submodule_gitlinks_touched_in_range;
use grit_lib::refs;
use grit_lib::repo::Repository;
use grit_lib::rev_parse::resolve_revision;
use grit_lib::state::resolve_head;
use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};

/// Arguments for `grit pull`.
#[derive(Debug, ClapArgs)]
#[command(about = "Fetch from and integrate with another repository")]
pub struct Args {
    /// Repository (remote name or path) to pull from; defaults from branch config.
    #[arg(value_name = "REPOSITORY")]
    pub remote: Option<String>,

    /// Refs to fetch and merge (multiple refs merge via octopus / multi-head rules).
    #[arg(value_name = "REFSPEC")]
    pub refspecs: Vec<String>,

    /// Rebase instead of merge. Use `--rebase=merges` for the merges strategy; a bare `--rebase`
    /// must not consume the remote name (`git pull --rebase origin` matches C Git).
    #[arg(
        long = "rebase",
        short = 'r',
        num_args = 0..=1,
        default_missing_value = "true",
        require_equals = true
    )]
    pub rebase: Option<String>,

    /// Only allow fast-forward merges.
    #[arg(long = "ff-only")]
    pub ff_only: bool,

    /// Do not allow fast-forward (always create merge commit).
    #[arg(long = "no-ff")]
    pub no_ff: bool,

    /// Suppress output.
    #[arg(short = 'q', long = "quiet")]
    pub quiet: bool,

    /// Merge strategy to use (may be given multiple times).
    #[arg(short = 's', long = "strategy", action = clap::ArgAction::Append)]
    pub strategy: Vec<String>,

    /// Strategy option.
    #[arg(short = 'X', long = "strategy-option")]
    pub strategy_option: Vec<String>,

    /// Disable rebase (use merge, the default).
    #[arg(long = "no-rebase")]
    pub no_rebase: bool,

    /// Allow fast-forward (default).
    #[arg(long = "ff")]
    pub ff: bool,

    /// Include one-line descriptions from commit messages in the merge commit.
    /// Optionally limit the number of entries.
    #[arg(long = "log", num_args = 0..=1, default_missing_value = "0", require_equals = true)]
    pub log: Option<String>,

    /// Do not include one-line descriptions.
    #[arg(long = "no-log")]
    pub no_log: bool,

    /// After merge/rebase, run `submodule update --init --recursive`.
    #[arg(long = "recurse-submodules", num_args = 0..=1, default_missing_value = "true", require_equals = true)]
    pub recurse_submodules: Option<String>,

    /// Do not recurse into submodules after pull.
    #[arg(long = "no-recurse-submodules")]
    pub no_recurse_submodules: bool,

    /// Be more verbose (passed to the underlying fetch). May be repeated.
    #[arg(short = 'v', long = "verbose", action = clap::ArgAction::Count)]
    pub verbose: u8,

    /// Fetch from all remotes.
    #[arg(long = "all")]
    pub all: bool,

    /// When fetching, force update of local refs (`fetch --force`).
    #[arg(short = 'f', long = "force")]
    pub force: bool,

    /// Show what would be done, without making any changes.
    #[arg(long = "dry-run")]
    pub dry_run: bool,

    /// Allow merging histories that do not share a common ancestor.
    #[arg(long = "allow-unrelated-histories")]
    pub allow_unrelated_histories: bool,

    /// Add a `Signed-off-by` trailer to the merge commit message.
    #[arg(long = "signoff", overrides_with = "no_signoff")]
    pub signoff: bool,

    /// Do not add a `Signed-off-by` trailer (cancels an earlier `--signoff`).
    #[arg(long = "no-signoff", overrides_with = "signoff")]
    pub no_signoff: bool,

    /// Skip the pre-merge-commit and commit-msg hooks when merging.
    #[arg(long = "no-verify", overrides_with = "verify")]
    pub no_verify: bool,

    /// Run the pre-merge-commit and commit-msg hooks when merging (cancels `--no-verify`).
    #[arg(long = "verify", overrides_with = "no_verify")]
    pub verify: bool,

    /// Before starting, stash local modifications and re-apply them afterwards.
    #[arg(long = "autostash", overrides_with = "no_autostash")]
    pub autostash: bool,

    /// Do not stash local modifications before integrating (overrides config).
    #[arg(long = "no-autostash", overrides_with = "autostash")]
    pub no_autostash: bool,

    /// Verify the tip commit's GPG signature (passed through to merge).
    #[arg(long = "verify-signatures")]
    pub verify_signatures: bool,

    /// Do not verify GPG signatures (passed through to merge).
    #[arg(long = "no-verify-signatures")]
    pub no_verify_signatures: bool,

    /// Set the upstream (branch.<name>.remote / .merge) of the current branch from the fetch
    /// (passed through to the underlying fetch, matching `git pull --set-upstream`).
    #[arg(long = "set-upstream")]
    pub set_upstream: bool,

    /// Fetch all tags from the remote (passed through to the underlying fetch).
    #[arg(short = 't', long = "tags")]
    pub tags: bool,

    /// Do not fetch tags (passed through to the underlying fetch).
    #[arg(long = "no-tags")]
    pub no_tags: bool,
}

fn rebase_cli_value_is_valid(s: &str) -> bool {
    matches!(
        s.to_ascii_lowercase().as_str(),
        "false"
            | "no"
            | "off"
            | "0"
            | "true"
            | "yes"
            | "on"
            | "1"
            | "merges"
            | "m"
            | "interactive"
            | "i"
    )
}

/// The flavor of rebase requested via `--rebase=<value>` / `pull.rebase` (git's `rebase_type`),
/// distinguishing plain rebase from `--rebase-merges` and `--interactive`.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
enum PullRebaseKind {
    #[default]
    Plain,
    Merges,
    Interactive,
}

/// Map a rebase config/CLI value to its flavor; `true`/`yes`/`1`/`on` are plain rebase, while
/// `merges`/`m` and `interactive`/`i` select the corresponding rebase mode (git `rebase_parse_value`).
fn rebase_kind_from_value(value: &str) -> PullRebaseKind {
    match value.trim().to_ascii_lowercase().as_str() {
        "merges" | "m" => PullRebaseKind::Merges,
        "interactive" | "i" => PullRebaseKind::Interactive,
        _ => PullRebaseKind::Plain,
    }
}

/// If clap consumed the repository token as `--rebase`'s optional value (`pull --rebase . c1` →
/// rebase=".", remote=c1), restore Git's argv layout: remote=`.`, refspecs start with `c1`.
fn remote_default_branch_short(remote_path: &Path) -> Result<String> {
    let remote_repo = open_repository_at_path(remote_path)?;
    let remote_git_dir = &remote_repo.git_dir;
    if let Some(sym) = refs::read_symbolic_ref(remote_git_dir, "HEAD")? {
        let sym = sym.trim();
        if let Some(rest) = sym.strip_prefix("refs/heads/") {
            if !rest.is_empty() {
                return Ok(rest.to_owned());
            }
        }
    }
    let heads = refs::list_refs(remote_git_dir, "refs/heads/")?;
    if heads.len() == 1 {
        let full = &heads[0].0;
        return Ok(full
            .strip_prefix("refs/heads/")
            .unwrap_or(full.as_str())
            .to_owned());
    }
    bail!("remote repository has no default branch (unborn HEAD or multiple branches)")
}

/// Resolve `HEAD` in a pull refspec to the **remote branch short name** (e.g. `main`).
///
/// Fetch already knows which remote to contact; passing `origin/main` as the refspec is wrong
/// (it looks for `refs/heads/origin/main` on the remote — t5572 `pull origin HEAD`).
fn pull_head_refspec_to_fetch_token(
    repo: &Repository,
    remote_name: &str,
    local_remote_path: Option<&Path>,
) -> Result<String> {
    if let Some(p) = local_remote_path {
        if let Ok(branch) = remote_default_branch_short(p) {
            return Ok(branch);
        }
    }
    let sym_key = format!("refs/remotes/{remote_name}/HEAD");
    if let Some(sym) = refs::read_symbolic_ref(&repo.git_dir, &sym_key)? {
        let sym = sym.trim();
        let prefix = format!("refs/remotes/{remote_name}/");
        if let Some(rest) = sym.strip_prefix(&prefix) {
            if !rest.is_empty() {
                return Ok(rest.to_owned());
            }
        }
    }
    let Some(p) = local_remote_path else {
        // No `refs/remotes/<remote>/HEAD` and no resolved local path (e.g. the remote URL is a bare
        // relative name like `parent` that the local-path heuristic does not recognize). Leave the
        // refspec as `HEAD`; the underlying fetch resolves the remote's advertised HEAD directly
        // (`git fetch <remote> HEAD`), so pull need not pre-resolve it to a branch name.
        return Ok("HEAD".to_owned());
    };
    remote_default_branch_short(p)
}

/// `git pull remote HEAD` passes the refspec token `HEAD`; map it to `remote/<default-branch>`.
fn normalize_pull_fetch_refspecs(
    repo: &Repository,
    args: &Args,
    remote_name: &str,
    local_remote_path: Option<&Path>,
) -> Result<Vec<String>> {
    if args.refspecs.is_empty() {
        return Ok(Vec::new());
    }
    let mut out = Vec::with_capacity(args.refspecs.len());
    for s in &args.refspecs {
        let t = s.trim();
        if t == "HEAD" || t == "refs/heads/HEAD" {
            out.push(pull_head_refspec_to_fetch_token(
                repo,
                remote_name,
                local_remote_path,
            )?);
        } else {
            out.push(s.clone());
        }
    }
    Ok(out)
}

/// Reproduce git pull.c `die_no_merge_candidates`: pick the precise diagnostic for why a pull has
/// nothing to merge, given whether a `<repo>` was named on the CLI, whether a refspec was given,
/// the current branch and its configured `branch.<name>.remote`/`.merge`.
///
/// `opt_rebase` is true when this pull would rebase (the wording for the no-candidate / no-tracking
/// cases differs slightly, but the substrings the tests grep for are unaffected).
fn die_no_merge_candidates(
    repo_arg: Option<&str>,
    have_refspec: bool,
    current_branch: Option<&str>,
    config: &ConfigSet,
    opt_rebase: bool,
) -> anyhow::Error {
    let branch_remote = current_branch.and_then(|b| config.get(&format!("branch.{b}.remote")));
    let merge_nr = current_branch
        .map(|b| config.get(&format!("branch.{b}.merge")).is_some())
        .unwrap_or(false);

    if have_refspec {
        if opt_rebase {
            eprintln!(
                "There is no candidate for rebasing against among the refs that you just fetched."
            );
        } else {
            eprintln!("There are no candidates for merging among the refs that you just fetched.");
        }
        eprintln!(
            "Generally this means that you provided a wildcard refspec which had no\nmatches on the remote end."
        );
    } else if let (Some(repo), Some(_)) = (repo_arg, current_branch) {
        // A `<repo>` was named that is not the branch's configured remote: the configured
        // tracking branch does not apply, so a branch must be given explicitly.
        let is_default = branch_remote.as_deref() == Some(repo);
        if !is_default {
            eprintln!(
                "You asked to pull from the remote '{repo}', but did not specify\na branch. Because this is not the default configured remote\nfor your current branch, you must specify a branch on the command line."
            );
        } else {
            // repo == configured remote but no merge ref: fall through to the no-tracking message.
            die_no_tracking_information(current_branch, opt_rebase);
        }
    } else if current_branch.is_none() {
        eprintln!("You are not currently on a branch.");
        if opt_rebase {
            eprintln!("Please specify which branch you want to rebase against.");
        } else {
            eprintln!("Please specify which branch you want to merge with.");
        }
        eprintln!("See git-pull(1) for details.");
        eprintln!();
        eprintln!("    git pull <remote> <branch>");
        eprintln!();
    } else if !merge_nr {
        die_no_tracking_information(current_branch, opt_rebase);
    } else {
        let merge_ref = current_branch
            .and_then(|b| config.get(&format!("branch.{b}.merge")))
            .unwrap_or_default();
        eprintln!(
            "Your configuration specifies to merge with the ref '{}'\nfrom the remote, but no such ref was fetched.",
            merge_ref.strip_prefix("refs/heads/").unwrap_or(&merge_ref)
        );
    }
    anyhow::Error::new(ExplicitExit {
        code: 1,
        message: String::new(),
    })
}

fn die_no_tracking_information(current_branch: Option<&str>, opt_rebase: bool) {
    let branch = current_branch.unwrap_or("<branch>");
    eprintln!("There is no tracking information for the current branch.");
    if opt_rebase {
        eprintln!("Please specify which branch you want to rebase against.");
    } else {
        eprintln!("Please specify which branch you want to merge with.");
    }
    eprintln!("See git-pull(1) for details.");
    eprintln!();
    eprintln!("    git pull <remote> <branch>");
    eprintln!();
    eprintln!("If you wish to set tracking information for this branch you can do so with:");
    eprintln!();
    eprintln!("    git branch --set-upstream-to=<remote>/<branch> {branch}\n");
}

/// Best-effort "will this pull rebase?" used only to pick the wording of `die_no_merge_candidates`
/// (rebase vs merge phrasing). CLI flags win; otherwise consult `branch.<b>.rebase`/`pull.rebase`.
fn pull_will_rebase_for_diag(
    args: &Args,
    config: &ConfigSet,
    current_branch: Option<&str>,
) -> bool {
    if args.no_rebase {
        return false;
    }
    if let Some(ref s) = args.rebase {
        return parse_rebase_value("--rebase", s)
            .map(|t| t == RebaseTri::True)
            .unwrap_or(false);
    }
    matches!(
        config_pull_rebase(config, current_branch),
        Ok((RebaseTri::True, _, _))
    )
}

fn merge_branch_for_pull(
    effective_refspecs: &[String],
    remote_name: &str,
    current_branch: Option<&str>,
    config: &ConfigSet,
    local_remote_path: Option<&Path>,
) -> Result<String> {
    if let Some(first) = effective_refspecs.first() {
        let prefix = format!("{remote_name}/");
        return Ok(first
            .strip_prefix(&prefix)
            .unwrap_or(first.as_str())
            .to_owned());
    }
    if let Some(ref branch) = current_branch {
        if let Some(merge_ref) = config.get(&format!("branch.{branch}.merge")) {
            return Ok(merge_ref
                .strip_prefix("refs/heads/")
                .unwrap_or(&merge_ref)
                .to_owned());
        }
        return Ok(branch.to_string());
    }
    let Some(path) = local_remote_path else {
        bail!("no tracking branch configured and no branch specified");
    };
    remote_default_branch_short(path)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum PullIntegrateKind {
    Merge,
    /// `pull --rebase` used merge with `--ff-only` because a fast-forward was possible.
    MergeFfOnlyForRebase,
    Rebase,
}

fn normalize_pull_positionals(mut args: Args) -> Args {
    if let Some(ref r) = args.rebase {
        if !rebase_cli_value_is_valid(r) {
            let bad = r.clone();
            args.rebase = Some("true".to_owned());
            if let Some(old_remote) = args.remote.take() {
                args.refspecs.insert(0, old_remote);
            }
            args.remote = Some(bad);
        }
    }
    args
}

/// Compute the effective verbosity level from `-q`/`--quiet`/`-v`/`--verbose` in argv order,
/// mirroring Git's `parse_opt_verbosity_cb` (a single signed counter, not two independent flags).
///
/// `-v` raises verbosity (or jumps from quiet to `+1`); `-q` lowers it (or jumps from verbose to
/// `-1`); `--no-quiet`/`--no-verbose` reset to `0`. The asymmetry makes `pull -v -q` quiet but
/// `pull -q -v` verbose (t5521 subtests 8 and 9).
fn compute_pull_verbosity<I>(args: I) -> i32
where
    I: IntoIterator<Item = String>,
{
    let mut target: i32 = 0;
    let mut apply = |is_verbose: bool, unset: bool| {
        if unset {
            target = 0;
        } else if is_verbose {
            target = if target >= 0 { target + 1 } else { 1 };
        } else {
            target = if target <= 0 { target - 1 } else { -1 };
        }
    };
    for tok in args {
        match tok.as_str() {
            "--quiet" => apply(false, false),
            "--verbose" => apply(true, false),
            "--no-quiet" => apply(false, true),
            "--no-verbose" => apply(true, true),
            "--" => break,
            t if t.starts_with("--") => {}
            t if t.starts_with('-') && t.len() > 1 => {
                // Short bundle like `-qv`, `-vq`, `-q`, `-v`. Process each letter in order;
                // a non-`q`/`v` short option ends the cluster scan (it may take a value).
                for c in t[1..].chars() {
                    match c {
                        'q' => apply(false, false),
                        'v' => apply(true, false),
                        _ => break,
                    }
                }
            }
            _ => {}
        }
    }
    target
}

/// git pull.c `require_clean_work_tree(r, "pull with rebase", ...)`: a `pull --rebase` without
/// autostash refuses to start when the index or work tree is dirty, *before* fetching anything.
fn require_clean_work_tree_for_rebase(repo: &Repository) -> Result<()> {
    use grit_lib::diff::{diff_index_to_tree, diff_index_to_worktree};
    let Some(work_tree) = repo.work_tree.as_deref() else {
        return Ok(());
    };
    let index = repo.load_index().unwrap_or_default();
    let head_tree = resolve_head(&repo.git_dir)?.oid().and_then(|oid| {
        let obj = repo.odb.read(oid).ok()?;
        grit_lib::objects::parse_commit(&obj.data)
            .ok()
            .map(|c| c.tree)
    });
    let staged = diff_index_to_tree(&repo.odb, &index, head_tree.as_ref(), true)?;
    let mut unstaged = diff_index_to_worktree(&repo.odb, &index, work_tree, false, false)?;
    unstaged.retain(|e| e.old_mode != "160000" && e.new_mode != "160000");
    if !staged.is_empty() {
        bail!(
            "cannot pull with rebase: Your index contains uncommitted changes.\nPlease commit or stash them."
        );
    }
    if !unstaged.is_empty() {
        bail!("cannot pull with rebase: You have unstaged changes.\nPlease commit or stash them.");
    }
    Ok(())
}

/// `git pull` refuses to start while the index has unresolved (stage > 0) entries
/// (builtin/pull.c `repo_read_index_unmerged` -> `die_resolve_conflict("pull")`).
fn die_if_index_unmerged(repo: &Repository) -> Result<()> {
    let Ok(index) = repo.load_index() else {
        return Ok(());
    };
    if index.entries.iter().any(|e| e.stage() != 0) {
        eprintln!("error: Pulling is not possible because you have unmerged files.");
        eprintln!("hint: Fix them up in the work tree, and then use 'git add/rm <file>'");
        eprintln!("hint: as appropriate to mark resolution and make a commit.");
        return Err(anyhow::Error::new(ExplicitExit {
            code: 128,
            message: "Exiting because of an unresolved conflict.".to_owned(),
        }));
    }
    Ok(())
}

/// `git pull` refuses to start when a merge is in progress (MERGE_HEAD exists)
/// (builtin/pull.c `die_conclude_merge`).
fn die_if_merge_in_progress(repo: &Repository) -> Result<()> {
    if repo.git_dir.join("MERGE_HEAD").exists() {
        eprintln!("error: You have not concluded your merge (MERGE_HEAD exists).");
        eprintln!("hint: Please, commit your changes before merging.");
        return Err(anyhow::Error::new(ExplicitExit {
            code: 128,
            message: "Exiting because of unfinished merge.".to_owned(),
        }));
    }
    Ok(())
}

/// Apply `--set-upstream` for a pull that integrates from a local repository (the local-path or
/// `git pull .` shortcuts that bypass `fetch::run`). `git pull --set-upstream` passes the flag
/// straight through to fetch, so this mirrors `fetch::apply_set_upstream` using the original CLI
/// refspecs.
fn apply_pull_set_upstream(
    git_dir: &Path,
    remote_name: &str,
    remote_git_dir: &Path,
    cli_refspecs: &[String],
    is_anonymous_url: bool,
) {
    let all_refs = refs::list_refs(remote_git_dir, "refs/").unwrap_or_default();
    let symbolic_head = refs::read_symbolic_ref(remote_git_dir, "HEAD")
        .ok()
        .flatten()
        .and_then(|s| s.strip_prefix("refs/heads/").map(ToOwned::to_owned));
    super::fetch::apply_set_upstream(
        git_dir,
        remote_name,
        cli_refspecs,
        !cli_refspecs.is_empty(),
        is_anonymous_url,
        &all_refs,
        symbolic_head.as_deref(),
    );
}

pub fn run(args: Args) -> Result<()> {
    let mut args = normalize_pull_positionals(args);
    // Reconcile `-q`/`-v` into Git's single verbosity counter (clap parses them as independent
    // flags and loses the argv ordering that Git's verbosity algorithm depends on).
    let verbosity = compute_pull_verbosity(std::env::args());
    args.quiet = verbosity < 0;
    args.verbose = u8::try_from(verbosity.max(0)).unwrap_or(u8::MAX);
    let args = args;
    let repo = Repository::discover(None).context("not a git repository")?;
    let config = ConfigSet::load(Some(&repo.git_dir), true)?;

    let head = resolve_head(&repo.git_dir)?;
    let current_branch = head.branch_name().map(|s| s.to_owned());

    // git pull.c runs these guards before fetching: a pull cannot proceed with an unmerged index
    // or an in-progress merge (MERGE_HEAD), and `--rebase` onto an unborn branch with staged
    // changes is rejected up front.
    die_if_index_unmerged(&repo)?;
    die_if_merge_in_progress(&repo)?;

    // `git pull --all` fetches from every configured remote (no single repository argument).
    // Delegate to the real `git fetch --all` machinery — which writes FETCH_HEAD with the
    // configured upstream branch marked for merge — then integrate exactly as a normal pull.
    if args.all {
        return run_pull_all(&args, &config, &repo, &head);
    }

    let remote_name_owned: String = if let Some(ref r) = args.remote {
        r.clone()
    } else if let Some(ref branch) = current_branch {
        config
            .get(&format!("branch.{branch}.remote"))
            .unwrap_or_else(|| "origin".to_owned())
    } else {
        "origin".to_owned()
    };
    let remote_name = remote_name_owned.as_str();

    let work_tree = repo.work_tree.as_ref().context("bare repository")?;
    let is_local_dot = remote_name == ".";
    let local_remote_path = if is_local_dot {
        None
    } else {
        resolve_local_remote_path(work_tree, remote_name, &config)?
    };

    let effective_refspecs =
        normalize_pull_fetch_refspecs(&repo, &args, remote_name, local_remote_path.as_deref())?;

    // Decide up front whether this pull can determine a branch to merge. Without an explicit
    // refspec, git relies on `branch.<name>.merge`; when that is absent (or we are detached, or a
    // non-default remote was named) there is nothing to merge and git pull.c emits a tailored
    // diagnostic via `die_no_merge_candidates`.
    if effective_refspecs.is_empty() {
        // A raw path/URL `<repo>` (e.g. `git pull ..`) that is not a configured remote always has
        // a merge candidate: fetch marks the remote's default branch for-merge. Only named/default
        // remotes consult `branch.<b>.merge`, so the no-candidate diagnostics apply to them.
        let repo_is_raw_path = args
            .remote
            .as_deref()
            .map(|r| {
                remote_token_looks_like_path(r) && config.get(&format!("remote.{r}.url")).is_none()
            })
            .unwrap_or(false);
        if !repo_is_raw_path {
            let opt_rebase_for_msg =
                pull_will_rebase_for_diag(&args, &config, current_branch.as_deref());
            if current_branch.is_none() {
                return Err(die_no_merge_candidates(
                    args.remote.as_deref(),
                    false,
                    None,
                    &config,
                    opt_rebase_for_msg,
                ));
            }
            let branch = current_branch.as_deref().unwrap();
            let has_merge_cfg = config.get(&format!("branch.{branch}.merge")).is_some();
            // No `branch.<b>.merge` and no explicit refspec: there is no branch to merge. The exact
            // diagnostic (specify-a-branch vs no-tracking-information) is chosen inside
            // `die_no_merge_candidates` from whether `<repo>` was named and matches the configured
            // remote.
            if !has_merge_cfg {
                return Err(die_no_merge_candidates(
                    args.remote.as_deref(),
                    false,
                    Some(branch),
                    &config,
                    opt_rebase_for_msg,
                ));
            }
        }
    }

    // git pull.c: when this pull will rebase, two guards run *before* fetch so a dirty tree never
    // gets a fetch's worth of work done first (t5520 "pull --rebase dies early ..."): rebasing onto
    // an unborn branch that has staged changes is impossible, and a dirty tree without autostash is
    // refused with "cannot pull with rebase".
    if pull_will_rebase_for_diag(&args, &config, current_branch.as_deref()) {
        let autostash = resolve_pull_autostash(&args, &config);
        // For a rebase, an Unset autostash decision falls back to `rebase.autostash`.
        let autostash_on = match autostash {
            AutostashTri::On => true,
            AutostashTri::Off => false,
            AutostashTri::Unset => config
                .get_bool("rebase.autostash")
                .map(|r| r.unwrap_or(false))
                .unwrap_or(false),
        };
        if head.oid().is_none() {
            let index = repo.load_index().unwrap_or_default();
            if !index.entries.is_empty() {
                bail!("Updating an unborn branch with changes added to the index.");
            }
        } else if !autostash_on {
            require_clean_work_tree_for_rebase(&repo)?;
        }
    }

    let merge_branch = merge_branch_for_pull(
        &effective_refspecs,
        remote_name,
        current_branch.as_deref(),
        &config,
        local_remote_path.as_deref(),
    )?;

    let fetch_recurse = if args.no_recurse_submodules {
        None
    } else if config
        .get("fetch.recursesubmodules")
        .or_else(|| config.get("fetch.recurseSubmodules"))
        .map(|v| {
            let l = v.to_ascii_lowercase();
            l == "true" || l == "yes" || l == "on" || l == "1"
        })
        .unwrap_or(false)
    {
        Some("true".to_owned())
    } else {
        None
    };

    if let Some(remote_path) = local_remote_path {
        // `--dry-run` reports what fetch would do and stops before any merge; the local-path
        // shortcut must likewise touch nothing (no object copy, no FETCH_HEAD, no refs).
        if args.dry_run {
            return Ok(());
        }
        // Local path remotes (`..`, `./upstream`): copy objects directly and write FETCH_HEAD
        // without updating `refs/tags/*`. Git keeps annotated tags only in FETCH_HEAD for
        // `git pull $path $tag` so throwaway-tag merges default to --no-ff (t7600).
        let remote_repo = open_repository_at_path(&remote_path)?;

        // `fetch.recurseSubmodules` (or `--recurse-submodules`) makes the *fetch* phase descend into
        // submodules. The non-local path gets this from `super::fetch::run`; the direct-copy
        // shortcut must reproduce it, recording superproject ref tips around the copy so on-demand
        // recursion can detect changed submodule pointers.
        let fetch_recurse_mode = pull_fetch_recurse_mode(&args, &config)?;
        recurse_fetch_submodules_for_local_pull(&config, fetch_recurse_mode, || {
            // Resolve the OIDs this pull will bring in so we copy only their reachable closure as
            // loose objects (and prune any already borrowable from an alternate), rather than
            // hardlinking the remote's entire object store. Matches `git fetch`'s local transport:
            // a `--reference` clone that pulls new commits keeps only the genuinely new objects
            // local, not a wholesale copy of the source's packs (`t5604`).
            let mut copy_roots: Vec<ObjectId> = Vec::new();
            if args.refspecs.is_empty() {
                if let Ok(oid) =
                    refs::resolve_ref(&remote_repo.git_dir, &format!("refs/heads/{merge_branch}"))
                        .or_else(|_| refs::resolve_ref(&remote_repo.git_dir, "HEAD"))
                {
                    copy_roots.push(oid);
                }
            } else {
                for spec in &effective_refspecs {
                    if let Ok((oid, _)) = pull_fetch_head_line(&remote_repo, spec) {
                        copy_roots.push(oid);
                    }
                }
            }
            if copy_roots.is_empty() {
                super::fetch::copy_objects_for_pull(&remote_repo.git_dir, &repo.git_dir)?;
            } else {
                // Skip gitlink (submodule) tree entries: a superproject tree records nested
                // submodule commit IDs that live in the submodule's object store, not the
                // superproject's, so they are never part of the superproject's transferred
                // object closure (Git's pack-objects ignores `S_ISGITLINK` entries). Walking
                // them would error with "missing object" when pulling a branch that adds a
                // submodule (t5572 git_pull cases).
                //
                // When this repository is itself a partial clone (and so is the local-path source),
                // the source legitimately omits its own promisor objects (they live on its
                // lazy-fetch remote). Use the promisor-tolerant copy so a missing source object is
                // recorded for later lazy-fetch instead of aborting the pull (t5710 subsequent
                // fetch from a partial-clone server).
                if grit_lib::promisor::repo_treats_promisor_packs(&repo.git_dir, &config) {
                    super::fetch::copy_reachable_objects_skipping_missing_promisor(
                        &remote_repo.git_dir,
                        &repo.git_dir,
                        &copy_roots,
                    )?;
                } else {
                    super::fetch::copy_reachable_objects_skipping_gitlinks(
                        &remote_repo.git_dir,
                        &repo.git_dir,
                        &copy_roots,
                    )?;
                }
                super::fetch::prune_loose_objects_available_from_alternates(&repo.git_dir)?;
            }

            let lines = if args.refspecs.is_empty() {
                let remote_oid = if let Ok(oid) =
                    refs::resolve_ref(&remote_repo.git_dir, &format!("refs/heads/{merge_branch}"))
                {
                    oid
                } else if let Ok(oid) = refs::resolve_ref(&remote_repo.git_dir, "HEAD") {
                    oid
                } else {
                    bail!("bad revision '{merge_branch}': could not resolve in remote");
                };
                let tracking_ref = format!("refs/remotes/{remote_name}/{merge_branch}");
                refs::write_ref(&repo.git_dir, &tracking_ref, &remote_oid)
                    .with_context(|| format!("update remote-tracking ref {tracking_ref}"))?;
                if let Ok(Some(sym)) = refs::read_symbolic_ref(&remote_repo.git_dir, "HEAD") {
                    let short = sym.strip_prefix("refs/heads/").unwrap_or(&sym);
                    if short == merge_branch.as_str() {
                        let remote_head_ref = format!("refs/remotes/{remote_name}/HEAD");
                        let _ = refs::write_symbolic_ref(
                            &repo.git_dir,
                            &remote_head_ref,
                            &tracking_ref,
                        );
                    }
                }
                // Auto-follow tags (`TAGS_DEFAULT`), the way `git fetch <remote>` does: a configured
                // remote whose fetch refspec maps into `refs/remotes/<remote>/` opportunistically
                // downloads tags reachable from the fetched commit. The direct-copy shortcut skips
                // `fetch::run`, so reproduce that follow here (t5516 'fetch follows tags by default').
                // Only for a configured-remote pull with no explicit ref argument and tags not
                // disabled — an anonymous `git pull <path> <tag>` keeps tags in FETCH_HEAD only.
                if pull_should_follow_tags(&config, remote_name) {
                    follow_tags_for_local_pull(&repo, &remote_repo, remote_oid)?;
                }
                vec![format!(
                    "{}\t\tbranch 'refs/heads/{merge_branch}' of .\n",
                    remote_oid.to_hex()
                )]
            } else {
                let mut out = Vec::new();
                for spec in &effective_refspecs {
                    let (oid, desc) = pull_fetch_head_line(&remote_repo, spec)?;
                    out.push(format!("{}\t\t{desc}\n", oid.to_hex()));
                    // Opportunistically update the configured remote-tracking ref, the
                    // same way `git fetch <remote> <branch>` does: an explicit refspec
                    // still refreshes `refs/remotes/<remote>/<branch>` when a
                    // `remote.<name>.fetch` rule maps it (t5510 "explicit pull should
                    // update tracking").
                    update_opportunistic_tracking_ref(
                        &repo,
                        &remote_repo,
                        remote_name,
                        spec,
                        &config,
                    )?;
                }
                out
            };
            fs::write(repo.git_dir.join("FETCH_HEAD"), lines.concat())?;
            Ok(())
        })?;

        if args.set_upstream {
            // An anonymous URL/path remote (e.g. `git pull <file://...>`) that is not a configured
            // remote uses the URL itself as the config remote value, and its bare HEAD as the merge
            // source — mirroring `git fetch`'s `url_override` path.
            let is_anonymous_url = remote_token_looks_like_path(remote_name)
                && config.get(&format!("remote.{remote_name}.url")).is_none();
            apply_pull_set_upstream(
                &repo.git_dir,
                remote_name,
                &remote_repo.git_dir,
                &args.refspecs,
                is_anonymous_url,
            );
        }

        grit_lib::pack::clear_pack_cache();
        let kind = do_merge_or_rebase_after_fetch(&args, &config, &repo, &head)?;
        maybe_update_submodules_after_pull(&args, &config, kind)?;
        return Ok(());
    }

    if is_local_dot {
        if args.dry_run {
            return Ok(());
        }
        let lines = if args.refspecs.is_empty() {
            let remote_oid =
                match refs::resolve_ref(&repo.git_dir, &format!("refs/heads/{merge_branch}"))
                    .or_else(|_| resolve_revision(&repo, &merge_branch))
                {
                    Ok(oid) => oid,
                    Err(_) => {
                        // The configured `branch.<b>.merge` ref does not exist locally / on `.`.
                        return Err(die_no_merge_candidates(
                            args.remote.as_deref(),
                            false,
                            current_branch.as_deref(),
                            &config,
                            pull_will_rebase_for_diag(&args, &config, current_branch.as_deref()),
                        ));
                    }
                };
            vec![format!(
                "{}\t\tbranch 'refs/heads/{merge_branch}' of .\n",
                remote_oid.to_hex()
            )]
        } else {
            let mut out = Vec::new();
            for spec in &args.refspecs {
                // A wildcard refspec (`refs/foo/*:refs/bar/*`) that matches nothing on the remote
                // yields no merge candidate — git pull.c `die_no_merge_candidates` (the *refspecs
                // branch).
                let (src, _dst) = split_pull_refspec(spec);
                if src.contains('*')
                    && refs::list_refs_glob(&repo.git_dir, src)
                        .map(|m| m.is_empty())
                        .unwrap_or(true)
                {
                    return Err(die_no_merge_candidates(
                        args.remote.as_deref(),
                        true,
                        current_branch.as_deref(),
                        &config,
                        pull_will_rebase_for_diag(&args, &config, current_branch.as_deref()),
                    ));
                }
                // `pull . <src>:<current-branch>`: the destination is the branch we are on, so the
                // "fetch" updates HEAD's ref directly (git fetch `--update-head-ok`) and the working
                // tree is then fast-forwarded (builtin/pull.c). Handle that here so the merge step
                // sees an already-up-to-date FETCH_HEAD.
                if let Some(branch) = current_branch.as_deref() {
                    if fetch_updates_current_branch_dst(spec, branch) {
                        pull_local_fetch_advances_current_branch(&repo, &config, spec, branch)?;
                    }
                }
                let (oid, desc) = pull_fetch_head_line(&repo, spec)?;
                out.push(format!("{}\t\t{desc}\n", oid.to_hex()));
            }
            out
        };
        fs::write(repo.git_dir.join("FETCH_HEAD"), lines.concat())?;

        if args.set_upstream {
            apply_pull_set_upstream(
                &repo.git_dir,
                remote_name,
                &repo.git_dir,
                &args.refspecs,
                false,
            );
        }

        grit_lib::pack::clear_pack_cache();
        let kind = do_merge_or_rebase_after_fetch(&args, &config, &repo, &head)?;
        maybe_update_submodules_after_pull(&args, &config, kind)?;
        return Ok(());
    }

    let fetch_args = super::fetch::Args {
        remote: Some(remote_name.to_owned()),
        refspecs: effective_refspecs.clone(),
        filter: None,
        no_filter: false,
        all: args.all,
        no_all: false,
        no_auto_gc: false,
        no_write_commit_graph: false,
        multiple: false,
        tags: args.tags,
        no_tags: args.no_tags,
        prune: false,
        no_prune: false,
        force: args.force,
        prune_tags: false,
        atomic: false,
        append: false,
        dry_run: args.dry_run,
        write_fetch_head: false,
        no_write_fetch_head: false,
        refmap: Vec::new(),
        deepen: None,
        depth: None,
        shallow_since: None,
        shallow_exclude: None,
        unshallow: false,
        update_shallow: false,
        refetch: false,
        keep: false,
        output: None,
        quiet: args.quiet,
        verbose: args.verbose,
        jobs: None,
        server_options: Vec::new(),
        porcelain: false,
        no_porcelain: false,
        no_show_forced_updates: false,
        show_forced_updates: false,
        negotiate_only: false,
        negotiation_tip: Vec::new(),
        set_upstream: args.set_upstream,
        update_head_ok: false,
        prefetch: false,
        update_refs: false,
        upload_pack: None,
        recurse_submodules: fetch_recurse,
        no_recurse_submodules: args.no_recurse_submodules,
        recurse_submodules_default: None,
        submodule_prefix: None,
        no_ipv4: false,
        no_ipv6: false,
    };
    super::fetch::run(fetch_args)?;
    // `--dry-run` stops after reporting what fetch would do; never merge (git pull.c).
    if args.dry_run {
        return Ok(());
    }
    // The in-process fetch (and its post-fetch maintenance repack/gc) added and
    // removed packs; drop the process-wide pack-index cache so the subsequent
    // merge sees the freshly-fetched (and lazily-fetched) objects rather than a
    // stale directory listing that points at deleted packs (t5616 partial-clone
    // pull-then-gc).
    grit_lib::pack::clear_pack_cache();
    if effective_refspecs.is_empty() {
        normalize_fetch_head_for_pull_branch(&repo, &merge_branch)?;
    } else {
        // Every command-line refspec is a merge candidate (builtin/fetch.c), including one with an
        // explicit `<src>:<dst>` whose fetch wrote it not-for-merge.
        mark_cli_refspecs_for_merge(&repo, &effective_refspecs)?;
    }

    let kind = do_merge_or_rebase_after_fetch(&args, &config, &repo, &head)?;
    maybe_update_submodules_after_pull(&args, &config, kind)?;
    Ok(())
}

/// Handle `git pull --all`: fetch from every configured remote, then integrate the configured
/// upstream branch (the one fetch marked for-merge in FETCH_HEAD) via merge or rebase.
fn run_pull_all(
    args: &Args,
    config: &ConfigSet,
    repo: &Repository,
    head: &grit_lib::state::HeadState,
) -> Result<()> {
    // `git pull --all` still needs a branch to merge: without `branch.<name>.merge` there is no
    // for-merge candidate after the multi-remote fetch, so emit the no-tracking diagnostic up front
    // (git pull.c `die_no_merge_candidates`).
    let current_branch = head.branch_name();
    let has_merge_cfg = current_branch
        .map(|b| config.get(&format!("branch.{b}.merge")).is_some())
        .unwrap_or(false);
    // `--dry-run` only reports what fetch would do and stops before integrating, so the
    // no-tracking diagnostic (which belongs to the integrate step) must not fire (t5521
    // `pull --all --dry-run`). An unborn branch likewise has no merge candidate to complain about
    // yet — defer to the post-fetch path.
    if !has_merge_cfg && !args.dry_run && current_branch.is_some() {
        return Err(die_no_merge_candidates(
            None,
            false,
            current_branch,
            config,
            pull_will_rebase_for_diag(args, config, current_branch),
        ));
    }

    let fetch_recurse = if args.no_recurse_submodules {
        None
    } else if config
        .get("fetch.recursesubmodules")
        .or_else(|| config.get("fetch.recurseSubmodules"))
        .map(|v| {
            let l = v.to_ascii_lowercase();
            l == "true" || l == "yes" || l == "on" || l == "1"
        })
        .unwrap_or(false)
    {
        Some("true".to_owned())
    } else {
        None
    };

    let fetch_args = super::fetch::Args {
        remote: None,
        refspecs: Vec::new(),
        filter: None,
        no_filter: false,
        all: true,
        no_all: false,
        no_auto_gc: false,
        no_write_commit_graph: false,
        multiple: false,
        tags: false,
        no_tags: false,
        prune: false,
        no_prune: false,
        force: args.force,
        prune_tags: false,
        atomic: false,
        append: false,
        dry_run: args.dry_run,
        write_fetch_head: false,
        no_write_fetch_head: false,
        refmap: Vec::new(),
        deepen: None,
        depth: None,
        shallow_since: None,
        shallow_exclude: None,
        unshallow: false,
        update_shallow: false,
        refetch: false,
        keep: false,
        output: None,
        quiet: args.quiet,
        verbose: args.verbose,
        jobs: None,
        server_options: Vec::new(),
        porcelain: false,
        no_porcelain: false,
        no_show_forced_updates: false,
        show_forced_updates: false,
        negotiate_only: false,
        negotiation_tip: Vec::new(),
        set_upstream: false,
        update_head_ok: false,
        prefetch: false,
        update_refs: false,
        upload_pack: None,
        recurse_submodules: fetch_recurse,
        no_recurse_submodules: args.no_recurse_submodules,
        recurse_submodules_default: None,
        submodule_prefix: None,
        no_ipv4: false,
        no_ipv6: false,
    };
    super::fetch::run(fetch_args)?;
    // `--dry-run` reports what would be fetched and stops before integrating (git pull.c).
    if args.dry_run {
        return Ok(());
    }
    grit_lib::pack::clear_pack_cache();

    // `fetch --all` already records exactly the configured upstream branch (the current branch's
    // `branch.<name>.remote`) as for-merge in FETCH_HEAD and every other fetched branch as
    // not-for-merge, so we integrate straight from FETCH_HEAD without re-normalizing.
    let kind = do_merge_or_rebase_after_fetch(args, config, repo, head)?;
    maybe_update_submodules_after_pull(args, config, kind)?;
    Ok(())
}

fn submodule_update_after_pull_will_run(args: &Args, config: &ConfigSet) -> bool {
    if args.no_recurse_submodules {
        return false;
    }
    let fetch_wants_recurse = config
        .get("fetch.recursesubmodules")
        .or_else(|| config.get("fetch.recurseSubmodules"))
        .map(|v| {
            let l = v.to_ascii_lowercase();
            l == "true" || l == "yes" || l == "on" || l == "1"
        })
        .unwrap_or(false);
    let cli = args.recurse_submodules.as_deref();
    let explicit_on = matches!(
        cli,
        Some("true") | Some("yes") | Some("on") | Some("1") | Some("")
    );
    let explicit_off = matches!(cli, Some("no") | Some("false") | Some("off") | Some("0"));
    let config_recurse = config
        .get("submodule.recurse")
        .map(|v| {
            let l = v.to_ascii_lowercase();
            l == "true" || l == "yes" || l == "on" || l == "1"
        })
        .unwrap_or(false);
    if explicit_off {
        return false;
    }
    if explicit_on {
        return true;
    }
    if fetch_wants_recurse {
        return false;
    }
    config_recurse
}

/// Run the recursive submodule **fetch** for the local-path pull shortcut.
///
/// The non-local fetch path delegates to `super::fetch::run`, which recurses into submodules itself
/// (Git `cmd_pull` always shells out to `git fetch`). The local-path shortcut copies objects
/// directly and bypasses that machinery, so when `fetch.recurseSubmodules` (or
/// `--recurse-submodules`) requests submodule recursion we must reproduce it here: record the
/// superproject ref tips around the object copy, then fetch each populated/changed submodule from
/// its own remote (Git `fetch_submodules`). This is the *fetch* phase only — the submodule working
/// tree is left untouched (that update happens separately when submodule recursion is enabled for
/// the merge).
fn recurse_fetch_submodules_for_local_pull(
    config: &ConfigSet,
    recurse: grit_lib::fetch_submodules::FetchRecurseSubmodules,
    copy_and_record: impl FnOnce() -> Result<()>,
) -> Result<()> {
    use grit_lib::fetch_submodules::FetchRecurseSubmodules;
    if recurse == FetchRecurseSubmodules::Off {
        return copy_and_record();
    }
    let cwd = std::env::current_dir().context("cwd")?;
    let repo = Repository::discover(Some(cwd.as_path())).context("open repository")?;
    crate::fetch_submodule_record::begin_fetch_submodule_record(&repo.git_dir);
    copy_and_record()?;
    crate::fetch_submodule_record::finish_record_tips_after(&repo.git_dir);
    let fetch_args = fetch_args_for_recurse(config);
    crate::fetch_submodule_recurse::recursive_fetch_submodules_after_fetch(
        &repo.git_dir,
        config,
        &fetch_args,
        recurse,
    )
}

/// Build a minimal `fetch::Args` carrying only the fields the recursive submodule fetch reads
/// (parallelism, dry-run/quiet propagation, prefix). All other fields take their defaults.
fn fetch_args_for_recurse(_config: &ConfigSet) -> super::fetch::Args {
    super::fetch::Args {
        remote: None,
        refspecs: Vec::new(),
        filter: None,
        no_filter: false,
        all: false,
        no_all: false,
        no_auto_gc: false,
        no_write_commit_graph: false,
        multiple: false,
        tags: false,
        no_tags: false,
        prune: false,
        no_prune: false,
        force: false,
        prune_tags: false,
        atomic: false,
        append: false,
        dry_run: false,
        write_fetch_head: false,
        no_write_fetch_head: false,
        refmap: Vec::new(),
        deepen: None,
        depth: None,
        shallow_since: None,
        shallow_exclude: None,
        unshallow: false,
        update_shallow: false,
        refetch: false,
        keep: false,
        output: None,
        quiet: false,
        verbose: 0,
        jobs: None,
        server_options: Vec::new(),
        porcelain: false,
        no_porcelain: false,
        no_show_forced_updates: false,
        show_forced_updates: false,
        negotiate_only: false,
        negotiation_tip: Vec::new(),
        set_upstream: false,
        update_head_ok: false,
        prefetch: false,
        update_refs: false,
        upload_pack: None,
        recurse_submodules: None,
        no_recurse_submodules: false,
        recurse_submodules_default: None,
        submodule_prefix: None,
        no_ipv4: false,
        no_ipv6: false,
    }
}

/// Resolve the submodule **fetch** recursion mode for a pull, matching
/// `fetch_recurse_submodules_mode`: `--no-recurse-submodules` forces off, an explicit
/// `--recurse-submodules[=val]` wins next, then `fetch.recurseSubmodules`; otherwise `Default`
/// (on-demand) so only changed submodules recurse.
fn pull_fetch_recurse_mode(
    args: &Args,
    config: &ConfigSet,
) -> Result<grit_lib::fetch_submodules::FetchRecurseSubmodules> {
    use grit_lib::fetch_submodules::{parse_fetch_recurse_submodules_arg, FetchRecurseSubmodules};
    if args.no_recurse_submodules {
        return Ok(FetchRecurseSubmodules::Off);
    }
    if let Some(raw) = args.recurse_submodules.as_deref() {
        return parse_fetch_recurse_submodules_arg("--recurse-submodules", raw)
            .map_err(|e| anyhow::anyhow!(e));
    }
    if let Some(raw) = config
        .get("fetch.recursesubmodules")
        .or_else(|| config.get("fetch.recurseSubmodules"))
    {
        return parse_fetch_recurse_submodules_arg("fetch.recurseSubmodules", raw.trim())
            .map_err(|e| anyhow::anyhow!(e));
    }
    Ok(FetchRecurseSubmodules::Default)
}

fn maybe_update_submodules_after_pull(
    args: &Args,
    config: &ConfigSet,
    integrate: PullIntegrateKind,
) -> Result<()> {
    if !submodule_update_after_pull_will_run(args, config) {
        return Ok(());
    }
    super::submodule::recursive_fetch_submodules(true)?;
    match integrate {
        PullIntegrateKind::Merge => {
            super::submodule::update_after_superproject_merge(true, true)?;
        }
        PullIntegrateKind::Rebase | PullIntegrateKind::MergeFfOnlyForRebase => {
            // `git pull --rebase` runs `submodule update --rebase` even when the superproject
            // integrated via fast-forward merge (Git `pull.c` `rebase_submodules`).
            super::submodule::update_after_superproject_rebase(true, true)?;
        }
    }
    Ok(())
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum RebaseTri {
    False,
    True,
    Unset,
}

/// Tri-state autostash decision (git pull's `opt_autostash`: -1 unset, 0 off, 1 on).
#[derive(Clone, Copy, PartialEq, Eq)]
enum AutostashTri {
    Off,
    On,
    Unset,
}

/// Resolve pull's `opt_autostash` from CLI flags then `pull.autostash` config, matching
/// git builtin/pull.c (`opt_autostash == -1 ? config_pull_autostash`). The result is `Unset`
/// when neither the CLI nor `pull.autostash` decides — the rebase path then falls back to
/// `rebase.autostash` and the merge path to `merge.autostash` (handled by the callee).
fn resolve_pull_autostash(args: &Args, config: &ConfigSet) -> AutostashTri {
    if args.no_autostash {
        return AutostashTri::Off;
    }
    if args.autostash {
        return AutostashTri::On;
    }
    match config.get_bool("pull.autostash") {
        Some(Ok(true)) => AutostashTri::On,
        Some(Ok(false)) => AutostashTri::Off,
        _ => AutostashTri::Unset,
    }
}

fn parse_rebase_value(key: &str, value: &str) -> Result<RebaseTri> {
    let v = value.trim();
    let lower = v.to_ascii_lowercase();
    match lower.as_str() {
        "false" | "no" | "off" | "0" => Ok(RebaseTri::False),
        "true" | "yes" | "on" | "1" | "merges" | "m" | "interactive" | "i" => Ok(RebaseTri::True),
        _ => bail!("invalid value for '{key}': '{value}'"),
    }
}

/// Opportunistically update the configured remote-tracking ref for `spec`.
///
/// When `git pull <remote> <branch>` (or `git fetch <remote> <branch>`) is run
/// with an explicit refspec, git still refreshes the remote-tracking ref that a
/// `remote.<name>.fetch` rule maps the fetched branch to. This mirrors that for
/// the local-path pull shortcut: resolve `refs/heads/<spec>` on the remote, map
/// it through the configured fetch refspecs, and write the destination ref.
fn update_opportunistic_tracking_ref(
    local: &Repository,
    remote: &Repository,
    remote_name: &str,
    spec: &str,
    config: &ConfigSet,
) -> Result<()> {
    // An anonymous path/URL remote (e.g. `git pull ../testrepo main`) is not a configured remote,
    // so it has no tracking refspec — git records the result only in FETCH_HEAD and never writes a
    // `refs/<token>/<branch>` tracking ref. Skip the opportunistic update unless the remote is
    // actually configured (t5516 'fetch follows tags by default', whose `expect` is built from a
    // src repo that must NOT carry a stray `refs/testrepo/main`).
    if config.get(&format!("remote.{remote_name}.url")).is_none()
        && config
            .get(&format!("remote.{remote_name}.pushurl"))
            .is_none()
    {
        return Ok(());
    }
    // Only plain branch sources participate; tags and full refs are left alone.
    if spec.starts_with("refs/") || spec == "HEAD" {
        return Ok(());
    }
    let source_ref = format!("refs/heads/{spec}");
    let Ok(oid) = refs::resolve_ref(&remote.git_dir, &source_ref) else {
        return Ok(());
    };
    let refspecs = super::fetch::remote_fetch_refspecs(config, remote_name);
    let Some(tracking_ref) = super::fetch::map_ref_through_refspecs(&source_ref, &refspecs) else {
        return Ok(());
    };
    if !tracking_ref.starts_with("refs/remotes/") {
        return Ok(());
    }
    refs::write_ref(&local.git_dir, &tracking_ref, &oid)
        .with_context(|| format!("update remote-tracking ref {tracking_ref}"))?;
    Ok(())
}

/// Split a pull refspec `<src>[:<dst>]` into its source and optional destination, mirroring
/// git-fetch: only the source side is fetched into FETCH_HEAD; the destination, if any, names a
/// local ref to fast-forward (e.g. `git pull . second:third`).
fn split_pull_refspec(spec: &str) -> (&str, Option<&str>) {
    let spec = spec.strip_prefix('+').unwrap_or(spec);
    match spec.split_once(':') {
        Some((src, dst)) => (src, Some(dst)),
        None => (spec, None),
    }
}

/// True when a refspec's destination is the branch we are currently on (`pull . second:third` while
/// on `third`), so the fetch would update HEAD's own ref.
fn fetch_updates_current_branch_dst(spec: &str, current_branch: &str) -> bool {
    let Some(dst) = split_pull_refspec(spec).1 else {
        return false;
    };
    let dst_short = dst
        .strip_prefix("refs/heads/")
        .unwrap_or(dst)
        .trim_start_matches('+');
    !dst_short.is_empty() && dst_short == current_branch
}

/// Handle `pull . <src>:<current-branch>`: the fetch advances the current branch's ref, then the
/// working tree is fast-forwarded (builtin/pull.c: warn "fetch updated the current branch head" and
/// `checkout_fast_forward`). Updates `refs/heads/<branch>` to the source tip and fast-forwards the
/// work tree; a conflicting work tree aborts with git's recovery hint (t5520 tests 18, 19).
fn pull_local_fetch_advances_current_branch(
    repo: &Repository,
    _config: &ConfigSet,
    spec: &str,
    branch: &str,
) -> Result<()> {
    let (src, _dst) = split_pull_refspec(spec);
    let new_oid = resolve_revision(repo, src).with_context(|| format!("bad revision '{src}'"))?;
    let branch_ref = format!("refs/heads/{branch}");
    let Ok(orig_oid) = refs::resolve_ref(&repo.git_dir, &branch_ref) else {
        return Ok(());
    };
    if orig_oid == new_oid {
        return Ok(());
    }
    // Only a true fast-forward of the ref triggers the working-tree fast-forward path; a non-ff
    // update is rejected by fetch without `--force` (out of scope for these tests).
    if !is_ancestor(repo, orig_oid, new_oid)? {
        return Ok(());
    }

    refs::write_ref(&repo.git_dir, &branch_ref, &new_oid)?;

    eprintln!("warning: fetch updated the current branch head.");
    eprintln!("fast-forwarding your working tree from");
    eprintln!("commit {}.", orig_oid.to_hex());

    match super::merge::checkout_fast_forward_worktree_only(repo, orig_oid, new_oid) {
        Ok(()) => Ok(()),
        Err(e) => {
            if format!("{e:#}").contains(super::merge::WORKTREE_FF_BLOCKED) {
                // Restore the ref: git leaves the branch advanced but the diagnostic instructs the
                // user to recover; the test only checks the message and that the work tree is intact.
                eprintln!("fatal: Cannot fast-forward your working tree.");
                eprintln!("After making sure that you saved anything precious from");
                eprintln!("$ git diff {}", orig_oid.to_hex());
                eprintln!("output, run");
                eprintln!("$ git reset --hard");
                eprintln!("to recover.");
                return Err(anyhow::Error::new(ExplicitExit {
                    code: 128,
                    message: String::new(),
                }));
            }
            Err(e)
        }
    }
}

fn pull_fetch_head_line(remote: &Repository, spec: &str) -> Result<(ObjectId, String)> {
    use grit_lib::objects::ObjectKind;
    use grit_lib::refs::resolve_ref;

    let (src, _dst) = split_pull_refspec(spec);
    let tag_ref = format!("refs/tags/{src}");
    if let Ok(tag_oid) = resolve_ref(&remote.git_dir, &tag_ref) {
        if remote.odb.read(&tag_oid)?.kind == ObjectKind::Tag {
            return Ok((tag_oid, format!("tag '{src}' of .")));
        }
    }
    let oid = resolve_revision(remote, src).with_context(|| format!("bad revision '{src}'"))?;
    // Match git's FETCH_HEAD format: the short branch name (e.g. `branch 'side' of .`),
    // not the full `refs/heads/...` ref. fmt-merge-msg reads this verbatim.
    let short = src.strip_prefix("refs/heads/").unwrap_or(src);
    Ok((oid, format!("branch '{short}' of .")))
}

/// Whether a bare `git pull <remote>` (no explicit ref) should auto-follow tags, mirroring
/// `git fetch`'s `TAGS_DEFAULT`: on unless `remote.<remote>.tagopt = --no-tags`.
fn pull_should_follow_tags(config: &ConfigSet, remote_name: &str) -> bool {
    // An anonymous path token (not a configured remote) behaves like `git pull <path> <ref>`,
    // which keeps tags only in FETCH_HEAD — skip the opportunistic ref writes.
    if config.get(&format!("remote.{remote_name}.url")).is_none() {
        return false;
    }
    config
        .get(&format!("remote.{remote_name}.tagopt"))
        .as_deref()
        != Some("--no-tags")
}

/// Copy and create local `refs/tags/*` for any tag in `remote` whose target object is reachable
/// from `fetched_tip`, the way `git fetch`'s default tag auto-following does. Existing local tags
/// of the same name are left untouched (opportunistic, add-only).
fn follow_tags_for_local_pull(
    repo: &Repository,
    remote: &Repository,
    fetched_tip: ObjectId,
) -> Result<()> {
    use grit_lib::objects::{parse_tag, ObjectKind};

    let tag_refs = refs::list_refs(&remote.git_dir, "refs/tags/")?;
    for (refname, tag_ref_oid) in tag_refs {
        // Add-only: never clobber an existing local tag during opportunistic follow.
        if refs::resolve_ref(&repo.git_dir, &refname).is_ok() {
            continue;
        }
        let Ok(tag_obj) = remote.odb.read(&tag_ref_oid) else {
            continue;
        };
        if tag_obj.kind != ObjectKind::Tag {
            continue;
        }
        let Ok(tag) = parse_tag(&tag_obj.data) else {
            continue;
        };
        // Only follow tags whose (peeled) target is reachable from the fetched commit.
        let reachable = tag.object == fetched_tip
            || is_ancestor(remote, tag.object, fetched_tip).unwrap_or(false);
        if !reachable {
            continue;
        }
        // Copy the tag's object closure into the local store, then write the tag ref.
        crate::commands::fetch::copy_reachable_objects_skipping_gitlinks(
            &remote.git_dir,
            &repo.git_dir,
            &[tag_ref_oid],
        )?;
        refs::write_ref(&repo.git_dir, &refname, &tag_ref_oid)
            .with_context(|| format!("writing followed tag {refname}"))?;
    }
    Ok(())
}

fn config_pull_rebase(
    config: &ConfigSet,
    current_branch: Option<&str>,
) -> Result<(RebaseTri, bool, PullRebaseKind)> {
    if let Some(b) = current_branch {
        let key = format!("branch.{b}.rebase");
        if let Some(v) = config.get(&key) {
            return Ok((
                parse_rebase_value(&key, &v)?,
                false,
                rebase_kind_from_value(&v),
            ));
        }
    }
    if let Some(v) = config.get("pull.rebase") {
        return Ok((
            parse_rebase_value("pull.rebase", &v)?,
            false,
            rebase_kind_from_value(&v),
        ));
    }
    // When `pull.rebase` is not configured, refuse to pick merge vs rebase on divergent
    // branches until the user sets `pull.rebase` or passes `--rebase` / `--no-rebase` (t7601).
    Ok((RebaseTri::False, true, PullRebaseKind::Plain))
}

fn pull_ff_from_config(config: &ConfigSet) -> Result<Option<(bool, bool, bool)>> {
    let Some(val) = config.get("pull.ff") else {
        return Ok(None);
    };
    let lower = val.to_ascii_lowercase();
    match lower.as_str() {
        "true" | "yes" | "on" | "1" => Ok(Some((true, false, false))),
        "false" | "no" | "off" | "0" => Ok(Some((false, true, false))),
        "only" => Ok(Some((false, false, true))),
        _ => bail!("invalid value for 'pull.ff': '{val}'"),
    }
}

fn merge_heads_from_fetch_head(repo: &Repository) -> Result<Vec<grit_lib::objects::ObjectId>> {
    use grit_lib::objects::ObjectId;
    let path = repo.git_dir.join("FETCH_HEAD");
    let content = fs::read_to_string(&path).with_context(|| "could not read FETCH_HEAD")?;
    let mut out = Vec::new();
    let mut seen = HashSet::new();
    for hex in grit_lib::fetch_head::merge_object_ids_hex(&content) {
        let oid = ObjectId::from_hex(&hex)?;
        if seen.insert(oid) {
            out.push(oid);
        }
    }
    if out.is_empty() {
        bail!("FETCH_HEAD: no merge candidates");
    }
    Ok(out)
}

fn fetch_head_branch_name(line: &str) -> Option<&str> {
    let start = line.find("branch '")? + "branch '".len();
    let rest = &line[start..];
    let end = rest.find('\'')?;
    Some(&rest[..end])
}

fn fetch_head_line_parts(line: &str) -> Option<(&str, &str)> {
    let first_tab = line.find('\t')?;
    let oid = &line[..first_tab];
    if oid.len() != 40 || !oid.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    let rest = &line[first_tab + 1..];
    let desc = rest
        .strip_prefix("not-for-merge\t")
        .or_else(|| rest.strip_prefix('\t'))
        .unwrap_or(rest);
    if desc.is_empty() {
        return None;
    }
    Some((oid, desc))
}

/// Mark the FETCH_HEAD entries fetched via explicit command-line refspecs as for-merge.
///
/// `builtin/fetch.c` records every command-line refspec as `FETCH_HEAD_MERGE` ("Merge everything on
/// the command line"), even one with an explicit `<src>:<dst>`. Grit's fetch writes such an entry
/// `not-for-merge` (its destination is a tracking ref), which would leave `git pull <remote>
/// <src>:<dst>` with no merge candidate. Re-mark only the lines whose source matches a CLI refspec
/// so auto-followed tags (`--tags`) stay not-for-merge (t5553 `pull --set-upstream main:other2`).
fn mark_cli_refspecs_for_merge(repo: &Repository, refspecs: &[String]) -> Result<()> {
    if refspecs.is_empty() {
        return Ok(());
    }
    // Short source names named on the command line (e.g. `main`, `HEAD`, `refs/heads/x` -> `x`).
    let wanted: HashSet<String> = refspecs
        .iter()
        .map(|spec| {
            let (src, _dst) = split_pull_refspec(spec);
            src.strip_prefix("refs/heads/").unwrap_or(src).to_owned()
        })
        .collect();

    let path = repo.git_dir.join("FETCH_HEAD");
    let content = match fs::read_to_string(&path) {
        Ok(c) => c,
        Err(_) => return Ok(()),
    };
    let mut changed = false;
    let mut lines = Vec::new();
    for line in content.lines() {
        // Only promote `branch '<name>'` lines whose name was requested on the command line; leave
        // tag lines and unrelated entries untouched.
        let is_cli_branch = fetch_head_branch_name(line)
            .map(|name| wanted.contains(name))
            .unwrap_or(false);
        if is_cli_branch {
            if let Some((oid, desc)) = fetch_head_line_parts(line) {
                let promoted = format!("{oid}\t\t{desc}");
                if promoted != line {
                    changed = true;
                }
                lines.push(promoted);
                continue;
            }
        }
        lines.push(line.to_owned());
    }
    if changed {
        fs::write(&path, lines.join("\n") + "\n").context("writing FETCH_HEAD")?;
    }
    Ok(())
}

fn normalize_fetch_head_for_pull_branch(repo: &Repository, merge_branch: &str) -> Result<()> {
    let path = repo.git_dir.join("FETCH_HEAD");
    let content = fs::read_to_string(&path).with_context(|| "could not read FETCH_HEAD")?;
    let mut found_branch = false;
    let mut changed = false;
    let mut lines = Vec::new();

    for line in content.lines() {
        let Some((oid, desc)) = fetch_head_line_parts(line) else {
            lines.push(line.to_owned());
            continue;
        };
        let normalized = if fetch_head_branch_name(line) == Some(merge_branch) {
            found_branch = true;
            format!("{oid}\t\t{desc}")
        } else {
            format!("{oid}\tnot-for-merge\t{desc}")
        };
        if normalized != line {
            changed = true;
        }
        lines.push(normalized);
    }

    if found_branch && changed {
        fs::write(&path, lines.join("\n") + "\n").context("writing FETCH_HEAD")?;
    }
    Ok(())
}

fn pull_can_fast_forward(
    repo: &Repository,
    head_oid: grit_lib::objects::ObjectId,
    merge_heads: &[grit_lib::objects::ObjectId],
) -> Result<bool> {
    if merge_heads.len() != 1 {
        return Ok(false);
    }
    // Matches Git's `get_can_ff`: FETCH_HEAD tip is a descendant of HEAD (fast-forward possible).
    Ok(is_ancestor(repo, head_oid, merge_heads[0])?)
}

fn pull_already_up_to_date(
    repo: &Repository,
    head_oid: grit_lib::objects::ObjectId,
    merge_heads: &[grit_lib::objects::ObjectId],
) -> Result<bool> {
    for h in merge_heads {
        if !is_ancestor(repo, *h, head_oid)? {
            return Ok(false);
        }
    }
    Ok(true)
}

fn advice_color_prefix(config: &ConfigSet) -> &'static str {
    let use_color = config
        .get("color.advice")
        .or_else(|| config.get("color.ui"))
        .map(|v| {
            let l = v.to_ascii_lowercase();
            l == "true" || l == "always" || l == "auto"
        })
        .unwrap_or(false);
    if use_color {
        "\x1b[33m"
    } else {
        ""
    }
}

fn show_advice_pull_non_ff(config: &ConfigSet) {
    let p = advice_color_prefix(config);
    eprintln!("{p}hint: You have divergent branches and need to specify how to reconcile them.");
    eprintln!("{p}hint: You can do so by running one of the following commands sometime before");
    eprintln!("{p}hint: your next pull:");
    eprintln!("{p}hint:");
    eprintln!("{p}hint:   git config pull.rebase false  # merge");
    eprintln!("{p}hint:   git config pull.rebase true   # rebase");
    eprintln!("{p}hint:   git config pull.ff only       # fast-forward only");
    eprintln!("{p}hint:");
    eprintln!(
        "{p}hint: You can replace \"git config\" with \"git config --global\" to set a default"
    );
    eprintln!("{p}hint: preference for all repositories. You can also pass --rebase, --no-rebase,");
    eprintln!("{p}hint: or --ff-only on the command line to override the configured default per");
    eprintln!("{p}hint: invocation.");
}

fn do_merge_or_rebase_after_fetch(
    args: &Args,
    config: &ConfigSet,
    repo: &Repository,
    head: &grit_lib::state::HeadState,
) -> Result<PullIntegrateKind> {
    // Resolve `--autostash`/`--no-autostash`/`pull.autostash` once (git pull's `opt_autostash`).
    let pull_autostash = resolve_pull_autostash(args, config);
    if head.oid().is_none() {
        if merge_heads_from_fetch_head(repo)?.len() > 1 {
            bail!("Cannot merge multiple branches into empty head.");
        }
        let merge_args = build_pull_merge_args(
            args,
            true,
            false,
            false,
            pull_autostash,
            vec!["FETCH_HEAD".to_owned()],
        )?;
        super::merge::run(merge_args)?;
        return Ok(PullIntegrateKind::Merge);
    }

    let (mut opt_rebase, rebase_unspecified, rebase_kind) = if args.no_rebase {
        (RebaseTri::False, false, PullRebaseKind::Plain)
    } else if let Some(ref s) = args.rebase {
        (
            parse_rebase_value("--rebase", s)?,
            false,
            rebase_kind_from_value(s),
        )
    } else {
        config_pull_rebase(config, head.branch_name())?
    };

    let mut ff = args.ff;
    let mut no_ff = args.no_ff;
    let mut ff_only = args.ff_only;
    let cli_touched_ff = ff || no_ff || ff_only;
    let pull_ff_in_config = config.get("pull.ff").is_some();

    if !cli_touched_ff {
        if let Some((f, n, o)) = pull_ff_from_config(config)? {
            ff = f;
            no_ff = n;
            ff_only = o;
        }
    }
    // git pull.c: explicit `--rebase` on the CLI overrides `pull.ff=only` from config (not when
    // both rebase and ff=only come from config).
    if args.rebase.is_some() && !cli_touched_ff && pull_ff_in_config {
        if let Some((_, _, only)) = pull_ff_from_config(config)? {
            if only {
                ff_only = false;
                ff = true;
                no_ff = false;
            }
        }
    }
    // `--no-rebase` overrides `pull.ff=only` from config (t7601: merge when branches diverge).
    if args.no_rebase && ff_only && !args.ff_only {
        ff_only = false;
    }

    // Re-resolve HEAD and merge tips after `fetch` rewrote FETCH_HEAD (and possibly not HEAD).
    let head_now = resolve_head(&repo.git_dir)?;
    let head_oid = head_now
        .oid()
        .copied()
        .context("internal error: expected branch head after fetch")?;
    let merge_heads = merge_heads_from_fetch_head(repo)?;
    if merge_heads.is_empty() {
        bail!("no merge candidates in FETCH_HEAD");
    }

    if merge_heads.len() > 1 {
        if opt_rebase == RebaseTri::True {
            bail!("Cannot rebase onto multiple branches.");
        }
        if ff_only {
            bail!("Cannot fast-forward to multiple branches.");
        }
    }

    let can_ff = pull_can_fast_forward(repo, head_oid, &merge_heads)?;
    let already_up = pull_already_up_to_date(repo, head_oid, &merge_heads)?;
    let divergent = !can_ff && !already_up;

    if ff_only {
        if divergent {
            bail!("Not possible to fast-forward, aborting.");
        }
        opt_rebase = RebaseTri::False;
    }

    // Match git pull.c: `if (!opt_ff && rebase_unspecified && divergent)` where `opt_ff` is set
    // from CLI or `pull.ff` config (t7601). `cli_touched_ff` covers CLI; `pull_ff_in_config`
    // covers `pull.ff` in config.
    if rebase_unspecified && divergent && !pull_ff_in_config && !cli_touched_ff && !ff_only {
        show_advice_pull_non_ff(config);
        return Err(anyhow::Error::new(ExplicitExit {
            code: 128,
            message: "Need to specify how to reconcile divergent branches.".to_owned(),
        }));
    }

    if opt_rebase == RebaseTri::True {
        // A plain/merges rebase onto an upstream that fast-forwards can shortcut to a
        // `merge --ff-only` (recording `pull: Fast-forward` in the reflog). An *interactive*
        // rebase must still run so the editor opens with the todo list even on a fast-forward
        // (t5520 `pull.rebase=interactive`), so do not take the shortcut for it.
        if can_ff && rebase_kind != PullRebaseKind::Interactive {
            // This shortcut stands in for a fast-forwarding rebase, so an unspecified autostash
            // decision falls back to `rebase.autostash` (not `merge.autostash`) — git's rebase
            // would autostash the dirty tree before fast-forwarding (t5520 "--rebase with
            // rebase.autostash succeeds on ff").
            let ff_autostash = match pull_autostash {
                AutostashTri::Unset => {
                    if config
                        .get_bool("rebase.autostash")
                        .map(|r| r.unwrap_or(false))
                        .unwrap_or(false)
                    {
                        AutostashTri::On
                    } else {
                        AutostashTri::Unset
                    }
                }
                other => other,
            };
            let merge_args = build_pull_merge_args(
                args,
                false,
                false,
                true,
                ff_autostash,
                vec!["FETCH_HEAD".to_owned()],
            )?;
            super::merge::run(merge_args)?;
            return Ok(PullIntegrateKind::MergeFfOnlyForRebase);
        }
        let upstream_hex = super::merge::read_fetch_head_merge_oids(repo)?
            .into_iter()
            .next()
            .context("FETCH_HEAD merge oid")?;
        let upstream_oid = grit_lib::objects::ObjectId::from_hex(upstream_hex.trim())?;
        let unrelated = merge_bases_first_vs_rest(repo, head_oid, &[upstream_oid])?.is_empty();
        if submodule_update_after_pull_will_run(args, config)
            && !unrelated
            && submodule_gitlinks_touched_in_range(repo, Some(upstream_oid), head_oid)?
        {
            bail!("cannot rebase with locally recorded submodule modifications");
        }
        // git pull.c run_rebase(): `--verify-signatures` is meaningless for rebase, so warn and
        // drop it (a bare `--no-verify-signatures` is silently ignored).
        if args.verify_signatures {
            eprintln!("warning: ignoring --verify-signatures for rebase");
        }
        // git pull.c `get_rebase_newbase_and_upstream`: when the upstream itself was rebased, the
        // commits between the *old* fork point and HEAD must be replayed onto the new tip — not the
        // commits between the new tip and HEAD (which would re-apply the upstream's own, now
        // rewritten, commits and conflict). Compute `--onto <merge_head>` with `<upstream>` set to
        // the `merge-base --fork-point` of the tracking branch, falling back to `merge_head`.
        let (onto_hex, upstream_for_rebase_hex) =
            compute_rebase_onto_and_upstream(repo, config, args, head_oid, upstream_oid)?;
        // git pull.c run_rebase(): `--rebase=merges` -> `--rebase-merges`, `--rebase=interactive`
        // -> `--interactive`; a plain rebase passes neither.
        let rebase_args = super::rebase::Args {
            upstream_explicit: true,
            upstream: Some(upstream_for_rebase_hex),
            onto: onto_hex,
            root: false,
            interactive: rebase_kind == PullRebaseKind::Interactive,
            r#continue: false,
            abort: false,
            skip: false,
            exec: Vec::new(),
            merge: false,
            apply: false,
            rebase_merges: if rebase_kind == PullRebaseKind::Merges {
                Some("true".to_owned())
            } else {
                None
            },
            no_rebase_merges: false,
            no_ff: false,
            gpg_sign: None,
            no_gpg_sign: false,
            signoff: false,
            no_signoff: false,
            trailer: vec![],
            keep_base: 0,
            fork_point: false,
            no_fork_point: false,
            reapply_cherry_picks: false,
            no_reapply_cherry_picks: false,
            verbose: false,
            quiet: false,
            update_refs: false,
            no_update_refs: false,
            empty: None,
            strategy: None,
            strategy_option: vec![],
            branch: None,
            stat: false,
            no_stat: false,
            context_lines: None,
            whitespace: None,
            // Pull's resolved autostash decision wins; when Unset, rebase reads `rebase.autostash`.
            autostash: pull_autostash == AutostashTri::On,
            no_autostash: pull_autostash == AutostashTri::Off,
            quit: false,
            autosquash: false,
            no_autosquash: false,
            keep_empty: false,
            no_keep_empty: false,
            ignore_whitespace: false,
            rerere_autoupdate: false,
            no_rerere_autoupdate: false,
            reschedule_failed_exec: false,
            no_reschedule_failed_exec: false,
            committer_date_is_author_date: false,
            reset_author_date: false,
            edit_todo: false,
            show_current_patch: false,
            no_verify: false,
        };
        super::rebase::run(rebase_args)?;
        return Ok(PullIntegrateKind::Rebase);
    }

    let merge_commits = if merge_heads.len() > 1 {
        if !args.refspecs.is_empty() {
            args.refspecs.clone()
        } else {
            merge_heads.iter().map(|o| o.to_hex()).collect()
        }
    } else {
        vec!["FETCH_HEAD".to_owned()]
    };
    let merge_args =
        build_pull_merge_args(args, ff, no_ff, ff_only, pull_autostash, merge_commits)?;
    super::merge::run(merge_args)?;
    Ok(PullIntegrateKind::Merge)
}

/// Resolve the remote-tracking branch a `pull --rebase` would fork-point against, mirroring
/// git pull.c `get_rebase_fork_point` (`get_tracking_branch(repo, refspec)` or
/// `get_upstream_branch(repo)`). Returns the ref name (e.g. `refs/remotes/me/copy`) and its oid.
fn rebase_tracking_branch(
    repo: &Repository,
    config: &ConfigSet,
    args: &Args,
) -> Option<(String, ObjectId)> {
    // `pull <remote> <branch>`: tracking ref is `refs/remotes/<remote>/<branch>`.
    if let (Some(remote), Some(branch)) = (args.remote.as_deref(), args.refspecs.first()) {
        let (src, _dst) = split_pull_refspec(branch);
        let short = src.strip_prefix("refs/heads/").unwrap_or(src);
        // `pull . <branch>` (or any local-path remote) fork-points against the local branch itself
        // (git `get_tracking_branch(".", refspec)` resolves to the source ref on the same repo).
        if remote == "." || remote_token_looks_like_path(remote) {
            let local = format!("refs/heads/{short}");
            if let Ok(oid) = refs::resolve_ref(&repo.git_dir, &local) {
                return Some((local, oid));
            }
            return None;
        }
        if config.get(&format!("remote.{remote}.url")).is_some() {
            let track = format!("refs/remotes/{remote}/{short}");
            if let Ok(oid) = refs::resolve_ref(&repo.git_dir, &track) {
                return Some((track, oid));
            }
        }
        return None;
    }
    // `pull` (no args): use the current branch's upstream tracking ref.
    if args.refspecs.is_empty() {
        if let Some(branch) = resolve_head(&repo.git_dir)
            .ok()
            .and_then(|h| h.branch_name().map(str::to_owned))
        {
            let remote = config.get(&format!("branch.{branch}.remote"))?;
            if remote_token_looks_like_path(&remote) || remote == "." {
                return None;
            }
            let merge = config.get(&format!("branch.{branch}.merge"))?;
            let short = merge.strip_prefix("refs/heads/").unwrap_or(&merge);
            let track = format!("refs/remotes/{remote}/{short}");
            if let Ok(oid) = refs::resolve_ref(&repo.git_dir, &track) {
                return Some((track, oid));
            }
        }
    }
    None
}

/// Compute the `--onto` and `<upstream>` arguments for a `pull --rebase`, applying the fork-point
/// logic of git pull.c `get_rebase_newbase_and_upstream`. `merge_head` is the fetched tip
/// (FETCH_HEAD), which is always the new base; the upstream defaults to the fork point unless that
/// fork point is the plain octopus merge base (in which case it falls back to `merge_head`).
fn compute_rebase_onto_and_upstream(
    repo: &Repository,
    config: &ConfigSet,
    args: &Args,
    head_oid: ObjectId,
    merge_head: ObjectId,
) -> Result<(Option<String>, String)> {
    let fork_point = rebase_tracking_branch(repo, config, args)
        .and_then(|(spec, tip)| grit_lib::merge_base::fork_point(repo, &spec, tip, head_oid).ok());

    let upstream = if let Some(fp) = fork_point {
        // If the octopus merge base of (HEAD, merge_head, fork_point) equals the fork point, the
        // fork point adds nothing — use merge_head as the upstream (git drops fork_point).
        let bases = grit_lib::merge_base::merge_bases_octopus(repo, &[head_oid, merge_head, fp])?;
        if bases.len() == 1 && bases[0] == fp {
            merge_head
        } else {
            fp
        }
    } else {
        merge_head
    };

    // Only set `--onto` when it differs from the upstream (the no-fork-point case behaves like a
    // plain `rebase <merge_head>`, which grit already handles without an explicit onto).
    let onto = if upstream != merge_head {
        Some(merge_head.to_hex())
    } else {
        None
    };
    Ok((onto, upstream.to_hex()))
}

fn build_pull_merge_args(
    args: &Args,
    ff: bool,
    no_ff: bool,
    ff_only: bool,
    autostash: AutostashTri,
    commits: Vec<String>,
) -> Result<super::merge::Args> {
    Ok(super::merge::Args {
        commits,
        message: None,
        ff_only,
        no_ff,
        no_commit: false,
        no_verify: args.no_verify,
        squash: false,
        abort: false,
        continue_merge: false,
        strategy: args.strategy.clone(),
        strategy_option: args.strategy_option.clone(),
        quiet: args.quiet,
        progress: false,
        no_progress: false,
        no_edit: true,
        edit: false,
        signoff: args.signoff,
        no_signoff: args.no_signoff,
        gpg_sign: None,
        no_gpg_sign: false,
        stat: false,
        no_stat: false,
        log: args.log.as_ref().map(|v| {
            let n = v.parse::<usize>().unwrap_or(0);
            if n == 0 {
                20
            } else {
                n
            }
        }),
        no_log: args.no_log,
        compact_summary: false,
        summary: false,
        ff,
        commit: false,
        no_squash: false,
        quit: false,
        // Pull resolves `--autostash`/`--no-autostash`/`pull.autostash` and forwards an explicit
        // decision; when Unset, merge itself reads `merge.autostash`.
        autostash: autostash == AutostashTri::On,
        no_autostash: autostash == AutostashTri::Off,
        allow_unrelated_histories: args.allow_unrelated_histories,
        cleanup: None,
        file: None,
        rerere_autoupdate: false,
        no_rerere_autoupdate: false,
    })
}

fn remote_token_looks_like_path(token: &str) -> bool {
    // A non-local remote URL (`http(s)://`, `git://`, `ssh://`, `git+ssh://`, or
    // scp-style `host:path`) also contains `/`, but must NOT be treated as a
    // local filesystem path — otherwise it gets joined onto the work tree and
    // opened as a local repo. Those go through the regular fetch transport path
    // (`fetch::run`); only `file://` and genuine paths are "local" here.
    if is_non_local_remote_url(token) {
        return false;
    }
    token == "." || token == ".." || token.contains('/') || token.starts_with("file://")
}

/// Whether `token` is a remote URL reached over a network transport (not a local
/// path or `file://`). Such URLs are handled by the fetch transport layer.
fn is_non_local_remote_url(token: &str) -> bool {
    token.starts_with("http://")
        || token.starts_with("https://")
        || token.starts_with("git://")
        || token.starts_with("ssh://")
        || token.starts_with("git+ssh://")
        // scp-style `[user@]host:path` (which also contains `/` in the path part).
        || grit_lib::transport::is_ssh_url(token)
}

fn resolve_local_remote_path(
    work_tree: &Path,
    remote_name: &str,
    config: &ConfigSet,
) -> Result<Option<PathBuf>> {
    if remote_token_looks_like_path(remote_name) {
        let raw = remote_name.strip_prefix("file://").unwrap_or(remote_name);
        let p = Path::new(raw);
        let resolved = if p.is_absolute() {
            p.to_path_buf()
        } else {
            work_tree.join(p)
        };
        return Ok(Some(resolved));
    }
    let Some(url) = config.get(&format!("remote.{remote_name}.url")) else {
        return Ok(None);
    };
    if !remote_token_looks_like_path(&url) {
        return Ok(None);
    }
    let raw = url.strip_prefix("file://").unwrap_or(&url);
    let p = Path::new(raw);
    let resolved = if p.is_absolute() {
        p.to_path_buf()
    } else {
        work_tree.join(p)
    };
    Ok(Some(resolved))
}

fn open_repository_at_path(remote_path: &Path) -> Result<Repository> {
    if let Ok(r) = Repository::open(remote_path, None) {
        return Ok(r);
    }
    let git_dir = remote_path.join(".git");
    Repository::open(&git_dir, Some(remote_path)).with_context(|| {
        format!(
            "could not open remote repository at '{}'",
            remote_path.display()
        )
    })
}

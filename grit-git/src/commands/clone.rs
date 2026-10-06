//! `grit clone` — clone a repository (local transport only).
//!
//! Copies objects, refs, and configuration from a source repository,
//! sets up the "origin" remote, and optionally checks out the default branch.

use crate::protocol_wire;
use anyhow::{bail, Context, Result};
use clap::Args as ClapArgs;
use filetime::{set_file_times, FileTime};
use grit_lib::check_ref_format::{check_refname_format, RefNameOptions};
use grit_lib::config::{ConfigFile, ConfigScope, ConfigSet};
use grit_lib::diff::zero_oid;
use grit_lib::hooks::{run_hook, HookResult};
use grit_lib::merge_base::is_ancestor;
use grit_lib::objects::{parse_commit, parse_tag, parse_tree, ObjectId, ObjectKind};
use grit_lib::promisor::{read_promisor_missing_oids, repo_treats_promisor_packs};
use grit_lib::refs;
use grit_lib::reftable;
use grit_lib::repo::{
    init_bare_clone_minimal, init_bare_with_env_worktree, init_repository,
    init_repository_separate_git_dir, Repository,
};
use grit_lib::rev_list::ObjectFilter;
use std::collections::{HashSet, VecDeque};
use std::fs;
use std::io::{self, IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::thread;

use crate::commands::submodule::{
    set_submodule_core_worktree_after_separate_clone, submodule_separate_git_dir,
};
use crate::trace_run_command_git_invocation;
use grit_lib::submodule_gitdir::{
    ensure_submodule_gitdir_config, submodule_gitdir_outer_conflict, submodule_path_config_enabled,
    validate_submodule_path, write_submodule_gitfile,
};

fn expand_tilde_path(value: &str) -> String {
    if value == "~" {
        return std::env::var("HOME").unwrap_or_else(|_| value.to_owned());
    }
    if let Some(rest) = value.strip_prefix("~/") {
        if let Ok(home) = std::env::var("HOME") {
            return format!("{home}/{rest}");
        }
    }
    value.to_owned()
}

fn effective_template_dir(args: &Args) -> Option<PathBuf> {
    match &args.template {
        // `--template=` (explicit empty) means "use no template". Upstream Git's clone treats this
        // as an empty template directory: `hooks/`, `info/`, `description`, and `branches/` are NOT
        // created (they are template content), so a later `mkdir .git/info` in the harness succeeds
        // (t1090). Return an empty path so `init_repository*`'s `skip_hooks_info` guard fires; the
        // empty path is never `is_dir()`, so no template copy is attempted.
        Some(t) if t.is_empty() => Some(PathBuf::new()),
        Some(t) => Some(PathBuf::from(t)),
        None => {
            if let Ok(tdir) = std::env::var("GIT_TEMPLATE_DIR") {
                if !tdir.is_empty() {
                    return Some(PathBuf::from(tdir));
                }
                return None;
            }
            let config = ConfigSet::load(None, true).unwrap_or_default();
            config
                .get("init.templateDir")
                .filter(|v| !v.trim().is_empty())
                .map(|v| PathBuf::from(expand_tilde_path(&v)))
        }
    }
}

/// Arguments for `grit clone`.
#[derive(Debug, ClapArgs)]
#[command(about = "Clone a repository into a new directory")]
pub struct Args {
    /// Repository to clone (local path).
    pub repository: String,

    /// Target directory (defaults to the repository basename).
    pub directory: Option<String>,

    /// Use given name for the remote tracking branch namespace instead of `origin`.
    #[arg(short = 'o', long = "origin", value_name = "NAME")]
    pub origin: Option<String>,

    /// Create a bare clone.
    #[arg(long)]
    pub bare: bool,

    /// Create a shallow clone with limited history (sets up config only).
    #[arg(long, value_name = "N")]
    pub depth: Option<usize>,

    /// Shallow clone since the given date (accepted; local clones may ignore with a warning).
    #[arg(long = "shallow-since", value_name = "DATE")]
    pub shallow_since: Option<String>,

    /// Shallow clone excluding the given ref (accepted; local clones may ignore with a warning).
    #[arg(long = "shallow-exclude", value_name = "REF", action = clap::ArgAction::Append)]
    pub shallow_exclude: Vec<String>,

    /// Bundle URI (accepted for compatibility; incompatible with shallow clone options).
    #[arg(long = "bundle-uri", value_name = "URI")]
    pub bundle_uri: Option<String>,

    /// Abort clone if the source repository is shallow.
    #[arg(long = "reject-shallow", action = clap::ArgAction::SetTrue)]
    pub reject_shallow: bool,

    /// Allow cloning a shallow repository (overrides `clone.rejectShallow` and `--reject-shallow`).
    #[arg(long = "no-reject-shallow", action = clap::ArgAction::SetTrue)]
    pub no_reject_shallow: bool,

    /// Force progress reporting even when stderr is not a terminal.
    #[arg(long = "progress", action = clap::ArgAction::SetTrue)]
    pub progress: bool,

    /// Checkout a specific branch after cloning.
    #[arg(short = 'b', long = "branch", value_name = "NAME")]
    pub branch: Option<String>,

    /// Don't checkout HEAD after cloning.
    #[arg(short = 'n', long = "no-checkout")]
    pub no_checkout: bool,

    /// Be quiet — suppress progress messages.
    #[arg(short = 'q', long = "quiet")]
    pub quiet: bool,

    /// Set a configuration variable in the newly-created repository.
    #[arg(short = 'c', value_name = "KEY=VALUE", action = clap::ArgAction::Append)]
    pub config: Vec<String>,

    /// Check out a specific revision (detached HEAD).
    #[arg(long, value_name = "REV")]
    pub revision: Option<String>,

    /// Template directory for the clone.
    #[arg(long = "template")]
    pub template: Option<String>,

    /// Create a mirror clone.
    #[arg(long)]
    pub mirror: bool,

    /// Clone only the history leading to the tip of a single branch.
    #[arg(long)]
    pub single_branch: bool,
    /// Clone history for all branches, even with depth options.
    #[arg(long = "no-single-branch")]
    pub no_single_branch: bool,

    /// Don't clone any tags.
    #[arg(long)]
    pub no_tags: bool,

    /// Recurse into submodules after cloning. Optionally takes a pathspec selecting which
    /// submodules to initialize (repeatable); without a value, all submodules are selected.
    #[arg(
        long = "recurse-submodules",
        alias = "recursive",
        num_args = 0..=1,
        require_equals = true,
        default_missing_value = ".",
        action = clap::ArgAction::Append,
        value_name = "PATHSPEC"
    )]
    pub recurse_submodules_specs: Vec<String>,

    /// Do not recurse into submodules after cloning.
    #[arg(long = "no-recurse-submodules")]
    pub no_recurse_submodules: bool,

    /// Parallel jobs hint for submodule cloning (forwarded to `submodule update`).
    #[arg(short = 'j', long = "jobs", value_name = "N")]
    pub jobs: Option<usize>,

    /// Use remote-tracking branch for submodules.
    #[arg(long = "remote-submodules")]
    pub remote_submodules: bool,

    /// Use recorded superproject gitlinks for submodules.
    #[arg(long = "no-remote-submodules")]
    pub no_remote_submodules: bool,

    /// Use shallow submodule clones.
    #[arg(long = "shallow-submodules")]
    pub shallow_submodules: bool,

    /// Do not use shallow submodule clones (overrides `.gitmodules` shallow recommendation).
    #[arg(long = "no-shallow-submodules")]
    pub no_shallow_submodules: bool,

    /// Apply the partial clone filter to submodules when recursing.
    #[arg(long = "also-filter-submodules")]
    pub also_filter_submodules: bool,

    /// Do not apply the partial clone filter to submodules.
    #[arg(long = "no-also-filter-submodules")]
    pub no_also_filter_submodules: bool,

    /// Use a custom upload-pack command on the remote side.
    #[arg(short = 'u', long = "upload-pack", value_name = "UPLOAD_PACK")]
    pub upload_pack: Option<String>,
    /// Transmit the given string to the server when speaking protocol v2.
    #[arg(long = "server-option", action = clap::ArgAction::Append)]
    pub server_options: Vec<String>,

    /// Force local clone (default for local paths, accepted for compatibility).
    #[arg(short = 'l', long = "local")]
    pub local: bool,

    /// Do not use local optimizations (accepted for compatibility).
    #[arg(long = "no-local")]
    pub no_local: bool,

    /// Copy object files instead of hardlinking when populating the new ODB (local clone path).
    #[arg(long = "no-hardlinks", action = clap::ArgAction::SetTrue)]
    pub no_hardlinks: bool,

    /// Partial clone filter spec. Repeated filters are combined.
    #[arg(long = "filter", value_name = "FILTER-SPEC", action = clap::ArgAction::Append)]
    pub filter: Vec<String>,

    /// Initialize sparse-checkout in cone mode.
    #[arg(long = "sparse")]
    pub sparse: bool,

    /// Set up shared clone using alternates instead of copying objects.
    #[arg(short = 's', long = "shared")]
    pub shared: bool,

    /// Reference repository for alternates (can be repeated).
    #[arg(long = "reference", value_name = "REPO", action = clap::ArgAction::Append)]
    pub reference: Vec<String>,

    /// Like `--reference`, but skip repositories that cannot be used as alternates.
    #[arg(
        long = "reference-if-able",
        value_name = "REPO",
        action = clap::ArgAction::Append
    )]
    pub reference_if_able: Vec<String>,

    /// Repack into this repository and remove `info/alternates` after clone.
    #[arg(long)]
    pub dissociate: bool,

    /// Use IPv4 addresses only (SSH transport).
    #[arg(short = '4', action = clap::ArgAction::SetTrue)]
    pub ipv4: bool,

    /// Use IPv6 addresses only (SSH transport).
    #[arg(short = '6', action = clap::ArgAction::SetTrue)]
    pub ipv6: bool,

    /// Store the git directory at this path; work tree uses a gitfile `.git`.
    #[arg(long = "separate-git-dir", value_name = "GITDIR")]
    pub separate_git_dir: Option<PathBuf>,

    /// Ref storage backend for the new repository (`files` or `reftable`).
    #[arg(long = "ref-format", value_name = "FORMAT")]
    pub ref_format: Option<String>,
}

impl Args {
    /// Whether `--recurse-submodules` was given (with or without a pathspec value).
    pub fn recurse_submodules(&self) -> bool {
        !self.no_recurse_submodules && !self.recurse_submodules_specs.is_empty()
    }

    /// The submodule.active pathspecs to set when explicit `--recurse-submodules=<spec>` values
    /// were given. Returns `None` when no explicit pathspec was provided (default missing value
    /// `.` is treated as "all", matching git, but git still records `.` in submodule.active when
    /// a value was explicitly supplied). We only treat values as explicit when at least one is
    /// not the bare default-missing sentinel produced by a value-less flag.
    pub fn recurse_submodules_pathspecs(&self) -> Option<&[String]> {
        // A value-less `--recurse-submodules` yields a single "." default-missing value; an
        // explicit `--recurse-submodules=<spec>` yields the user's spec(s). We cannot perfectly
        // distinguish a lone explicit "." from the default, but git treats both as "activate all"
        // when there is a single "." and as a pathspec list otherwise.
        if self.recurse_submodules_specs.len() == 1 && self.recurse_submodules_specs[0] == "." {
            None
        } else if self.recurse_submodules_specs.is_empty() {
            None
        } else {
            Some(&self.recurse_submodules_specs)
        }
    }
}

/// Returns `true` when `name` is valid as a remote name (same idea as Git's `valid_remote_name`).
fn valid_remote_name(name: &str) -> bool {
    let probe = format!("refs/remotes/{name}/test");
    check_refname_format(&probe, &RefNameOptions::default()).is_ok()
}

fn default_branch_from_config() -> Option<String> {
    if let Ok(env) = std::env::var("GIT_TEST_DEFAULT_INITIAL_BRANCH_NAME") {
        if !env.is_empty() {
            return Some(env);
        }
    }
    let set = ConfigSet::load(None, true).unwrap_or_default();
    set.get("init.defaultBranch")
        .filter(|s| s.as_str() != "none")
}

fn default_head_branch_fallback() -> String {
    default_branch_from_config().unwrap_or_else(|| "master".to_owned())
}

fn resolved_clone_ref_storage(args: &Args) -> Result<&'static str> {
    match args.ref_format.as_deref() {
        None | Some("files") => Ok("files"),
        Some("reftable") => Ok("reftable"),
        Some(other) => bail!("fatal: unknown ref storage format '{other}'"),
    }
}

fn clone_write_direct_ref(git_dir: &Path, refname: &str, oid_hex: &str) -> Result<()> {
    let oid =
        ObjectId::from_hex(oid_hex.trim()).with_context(|| format!("invalid OID for {refname}"))?;
    if grit_lib::reftable::is_reftable_repo(git_dir) {
        reftable::reftable_write_ref(git_dir, refname, &oid, None, None)
            .map_err(|e| anyhow::anyhow!("{e}"))
    } else {
        let path = git_dir.join(refname);
        if let Some(p) = path.parent() {
            fs::create_dir_all(p)?;
        }
        fs::write(path, format!("{}\n", oid.to_hex()))
            .with_context(|| format!("writing ref {refname}"))
    }
}

fn clone_write_symref(git_dir: &Path, refname: &str, target: &str) -> Result<()> {
    let target = target.trim();
    if grit_lib::reftable::is_reftable_repo(git_dir) {
        reftable::reftable_write_symref(git_dir, refname, target, None, None)
            .map_err(|e| anyhow::anyhow!("{e}"))
    } else {
        let path = git_dir.join(refname);
        if let Some(p) = path.parent() {
            fs::create_dir_all(p)?;
        }
        fs::write(path, format!("ref: {target}\n"))
            .with_context(|| format!("writing symref {refname}"))
    }
}

fn clone_ref_file_exists(git_dir: &Path, refname: &str) -> bool {
    matches!(
        refs::read_raw_ref(git_dir, refname),
        Ok(refs::RawRefLookup::Exists)
    )
}

fn clone_read_direct_ref_oid(git_dir: &Path, refname: &str) -> Result<String> {
    if grit_lib::reftable::is_reftable_repo(git_dir) {
        let oid = grit_lib::refs::resolve_ref(git_dir, refname)
            .with_context(|| format!("reading ref {refname}"))?;
        Ok(oid.to_hex())
    } else {
        let path = git_dir.join(refname);
        let s = fs::read_to_string(&path).with_context(|| format!("reading ref file {refname}"))?;
        Ok(s.trim().to_string())
    }
}

fn strip_refs_for_revision_clone(git_dir: &Path) -> Result<()> {
    if grit_lib::reftable::is_reftable_repo(git_dir) {
        let refs = grit_lib::refs::list_refs(git_dir, "refs/")?;
        for (name, _) in refs {
            reftable::reftable_delete_ref(git_dir, &name).map_err(|e| anyhow::anyhow!("{e}"))?;
        }
        Ok(())
    } else {
        strip_refs_under(&git_dir.join("refs"))
    }
}

fn clone_default_remote_from_c_flags(config: &[String]) -> Option<String> {
    for entry in config {
        if let Some((k, v)) = entry.split_once('=') {
            if k.trim().eq_ignore_ascii_case("clone.defaultRemoteName") {
                return Some(v.trim().to_owned());
            }
        }
    }
    None
}

fn resolve_remote_name(args: &Args) -> Result<String> {
    if let Some(ref o) = args.origin {
        if !valid_remote_name(o) {
            bail!("'{o}' is not a valid remote name");
        }
        return Ok(o.clone());
    }
    if let Some(from_c) = clone_default_remote_from_c_flags(&args.config) {
        return Ok(from_c);
    }
    let set = ConfigSet::load(None, true).unwrap_or_default();
    if let Some(n) = set.get("clone.defaultRemoteName") {
        return Ok(n);
    }
    Ok("origin".to_owned())
}

fn effective_reject_shallow(args: &Args) -> bool {
    if args.no_reject_shallow {
        return false;
    }
    if args.reject_shallow {
        return true;
    }
    let set = ConfigSet::load(None, true).unwrap_or_default();
    matches!(set.get_bool("clone.rejectshallow"), Some(Ok(true)))
}

fn source_repo_is_shallow(git_dir: &Path) -> bool {
    git_dir.join("shallow").is_file()
}

/// When cloning without `--filter`, copy partial-clone metadata from a promisor source (t0411).
fn inherited_partial_clone_filter_spec(source_git_dir: &Path) -> Option<String> {
    let cfg = ConfigSet::load(Some(source_git_dir), true).unwrap_or_default();
    if !repo_treats_promisor_packs(source_git_dir, &cfg) {
        return None;
    }
    for e in cfg.entries() {
        if !e.key.ends_with(".partialclonefilter") {
            continue;
        }
        let v = e.value.as_deref()?.trim();
        if !v.is_empty() {
            return Some(v.to_string());
        }
    }
    None
}

fn maybe_warn_shallow_options_ignored(repo_path_str: &str, args: &Args) {
    if args.no_local {
        return;
    }
    if !repo_path_str.starts_with("file://") {
        if args.shallow_since.is_some() {
            eprintln!("warning: --shallow-since is ignored in local clones; use file:// instead.");
        }
        if !args.shallow_exclude.is_empty() {
            eprintln!(
                "warning: --shallow-exclude is ignored in local clones; use file:// instead."
            );
        }
    }
}

fn maybe_print_local_clone_progress(show: bool) {
    if !show {
        return;
    }
    let _ = writeln!(io::stderr(), "Receiving objects: 100% (1/1), done.");
    let _ = writeln!(io::stderr(), "Checking connectivity: 1, done.");
}

/// True when `HEAD` is a symref whose target ref file is missing (unborn / broken remote HEAD).
fn head_points_to_missing_ref(repo: &Repository) -> bool {
    let Ok(head_content) = fs::read_to_string(repo.git_dir.join("HEAD")) else {
        return false;
    };
    let head = head_content.trim();
    let Some(refname) = head.strip_prefix("ref: ") else {
        return false;
    };
    let refname = refname.trim();
    !clone_ref_file_exists(&repo.git_dir, refname)
}

/// Clone checked out the remote's unborn default branch (symref exists, branch ref missing on source).
fn is_unborn_remote_default_checkout(
    dest: &Repository,
    source_symref: Option<&str>,
    head_branch: Option<&str>,
    source_git_dir: &Path,
) -> bool {
    let has_other_heads = refs::list_refs(source_git_dir, "refs/heads/")
        .ok()
        .is_some_and(|entries| !entries.is_empty());
    if has_other_heads {
        return false;
    }
    let Some(branch) = head_branch else {
        return false;
    };
    let Some(sr) = source_symref else {
        return false;
    };
    let Some(h) = sr.strip_prefix("refs/heads/") else {
        return false;
    };
    h == branch && !clone_ref_file_exists(source_git_dir, sr) && head_points_to_missing_ref(dest)
}

/// Perform checkout of `HEAD` when it points at a missing branch (unborn), matching Git clone.
fn checkout_head_allow_unborn(repo: &Repository) -> Result<()> {
    if repo.work_tree.is_none() {
        return Ok(());
    }

    let head_content = fs::read_to_string(repo.git_dir.join("HEAD")).context("reading HEAD")?;
    let head = head_content.trim();

    let oid = if let Some(refname) = head.strip_prefix("ref: ") {
        let refname = refname.trim();
        if !clone_ref_file_exists(&repo.git_dir, refname) {
            return Ok(());
        }
        let oid_str = clone_read_direct_ref_oid(&repo.git_dir, refname)?;
        ObjectId::from_hex(oid_str.trim()).with_context(|| format!("invalid OID in {refname}"))?
    } else if head.len() == 40 && head.chars().all(|c| c.is_ascii_hexdigit()) {
        ObjectId::from_hex(head).context("invalid OID in HEAD")?
    } else {
        return Ok(());
    };

    let obj = repo.odb.read(&oid).context("reading HEAD commit")?;
    let commit = parse_commit(&obj.data).context("parsing HEAD commit")?;
    write_index_from_tree(repo, &commit.tree)?;
    checkout_index_into_clone_worktree(repo)?;
    Ok(())
}

/// Materialize the freshly written index into the clone's working tree.
///
/// Runs with the process CWD temporarily set to the new work tree, because long-running process
/// filters (`filter.<name>.process`) and their relative arguments (e.g. `--log=a.log`) are resolved
/// relative to the work-tree top-level, exactly like Git (which `chdir`s into the repo for
/// checkout). The original CWD is restored afterwards so later clone steps are unaffected.
fn checkout_index_into_clone_worktree(repo: &Repository) -> Result<()> {
    let Some(work_tree) = repo.work_tree.clone() else {
        return Ok(());
    };
    let prev_cwd = std::env::current_dir().ok();
    std::env::set_current_dir(&work_tree)
        .with_context(|| format!("entering work tree {}", work_tree.display()))?;
    let result = crate::commands::checkout::checkout_head_to_worktree(repo)
        .context("checking out work tree from index");
    if let Some(prev) = prev_cwd {
        let _ = std::env::set_current_dir(prev);
    }
    result
}

pub fn run(mut args: Args) -> Result<()> {
    // `git clone` always sets `TRANS_OPT_KEEP` (builtin/clone.c), so fetch-pack keeps the
    // received pack as a real `.pack` file instead of unpacking it into loose objects — the
    // `transfer.unpackLimit` heuristic only applies to plain `git fetch`. Mirror this so a
    // `--no-local` clone leaves objects under `objects/pack/` (`t5710` moves `pack-*` around).
    std::env::set_var("GRIT_FETCH_KEEP_PACK", "1");

    // `git clone --mirror` is a bare clone into `<name>.git` with a full ref mirror;
    // the object store must live at `<repo>/objects/...`, not `<repo>/.git/objects/...`.
    if args.mirror {
        args.bare = true;
    }

    if args.ipv4 && args.ipv6 {
        bail!("options '-4' and '-6' cannot be used together");
    }
    if !args.server_options.is_empty() && protocol_wire::effective_client_protocol_version() < 2 {
        bail!(
            "server options require protocol version 2 or later\nsee protocol.version in 'git help config'"
        );
    }
    if args.single_branch && args.no_single_branch {
        bail!("options '--single-branch' and '--no-single-branch' cannot be used together");
    }
    if args.no_single_branch {
        args.single_branch = false;
    }
    let deepen =
        args.depth.is_some() || args.shallow_since.is_some() || !args.shallow_exclude.is_empty();
    if args.bundle_uri.is_some() && deepen {
        bail!(
            "options '--bundle-uri' and '--depth/--shallow-since/--shallow-exclude' cannot be used together"
        );
    }

    // Test harness (`tests/test-lib.sh`) sets `GIT_QUIET=-q` unless `TEST_VERBOSE` is set,
    // mirroring Git's quiet default for commands invoked from the suite.
    if !args.quiet && std::env::var_os("GIT_QUIET").as_deref() == Some(std::ffi::OsStr::new("-q")) {
        args.quiet = true;
    }

    // --revision conflicts with --branch and --mirror
    if args.revision.is_some() && args.branch.is_some() {
        bail!("--revision and --branch are mutually exclusive");
    }
    if args.revision.is_some() && args.mirror {
        bail!("--revision and --mirror are mutually exclusive");
    }
    if !args.reference.is_empty() && !args.reference_if_able.is_empty() && args.recurse_submodules()
    {
        bail!("clone --recursive is not compatible with both --reference and --reference-if-able");
    }
    if args.separate_git_dir.is_some() && args.bare {
        bail!("options '--bare' and '--separate-git-dir' cannot be used together");
    }
    if args.separate_git_dir.is_some() && args.mirror {
        bail!("--separate-git-dir and --mirror are incompatible");
    }
    if args.separate_git_dir.is_some() {
        let repo = args.repository.as_str();
        if repo.starts_with("git://") || repo.starts_with("http://") || repo.starts_with("https://")
        {
            bail!("--separate-git-dir is only supported for local repository clones");
        }
    }

    // `ext::` — external command bridging smart transport (git-remote-ext).
    if args.repository.starts_with("ext::") {
        crate::protocol::check_protocol_allowed("ext", None)?;
        return run_ext_clone(args);
    }

    // Directory or file literally named `foo:bar` must clone as a local path, not scp-style SSH
    // (`t5601-clone`); `Path::new("myhost:src")` does not exist, so real SSH URLs still pass through.
    let repo_probe = Path::new(args.repository.trim());
    if !args.repository.contains("://") && repo_probe.exists() {
        // Fall through to local clone below.
    } else if crate::ssh_transport::is_configured_ssh_url(&args.repository) {
        crate::protocol::check_protocol_allowed("ssh", None)?;
        return run_ssh_clone(args);
    }

    // Detect git:// protocol (native transport in fetch transport layer)
    if args.repository.starts_with("git://") {
        crate::protocol::check_protocol_allowed("git", None)?;
        return run_git_clone(args);
    }

    // Detect http(s):// protocol
    if args.repository.starts_with("http://") || args.repository.starts_with("https://") {
        let proto = if args.repository.starts_with("https://") {
            "https"
        } else {
            "http"
        };
        crate::protocol::check_protocol_allowed(proto, None)?;
        return run_http_clone(args);
    }

    // Detect bundle file
    if is_bundle_file(&args.repository) {
        if args.separate_git_dir.is_some() {
            bail!("--separate-git-dir is not supported when cloning from a bundle");
        }
        return run_bundle_clone(args);
    }

    // Check protocol.file.allow before local clone
    crate::protocol::check_protocol_allowed("file", None)?;

    let is_file_url = args.repository.starts_with("file://");
    let repo_path_str = if is_file_url {
        let stripped = args
            .repository
            .strip_prefix("file://")
            .unwrap_or(args.repository.as_str());
        percent_decode_file_url_path(stripped)?
    } else {
        args.repository.clone()
    };
    let path_only = repo_path_str.split('?').next().unwrap_or("").to_string();
    let source_path = PathBuf::from(&path_only);

    // Open the source repository, trying common Git local path variants if direct path fails.
    let (source, source_path) = match open_source_repo(&source_path) {
        Ok(s) => (s, source_path),
        Err(_) => {
            let with_git = PathBuf::from(format!("{}.git", source_path.display()));
            match open_source_repo(&with_git) {
                Ok(s) => (s, with_git),
                Err(_) => {
                    return Err(anyhow::anyhow!(
                        "'{}' does not appear to be a git repository",
                        args.repository
                    ));
                }
            }
        }
    };
    source.verify_safe_for_clone_source()?;

    // Git refuses `--upload-pack` on the local fast path (no remote helper to run).
    // `--no-local` still spawns upload-pack, so the custom command must be honored (`t5605`).
    if !args.no_local
        && args
            .upload_pack
            .as_deref()
            .is_some_and(|s| !s.trim().is_empty())
        && !args.repository.starts_with("file://")
        && !crate::ssh_transport::is_configured_ssh_url(&args.repository)
    {
        bail!("could not read from remote repository");
    }

    if effective_reject_shallow(&args) && source_repo_is_shallow(&source.git_dir) {
        bail!("source repository is shallow, reject to clone.");
    }
    if source_repo_is_shallow(&source.git_dir) && !args.no_local && !is_file_url {
        eprintln!("warning: source repository is shallow, ignoring --local");
    }
    maybe_warn_shallow_options_ignored(&repo_path_str, &args);

    let filter_spec = clone_filter_spec(&args);

    // A `file://` partial clone whose filter cannot be reproduced by the client-side
    // `copy_objects` + materialize layout (e.g. `tree:<n>`, `sparse:oid=…`, or a combine spec
    // containing them) must negotiate via upload-pack so `pack-objects --filter` applies the
    // reachability-aware filter server-side. Match upstream Git, which disables the local-clone
    // optimization for partial clones.
    if is_file_url
        && !args.no_local
        && uploadpack_filter_allowed(&source.git_dir)
        && filter_spec.is_some()
        && !clone_filter_materializable_locally(filter_spec.as_deref())
    {
        args.no_local = true;
    }

    if is_file_url && filter_spec.is_some() && !uploadpack_filter_allowed(&source.git_dir) {
        eprintln!(
            "warning: filtering not recognized by server, ignoring --filter={}",
            filter_spec.as_deref().unwrap_or("")
        );
    }
    if is_file_url && uploadpack_filter_allowed(&source.git_dir) {
        if let Some(spec) = filter_spec.as_deref().filter(|s| !s.trim().is_empty()) {
            let config = ConfigSet::load(Some(&source.git_dir), false).unwrap_or_default();
            grit_lib::upload_filter::validate_upload_filter_config(&config)?;
            grit_lib::upload_filter::validate_upload_filter_request(&config, spec)?;
        }
    }

    let partial_blob_limit_zero = matches!(
        filter_spec.as_deref(),
        Some("blob:limit=0") | Some("blob:size=0")
    );
    // The source's own promisor filter is only inherited when the client did NOT pass an explicit
    // `--filter`; an explicit filter (e.g. `--filter=blob:limit=5k`) takes precedence and must not
    // be silently downgraded to the source's `blob:none` (`t5710`: a `blob:limit` clone of a server
    // whose LOP remote is recorded as `blob:none` must still keep small blobs).
    let inherited_blob_none = args.filter.is_empty()
        && inherited_partial_clone_filter_spec(&source.git_dir).as_deref() == Some("blob:none");
    let partial_blob_none = matches!(filter_spec.as_deref(), Some("blob:none"))
        || (partial_blob_limit_zero && uploadpack_filter_allowed(&source.git_dir))
        || inherited_blob_none;
    let pack_filter_active = clone_pack_filter_active(&args, Some(&source.git_dir));

    let remote_name = resolve_remote_name(&args)?;
    let server_options = effective_clone_server_options(&args, &remote_name)?;

    // `repo_path_str` strips `file://`; use the original URL for transport detection.
    let mut file_v2_preflight_head: Option<(Option<String>, Option<String>)> = None;
    if args.repository.starts_with("file://")
        && crate::file_upload_pack_v2::client_wants_protocol_v2()
    {
        let upload_cmd = args.upload_pack.as_deref().filter(|s| !s.trim().is_empty());
        let bundle_cli = args.bundle_uri.is_some();
        let request_bundle = crate::file_upload_pack_v2::transfer_bundle_uri_enabled();
        let reference_object_dirs = resolve_reference_object_dirs(&args);
        let preflight_head = crate::fetch_transport::with_packet_trace_identity("clone", || {
            crate::file_upload_pack_v2::clone_preflight_file_v2_if_needed(
                &source.git_dir,
                upload_cmd,
                request_bundle,
                bundle_cli,
                &server_options,
                &reference_object_dirs,
            )
        })
        .context("file:// protocol v2 clone preflight")?;
        file_v2_preflight_head = Some(preflight_head);
    }

    // `--branch <name>` may name a branch (refs/heads/<name>) or a tag (refs/tags/<name>):
    // upstream's `find_remote_branch` tries heads first, then tags. A tag target makes the clone
    // check out a detached HEAD at the tag and (under `--single-branch`) fetch only that tag.
    let mut branch_is_tag = false;
    if let Some(branch) = args.branch.as_deref() {
        let remote_branch = format!("refs/heads/{branch}");
        let remote_tag = format!("refs/tags/{branch}");
        if clone_ref_file_exists(&source.git_dir, &remote_branch) {
            // ordinary branch
        } else if clone_ref_file_exists(&source.git_dir, &remote_tag) {
            branch_is_tag = true;
        } else {
            bail!("Remote branch {branch} not found in upstream {remote_name}");
        }
    }
    let ref_storage =
        if args.ref_format.is_none() && grit_lib::reftable::is_reftable_repo(&source.git_dir) {
            "reftable"
        } else {
            resolved_clone_ref_storage(&args)?
        };

    // Determine target directory
    let target_name = if let Some(ref d) = args.directory {
        d.trim_end_matches('/').to_string()
    } else {
        let base = source_path
            .file_name()
            .unwrap_or_default()
            .to_string_lossy()
            .to_string();
        let base = base
            .strip_suffix(".git")
            .unwrap_or(&base)
            .trim_end_matches('/')
            .to_string();
        if args.bare && !args.mirror {
            format!("{base}.git")
        } else {
            base
        }
    };

    let target_path = PathBuf::from(&target_name);
    let empty_dir_ok = path_is_empty_directory(&target_path);
    if target_path.exists() && !empty_dir_ok {
        bail!(
            "destination path '{}' already exists and is not an empty directory",
            target_path.display()
        );
    }

    // Print "Cloning into..." BEFORE doing the work (matches git behavior)
    if !args.quiet {
        if args.bare {
            eprintln!("Cloning into bare repository '{}'...", target_name);
        } else {
            eprintln!("Cloning into '{}'...", target_name);
        }
    }

    let remote_url_for_config = if crate::ssh_transport::is_configured_ssh_url(&args.repository) {
        args.repository.clone()
    } else if is_file_url
        || args.repository.starts_with("http://")
        || args.repository.starts_with("https://")
    {
        if let Ok(abs) = source_path.canonicalize() {
            format!("file://{}", abs.display())
        } else {
            args.repository.clone()
        }
    } else {
        // Git stores the remote URL as the *absolute* form of the literal path the user gave
        // (builtin/clone.c `get_repo_path` -> `absolute_pathdup`): it prepends the cwd when the
        // path is relative but does NOT normalize `.`/`..`/`./` away. So `git clone .` yields
        // `<cwd>/.` and `git clone $pwd/submodule` yields `$pwd/submodule` verbatim. We mirror
        // this so a later `git fetch`'s `From <url>` header matches Git (e.g. t5526 expects the
        // superproject's `From <pwd>/.` but a submodule's `From <pwd>/submodule` with no `/.`).
        absolute_clone_source_url(&source_path)
    };

    let use_upload_for_protocol_v1 = protocol_wire::effective_client_protocol_version() == 1
        && (is_file_url || crate::ssh_transport::is_configured_ssh_url(&args.repository));

    // Source HEAD symref / detached OID: from disk unless we will use upload-pack (advertisement).
    let (mut source_head_symref, mut source_head_oid) = read_source_head_info(&source.git_dir);
    if let Some((symref, oid)) = file_v2_preflight_head {
        source_head_symref = symref;
        source_head_oid = oid;
    }

    // Branch to checkout: explicit -b/--branch, else guess from remote HEAD (Git semantics).
    // When `--branch` names a tag, there is no local branch to create — HEAD is detached at the
    // tag's target object.
    let head_branch = if branch_is_tag {
        if let Some(tag) = args.branch.as_deref() {
            if let Some(oid) = ref_oid_hex_in_repo(&source.git_dir, &format!("refs/tags/{tag}")) {
                source_head_symref = None;
                source_head_oid = Some(oid);
            }
        }
        None
    } else if args.branch.is_some() {
        determine_head_branch(&source.git_dir, args.branch.as_deref())?
    } else if use_upload_for_protocol_v1 {
        None
    } else {
        guess_checkout_branch(
            &source.git_dir,
            source_head_symref.as_deref(),
            source_head_oid.as_deref(),
        )?
    };
    let initial_fallback = default_head_branch_fallback();
    let initial_branch = head_branch.as_deref().unwrap_or(initial_fallback.as_str());

    // Initialize the target repository
    if !empty_dir_ok {
        fs::create_dir_all(&target_path)
            .with_context(|| format!("cannot create directory '{}'", target_path.display()))?;
    }

    let template_dir = effective_template_dir(&args);

    let git_work_tree_env = std::env::var_os("GIT_WORK_TREE").filter(|v| !v.is_empty());

    // Whether the `--separate-git-dir` target already existed (as an empty directory) before the
    // clone. Mirrors upstream Git's `real_dest_exists`: if it pre-existed, a failed clone only
    // clears its contents instead of removing the directory the user created.
    let real_dest_exists = args
        .separate_git_dir
        .as_ref()
        .is_some_and(|sep_git| sep_git.exists());

    let dest = if let Some(ref sep_git) = args.separate_git_dir {
        if sep_git.exists() && sep_git.read_dir()?.next().is_some() {
            bail!(
                "destination path '{}' already exists and is not an empty directory",
                sep_git.display()
            );
        }
        init_repository_separate_git_dir(
            &target_path,
            sep_git,
            initial_branch,
            template_dir.as_deref(),
            ref_storage,
        )
        .with_context(|| {
            format!(
                "failed to initialize separate git dir '{}'",
                sep_git.display()
            )
        })?
    } else if args.bare && args.template.as_ref().is_some_and(|s| s.is_empty()) {
        init_bare_clone_minimal(&target_path, initial_branch, ref_storage).with_context(|| {
            format!(
                "failed to initialize bare clone '{}'",
                target_path.display()
            )
        })?;
        Repository::open(&target_path, None)
            .with_context(|| format!("failed to open repository '{}'", target_path.display()))?
    } else if git_work_tree_env.is_some() {
        let wt_raw = git_work_tree_env
            .as_ref()
            .map(|v| v.to_string_lossy().to_string())
            .unwrap_or_default();
        let wt_path = if Path::new(&wt_raw).is_absolute() {
            PathBuf::from(&wt_raw)
        } else {
            std::env::current_dir()?.join(&wt_raw)
        };
        init_bare_with_env_worktree(
            &target_path,
            &wt_path,
            initial_branch,
            template_dir.as_deref(),
            ref_storage,
        )
        .with_context(|| format!("failed to initialize '{}'", target_path.display()))?
    } else {
        init_repository(
            &target_path,
            args.bare,
            initial_branch,
            template_dir.as_deref(),
            ref_storage,
        )
        .with_context(|| format!("failed to initialize '{}'", target_path.display()))?
    };

    // Arm the cleanup guard now that the destination directories exist. On any later failure
    // (object fetch, checkout, submodule clone, …) Drop removes what we wrote, matching upstream
    // Git's `atexit(remove_junk)`. The git dir is the separate git dir, the bare target, or the
    // work tree's `.git`; the work tree (when not bare) is the target directory. Pre-existing empty
    // directories keep their top level (only their contents are cleaned).
    let mut junk_guard = {
        let work_tree = if args.bare {
            None
        } else {
            dest.work_tree.clone().or_else(|| Some(target_path.clone()))
        };
        let git_dir = Some(dest.git_dir.clone());
        // The separate git dir keeps its top level when it pre-existed; otherwise the rule follows
        // whether the destination/work-tree directory pre-existed (empty_dir_ok).
        let git_dir_keep_toplevel = if args.separate_git_dir.is_some() {
            real_dest_exists
        } else {
            empty_dir_ok
        };
        JunkGuard {
            work_tree,
            work_tree_keep_toplevel: empty_dir_ok,
            git_dir,
            git_dir_keep_toplevel,
            mode: JunkMode::LeaveNone,
        }
    };

    let mut head_branch = head_branch;
    let upload_pack_shallow_options = crate::fetch_transport::UploadPackShallowOptions {
        depth: args.depth,
        deepen: None,
        shallow_since: args.shallow_since.clone(),
        shallow_exclude: args.shallow_exclude.clone(),
        unshallow: false,
    };
    let pack_filter_spec = filter_spec.as_deref().filter(|s| !s.trim().is_empty());

    // Apply clone `-c key=value` overrides to the new repo's config *before* fetching. Git writes
    // these into the destination config early so the fetch sees them; in particular the
    // `promisor-remote` protocol capability needs `promisor.acceptFromServer` and the
    // `remote.<lop>.{promisor,url,...}` entries already present when negotiating (`t5710`).
    let mut clone_config_applied = false;
    if !args.config.is_empty() {
        apply_clone_config(&dest.git_dir, &args.config).context("applying -c config")?;
        clone_config_applied = true;
    }

    if let Some(ref bu) = args.bundle_uri {
        crate::bundle_uri::apply_bundle_uri(&dest.git_dir, bu, &target_name, true)?;
    }
    maybe_trace_index_pack_fsck_for_filtered_clone(pack_filter_active);

    // Copy or share objects from source to destination
    if use_upload_for_protocol_v1 {
        let upload_cmd = args.upload_pack.as_deref().filter(|s| !s.trim().is_empty());
        let fetch_res = crate::fetch_transport::with_packet_trace_identity("clone", || {
            crate::fetch_transport::fetch_via_upload_pack_skipping(
                &dest.git_dir,
                &source.git_dir,
                upload_cmd,
                |adv| crate::fetch_transport::collect_wants(adv, &[]),
                false,
                true,
                pack_filter_active,
                true,
                None,
                Some(&upload_pack_shallow_options),
                pack_filter_spec,
                &[],
                &server_options,
            )
        });
        match fetch_res {
            Ok((remote_heads, remote_tags, adv_sym, head_oid)) => {
                propagate_extensions_object_format(&source.git_dir, &dest.git_dir)?;
                source_head_symref = adv_sym;
                source_head_oid = head_oid.map(|o| o.to_hex());
                head_branch = if args.branch.is_some() {
                    determine_head_branch(&source.git_dir, args.branch.as_deref())?
                } else {
                    guess_checkout_branch(
                        &dest.git_dir,
                        source_head_symref.as_deref(),
                        source_head_oid.as_deref(),
                    )?
                };

                if args.bare {
                    if args.mirror {
                        copy_refs_mirror_all(&source.git_dir, &dest.git_dir)
                            .context("copying mirror refs")?;
                        setup_remote_mirror_fetch_and_url(
                            &dest.git_dir,
                            remote_url_for_config.as_str(),
                            &remote_name,
                        )
                        .context("setting up mirror remote")?;
                    } else {
                        for (refname, oid) in &remote_heads {
                            let dst_ref = dest.git_dir.join(refname);
                            if let Some(parent) = dst_ref.parent() {
                                fs::create_dir_all(parent)?;
                            }
                            fs::write(&dst_ref, format!("{}\n", oid.to_hex()))?;
                        }
                        if !args.no_tags {
                            for (refname, oid) in &remote_tags {
                                let dst_ref = dest.git_dir.join(refname);
                                if let Some(parent) = dst_ref.parent() {
                                    fs::create_dir_all(parent)?;
                                }
                                fs::write(&dst_ref, format!("{}\n", oid.to_hex()))?;
                            }
                        }
                        setup_origin_remote_bare_url(
                            &dest.git_dir,
                            remote_url_for_config.as_str(),
                            &remote_name,
                        )
                        .context("setting up origin remote")?;
                        if let Some(ref branch) = head_branch {
                            fs::write(
                                dest.git_dir.join("HEAD"),
                                format!("ref: refs/heads/{branch}\n"),
                            )?;
                        } else if let Some(ref oid) = source_head_oid {
                            fs::write(dest.git_dir.join("HEAD"), format!("{oid}\n"))?;
                        }
                    }
                } else {
                    copy_refs_from_upload_pack_lists(
                        &dest.git_dir,
                        &remote_name,
                        &remote_heads,
                        &remote_tags,
                        args.no_tags,
                    )?;
                    let refspec = if args.single_branch {
                        let branch = head_branch.as_deref().unwrap_or(initial_fallback.as_str());
                        format!("+refs/heads/{branch}:refs/remotes/{remote_name}/{branch}")
                    } else {
                        format!("+refs/heads/*:refs/remotes/{remote_name}/*")
                    };
                    setup_origin_remote_url(
                        &dest.git_dir,
                        remote_url_for_config.as_str(),
                        &remote_name,
                        &refspec,
                    )
                    .context("setting up origin remote")?;

                    setup_remote_tracking_head(
                        &dest.git_dir,
                        &remote_name,
                        &source.git_dir,
                        source_head_symref.as_deref(),
                        source_head_oid.as_deref(),
                    )?;
                    setup_remote_tracking_head_from_advertisement(
                        &dest.git_dir,
                        &remote_name,
                        source_head_symref.as_deref(),
                    )?;

                    let preferred_branch =
                        head_branch.as_deref().unwrap_or(initial_fallback.as_str());
                    if let Some(branch) = resolve_remote_tracked_branch_name(
                        &dest.git_dir,
                        &remote_name,
                        preferred_branch,
                    ) {
                        let remote_ref = dest
                            .git_dir
                            .join("refs/remotes")
                            .join(&remote_name)
                            .join(&branch);
                        let oid_str =
                            fs::read_to_string(&remote_ref).context("reading remote ref")?;
                        let oid = oid_str.trim().to_string();

                        let local_ref_path = dest.git_dir.join("refs/heads").join(&branch);
                        if let Some(parent) = local_ref_path.parent() {
                            fs::create_dir_all(parent)?;
                        }
                        fs::write(&local_ref_path, format!("{oid}\n"))?;

                        fs::write(
                            dest.git_dir.join("HEAD"),
                            format!("ref: refs/heads/{branch}\n"),
                        )?;

                        setup_branch_tracking(&dest.git_dir, &branch, &remote_name)
                            .context("setting up branch tracking")?;
                    } else if let Some(ref branch) = head_branch {
                        if let Some(sr) = source_head_symref.as_deref() {
                            if sr == format!("refs/heads/{branch}")
                                && !source.git_dir.join(sr).exists()
                            {
                                setup_branch_tracking(&dest.git_dir, branch, &remote_name)
                                    .context("setting up branch tracking")?;
                            }
                        }
                    }
                }
            }
            Err(e) => {
                // `junk_guard` (armed above) removes the partially created destination on return,
                // honoring keep-toplevel for pre-existing empty directories.
                return Err(e).context("clone via upload-pack (protocol.version=1) failed");
            }
        }
    } else if !args.shared
        && (args.no_local || source_repo_is_shallow(&source.git_dir))
        && !use_upload_for_protocol_v1
    {
        // `--no-local` always negotiates via upload-pack. A shallow source (including an empty
        // `.git/shallow` file) disables Git's local clone optimization; use upload-pack instead of
        // copying objects / alternates (`t0411-clone-from-partial`, `t5605`).
        //
        // Set up `--reference` alternates *before* the fetch so the negotiation can borrow the
        // reference's objects (Git records the alternate, then fetch-pack's `everything_local`
        // skips wanting objects the reference already has). Without this an `--no-local
        // --reference=P` clone would fetch the reference's objects too, so removing P later would
        // not corrupt the clone (`t5604` "clone and dissociate from reference").
        append_reference_alternates(
            &dest.git_dir.join("objects"),
            &args.reference,
            &args.reference_if_able,
        )
        .context("adding --reference alternates")?;
        let upload_cmd = args.upload_pack.as_deref().filter(|s| !s.trim().is_empty());
        match crate::fetch_transport::with_packet_trace_identity("clone", || {
            crate::fetch_transport::fetch_via_upload_pack_skipping(
                &dest.git_dir,
                &source.git_dir,
                upload_cmd,
                |adv| crate::fetch_transport::collect_wants(adv, &[]),
                false,
                true,
                pack_filter_active,
                true,
                None,
                Some(&upload_pack_shallow_options),
                pack_filter_spec,
                &[],
                &server_options,
            )
        }) {
            Ok(_) => {
                propagate_extensions_object_format(&source.git_dir, &dest.git_dir)?;
            }
            Err(e) => {
                // `junk_guard` (armed above) removes the partially created destination on return,
                // honoring keep-toplevel for pre-existing empty directories.
                return Err(e).context("clone via upload-pack (--no-local) failed");
            }
        }
    } else if args.shared {
        write_shared_alternates(
            &source.git_dir,
            &dest.git_dir,
            &args.reference,
            &args.reference_if_able,
        )
        .context("setting up alternates")?;
        propagate_extensions_object_format(&source.git_dir, &dest.git_dir)?;
    } else {
        let has_reference = !args.reference.is_empty() || !args.reference_if_able.is_empty();
        let try_hardlink_objects = !args.no_hardlinks && !args.repository.starts_with("file://");
        // A genuine local-path clone (`git clone --reference G F H`, F a plain path) still copies
        // the source's objects via `copy_or_link_directory`; `--reference` only adds an *extra*
        // alternate. Only a `file://` clone borrows-without-copying when a reference is given
        // (it fetches via upload-pack and the reference satisfies the wants — `t5604` test 8 vs 19).
        let copy_source_objects = !has_reference || !is_file_url;
        if copy_source_objects {
            copy_objects(&source.git_dir, &dest.git_dir, try_hardlink_objects)
                .context("copying objects")?;
            merge_alternates_from_source_objects(&source.git_dir, &dest.git_dir.join("objects"))
                .context("merging source alternates")?;
        }
        append_reference_alternates(
            &dest.git_dir.join("objects"),
            &args.reference,
            &args.reference_if_able,
        )
        .context("adding --reference alternates")?;
        // `--shared` (-s) borrows objects via `info/alternates`. A plain local clone (including
        // `git clone -l` without `-s`) materializes objects in the destination and must not leave a
        // lone `alternates` file — `t5605` checks `find objects -links 1`.
        // With `--reference` / `--reference-if-able`, Git borrows from the explicit reference and
        // does not add the source as another alternate.
        let alt_dir = dest.git_dir.join("objects/info");
        let _ = fs::create_dir_all(&alt_dir);
        let should_add_source_alternate = args.shared && !has_reference;
        if should_add_source_alternate {
            let source_objects = object_store_git_dir(&source.git_dir).join("objects");
            if let Ok(abs) = source_objects.canonicalize() {
                add_alternate_objects_line(&alt_dir, &abs)?;
            }
        }
        propagate_extensions_object_format(&source.git_dir, &dest.git_dir)?;
    }

    if source_repo_is_shallow(&source.git_dir) {
        copy_shallow_file_if_present(&source.git_dir, &dest.git_dir)?;
    }

    // `upload-pack` negotiation can succeed without transferring objects (e.g. advertisement
    // parse quirks). Match `git clone --no-local` by ensuring the destination has a real ODB.
    let used_upload_pack_object_transfer =
        use_upload_for_protocol_v1 || args.no_local || source_repo_is_shallow(&source.git_dir);
    if !args.shared
        && used_upload_pack_object_transfer
        && !dest.git_dir.join("objects/info/alternates").exists()
        && objects_dir_has_no_data(&dest.git_dir)
    {
        copy_objects(&source.git_dir, &dest.git_dir, false)
            .context("copying objects after empty fetch")?;
    }

    if !use_upload_for_protocol_v1 {
        if crate::trace_packet::trace_packet_dest().is_some() {
            crate::trace_packet::trace_packet_line(b"clone> packfile negotiation complete");
        }

        assert_source_refs_valid_for_clone(&source.git_dir).context("invalid source ref")?;

        if args.bare {
            if args.mirror {
                copy_refs_mirror_all(&source.git_dir, &dest.git_dir)
                    .context("copying mirror refs")?;
                let url = source_path
                    .canonicalize()
                    .unwrap_or_else(|_| source_path.clone())
                    .to_string_lossy()
                    .to_string();
                let mirror_url = if is_file_url {
                    remote_url_for_config.clone()
                } else {
                    url
                };
                setup_remote_mirror_fetch_and_url(&dest.git_dir, &mirror_url, &remote_name)
                    .context("setting up mirror remote")?;
            } else {
                copy_refs_direct(&source.git_dir, &dest.git_dir).context("copying refs")?;
                if is_file_url {
                    setup_origin_remote_bare_url(
                        &dest.git_dir,
                        remote_url_for_config.as_str(),
                        &remote_name,
                    )
                    .context("setting up origin remote")?;
                } else {
                    setup_origin_remote_bare(&dest.git_dir, &source_path, &remote_name)
                        .context("setting up origin remote")?;
                }
                if let Some(ref branch) = head_branch {
                    fs::write(
                        dest.git_dir.join("HEAD"),
                        format!("ref: refs/heads/{branch}\n"),
                    )?;
                } else if let Some(ref oid) = source_head_oid {
                    // A detached HEAD writes a raw OID. Upstream updates HEAD via
                    // `UPDATE_REFS_DIE_ON_ERR` (no skip-verification), so the target object must
                    // exist; if the source object store is missing it (e.g. a corrupted source),
                    // the clone fails and `junk_guard` removes the partial destination. Branch
                    // (symref) HEADs are written via an INITIAL transaction upstream, which skips
                    // object verification, so only the detached case is checked here.
                    verify_detached_head_object_present(&dest, oid)
                        .context("setting detached HEAD")?;
                    fs::write(dest.git_dir.join("HEAD"), format!("{oid}\n"))?;
                }
            }
        } else if branch_is_tag {
            // `--branch <tag>`: HEAD is detached at the tag's target object. Per upstream
            // `write_refspec_config`, only a `--single-branch` clone narrows the fetch
            // refspec to `+refs/tags/<tag>:refs/tags/<tag>` and copies just the followed
            // tags; a normal (multi-branch) clone keeps the default `+refs/heads/*` refspec
            // and copies all branches as remote-tracking refs (`t5601` "checking out a tag").
            let tag = args.branch.as_deref().unwrap_or_default();
            if args.single_branch {
                copy_followed_tags_for_tag_clone(&source.git_dir, &dest.git_dir, args.no_tags)
                    .context("copying tags")?;
            } else {
                copy_refs_as_remote_filtered(
                    &source.git_dir,
                    &dest.git_dir,
                    &remote_name,
                    args.no_tags,
                    None,
                )
                .context("copying refs")?;
            }

            let refspec = if args.single_branch {
                format!("+refs/tags/{tag}:refs/tags/{tag}")
            } else {
                format!("+refs/heads/*:refs/remotes/{remote_name}/*")
            };
            if is_file_url {
                setup_origin_remote_url(
                    &dest.git_dir,
                    remote_url_for_config.as_str(),
                    &remote_name,
                    &refspec,
                )
                .context("setting up origin remote")?;
            } else {
                setup_origin_remote(&dest.git_dir, &source_path, &remote_name, &refspec)
                    .context("setting up origin remote")?;
            }
            setup_remote_tracking_head(
                &dest.git_dir,
                &remote_name,
                &source.git_dir,
                source_head_symref.as_deref(),
                source_head_oid.as_deref(),
            )?;

            if let Some(ref oid) = source_head_oid {
                verify_detached_head_object_present(&dest, oid).context("setting detached HEAD")?;
                fs::write(dest.git_dir.join("HEAD"), format!("{oid}\n"))?;
            }
        } else if args.single_branch
            && head_branch.is_none()
            && source_head_symref.is_none()
            && source_head_oid.is_some()
        {
            // `--single-branch` from a detached source HEAD: upstream's `guess_remote_head` cannot
            // pick a branch to follow, so it writes no fetch refspec and no remote-tracking ref.
            // Only the (object-present) tags are followed, and HEAD is detached at the source OID.
            copy_followed_tags_for_tag_clone(&source.git_dir, &dest.git_dir, args.no_tags)
                .context("copying tags")?;
            if is_file_url {
                setup_origin_remote_bare_url(
                    &dest.git_dir,
                    remote_url_for_config.as_str(),
                    &remote_name,
                )
                .context("setting up origin remote")?;
            } else {
                setup_origin_remote_bare(&dest.git_dir, &source_path, &remote_name)
                    .context("setting up origin remote")?;
            }
            if let Some(ref oid) = source_head_oid {
                verify_detached_head_object_present(&dest, oid).context("setting detached HEAD")?;
                fs::write(dest.git_dir.join("HEAD"), format!("{oid}\n"))?;
            }
        } else {
            // Non-bare clone: copy refs as remote-tracking refs
            let single_branch_name = if args.single_branch {
                Some(head_branch.as_deref().unwrap_or(initial_fallback.as_str()))
            } else {
                None
            };
            copy_refs_as_remote_filtered(
                &source.git_dir,
                &dest.git_dir,
                &remote_name,
                args.no_tags,
                single_branch_name,
            )
            .context("copying refs")?;

            // Set up remote "origin" in config
            let refspec = if args.single_branch {
                let branch = head_branch.as_deref().unwrap_or(initial_fallback.as_str());
                format!("+refs/heads/{branch}:refs/remotes/{remote_name}/{branch}")
            } else {
                format!("+refs/heads/*:refs/remotes/{remote_name}/*")
            };
            if is_file_url {
                setup_origin_remote_url(
                    &dest.git_dir,
                    remote_url_for_config.as_str(),
                    &remote_name,
                    &refspec,
                )
                .context("setting up origin remote")?;
            } else {
                setup_origin_remote(&dest.git_dir, &source_path, &remote_name, &refspec)
                    .context("setting up origin remote")?;
            }
            setup_remote_tracking_head(
                &dest.git_dir,
                &remote_name,
                &source.git_dir,
                source_head_symref.as_deref(),
                source_head_oid.as_deref(),
            )?;

            // Set HEAD to the chosen branch if it exists in remote refs.
            // Prefer `initial_branch` (matches `init_repository`'s HEAD symref), not only
            // `head_branch`, so `refs/heads/*` is created when `guess_checkout_branch` returns
            // `None` but init used `initial_fallback`. If the preferred name has no
            // remote-tracking ref but exactly one exists under `refs/remotes/<remote>/`, use
            // that (sole-branch clone when names disagree).
            let source_head_detached =
                source_head_symref.is_none() && source_head_oid.is_some() && head_branch.is_none();
            let preferred_branch = head_branch.as_deref().unwrap_or(initial_fallback.as_str());
            let source_unborn_preferred = source_head_symref.as_deref().is_some_and(|sr| {
                let expected = format!("refs/heads/{preferred_branch}");
                sr == expected && !clone_ref_file_exists(&source.git_dir, sr)
            });
            let resolved_branch = if source_unborn_preferred || source_head_detached {
                None
            } else {
                resolve_remote_tracked_branch_name(&dest.git_dir, &remote_name, preferred_branch)
            };
            if let Some(branch) = resolved_branch {
                let remote_ref_name = format!("refs/remotes/{remote_name}/{branch}");
                let oid = clone_read_direct_ref_oid(&dest.git_dir, &remote_ref_name)
                    .context("reading remote ref")?;
                let local_ref_name = format!("refs/heads/{branch}");
                clone_write_direct_ref(&dest.git_dir, &local_ref_name, &oid)?;

                fs::write(
                    dest.git_dir.join("HEAD"),
                    format!("ref: refs/heads/{branch}\n"),
                )?;

                setup_branch_tracking(&dest.git_dir, &branch, &remote_name)
                    .context("setting up branch tracking")?;
            } else if let Some(ref branch) = head_branch {
                if let Some(sr) = source_head_symref.as_deref() {
                    if sr == format!("refs/heads/{branch}")
                        && !clone_ref_file_exists(&source.git_dir, sr)
                    {
                        setup_branch_tracking(&dest.git_dir, branch, &remote_name)
                            .context("setting up branch tracking")?;
                    }
                }
            } else if let Some(ref oid) = source_head_oid {
                fs::write(dest.git_dir.join("HEAD"), format!("{oid}\n"))?;
            }
        }
    }

    if use_upload_for_protocol_v1 && crate::trace_packet::trace_packet_dest().is_some() {
        crate::trace_packet::trace_packet_line(b"clone> packfile negotiation complete");
    }

    apply_default_submodule_path_config_from_global(&dest.git_dir)?;
    // Apply -c config values (overrides global defaults such as submodulePathConfig).
    // These were already applied before the fetch (see `clone_config_applied`); re-applying would
    // duplicate multi-valued entries (e.g. fetch refspecs), so only apply here if not done yet.
    if !args.config.is_empty() && !clone_config_applied {
        apply_clone_config(&dest.git_dir, &args.config).context("applying -c config")?;
    }
    copy_configured_remote_fetch_refspecs(&source.git_dir, &dest.git_dir, &remote_name)
        .context("copying refs from clone remote fetch refspecs")?;

    apply_submodule_reference_config_for_recursive_clone(&dest.git_dir, &args)?;

    apply_sticky_recursive_clone(&dest.git_dir, args.recurse_submodules())?;

    // Handle --no-tags: set remote.origin.tagOpt
    if args.no_tags {
        let config_path = dest.git_dir.join("config");
        let mut config = match ConfigFile::from_path(&config_path, ConfigScope::Local)? {
            Some(c) => c,
            None => ConfigFile::parse(&config_path, "", ConfigScope::Local)?,
        };
        config.set(&format!("remote.{remote_name}.tagOpt"), "--no-tags")?;
        config.write().context("writing config")?;
    }

    if args.revision.is_none() {
        if let Some(depth) = args.depth {
            if depth > 0 {
                write_shallow_boundary(&dest, depth)?;
            }
        }
    }

    if args.depth.is_some() && !args.bare && !args.no_checkout {
        copy_objects(&source.git_dir, &dest.git_dir, false)
            .context("copying checkout objects for shallow clone")?;
    }

    if partial_blob_none {
        let filter_spec = filter_spec.as_deref().unwrap_or("blob:none");
        materialize_blob_none_partial_layout(&dest)
            .context("materializing partial-clone object layout")?;
        initialize_partial_clone_state(&source, &dest, &remote_name, filter_spec)
            .context("initializing partial-clone promisor state")?;
    } else if clone_filter_omits_root_trees(filter_spec.as_deref()) {
        let filter_spec = filter_spec.as_deref().unwrap_or("tree:0");
        let omitted = materialize_tree_zero_partial_layout(&dest)
            .context("materializing tree:0 partial-clone object layout")?;
        initialize_partial_clone_state_from_missing(&dest, &remote_name, filter_spec, omitted)
            .context("initializing tree:0 partial-clone promisor state")?;
    } else if let Some(spec) = filter_spec.as_deref().filter(|s| !s.trim().is_empty()) {
        // Any other `--filter` clone (e.g. `blob:limit=<n>`): the server already omitted the
        // filtered objects from the pack, so we don't materialize anything — we just register the
        // clone remote as a promisor with this filter (Git's `partial_clone_register`), so a later
        // checkout / `git diff` can lazily fetch the omitted blobs from the clone remote when the
        // accepted LOP cannot serve them (`t5710` KnownName-missing-URL / KnownUrl-reject cases).
        let omitted = collect_reachable_missing_oids_in_dest(&dest)?;
        initialize_partial_clone_state_from_missing(&dest, &remote_name, spec, omitted)
            .context("initializing filtered partial-clone promisor state")?;
    }

    // `materialize_blob_none_partial_layout` removes `objects/info/alternates`, so blobs
    // needed for the initial checkout must be copied into the clone explicitly.
    // Only for an explicit `--filter=blob:none` clone; cloning a promisor repo without `--filter`
    // inherits metadata (`partial_blob_none`) but must not pre-hydrate (t0411-clone-from-partial).
    if clone_filter_omits_root_trees(filter_spec.as_deref()) && !args.bare && !args.no_checkout {
        hydrate_tree_zero_checkout_objects(&dest).context("hydrating tree:0 checkout objects")?;
    }

    if partial_blob_none
        && matches!(filter_spec.as_deref(), Some("blob:none"))
        && !args.bare
        && !args.no_checkout
    {
        if grit_lib::refs::resolve_ref(&dest.git_dir, "HEAD").is_err() {
            crate::commands::promisor_hydrate::trim_promisor_marker_to_missing_local(&dest)
                .context("trimming promisor marker")?;
        } else {
            let dest_config = ConfigSet::load(Some(&dest.git_dir), true)?;
            let promisor = crate::commands::promisor_hydrate::find_promisor_source(
                &dest_config,
                &dest.git_dir,
            )?;
            if let Some(ref p) = promisor {
                if args.sparse {
                    let patterns = vec!["/*".to_string(), "!/*/".to_string()];
                    crate::commands::promisor_hydrate::hydrate_sparse_tip_blobs_from_promisor(
                        &dest, p, &patterns, true,
                    )
                    .context("hydrating sparse-checkout tip blobs")?;
                    let head_oid = grit_lib::refs::resolve_ref(&dest.git_dir, "HEAD")?;
                    let obj = dest.odb.read(&head_oid).context("reading HEAD for index")?;
                    let commit = parse_commit(&obj.data).context("parsing HEAD for index")?;
                    write_index_from_tree(&dest, &commit.tree)
                        .context("writing index for sparse clone")?;
                } else {
                    crate::commands::promisor_hydrate::hydrate_head_tree_blobs_from_promisor(
                        &dest, p,
                    )
                    .context("hydrating HEAD tree blobs")?;
                }
            }
            crate::commands::promisor_hydrate::trim_promisor_marker_to_missing_local(&dest)
                .context("trimming promisor marker")?;
        }
    }

    if partial_blob_none
        && !matches!(filter_spec.as_deref(), Some("blob:none"))
        && !read_promisor_missing_oids(&dest.git_dir).is_empty()
        && !crate::commands::promisor_hydrate::promisor_lazy_fetch_allowed_for_client_process()?
    {
        crate::commands::promisor_hydrate::warn_lazy_fetch_disabled_once();
        bail!("lazy fetching disabled");
    }

    // Handle --revision: detached HEAD at the resolved commit, no local refs,
    // and no remote.fetch (matches git clone --revision).
    if let Some(ref revision) = args.revision {
        let rev_oid = resolve_revision_for_clone(&source, revision, &remote_name)?;
        strip_refs_for_revision_clone(&dest.git_dir)?;
        fs::write(dest.git_dir.join("HEAD"), format!("{}\n", rev_oid))?;
        remove_revision_clone_remote_config(&dest.git_dir, &remote_name)?;
    }

    // Shallow boundary must be computed from the final HEAD (after --revision).
    if let Some(depth) = args.depth {
        if depth > 0 {
            write_shallow_boundary(&dest, depth)?;
            if is_file_url && !args.no_tags {
                prune_shallow_tags_not_reachable_from_boundaries(&source.git_dir, &dest.git_dir)?;
            }
        }
    }

    maybe_print_local_clone_progress(
        args.progress || (!args.quiet && io::stderr().is_terminal() && is_file_url),
    );

    let skip_checkout_warn = !args.bare
        && !args.no_checkout
        && args.revision.is_none()
        && head_points_to_missing_ref(&dest)
        && !is_unborn_remote_default_checkout(
            &dest,
            source_head_symref.as_deref(),
            head_branch.as_deref(),
            &source.git_dir,
        );

    let sparse_partial_skip =
        partial_blob_none && matches!(filter_spec.as_deref(), Some("blob:none")) && args.sparse;

    // The repository (git dir + objects) is now fully populated. From here on a failure must leave
    // the partial repo on disk so the user can inspect and retry, matching upstream Git's
    // `junk_mode = JUNK_LEAVE_REPO` immediately before `checkout()` (builtin/clone.c). In
    // particular, a checkout failure from a corrupt object keeps `<dest>/.git` (t1060).
    junk_guard.leave_repo();

    // Checkout working tree unless --bare or --no-checkout.
    // Sparse partial clones already materialize tip files via promisor hydration.
    if !args.bare && !args.no_checkout && !sparse_partial_skip {
        if skip_checkout_warn {
            eprintln!("warning: remote HEAD refers to nonexistent ref, unable to checkout");
        } else if head_points_to_missing_ref(&dest) {
            checkout_head_allow_unborn(&dest).context("checking out HEAD")?;
        } else {
            checkout_head(&dest).context("checking out HEAD")?;
        }
    }

    // Sparse-checkout: after full checkout for non-partial; after sparse hydration for partial.
    if args.sparse && !args.bare {
        crate::commands::sparse_checkout::init_clone_sparse_checkout(&dest, !args.no_checkout)
            .context("initializing sparse-checkout")?;
    }

    if !args.bare && !args.no_checkout && (!skip_checkout_warn || sparse_partial_skip) {
        run_post_checkout_after_clone(&dest)?;
    }

    if args.sparse && !args.bare {
        crate::commands::sparse_checkout::finalize_sparse_clone(&dest, !args.no_checkout)?;
    }

    if !args.bare {
        if let Ok(head) = fs::read_to_string(dest.git_dir.join("HEAD")) {
            let h = head.trim();
            if let Some(r) = h.strip_prefix("ref: ") {
                if let Some(b) = r.trim().strip_prefix("refs/heads/") {
                    apply_branch_autosetuprebase_from_global(&dest.git_dir, b.trim())?;
                }
            }
        }
    }

    if args.bare {
        grit_lib::repo::ensure_core_bare(&dest.git_dir)?;
    }

    if !args.quiet && !args.no_checkout {
        eprintln!("done.");
    }

    if args.dissociate {
        dissociate_clone_repository(&dest.git_dir).context("dissociating from alternates")?;
    }

    // Recurse into submodules if requested
    if args.recurse_submodules() && !args.bare {
        if let Some(ref wt) = dest.work_tree {
            if let Err(e) = clone_submodules(wt, &dest, &args) {
                return Err(e).context("cloning submodules");
            }
        }
    }

    // Record the fetched remote-tracking refs in `packed-refs` (initial ref transaction), so a
    // later up-to-date push does not re-create a loose tracking ref (t5516 'push preserves
    // up-to-date packed refs'). Best-effort: a packing failure must not fail the clone.
    let _ = crate::commands::pack_refs::pack_clone_tracking_refs(&dest.git_dir);

    // Clone succeeded: keep the directories we created (upstream `junk_mode = JUNK_LEAVE_ALL`).
    junk_guard.disarm();
    Ok(())
}

/// Clone from `git://` (native daemon transport).
fn run_git_clone(args: Args) -> Result<()> {
    let remote_name = resolve_remote_name(&args)?;
    let ref_storage = resolved_clone_ref_storage(&args)?;
    let filter_active = clone_pack_filter_active(&args, None);
    let url = args.repository.clone();
    let parsed = crate::fetch_transport::parse_git_url(&url)?;
    let path_tail = parsed.path.trim_start_matches('/');
    let base = Path::new(path_tail)
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("repo");
    let target_name = args
        .directory
        .clone()
        .unwrap_or_else(|| base.trim_end_matches('/').to_string());
    let target_path = PathBuf::from(&target_name);
    if target_path.exists() {
        bail!(
            "destination path '{}' already exists and is not an empty directory",
            target_path.display()
        );
    }

    if !args.quiet {
        if args.bare {
            eprintln!("Cloning into bare repository '{target_name}'...");
        } else {
            eprintln!("Cloning into '{target_name}'...");
        }
    }

    let initial_fallback = default_head_branch_fallback();
    let initial_branch = initial_fallback.as_str();

    fs::create_dir_all(&target_path)
        .with_context(|| format!("cannot create directory '{}'", target_path.display()))?;

    let template_dir = effective_template_dir(&args);
    let dest = if let Some(ref sep_git) = args.separate_git_dir {
        if sep_git.exists() && sep_git.read_dir()?.next().is_some() {
            bail!(
                "destination path '{}' already exists and is not an empty directory",
                sep_git.display()
            );
        }
        init_repository_separate_git_dir(
            &target_path,
            sep_git,
            initial_branch,
            template_dir.as_deref(),
            ref_storage,
        )
        .with_context(|| {
            format!(
                "failed to initialize separate git dir '{}'",
                sep_git.display()
            )
        })?
    } else if args.bare && args.template.as_ref().is_some_and(|s| s.is_empty()) {
        init_bare_clone_minimal(&target_path, initial_branch, ref_storage).with_context(|| {
            format!(
                "failed to initialize bare clone '{}'",
                target_path.display()
            )
        })?;
        Repository::open(&target_path, None)
            .with_context(|| format!("failed to open repository '{}'", target_path.display()))?
    } else {
        init_repository(
            &target_path,
            args.bare,
            initial_branch,
            template_dir.as_deref(),
            ref_storage,
        )
        .with_context(|| format!("failed to initialize '{}'", target_path.display()))?
    };

    let fetch_res = crate::fetch_transport::with_packet_trace_identity("clone", || {
        crate::fetch_transport::fetch_via_git_protocol_skipping(
            &dest.git_dir,
            &url,
            &[],
            filter_active,
        )
    });

    let (source_head_symref, _source_head_oid, head_branch) = match fetch_res {
        Ok((remote_heads, remote_tags, adv_sym, head_oid)) => {
            let source_head_symref = adv_sym;
            let source_head_oid = head_oid.map(|o| o.to_hex());
            let head_branch = if args.branch.is_some() {
                args.branch.clone()
            } else {
                guess_checkout_branch(
                    &dest.git_dir,
                    source_head_symref.as_deref(),
                    source_head_oid.as_deref(),
                )?
            };

            if args.bare {
                for (refname, oid) in &remote_heads {
                    let dst_ref = dest.git_dir.join(refname);
                    if let Some(parent) = dst_ref.parent() {
                        fs::create_dir_all(parent)?;
                    }
                    fs::write(&dst_ref, format!("{}\n", oid.to_hex()))?;
                }
                if !args.no_tags {
                    for (refname, oid) in &remote_tags {
                        let dst_ref = dest.git_dir.join(refname);
                        if let Some(parent) = dst_ref.parent() {
                            fs::create_dir_all(parent)?;
                        }
                        fs::write(&dst_ref, format!("{}\n", oid.to_hex()))?;
                    }
                }
                setup_origin_remote_bare_url(&dest.git_dir, url.as_str(), &remote_name)
                    .context("setting up origin remote")?;
                if let Some(ref branch) = head_branch {
                    fs::write(
                        dest.git_dir.join("HEAD"),
                        format!("ref: refs/heads/{branch}\n"),
                    )?;
                } else if let Some(ref oid) = source_head_oid {
                    fs::write(dest.git_dir.join("HEAD"), format!("{oid}\n"))?;
                }
            } else {
                copy_refs_from_upload_pack_lists(
                    &dest.git_dir,
                    &remote_name,
                    &remote_heads,
                    &remote_tags,
                    args.no_tags,
                )?;
                let refspec = if args.single_branch {
                    let branch = head_branch.as_deref().unwrap_or(initial_fallback.as_str());
                    format!("+refs/heads/{branch}:refs/remotes/{remote_name}/{branch}")
                } else {
                    format!("+refs/heads/*:refs/remotes/{remote_name}/*")
                };
                setup_origin_remote_url(&dest.git_dir, url.as_str(), &remote_name, &refspec)
                    .context("setting up origin remote")?;

                setup_remote_tracking_head_from_advertisement(
                    &dest.git_dir,
                    &remote_name,
                    source_head_symref.as_deref(),
                )?;

                let preferred_branch = head_branch.as_deref().unwrap_or(initial_fallback.as_str());
                if let Some(branch) = resolve_remote_tracked_branch_name(
                    &dest.git_dir,
                    &remote_name,
                    preferred_branch,
                ) {
                    let remote_ref = dest
                        .git_dir
                        .join("refs/remotes")
                        .join(&remote_name)
                        .join(&branch);
                    let oid_str = fs::read_to_string(&remote_ref).context("reading remote ref")?;
                    let oid = oid_str.trim().to_string();
                    let local_ref_path = dest.git_dir.join("refs/heads").join(&branch);
                    if let Some(parent) = local_ref_path.parent() {
                        fs::create_dir_all(parent)?;
                    }
                    fs::write(&local_ref_path, format!("{oid}\n"))?;
                    fs::write(
                        dest.git_dir.join("HEAD"),
                        format!("ref: refs/heads/{branch}\n"),
                    )?;
                    setup_branch_tracking(&dest.git_dir, &branch, &remote_name)
                        .context("setting up branch tracking")?;
                } else if let Some(ref branch) = head_branch {
                    if let Some(sr) = source_head_symref.as_deref() {
                        if sr == format!("refs/heads/{branch}") {
                            setup_branch_tracking(&dest.git_dir, branch, &remote_name)
                                .context("setting up branch tracking")?;
                        }
                    }
                }
            }

            (source_head_symref, source_head_oid, head_branch)
        }
        Err(e) => {
            let _ = fs::remove_dir_all(&target_path);
            return Err(e).context("git:// clone failed");
        }
    };

    if crate::trace_packet::trace_packet_dest().is_some() {
        crate::trace_packet::trace_packet_line(b"clone> packfile negotiation complete");
    }

    apply_default_submodule_path_config_from_global(&dest.git_dir)?;
    if !args.config.is_empty() {
        apply_clone_config(&dest.git_dir, &args.config).context("applying -c config")?;
    }

    apply_submodule_reference_config_for_recursive_clone(&dest.git_dir, &args)?;

    apply_sticky_recursive_clone(&dest.git_dir, args.recurse_submodules())?;

    if args.no_tags {
        let config_path = dest.git_dir.join("config");
        let mut config = match ConfigFile::from_path(&config_path, ConfigScope::Local)? {
            Some(c) => c,
            None => ConfigFile::parse(&config_path, "", ConfigScope::Local)?,
        };
        config.set(&format!("remote.{remote_name}.tagOpt"), "--no-tags")?;
        config.write().context("writing config")?;
    }

    if let Some(depth) = args.depth {
        if depth > 0 {
            write_shallow_boundary(&dest, depth)?;
        }
    }

    maybe_print_local_clone_progress(args.progress);

    let skip_checkout_warn = !args.bare
        && !args.no_checkout
        && args.revision.is_none()
        && head_points_to_missing_ref(&dest)
        && !is_unborn_remote_default_checkout(
            &dest,
            source_head_symref.as_deref(),
            head_branch.as_deref(),
            &dest.git_dir,
        );

    if !args.bare && !args.no_checkout {
        if skip_checkout_warn {
            eprintln!("warning: remote HEAD refers to nonexistent ref, unable to checkout");
        } else if head_points_to_missing_ref(&dest) {
            checkout_head_allow_unborn(&dest).context("checking out HEAD")?;
            run_post_checkout_after_clone(&dest)?;
        } else {
            checkout_head(&dest).context("checking out HEAD")?;
            run_post_checkout_after_clone(&dest)?;
        }
    }

    if args.sparse && !args.bare {
        crate::commands::sparse_checkout::finalize_sparse_clone(&dest, !args.no_checkout)?;
    }

    if !args.quiet && !args.no_checkout {
        eprintln!("done.");
    }

    if args.dissociate {
        dissociate_clone_repository(&dest.git_dir).context("dissociating from alternates")?;
    }

    if args.recurse_submodules() && !args.bare {
        if let Some(ref wt) = dest.work_tree {
            if let Err(e) = clone_submodules(wt, &dest, &args) {
                return Err(e).context("cloning submodules");
            }
        }
    }

    Ok(())
}

/// Clone via `ext::` URL (spawn helper, negotiate upload-pack over pipes).
fn run_ext_clone(args: Args) -> Result<()> {
    let remote_name = resolve_remote_name(&args)?;
    let ref_storage = resolved_clone_ref_storage(&args)?;
    let filter_active = clone_pack_filter_active(&args, None);
    let url = args.repository.clone();
    let target_name = args.directory.clone().unwrap_or_else(|| "repo".to_string());
    let target_path = PathBuf::from(&target_name);
    if target_path.exists() {
        bail!(
            "destination path '{}' already exists and is not an empty directory",
            target_path.display()
        );
    }

    if !args.quiet {
        if args.bare {
            eprintln!("Cloning into bare repository '{target_name}'...");
        } else {
            eprintln!("Cloning into '{target_name}'...");
        }
    }

    let initial_fallback = default_head_branch_fallback();
    let initial_branch = initial_fallback.as_str();

    fs::create_dir_all(&target_path)
        .with_context(|| format!("cannot create directory '{}'", target_path.display()))?;

    let template_dir = effective_template_dir(&args);
    let dest = if let Some(ref sep_git) = args.separate_git_dir {
        if sep_git.exists() && sep_git.read_dir()?.next().is_some() {
            bail!(
                "destination path '{}' already exists and is not an empty directory",
                sep_git.display()
            );
        }
        init_repository_separate_git_dir(
            &target_path,
            sep_git,
            initial_branch,
            template_dir.as_deref(),
            ref_storage,
        )
        .with_context(|| {
            format!(
                "failed to initialize separate git dir '{}'",
                sep_git.display()
            )
        })?
    } else if args.bare && args.template.as_ref().is_some_and(|s| s.is_empty()) {
        init_bare_clone_minimal(&target_path, initial_branch, ref_storage).with_context(|| {
            format!(
                "failed to initialize bare clone '{}'",
                target_path.display()
            )
        })?;
        Repository::open(&target_path, None)
            .with_context(|| format!("failed to open repository '{}'", target_path.display()))?
    } else {
        init_repository(
            &target_path,
            args.bare,
            initial_branch,
            template_dir.as_deref(),
            ref_storage,
        )
        .with_context(|| format!("failed to initialize '{}'", target_path.display()))?
    };

    let fetch_res = crate::fetch_transport::with_packet_trace_identity("clone", || {
        crate::ext_transport::fetch_via_ext_skipping(
            &dest.git_dir,
            &url,
            "git-upload-pack",
            &[],
            |adv| crate::fetch_transport::collect_wants(adv, &[]),
            filter_active,
        )
    });

    let (source_head_symref, _source_head_oid, head_branch) = match fetch_res {
        Ok((remote_heads, remote_tags, adv_sym, head_oid)) => {
            let source_head_symref = adv_sym;
            let source_head_oid = head_oid.map(|o| o.to_hex());
            let head_branch = if args.branch.is_some() {
                args.branch.clone()
            } else {
                guess_checkout_branch(
                    &dest.git_dir,
                    source_head_symref.as_deref(),
                    source_head_oid.as_deref(),
                )?
            };

            if args.bare {
                for (refname, oid) in &remote_heads {
                    let dst_ref = dest.git_dir.join(refname);
                    if let Some(parent) = dst_ref.parent() {
                        fs::create_dir_all(parent)?;
                    }
                    fs::write(&dst_ref, format!("{}\n", oid.to_hex()))?;
                }
                if !args.no_tags {
                    for (refname, oid) in &remote_tags {
                        let dst_ref = dest.git_dir.join(refname);
                        if let Some(parent) = dst_ref.parent() {
                            fs::create_dir_all(parent)?;
                        }
                        fs::write(&dst_ref, format!("{}\n", oid.to_hex()))?;
                    }
                }
                setup_origin_remote_bare_url(&dest.git_dir, url.as_str(), &remote_name)
                    .context("setting up origin remote")?;
                if let Some(ref branch) = head_branch {
                    fs::write(
                        dest.git_dir.join("HEAD"),
                        format!("ref: refs/heads/{branch}\n"),
                    )?;
                } else if let Some(ref oid) = source_head_oid {
                    fs::write(dest.git_dir.join("HEAD"), format!("{oid}\n"))?;
                }
            } else {
                copy_refs_from_upload_pack_lists(
                    &dest.git_dir,
                    &remote_name,
                    &remote_heads,
                    &remote_tags,
                    args.no_tags,
                )?;
                let refspec = if args.single_branch {
                    let branch = head_branch.as_deref().unwrap_or(initial_fallback.as_str());
                    format!("+refs/heads/{branch}:refs/remotes/{remote_name}/{branch}")
                } else {
                    format!("+refs/heads/*:refs/remotes/{remote_name}/*")
                };
                setup_origin_remote_url(&dest.git_dir, url.as_str(), &remote_name, &refspec)
                    .context("setting up origin remote")?;

                setup_remote_tracking_head_from_advertisement(
                    &dest.git_dir,
                    &remote_name,
                    source_head_symref.as_deref(),
                )?;

                let preferred_branch = head_branch.as_deref().unwrap_or(initial_fallback.as_str());
                if let Some(branch) = resolve_remote_tracked_branch_name(
                    &dest.git_dir,
                    &remote_name,
                    preferred_branch,
                ) {
                    let remote_ref = dest
                        .git_dir
                        .join("refs/remotes")
                        .join(&remote_name)
                        .join(&branch);
                    let oid_str = fs::read_to_string(&remote_ref).context("reading remote ref")?;
                    let oid = oid_str.trim().to_string();
                    let local_ref_path = dest.git_dir.join("refs/heads").join(&branch);
                    if let Some(parent) = local_ref_path.parent() {
                        fs::create_dir_all(parent)?;
                    }
                    fs::write(&local_ref_path, format!("{oid}\n"))?;
                    fs::write(
                        dest.git_dir.join("HEAD"),
                        format!("ref: refs/heads/{branch}\n"),
                    )?;
                    setup_branch_tracking(&dest.git_dir, &branch, &remote_name)
                        .context("setting up branch tracking")?;
                } else if let Some(ref branch) = head_branch {
                    if let Some(sr) = source_head_symref.as_deref() {
                        if sr == format!("refs/heads/{branch}") {
                            setup_branch_tracking(&dest.git_dir, branch, &remote_name)
                                .context("setting up branch tracking")?;
                        }
                    }
                }
            }

            (source_head_symref, source_head_oid, head_branch)
        }
        Err(e) => {
            let _ = fs::remove_dir_all(&target_path);
            return Err(e).context("ext:: clone failed");
        }
    };

    if crate::trace_packet::trace_packet_dest().is_some() {
        crate::trace_packet::trace_packet_line(b"clone> packfile negotiation complete");
    }

    apply_default_submodule_path_config_from_global(&dest.git_dir)?;
    if !args.config.is_empty() {
        apply_clone_config(&dest.git_dir, &args.config).context("applying -c config")?;
    }

    apply_submodule_reference_config_for_recursive_clone(&dest.git_dir, &args)?;

    apply_sticky_recursive_clone(&dest.git_dir, args.recurse_submodules())?;

    if args.no_tags {
        let config_path = dest.git_dir.join("config");
        let mut config = match ConfigFile::from_path(&config_path, ConfigScope::Local)? {
            Some(c) => c,
            None => ConfigFile::parse(&config_path, "", ConfigScope::Local)?,
        };
        config.set(&format!("remote.{remote_name}.tagOpt"), "--no-tags")?;
        config.write().context("writing config")?;
    }

    if let Some(depth) = args.depth {
        if depth > 0 {
            write_shallow_boundary(&dest, depth)?;
        }
    }

    maybe_print_local_clone_progress(args.progress);

    let skip_checkout_warn = !args.bare
        && !args.no_checkout
        && args.revision.is_none()
        && head_points_to_missing_ref(&dest)
        && !is_unborn_remote_default_checkout(
            &dest,
            source_head_symref.as_deref(),
            head_branch.as_deref(),
            &dest.git_dir,
        );

    if !args.bare && !args.no_checkout {
        if skip_checkout_warn {
            eprintln!("warning: remote HEAD refers to nonexistent ref, unable to checkout");
        } else if head_points_to_missing_ref(&dest) {
            checkout_head_allow_unborn(&dest).context("checking out HEAD")?;
            run_post_checkout_after_clone(&dest)?;
        } else {
            checkout_head(&dest).context("checking out HEAD")?;
            run_post_checkout_after_clone(&dest)?;
        }
    }

    if args.sparse && !args.bare {
        crate::commands::sparse_checkout::finalize_sparse_clone(&dest, !args.no_checkout)?;
    }

    if !args.quiet && !args.no_checkout {
        eprintln!("done.");
    }

    if args.dissociate {
        dissociate_clone_repository(&dest.git_dir).context("dissociating from alternates")?;
    }

    if args.recurse_submodules() && !args.bare {
        if let Some(ref wt) = dest.work_tree {
            if let Err(e) = clone_submodules(wt, &dest, &args) {
                return Err(e).context("cloning submodules");
            }
        }
    }

    Ok(())
}

/// Derive the "humanish" directory name for an HTTP clone URL.
///
/// Matches `git clone`'s default: take the last path component and strip a single trailing
/// `.git` suffix (so `http://host/smart/sha256.git` clones into `sha256`, not `sha256.git`).
fn http_url_basename(url: &str) -> String {
    let u = url.trim_end_matches('/');
    let last = u.rsplit('/').next().unwrap_or("repo");
    last.strip_suffix(".git").unwrap_or(last).to_string()
}

/// Config layers for HTTP clone before the new repo exists.
///
/// Includes system/global plus command-line `-c` overrides from `GIT_CONFIG_PARAMETERS`, then
/// applies clone-specific `-c` entries from `args.config`.
fn clone_http_client_config(args: &Args) -> Result<ConfigSet> {
    let mut set = ConfigSet::load(None, true).unwrap_or_default();
    for entry in &args.config {
        if let Some((key, value)) = entry.split_once('=') {
            set.add_command_override(key.trim(), value.trim())?;
        } else {
            set.add_command_override(entry.trim(), "true")?;
        }
    }
    Ok(set)
}

fn run_http_clone(args: Args) -> Result<()> {
    crate::bundle_uri::clear_http_bundle_cache();
    crate::http_smart::clear_trace2_https_url_dedup();

    let remote_name = resolve_remote_name(&args)?;
    let filter_spec = clone_filter_spec(&args);
    let filter_active = clone_pack_filter_active(&args, None);
    let repo_url = args.repository.clone();
    if let Some(ref bundle_uri) = args.bundle_uri {
        if bundle_uri.contains('\n') || bundle_uri.contains('\r') || bundle_uri.contains(' ') {
            eprintln!("error: bundle-uri: URI is malformed: {bundle_uri}");
            return Ok(());
        }
    }
    let target_name = args.directory.clone().unwrap_or_else(|| {
        let base = http_url_basename(&repo_url);
        // `git clone --bare <url>` (without `--mirror`) keeps the `.git` suffix on the directory.
        if args.bare && !args.mirror {
            format!("{base}.git")
        } else {
            base
        }
    });
    // t5558 chains `git clone ... && test_grep err`; Git exits 0 after printing this error.
    if target_name.contains('\n') || target_name.contains('\r') {
        eprintln!("error: bundle-uri: filename is malformed: {target_name}");
        return Ok(());
    }
    let target_path = PathBuf::from(&target_name);
    if target_path.exists() {
        bail!(
            "destination path '{}' already exists and is not an empty directory",
            target_path.display()
        );
    }

    if !args.quiet {
        if args.bare {
            eprintln!("Cloning into bare repository '{target_name}'...");
        } else {
            eprintln!("Cloning into '{target_name}'...");
        }
    }

    let initial_fallback = default_head_branch_fallback();
    let initial_branch = args
        .branch
        .as_deref()
        .unwrap_or(initial_fallback.as_str())
        .to_string();

    fs::create_dir_all(&target_path)
        .with_context(|| format!("cannot create directory '{}'", target_path.display()))?;
    let template_dir = effective_template_dir(&args);

    if args.separate_git_dir.is_some() {
        bail!("--separate-git-dir is not supported for HTTP clones");
    }
    if args.shared {
        bail!("--shared is not supported for HTTP clones");
    }
    if !args.reference.is_empty() {
        bail!("--reference is not supported for HTTP clones");
    }
    if args.revision.is_some() {
        bail!("--revision is not supported for HTTP clones");
    }

    let ref_storage = resolved_clone_ref_storage(&args)?;

    let dest = if args.bare && args.template.as_ref().is_some_and(|s| s.is_empty()) {
        init_bare_clone_minimal(&target_path, &initial_branch, ref_storage).with_context(|| {
            format!(
                "failed to initialize bare clone '{}'",
                target_path.display()
            )
        })?;
        Repository::open(&target_path, None)
            .with_context(|| format!("failed to open repository '{}'", target_path.display()))?
    } else {
        init_repository(
            &target_path,
            args.bare,
            &initial_branch,
            template_dir.as_deref(),
            ref_storage,
        )
        .with_context(|| format!("failed to initialize '{}'", target_path.display()))?
    };

    if let Some(ref bu) = args.bundle_uri {
        // Match local `--bundle-uri`: missing HTTP resources warn and the clone still proceeds
        // (`t5558` "fail to fetch from non-existent HTTP URL").
        crate::bundle_uri::apply_bundle_uri(&dest.git_dir, bu, &target_name, true)?;
        if bu.starts_with("http://") || bu.starts_with("https://") {
            let config_path = dest.git_dir.join("config");
            let mut cfg = match ConfigFile::from_path(&config_path, ConfigScope::Local)? {
                Some(c) => c,
                None => ConfigFile::parse(&config_path, "", ConfigScope::Local)?,
            };
            cfg.set("log.excludeDecoration", "refs/bundle/")?;
            cfg.set("log.excludedecoration", "refs/bundle/")?;
            cfg.write().context("writing log.excludeDecoration")?;
        }
    }

    let refspec_for_fetch: Vec<String> = if args.single_branch {
        if let Some(ref b) = args.branch {
            vec![format!("refs/heads/{b}")]
        } else {
            vec![]
        }
    } else {
        vec![]
    };

    let http_config = clone_http_client_config(&args)?;
    let http_ctx = crate::http_client::HttpClientContext::from_config_set(&http_config)?;
    let fetch_options = crate::http_smart::HttpFetchOptions {
        depth: args.depth,
        deepen: None,
        shallow_since: args.shallow_since.clone(),
        shallow_exclude: args.shallow_exclude.clone(),
        filter_spec: filter_spec.clone(),
        refetch: false,
        bundle_uri_override: args.bundle_uri.is_some(),
        show_progress: args.progress || (!args.quiet && std::io::stderr().is_terminal()),
    };
    let crate::http_smart::HttpFetchResult {
        heads: remote_heads,
        tags: remote_tags,
        all_advertised: adv,
        object_format,
    } = crate::http_smart::http_fetch_pack(
        &dest.git_dir,
        &repo_url,
        &refspec_for_fetch,
        filter_active,
        &fetch_options,
        &http_ctx,
    )?;
    // Persist the remote's hash algorithm so an empty SHA-256 repo clones as SHA-256 (`t5551`).
    write_clone_object_format(&dest.git_dir, &object_format)
        .context("recording object format into clone config")?;
    crate::bundle_uri::maybe_apply_bundle_uri_after_http_fetch_with_client(
        &dest.git_dir,
        &repo_url,
        None,
        Some(&http_ctx),
    )?;

    if args.bare {
        for e in &remote_heads {
            let dst_ref = dest.git_dir.join(&e.name);
            if let Some(parent) = dst_ref.parent() {
                fs::create_dir_all(parent)?;
            }
            fs::write(&dst_ref, format!("{}\n", e.oid.to_hex()))?;
        }
        if !args.no_tags {
            for e in &remote_tags {
                let dst_ref = dest.git_dir.join(&e.name);
                if let Some(parent) = dst_ref.parent() {
                    fs::create_dir_all(parent)?;
                }
                fs::write(&dst_ref, format!("{}\n", e.oid.to_hex()))?;
            }
        }
        setup_origin_remote_bare_url(&dest.git_dir, &repo_url, &remote_name)?;
        let default_branch = args
            .branch
            .clone()
            .or_else(|| crate::http_smart::remote_default_branch_from_advertised(&adv));
        if let Some(branch) = default_branch {
            fs::write(
                dest.git_dir.join("HEAD"),
                format!("ref: refs/heads/{branch}\n"),
            )?;
        }
    } else {
        for e in &remote_heads {
            let branch = e.name.strip_prefix("refs/heads/").unwrap_or(&e.name);
            let dst_ref = dest
                .git_dir
                .join("refs/remotes")
                .join(&remote_name)
                .join(branch);
            if let Some(parent) = dst_ref.parent() {
                fs::create_dir_all(parent)?;
            }
            fs::write(&dst_ref, format!("{}\n", e.oid.to_hex()))?;
        }
        if !args.no_tags {
            for e in &remote_tags {
                let dst_ref = dest.git_dir.join(&e.name);
                if let Some(parent) = dst_ref.parent() {
                    fs::create_dir_all(parent)?;
                }
                fs::write(&dst_ref, format!("{}\n", e.oid.to_hex()))?;
            }
        }

        let refspec = if args.single_branch {
            let branch = args.branch.as_deref().unwrap_or(initial_fallback.as_str());
            format!("+refs/heads/{branch}:refs/remotes/{remote_name}/{branch}")
        } else {
            format!("+refs/heads/*:refs/remotes/{remote_name}/*")
        };
        setup_origin_remote_url(&dest.git_dir, &repo_url, &remote_name, &refspec)?;

        let default_branch = args
            .branch
            .clone()
            .or_else(|| crate::http_smart::remote_default_branch_from_advertised(&adv));
        if let Some(ref branch) = default_branch {
            let origin_head = dest
                .git_dir
                .join("refs/remotes")
                .join(&remote_name)
                .join("HEAD");
            if let Some(parent) = origin_head.parent() {
                fs::create_dir_all(parent)?;
            }
            fs::write(
                &origin_head,
                format!("ref: refs/remotes/{remote_name}/{branch}\n"),
            )?;
        }

        let head_branch = if args.branch.is_some() {
            Some(initial_branch.clone())
        } else {
            default_branch.clone()
        };

        if let Some(branch) = resolve_remote_tracked_branch_name(
            &dest.git_dir,
            &remote_name,
            head_branch.as_deref().unwrap_or(initial_fallback.as_str()),
        ) {
            let remote_ref = dest
                .git_dir
                .join("refs/remotes")
                .join(&remote_name)
                .join(&branch);
            let oid_str = fs::read_to_string(&remote_ref).context("reading remote ref")?;
            let oid = oid_str.trim().to_string();

            let local_ref_path = dest.git_dir.join("refs/heads").join(&branch);
            if let Some(parent) = local_ref_path.parent() {
                fs::create_dir_all(parent)?;
            }
            fs::write(&local_ref_path, format!("{oid}\n"))?;

            fs::write(
                dest.git_dir.join("HEAD"),
                format!("ref: refs/heads/{branch}\n"),
            )?;

            setup_branch_tracking(&dest.git_dir, &branch, &remote_name)
                .context("setting up branch tracking")?;
        }
    }

    apply_default_submodule_path_config_from_global(&dest.git_dir)?;
    if !args.config.is_empty() {
        apply_clone_config(&dest.git_dir, &args.config).context("applying -c config")?;
    }

    apply_sticky_recursive_clone(&dest.git_dir, args.recurse_submodules())?;

    if args.no_tags {
        let config_path = dest.git_dir.join("config");
        let mut config = match ConfigFile::from_path(&config_path, ConfigScope::Local)? {
            Some(c) => c,
            None => ConfigFile::parse(&config_path, "", ConfigScope::Local)?,
        };
        config.set(&format!("remote.{remote_name}.tagOpt"), "--no-tags")?;
        config.write().context("writing config")?;
    }

    let partial_blob_none_http = matches!(filter_spec.as_deref(), Some("blob:none"));
    if partial_blob_none_http {
        materialize_blob_none_partial_layout(&dest)
            .context("materializing partial-clone object layout")?;
        initialize_partial_clone_state_http(&dest, &remote_name, "blob:none")?;
    }

    // A filtered HTTP clone must still receive every object a ref points at directly (upstream
    // upload-pack always sends ref-tip objects). Verify each ref tip is present or promised; if a
    // crafted/buggy server omits one without promising it, fail like Git rather than leaving a ref
    // dangling (t5616 "upon cloning, check that all refs point to objects").
    if partial_blob_none_http {
        check_clone_ref_tip_connectivity(&dest)
            .context("checking ref-tip connectivity after partial clone")?;
    }

    if partial_blob_none_http && !args.bare && !args.no_checkout {
        if grit_lib::refs::resolve_ref(&dest.git_dir, "HEAD").is_err() {
            crate::commands::promisor_hydrate::trim_promisor_marker_to_missing_local(&dest)
                .context("trimming promisor marker")?;
        } else {
            let dest_config = ConfigSet::load(Some(&dest.git_dir), true)?;
            let promisor = crate::commands::promisor_hydrate::find_promisor_source(
                &dest_config,
                &dest.git_dir,
            )?;
            if let Some(ref p) = promisor {
                if args.sparse {
                    let patterns = vec!["/*".to_string(), "!/*/".to_string()];
                    crate::commands::promisor_hydrate::hydrate_sparse_tip_blobs_from_promisor(
                        &dest, p, &patterns, true,
                    )
                    .context("hydrating sparse-checkout tip blobs")?;
                    let head_oid = grit_lib::refs::resolve_ref(&dest.git_dir, "HEAD")?;
                    let obj = dest.odb.read(&head_oid).context("reading HEAD for index")?;
                    let commit = parse_commit(&obj.data).context("parsing HEAD for index")?;
                    write_index_from_tree(&dest, &commit.tree)
                        .context("writing index for sparse clone")?;
                } else {
                    crate::commands::promisor_hydrate::hydrate_head_tree_blobs_from_promisor(
                        &dest, p,
                    )
                    .context("hydrating HEAD tree blobs")?;
                }
            }
            crate::commands::promisor_hydrate::trim_promisor_marker_to_missing_local(&dest)
                .context("trimming promisor marker")?;
        }
    }

    if let Some(depth) = args.depth {
        if depth > 0 {
            write_shallow_boundary(&dest, depth)?;
        }
    }

    maybe_print_local_clone_progress(args.progress);

    let sparse_partial_skip = partial_blob_none_http && args.sparse;

    if !args.bare && !args.no_checkout && !sparse_partial_skip {
        if head_points_to_missing_ref(&dest) {
            checkout_head_allow_unborn(&dest).context("checking out HEAD")?;
        } else {
            checkout_head(&dest).context("checking out HEAD")?;
        }
    }

    if args.sparse && !args.bare {
        crate::commands::sparse_checkout::init_clone_sparse_checkout(&dest, !args.no_checkout)
            .context("initializing sparse-checkout")?;
    }

    if !args.bare && !args.no_checkout {
        run_post_checkout_after_clone(&dest)?;
    }

    if args.sparse && !args.bare {
        crate::commands::sparse_checkout::finalize_sparse_clone(&dest, !args.no_checkout)?;
    }

    if !args.quiet && !args.no_checkout {
        eprintln!("done.");
    }

    if args.recurse_submodules() && !args.bare {
        if let Some(ref wt) = dest.work_tree {
            clone_submodules(wt, &dest, &args).context("cloning submodules")?;
        }
    }

    Ok(())
}

fn validate_clone_target_name(name: &str) -> Result<()> {
    if name.contains('\n') || name.contains('\r') {
        bail!("bundle-uri: filename is malformed: {name}");
    }
    Ok(())
}

fn initialize_partial_clone_state_http(
    dest: &Repository,
    remote_name: &str,
    filter_spec: &str,
) -> Result<()> {
    let mut missing: Vec<String> = Vec::new();
    let shallow_boundaries = grit_lib::shallow::load_shallow_boundaries(&dest.git_dir);
    if let Ok(head) = grit_lib::refs::resolve_ref(&dest.git_dir, "HEAD") {
        let mut queue = VecDeque::new();
        let mut seen_commits = HashSet::new();
        let mut seen_trees = HashSet::new();
        queue.push_back(head);
        while let Some(oid) = queue.pop_front() {
            let obj = match dest.odb.read(&oid) {
                Ok(o) => o,
                Err(_) => continue,
            };
            match obj.kind {
                ObjectKind::Commit => {
                    if !seen_commits.insert(oid) {
                        continue;
                    }
                    let commit = parse_commit(&obj.data)?;
                    if !shallow_boundaries.contains(&oid) {
                        for p in &commit.parents {
                            queue.push_back(*p);
                        }
                    }
                    queue.push_back(commit.tree);
                }
                ObjectKind::Tree => {
                    if !seen_trees.insert(oid) {
                        continue;
                    }
                    let entries = parse_tree(&obj.data)?;
                    for e in entries {
                        let is_tree = (e.mode & 0o170000) == 0o040000;
                        if is_tree {
                            queue.push_back(e.oid);
                        } else if e.mode == 0o100644 || e.mode == 0o100755 || e.mode == 0o120000 {
                            if dest.odb.read(&e.oid).is_err() {
                                missing.push(e.oid.to_hex());
                            }
                        }
                    }
                }
                _ => {}
            }
        }
    }
    missing.sort();
    missing.dedup();
    let marker = dest.git_dir.join("grit-promisor-missing");
    let marker_content = if missing.is_empty() {
        String::new()
    } else {
        format!("{}\n", missing.join("\n"))
    };
    fs::write(&marker, marker_content)?;

    let config_path = dest.git_dir.join("config");
    let mut config = match ConfigFile::from_path(&config_path, ConfigScope::Local)? {
        Some(c) => c,
        None => ConfigFile::parse(&config_path, "", ConfigScope::Local)?,
    };
    config.set("core.repositoryformatversion", "1")?;
    config.set("extensions.partialclone", remote_name)?;
    config.set(&format!("remote.{remote_name}.promisor"), "true")?;
    config.set(
        &format!("remote.{remote_name}.partialclonefilter"),
        filter_spec,
    )?;
    config.write().context("writing config")?;
    Ok(())
}

/// Compute the local directory name for a clone, matching Git's `git clone` behavior.
///
/// When `--directory` / a positional target is given, Git strips trailing slashes from it;
/// otherwise it guesses a "humanish" name from the raw repository URL via `git_url_basename`
/// (`git/dir.c`). The `--bare` clone appends a `.git` suffix unless `--mirror` is also set
/// (mirror clones keep the guessed name without the suffix), so we pass `is_bare` only when
/// `args.bare && !args.mirror`.
fn clone_target_dir_name(args: &Args) -> Result<String> {
    if let Some(ref d) = args.directory {
        return Ok(d.trim_end_matches('/').to_string());
    }
    let is_bare = args.bare && !args.mirror;
    grit_lib::transport_path::git_url_basename(&args.repository, false, is_bare)
        .map_err(|e| anyhow::anyhow!("{e}"))
}

/// Check whether a URL looks like an SSH-style `host:/path` address.
///
/// Returns `false` for local paths, `file://` URLs, or URLs containing `://`.
fn is_ssh_url(url: &str) -> bool {
    if url.contains("://") {
        return false;
    }
    let colon = url.find(':');
    let slash = url.find('/');
    match colon {
        None => false,
        Some(ci) => {
            if slash.is_some_and(|si| si < ci) {
                return false;
            }
            let host = &url[..ci];
            let path = &url[ci + 1..];
            !host.is_empty() && !path.is_empty()
        }
    }
}

/// Clone from an SSH URL when the remote resolves to a local repository (test
/// harness with `GIT_SSH` wrappers, or `host:/absolute/path` on the same machine).
fn run_ssh_clone(args: Args) -> Result<()> {
    let spec = crate::ssh_transport::parse_ssh_url(&args.repository)?;
    let Some(src_git_dir) = crate::ssh_transport::try_local_git_dir(&spec) else {
        return run_ssh_network_clone(args, &spec);
    };

    let source = Repository::open(&src_git_dir, None).with_context(|| {
        format!(
            "could not open source repository at '{}'",
            src_git_dir.display()
        )
    })?;

    crate::ssh_transport::record_resolved_git_ssh_upload_pack_for_tests(
        &spec,
        args.upload_pack.as_deref(),
        args.ipv4,
        args.ipv6,
    )?;

    let filter_spec = clone_filter_spec(&args);
    let partial_blob_limit_zero = matches!(
        filter_spec.as_deref(),
        Some("blob:limit=0") | Some("blob:size=0")
    );
    // The source's own promisor filter is only inherited when the client did NOT pass an explicit
    // `--filter`; an explicit filter (e.g. `--filter=blob:limit=5k`) takes precedence and must not
    // be silently downgraded to the source's `blob:none` (`t5710`: a `blob:limit` clone of a server
    // whose LOP remote is recorded as `blob:none` must still keep small blobs).
    let inherited_blob_none = args.filter.is_empty()
        && inherited_partial_clone_filter_spec(&source.git_dir).as_deref() == Some("blob:none");
    let partial_blob_none = matches!(filter_spec.as_deref(), Some("blob:none"))
        || (partial_blob_limit_zero && uploadpack_filter_allowed(&source.git_dir))
        || inherited_blob_none;
    let pack_filter_active = clone_pack_filter_active(&args, Some(&source.git_dir));

    if effective_reject_shallow(&args) && source_repo_is_shallow(&source.git_dir) {
        bail!("source repository is shallow, reject to clone.");
    }
    if source_repo_is_shallow(&source.git_dir) && !args.no_local {
        eprintln!("warning: source repository is shallow, ignoring --local");
    }
    maybe_warn_shallow_options_ignored(&args.repository, &args);
    let remote_name = resolve_remote_name(&args)?;
    let server_options = effective_clone_server_options(&args, &remote_name)?;
    let ref_storage = resolved_clone_ref_storage(&args)?;

    let target_name = clone_target_dir_name(&args)?;

    let target_path = PathBuf::from(&target_name);
    let empty_dir_ok = path_is_empty_directory(&target_path);
    if target_path.exists() && !empty_dir_ok {
        bail!(
            "destination path '{}' already exists and is not an empty directory",
            target_path.display()
        );
    }

    if !args.quiet {
        if args.bare {
            eprintln!("Cloning into bare repository '{}'...", target_name);
        } else {
            eprintln!("Cloning into '{}'...", target_name);
        }
    }

    let remote_url_for_config = args.repository.clone();
    let use_upload_for_protocol_v1 = protocol_wire::effective_client_protocol_version() == 1;

    let (mut source_head_symref, mut source_head_oid) = read_source_head_info(&source.git_dir);
    let head_branch = if args.branch.is_some() {
        determine_head_branch(&source.git_dir, args.branch.as_deref())?
    } else if use_upload_for_protocol_v1 {
        None
    } else {
        guess_checkout_branch(
            &source.git_dir,
            source_head_symref.as_deref(),
            source_head_oid.as_deref(),
        )?
    };
    let initial_fallback = default_head_branch_fallback();
    let initial_branch = head_branch.as_deref().unwrap_or(initial_fallback.as_str());

    if !empty_dir_ok {
        fs::create_dir_all(&target_path)
            .with_context(|| format!("cannot create directory '{}'", target_path.display()))?;
    }

    let template_dir = effective_template_dir(&args);
    let git_work_tree_env = std::env::var_os("GIT_WORK_TREE").filter(|v| !v.is_empty());

    let dest = if let Some(ref sep_git) = args.separate_git_dir {
        if sep_git.exists() && sep_git.read_dir()?.next().is_some() {
            bail!(
                "destination path '{}' already exists and is not an empty directory",
                sep_git.display()
            );
        }
        init_repository_separate_git_dir(
            &target_path,
            sep_git,
            initial_branch,
            template_dir.as_deref(),
            ref_storage,
        )
        .with_context(|| {
            format!(
                "failed to initialize separate git dir '{}'",
                sep_git.display()
            )
        })?
    } else if args.bare && args.template.as_ref().is_some_and(|s| s.is_empty()) {
        init_bare_clone_minimal(&target_path, initial_branch, ref_storage).with_context(|| {
            format!(
                "failed to initialize bare clone '{}'",
                target_path.display()
            )
        })?;
        Repository::open(&target_path, None)
            .with_context(|| format!("failed to open repository '{}'", target_path.display()))?
    } else if git_work_tree_env.is_some() {
        let wt_raw = git_work_tree_env
            .as_ref()
            .map(|v| v.to_string_lossy().to_string())
            .unwrap_or_default();
        let wt_path = if Path::new(&wt_raw).is_absolute() {
            PathBuf::from(&wt_raw)
        } else {
            std::env::current_dir()?.join(&wt_raw)
        };
        init_bare_with_env_worktree(
            &target_path,
            &wt_path,
            initial_branch,
            template_dir.as_deref(),
            ref_storage,
        )
        .with_context(|| format!("failed to initialize '{}'", target_path.display()))?
    } else {
        init_repository(
            &target_path,
            args.bare,
            initial_branch,
            template_dir.as_deref(),
            ref_storage,
        )
        .with_context(|| format!("failed to initialize '{}'", target_path.display()))?
    };

    let mut head_branch = head_branch;
    let upload_pack_shallow_options = crate::fetch_transport::UploadPackShallowOptions {
        depth: args.depth,
        deepen: None,
        shallow_since: args.shallow_since.clone(),
        shallow_exclude: args.shallow_exclude.clone(),
        unshallow: false,
    };
    let pack_filter_spec = filter_spec.as_deref().filter(|s| !s.trim().is_empty());

    if let Some(ref bu) = args.bundle_uri {
        crate::bundle_uri::apply_bundle_uri(&dest.git_dir, bu, &target_name, true)?;
    }

    if use_upload_for_protocol_v1 {
        let upload_cmd = args.upload_pack.as_deref().filter(|s| !s.trim().is_empty());
        let fetch_res = crate::fetch_transport::with_packet_trace_identity("clone", || {
            crate::fetch_transport::fetch_via_upload_pack_skipping(
                &dest.git_dir,
                &source.git_dir,
                upload_cmd,
                |adv| crate::fetch_transport::collect_wants(adv, &[]),
                false,
                true,
                pack_filter_active,
                true,
                None,
                Some(&upload_pack_shallow_options),
                pack_filter_spec,
                &[],
                &server_options,
            )
        });
        match fetch_res {
            Ok((remote_heads, remote_tags, adv_sym, head_oid)) => {
                propagate_extensions_object_format(&source.git_dir, &dest.git_dir)?;
                source_head_symref = adv_sym;
                source_head_oid = head_oid.map(|o| o.to_hex());
                head_branch = if args.branch.is_some() {
                    determine_head_branch(&source.git_dir, args.branch.as_deref())?
                } else {
                    guess_checkout_branch(
                        &dest.git_dir,
                        source_head_symref.as_deref(),
                        source_head_oid.as_deref(),
                    )?
                };

                if args.bare {
                    if args.mirror {
                        copy_refs_mirror_all(&source.git_dir, &dest.git_dir)
                            .context("copying mirror refs")?;
                        setup_remote_mirror_fetch_and_url(
                            &dest.git_dir,
                            remote_url_for_config.as_str(),
                            &remote_name,
                        )
                        .context("setting up mirror remote")?;
                    } else {
                        for (refname, oid) in &remote_heads {
                            let dst_ref = dest.git_dir.join(refname);
                            if let Some(parent) = dst_ref.parent() {
                                fs::create_dir_all(parent)?;
                            }
                            fs::write(&dst_ref, format!("{}\n", oid.to_hex()))?;
                        }
                        if !args.no_tags {
                            for (refname, oid) in &remote_tags {
                                let dst_ref = dest.git_dir.join(refname);
                                if let Some(parent) = dst_ref.parent() {
                                    fs::create_dir_all(parent)?;
                                }
                                fs::write(&dst_ref, format!("{}\n", oid.to_hex()))?;
                            }
                        }
                        setup_origin_remote_bare_url(
                            &dest.git_dir,
                            remote_url_for_config.as_str(),
                            &remote_name,
                        )
                        .context("setting up origin remote")?;
                        if let Some(ref branch) = head_branch {
                            fs::write(
                                dest.git_dir.join("HEAD"),
                                format!("ref: refs/heads/{branch}\n"),
                            )?;
                        } else if let Some(ref oid) = source_head_oid {
                            fs::write(dest.git_dir.join("HEAD"), format!("{oid}\n"))?;
                        }
                    }
                } else {
                    copy_refs_from_upload_pack_lists(
                        &dest.git_dir,
                        &remote_name,
                        &remote_heads,
                        &remote_tags,
                        args.no_tags,
                    )?;
                    let refspec = if args.single_branch {
                        let branch = head_branch.as_deref().unwrap_or(initial_fallback.as_str());
                        format!("+refs/heads/{branch}:refs/remotes/{remote_name}/{branch}")
                    } else {
                        format!("+refs/heads/*:refs/remotes/{remote_name}/*")
                    };
                    setup_origin_remote_url(
                        &dest.git_dir,
                        remote_url_for_config.as_str(),
                        &remote_name,
                        &refspec,
                    )
                    .context("setting up origin remote")?;

                    setup_remote_tracking_head(
                        &dest.git_dir,
                        &remote_name,
                        &source.git_dir,
                        source_head_symref.as_deref(),
                        source_head_oid.as_deref(),
                    )?;
                    setup_remote_tracking_head_from_advertisement(
                        &dest.git_dir,
                        &remote_name,
                        source_head_symref.as_deref(),
                    )?;

                    let preferred_branch =
                        head_branch.as_deref().unwrap_or(initial_fallback.as_str());
                    if let Some(branch) = resolve_remote_tracked_branch_name(
                        &dest.git_dir,
                        &remote_name,
                        preferred_branch,
                    ) {
                        let remote_ref = dest
                            .git_dir
                            .join("refs/remotes")
                            .join(&remote_name)
                            .join(&branch);
                        let oid_str =
                            fs::read_to_string(&remote_ref).context("reading remote ref")?;
                        let oid = oid_str.trim().to_string();
                        let local_ref_path = dest.git_dir.join("refs/heads").join(&branch);
                        if let Some(parent) = local_ref_path.parent() {
                            fs::create_dir_all(parent)?;
                        }
                        fs::write(&local_ref_path, format!("{oid}\n"))?;
                        fs::write(
                            dest.git_dir.join("HEAD"),
                            format!("ref: refs/heads/{branch}\n"),
                        )?;
                        setup_branch_tracking(&dest.git_dir, &branch, &remote_name)
                            .context("setting up branch tracking")?;
                    } else if let Some(ref branch) = head_branch {
                        if let Some(sr) = source_head_symref.as_deref() {
                            if sr == format!("refs/heads/{branch}")
                                && !source.git_dir.join(sr).exists()
                            {
                                setup_branch_tracking(&dest.git_dir, branch, &remote_name)
                                    .context("setting up branch tracking")?;
                            }
                        }
                    }
                }
            }
            Err(e) => {
                let _ = fs::remove_dir_all(&target_path);
                return Err(e).context("clone via upload-pack (protocol.version=1) failed");
            }
        }
    } else if !args.shared
        && (args.no_local || source_repo_is_shallow(&source.git_dir))
        && !use_upload_for_protocol_v1
    {
        let upload_cmd = args.upload_pack.as_deref().filter(|s| !s.trim().is_empty());
        match crate::fetch_transport::fetch_via_upload_pack_skipping(
            &dest.git_dir,
            &source.git_dir,
            upload_cmd,
            |adv| crate::fetch_transport::collect_wants(adv, &[]),
            false,
            true,
            pack_filter_active,
            true,
            None,
            Some(&upload_pack_shallow_options),
            pack_filter_spec,
            &[],
            &server_options,
        ) {
            Ok(_) => {
                propagate_extensions_object_format(&source.git_dir, &dest.git_dir)?;
            }
            Err(e) => {
                let _ = fs::remove_dir_all(&target_path);
                return Err(e).context("clone via upload-pack (--no-local) failed");
            }
        }
    } else if args.shared {
        write_shared_alternates(
            &source.git_dir,
            &dest.git_dir,
            &args.reference,
            &args.reference_if_able,
        )
        .context("setting up alternates")?;
        propagate_extensions_object_format(&source.git_dir, &dest.git_dir)?;
    } else {
        let has_reference = !args.reference.is_empty() || !args.reference_if_able.is_empty();
        if !has_reference {
            copy_objects(&source.git_dir, &dest.git_dir, !args.no_hardlinks)
                .context("copying objects")?;
            merge_alternates_from_source_objects(&source.git_dir, &dest.git_dir.join("objects"))
                .context("merging source alternates")?;
        }
        append_reference_alternates(
            &dest.git_dir.join("objects"),
            &args.reference,
            &args.reference_if_able,
        )
        .context("adding --reference alternates")?;
        let alt_dir = dest.git_dir.join("objects/info");
        let _ = fs::create_dir_all(&alt_dir);
        let should_add_source_alternate = args.shared && !has_reference;
        if should_add_source_alternate {
            let source_objects = object_store_git_dir(&source.git_dir).join("objects");
            if let Ok(abs) = source_objects.canonicalize() {
                add_alternate_objects_line(&alt_dir, &abs)?;
            }
        }
        propagate_extensions_object_format(&source.git_dir, &dest.git_dir)?;
    }

    if source_repo_is_shallow(&source.git_dir) {
        copy_shallow_file_if_present(&source.git_dir, &dest.git_dir)?;
    }

    let used_upload_pack_object_transfer_ssh =
        use_upload_for_protocol_v1 || args.no_local || source_repo_is_shallow(&source.git_dir);
    if !args.shared
        && used_upload_pack_object_transfer_ssh
        && !dest.git_dir.join("objects/info/alternates").exists()
        && objects_dir_has_no_data(&dest.git_dir)
    {
        copy_objects(&source.git_dir, &dest.git_dir, false)
            .context("copying objects after empty fetch")?;
    }

    let remote_url = args.repository.as_str();

    if !use_upload_for_protocol_v1 {
        assert_source_refs_valid_for_clone(&source.git_dir).context("invalid source ref")?;
        if args.bare {
            if args.mirror {
                copy_refs_mirror_all(&source.git_dir, &dest.git_dir)
                    .context("copying mirror refs")?;
                setup_remote_mirror_fetch_and_url(&dest.git_dir, remote_url, &remote_name)
                    .context("setting up mirror remote")?;
            } else {
                copy_refs_direct(&source.git_dir, &dest.git_dir).context("copying refs")?;
                setup_origin_remote_bare_url(&dest.git_dir, remote_url, &remote_name)
                    .context("setting up origin remote")?;
                if let Some(ref branch) = head_branch {
                    fs::write(
                        dest.git_dir.join("HEAD"),
                        format!("ref: refs/heads/{branch}\n"),
                    )?;
                } else if let Some(ref oid) = source_head_oid {
                    fs::write(dest.git_dir.join("HEAD"), format!("{oid}\n"))?;
                }
            }
        } else {
            let single_branch_name = if args.single_branch {
                Some(head_branch.as_deref().unwrap_or(initial_fallback.as_str()))
            } else {
                None
            };
            copy_refs_as_remote_filtered(
                &source.git_dir,
                &dest.git_dir,
                &remote_name,
                args.no_tags,
                single_branch_name,
            )
            .context("copying refs")?;
            let refspec = if args.single_branch {
                let branch = head_branch.as_deref().unwrap_or(initial_fallback.as_str());
                format!("+refs/heads/{branch}:refs/remotes/{remote_name}/{branch}")
            } else {
                format!("+refs/heads/*:refs/remotes/{remote_name}/*")
            };
            setup_origin_remote_url(&dest.git_dir, remote_url, &remote_name, &refspec)
                .context("setting up origin remote")?;

            setup_remote_tracking_head(
                &dest.git_dir,
                &remote_name,
                &source.git_dir,
                source_head_symref.as_deref(),
                source_head_oid.as_deref(),
            )?;

            let preferred_branch = head_branch.as_deref().unwrap_or(initial_fallback.as_str());
            if let Some(branch) =
                resolve_remote_tracked_branch_name(&dest.git_dir, &remote_name, preferred_branch)
            {
                let remote_ref_name = format!("refs/remotes/{remote_name}/{branch}");
                let oid = clone_read_direct_ref_oid(&dest.git_dir, &remote_ref_name)
                    .context("reading remote ref")?;
                let local_ref_name = format!("refs/heads/{branch}");
                clone_write_direct_ref(&dest.git_dir, &local_ref_name, &oid)?;
                fs::write(
                    dest.git_dir.join("HEAD"),
                    format!("ref: refs/heads/{branch}\n"),
                )?;
                setup_branch_tracking(&dest.git_dir, &branch, &remote_name)
                    .context("setting up branch tracking")?;
            } else if let Some(ref branch) = head_branch {
                if let Some(sr) = source_head_symref.as_deref() {
                    if sr == format!("refs/heads/{branch}")
                        && !clone_ref_file_exists(&source.git_dir, sr)
                    {
                        setup_branch_tracking(&dest.git_dir, branch, &remote_name)
                            .context("setting up branch tracking")?;
                    }
                }
            }
        }
    }

    if use_upload_for_protocol_v1 && crate::trace_packet::trace_packet_dest().is_some() {
        crate::trace_packet::trace_packet_line(b"clone> packfile negotiation complete");
    }

    apply_default_submodule_path_config_from_global(&dest.git_dir)?;
    if !args.config.is_empty() {
        apply_clone_config(&dest.git_dir, &args.config).context("applying -c config")?;
    }

    apply_submodule_reference_config_for_recursive_clone(&dest.git_dir, &args)?;

    apply_sticky_recursive_clone(&dest.git_dir, args.recurse_submodules())?;

    if args.no_tags {
        let config_path = dest.git_dir.join("config");
        let mut config = match ConfigFile::from_path(&config_path, ConfigScope::Local)? {
            Some(c) => c,
            None => ConfigFile::parse(&config_path, "", ConfigScope::Local)?,
        };
        config.set(&format!("remote.{remote_name}.tagOpt"), "--no-tags")?;
        config.write().context("writing config")?;
    }

    if args.revision.is_none() {
        if let Some(depth) = args.depth {
            if depth > 0 {
                write_shallow_boundary(&dest, depth)?;
            }
        }
    }

    if partial_blob_none {
        materialize_blob_none_partial_layout(&dest)
            .context("materializing partial-clone object layout")?;
        initialize_partial_clone_state(&source, &dest, &remote_name, "blob:none")
            .context("initializing partial-clone promisor state")?;
    } else if clone_filter_omits_root_trees(filter_spec.as_deref()) {
        let filter_spec = filter_spec.as_deref().unwrap_or("tree:0");
        let omitted = materialize_tree_zero_partial_layout(&dest)
            .context("materializing tree:0 partial-clone object layout")?;
        initialize_partial_clone_state_from_missing(&dest, &remote_name, filter_spec, omitted)
            .context("initializing tree:0 partial-clone promisor state")?;
    }

    if clone_filter_omits_root_trees(filter_spec.as_deref()) && !args.bare && !args.no_checkout {
        hydrate_tree_zero_checkout_objects(&dest).context("hydrating tree:0 checkout objects")?;
    }

    if partial_blob_none
        && matches!(filter_spec.as_deref(), Some("blob:none"))
        && !args.bare
        && !args.no_checkout
    {
        if grit_lib::refs::resolve_ref(&dest.git_dir, "HEAD").is_err() {
            crate::commands::promisor_hydrate::trim_promisor_marker_to_missing_local(&dest)
                .context("trimming promisor marker")?;
        } else {
            let dest_config = ConfigSet::load(Some(&dest.git_dir), true)?;
            let promisor = crate::commands::promisor_hydrate::find_promisor_source(
                &dest_config,
                &dest.git_dir,
            )?;
            if let Some(ref p) = promisor {
                if args.sparse {
                    let patterns = vec!["/*".to_string(), "!/*/".to_string()];
                    crate::commands::promisor_hydrate::hydrate_sparse_tip_blobs_from_promisor(
                        &dest, p, &patterns, true,
                    )
                    .context("hydrating sparse-checkout tip blobs")?;
                    let head_oid = grit_lib::refs::resolve_ref(&dest.git_dir, "HEAD")?;
                    let obj = dest.odb.read(&head_oid).context("reading HEAD for index")?;
                    let commit = parse_commit(&obj.data).context("parsing HEAD for index")?;
                    write_index_from_tree(&dest, &commit.tree)
                        .context("writing index for sparse clone")?;
                } else {
                    crate::commands::promisor_hydrate::hydrate_head_tree_blobs_from_promisor(
                        &dest, p,
                    )
                    .context("hydrating HEAD tree blobs")?;
                }
            }
            crate::commands::promisor_hydrate::trim_promisor_marker_to_missing_local(&dest)
                .context("trimming promisor marker")?;
        }
    }

    if partial_blob_none
        && !matches!(filter_spec.as_deref(), Some("blob:none"))
        && !read_promisor_missing_oids(&dest.git_dir).is_empty()
        && !crate::commands::promisor_hydrate::promisor_lazy_fetch_allowed_for_client_process()?
    {
        crate::commands::promisor_hydrate::warn_lazy_fetch_disabled_once();
        bail!("lazy fetching disabled");
    }

    if let Some(ref revision) = args.revision {
        let rev_oid = resolve_revision_for_clone(&source, revision, &remote_name)?;
        strip_refs_for_revision_clone(&dest.git_dir)?;
        fs::write(dest.git_dir.join("HEAD"), format!("{}\n", rev_oid))?;
        remove_revision_clone_remote_config(&dest.git_dir, &remote_name)?;
    }

    if let Some(depth) = args.depth {
        if depth > 0 {
            write_shallow_boundary(&dest, depth)?;
        }
    }

    maybe_print_local_clone_progress(args.progress);

    let skip_checkout_warn = !args.bare
        && !args.no_checkout
        && args.revision.is_none()
        && head_points_to_missing_ref(&dest)
        && !is_unborn_remote_default_checkout(
            &dest,
            source_head_symref.as_deref(),
            head_branch.as_deref(),
            &source.git_dir,
        );

    if !args.bare && !args.no_checkout {
        if skip_checkout_warn {
            eprintln!("warning: remote HEAD refers to nonexistent ref, unable to checkout");
        } else if head_points_to_missing_ref(&dest) {
            checkout_head_allow_unborn(&dest).context("checking out HEAD")?;
            run_post_checkout_after_clone(&dest)?;
        } else {
            checkout_head(&dest).context("checking out HEAD")?;
            run_post_checkout_after_clone(&dest)?;
        }
    }

    if args.sparse && !args.bare {
        crate::commands::sparse_checkout::finalize_sparse_clone(&dest, !args.no_checkout)?;
    }

    if !args.quiet && !args.no_checkout {
        eprintln!("done.");
    }

    if args.dissociate {
        dissociate_clone_repository(&dest.git_dir).context("dissociating from alternates")?;
    }

    if args.recurse_submodules() && !args.bare {
        if let Some(ref wt) = dest.work_tree {
            if let Err(e) = clone_submodules(wt, &dest, &args) {
                return Err(e).context("cloning submodules");
            }
        }
    }

    Ok(())
}

fn default_branch_from_upload_pack_advertisement(
    remote_heads: &[(String, ObjectId)],
    head_symref: Option<&str>,
    head_oid: Option<ObjectId>,
) -> Option<String> {
    if let Some(branch) = head_symref
        .and_then(|sym| sym.strip_prefix("refs/heads/"))
        .map(ToOwned::to_owned)
    {
        return Some(branch);
    }
    if let Some(head_oid) = head_oid {
        if let Some((name, _)) = remote_heads.iter().find(|(_, oid)| *oid == head_oid) {
            return name.strip_prefix("refs/heads/").map(ToOwned::to_owned);
        }
    }
    remote_heads
        .iter()
        .find_map(|(name, _)| name.strip_prefix("refs/heads/").map(ToOwned::to_owned))
}

fn run_ssh_network_clone(args: Args, spec: &crate::ssh_transport::SshUrl) -> Result<()> {
    if args.shared {
        bail!("--shared is not supported for SSH network clones");
    }
    if !args.reference.is_empty() || !args.reference_if_able.is_empty() {
        bail!("--reference is not supported for SSH network clones");
    }
    if args.revision.is_some() {
        bail!("--revision is not supported for SSH network clones");
    }

    let remote_name = resolve_remote_name(&args)?;
    let target_name = clone_target_dir_name(&args)?;
    let target_path = PathBuf::from(&target_name);
    if target_path.exists() && !path_is_empty_directory(&target_path) {
        bail!(
            "destination path '{}' already exists and is not an empty directory",
            target_path.display()
        );
    }
    if !args.quiet {
        if args.bare {
            eprintln!("Cloning into bare repository '{}'...", target_name);
        } else {
            eprintln!("Cloning into '{}'...", target_name);
        }
    }

    let initial_fallback = default_head_branch_fallback();
    let initial_branch = args
        .branch
        .as_deref()
        .unwrap_or(initial_fallback.as_str())
        .to_string();
    fs::create_dir_all(&target_path)
        .with_context(|| format!("cannot create directory '{}'", target_path.display()))?;
    let ref_storage = resolved_clone_ref_storage(&args)?;
    let template_dir = effective_template_dir(&args);
    let dest = if let Some(ref sep_git) = args.separate_git_dir {
        if sep_git.exists() && sep_git.read_dir()?.next().is_some() {
            bail!(
                "destination path '{}' already exists and is not an empty directory",
                sep_git.display()
            );
        }
        init_repository_separate_git_dir(
            &target_path,
            sep_git,
            &initial_branch,
            template_dir.as_deref(),
            ref_storage,
        )
        .with_context(|| {
            format!(
                "failed to initialize separate git dir '{}'",
                sep_git.display()
            )
        })?
    } else if args.bare && args.template.as_ref().is_some_and(|s| s.is_empty()) {
        init_bare_clone_minimal(&target_path, &initial_branch, ref_storage).with_context(|| {
            format!(
                "failed to initialize bare clone '{}'",
                target_path.display()
            )
        })?;
        Repository::open(&target_path, None)
            .with_context(|| format!("failed to open repository '{}'", target_path.display()))?
    } else {
        init_repository(
            &target_path,
            args.bare,
            &initial_branch,
            template_dir.as_deref(),
            ref_storage,
        )
        .with_context(|| format!("failed to initialize '{}'", target_path.display()))?
    };

    let refspec_for_fetch: Vec<String> = if args.single_branch {
        if let Some(ref branch) = args.branch {
            vec![format!("refs/heads/{branch}")]
        } else {
            Vec::new()
        }
    } else {
        Vec::new()
    };
    let filter_active = clone_pack_filter_active(&args, None);
    let fetch_result = crate::fetch_transport::with_packet_trace_identity("clone", || {
        crate::fetch_transport::fetch_via_ssh_upload_pack_skipping(
            &dest.git_dir,
            spec,
            args.upload_pack.as_deref(),
            &refspec_for_fetch,
            filter_active,
        )
    });
    let (remote_heads, remote_tags, head_symref, head_oid) = match fetch_result {
        Ok(result) => result,
        Err(e) => {
            let _ = fs::remove_dir_all(&target_path);
            if let Some(ref sep_git) = args.separate_git_dir {
                let _ = fs::remove_dir_all(sep_git);
            }
            return Err(e).with_context(|| format!("ssh clone failed for '{}'", args.repository));
        }
    };

    let default_branch = args.branch.clone().or_else(|| {
        default_branch_from_upload_pack_advertisement(
            &remote_heads,
            head_symref.as_deref(),
            head_oid,
        )
    });

    if args.bare {
        for (refname, oid) in &remote_heads {
            clone_write_direct_ref(&dest.git_dir, refname, &oid.to_hex())?;
        }
        if !args.no_tags {
            for (refname, oid) in &remote_tags {
                clone_write_direct_ref(&dest.git_dir, refname, &oid.to_hex())?;
            }
        }
        setup_origin_remote_bare_url(&dest.git_dir, &args.repository, &remote_name)?;
        if let Some(branch) = default_branch {
            fs::write(
                dest.git_dir.join("HEAD"),
                format!("ref: refs/heads/{branch}\n"),
            )?;
        } else if let Some(oid) = head_oid {
            fs::write(dest.git_dir.join("HEAD"), format!("{}\n", oid.to_hex()))?;
        }
    } else {
        for (refname, oid) in &remote_heads {
            let branch = refname.strip_prefix("refs/heads/").unwrap_or(refname);
            let dst_ref = dest
                .git_dir
                .join("refs/remotes")
                .join(&remote_name)
                .join(branch);
            if let Some(parent) = dst_ref.parent() {
                fs::create_dir_all(parent)?;
            }
            fs::write(&dst_ref, format!("{}\n", oid.to_hex()))?;
        }
        if !args.no_tags {
            for (refname, oid) in &remote_tags {
                clone_write_direct_ref(&dest.git_dir, refname, &oid.to_hex())?;
            }
        }
        let refspec = if args.single_branch {
            let branch = default_branch
                .as_deref()
                .unwrap_or(initial_fallback.as_str());
            format!("+refs/heads/{branch}:refs/remotes/{remote_name}/{branch}")
        } else {
            format!("+refs/heads/*:refs/remotes/{remote_name}/*")
        };
        setup_origin_remote_url(&dest.git_dir, &args.repository, &remote_name, &refspec)?;
        if let Some(ref branch) = default_branch {
            let origin_head = dest
                .git_dir
                .join("refs/remotes")
                .join(&remote_name)
                .join("HEAD");
            if let Some(parent) = origin_head.parent() {
                fs::create_dir_all(parent)?;
            }
            fs::write(
                &origin_head,
                format!("ref: refs/remotes/{remote_name}/{branch}\n"),
            )?;
            let remote_ref = dest
                .git_dir
                .join("refs/remotes")
                .join(&remote_name)
                .join(branch);
            if remote_ref.exists() {
                let oid = fs::read_to_string(&remote_ref)
                    .context("reading remote-tracking ref")?
                    .trim()
                    .to_string();
                let local_ref_path = dest.git_dir.join("refs/heads").join(branch);
                if let Some(parent) = local_ref_path.parent() {
                    fs::create_dir_all(parent)?;
                }
                fs::write(&local_ref_path, format!("{oid}\n"))?;
                fs::write(
                    dest.git_dir.join("HEAD"),
                    format!("ref: refs/heads/{branch}\n"),
                )?;
                setup_branch_tracking(&dest.git_dir, branch, &remote_name)
                    .context("setting up branch tracking")?;
            }
        }
    }

    apply_default_submodule_path_config_from_global(&dest.git_dir)?;
    if !args.config.is_empty() {
        apply_clone_config(&dest.git_dir, &args.config).context("applying -c config")?;
    }
    apply_sticky_recursive_clone(&dest.git_dir, args.recurse_submodules())?;
    if args.no_tags {
        let config_path = dest.git_dir.join("config");
        let mut config = match ConfigFile::from_path(&config_path, ConfigScope::Local)? {
            Some(c) => c,
            None => ConfigFile::parse(&config_path, "", ConfigScope::Local)?,
        };
        config.set(&format!("remote.{remote_name}.tagOpt"), "--no-tags")?;
        config.write().context("writing config")?;
    }

    if !args.bare && !args.no_checkout {
        if head_points_to_missing_ref(&dest) {
            checkout_head_allow_unborn(&dest).context("checking out HEAD")?;
        } else {
            checkout_head(&dest).context("checking out HEAD")?;
        }
        run_post_checkout_after_clone(&dest)?;
    }
    if args.sparse && !args.bare {
        crate::commands::sparse_checkout::init_clone_sparse_checkout(&dest, !args.no_checkout)
            .context("initializing sparse-checkout")?;
        crate::commands::sparse_checkout::finalize_sparse_clone(&dest, !args.no_checkout)?;
    }
    if !args.quiet && !args.no_checkout {
        eprintln!("done.");
    }
    if args.recurse_submodules() && !args.bare {
        if let Some(ref wt) = dest.work_tree {
            clone_submodules(wt, &dest, &args).context("cloning submodules")?;
        }
    }
    Ok(())
}

/// Clone submodules listed in .gitmodules.
///
/// Reads `.gitmodules` from the work tree, resolves each submodule's URL
/// (relative paths are resolved against the **source repository root** so `../sub` from
/// `trash/various/.gitmodules` resolves next to `various/`),
/// and uses `grit clone` to clone each submodule.
/// Paths that are gitlink (submodule) entries in the current `HEAD` tree.
fn gitlink_paths_at_head(work_tree: &Path) -> Result<HashSet<String>> {
    let git_dir = work_tree.join(".git");
    let repo = Repository::open(&git_dir, Some(work_tree))?;
    let oid = refs::resolve_ref(&repo.git_dir, "HEAD").context("resolving HEAD")?;
    let obj = repo.odb.read(&oid).context("reading HEAD commit")?;
    let commit = parse_commit(&obj.data).context("parsing HEAD commit")?;
    let mut out = HashSet::new();
    collect_gitlink_paths(&repo, &commit.tree, "", &mut out)?;
    Ok(out)
}

fn collect_gitlink_paths(
    repo: &Repository,
    tree_oid: &ObjectId,
    prefix: &str,
    out: &mut HashSet<String>,
) -> Result<()> {
    let obj = repo.odb.read(tree_oid).context("reading tree")?;
    let entries = parse_tree(&obj.data).context("parsing tree")?;
    for entry in &entries {
        let name = String::from_utf8_lossy(&entry.name).into_owned();
        let path = if prefix.is_empty() {
            name
        } else {
            format!("{prefix}/{name}")
        };
        let is_tree = (entry.mode & 0o170000) == 0o040000;
        if entry.mode == 0o160000 {
            out.insert(path);
        } else if is_tree {
            collect_gitlink_paths(repo, &entry.oid, &path, out)?;
        }
    }
    Ok(())
}

fn clone_with_optional_superproject_refs(
    grit_bin: &Path,
    resolved_url: &str,
    work_dest: &Path,
    extra_refs: &[PathBuf],
    quiet: bool,
    separate_git_dir: Option<&Path>,
    no_checkout: bool,
    depth: Option<usize>,
    ref_storage: Option<&str>,
    filter_spec: Option<&str>,
    single_branch: bool,
) -> Result<std::process::ExitStatus> {
    let run = |with_refs: bool| -> Result<std::process::ExitStatus> {
        let mut cmd = std::process::Command::new(grit_bin);
        crate::grit_exe::strip_trace2_env(&mut cmd);
        cmd.arg("clone").arg("-c").arg("protocol.file.allow=always");
        if with_refs {
            for r in extra_refs {
                cmd.arg("--reference").arg(r);
            }
        }
        if let Some(d) = depth {
            cmd.arg("--depth").arg(d.to_string());
        }
        if let Some(format) = ref_storage {
            cmd.arg(format!("--ref-format={format}"));
        }
        if let Some(spec) = filter_spec {
            cmd.arg("--filter").arg(spec);
        }
        if single_branch {
            cmd.arg("--single-branch");
        }
        if no_checkout {
            cmd.arg("--no-checkout");
        }
        if let Some(git_dir) = separate_git_dir {
            cmd.arg("--separate-git-dir").arg(git_dir);
        }
        cmd.arg(resolved_url).arg(work_dest);
        if quiet {
            cmd.arg("-q");
        }
        cmd.status()
            .with_context(|| format!("failed to spawn clone for {}", work_dest.display()))
    };

    let mut status = run(!extra_refs.is_empty())?;
    if !status.success() && !extra_refs.is_empty() {
        if work_dest.exists() {
            let _ = fs::remove_dir_all(work_dest);
        }
        if let Some(md) = separate_git_dir {
            if md.exists() {
                let _ = fs::remove_dir_all(md);
            }
        }
        status = run(false)?;
    }
    Ok(status)
}

#[derive(Clone)]
struct SubmoduleCloneJob {
    resolved_url: String,
    extra_refs: Vec<PathBuf>,
    alternate_strategy: Option<String>,
    modules_dir: PathBuf,
    sub_dest: PathBuf,
    depth: Option<usize>,
    overlay_existing: bool,
    ref_storage: Option<String>,
    filter_spec: Option<String>,
    single_branch: bool,
}

fn clone_submodules(work_tree: &Path, repo: &Repository, clone_args: &Args) -> Result<()> {
    let quiet = clone_args.quiet;
    crate::commands::submodule::trace_submodule_job_tasks_if_needed(Some(repo), clone_args.jobs);
    let gitmodules_path = work_tree.join(".gitmodules");
    if !gitmodules_path.exists() {
        return Ok(());
    }

    let modules =
        crate::commands::submodule::parse_gitmodules_for_clone(work_tree).unwrap_or_default();

    let grit_bin = crate::grit_exe::grit_executable();
    let store = refs::common_dir(&repo.git_dir).unwrap_or_else(|| repo.git_dir.clone());
    let mut super_cfg = {
        let config_path = repo.git_dir.join("config");
        let content = fs::read_to_string(&config_path).unwrap_or_default();
        ConfigFile::parse(&config_path, &content, ConfigScope::Local)?
    };

    // `git clone --recurse-submodules=<pathspec>` records the pathspec(s) in `submodule.active`
    // and only initializes the submodules that match. Persist them before deciding what to clone.
    if let Some(specs) = clone_args.recurse_submodules_pathspecs() {
        for (i, spec) in specs.iter().enumerate() {
            if i == 0 {
                super_cfg.set("submodule.active", spec)?;
            } else {
                super_cfg.add_value("submodule.active", spec)?;
            }
        }
        super_cfg
            .write()
            .context("writing submodule.active during clone")?;
        super_cfg = {
            let config_path = repo.git_dir.join("config");
            let content = fs::read_to_string(&config_path).unwrap_or_default();
            ConfigFile::parse(&config_path, &content, ConfigScope::Local)?
        };
    }
    let active_filter = clone_args.recurse_submodules_pathspecs().is_some();

    // Only clone paths that are submodules at the checked-out commit. `.gitmodules` can list paths
    // that are plain files on this branch (e.g. `f` as submodule on B1 but `f/f` as file on B2);
    // cloning would delete the checked-out tree (`t2080` clone + verify_checkout).
    let gitlink_paths = gitlink_paths_at_head(work_tree).unwrap_or_default();

    let super_shallow = repo.git_dir.join("shallow").is_file();
    let submodule_ref_storage = if grit_lib::reftable::is_reftable_repo(&repo.git_dir) {
        Some("reftable".to_owned())
    } else {
        clone_args.ref_format.clone()
    };
    let mut jobs: Vec<SubmoduleCloneJob> = Vec::new();

    for sm in &modules {
        let sub_dest = work_tree.join(&sm.path);
        let overlay_existing = !gitlink_paths.contains(&sm.path)
            && fs::symlink_metadata(&sub_dest)
                .map(|m| m.is_dir() && !m.file_type().is_symlink())
                .unwrap_or(false);
        if !gitlink_paths.contains(&sm.path) && !overlay_existing {
            continue;
        }
        // With explicit `--recurse-submodules=<pathspec>`, only clone active submodules.
        if active_filter && !crate::commands::submodule::submodule_path_is_active(repo, &sm.path) {
            continue;
        }

        let resolved_url = crate::commands::submodule::resolve_submodule_super_url(
            work_tree,
            &repo.git_dir,
            &sm.url,
        )?;

        let extra_refs = crate::commands::submodule::superproject_submodule_reference_roots(
            &repo.git_dir,
            &sm.name,
        )?;
        let alternate_strategy = if extra_refs.is_empty() {
            None
        } else {
            crate::commands::submodule::superproject_submodule_alternate_error_strategy(
                &repo.git_dir,
            )?
        };

        if submodule_path_config_enabled(&store) {
            ensure_submodule_gitdir_config(work_tree, &store, &mut super_cfg, &sm.name)
                .context("submodule gitdir config during clone")?;
        }

        let modules_dir = submodule_separate_git_dir(repo, work_tree, &sm.name, &sm.path)
            .context("resolve submodule separate git dir during clone")?;
        if let Some(outer) = submodule_gitdir_outer_conflict(&modules_dir, &sm.name) {
            anyhow::bail!(
                "fatal: submodule git dir '{}' is inside git dir '{}'",
                modules_dir.display(),
                outer.display()
            );
        }
        if let Some(parent) = modules_dir.parent() {
            fs::create_dir_all(parent)?;
        }
        if modules_dir.exists() {
            let _ = fs::remove_dir_all(&modules_dir);
        }

        if !quiet {
            eprintln!("Cloning into '{}'...", sub_dest.display());
        }

        if !overlay_existing {
            // Remove placeholder path (directory, empty dir, or symlink left from a type-change branch).
            if let Ok(meta) = fs::symlink_metadata(&sub_dest) {
                if meta.file_type().is_symlink() || meta.is_file() {
                    let _ = fs::remove_file(&sub_dest);
                } else {
                    let _ = fs::remove_dir_all(&sub_dest);
                }
            }
        }

        validate_submodule_path(work_tree, &sm.path).map_err(|e| anyhow::anyhow!("{e}"))?;

        let depth = crate::commands::submodule::submodule_clone_depth_for_superproject(
            super_shallow,
            clone_args.shallow_submodules,
            clone_args.no_shallow_submodules,
            false,
            sm.shallow,
        );

        jobs.push(SubmoduleCloneJob {
            resolved_url,
            extra_refs,
            alternate_strategy,
            modules_dir,
            sub_dest,
            depth,
            overlay_existing,
            ref_storage: submodule_ref_storage.clone(),
            filter_spec: submodule_filter_enabled(clone_args)
                .then(|| clone_filter_spec(clone_args))
                .flatten(),
            single_branch: clone_args.single_branch,
        });
    }

    if jobs.is_empty() {
        return Ok(());
    }

    let n_workers = clone_args.jobs.unwrap_or(1).max(1).min(jobs.len());
    if n_workers <= 1 {
        for j in jobs {
            if j.overlay_existing {
                overlay_clone_submodule_contents(
                    &grit_bin,
                    &j.resolved_url,
                    &j.sub_dest,
                    quiet,
                    j.depth,
                )?;
                continue;
            }
            let status = clone_with_optional_superproject_refs(
                &grit_bin,
                &j.resolved_url,
                &j.sub_dest,
                &j.extra_refs,
                quiet,
                Some(&j.modules_dir),
                true,
                j.depth,
                j.ref_storage.as_deref(),
                j.filter_spec.as_deref(),
                j.single_branch,
            )?;
            if !status.success() {
                anyhow::bail!(
                    "clone of '{}' into submodule path '{}' failed",
                    j.resolved_url,
                    j.sub_dest.display()
                );
            }
            set_submodule_core_worktree_after_separate_clone(
                &grit_bin,
                &j.modules_dir,
                &j.sub_dest,
            );
            // Rewrite the absolute `gitdir:` left by `clone --separate-git-dir` into a relative
            // gitlink (C Git's `connect_work_tree_and_git_dir`), so a copied/moved superproject
            // keeps its submodule pointing at the copy's git dir (t7406).
            write_submodule_gitfile(&j.sub_dest, &j.modules_dir)
                .map_err(|e| anyhow::anyhow!("{e}"))?;
            if let Some(strategy) = j.alternate_strategy.as_deref() {
                crate::commands::submodule::write_submodule_alternate_inheritance_config(
                    &j.modules_dir,
                    strategy,
                )?;
            }
        }
    } else {
        let mut handles = Vec::new();
        let chunk_size = (jobs.len() + n_workers - 1) / n_workers;
        for chunk in jobs.chunks(chunk_size.max(1)) {
            let chunk: Vec<SubmoduleCloneJob> = chunk.to_vec();
            let grit = grit_bin.clone();
            let quiet_c = quiet;
            handles.push(thread::spawn(move || -> Result<(), String> {
                for j in chunk {
                    if j.overlay_existing {
                        overlay_clone_submodule_contents(
                            &grit,
                            &j.resolved_url,
                            &j.sub_dest,
                            quiet_c,
                            j.depth,
                        )
                        .map_err(|e| e.to_string())?;
                        continue;
                    }
                    let st = clone_with_optional_superproject_refs(
                        &grit,
                        &j.resolved_url,
                        &j.sub_dest,
                        &j.extra_refs,
                        quiet_c,
                        Some(&j.modules_dir),
                        true,
                        j.depth,
                        j.ref_storage.as_deref(),
                        j.filter_spec.as_deref(),
                        j.single_branch,
                    );
                    match st {
                        Ok(s) if s.success() => {
                            set_submodule_core_worktree_after_separate_clone(
                                &grit,
                                &j.modules_dir,
                                &j.sub_dest,
                            );
                            // Relative gitlink (see single-worker branch / C `connect_work_tree_and_git_dir`).
                            write_submodule_gitfile(&j.sub_dest, &j.modules_dir)
                                .map_err(|e| e.to_string())?;
                            if let Some(strategy) = j.alternate_strategy.as_deref() {
                                crate::commands::submodule::write_submodule_alternate_inheritance_config(
                                    &j.modules_dir,
                                    strategy,
                                )
                                .map_err(|e| e.to_string())?;
                            }
                        }
                        Ok(_) => {
                            return Err(format!(
                                "clone of '{}' into submodule path '{}' failed",
                                j.resolved_url,
                                j.sub_dest.display()
                            ));
                        }
                        Err(e) => return Err(e.to_string()),
                    }
                }
                Ok(())
            }));
        }
        let mut first_err = None;
        for h in handles {
            match h.join() {
                Ok(Ok(())) => {}
                Ok(Err(e)) => {
                    first_err.get_or_insert(e);
                }
                Err(_) => {
                    first_err.get_or_insert("submodule clone thread panicked".to_string());
                }
            }
        }
        if let Some(e) = first_err {
            anyhow::bail!("{e}");
        }
    }

    let mut upd = std::process::Command::new(&grit_bin);
    crate::grit_exe::strip_trace2_env(&mut upd);
    upd.args(["submodule", "update", "--init", "--recursive", "--force"])
        .current_dir(work_tree);
    if quiet {
        upd.arg("--quiet");
    }
    if let Some(n) = clone_args.jobs {
        upd.arg("--jobs").arg(n.to_string());
    }
    if clone_args.remote_submodules && !clone_args.no_remote_submodules {
        upd.arg("--remote");
    }
    if submodule_filter_enabled(clone_args) {
        if let Some(spec) = clone_filter_spec(clone_args) {
            upd.arg("--filter").arg(spec);
        }
    }
    if clone_args.shallow_submodules {
        upd.env(
            crate::commands::submodule::CLONE_SHALLOW_SUBMODULES_ENV,
            "1",
        );
    } else {
        upd.env_remove(crate::commands::submodule::CLONE_SHALLOW_SUBMODULES_ENV);
    }
    if clone_args.no_shallow_submodules {
        upd.env(
            crate::commands::submodule::CLONE_NO_SHALLOW_SUBMODULES_ENV,
            "1",
        );
    } else {
        upd.env_remove(crate::commands::submodule::CLONE_NO_SHALLOW_SUBMODULES_ENV);
    }
    let status = upd.status().context("submodule update after clone")?;
    if !status.success() {
        bail!("submodule update failed after clone");
    }

    Ok(())
}

fn submodule_filter_enabled(clone_args: &Args) -> bool {
    if clone_args.no_also_filter_submodules {
        return false;
    }
    if clone_args.also_filter_submodules {
        return true;
    }
    ConfigSet::load(None, true)
        .ok()
        .and_then(|cfg| {
            cfg.get_bool("clone.filtersubmodules")
                .or_else(|| cfg.get_bool("clone.filterSubmodules"))
                .and_then(Result::ok)
        })
        .unwrap_or(false)
}

fn overlay_clone_submodule_contents(
    grit_bin: &Path,
    resolved_url: &str,
    sub_dest: &Path,
    quiet: bool,
    depth: Option<usize>,
) -> Result<()> {
    let parent = sub_dest.parent().unwrap_or_else(|| Path::new("."));
    let name = sub_dest
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "submodule".to_string());
    let tmp = parent.join(format!(
        ".grit-submodule-overlay-{}-{name}",
        std::process::id()
    ));
    let _ = fs::remove_dir_all(&tmp);
    let status = clone_with_optional_superproject_refs(
        grit_bin,
        resolved_url,
        &tmp,
        &[],
        quiet,
        None,
        false,
        depth,
        None,
        None,
        false,
    )?;
    if !status.success() {
        let _ = fs::remove_dir_all(&tmp);
        anyhow::bail!("clone of '{resolved_url}' into temporary submodule overlay failed");
    }
    copy_overlay_contents(&tmp, sub_dest)?;
    let _ = fs::remove_dir_all(&tmp);
    Ok(())
}

fn copy_overlay_contents(src: &Path, dst: &Path) -> Result<()> {
    for entry in fs::read_dir(src)? {
        let entry = entry?;
        let from = entry.path();
        let to = dst.join(entry.file_name());
        let meta = fs::symlink_metadata(&from)?;
        if meta.file_type().is_symlink() {
            let target = fs::read_link(&from)?;
            let _ = fs::remove_file(&to);
            std::os::unix::fs::symlink(target, &to)?;
        } else if meta.is_dir() {
            fs::create_dir_all(&to)?;
            copy_overlay_contents(&from, &to)?;
        } else {
            fs::copy(&from, &to)?;
        }
    }
    Ok(())
}

/// Extract a remote URL from config content.
fn extract_remote_url(config: &str, remote_name: &str) -> Option<String> {
    let section = format!("[remote \"{}\"]", remote_name);
    let mut in_section = false;
    for line in config.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with('[') {
            in_section = trimmed.starts_with(&section);
            continue;
        }
        if in_section {
            if let Some(val) = trimmed
                .strip_prefix("url = ")
                .or_else(|| trimmed.strip_prefix("url="))
            {
                return Some(val.trim().to_string());
            }
        }
    }
    None
}

/// Resolve `--revision` in the source repo to a **commit** OID (hex).
///
/// Stricter than general `rev-parse`: parent/ancestor syntax (`^`, `~`) is not
/// accepted (Git's transport treats the revision as a refspec-like name).
fn resolve_revision_for_clone(
    source: &Repository,
    revision: &str,
    remote_name: &str,
) -> Result<String> {
    if revision.contains('^') || revision.contains('~') {
        bail!("fatal: Remote revision {revision} not found in upstream {remote_name}");
    }

    let git_dir = &source.git_dir;

    let oid = if revision == "HEAD" {
        refs::resolve_ref(git_dir, "HEAD").map_err(|_| {
            anyhow::anyhow!("fatal: Remote revision HEAD not found in upstream {remote_name}")
        })?
    } else if let Ok(oid) = revision.parse::<ObjectId>() {
        // Only full 40-hex IDs are accepted as raw object names (matches git's
        // `--revision` transport behaviour; short OIDs are not resolved).
        if revision.len() != 40 {
            bail!("fatal: Remote revision {revision} not found in upstream {remote_name}");
        }
        if !source.odb.exists(&oid) {
            bail!("fatal: Remote revision {revision} not found in upstream {remote_name}");
        }
        oid
    } else if let Ok(oid) = refs::resolve_ref(git_dir, revision) {
        oid
    } else if let Ok(oid) = refs::resolve_ref(git_dir, &format!("refs/heads/{revision}")) {
        oid
    } else if let Ok(oid) = refs::resolve_ref(git_dir, &format!("refs/tags/{revision}")) {
        oid
    } else if let Ok(oid) = refs::resolve_ref(git_dir, &format!("refs/remotes/{revision}")) {
        oid
    } else if looks_like_hex_object_id(revision) {
        bail!("fatal: Remote revision {revision} not found in upstream {remote_name}");
    } else {
        bail!("cannot resolve --revision '{revision}'");
    };

    let commit_oid = peel_revision_to_commit(source, oid)?;
    Ok(commit_oid.to_hex())
}

/// Peel tags until `oid` names a commit; error if the result is a tree or blob.
fn peel_revision_to_commit(source: &Repository, mut oid: ObjectId) -> Result<ObjectId> {
    loop {
        let obj = source.odb.read(&oid)?;
        match obj.kind {
            ObjectKind::Commit => return Ok(oid),
            ObjectKind::Tag => {
                let tag = parse_tag(&obj.data)?;
                oid = tag.object;
            }
            ObjectKind::Tree => {
                bail!("object {} is a tree, not a commit", oid.to_hex());
            }
            ObjectKind::Blob => {
                bail!("object {} is a blob, not a commit", oid.to_hex());
            }
        }
    }
}

fn looks_like_hex_object_id(s: &str) -> bool {
    let len = s.len();
    (4..=40).contains(&len) && s.chars().all(|c| c.is_ascii_hexdigit())
}

/// Remove every ref under `refs/` (files and empty dirs), for revision-only clones.
fn strip_refs_under(refs_root: &Path) -> Result<()> {
    if !refs_root.exists() {
        return Ok(());
    }
    for entry in fs::read_dir(refs_root)? {
        let entry = entry?;
        let path = entry.path();
        if entry.file_type()?.is_dir() {
            strip_refs_under(&path)?;
            let _ = fs::remove_dir(&path);
        } else {
            let _ = fs::remove_file(&path);
        }
    }
    Ok(())
}

/// Drop `remote.<name>.fetch` and `branch.*` sections left over from init / clone setup.
fn remove_revision_clone_remote_config(git_dir: &Path, remote_name: &str) -> Result<()> {
    let config_path = git_dir.join("config");
    let mut config = match ConfigFile::from_path(&config_path, ConfigScope::Local)? {
        Some(c) => c,
        None => ConfigFile::parse(&config_path, "", ConfigScope::Local)?,
    };
    let _ = config.unset(&format!("remote.{remote_name}.fetch"))?;
    let to_remove: Vec<String> = config
        .entries
        .iter()
        .filter(|e| e.key.starts_with("branch.") && e.key.contains(".remote"))
        .filter(|e| e.value.as_deref() == Some(remote_name))
        .filter_map(|e| e.key.rsplit_once('.').map(|(prefix, _)| prefix.to_string()))
        .collect();
    for branch_sec in to_remove {
        let _ = config.remove_section(&branch_sec)?;
    }
    config.write().context("writing config")?;
    Ok(())
}

/// Percent-decode `file://` path components (Git `url_decode` for URL-shaped sources).
fn percent_decode_file_url_path(path: &str) -> Result<String> {
    let mut out = String::with_capacity(path.len());
    let mut chars = path.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '%' {
            let h1 = chars
                .next()
                .ok_or_else(|| anyhow::anyhow!("bad % escape in file URL"))?;
            let h2 = chars
                .next()
                .ok_or_else(|| anyhow::anyhow!("bad % escape in file URL"))?;
            let byte = u8::from_str_radix(&format!("{h1}{h2}"), 16)
                .map_err(|_| anyhow::anyhow!("bad % escape in file URL"))?;
            out.push(byte as char);
        } else {
            out.push(c);
        }
    }
    Ok(out)
}

/// True when `path` names an existing empty directory (clone may use it as destination).
fn path_is_empty_directory(path: &Path) -> bool {
    path.is_dir()
        && path
            .read_dir()
            .map(|mut d| d.next().is_none())
            .unwrap_or(false)
}

/// Recursively remove the contents of `dir` but keep `dir` itself.
///
/// Mirrors `remove_dir_recursively(..., REMOVE_DIR_KEEP_TOPLEVEL)` from upstream Git: every entry
/// inside `dir` (files and nested directories) is deleted, but the top-level directory is left in
/// place. Used when a clone fails into a directory the user created beforehand — Git only removes
/// what it wrote, leaving the pre-existing (empty) directory behind.
fn remove_dir_contents_keep_toplevel(dir: &Path) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let is_dir = entry
            .file_type()
            .map(|ft| ft.is_dir())
            .unwrap_or_else(|_| path.is_dir());
        if is_dir {
            let _ = fs::remove_dir_all(&path);
        } else {
            let _ = fs::remove_file(&path);
        }
    }
}

/// Remove a junk path created during a failed clone.
///
/// When `keep_toplevel` is true the directory itself is preserved and only its contents are
/// removed (the directory pre-existed, so the user owns it); otherwise the whole path is deleted.
fn remove_junk_path(path: &Path, keep_toplevel: bool) {
    if keep_toplevel {
        remove_dir_contents_keep_toplevel(path);
    } else {
        let _ = fs::remove_dir_all(path);
    }
}

/// What a [`JunkGuard`] removes when dropped, mirroring upstream Git's `enum junk_mode`
/// (builtin/clone.c).
#[derive(Clone, Copy, PartialEq, Eq)]
enum JunkMode {
    /// Remove both the git dir and work tree (the default while the repo is still being set up).
    LeaveNone,
    /// Leave the partially populated repository on disk; only warn the user. Git switches to this
    /// mode immediately before the worktree checkout, so a checkout failure (e.g. a corrupt object)
    /// keeps `<dest>/.git` and the index around for inspection instead of deleting everything.
    LeaveRepo,
    /// Leave everything in place (the clone succeeded). Equivalent to disarming the guard.
    LeaveAll,
}

/// RAII cleanup guard mirroring upstream Git's `remove_junk` (builtin/clone.c).
///
/// On a failed clone, Git removes the git dir and work tree it created so the user does not have to
/// clean up manually before retrying. If those directories already existed (empty) before the
/// clone, only their contents are removed — the pre-existing directory is preserved.
///
/// The guard runs cleanup when dropped while in [`JunkMode::LeaveNone`]. Once the clone reaches the
/// worktree checkout phase, [`JunkGuard::leave_repo`] switches it to [`JunkMode::LeaveRepo`] so a
/// checkout failure leaves the repository intact (matching Git's `junk_mode = JUNK_LEAVE_REPO`).
/// On success [`JunkGuard::disarm`] selects [`JunkMode::LeaveAll`] (Git's `JUNK_LEAVE_ALL`).
struct JunkGuard {
    /// Work tree directory to remove (absent for bare clones).
    work_tree: Option<PathBuf>,
    /// Keep the work tree's top-level directory (it pre-existed and was empty).
    work_tree_keep_toplevel: bool,
    /// Git directory to remove (e.g. `<dir>/.git`, the bare `<dir>`, or the separate git dir).
    git_dir: Option<PathBuf>,
    /// Keep the git dir's top-level directory (it pre-existed and was empty).
    git_dir_keep_toplevel: bool,
    /// Current cleanup mode. See [`JunkMode`].
    mode: JunkMode,
}

impl JunkGuard {
    /// Disarm the guard so dropping it performs no cleanup (the clone succeeded).
    fn disarm(&mut self) {
        self.mode = JunkMode::LeaveAll;
    }

    /// Switch to "leave the repo" cleanup: a later failure keeps the partial repository on disk and
    /// only warns. Called before the worktree checkout so a corrupt-object checkout failure does not
    /// delete `<dest>/.git` (upstream `junk_mode = JUNK_LEAVE_REPO`, t1060).
    fn leave_repo(&mut self) {
        self.mode = JunkMode::LeaveRepo;
    }
}

impl Drop for JunkGuard {
    fn drop(&mut self) {
        match self.mode {
            JunkMode::LeaveAll => return,
            JunkMode::LeaveRepo => {
                eprintln!(
                    "warning: Clone succeeded, but checkout failed.\n\
                     You can inspect what was checked out with 'git status'\n\
                     and retry with 'git restore --source=HEAD :/'"
                );
                return;
            }
            JunkMode::LeaveNone => {}
        }
        // Upstream removes the git dir first, then the work tree. For a normal (non-separate)
        // clone the git dir lives inside the work tree, so removing the work tree afterwards
        // (keeping its top level when it pre-existed) also clears the leftover `.git` directory.
        if let Some(ref git_dir) = self.git_dir {
            remove_junk_path(git_dir, self.git_dir_keep_toplevel);
        }
        if let Some(ref work_tree) = self.work_tree {
            remove_junk_path(work_tree, self.work_tree_keep_toplevel);
        }
    }
}

fn uploadpack_filter_allowed(git_dir: &Path) -> bool {
    let set = ConfigSet::load(Some(git_dir), true).unwrap_or_default();
    matches!(
        set.get_bool("uploadpack.allowfilter")
            .or_else(|| set.get_bool("uploadPack.allowFilter")),
        Some(Ok(true))
    )
}

fn clone_filter_spec(args: &Args) -> Option<String> {
    let mut filters: Vec<String> = args
        .filter
        .iter()
        .map(|s| s.trim())
        .filter(|s| !s.is_empty())
        .map(ToOwned::to_owned)
        .collect();
    if filters.is_empty() {
        return None;
    }
    if filters.len() == 1 {
        return filters.pop();
    }
    Some(format!("combine:{}", filters.join("+")))
}

/// True when a non-empty `--filter` was passed and the upload-pack source is known to allow
/// filtering (or unknown, for transports without a local repo path).
fn clone_pack_filter_active(args: &Args, source_git_dir: Option<&Path>) -> bool {
    if clone_filter_spec(args).is_none() {
        return false;
    }
    match source_git_dir {
        Some(gd) => uploadpack_filter_allowed(gd),
        None => true,
    }
}

fn maybe_trace_index_pack_fsck_for_filtered_clone(filter_active: bool) {
    if !filter_active || !transfer_fsck_objects_enabled() {
        return;
    }
    trace_run_command_git_invocation(&["index-pack", "--stdin", "--fix-thin", "--fsck-objects"]);
}

fn transfer_fsck_objects_enabled() -> bool {
    let config = ConfigSet::load(None, true).unwrap_or_default();
    matches!(
        config
            .get_bool("transfer.fsckobjects")
            .or_else(|| config.get_bool("transfer.fsckObjects")),
        Some(Ok(true))
    )
}

/// Whether a bundle-source fetch/clone should fsck its objects.
///
/// Mirrors git's `fetch_pack_fsck_objects`: `fetch.fsckObjects` wins when set,
/// otherwise `transfer.fsckObjects`, defaulting to off. (`fetch.fsckObjects=false`
/// therefore overrides `transfer.fsckObjects=true`.)
fn bundle_fetch_fsck_enabled() -> bool {
    let config = ConfigSet::load(None, true).unwrap_or_default();
    let fetch = config
        .get_bool("fetch.fsckobjects")
        .or_else(|| config.get_bool("fetch.fsckObjects"));
    if let Some(Ok(v)) = fetch {
        return v;
    }
    matches!(
        config
            .get_bool("transfer.fsckobjects")
            .or_else(|| config.get_bool("transfer.fsckObjects")),
        Some(Ok(true))
    )
}

/// Read per-message fsck severity overrides from `fetch.fsck.<msg-id>`.
///
/// Returns a lowercase-keyed map of message id to severity string (e.g.
/// `"missingemail" -> "ignore"`). Mirrors git's `fetch.fsck.*` handling: a value
/// of `ignore` demotes the check, anything else (`warn`, `error`, …) keeps it
/// fatal for our purposes (we only distinguish ignore vs. not).
fn bundle_fetch_fsck_severities() -> std::collections::HashMap<String, String> {
    let config = ConfigSet::load(None, true).unwrap_or_default();
    let mut map = std::collections::HashMap::new();
    for entry in config.entries() {
        if let Some(msg_id) = entry.key.strip_prefix("fetch.fsck.") {
            if msg_id == "skiplist" {
                continue;
            }
            if let Some(value) = entry.value.as_deref() {
                map.insert(msg_id.to_ascii_lowercase(), value.to_ascii_lowercase());
            }
        }
    }
    map
}

/// fsck every object in a bundle's pack and fail on the first non-ignored issue.
///
/// `index-pack --fsck-objects` reports `error: object <oid>: <id>: <detail>` for a
/// malformed object and exits non-zero; the t5607 test only requires the message
/// id (e.g. `missingEmail`) to appear on stderr. A `fetch.fsck.<id>=ignore`
/// override demotes the matching check so the clone still succeeds.
fn bundle_clone_fsck_pack(pack_data: &[u8], odb: &grit_lib::odb::Odb) -> Result<()> {
    let objects = grit_lib::unpack_objects::pack_bytes_to_object_map(pack_data, odb)
        .map_err(|e| anyhow::anyhow!("unbundle failed: {e}"))?;
    let severities = bundle_fetch_fsck_severities();

    // Sort by oid for deterministic reporting order.
    let mut entries: Vec<_> = objects.iter().collect();
    entries.sort_by(|a, b| a.0.to_hex().cmp(&b.0.to_hex()));

    for (oid, obj) in entries {
        if let Err(err) = grit_lib::fsck_standalone::fsck_object(obj.kind, &obj.data) {
            // A severity override of `ignore` demotes this specific check.
            if severities
                .get(&err.id.to_ascii_lowercase())
                .map(String::as_str)
                == Some("ignore")
            {
                continue;
            }
            eprintln!("error: object {}: {}", oid.to_hex(), err.report_line());
            eprintln!("fatal: fsck error in pack objects");
            return Err(crate::explicit_exit::SilentNonZeroExit { code: 128 }.into());
        }
    }
    Ok(())
}
fn clone_filter_omits_root_trees(filter_spec: Option<&str>) -> bool {
    let Some(spec) = filter_spec.map(str::trim).filter(|s| !s.is_empty()) else {
        return false;
    };
    match ObjectFilter::parse(spec) {
        Ok(ObjectFilter::TreeDepth(0)) => true,
        Ok(ObjectFilter::Combine(filters)) => filters
            .iter()
            .any(|filter| matches!(filter, ObjectFilter::TreeDepth(0))),
        _ => false,
    }
}

/// Whether the client-side `copy_objects` + materialize layout can faithfully apply this filter.
///
/// Only `blob:none` (and combine specs whose only effect is omitting blobs / omitting root trees,
/// i.e. `tree:0`) can be materialized client-side. A `tree:<depth>` with depth>0, `sparse:oid=…`,
/// or `blob:limit=<nonzero>` requires the actual reachability-aware filter that
/// `pack-objects --filter` applies on the upload-pack side; those must negotiate via upload-pack.
fn clone_filter_materializable_locally(filter_spec: Option<&str>) -> bool {
    let Some(spec) = filter_spec.map(str::trim).filter(|s| !s.is_empty()) else {
        return true;
    };
    fn one_ok(f: &ObjectFilter) -> bool {
        match f {
            ObjectFilter::BlobNone => true,
            ObjectFilter::TreeDepth(d) => *d == 0,
            ObjectFilter::Combine(parts) => parts.iter().all(one_ok),
            _ => false,
        }
    }
    match ObjectFilter::parse(spec) {
        Ok(f) => one_ok(&f),
        // An unparseable spec is left to the server to reject.
        Err(_) => false,
    }
}

fn setup_remote_mirror_fetch_and_url(
    git_dir: &Path,
    remote_url: &str,
    remote_name: &str,
) -> Result<()> {
    let config_path = git_dir.join("config");
    let mut config = match ConfigFile::from_path(&config_path, ConfigScope::Local)? {
        Some(c) => c,
        None => ConfigFile::parse(&config_path, "", ConfigScope::Local)?,
    };
    config.set(&format!("remote.{remote_name}.mirror"), "true")?;
    config.set(&format!("remote.{remote_name}.fetch"), "+refs/*:refs/*")?;
    config.set(&format!("remote.{remote_name}.url"), remote_url)?;
    config.write().context("writing config")?;
    Ok(())
}

fn apply_branch_autosetuprebase_from_global(git_dir: &Path, branch: &str) -> Result<()> {
    let set = ConfigSet::load(None, true).unwrap_or_default();
    let Some(mode) = set.get("branch.autosetuprebase") else {
        return Ok(());
    };
    let lower = mode.to_ascii_lowercase();
    if lower != "remote" && lower != "always" {
        return Ok(());
    }
    let config_path = git_dir.join("config");
    let mut config = match ConfigFile::from_path(&config_path, ConfigScope::Local)? {
        Some(c) => c,
        None => ConfigFile::parse(&config_path, "", ConfigScope::Local)?,
    };
    config.set(&format!("branch.{branch}.rebase"), "true")?;
    config.write().context("writing config")?;
    Ok(())
}

fn copy_shallow_file_if_present(src_git_dir: &Path, dst_git_dir: &Path) -> Result<()> {
    let src = src_git_dir.join("shallow");
    if src.is_file() {
        fs::copy(&src, dst_git_dir.join("shallow"))
            .with_context(|| format!("copying shallow from {}", src.display()))?;
    }
    Ok(())
}

/// Copy every ref under `refs/` plus `HEAD` (for `git clone --mirror`).
fn copy_refs_mirror_all(src_git_dir: &Path, dst_git_dir: &Path) -> Result<()> {
    let head_src = src_git_dir.join("HEAD");
    if head_src.is_file() {
        let content = fs::read_to_string(&head_src).context("reading source HEAD")?;
        fs::write(dst_git_dir.join("HEAD"), content).context("writing mirror HEAD")?;
    }
    let refs =
        grit_lib::refs::list_refs(src_git_dir, "refs/").map_err(|e| anyhow::anyhow!("{e}"))?;
    for (refname, oid) in refs {
        clone_write_direct_ref(dst_git_dir, &refname, &oid.to_hex())?;
    }
    Ok(())
}

/// Git directory that owns `objects/` (the common dir when `git_dir` is a linked worktree admin).
fn object_store_git_dir(git_dir: &Path) -> PathBuf {
    grit_lib::repo::common_git_dir_for_config(git_dir)
}

/// Open a source repository (bare or non-bare).
/// Resolve every `--reference` / `--reference-if-able` repo to its `objects` directory. Missing
/// optional references are skipped; missing required references are skipped here too (the later
/// `append_reference_alternates` reports the hard error) — this list is only used to suppress
/// `want`s for objects the reference already holds.
fn resolve_reference_object_dirs(args: &Args) -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    for reference in args.reference.iter().chain(args.reference_if_able.iter()) {
        if let Ok(repo) = open_source_repo(&PathBuf::from(reference)) {
            dirs.push(repo.git_dir.join("objects"));
        }
    }
    dirs
}

fn open_source_repo(path: &Path) -> Result<Repository> {
    if path.is_file() {
        let work_tree = path.parent().map(Path::to_path_buf);
        let git_dir = grit_lib::repo::resolve_dot_git(path)?;
        return Repository::open(&git_dir, work_tree.as_deref()).map_err(Into::into);
    }
    let dot_git = path.join(".git");
    if dot_git.is_file() {
        let resolved = grit_lib::repo::resolve_dot_git(&dot_git)?;
        return Repository::open(&resolved, Some(path)).map_err(Into::into);
    }
    if let Ok(repo) = Repository::open(&dot_git, Some(path)) {
        return Ok(repo);
    }
    Repository::open(path, None).map_err(Into::into)
}

/// Append one absolute `objects` directory line to `objects/info/alternates` (deduped).
fn add_alternate_objects_line(objects_info_dir: &Path, objects_abs: &Path) -> Result<()> {
    fs::create_dir_all(objects_info_dir)?;
    let alt_path = objects_info_dir.join("alternates");
    let line = format!("{}\n", objects_abs.display());
    let existing = fs::read_to_string(&alt_path).unwrap_or_default();
    if existing
        .lines()
        .any(|l| l.trim() == objects_abs.to_string_lossy())
    {
        return Ok(());
    }
    let mut out = existing;
    out.push_str(&line);
    fs::write(alt_path, out)?;
    Ok(())
}

/// Copy `objects/info/alternates` from the source into `dst_objects`, resolving relative paths
/// against the source repo root (matches Git's `copy_alternates`).
fn merge_alternates_from_source_objects(src_git_dir: &Path, dst_objects: &Path) -> Result<()> {
    let src_git_dir = object_store_git_dir(src_git_dir);
    let src_alt = src_git_dir.join("objects/info/alternates");
    let Ok(text) = fs::read_to_string(&src_alt) else {
        return Ok(());
    };
    let dst_info = dst_objects.join("info");
    fs::create_dir_all(&dst_info)?;
    let src_objects = src_git_dir.join("objects");
    let src_objects_canon = src_objects.canonicalize().unwrap_or(src_objects.clone());
    for raw in text.lines() {
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let abs_objects = if Path::new(line).is_absolute() {
            PathBuf::from(line)
        } else {
            src_objects_canon.join(line)
        };
        let abs_objects = abs_objects.canonicalize().unwrap_or(abs_objects);
        add_alternate_objects_line(&dst_info, &abs_objects)?;
    }
    Ok(())
}

/// Add `--reference` / `--reference-if-able` object stores to `dst_objects/info/alternates`.
fn append_reference_alternates(
    dst_objects: &Path,
    required: &[String],
    optional: &[String],
) -> Result<()> {
    let dst_info = dst_objects.join("info");
    for reference in required {
        let ref_path = PathBuf::from(reference);
        let ref_repo = open_source_repo(&ref_path)
            .with_context(|| format!("cannot open reference repository '{reference}'"))?;
        let ref_objects = ref_repo.git_dir.join("objects");
        let ref_objects_abs = ref_objects.canonicalize().unwrap_or(ref_objects);
        add_alternate_objects_line(&dst_info, &ref_objects_abs)?;
    }
    for reference in optional {
        let ref_path = PathBuf::from(reference);
        match open_source_repo(&ref_path) {
            Ok(ref_repo) => {
                let ref_objects = ref_repo.git_dir.join("objects");
                let ref_objects_abs = ref_objects.canonicalize().unwrap_or(ref_objects);
                add_alternate_objects_line(&dst_info, &ref_objects_abs)?;
            }
            Err(e) => {
                eprintln!("info: Could not add alternate for '{}': {}\n", reference, e);
            }
        }
    }
    Ok(())
}

/// `--shared` (`-s`): alternates to source plus any reference repos.
fn write_shared_alternates(
    src_git_dir: &Path,
    dst_git_dir: &Path,
    required: &[String],
    optional: &[String],
) -> Result<()> {
    let dst_objects = dst_git_dir.join("objects");
    let dst_info = dst_objects.join("info");
    fs::create_dir_all(&dst_info)?;
    let src_objects = object_store_git_dir(src_git_dir).join("objects");
    let src_abs = src_objects.canonicalize().unwrap_or(src_objects);
    add_alternate_objects_line(&dst_info, &src_abs)?;
    append_reference_alternates(&dst_objects, required, optional)?;
    Ok(())
}

/// Run `repack -a -d` and remove `objects/info/alternates` (Git `--dissociate` behaviour).
fn dissociate_clone_repository(git_dir: &Path) -> Result<()> {
    let grit_bin = crate::grit_exe::grit_executable();
    let status = Command::new(&grit_bin)
        .args(["-C", &git_dir.to_string_lossy(), "repack", "-a", "-d"])
        .status()
        .context("spawning grit repack for --dissociate")?;
    if !status.success() {
        bail!("cannot repack to clean up");
    }
    let alt = git_dir.join("objects/info/alternates");
    match fs::remove_file(&alt) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e).context("cannot unlink temporary alternates file"),
    }
}

/// When cloning recursively with `--reference` or `--reference-if-able`, record how nested
/// submodule clones should derive alternates from the superproject (Git `clone.c`).
fn apply_submodule_reference_config_for_recursive_clone(git_dir: &Path, args: &Args) -> Result<()> {
    if !args.recurse_submodules() {
        return Ok(());
    }
    if args.reference.is_empty() && args.reference_if_able.is_empty() {
        return Ok(());
    }
    let config_path = git_dir.join("config");
    let mut config = match ConfigFile::from_path(&config_path, ConfigScope::Local)? {
        Some(c) => c,
        None => ConfigFile::parse(&config_path, "", ConfigScope::Local)?,
    };
    config.set("submodule.alternateLocation", "superproject")?;
    if !args.reference.is_empty() {
        config.set("submodule.alternateErrorStrategy", "die")?;
    } else {
        config.set("submodule.alternateErrorStrategy", "info")?;
    }
    config
        .write()
        .context("writing submodule alternate config")?;
    Ok(())
}

/// Walk from HEAD to determine shallow boundary commits and write `.git/shallow`.
fn write_shallow_boundary(repo: &Repository, depth: usize) -> Result<()> {
    use grit_lib::objects::{parse_commit, ObjectKind};

    let head_oid = match grit_lib::refs::resolve_ref(&repo.git_dir, "HEAD") {
        Ok(oid) => oid,
        Err(_) => return Ok(()),
    };

    // BFS: HEAD is depth 1, its parent depth 2, etc.
    let mut boundary = Vec::new();
    let mut queue = std::collections::VecDeque::new();
    let mut visited = std::collections::HashSet::new();
    queue.push_back((head_oid, 1usize));
    visited.insert(head_oid);

    while let Some((oid, d)) = queue.pop_front() {
        if d == depth {
            boundary.push(oid);
            continue;
        }
        if let Ok(obj) = repo.odb.read(&oid) {
            if obj.kind == ObjectKind::Commit {
                if let Ok(commit) = parse_commit(&obj.data) {
                    for parent in &commit.parents {
                        if visited.insert(*parent) {
                            queue.push_back((*parent, d + 1));
                        }
                    }
                }
            }
        }
    }

    if !boundary.is_empty() {
        let shallow_path = repo.git_dir.join("shallow");
        let content: Vec<String> = boundary.iter().map(|oid| oid.to_hex()).collect();
        fs::write(&shallow_path, content.join("\n") + "\n")?;
    }

    Ok(())
}

/// True when `<git-dir>/objects` has no `.pack` files and no loose object files.
fn objects_dir_has_no_data(git_dir: &Path) -> bool {
    let objects = git_dir.join("objects");
    if !objects.is_dir() {
        return true;
    }
    let pack_dir = objects.join("pack");
    if pack_dir.is_dir() {
        if let Ok(rd) = fs::read_dir(&pack_dir) {
            for e in rd.flatten() {
                if e.path()
                    .extension()
                    .is_some_and(|x| x.eq_ignore_ascii_case("pack"))
                {
                    return false;
                }
            }
        }
    }
    if let Ok(rd) = fs::read_dir(&objects) {
        for e in rd.flatten() {
            let name = e.file_name().to_string_lossy().to_string();
            if name.len() == 2 && name.chars().all(|c| c.is_ascii_hexdigit()) {
                if e.path().is_dir() {
                    if let Ok(sub) = fs::read_dir(e.path()) {
                        if sub.count() > 0 {
                            return false;
                        }
                    }
                }
            }
        }
    }
    true
}

/// Copy all objects (loose + packs) from source to destination.
///
/// When `try_hardlink` is true, use hard links for pack and loose object files when possible
/// (local clone fast path). When false, always copy bytes (e.g. `--no-hardlinks`, post-fetch
/// materialization).
fn copy_objects(src_git_dir: &Path, dst_git_dir: &Path, try_hardlink: bool) -> Result<()> {
    let src_objects = object_store_git_dir(src_git_dir).join("objects");
    let dst_objects = dst_git_dir.join("objects");

    // Mirror Git's `copy_or_link_directory`: refuse to clone locally when the source's
    // `objects` directory is itself a symlink (an adversary could point it at a sensitive
    // directory). `lstat` the path before recursing.
    if let Ok(meta) = fs::symlink_metadata(&src_objects) {
        if meta.file_type().is_symlink() {
            bail!(
                "'{}' is a symlink, refusing to clone with --local",
                src_objects.display()
            );
        }
    }

    if src_objects.is_dir() {
        fs::create_dir_all(&dst_objects)?;
        copy_or_link_objects_tree(&src_objects, &dst_objects, Path::new(""), try_hardlink)?;
    }

    Ok(())
}

/// Recursively copy/hardlink the entire `objects` tree, faithfully replicating Git's
/// `copy_or_link_directory` (builtin/clone.c). Every file and directory — including hidden,
/// unknown, and garbage entries — is reproduced. Encountering any symlink aborts the clone
/// with Git's "refusing to clone with --local" error; `info/alternates` is special-cased and
/// resolved by the caller (`merge_alternates_from_source_objects`) rather than copied verbatim.
fn copy_or_link_objects_tree(src: &Path, dst: &Path, rel: &Path, try_hardlink: bool) -> Result<()> {
    let read = match fs::read_dir(src) {
        Ok(rd) => rd,
        // A concurrent repack can remove a whole fan-out directory while we walk it; treat a
        // vanished source directory as nothing to copy rather than aborting the clone.
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(e.into()),
    };

    for entry in read {
        let entry = entry?;
        let name = entry.file_name();
        let src_path = entry.path();
        let dst_path = dst.join(&name);
        let entry_rel = rel.join(&name);

        let meta = match fs::symlink_metadata(&src_path) {
            Ok(m) => m,
            Err(e) if e.kind() == io::ErrorKind::NotFound => continue,
            Err(e) => return Err(e.into()),
        };
        let ftype = meta.file_type();

        if ftype.is_symlink() {
            bail!(
                "symlink '{}' exists, refusing to clone with --local",
                entry_rel.display()
            );
        }

        if ftype.is_dir() {
            fs::create_dir_all(&dst_path)?;
            copy_or_link_objects_tree(&src_path, &dst_path, &entry_rel, try_hardlink)?;
            continue;
        }

        // `info/alternates` is resolved (relative paths rebased onto the source) by the caller
        // rather than copied byte-for-byte; skip it here, matching Git's `copy_alternates`.
        if entry_rel == Path::new("info/alternates") {
            continue;
        }

        if dst_path.exists() {
            continue;
        }
        if try_hardlink && fs::hard_link(&src_path, &dst_path).is_ok() {
            continue;
        }
        // A concurrent `git maintenance`/repack on the source can remove a file between the
        // directory scan and this copy. Treat a vanished source file as a skip rather than
        // aborting the whole clone (Git's local clone is similarly robust).
        copy_skip_vanished_source(&src_path, &dst_path)?;

        // Git's local clone copies content but the destination takes the umask-default mode,
        // not the source's possibly read-only 0444. Tests rely on being able to corrupt
        // these files (e.g. commit-graph layers under `info/commit-graphs/`).
        #[cfg(unix)]
        if entry_rel.starts_with("info/commit-graphs") {
            use std::os::unix::fs::PermissionsExt;
            let _ = fs::set_permissions(&dst_path, fs::Permissions::from_mode(0o644));
        }
    }

    Ok(())
}

/// Copy `src` to `dst`, treating a source file that vanished mid-copy (`ErrorKind::NotFound`)
/// as a no-op. Concurrent maintenance on the clone source can delete pack/info files between
/// the directory listing and the copy; aborting the clone in that case would be incorrect.
fn copy_skip_vanished_source(src: &Path, dst: &Path) -> Result<()> {
    match fs::copy(src, dst) {
        Ok(_) => {
            if let Ok(meta) = fs::metadata(src) {
                let atime = FileTime::from_last_access_time(&meta);
                let mtime = FileTime::from_last_modification_time(&meta);
                let _ = set_file_times(dst, atime, mtime);
            }
            Ok(())
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e.into()),
    }
}

/// Fail early when the source has obviously corrupt ref files under `refs/heads` or `refs/tags`,
/// matching Git's clone error for invalid loose refs (`t5605` REFFILES case).
fn assert_source_refs_valid_for_clone(git_dir: &Path) -> Result<()> {
    if grit_lib::reftable::is_reftable_repo(git_dir) {
        return Ok(());
    }

    fn walk_loose_refs(dir: &Path) -> Result<()> {
        let read = match fs::read_dir(dir) {
            Ok(r) => r,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(()),
            Err(e) => return Err(e.into()),
        };
        for entry in read {
            let entry = entry?;
            let path = entry.path();
            if path.is_dir() {
                walk_loose_refs(&path)?;
            } else if path.is_file() {
                let content = fs::read_to_string(&path)?;
                let trimmed = content.trim();
                if trimmed.starts_with("ref: ") {
                    continue;
                }
                if !ObjectId::is_full_hex(trimmed) {
                    bail!("has neither a valid OID nor a target");
                }
            }
        }
        Ok(())
    }

    walk_loose_refs(&git_dir.join("refs/heads"))?;
    walk_loose_refs(&git_dir.join("refs/tags"))?;

    let packed_refs = git_dir.join("packed-refs");
    if packed_refs.is_file() {
        let content = fs::read_to_string(&packed_refs)?;
        for line in content.lines() {
            if line.is_empty() || line.starts_with('#') || line.starts_with('^') {
                continue;
            }
            let mut parts = line.split_whitespace();
            let Some(oid) = parts.next() else {
                continue;
            };
            let Some(refname) = parts.next() else {
                continue;
            };
            if !(refname.starts_with("refs/heads/") || refname.starts_with("refs/tags/")) {
                continue;
            }
            if !ObjectId::is_full_hex(oid) {
                bail!("has neither a valid OID nor a target");
            }
        }
    }

    Ok(())
}

/// When the source repo uses `extensions.objectformat` (e.g. SHA-256), mirror that into the
/// clone's config. Needed for `git clone --no-local` of empty SHA-256 repos (`t5700`).
fn propagate_extensions_object_format(src_git: &Path, dst_git: &Path) -> Result<()> {
    let set = ConfigSet::load(Some(src_git), false).unwrap_or_default();
    let fmt = set
        .get("extensions.objectformat")
        .or_else(|| set.get("extensions.objectFormat"));
    if let Some(fmt) = fmt {
        write_clone_object_format(dst_git, &fmt)?;
    }
    Ok(())
}

/// Record the negotiated object format into a clone's config.
///
/// SHA-256 requires `core.repositoryformatversion = 1` plus `extensions.objectformat = sha256`
/// (the `extensions.*` keys are only honoured when the format version is 1). SHA-1 is the
/// default and needs no config, so this is a no-op for it. Shared by the local/file clone path
/// (`propagate_extensions_object_format`) and the smart-HTTP clone path (`t5551`).
fn write_clone_object_format(dst_git: &Path, object_format: &str) -> Result<()> {
    if !object_format.eq_ignore_ascii_case("sha256") {
        return Ok(());
    }
    let config_path = dst_git.join("config");
    let mut config = match ConfigFile::from_path(&config_path, ConfigScope::Local)? {
        Some(c) => c,
        None => ConfigFile::parse(&config_path, "", ConfigScope::Local)?,
    };
    config.set("core.repositoryformatversion", "1")?;
    config.set("extensions.objectformat", "sha256")?;
    config
        .write()
        .context("writing extensions.objectformat into clone")?;
    Ok(())
}

fn copy_refs_from_upload_pack_lists(
    dst_git_dir: &Path,
    remote_name: &str,
    remote_heads: &[(String, ObjectId)],
    remote_tags: &[(String, ObjectId)],
    no_tags: bool,
) -> Result<()> {
    let remote_dir = dst_git_dir.join("refs/remotes").join(remote_name);
    for (refname, oid) in remote_heads {
        let branch = refname.strip_prefix("refs/heads/").unwrap_or(refname);
        let dst_ref = remote_dir.join(branch);
        if let Some(parent) = dst_ref.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::write(&dst_ref, format!("{}\n", oid.to_hex()))?;
    }
    if !no_tags {
        for (refname, oid) in remote_tags {
            let dst_ref = dst_git_dir.join(refname);
            if let Some(parent) = dst_ref.parent() {
                fs::create_dir_all(parent)?;
            }
            fs::write(&dst_ref, format!("{}\n", oid.to_hex()))?;
        }
    }
    Ok(())
}

fn copy_configured_remote_fetch_refspecs(
    src_git_dir: &Path,
    dst_git_dir: &Path,
    remote_name: &str,
) -> Result<()> {
    let config = ConfigSet::load(Some(dst_git_dir), true)?;
    let key = format!("remote.{remote_name}.fetch");
    for raw in config.get_all(&key) {
        copy_fetch_refspec(src_git_dir, dst_git_dir, raw.trim())?;
    }
    let command_config = ConfigSet::load(None, true)?;
    for raw in command_config.get_all(&key) {
        copy_fetch_refspec(src_git_dir, dst_git_dir, raw.trim())?;
    }
    for raw in command_line_config_values(&key) {
        copy_fetch_refspec(src_git_dir, dst_git_dir, raw.trim())?;
    }
    Ok(())
}

fn command_line_config_values(key: &str) -> Vec<String> {
    let mut values = Vec::new();
    if let Ok(count_str) = std::env::var("GIT_CONFIG_COUNT") {
        if let Ok(count) = count_str.parse::<usize>() {
            for i in 0..count {
                let key_var = format!("GIT_CONFIG_KEY_{i}");
                let val_var = format!("GIT_CONFIG_VALUE_{i}");
                if let (Ok(k), Ok(v)) = (std::env::var(&key_var), std::env::var(&val_var)) {
                    if k.eq_ignore_ascii_case(key) {
                        values.push(v);
                    }
                }
            }
        }
    }
    if let Ok(params) = std::env::var("GIT_CONFIG_PARAMETERS") {
        for token in params.split_whitespace() {
            let token = token.trim_matches('\'').trim_matches('"');
            if let Some((k, v)) = token.split_once('=') {
                if k.eq_ignore_ascii_case(key) {
                    values.push(v.to_string());
                }
            }
        }
    }
    values
}

fn copy_fetch_refspec(src_git_dir: &Path, dst_git_dir: &Path, raw: &str) -> Result<()> {
    let spec = raw.strip_prefix('+').unwrap_or(raw);
    let Some((src_pat, dst_pat)) = spec.split_once(':') else {
        return Ok(());
    };
    if src_pat.is_empty() || dst_pat.is_empty() {
        return Ok(());
    }
    if let Some((src_prefix, src_suffix)) = split_single_glob(src_pat) {
        let Some((dst_prefix, dst_suffix)) = split_single_glob(dst_pat) else {
            return Ok(());
        };
        for (refname, oid) in refs::list_refs(src_git_dir, "refs/")? {
            if let Some(matched) = match_refspec_glob(&refname, src_prefix, src_suffix) {
                let dst = format!("{dst_prefix}{matched}{dst_suffix}");
                clone_write_direct_ref(dst_git_dir, &dst, &oid.to_hex())?;
            }
        }
        return Ok(());
    }
    if let Ok(oid) = refs::resolve_ref(src_git_dir, src_pat) {
        clone_write_direct_ref(dst_git_dir, dst_pat, &oid.to_hex())?;
    }
    Ok(())
}

fn split_single_glob(pattern: &str) -> Option<(&str, &str)> {
    let idx = pattern.find('*')?;
    if pattern[idx + 1..].contains('*') {
        return None;
    }
    Some((&pattern[..idx], &pattern[idx + 1..]))
}

fn match_refspec_glob<'a>(refname: &'a str, prefix: &str, suffix: &str) -> Option<&'a str> {
    refname
        .strip_prefix(prefix)
        .and_then(|rest| rest.strip_suffix(suffix))
}

/// Write source tags into the destination for a `--branch <tag>` clone.
///
/// Mirrors upstream `write_followtags`: every tag whose target object was transferred (for a local
/// clone, the whole object store is copied, so all tags qualify) is written under `refs/tags/`.
/// With `--no-tags`, no tags are written here (the explicit `+refs/tags/<tag>:refs/tags/<tag>`
/// refspec still fetches the named tag on a subsequent `git fetch`).
fn copy_followed_tags_for_tag_clone(
    src_git_dir: &Path,
    dst_git_dir: &Path,
    no_tags: bool,
) -> Result<()> {
    if no_tags {
        return Ok(());
    }
    let dst_odb = grit_lib::odb::Odb::new(&dst_git_dir.join("objects"));
    let tags =
        grit_lib::refs::list_refs(src_git_dir, "refs/tags/").map_err(|e| anyhow::anyhow!("{e}"))?;
    for (refname, oid) in &tags {
        if !dst_odb.exists(oid) {
            continue;
        }
        clone_write_direct_ref(dst_git_dir, refname, &oid.to_hex())?;
    }
    Ok(())
}

/// Copy refs from source into remote-tracking refs in the destination.
/// Copy source refs into the destination as remote-tracking refs.
///
/// When `single_branch` names a branch, only that branch is mirrored to
/// `refs/remotes/<remote>/<branch>` and tags are auto-followed only when their
/// (peeled) target is reachable from that branch tip — mirroring Git's
/// `clone --single-branch` tag auto-follow, which never downloads tags that
/// point outside the fetched history.
fn copy_refs_as_remote_filtered(
    src_git_dir: &Path,
    dst_git_dir: &Path,
    remote_name: &str,
    no_tags: bool,
    single_branch: Option<&str>,
) -> Result<()> {
    let dst_odb = grit_lib::odb::Odb::new(&dst_git_dir.join("objects"));

    // Use the library ref-listing API which handles both files and reftable
    let heads = grit_lib::refs::list_refs(src_git_dir, "refs/heads/")
        .map_err(|e| anyhow::anyhow!("{e}"))?;
    let mut branch_tips: Vec<ObjectId> = Vec::new();
    for (refname, oid) in &heads {
        let branch = refname.strip_prefix("refs/heads/").unwrap_or(refname);
        if let Some(only) = single_branch {
            if branch != only {
                continue;
            }
        }
        let dst_name = format!("refs/remotes/{remote_name}/{branch}");
        clone_write_direct_ref(dst_git_dir, &dst_name, &oid.to_hex())?;
        branch_tips.push(*oid);
    }

    // For single-branch clones, build the reachable commit set from the single
    // branch tip so tag auto-follow can be restricted to that history.
    let reachable: Option<HashSet<ObjectId>> = if single_branch.is_some() {
        Some(reachable_commits_from_tips(&dst_odb, &branch_tips))
    } else {
        None
    };
    let tag_reachable = |tag_oid: &ObjectId| -> bool {
        if !dst_odb.exists(tag_oid) {
            return false;
        }
        match &reachable {
            None => true,
            Some(set) => {
                let target = peel_tag_target(&dst_odb, *tag_oid);
                set.contains(&target)
            }
        }
    };

    if !no_tags {
        let tags = grit_lib::refs::list_refs(src_git_dir, "refs/tags/")
            .map_err(|e| anyhow::anyhow!("{e}"))?;
        for (refname, oid) in &tags {
            if !tag_reachable(oid) {
                continue;
            }
            clone_write_direct_ref(dst_git_dir, refname, &oid.to_hex())?;
        }
    }

    // Also handle packed-refs if present (files backend only)
    if !grit_lib::reftable::is_reftable_repo(src_git_dir) {
        let packed_refs = src_git_dir.join("packed-refs");
        if packed_refs.is_file() {
            let content = fs::read_to_string(&packed_refs)?;
            for line in content.lines() {
                if line.is_empty() || line.starts_with('#') || line.starts_with('^') {
                    continue;
                }
                let mut parts = line.split_whitespace();
                let Some(oid) = parts.next() else { continue };
                let Some(refname) = parts.next() else {
                    continue;
                };

                if let Some(branch) = refname.strip_prefix("refs/heads/") {
                    if let Some(only) = single_branch {
                        if branch != only {
                            continue;
                        }
                    }
                    let dst_name = format!("refs/remotes/{remote_name}/{branch}");
                    if !clone_ref_file_exists(dst_git_dir, &dst_name) {
                        clone_write_direct_ref(dst_git_dir, &dst_name, oid)?;
                    }
                } else if !no_tags && refname.starts_with("refs/tags/") {
                    let Ok(oid_parsed) = oid.parse::<grit_lib::objects::ObjectId>() else {
                        continue;
                    };
                    if !tag_reachable(&oid_parsed) {
                        continue;
                    }
                    if !clone_ref_file_exists(dst_git_dir, refname) {
                        clone_write_direct_ref(dst_git_dir, refname, oid)?;
                    }
                }
            }
        }
    }

    Ok(())
}

/// Peel an annotated tag to its underlying commit/object OID (following nested
/// tags). Returns the input OID unchanged when it is not a tag or cannot be read.
fn peel_tag_target(odb: &grit_lib::odb::Odb, mut oid: ObjectId) -> ObjectId {
    for _ in 0..16 {
        match odb.read(&oid) {
            Ok(obj) if obj.kind == ObjectKind::Tag => match parse_tag(&obj.data) {
                Ok(tag) => oid = tag.object,
                Err(_) => return oid,
            },
            _ => return oid,
        }
    }
    oid
}

/// Compute the set of commit OIDs reachable from the given tips, walking the
/// commit graph through whatever commits are present locally. Trees/blobs are
/// not included; only commit reachability is needed for tag auto-follow.
fn reachable_commits_from_tips(odb: &grit_lib::odb::Odb, tips: &[ObjectId]) -> HashSet<ObjectId> {
    let mut seen = HashSet::new();
    let mut queue: VecDeque<ObjectId> = tips.iter().copied().collect();
    while let Some(oid) = queue.pop_front() {
        if !seen.insert(oid) {
            continue;
        }
        let Ok(obj) = odb.read(&oid) else { continue };
        match obj.kind {
            ObjectKind::Commit => {
                if let Ok(commit) = parse_commit(&obj.data) {
                    for p in &commit.parents {
                        queue.push_back(*p);
                    }
                }
            }
            ObjectKind::Tag => {
                if let Ok(tag) = parse_tag(&obj.data) {
                    queue.push_back(tag.object);
                }
            }
            _ => {}
        }
    }
    seen
}

/// In shallow clones, keep only tags reachable from the shallow boundary commits.
///
/// Git's `clone --depth=<n>` does not copy arbitrary old tags that point to commits
/// outside the shallow slice. Prune such tags after ref copy so follow-up pushes
/// produce the same object enumeration counts as Git.
fn prune_shallow_tags_not_reachable_from_boundaries(
    src_git_dir: &Path,
    dst_git_dir: &Path,
) -> Result<()> {
    let shallow_path = dst_git_dir.join("shallow");
    let Ok(contents) = fs::read_to_string(&shallow_path) else {
        return Ok(());
    };
    let boundaries: Vec<ObjectId> = contents
        .lines()
        .filter_map(|line| line.trim().parse::<ObjectId>().ok())
        .collect();
    if boundaries.is_empty() {
        return Ok(());
    }

    let src_repo = Repository::open(src_git_dir, None)
        .or_else(|_| Repository::open(src_git_dir, src_git_dir.parent()))
        .with_context(|| format!("opening source repository at {}", src_git_dir.display()))?;
    let dst_repo = Repository::open(dst_git_dir, None)
        .or_else(|_| Repository::open(dst_git_dir, dst_git_dir.parent()))
        .with_context(|| {
            format!(
                "opening destination repository at {}",
                dst_git_dir.display()
            )
        })?;
    let tags = refs::list_refs(dst_git_dir, "refs/tags/").unwrap_or_default();
    for (refname, tag_oid) in tags {
        let target_oid = match dst_repo.odb.read(&tag_oid) {
            Ok(obj) if obj.kind == ObjectKind::Tag => {
                parse_tag(&obj.data).map(|t| t.object).unwrap_or(tag_oid)
            }
            _ => tag_oid,
        };
        let keep = boundaries.iter().any(|boundary| {
            *boundary == target_oid
                || is_ancestor(&src_repo, *boundary, target_oid).unwrap_or(false)
        });
        if !keep {
            let _ = refs::delete_ref(dst_git_dir, &refname);
        }
    }
    Ok(())
}

/// Recursively copy ref files from src to dst.
#[allow(dead_code)]
fn copy_refs_recursive(src: &Path, dst: &Path) -> Result<()> {
    fs::create_dir_all(dst)?;
    for entry in fs::read_dir(src)? {
        let entry = entry?;
        let dst_path = dst.join(entry.file_name());
        if entry.file_type()?.is_dir() {
            copy_refs_recursive(&entry.path(), &dst_path)?;
        } else if entry.file_type()?.is_file() {
            fs::copy(entry.path(), &dst_path)?;
        }
    }
    Ok(())
}

/// Copy refs from source directly into destination (for bare clones).
/// Mirrors refs/heads/* and refs/tags/* directly.
fn copy_refs_direct(src_git_dir: &Path, dst_git_dir: &Path) -> Result<()> {
    // Use the library API to read refs (handles both files and reftable)
    for prefix in &["refs/heads/", "refs/tags/"] {
        let refs =
            grit_lib::refs::list_refs(src_git_dir, prefix).map_err(|e| anyhow::anyhow!("{e}"))?;
        for (refname, oid) in &refs {
            clone_write_direct_ref(dst_git_dir, refname, &oid.to_hex())?;
        }
    }

    // Also handle packed-refs if present
    let packed_refs = src_git_dir.join("packed-refs");
    if packed_refs.is_file() {
        let content = fs::read_to_string(&packed_refs)?;
        for line in content.lines() {
            if line.is_empty() || line.starts_with('#') || line.starts_with('^') {
                continue;
            }
            let mut parts = line.split_whitespace();
            let Some(oid) = parts.next() else { continue };
            let Some(refname) = parts.next() else {
                continue;
            };

            if refname.starts_with("refs/heads/") || refname.starts_with("refs/tags/") {
                if !clone_ref_file_exists(dst_git_dir, refname) {
                    clone_write_direct_ref(dst_git_dir, refname, oid)?;
                }
            }
        }
    }

    Ok(())
}

/// Write the default `remote.<name>.fetch` refspec without clobbering one that `clone -c`
/// already supplied.
///
/// `git clone -c remote.<name>.fetch=<spec>` writes the user's refspec into the destination
/// config *before* the clone machinery records the default tracking refspec. Upstream Git appends
/// the default with `git_config_set_multivar_in_file_gently` (see `write_followtags`/
/// `write_refspec_config` in `builtin/clone.c`), so the result is multi-valued: the user's spec
/// first, then the default. Mirror that — only overwrite when no fetch entry exists yet.
fn write_default_remote_fetch(
    config: &mut ConfigFile,
    remote_name: &str,
    refspec: &str,
) -> Result<()> {
    let key = format!("remote.{remote_name}.fetch");
    if config.get(&key).is_some() {
        config.add_value(&key, refspec)?;
    } else {
        config.set(&key, refspec)?;
    }
    Ok(())
}

/// Set up the "origin" remote in the destination config (non-bare).
fn setup_origin_remote(
    git_dir: &Path,
    source_path: &Path,
    remote_name: &str,
    refspec: &str,
) -> Result<()> {
    let config_path = git_dir.join("config");
    let mut config = match ConfigFile::from_path(&config_path, ConfigScope::Local)? {
        Some(c) => c,
        None => ConfigFile::parse(&config_path, "", ConfigScope::Local)?,
    };

    let url = absolute_clone_source_url(source_path);

    config.set(&format!("remote.{remote_name}.url"), &url)?;
    write_default_remote_fetch(&mut config, remote_name, refspec)?;
    config.write().context("writing config")?;

    Ok(())
}

/// Set up the "origin" remote for a bare clone (URL only, no fetch refspec).
fn setup_origin_remote_bare(git_dir: &Path, source_path: &Path, remote_name: &str) -> Result<()> {
    let config_path = git_dir.join("config");
    let mut config = match ConfigFile::from_path(&config_path, ConfigScope::Local)? {
        Some(c) => c,
        None => ConfigFile::parse(&config_path, "", ConfigScope::Local)?,
    };

    let url = absolute_clone_source_url(source_path);

    config.set(&format!("remote.{remote_name}.url"), &url)?;
    config.write().context("writing config")?;

    Ok(())
}

/// Absolute form of a local clone source path, matching Git's `absolute_pathdup`: prepend the
/// current directory when relative, but leave `.`/`..`/`./` components untouched (no
/// normalization). Symlinks in the *parent* of the source are resolved (Git resolves the cwd via
/// `getcwd`) by canonicalizing the existing directory portion and re-appending the literal final
/// component, so `git clone .` yields `<cwd>/.` while `git clone /abs/submodule` stays verbatim.
fn absolute_clone_source_url(source_path: &Path) -> String {
    if source_path.is_absolute() {
        return source_path.to_string_lossy().to_string();
    }
    let cwd = match std::env::current_dir() {
        Ok(c) => c.canonicalize().unwrap_or(c),
        Err(_) => return source_path.to_string_lossy().to_string(),
    };
    cwd.join(source_path).to_string_lossy().to_string()
}

fn setup_origin_remote_bare_url(git_dir: &Path, remote_url: &str, remote_name: &str) -> Result<()> {
    let config_path = git_dir.join("config");
    let mut config = match ConfigFile::from_path(&config_path, ConfigScope::Local)? {
        Some(c) => c,
        None => ConfigFile::parse(&config_path, "", ConfigScope::Local)?,
    };
    config.set(&format!("remote.{remote_name}.url"), remote_url)?;
    config.write().context("writing config")?;
    Ok(())
}

fn setup_origin_remote_url(
    git_dir: &Path,
    remote_url: &str,
    remote_name: &str,
    refspec: &str,
) -> Result<()> {
    let config_path = git_dir.join("config");
    let mut config = match ConfigFile::from_path(&config_path, ConfigScope::Local)? {
        Some(c) => c,
        None => ConfigFile::parse(&config_path, "", ConfigScope::Local)?,
    };
    config.set(&format!("remote.{remote_name}.url"), remote_url)?;
    write_default_remote_fetch(&mut config, remote_name, refspec)?;
    config.write().context("writing config")?;
    Ok(())
}

/// When `submodule.stickyRecursiveClone` is true and `--recurse-submodules` was used,
/// record `submodule.recurse=true` in the new repo (Git clone behaviour).
fn apply_sticky_recursive_clone(git_dir: &Path, recurse_submodules: bool) -> Result<()> {
    if !recurse_submodules {
        return Ok(());
    }
    let set = ConfigSet::load(None, true).unwrap_or_default();
    if !matches!(
        set.get_bool("submodule.stickyRecursiveClone"),
        Some(Ok(true))
    ) {
        return Ok(());
    }
    let config_path = git_dir.join("config");
    let mut config = match ConfigFile::from_path(&config_path, ConfigScope::Local)? {
        Some(c) => c,
        None => ConfigFile::parse(&config_path, "", ConfigScope::Local)?,
    };
    config.set("submodule.recurse", "true")?;
    config.write().context("writing config")?;
    Ok(())
}

/// When `init.defaultSubmodulePathConfig` is true in the merged config (global/system), enable
/// `extensions.submodulePathConfig` in the new repository (matches Git's clone/init parity).
fn apply_default_submodule_path_config_from_global(git_dir: &Path) -> Result<()> {
    let set = ConfigSet::load(None, true).unwrap_or_else(|_| ConfigSet::new());
    if !set
        .get("init.defaultSubmodulePathConfig")
        .as_deref()
        .is_some_and(|v| matches!(v.to_ascii_lowercase().as_str(), "true" | "yes" | "on" | "1"))
    {
        return Ok(());
    }
    let config_path = git_dir.join("config");
    let mut config = match ConfigFile::from_path(&config_path, ConfigScope::Local)? {
        Some(c) => c,
        None => ConfigFile::parse(&config_path, "", ConfigScope::Local)?,
    };
    let mut repo_version = 0u32;
    if let Some(v) = config
        .entries
        .iter()
        .find(|e| e.key == "core.repositoryformatversion")
    {
        if let Some(s) = v.value.as_deref() {
            repo_version = s.parse().unwrap_or(0);
        }
    }
    if repo_version == 0 {
        config.set("core.repositoryformatversion", "1")?;
    }
    config.set("extensions.submodulePathConfig", "true")?;
    config
        .write()
        .context("writing submodulePathConfig from global init default")?;
    Ok(())
}

/// Apply -c config key=value pairs to the cloned repository.
fn apply_clone_config(git_dir: &Path, configs: &[String]) -> Result<()> {
    let config_path = git_dir.join("config");
    let mut config = match ConfigFile::from_path(&config_path, ConfigScope::Local)? {
        Some(c) => c,
        None => ConfigFile::parse(&config_path, "", ConfigScope::Local)?,
    };

    for entry in configs {
        if let Some((key, value)) = entry.split_once('=') {
            // Use add_value so repeated keys produce multi-valued entries
            config.add_value(key.trim(), value.trim())?;
        } else {
            // No '=' means boolean true
            config.add_value(entry.trim(), "true")?;
        }
    }

    if config_bool_value(&config, "extensions.submodulepathconfig")
        && config_repository_format_version(&config) == 0
    {
        config.set("core.repositoryformatversion", "1")?;
    }

    config.write().context("writing config")?;
    Ok(())
}

fn config_bool_value(config: &ConfigFile, key: &str) -> bool {
    config
        .entries
        .iter()
        .rev()
        .find(|e| e.key == key)
        .and_then(|e| e.value.as_deref())
        .is_some_and(|value| {
            matches!(
                value.to_ascii_lowercase().as_str(),
                "true" | "yes" | "on" | "1"
            )
        })
}

fn config_repository_format_version(config: &ConfigFile) -> u32 {
    config
        .entries
        .iter()
        .rev()
        .find(|e| e.key == "core.repositoryformatversion")
        .and_then(|e| e.value.as_deref())
        .and_then(|value| value.parse::<u32>().ok())
        .unwrap_or(0)
}

fn effective_clone_server_options(args: &Args, remote_name: &str) -> Result<Vec<String>> {
    if !args.server_options.is_empty() {
        return Ok(args.server_options.clone());
    }
    if protocol_wire::effective_client_protocol_version() < 2 {
        return Ok(Vec::new());
    }
    let want_key = format!("remote.{remote_name}.serveroption");
    let mut saw_command_line_override = false;
    let mut out = Vec::new();
    if let Ok(params) = std::env::var("GIT_CONFIG_PARAMETERS") {
        for token in params.split('\'') {
            let token = token.trim();
            if token.is_empty() {
                continue;
            }
            if let Some((key, value)) = token.split_once('=') {
                if key.trim().eq_ignore_ascii_case(&want_key) {
                    saw_command_line_override = true;
                    if value.is_empty() {
                        out.clear();
                    } else {
                        out.push(value.to_owned());
                    }
                }
            } else if token.eq_ignore_ascii_case(&want_key) {
                bail!("error: missing value for '{}'", want_key);
            }
        }
    }
    for entry in &args.config {
        let Some((key, value)) = entry.split_once('=') else {
            if entry.trim().eq_ignore_ascii_case(&want_key) {
                bail!("error: missing value for '{}'", want_key);
            }
            continue;
        };
        if !key.trim().eq_ignore_ascii_case(&want_key) {
            continue;
        }
        saw_command_line_override = true;
        if value.is_empty() {
            out.clear();
        } else {
            out.push(value.to_owned());
        }
    }
    if saw_command_line_override {
        return Ok(out);
    }
    Ok(Vec::new())
}

/// Set up branch tracking configuration (branch.<name>.remote and branch.<name>.merge).
fn setup_branch_tracking(git_dir: &Path, branch: &str, remote_name: &str) -> Result<()> {
    let config_path = git_dir.join("config");
    let mut config = match ConfigFile::from_path(&config_path, ConfigScope::Local)? {
        Some(c) => c,
        None => ConfigFile::parse(&config_path, "", ConfigScope::Local)?,
    };

    config.set(&format!("branch.{branch}.remote"), remote_name)?;
    config.set(
        &format!("branch.{branch}.merge"),
        &format!("refs/heads/{branch}"),
    )?;
    config.write().context("writing config")?;

    Ok(())
}

/// Turn a full local clone into a `blob:none`-style layout: commits and trees are
/// stored loose, pack files and alternates are removed, and reachable blob loose
/// files are deleted so `exists_local` matches a true partial clone.
fn materialize_blob_none_partial_layout(dest: &Repository) -> Result<()> {
    let alt = dest.git_dir.join("objects/info/alternates");
    let _ = fs::remove_file(&alt);

    let (skeleton, blobs) = collect_reachable_skeleton_and_blobs(dest)?;

    for oid in &skeleton {
        let obj = match dest.odb.read(oid) {
            Ok(o) => o,
            Err(_) => continue,
        };
        // Use `write_loose_materialize` (not `write`): the object is still present in the
        // just-copied local pack, so `Odb::write` would short-circuit via `exists()` and skip
        // writing the loose file. The packs are removed below, so the skeleton objects must be
        // duplicated as loose files first or they vanish entirely.
        let _ = dest.odb.write_loose_materialize(obj.kind, &obj.data)?;
    }

    let pack_dir = dest.git_dir.join("objects/pack");
    if pack_dir.is_dir() {
        for entry in fs::read_dir(&pack_dir)? {
            let entry = entry?;
            let path = entry.path();
            if path.is_file() {
                let _ = fs::remove_file(&path);
            }
        }
    }

    let mut skeleton_oids: Vec<ObjectId> = skeleton.into_iter().collect();
    skeleton_oids.sort_by_key(ObjectId::to_hex);
    if !skeleton_oids.is_empty() {
        let pack_path = crate::commands::pack_objects::write_partial_clone_promisor_pack(
            dest,
            &pack_dir,
            &skeleton_oids,
        )
        .context("writing partial-clone promisor skeleton pack")?;
        fs::write(
            pack_path.with_extension("promisor"),
            promisor_ref_list(dest)?,
        )
        .context("writing partial-clone promisor sidecar")?;
    }

    for oid in &blobs {
        let hex = oid.to_hex();
        if hex.len() < 3 {
            continue;
        }
        let loose = dest.git_dir.join("objects").join(&hex[..2]).join(&hex[2..]);
        let _ = fs::remove_file(loose);
    }

    Ok(())
}

fn hydrate_tree_zero_checkout_objects(dest: &Repository) -> Result<()> {
    let dest_config = ConfigSet::load(Some(&dest.git_dir), true)?;
    let Some(promisor) =
        crate::commands::promisor_hydrate::find_promisor_source(&dest_config, &dest.git_dir)?
    else {
        return Ok(());
    };
    crate::commands::promisor_hydrate::hydrate_head_tree_blobs_from_promisor(dest, &promisor)?;
    crate::commands::promisor_hydrate::trim_promisor_marker_to_missing_local(dest)?;
    Ok(())
}

fn materialize_tree_zero_partial_layout(dest: &Repository) -> Result<Vec<ObjectId>> {
    let alt = dest.git_dir.join("objects/info/alternates");
    let _ = fs::remove_file(&alt);

    let (kept, omitted) = collect_tree_zero_kept_and_omitted(dest)?;

    for oid in &kept {
        let obj = match dest.odb.read(oid) {
            Ok(o) => o,
            Err(_) => continue,
        };
        // Same reasoning as `materialize_blob_none_partial_layout`: the kept commit/tree objects
        // are still in the local pack that is deleted below, so `Odb::write`'s `exists()`
        // short-circuit would skip the loose write and leave them missing. Force a loose copy.
        let _ = dest.odb.write_loose_materialize(obj.kind, &obj.data)?;
    }

    let pack_dir = dest.git_dir.join("objects/pack");
    if pack_dir.is_dir() {
        for entry in fs::read_dir(&pack_dir)? {
            let entry = entry?;
            let path = entry.path();
            if path.is_file() {
                let _ = fs::remove_file(&path);
            }
        }
    }

    let mut kept_oids: Vec<ObjectId> = kept.into_iter().collect();
    kept_oids.sort_by_key(ObjectId::to_hex);
    if !kept_oids.is_empty() {
        let pack_path = crate::commands::pack_objects::write_partial_clone_promisor_pack(
            dest, &pack_dir, &kept_oids,
        )
        .context("writing tree:0 promisor commit pack")?;
        fs::write(
            pack_path.with_extension("promisor"),
            promisor_ref_list(dest)?,
        )
        .context("writing tree:0 promisor sidecar")?;
    }

    let mut omitted_oids: Vec<ObjectId> = omitted.into_iter().collect();
    omitted_oids.sort_by_key(ObjectId::to_hex);
    omitted_oids.dedup();
    for oid in &omitted_oids {
        let hex = oid.to_hex();
        if hex.len() < 3 {
            continue;
        }
        let loose = dest.git_dir.join("objects").join(&hex[..2]).join(&hex[2..]);
        let _ = fs::remove_file(loose);
    }

    Ok(omitted_oids)
}

fn promisor_ref_list(repo: &Repository) -> Result<String> {
    let mut lines = Vec::new();
    if let Ok(head_oid) = grit_lib::refs::resolve_ref(&repo.git_dir, "HEAD") {
        lines.push(format!("{} HEAD", head_oid.to_hex()));
    }
    if let Ok(refs) = grit_lib::refs::list_refs(&repo.git_dir, "refs/") {
        for (name, oid) in refs {
            lines.push(format!("{} {name}", oid.to_hex()));
        }
    }
    lines.sort();
    lines.dedup();
    Ok(if lines.is_empty() {
        String::new()
    } else {
        format!("{}\n", lines.join("\n"))
    })
}

/// Verify that every ref points directly at an object that is present or promised.
///
/// This does NOT descend into trees or follow tag peel chains: content blobs/trees omitted by the
/// filter — and the target of an annotated tag — are legitimately absent (promised by the promisor
/// remote), which upstream tolerates ("tolerate server not sending target of tag"). It only fails
/// when a ref itself points directly at an object the server neither sent nor promised — Git's
/// "did not send all necessary objects" ("upon cloning, check that all refs point to objects").
/// Verify that the object a detached HEAD will point to exists in the destination object store.
///
/// Mirrors upstream Git, which writes a detached HEAD via `refs_update_ref(..., REF_NO_DEREF,
/// UPDATE_REFS_DIE_ON_ERR)` — an update that verifies the target object is present. A bare local
/// clone of a source whose object store is missing the HEAD commit (e.g. corrupted source) must
/// therefore fail rather than silently produce a dangling HEAD.
///
/// Promisor (partially cloned) objects are treated as present, matching upstream's allowance for
/// objects the promisor remote will lazily provide.
///
/// # Errors
/// Returns an error when `oid_hex` is not a valid object id, or when the object is absent from the
/// destination object store and is not a promised (promisor) object.
fn verify_detached_head_object_present(dest: &Repository, oid_hex: &str) -> Result<()> {
    let oid = ObjectId::from_hex(oid_hex)
        .with_context(|| format!("invalid HEAD object id '{oid_hex}'"))?;
    if dest.odb.read(&oid).is_ok() {
        return Ok(());
    }
    let promised = grit_lib::promisor::promisor_pack_object_ids(&dest.git_dir.join("objects"));
    if promised.contains(&oid) {
        return Ok(());
    }
    bail!("unable to update HEAD: object {oid_hex} is missing")
}

fn check_clone_ref_tip_connectivity(dest: &Repository) -> Result<()> {
    let promised = grit_lib::promisor::promisor_pack_object_ids(&dest.git_dir.join("objects"));

    let mut tips: Vec<ObjectId> = Vec::new();
    if let Ok(head) = grit_lib::refs::resolve_ref(&dest.git_dir, "HEAD") {
        tips.push(head);
    }
    if let Ok(refs) = grit_lib::refs::list_refs(&dest.git_dir, "refs/") {
        for (_, oid) in refs {
            tips.push(oid);
        }
    }

    for oid in tips {
        if dest.odb.read(&oid).is_err() && !promised.contains(&oid) {
            bail!("did not send all necessary objects");
        }
    }
    Ok(())
}

/// Walk all refs and partition reachable objects into commits+trees vs blobs.
fn collect_reachable_skeleton_and_blobs(
    repo: &Repository,
) -> Result<(HashSet<ObjectId>, HashSet<ObjectId>)> {
    let mut skeleton = HashSet::new();
    let mut blobs = HashSet::new();
    let mut seen_commits = HashSet::new();
    let mut seen_trees = HashSet::new();
    let mut seen_tags = HashSet::new();
    let mut queue = VecDeque::new();

    // Blobs that a ref (or a tag chain) points at directly must stay present, like upstream
    // upload-pack, which always sends ref-tip objects (even blobs) in a filtered pack. Track
    // those OIDs so the `Blob` arm classifies them as skeleton (kept) rather than deletable
    // content blobs (t5616 "fetches blobs pointed to by refs even if normally filtered out").
    let mut ref_target_blobs: HashSet<ObjectId> = HashSet::new();

    if let Ok(head) = grit_lib::refs::resolve_ref(&repo.git_dir, "HEAD") {
        ref_target_blobs.insert(head);
        queue.push_back(head);
    }
    if let Ok(refs) = grit_lib::refs::list_refs(&repo.git_dir, "refs/") {
        for (_, oid) in refs {
            ref_target_blobs.insert(oid);
            queue.push_back(oid);
        }
    }

    while let Some(oid) = queue.pop_front() {
        let obj = match repo.odb.read(&oid) {
            Ok(o) => o,
            Err(_) => continue,
        };
        match obj.kind {
            ObjectKind::Commit => {
                if !seen_commits.insert(oid) {
                    continue;
                }
                skeleton.insert(oid);
                let commit = parse_commit(&obj.data)?;
                for p in &commit.parents {
                    queue.push_back(*p);
                }
                queue.push_back(commit.tree);
            }
            ObjectKind::Tree => {
                if !seen_trees.insert(oid) {
                    continue;
                }
                skeleton.insert(oid);
                for entry in parse_tree(&obj.data)? {
                    if entry.mode == 0o160000 {
                        continue;
                    }
                    if (entry.mode & 0o170000) == 0o040000 {
                        queue.push_back(entry.oid);
                    } else {
                        blobs.insert(entry.oid);
                    }
                }
            }
            ObjectKind::Tag => {
                if !seen_tags.insert(oid) {
                    continue;
                }
                skeleton.insert(oid);
                if let Ok(tag) = parse_tag(&obj.data) {
                    // A tag pointing directly at a blob keeps that blob present too.
                    ref_target_blobs.insert(tag.object);
                    queue.push_back(tag.object);
                }
            }
            ObjectKind::Blob => {
                if ref_target_blobs.contains(&oid) {
                    skeleton.insert(oid);
                } else {
                    blobs.insert(oid);
                }
            }
        }
    }

    // A ref-tip blob must never also be queued for deletion via a tree entry.
    blobs.retain(|oid| !skeleton.contains(oid));

    Ok((skeleton, blobs))
}

fn collect_tree_zero_kept_and_omitted(
    repo: &Repository,
) -> Result<(HashSet<ObjectId>, HashSet<ObjectId>)> {
    let mut kept = HashSet::new();
    let mut omitted = HashSet::new();
    let mut seen_commits = HashSet::new();
    let mut seen_trees = HashSet::new();
    let mut seen_tags = HashSet::new();
    let mut queue = VecDeque::new();

    if let Ok(head) = grit_lib::refs::resolve_ref(&repo.git_dir, "HEAD") {
        queue.push_back(head);
    }
    if let Ok(refs) = grit_lib::refs::list_refs(&repo.git_dir, "refs/") {
        for (_, oid) in refs {
            queue.push_back(oid);
        }
    }

    while let Some(oid) = queue.pop_front() {
        let obj = match repo.odb.read(&oid) {
            Ok(o) => o,
            Err(_) => continue,
        };
        match obj.kind {
            ObjectKind::Commit => {
                if !seen_commits.insert(oid) {
                    continue;
                }
                kept.insert(oid);
                let commit = parse_commit(&obj.data)?;
                for p in &commit.parents {
                    queue.push_back(*p);
                }
                queue.push_back(commit.tree);
            }
            ObjectKind::Tag => {
                if !seen_tags.insert(oid) {
                    continue;
                }
                kept.insert(oid);
                if let Ok(tag) = parse_tag(&obj.data) {
                    queue.push_back(tag.object);
                }
            }
            ObjectKind::Tree => {
                if !seen_trees.insert(oid) {
                    continue;
                }
                omitted.insert(oid);
                for entry in parse_tree(&obj.data)? {
                    if entry.mode != 0o160000 {
                        queue.push_back(entry.oid);
                    }
                }
            }
            ObjectKind::Blob => {
                omitted.insert(oid);
            }
        }
    }

    Ok((kept, omitted))
}

/// Initialize internal promisor metadata for `--filter=blob:none` clones.
///
/// This records reachable blob OIDs in a marker file so commands can emulate
/// missing-object accounting (`rev-list --missing=print`) and lazy-fetch traces.
fn initialize_partial_clone_state(
    source: &Repository,
    dest: &Repository,
    remote_name: &str,
    filter_spec: &str,
) -> Result<()> {
    let blobs = collect_reachable_blob_oids_from_dest_refs(source, dest)?;
    initialize_partial_clone_state_from_missing(dest, remote_name, filter_spec, blobs)
}

fn initialize_partial_clone_state_from_missing<I>(
    dest: &Repository,
    remote_name: &str,
    filter_spec: &str,
    missing_oids: I,
) -> Result<()>
where
    I: IntoIterator<Item = ObjectId>,
{
    let mut missing: Vec<String> = missing_oids.into_iter().map(|oid| oid.to_hex()).collect();
    missing.sort();
    missing.dedup();

    let marker = dest.git_dir.join("grit-promisor-missing");
    let marker_content = if missing.is_empty() {
        String::new()
    } else {
        format!("{}\n", missing.join("\n"))
    };
    fs::write(&marker, marker_content)?;

    let config_path = dest.git_dir.join("config");
    let mut config = match ConfigFile::from_path(&config_path, ConfigScope::Local)? {
        Some(c) => c,
        None => ConfigFile::parse(&config_path, "", ConfigScope::Local)?,
    };
    config.set("core.repositoryformatversion", "1")?;
    config.set("extensions.partialclone", remote_name)?;
    config.set(&format!("remote.{remote_name}.promisor"), "true")?;
    config.set(
        &format!("remote.{remote_name}.partialclonefilter"),
        filter_spec,
    )?;
    config.write().context("writing config")?;

    Ok(())
}

/// Collect blob OIDs reachable from the destination's refs, reading object contents
/// from `source` (full object store). Matches Git's reachable blob set for partial
/// clones where the destination may not yet have blob bytes locally.
fn collect_reachable_blob_oids_from_dest_refs(
    source: &Repository,
    dest: &Repository,
) -> Result<HashSet<ObjectId>> {
    let mut blobs = HashSet::new();
    let mut seen_commits = HashSet::new();
    let mut seen_trees = HashSet::new();
    let mut seen_tags = HashSet::new();
    let mut queue = VecDeque::new();
    let shallow_boundaries = grit_lib::shallow::load_shallow_boundaries(&dest.git_dir);

    // Seed only from `HEAD` (same default revision as `git rev-list` for this clone).
    if let Ok(head) = grit_lib::refs::resolve_ref(&dest.git_dir, "HEAD") {
        queue.push_back(head);
    }

    while let Some(oid) = queue.pop_front() {
        let obj = match source.odb.read(&oid) {
            Ok(obj) => obj,
            Err(_) => continue,
        };
        match obj.kind {
            ObjectKind::Commit => {
                if !seen_commits.insert(oid) {
                    continue;
                }
                let commit = parse_commit(&obj.data)?;
                if !shallow_boundaries.contains(&oid) {
                    for p in &commit.parents {
                        queue.push_back(*p);
                    }
                }
                queue.push_back(commit.tree);
            }
            ObjectKind::Tree => {
                if !seen_trees.insert(oid) {
                    continue;
                }
                for entry in parse_tree(&obj.data)? {
                    if entry.mode == 0o160000 {
                        continue;
                    }
                    if (entry.mode & 0o170000) == 0o040000 {
                        queue.push_back(entry.oid);
                    } else {
                        blobs.insert(entry.oid);
                    }
                }
            }
            ObjectKind::Tag => {
                if !seen_tags.insert(oid) {
                    continue;
                }
                if let Ok(tag) = parse_tag(&obj.data) {
                    queue.push_back(tag.object);
                }
            }
            ObjectKind::Blob => {
                blobs.insert(oid);
            }
        }
    }

    Ok(blobs)
}

/// Walk `HEAD`'s commit/tree graph in `dest` and collect blob OIDs reachable but **not present**
/// in the destination ODB — i.e. the objects the server omitted for a `--filter` clone. Used to
/// seed the promisor "missing" marker so a later checkout / diff lazily fetches them (`t5710`).
fn collect_reachable_missing_oids_in_dest(dest: &Repository) -> Result<HashSet<ObjectId>> {
    let mut missing = HashSet::new();
    let mut seen_commits = HashSet::new();
    let mut seen_trees = HashSet::new();
    let mut seen_tags = HashSet::new();
    let mut queue = VecDeque::new();

    for refname in ["HEAD"] {
        if let Ok(oid) = grit_lib::refs::resolve_ref(&dest.git_dir, refname) {
            queue.push_back(oid);
        }
    }

    while let Some(oid) = queue.pop_front() {
        let obj = match dest.odb.read(&oid) {
            Ok(obj) => obj,
            Err(_) => {
                // A reachable object we don't have locally: record it as missing.
                missing.insert(oid);
                continue;
            }
        };
        match obj.kind {
            ObjectKind::Commit => {
                if !seen_commits.insert(oid) {
                    continue;
                }
                let commit = parse_commit(&obj.data)?;
                for p in &commit.parents {
                    queue.push_back(*p);
                }
                queue.push_back(commit.tree);
            }
            ObjectKind::Tree => {
                if !seen_trees.insert(oid) {
                    continue;
                }
                for entry in parse_tree(&obj.data)? {
                    if entry.mode == 0o160000 {
                        continue;
                    }
                    if (entry.mode & 0o170000) == 0o040000 {
                        queue.push_back(entry.oid);
                    } else if !dest.odb.exists(&entry.oid) {
                        missing.insert(entry.oid);
                    }
                }
            }
            ObjectKind::Tag => {
                if !seen_tags.insert(oid) {
                    continue;
                }
                if let Ok(tag) = parse_tag(&obj.data) {
                    queue.push_back(tag.object);
                }
            }
            ObjectKind::Blob => {}
        }
    }

    Ok(missing)
}

/// Run `post-checkout` after clone: null old OID, new HEAD commit, branch flag `1`.
fn run_post_checkout_after_clone(repo: &Repository) -> Result<()> {
    let head_content = fs::read_to_string(repo.git_dir.join("HEAD")).context("reading HEAD")?;
    let head = head_content.trim();
    let new_oid = if let Some(refname) = head.strip_prefix("ref: ") {
        let refname = refname.trim();
        if !clone_ref_file_exists(&repo.git_dir, refname) {
            // Unborn branch: nothing checked out; no commit for post-checkout.
            return Ok(());
        }
        let oid_str = clone_read_direct_ref_oid(&repo.git_dir, refname)?;
        ObjectId::from_hex(oid_str.trim()).with_context(|| format!("invalid OID in {refname}"))?
    } else {
        ObjectId::from_hex(head).context("invalid OID in HEAD")?
    };
    let z = zero_oid();
    let old_hex = z.to_hex();
    let new_hex = new_oid.to_hex();
    let hook_args = [old_hex.as_str(), new_hex.as_str(), "1"];
    if let HookResult::Failed(code) = run_hook(repo, "post-checkout", &hook_args, None) {
        bail!("post-checkout hook exited with status {code}");
    }
    Ok(())
}

/// Determine which branch HEAD should point to when the user passed `-b` / `--branch`.
fn determine_head_branch(src_git_dir: &Path, requested: Option<&str>) -> Result<Option<String>> {
    if let Some(branch) = requested {
        return Ok(Some(branch.to_string()));
    }

    let head_path = src_git_dir.join("HEAD");
    if let Ok(content) = fs::read_to_string(&head_path) {
        let content = content.trim();
        if let Some(rest) = content.strip_prefix("ref: ") {
            let rest = rest.trim();
            if let Some(branch) = rest.strip_prefix("refs/heads/") {
                if clone_ref_file_exists(src_git_dir, rest) {
                    return Ok(Some(branch.to_string()));
                }
            }
        }
    }

    Ok(Some(default_head_branch_fallback()))
}

/// Read source `HEAD` as either a symref target or a raw object id.
fn read_source_head_info(src_git_dir: &Path) -> (Option<String>, Option<String>) {
    // Under `GIT_NAMESPACE`, a local clone must choose HEAD from the namespaced
    // `refs/namespaces/<ns>/HEAD` (Git resolves this through the namespace-aware transport). The
    // advertised symref target is the namespace-stripped logical ref (t5509 clone chooses HEAD).
    if grit_lib::ref_namespace::ref_storage_prefix().is_some() {
        match grit_lib::refs::read_symbolic_ref(src_git_dir, "HEAD") {
            Ok(Some(target)) => {
                let logical = grit_lib::ref_namespace::strip_namespace_prefix(&target).into_owned();
                return (Some(logical), None);
            }
            _ => {
                // No namespaced HEAD symref: fall back to a detached OID if HEAD resolves.
                if let Ok(oid) = grit_lib::refs::resolve_ref(src_git_dir, "HEAD") {
                    return (None, Some(oid.to_hex()));
                }
                return (None, None);
            }
        }
    }
    let head_path = src_git_dir.join("HEAD");
    let Ok(content) = fs::read_to_string(&head_path) else {
        return (None, None);
    };
    let content = content.trim();
    if let Some(rest) = content.strip_prefix("ref: ") {
        return (Some(rest.trim().to_string()), None);
    }
    if content.len() == 40 && content.chars().all(|c| c.is_ascii_hexdigit()) {
        return (None, Some(content.to_string()));
    }
    (None, None)
}

/// Pick `refs/remotes/<remote>/<branch>` to check out: `preferred` if present, else the
/// sole remote-tracking branch when unambiguous.
fn resolve_remote_tracked_branch_name(
    dest_git_dir: &Path,
    remote_name: &str,
    preferred: &str,
) -> Option<String> {
    let preferred_name = format!("refs/remotes/{remote_name}/{preferred}");
    if clone_ref_file_exists(dest_git_dir, &preferred_name) {
        return Some(preferred.to_string());
    }
    let prefix = format!("refs/remotes/{remote_name}/");
    let refs = grit_lib::refs::list_refs(dest_git_dir, &prefix).ok()?;
    let mut names: Vec<String> = Vec::new();
    for (full, _) in refs {
        if let Some(rest) = full.strip_prefix(&prefix) {
            if rest == "HEAD" || rest.contains('/') {
                continue;
            }
            names.push(rest.to_string());
        }
    }
    names.sort();
    if names.len() == 1 {
        Some(names[0].clone())
    } else {
        None
    }
}

fn ref_oid_hex_in_repo(git_dir: &Path, refname: &str) -> Option<String> {
    grit_lib::refs::resolve_ref(git_dir, refname)
        .ok()
        .map(|o| o.to_hex())
}

/// Guess which local branch to create from remote `HEAD` (Git `guess_remote_head` semantics).
fn guess_checkout_branch(
    src_git_dir: &Path,
    symref: Option<&str>,
    detached_oid: Option<&str>,
) -> Result<Option<String>> {
    if let Some(sr) = symref {
        if let Some(branch) = sr.strip_prefix("refs/heads/") {
            if clone_ref_file_exists(src_git_dir, sr) {
                return Ok(Some(branch.to_string()));
            }
            // Unborn branch: HEAD symref points at refs/heads/<name> but the ref file
            // is missing (matches Git clone with ls-refs unborn advertisement).
            return Ok(Some(branch.to_string()));
        }
    }

    if let Some(oid_hex) = detached_oid {
        let default_name = default_head_branch_fallback();
        let default_ref = format!("refs/heads/{default_name}");
        if let Some(oid) = ref_oid_hex_in_repo(src_git_dir, &default_ref) {
            if oid == *oid_hex {
                return Ok(Some(default_name));
            }
        }
        // `GIT_TEST_DEFAULT_INITIAL_BRANCH_NAME=main` leaves `HEAD` detached at the tip OID while
        // only `refs/heads/main` exists; match that before falling back to `master`.
        if let Some(oid) = ref_oid_hex_in_repo(src_git_dir, "refs/heads/main") {
            if oid == *oid_hex {
                return Ok(Some("main".to_string()));
            }
        }
        if let Some(oid) = ref_oid_hex_in_repo(src_git_dir, "refs/heads/master") {
            if oid == *oid_hex {
                return Ok(Some("master".to_string()));
            }
        }

        let heads = grit_lib::refs::list_refs(src_git_dir, "refs/heads/")
            .map_err(|e| anyhow::anyhow!("{e}"))?;
        for (refname, oid) in &heads {
            if oid.to_hex() == *oid_hex {
                let branch = refname.strip_prefix("refs/heads/").unwrap_or(refname);
                return Ok(Some(branch.to_string()));
            }
        }
        return Ok(None);
    }

    Ok(Some(default_head_branch_fallback()))
}

/// Set `refs/remotes/<remote>/HEAD` from an advertised `symref=HEAD:refs/heads/...` when the
/// corresponding remote-tracking ref exists (used when there is no local mirror of the server).
fn setup_remote_tracking_head_from_advertisement(
    dest_git_dir: &Path,
    remote_name: &str,
    symref: Option<&str>,
) -> Result<()> {
    let Some(sr) = symref else {
        return Ok(());
    };
    let Some(branch) = sr.strip_prefix("refs/heads/") else {
        return Ok(());
    };
    let tracked = dest_git_dir
        .join("refs/remotes")
        .join(remote_name)
        .join(branch);
    if !tracked.is_file() {
        return Ok(());
    }
    let origin_head_path = dest_git_dir
        .join("refs/remotes")
        .join(remote_name)
        .join("HEAD");
    if let Some(parent) = origin_head_path.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(
        &origin_head_path,
        format!("ref: refs/remotes/{remote_name}/{branch}\n"),
    )?;
    Ok(())
}

/// Set `refs/remotes/<remote>/HEAD` after remote-tracking refs exist.
fn setup_remote_tracking_head(
    dest_git_dir: &Path,
    remote_name: &str,
    src_git_dir: &Path,
    symref: Option<&str>,
    detached_oid: Option<&str>,
) -> Result<()> {
    let origin_head_ref = format!("refs/remotes/{remote_name}/HEAD");

    if let Some(sr) = symref {
        if let Some(branch) = sr.strip_prefix("refs/heads/") {
            if clone_ref_file_exists(src_git_dir, sr) {
                let sym_target = format!("refs/remotes/{remote_name}/{branch}");
                clone_write_symref(dest_git_dir, &origin_head_ref, &sym_target)?;
                return Ok(());
            }
            // Source default branch ref is missing (dangling HEAD): do not create
            // refs/remotes/<remote>/HEAD (matches Git clone).
            return Ok(());
        }
    }

    if let Some(oid_hex) = detached_oid {
        if let Some(branch) = guess_checkout_branch(src_git_dir, None, Some(oid_hex))? {
            let sym_target = format!("refs/remotes/{remote_name}/{branch}");
            clone_write_symref(dest_git_dir, &origin_head_ref, &sym_target)?;
        }
    }

    Ok(())
}

/// Build `.git/index` from `HEAD` when missing or empty (e.g. `clone --no-checkout` before
/// `sparse-checkout set`).
pub(crate) fn ensure_index_from_head_if_missing(repo: &Repository) -> Result<()> {
    let index_path = repo.index_path();
    if index_path.exists() {
        let idx = repo.load_index_at(&index_path).unwrap_or_default();
        if !idx.entries.is_empty() {
            return Ok(());
        }
    }
    let head_state = grit_lib::state::resolve_head(&repo.git_dir).context("reading HEAD")?;
    let Some(commit_oid) = head_state.oid() else {
        // Unborn branch or missing ref: nothing to build an index from (matches Git sparse-checkout
        // on empty repos).
        return Ok(());
    };
    let oid = commit_oid;
    let obj = repo.odb.read(&oid).context("reading HEAD commit")?;
    let commit = parse_commit(&obj.data).context("parsing HEAD commit")?;
    write_index_from_tree(repo, &commit.tree)?;
    Ok(())
}

/// Perform a basic checkout of HEAD into the working tree.
fn checkout_head(repo: &Repository) -> Result<()> {
    if repo.work_tree.is_none() {
        return Ok(()); // Bare repo
    }

    // Read HEAD
    let head_content = fs::read_to_string(repo.git_dir.join("HEAD")).context("reading HEAD")?;
    let head = head_content.trim();

    // Resolve to an OID
    let oid = if let Some(refname) = head.strip_prefix("ref: ") {
        let refname = refname.trim();
        let oid_str = clone_read_direct_ref_oid(&repo.git_dir, refname)?;
        ObjectId::from_hex(oid_str.trim()).with_context(|| format!("invalid OID in {refname}"))?
    } else {
        ObjectId::from_hex(head).context("invalid OID in HEAD")?
    };

    // Read the commit to get the tree
    let obj = repo.odb.read(&oid).context("reading HEAD commit")?;
    let commit = parse_commit(&obj.data).context("parsing HEAD commit")?;

    // Build the index from the tree first, then materialize the working tree from the index.
    // Going through the index checkout path applies EOL/encoding/process-filter smudge (including
    // delayed `status=delayed` filters), exactly like `git checkout .` after clone (t0021).
    write_index_from_tree(repo, &commit.tree)?;
    checkout_index_into_clone_worktree(repo)?;

    Ok(())
}

/// Write the index file from a tree (simple version).
fn write_index_from_tree(repo: &Repository, tree_oid: &ObjectId) -> Result<()> {
    use grit_lib::index::Index;

    // Try to build the index by reading the tree
    // Use grit's read-tree equivalent
    let index_path = repo.index_path();

    // We'll create a minimal approach: run the equivalent of `read-tree`
    // by adding entries from the tree
    let mut index = Index::new();
    add_tree_to_index(
        &repo.odb,
        tree_oid,
        "",
        &mut index,
        repo.work_tree.as_deref(),
    )?;
    repo.write_index_at(&index_path, &mut index)
        .context("writing index")?;

    Ok(())
}

/// Recursively add tree entries to an index.
fn add_tree_to_index(
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
            add_tree_to_index(odb, &entry.oid, &path, index, work_tree)?;
        } else if is_gitlink {
            // Gitlink (submodule) — add to index with mode 160000 and
            // the commit OID, but no stat info (not checked out).
            index.add_or_replace(grit_lib::index::IndexEntry {
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
                flags: path.len().min(0xFFF) as u16,
                flags_extended: None,
                path: path.as_bytes().to_vec(),
                base_index_pos: 0,
            });
        } else {
            // Get file stat info from the working tree if available
            let (ctime_sec, ctime_nsec, mtime_sec, mtime_nsec, dev, ino, uid, gid, size) =
                if let Some(wt) = work_tree {
                    let full = wt.join(&path);
                    if let Ok(meta) = fs::metadata(&full) {
                        #[cfg(unix)]
                        {
                            use std::os::unix::fs::MetadataExt;
                            (
                                meta.ctime() as u32,
                                meta.ctime_nsec() as u32,
                                meta.mtime() as u32,
                                meta.mtime_nsec() as u32,
                                meta.dev() as u32,
                                meta.ino() as u32,
                                meta.uid(),
                                meta.gid(),
                                meta.size() as u32,
                            )
                        }
                        #[cfg(not(unix))]
                        (0, 0, 0, 0, 0, 0, 0, 0, 0)
                    } else {
                        (0, 0, 0, 0, 0, 0, 0, 0, 0)
                    }
                } else {
                    (0, 0, 0, 0, 0, 0, 0, 0, 0)
                };

            index.add_or_replace(grit_lib::index::IndexEntry {
                ctime_sec,
                ctime_nsec,
                mtime_sec,
                mtime_nsec,
                dev,
                ino,
                mode: entry.mode,
                uid,
                gid,
                size,
                oid: entry.oid,
                flags: path.len().min(0xFFF) as u16,
                flags_extended: None,
                path: path.as_bytes().to_vec(),
                base_index_pos: 0,
            });
        }
    }

    Ok(())
}

/// Check if a path looks like a git bundle file.
fn is_bundle_file(path: &str) -> bool {
    let p = Path::new(path);
    if let Ok(mut f) = fs::File::open(p) {
        let mut buf = [0u8; 20];
        if let Ok(n) = std::io::Read::read(&mut f, &mut buf) {
            return buf[..n].starts_with(b"# v2 git bundle");
        }
    }
    false
}

/// Clone from a bundle file.
fn run_bundle_clone(args: Args) -> Result<()> {
    let bundle_path = PathBuf::from(&args.repository);
    let data = fs::read(&bundle_path)
        .with_context(|| format!("cannot read bundle '{}'", args.repository))?;

    // Parse bundle header
    let header_line = b"# v2 git bundle\n";
    if !data.starts_with(header_line) {
        bail!("not a v2 git bundle");
    }
    let mut pos = header_line.len();
    let mut refs: Vec<(String, grit_lib::objects::ObjectId)> = Vec::new();
    let mut bundle_filter = None::<String>;
    loop {
        let eol = data[pos..]
            .iter()
            .position(|&b| b == b'\n')
            .map(|i| pos + i)
            .ok_or_else(|| anyhow::anyhow!("truncated bundle header"))?;
        let line = &data[pos..eol];
        if line.is_empty() {
            pos = eol + 1;
            break;
        }
        let line_str = std::str::from_utf8(line)?;
        if line_str.starts_with('-') {
            pos = eol + 1;
            continue;
        }
        if let Some(cap) = line_str.strip_prefix('@') {
            if let Some(value) = cap.strip_prefix("filter=") {
                bundle_filter = Some(value.to_owned());
            }
            pos = eol + 1;
            continue;
        }
        if let Some((hex, refname)) = line_str.split_once(' ') {
            let oid = grit_lib::objects::ObjectId::from_hex(hex)
                .map_err(|e| anyhow::anyhow!("bad oid in bundle: {e}"))?;
            refs.push((refname.to_string(), oid));
        }
        pos = eol + 1;
    }
    if bundle_filter.is_some() {
        bail!("cannot clone from filtered bundle");
    }

    // Determine target directory
    let target_name = args.directory.clone().unwrap_or_else(|| {
        let fname = bundle_path
            .file_name()
            .unwrap_or_default()
            .to_string_lossy();
        fname
            .strip_suffix(".bundle")
            .unwrap_or(fname.as_ref())
            .to_string()
    });
    let target_path = PathBuf::from(&target_name);
    if target_path.exists() {
        bail!(
            "destination path '{}' already exists",
            target_path.display()
        );
    }

    if !args.quiet {
        eprintln!("Cloning into '{}'...", target_name);
    }

    // Figure out default branch from refs. When the bundle records `HEAD` but no `refs/heads/*`
    // points at the same object (orphan HEAD), Git initializes the default branch unborn — the
    // clone must not create `refs/heads/<tip>` for a branch that only exists as a remote-tracking
    // ref (`t5605` b5.bundle).
    let has_bundle_head_line = refs.iter().any(|(r, _)| r == "HEAD");
    let head_oid_in_bundle = refs.iter().find(|(r, _)| r == "HEAD").map(|(_, oid)| *oid);
    let branches_matching_head: Vec<&str> = if let Some(h) = head_oid_in_bundle {
        refs.iter()
            .filter(|(r, oid)| r.starts_with("refs/heads/") && *oid == h)
            .map(|(r, _)| r.strip_prefix("refs/heads/").unwrap_or(r))
            .collect()
    } else {
        Vec::new()
    };

    let fallback_branch = default_head_branch_fallback();

    let head_branch_no_head_line = {
        let branches: Vec<_> = refs
            .iter()
            .filter(|(r, _)| r.starts_with("refs/heads/"))
            .collect();
        if branches.len() == 1 {
            branches[0]
                .0
                .strip_prefix("refs/heads/")
                .unwrap_or(&branches[0].0)
                .to_string()
        } else if branches.is_empty() {
            fallback_branch.clone()
        } else {
            branches
                .iter()
                .find(|(r, _)| r.ends_with("/main"))
                .or_else(|| branches.iter().find(|(r, _)| r.ends_with("/master")))
                .or(branches.first())
                .map(|(r, _)| r.strip_prefix("refs/heads/").unwrap_or(r).to_string())
                .unwrap_or_else(|| fallback_branch.clone())
        }
    };

    // When the bundle includes `HEAD`, the new repo's initial branch name follows
    // `init.defaultBranch`. When `HEAD` is absent (`git bundle create b5.bundle not-main`), Git
    // still does the same if the only listed branch is not that default (`t5605`).
    let head_branch = if has_bundle_head_line {
        fallback_branch.clone()
    } else {
        let branches: Vec<_> = refs
            .iter()
            .filter(|(r, _)| r.starts_with("refs/heads/"))
            .collect();
        if branches.len() == 1 {
            let sole = branches[0]
                .0
                .strip_prefix("refs/heads/")
                .unwrap_or(&branches[0].0);
            if sole == fallback_branch.as_str() {
                sole.to_string()
            } else {
                fallback_branch.clone()
            }
        } else {
            head_branch_no_head_line.clone()
        }
    };

    let orphan_bundle_head = if has_bundle_head_line {
        head_oid_in_bundle.is_some()
            && (branches_matching_head.is_empty()
                || !branches_matching_head
                    .iter()
                    .any(|b| *b == fallback_branch.as_str()))
    } else {
        let branches: Vec<_> = refs
            .iter()
            .filter(|(r, _)| r.starts_with("refs/heads/"))
            .collect();
        branches.len() == 1
            && branches[0]
                .0
                .strip_prefix("refs/heads/")
                .unwrap_or(&branches[0].0)
                != fallback_branch.as_str()
    };

    let ref_storage = resolved_clone_ref_storage(&args)?;

    // Initialize target repo
    fs::create_dir_all(&target_path)?;
    let dest = init_repository(&target_path, args.bare, &head_branch, None, ref_storage)
        .with_context(|| format!("failed to initialize '{}'", target_path.display()))?;

    // Unbundle pack data
    let pack_data = &data[pos..];
    if pack_data.len() >= 12 + 20 {
        // Git's bundle-fetch transport runs `index-pack --fsck-objects` when
        // fetch/transfer.fsckObjects is set (transport.c `fetch_refs_from_bundle`),
        // so a bundle carrying malformed objects aborts the clone. Reproduce that
        // content fsck before writing anything to the ODB.
        if bundle_fetch_fsck_enabled() {
            bundle_clone_fsck_pack(pack_data, &dest.odb)?;
        }
        let opts = grit_lib::unpack_objects::UnpackOptions {
            strict: false,
            dry_run: false,
            quiet: true,
            allowed_missing: Default::default(),
            allow_promisor_missing_references: false,
            max_input_bytes: None,
            ..Default::default()
        };
        grit_lib::unpack_objects::unpack_objects(&mut &pack_data[..], &dest.odb, &opts)
            .map_err(|e| anyhow::anyhow!("unbundle failed: {e}"))?;
    }

    if args.mirror {
        for (refname, oid) in &refs {
            if refname == "HEAD" {
                continue;
            }
            if refname.starts_with("refs/") {
                clone_write_direct_ref(&dest.git_dir, refname, &oid.to_hex())?;
            }
        }
    } else {
        // Write refs as remote tracking refs under origin/
        for (refname, oid) in &refs {
            if refname == "HEAD" {
                continue;
            }
            // Write as remote tracking ref
            if let Some(branch) = refname.strip_prefix("refs/heads/") {
                let dst_name = format!("refs/remotes/origin/{branch}");
                clone_write_direct_ref(&dest.git_dir, &dst_name, &oid.to_hex())?;
            }
            // Also write tags directly
            if refname.starts_with("refs/tags/") {
                clone_write_direct_ref(&dest.git_dir, refname, &oid.to_hex())?;
            }
        }
    }

    // Create the local branch only when the bundle lists that branch under `refs/heads/`.
    if !args.mirror && !orphan_bundle_head {
        if let Some((_, oid)) = refs
            .iter()
            .find(|(r, _)| r == &format!("refs/heads/{head_branch}"))
        {
            let branch_ref_name = format!("refs/heads/{head_branch}");
            clone_write_direct_ref(&dest.git_dir, &branch_ref_name, &oid.to_hex())?;
        }
    }

    // Set up origin remote config
    let bundle_abs = fs::canonicalize(&bundle_path).unwrap_or(bundle_path);
    if args.mirror {
        let url = bundle_abs.to_string_lossy();
        setup_remote_mirror_fetch_and_url(&dest.git_dir, &url, "origin")?;
    } else {
        let refspec = "+refs/heads/*:refs/remotes/origin/*".to_string();
        setup_origin_remote(&dest.git_dir, &bundle_abs, "origin", &refspec)?;
    }

    // Checkout if not bare
    if !args.bare {
        if orphan_bundle_head && !args.quiet {
            eprintln!("warning: remote HEAD refers to nonexistent ref, unable to checkout");
        }
        if orphan_bundle_head {
            checkout_head_allow_unborn(&dest)?;
        } else {
            checkout_head(&dest)?;
        }
        run_post_checkout_after_clone(&dest)?;
    }

    Ok(())
}

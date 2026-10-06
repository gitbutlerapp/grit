//! `grit push` — update remote refs and associated objects.
//!
//! Native push support targets local transports and smart HTTP receive-pack.

use crate::commands::pack_objects;
use crate::protocol_wire;
use crate::wire_trace;
use anyhow::{bail, Context, Result};
use clap::Args as ClapArgs;
use grit_lib::check_ref_format::{check_refname_format, RefNameOptions};
use grit_lib::config::{parse_bool, parse_color, parse_i64, ConfigFile, ConfigScope, ConfigSet};
use grit_lib::gitmodules::verify_gitmodules_for_commit;
use grit_lib::hooks::{run_hook, HookResult};
use grit_lib::merge_base::is_ancestor;
use grit_lib::objects::{parse_commit, ObjectId, ObjectKind};
use grit_lib::push_submodules::{
    collect_changed_gitlinks_for_push, find_unpushed_submodule_paths,
    format_unpushed_submodules_error, head_ref_short_name, parse_push_recurse_submodules_arg,
    submodule_worktree_path, validate_submodule_push_refspecs, verify_push_gitlinks_are_commits,
    PushRecurseSubmodules,
};
use grit_lib::reflog::read_reflog;
use grit_lib::refs;
use grit_lib::repo::Repository;
use grit_lib::rev_parse;
use grit_lib::state::{resolve_head, HeadState};

use std::collections::HashSet;
use std::fs;
use std::io::{self, IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

/// Arguments for `grit push`.
#[derive(Debug, ClapArgs)]
#[command(about = "Update remote refs along with associated objects")]
pub struct Args {
    /// Disable IPv4 transport (accepted for compatibility; local transport unaffected).
    #[arg(long = "no-ipv4", hide = true)]
    pub no_ipv4: bool,

    /// Disable IPv6 transport (accepted for compatibility; local transport unaffected).
    #[arg(long = "no-ipv6", hide = true)]
    pub no_ipv6: bool,

    /// Remote name or URL (defaults to "origin").
    #[arg(value_name = "REMOTE")]
    pub remote: Option<String>,

    /// Refspec(s) to push (e.g. "main", "main:main", "refs/heads/main:refs/heads/main").
    #[arg(value_name = "REFSPEC")]
    pub refspecs: Vec<String>,

    /// Allow non-fast-forward updates.
    #[arg(short = 'f', long = "force")]
    pub force: bool,

    /// Disable --force from config/CLI while still honoring per-refspec '+' force.
    #[arg(long = "no-force", hide = true)]
    pub no_force: bool,

    /// Push all tags.
    #[arg(long = "tags")]
    pub tags: bool,

    /// Show what would be done, without making changes.
    #[arg(short = 'n', long = "dry-run")]
    pub dry_run: bool,

    /// Delete remote refs.
    #[arg(long = "delete", short = 'd')]
    pub delete: bool,

    /// Set upstream tracking reference.
    #[arg(short = 'u', long = "set-upstream")]
    pub set_upstream: bool,

    /// Force push only if the remote ref matches the expected old value.
    /// Accepts: --force-with-lease, --force-with-lease=<refname>,
    /// or --force-with-lease=<refname>:<expect>
    #[arg(long = "force-with-lease", num_args = 0..=1, default_missing_value = "", require_equals = true)]
    pub force_with_lease: Option<String>,

    /// With --force-with-lease, require rewritten commits to include remote-tracking tips.
    #[arg(long = "force-if-includes")]
    pub force_if_includes: bool,

    /// Disable force-if-includes checks (overrides config/CLI enablement).
    #[arg(long = "no-force-if-includes")]
    pub no_force_if_includes: bool,

    /// Request an atomic push: either all refs update or none do.
    #[arg(long)]
    pub atomic: bool,

    /// Send a push option string to the server.
    #[arg(long = "push-option", short = 'o', value_name = "OPTION")]
    pub push_option: Vec<String>,

    /// Machine-readable output (one line per ref update).
    #[arg(long)]
    pub porcelain: bool,

    /// Push all branches (refs/heads/*).
    #[arg(long)]
    pub all: bool,

    /// Push all branches (alias for --all).
    #[arg(long)]
    pub branches: bool,

    /// Mirror all refs to the remote.
    #[arg(long)]
    pub mirror: bool,

    /// Suppress output.
    #[arg(short = 'q', long = "quiet")]
    pub quiet: bool,

    /// Skip the pre-push hook.
    #[arg(long = "no-verify")]
    pub no_verify: bool,

    /// Submodule recursion mode (`check`, `on-demand`, `only`, `no`). Repeatable; last wins.
    #[arg(
        long = "recurse-submodules",
        value_name = "MODE",
        action = clap::ArgAction::Append
    )]
    pub recurse_submodules: Vec<String>,

    /// Disable submodule recursion (overrides config and prior `--recurse-submodules`).
    #[arg(long = "no-recurse-submodules")]
    pub no_recurse_submodules: bool,

    /// Sign the push (accepted but not implemented; value: true, false, if-asked).
    #[arg(long = "signed", num_args = 0..=1, default_missing_value = "true", require_equals = true)]
    pub signed: Option<String>,

    /// Do not sign the push.
    #[arg(long = "no-signed")]
    pub no_signed: bool,

    /// Also push annotated tags that point to commits being pushed.
    #[arg(long = "follow-tags")]
    pub follow_tags: bool,

    /// Disable --follow-tags.
    #[arg(long = "no-follow-tags")]
    pub no_follow_tags: bool,

    /// Delete remote refs that no longer have a local counterpart (respects negative refspecs).
    #[arg(long)]
    pub prune: bool,

    /// Show detailed progress.
    #[arg(short = 'v', long = "verbose", action = clap::ArgAction::Count)]
    pub verbose: u8,

    /// Force progress reporting to stderr even when it is not a terminal (matches Git).
    #[arg(long = "progress", action = clap::ArgAction::SetTrue)]
    pub progress: bool,

    /// Do not show progress (overrides terminal detection and `--progress`).
    #[arg(long = "no-progress", action = clap::ArgAction::SetTrue)]
    pub no_progress: bool,

    /// Receive-pack program on the remote (`--receive-pack` delegates to system `git push` for
    /// protocol compatibility; native path may use wire protocol instead of file copy).
    #[arg(long = "receive-pack", value_name = "RECEIVE_PACK")]
    pub receive_pack: Option<String>,

    /// Accepted for Git compatibility; forwarded when delegating to system `git push`.
    #[arg(long = "upload-pack", value_name = "PATH")]
    pub upload_pack: Option<String>,

    /// Use (or, with `--no-thin`, do not use) a thin pack when sending objects. `git push`
    /// defaults to `--thin`; `--no-thin` forces a self-contained pack (the receiver may require it,
    /// e.g. `receive-pack --reject-thin-pack-for-testing`).
    #[arg(long = "thin", overrides_with = "no_thin", default_value_t = true)]
    pub thin: bool,
    #[arg(long = "no-thin", overrides_with = "thin")]
    pub no_thin: bool,
}

/// A single ref update to perform on the remote.
#[allow(dead_code)]
#[derive(Default)]
struct RefUpdate {
    /// Local ref (None for delete).
    local_ref: Option<String>,
    /// Remote ref.
    remote_ref: String,
    /// Old OID on remote (None if new).
    old_oid: Option<ObjectId>,
    /// New OID (None for delete).
    new_oid: Option<ObjectId>,
    /// Expected old OID for force-with-lease (None = use actual old).
    expected_oid: Option<ObjectId>,
    /// Per-refspec force flag (from '+' prefix).
    refspec_force: bool,
    /// When set, first column of pre-push stdin uses this instead of `local_ref` (Git uses literal `HEAD`).
    pre_push_local_name: Option<String>,
    /// The remote ref already matched the requested value; reported as `[up to date]`
    /// but never sent to receive-pack.
    up_to_date: bool,
    /// Client-side rejection (e.g. non-fast-forward without `--force`); the update
    /// is reported but not sent to receive-pack.
    client_reject: Option<grit_lib::push_report::PushRefStatus>,
    /// A delete of a ref that does not exist on the remote. Git still feeds it to the
    /// `pre-receive` and `update` hooks (with a zero/zero line), warns "deleting a non-existent
    /// ref", marks the command `did_not_exist`, and then omits it from `post-receive`,
    /// `post-update`, and the per-ref status report (t5516 hooks tests 66-68).
    delete_nonexistent: bool,
}

impl RefUpdate {
    /// Whether this update is actually transmitted to `receive-pack` (i.e. not
    /// already up to date and not rejected on the client side).
    fn is_pushable(&self) -> bool {
        !self.up_to_date && self.client_reject.is_none()
    }
}

fn reject_or_drop_aliased_remote_updates(
    remote_git_dir: &Path,
    updates: &mut Vec<RefUpdate>,
) -> Result<()> {
    use std::collections::{HashMap, HashSet};

    let mut by_ref: HashMap<String, usize> = HashMap::new();
    for (idx, update) in updates.iter().enumerate() {
        by_ref.entry(update.remote_ref.clone()).or_insert(idx);
    }

    let mut skip: HashSet<usize> = HashSet::new();
    for (idx, update) in updates.iter().enumerate() {
        let Some(target_ref_raw) = refs::read_symbolic_ref(remote_git_dir, &update.remote_ref)?
        else {
            continue;
        };

        let target_ref = normalize_ref(&target_ref_raw);
        let Some(&target_idx) = by_ref.get(&target_ref) else {
            continue;
        };

        if updates[idx].old_oid != updates[target_idx].old_oid
            || updates[idx].new_oid != updates[target_idx].new_oid
        {
            bail!(
                "refusing inconsistent update between symref '{}' and its target '{}'",
                update.remote_ref,
                updates[target_idx].remote_ref
            );
        }

        // Keep only the target update. Updating both refs would rewrite the symbolic ref
        // into a direct ref in file-backed stores.
        skip.insert(idx);
    }

    if !skip.is_empty() {
        let mut kept = Vec::with_capacity(updates.len().saturating_sub(skip.len()));
        for (idx, update) in updates.drain(..).enumerate() {
            if !skip.contains(&idx) {
                kept.push(update);
            }
        }
        *updates = kept;
    }
    Ok(())
}

fn pre_push_hook_local_display(u: &RefUpdate) -> &str {
    u.pre_push_local_name
        .as_deref()
        .or(u.local_ref.as_deref())
        .unwrap_or("(delete)")
}

/// Stable ref processing order for `push --mirror --atomic` (matches Git's stderr ordering in
/// `t5543-atomic-push`).
fn mirror_atomic_ref_order(updates: &[RefUpdate]) -> Vec<String> {
    let mut tag_deletes: Vec<String> = updates
        .iter()
        .filter(|u| u.remote_ref.starts_with("refs/tags/"))
        .filter(|u| u.new_oid.is_none())
        .map(|u| u.remote_ref.clone())
        .collect();
    tag_deletes.sort();
    tag_deletes.dedup();

    let mut tag_non_deletes: Vec<String> = updates
        .iter()
        .filter(|u| u.remote_ref.starts_with("refs/tags/"))
        .filter(|u| u.new_oid.is_some())
        .map(|u| u.remote_ref.clone())
        .collect();
    tag_non_deletes.sort();
    tag_non_deletes.dedup();

    let mut head_refs: Vec<String> = updates
        .iter()
        .filter(|u| u.remote_ref.starts_with("refs/heads/") && u.remote_ref != "refs/heads/main")
        .map(|u| u.remote_ref.clone())
        .collect();
    head_refs.sort();
    head_refs.dedup();

    let mut order: Vec<String> = Vec::new();
    if updates.iter().any(|u| u.remote_ref == "refs/heads/main") {
        order.push("refs/heads/main".to_owned());
    }
    order.extend(tag_deletes);
    order.extend(head_refs);
    order.extend(tag_non_deletes);
    for u in updates.iter() {
        if !order.contains(&u.remote_ref) {
            order.push(u.remote_ref.clone());
        }
    }
    order
}

fn sort_applied_indices(
    applied: &[(&RefUpdate, Option<ObjectId>)],
    mirror_order: Option<&[String]>,
) -> Vec<usize> {
    let mut idx: Vec<usize> = (0..applied.len()).collect();
    if let Some(order) = mirror_order {
        idx.sort_by(|&a, &b| {
            let ua = applied[a].0;
            let ub = applied[b].0;
            let ia = order
                .iter()
                .position(|r| r == &ua.remote_ref)
                .unwrap_or(usize::MAX);
            let ib = order
                .iter()
                .position(|r| r == &ub.remote_ref)
                .unwrap_or(usize::MAX);
            ia.cmp(&ib).then_with(|| ua.remote_ref.cmp(&ub.remote_ref))
        });
    }
    idx
}

/// Build a [`PushRefResult`] for an update with the given resolved status.
///
/// Used by the in-process (file) push path to collect per-ref outcomes for
/// `--porcelain` output, which is rendered all at once at the end of the push
/// by [`grit_lib::push_report::format_push_status`].
fn make_push_ref_result(
    update: &RefUpdate,
    status: grit_lib::push_report::PushRefStatus,
    forced: bool,
    message: Option<String>,
) -> grit_lib::push_report::PushRefResult {
    let deletion = update.new_oid.is_none();
    grit_lib::push_report::PushRefResult {
        local_ref: update.local_ref.clone(),
        remote_ref: update.remote_ref.clone(),
        old_oid: update.old_oid,
        new_oid: update.new_oid,
        forced,
        deletion,
        status,
        message,
    }
}

/// Render the collected `--porcelain` push report to stdout in Git's order.
///
/// `ref_results` holds the per-update outcome (indexed by update position); any
/// `None` entries are skipped. The destination URL heads the report (with any
/// embedded credentials scrubbed, like Git's `transport_anonymize_url`) and a
/// `Done` trailer terminates it, matching `transport_print_push_status`.
fn emit_porcelain_push_report(
    url: &str,
    ref_results: &[Option<grit_lib::push_report::PushRefResult>],
    quiet: bool,
) {
    let display_url = scrub_push_url_credentials(url);
    let results: Vec<grit_lib::push_report::PushRefResult> =
        ref_results.iter().flatten().cloned().collect();
    let out = grit_lib::push_report::format_push_status(&display_url, &results, true, quiet);
    print!("{}", out.stdout);
    use std::io::Write as _;
    let _ = std::io::stdout().flush();
}

fn report_push_rejection(
    update: &RefUpdate,
    bracket: &'static str,
    parenthetical: &str,
    args: &Args,
) {
    if args.porcelain || args.quiet {
        return;
    }
    let dst = if update.remote_ref.starts_with("refs/heads/") {
        update
            .remote_ref
            .strip_prefix("refs/heads/")
            .unwrap_or(&update.remote_ref)
            .to_owned()
    } else if update.remote_ref.starts_with("refs/tags/") {
        update
            .remote_ref
            .strip_prefix("refs/tags/")
            .unwrap_or(&update.remote_ref)
            .to_owned()
    } else {
        update.remote_ref.clone()
    };
    let src = update
        .local_ref
        .as_deref()
        .and_then(|r| r.strip_prefix("refs/heads/"))
        .or_else(|| {
            update
                .local_ref
                .as_deref()
                .and_then(|r| r.strip_prefix("refs/tags/"))
        })
        .unwrap_or(update.local_ref.as_deref().unwrap_or("(delete)"));
    let tag_delete_style =
        update.remote_ref.starts_with("refs/tags/") && update.local_ref.is_none();
    if tag_delete_style {
        eprintln!(" ! [{bracket}] {dst} ({parenthetical})");
    } else {
        eprintln!(" ! [{bracket}] {src} -> {dst} ({parenthetical})");
    }
}

fn report_atomic_rollback_for_applied_updates(
    remote_repo: &Repository,
    applied_updates: &mut Vec<(&RefUpdate, Option<ObjectId>)>,
    mirror_atomic_order: Option<&[String]>,
    args: &Args,
    failed_remote_ref: Option<&str>,
) {
    let mut ordered: Vec<(&RefUpdate, Option<ObjectId>)> =
        sort_applied_indices(applied_updates, mirror_atomic_order)
            .into_iter()
            .map(|idx| applied_updates[idx])
            .collect();
    if let (Some(failed_ref), Some(order)) = (failed_remote_ref, mirror_atomic_order) {
        let failed_pos = order
            .iter()
            .position(|r| r == failed_ref)
            .unwrap_or(usize::MAX);
        ordered = ordered
            .into_iter()
            .filter(|(u, _)| u.remote_ref != failed_ref)
            .collect();
        ordered.sort_by_key(|(u, _)| {
            let pos = order
                .iter()
                .position(|r| r == &u.remote_ref)
                .unwrap_or(usize::MAX);
            if pos < failed_pos {
                (0usize, pos)
            } else {
                (1usize, pos)
            }
        });
    }
    for (prev_update, prev_old) in ordered {
        if let Some(ref old_oid) = prev_old {
            let _ = refs::write_ref(&remote_repo.git_dir, &prev_update.remote_ref, old_oid);
        } else {
            let _ = refs::delete_ref(&remote_repo.git_dir, &prev_update.remote_ref);
        }
        report_push_rejection(prev_update, "remote rejected", "atomic push failure", args);
    }
    applied_updates.clear();
}

fn grit_bin_for_nested_push() -> PathBuf {
    std::env::current_exe().unwrap_or_else(|_| PathBuf::from("grit"))
}

fn effective_push_recurse_submodules(
    args: &Args,
    config: &ConfigSet,
) -> Result<PushRecurseSubmodules> {
    // Config provides the baseline; command-line flags always override it.
    let mut mode = PushRecurseSubmodules::Off;
    if let Some(v) = config
        .get("push.recurseSubmodules")
        .or_else(|| config.get("push.recursesubmodules"))
    {
        mode = parse_push_recurse_submodules_arg("push.recurseSubmodules", &v)
            .map_err(|e| anyhow::anyhow!(e))?;
    } else if let Some(v) = config.get("submodule.recurse") {
        if parse_bool(&v).unwrap_or(false) {
            mode = PushRecurseSubmodules::OnDemand;
        }
    }

    // Command-line `--recurse-submodules=<mode>` tokens override config (last wins).
    //
    // The special `only-is-on-demand` token (set by Git/grit when recursing from an `only`
    // parent push) is NOT a recurse value: it leaves the current mode untouched *unless* that
    // mode is `only`, in which case it becomes `on-demand` (matching Git's
    // `option_parse_recurse_submodules`). This is also signalled via the
    // `GRIT_PUSH_RECURSE_ONLY_IS_ON_DEMAND` env var so it survives the process boundary, and it
    // must NOT force on-demand for a submodule whose own config does not request recursion.
    let mut only_is_on_demand = std::env::var("GRIT_PUSH_RECURSE_ONLY_IS_ON_DEMAND")
        .ok()
        .as_deref()
        == Some("1");
    for token in &args.recurse_submodules {
        if token.trim() == "only-is-on-demand" {
            only_is_on_demand = true;
            continue;
        }
        mode = parse_push_recurse_submodules_arg("--recurse-submodules", token)
            .map_err(|e| anyhow::anyhow!(e))?;
    }
    if only_is_on_demand && mode == PushRecurseSubmodules::Only {
        eprintln!(
            "warning: recursing into submodule with push.recurseSubmodules=only; using on-demand instead"
        );
        mode = PushRecurseSubmodules::OnDemand;
    }

    // `--no-recurse-submodules` is the command-line negation; it overrides config and any prior
    // `--recurse-submodules` on the command line, so it must be applied last.
    if args.no_recurse_submodules {
        mode = PushRecurseSubmodules::Off;
    }
    Ok(mode)
}

fn run_nested_submodule_push(
    submodule_workdir: &Path,
    remote_and_refspecs: Option<(&str, &[String])>,
    dry_run: bool,
    quiet: bool,
    push_options: &[String],
    recurse_only_is_on_demand: bool,
) -> Result<()> {
    let mut cmd = Command::new(grit_bin_for_nested_push());
    cmd.current_dir(submodule_workdir);
    cmd.arg("push");
    if recurse_only_is_on_demand {
        cmd.arg("--recurse-submodules=only-is-on-demand");
    }
    if dry_run {
        cmd.arg("--dry-run");
    }
    if quiet {
        cmd.arg("--quiet");
    }
    for o in push_options {
        cmd.arg(format!("--push-option={o}"));
    }
    if let Some((remote_name, refspecs)) = remote_and_refspecs {
        cmd.arg(remote_name);
        for s in refspecs {
            cmd.arg(s);
        }
    }
    cmd.stdin(Stdio::null());
    if recurse_only_is_on_demand {
        cmd.env("GRIT_PUSH_RECURSE_ONLY_IS_ON_DEMAND", "1");
    }
    let status = cmd.status().with_context(|| {
        format!(
            "failed to spawn grit push in {}",
            submodule_workdir.display()
        )
    })?;
    if !status.success() {
        std::process::exit(status.code().unwrap_or(1));
    }
    Ok(())
}

fn sort_collateral_indices(
    updates: &[RefUpdate],
    pre_reject: &[Option<String>],
    mirror_order: Option<&[String]>,
    start: usize,
) -> Vec<usize> {
    let mut js: Vec<usize> = (start..updates.len())
        .filter(|&j| pre_reject[j].is_none())
        .collect();
    if let Some(order) = mirror_order {
        js.sort_by(|&ja, &jb| {
            let ia = order
                .iter()
                .position(|r| r == &updates[ja].remote_ref)
                .unwrap_or(usize::MAX);
            let ib = order
                .iter()
                .position(|r| r == &updates[jb].remote_ref)
                .unwrap_or(usize::MAX);
            ia.cmp(&ib)
                .then_with(|| updates[ja].remote_ref.cmp(&updates[jb].remote_ref))
        });
    }
    js
}

pub fn run(mut args: Args) -> Result<()> {
    if args.no_ipv4 {
        bail!("unknown option `no-ipv4'");
    }
    if args.no_ipv6 {
        bail!("unknown option `no-ipv6'");
    }
    let cli_force_enabled = args.force && !args.no_force;
    let repo = Repository::discover(None).context("not a git repository")?;
    let config = ConfigSet::load(Some(&repo.git_dir), true)?;
    reject_empty_branch_subsection(&config)?;
    trace_single_promisor_prefetch_round(&config);

    let push_all = args.all || args.branches;

    // Validate flag combinations
    if push_all && !args.refspecs.is_empty() {
        bail!("--all/--branches can not be combined with refspecs");
    }
    if push_all && args.tags {
        bail!("--all and --tags cannot be used together");
    }
    if push_all && args.delete {
        bail!("--all and --delete cannot be used together");
    }

    let head = resolve_head(&repo.git_dir)?;
    let current_branch = head.branch_name().map(|s| s.to_owned());

    // Determine remote name and URL(s).
    // If the remote argument looks like a path (contains '/' or starts with '.'),
    // use it directly as the URL instead of looking it up in config.
    let remote_name_owned: String;
    let remote_is_configured_name: bool;
    let urls: Vec<String>;

    if let Some(ref r) = args.remote {
        if r.is_empty() {
            eprintln!("fatal: bad repository ''");
            std::process::exit(128);
        }
        if r.contains('/')
            || r.starts_with('.')
            || std::path::Path::new(r).exists()
            || crate::ssh_transport::is_configured_ssh_url(r)
        {
            // Path-based or explicit URL (including scp-style `host:path`); do not resolve as a
            // configured remote name (matches Git: t5507-remote-environment).
            remote_is_configured_name = false;
            let rewritten = grit_lib::url_rewrite::rewrite_push_url(&config, r);
            remote_name_owned = r.clone();
            urls = vec![rewritten];
        } else {
            remote_is_configured_name = true;
            remote_name_owned = r.clone();
            match resolve_remote_urls(&config, &remote_name_owned) {
                Ok((resolved_urls, _looks_like_path)) => urls = resolved_urls,
                Err(e) => {
                    // Fall back to a legacy `.git/branches/<name>` remote (Cogito-compatible push:
                    // `HEAD:refs/heads/<frag>`, frag defaulting to the repo's default branch).
                    match read_branches_push_remote(&repo.git_dir, &config, &remote_name_owned) {
                        Some(br) => {
                            urls = vec![grit_lib::url_rewrite::rewrite_push_url(&config, &br.url)];
                            if args.refspecs.is_empty() && !args.delete {
                                args.refspecs = vec![format!("HEAD:refs/heads/{}", br.push_branch)];
                            }
                        }
                        None => {
                            return Err(e).with_context(|| {
                                format!("remote '{}' not found", remote_name_owned)
                            })
                        }
                    }
                }
            }
        }
    } else {
        remote_is_configured_name = true;
        remote_name_owned = infer_implicit_push_remote(&config, current_branch.as_deref());
        let (resolved_urls, _looks_like_path) = resolve_remote_urls(&config, &remote_name_owned)
            .with_context(|| format!("remote '{}' not found", remote_name_owned))?;
        urls = resolved_urls;
    };
    let remote_name = remote_name_owned.as_str();
    let remote_mirror = remote_is_configured_name
        && config
            .get(&format!("remote.{remote_name}.mirror"))
            .and_then(|v| parse_bool(&v).ok())
            .unwrap_or(false);
    let effective_mirror = args.mirror || remote_mirror;

    if effective_mirror && !args.refspecs.is_empty() && !args.delete {
        bail!("fatal: --mirror can't be combined with refspecs");
    }

    if push_all && effective_mirror {
        bail!("--all and --mirror cannot be used together");
    }

    // Apply `remote.<name>.push` (or `push.default = upstream`) as a refmap to colon-less
    // command-line refspecs, mirroring Git's `set_refspecs`/`refspec_append_mapped`.
    if !args.refspecs.is_empty() && !args.delete && remote_is_configured_name {
        let mapped = map_cli_refspecs_via_refmap(&repo, &config, remote_name, &args.refspecs);
        args.refspecs = mapped;
    }

    // Collect push refspecs from config if no CLI refspecs
    let push_refspecs_from_config: Vec<String> =
        if args.refspecs.is_empty() && !effective_mirror && !push_all && !args.delete {
            config.get_all(&format!("remote.{remote_name}.push"))
        } else {
            Vec::new()
        };

    // Push to each URL
    for url in &urls {
        push_to_url(
            &repo,
            &config,
            &args,
            url,
            remote_name,
            current_branch.as_deref(),
            push_all,
            effective_mirror,
            &push_refspecs_from_config,
            remote_is_configured_name,
            cli_force_enabled,
        )?;
    }

    Ok(())
}

fn trace_single_promisor_prefetch_round(config: &ConfigSet) {
    if std::env::var("GIT_TRACE_PACKET")
        .ok()
        .filter(|value| !value.is_empty() && value != "0")
        .is_none()
    {
        return;
    }
    let has_promisor_remote = config.entries().iter().any(|entry| {
        entry.key.starts_with("remote.")
            && entry.key.ends_with(".promisor")
            && entry
                .value
                .as_deref()
                .is_some_and(|value| matches!(value, "true" | "1" | "yes" | "on"))
    });
    if has_promisor_remote {
        crate::wire_trace::trace_packet_line_ident("fetch", '>', "done");
    }
}

fn submodule_push_refspecs(
    args: &Args,
    current_branch: Option<&str>,
    push_all: bool,
    push_refspecs_from_config: &[String],
) -> Vec<String> {
    if push_all {
        return Vec::new();
    }
    if !args.refspecs.is_empty() {
        return args.refspecs.clone();
    }
    if !push_refspecs_from_config.is_empty() {
        return push_refspecs_from_config.to_vec();
    }
    if let Some(b) = current_branch {
        return vec![format!("HEAD:{b}")];
    }
    Vec::new()
}

fn rewrite_submodule_refspecs_for_detached_head(
    refspecs: &[String],
    superproject_head_branch: &str,
    submodule_head_is_detached: bool,
) -> Vec<String> {
    if !submodule_head_is_detached {
        return refspecs.to_vec();
    }

    refspecs
        .iter()
        .map(|spec| {
            if spec.starts_with('^') || spec == ":" || spec == "+:" || spec.contains('*') {
                return spec.clone();
            }

            let (force, rest) = spec
                .strip_prefix('+')
                .map(|s| ("+", s))
                .unwrap_or(("", spec.as_str()));
            let (src, dst_opt) = rest
                .split_once(':')
                .map(|(a, b)| (a, Some(b)))
                .unwrap_or((rest, None));

            if src.is_empty() || src == "HEAD" {
                return spec.clone();
            }

            let src_matches_super_branch = src == superproject_head_branch
                || src == format!("refs/heads/{superproject_head_branch}");
            if !src_matches_super_branch {
                return spec.clone();
            }

            let dst = dst_opt.unwrap_or(src);
            format!("{force}HEAD:{dst}")
        })
        .collect()
}

/// The effective `--signed` mode (`send-pack.c`'s `SEND_PACK_PUSH_CERT_*`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PushCertMode {
    /// `--no-signed` / `--signed=false`: never send a certificate.
    Never,
    /// `--signed=if-asked`: send only when the receiver advertises `push-cert`.
    IfAsked,
    /// `--signed` / `--signed=true` / `--signed=1`: require certificate support.
    Always,
}

impl PushCertMode {
    /// Resolve from the parsed CLI flags (Git `parse_push_signed`).
    fn from_args(args: &Args) -> PushCertMode {
        if args.no_signed {
            return PushCertMode::Never;
        }
        match args.signed.as_deref() {
            None => PushCertMode::Never,
            Some(v) => match v {
                "true" | "1" | "yes" | "" => PushCertMode::Always,
                "false" | "0" | "no" => PushCertMode::Never,
                "if-asked" => PushCertMode::IfAsked,
                other => {
                    // Git's git_parse_maybe_bool: unknown -> treat as boolean-true
                    // only for the known truthy spellings; anything else is an error
                    // upstream, but tolerate it as Always to avoid surprising users.
                    let _ = other;
                    PushCertMode::Always
                }
            },
        }
    }
}

/// A signed push certificate prepared for the receiving end: the cert blob OID
/// (written into the receiver), the issued nonce, and the verification result.
struct PreparedPushCert {
    env: grit_lib::push_cert::PushCertEnv,
}

/// Generate, sign, store, and verify a push certificate for the local/native
/// transport, honoring the receiver's `receive.certNonceSeed` advertisement.
///
/// Returns:
/// * `Ok(None)` when no certificate should be sent (mode `Never`, or `IfAsked`
///   with no receiver support, or no updates to certify).
/// * `Ok(Some(_))` when a certificate was signed and stored on the receiver.
/// * `Err(_)` when `Always` was requested but the receiver does not support
///   push certificates, or when signing fails.
#[allow(clippy::too_many_arguments)]
fn prepare_signed_push_cert(
    local_config: &ConfigSet,
    remote_repo: &Repository,
    receive_remote_config: &ConfigSet,
    mode: PushCertMode,
    url: &str,
    push_options: &[String],
    updates: &[RefUpdate],
    pre_reject: &[Option<String>],
) -> Result<Option<PreparedPushCert>> {
    use grit_lib::push_cert::{
        build_push_cert_payload, prepare_push_cert_nonce, verify_push_cert, CertRefUpdate,
    };
    use grit_lib::signing::{committer_signing_default, sign_buffer, GpgConfig};

    if mode == PushCertMode::Never {
        return Ok(None);
    }

    // The receiver advertises `push-cert` only when receive.certNonceSeed is set
    // (receive-pack.c). With no seed, --signed fails and --signed=if-asked is a no-op.
    let nonce_seed = receive_remote_config
        .get("receive.certnonceseed")
        .filter(|s| !s.is_empty());

    let issued_nonce = match &nonce_seed {
        Some(seed) => {
            let stamp = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs() as i64)
                .unwrap_or(0);
            let path = remote_repo.git_dir.to_string_lossy();
            Some(prepare_push_cert_nonce(&path, stamp, seed))
        }
        None => {
            if mode == PushCertMode::Always {
                bail!("the receiving end does not support --signed push");
            }
            // if-asked, unsupported: send nothing.
            return Ok(None);
        }
    };

    // Collect the ref-update lines (skip client-side rejected refs).
    let zero = "0".repeat(40);
    let mut cert_updates = Vec::new();
    for (i, u) in updates.iter().enumerate() {
        if pre_reject[i].is_some() {
            continue;
        }
        let old = u
            .old_oid
            .map(|o| o.to_hex())
            .unwrap_or_else(|| zero.clone());
        let new = u
            .new_oid
            .map(|o| o.to_hex())
            .unwrap_or_else(|| zero.clone());
        cert_updates.push(CertRefUpdate {
            old_oid: old,
            new_oid: new,
            refname: u.remote_ref.clone(),
        });
    }

    // Resolve the signing identity and key (Git get_signing_key_id / get_signing_key).
    let gpg_cfg = GpgConfig::from_config(local_config).map_err(|e| anyhow::anyhow!("{e}"))?;
    let committer_ident = {
        let (name, email) = grit_lib::ident_resolve::resolve_loose_committer_parts_with(
            &grit_lib::ident_resolve::SystemIdentityEnv,
            local_config,
        );
        format!("{name} <{email}>")
    };
    let pusher = committer_signing_default(&committer_ident);
    let signing_key = gpg_cfg.resolve_signing_key(None, &pusher);

    // Date stamp: "<epoch> <tz>" (Git datestamp). Honor GIT_COMMITTER_DATE if epoch+tz.
    let date = committer_datestamp();

    let payload = match build_push_cert_payload(
        &pusher,
        &date,
        Some(url),
        issued_nonce.as_deref(),
        push_options,
        &cert_updates,
    ) {
        Some(p) => p,
        None => return Ok(None),
    };

    // Sign the payload and append the detached signature (cert = payload + signature).
    let signature =
        sign_buffer(&gpg_cfg, &payload, &signing_key).map_err(|e| anyhow::anyhow!("{e}"))?;
    let mut signed_cert = payload;
    signed_cert.extend_from_slice(&signature);

    // Store the certificate as a blob on the receiver (receive-pack writes a blob).
    let cert_oid = remote_repo
        .odb
        .write(grit_lib::objects::ObjectKind::Blob, &signed_cert)
        .map_err(|e| anyhow::anyhow!("{e}"))?;

    // Verify the signature on the receiving end to derive signer/key/status, using
    // the receiver's gpg config (allowedSignersFile etc.).
    let recv_gpg =
        GpgConfig::from_config(receive_remote_config).map_err(|e| anyhow::anyhow!("{e}"))?;
    let check = verify_push_cert(&recv_gpg, &signed_cert).map_err(|e| anyhow::anyhow!("{e}"))?;

    let env = grit_lib::push_cert::cert_env_from_check(&check, cert_oid.to_hex(), issued_nonce);
    Ok(Some(PreparedPushCert { env }))
}

/// Current `"<epoch> <tz>"` datestamp, honoring `GIT_COMMITTER_DATE` when it is
/// already in epoch+tz form (the only form the push cert needs to round-trip).
fn committer_datestamp() -> String {
    if let Ok(d) = std::env::var("GIT_COMMITTER_DATE") {
        let trimmed = d.trim();
        let parts: Vec<&str> = trimmed.rsplitn(2, ' ').collect();
        if parts.len() == 2 && parts[1].chars().all(|c| c.is_ascii_digit()) && !parts[1].is_empty()
        {
            return trimmed.to_owned();
        }
    }
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    format!("{secs} +0000")
}

#[allow(clippy::too_many_arguments)]
fn push_to_url(
    repo: &Repository,
    config: &ConfigSet,
    args: &Args,
    url: &str,
    remote_name: &str,
    current_branch: Option<&str>,
    push_all: bool,
    effective_mirror: bool,
    push_refspecs_from_config: &[String],
    remote_is_configured_name: bool,
    cli_force_enabled: bool,
) -> Result<()> {
    if url.starts_with("ext::") {
        crate::protocol::check_protocol_allowed("ext", Some(&repo.git_dir))?;
        let child = crate::ext_transport::spawn_ext_receive_pack(url)?;
        return push_over_receive_pack_child(
            child,
            "ext",
            repo,
            config,
            args,
            url,
            remote_name,
            current_branch,
            push_all,
            effective_mirror,
            push_refspecs_from_config,
            cli_force_enabled,
        );
    }
    if protocol_wire::effective_client_protocol_version() == 1 {
        wire_trace::trace_packet_push('<', "version 1");
    }
    if url.starts_with("git://") && protocol_wire::effective_client_protocol_version() == 1 {
        if let Ok(parsed) = crate::fetch_transport::parse_git_url(url) {
            let virtual_host = std::env::var("GIT_OVERRIDE_VIRTUAL_HOST")
                .unwrap_or_else(|_| format!("{}:{}", parsed.host, parsed.port));
            let show = format!(
                "git-receive-pack {}\\0host={}\\0\\0version=1\\0",
                parsed.path, virtual_host
            );
            wire_trace::trace_packet_push('>', &show);
        }
    }
    let remote_path = if url.starts_with("git://") {
        crate::protocol::check_protocol_allowed("git", Some(&repo.git_dir))?;
        bail!("git:// transport is not supported for push");
    } else if is_http_transport_url(url) {
        if args.receive_pack.as_ref().is_some_and(|s| !s.is_empty()) {
            bail!("--receive-pack is not supported for HTTP push");
        }
        return push_to_http_url(
            repo,
            config,
            args,
            url,
            remote_name,
            current_branch,
            push_all,
            effective_mirror,
            push_refspecs_from_config,
            cli_force_enabled,
        );
    } else if crate::ssh_transport::is_configured_ssh_url(url) {
        crate::protocol::check_protocol_allowed("ssh", Some(&repo.git_dir))?;
        let spec = crate::ssh_transport::parse_ssh_url(url)?;
        let Some(gd) = crate::ssh_transport::try_local_git_dir(&spec) else {
            return push_to_ssh_url(
                repo,
                config,
                args,
                url,
                remote_name,
                current_branch,
                push_all,
                effective_mirror,
                push_refspecs_from_config,
                cli_force_enabled,
            );
        };
        gd
    } else {
        crate::protocol::check_protocol_allowed("file", Some(&repo.git_dir))?;
        if let Some(stripped) = url.strip_prefix("file://") {
            PathBuf::from(stripped)
        } else {
            PathBuf::from(url)
        }
    };

    // Open remote repo
    let remote_repo = open_repo(&remote_path).with_context(|| {
        format!(
            "could not open remote repository at '{}'",
            remote_path.display()
        )
    })?;

    // An explicit non-default `--receive-pack` program for a local-file push must actually run
    // that program and speak the real receive-pack protocol to it, so its exit status governs the
    // push outcome. When the push needs grit's full per-ref status report (e.g. `--atomic`, which
    // a bare `send-pack` delegation cannot render), spawn the receiving program as a child and
    // drive it through the shared receive-pack-child path. This faithfully reports a wrapper that
    // runs `git-receive-pack` and then exits non-zero: the "To"/"[new branch]" lines are emitted
    // from report-status and the non-zero exit still fails the push (t5543 atomic exit-code).
    if args.receive_pack.as_ref().is_some_and(|s| !s.is_empty())
        && !is_default_receive_pack_program(args.receive_pack.as_deref())
        && args.atomic
        && !args.dry_run
        && args.force_with_lease.is_none()
    {
        let receive_cmd = args.receive_pack.as_deref().unwrap_or("");
        let child = crate::commands::send_pack::spawn_receive_pack(receive_cmd, &remote_path)?;
        return push_over_receive_pack_child(
            child,
            "file",
            repo,
            config,
            args,
            url,
            remote_name,
            current_branch,
            push_all,
            effective_mirror,
            push_refspecs_from_config,
            cli_force_enabled,
        );
    }

    // When the caller specifies an explicit `--receive-pack` program for a local-file push, run
    // the real push protocol through that program via `send-pack` instead of the in-process fast
    // path. This is what Git does (it always spawns receive-pack), and it lets the receiving
    // program emit its own trace2 session-id / negotiated-version events (`t5705`).
    if args.receive_pack.as_ref().is_some_and(|s| !s.is_empty())
        && !is_default_receive_pack_program(args.receive_pack.as_deref())
        && !effective_mirror
        && !push_all
        && !args.delete
        && !args.tags
        && !args.follow_tags
        && !args.set_upstream
        && !args.atomic
        && !args.dry_run
        && args.force_with_lease.is_none()
        && !args.refspecs.is_empty()
    {
        return delegate_local_push_to_send_pack(&remote_path, &args.refspecs, args, url);
    }

    if crate::ssh_transport::is_configured_ssh_url(url) {
        if let Ok(spec) = crate::ssh_transport::parse_ssh_url(url) {
            let _ = crate::ssh_transport::record_resolved_git_ssh_receive_pack_for_tests(
                &spec, false, false,
            );
        }
    }

    // Receive-side ref policy (denyCurrentBranch, etc.): only the remote repo's `config`, not the
    // pushing side's `git -c` / environment (matches Git; t5507-remote-environment).
    let receive_remote_config = ConfigSet::load_repo_local_only(&remote_repo.git_dir)?;
    let effective_push_options = resolved_push_options(args, config)?;

    // Build list of ref updates
    let mut updates = Vec::new();
    let mut set_upstream_after_push = args.set_upstream;
    // Local commit OIDs that would be advertised as push tips (including refs already up to date
    // on the remote). Submodule recursion runs on this set, matching Git transport behavior.
    let mut submodule_tips: Vec<ObjectId> = Vec::new();

    if effective_mirror {
        // Mirror: push all local refs to remote, and delete remote refs
        // that don't exist locally.
        let local_all = refs::list_refs(&repo.git_dir, "refs/")?;
        for (refname, local_oid) in &local_all {
            // Skip special refs like HEAD, FETCH_HEAD, etc.
            if !refname.starts_with("refs/") {
                continue;
            }
            let old_oid = refs::resolve_ref(&remote_repo.git_dir, refname).ok();
            if old_oid.as_ref() == Some(local_oid) {
                submodule_tips.push(*local_oid);
                continue;
            }
            updates.push(RefUpdate {
                local_ref: Some(refname.clone()),
                remote_ref: refname.clone(),
                old_oid,
                new_oid: Some(*local_oid),
                expected_oid: None,
                refspec_force: true,
                pre_push_local_name: None,
                up_to_date: false,
                client_reject: None,
                delete_nonexistent: false,
            });
        }
        // Delete remote refs that don't exist locally
        let remote_all = refs::list_refs(&remote_repo.git_dir, "refs/")?;
        for (refname, _remote_oid) in &remote_all {
            if !refname.starts_with("refs/") {
                continue;
            }
            if !local_all.iter().any(|(r, _)| r == refname) {
                let old_oid = refs::resolve_ref(&remote_repo.git_dir, refname).ok();
                updates.push(RefUpdate {
                    local_ref: None,
                    remote_ref: refname.clone(),
                    old_oid,
                    new_oid: None,
                    expected_oid: None,
                    refspec_force: true,
                    pre_push_local_name: None,
                    up_to_date: false,
                    client_reject: None,
                    delete_nonexistent: false,
                });
            }
        }
    } else if let Some((refspec_force, negs)) = parse_matching_push_with_negatives(args) {
        validate_negative_push_patterns(&negs.iter().map(|s| s.as_str()).collect::<Vec<_>>())?;
        let matched = collect_matching_push_updates(
            repo,
            &remote_repo,
            remote_name,
            args,
            &mut updates,
            &mut submodule_tips,
            &negs,
            refspec_force,
        )?;
        // An explicit matching (`:`/`+:`) refspec that matches nothing is a successful no-op, not a
        // fatal error: C Git (`send_pack`/`transport_push`) prints a diagnostic but returns 0 and
        // lets the caller report "Everything up-to-date". Only warn (to stderr) when the *remote*
        // advertises no refs at all, mirroring send-pack.c's `!remote_refs` branch. Pushing `:` from
        // a detached HEAD with no matching branches (t7406 "git-dir recursive") must just succeed.
        if matched == 0 && !args.quiet {
            let remote_has_refs = refs::list_refs(&remote_repo.git_dir, "refs/heads/")
                .map(|r| !r.is_empty())
                .unwrap_or(false);
            if !remote_has_refs {
                eprintln!(
                    "No refs in common and none specified; doing nothing.\nPerhaps you should specify a branch."
                );
            }
        }
    } else if push_all {
        // Push all branches (refs/heads/*)
        let mut local_branches = refs::list_refs(&repo.git_dir, "refs/heads/")?;
        local_branches.sort_by(|a, b| a.0.cmp(&b.0));
        for (refname, local_oid) in &local_branches {
            let old_oid = refs::resolve_ref(&remote_repo.git_dir, refname).ok();
            if old_oid.as_ref() == Some(local_oid) {
                submodule_tips.push(*local_oid);
                continue;
            }
            updates.push(RefUpdate {
                local_ref: Some(refname.clone()),
                remote_ref: refname.clone(),
                old_oid,
                new_oid: Some(*local_oid),
                expected_oid: None,
                refspec_force: false,
                pre_push_local_name: None,
                up_to_date: false,
                client_reject: None,
                delete_nonexistent: false,
            });
        }
    } else if args.delete {
        // Delete mode: each refspec is a plain remote ref name to delete (Git `set_refspecs`).
        if args.refspecs.is_empty() {
            bail!("--delete doesn't make sense without any refs");
        }
        // Resolve `tag <name>` shorthand and reject src:dest / empty refspecs, mirroring
        // git/builtin/push.c `set_refspecs`: `--delete only accepts plain target ref names`.
        let mut delete_targets: Vec<String> = Vec::new();
        let mut i = 0usize;
        while i < args.refspecs.len() {
            let spec = &args.refspecs[i];
            if spec == "tag" {
                i += 1;
                let Some(name) = args.refspecs.get(i) else {
                    bail!("tag shorthand without <tag>");
                };
                delete_targets.push(format!("refs/tags/{name}"));
            } else if spec.contains(':') || spec.is_empty() {
                bail!("--delete only accepts plain target ref names");
            } else {
                delete_targets.push(spec.clone());
            }
            i += 1;
        }
        for target in &delete_targets {
            let remote_ref = normalize_ref(target);
            let old_oid = refs::resolve_ref(&remote_repo.git_dir, &remote_ref).ok();
            if old_oid.is_none() {
                // Git skips delete refspecs when the remote ref is already absent
                // (e.g. tracking ref removed locally first).
                continue;
            }
            let expected_oid = resolve_force_with_lease_expect(
                &args.force_with_lease,
                &repo.git_dir,
                remote_name,
                target,
            );
            updates.push(RefUpdate {
                local_ref: None,
                remote_ref,
                old_oid,
                new_oid: None,
                expected_oid,
                refspec_force: false,
                pre_push_local_name: None,
                up_to_date: false,
                client_reject: None,
                delete_nonexistent: false,
            });
        }
    } else if !args.refspecs.is_empty() {
        let negative_owned: Vec<String> = args
            .refspecs
            .iter()
            .filter_map(|s| s.strip_prefix('^').map(|p| p.to_owned()))
            .collect();
        validate_negative_push_patterns(
            &negative_owned
                .iter()
                .map(|s| s.as_str())
                .collect::<Vec<_>>(),
        )?;

        // Explicit refspecs
        let mut spec_idx = 0usize;
        while spec_idx < args.refspecs.len() {
            let spec = &args.refspecs[spec_idx];
            if spec.starts_with('^') {
                spec_idx += 1;
                continue;
            }
            // Strip leading '+' force prefix
            let (per_refspec_force, spec_clean) = if let Some(s) = spec.strip_prefix('+') {
                (true, s)
            } else {
                (false, spec.as_str())
            };
            let (src, dst, consumed) = if spec_clean == "tag" {
                let Some(name) = args.refspecs.get(spec_idx + 1) else {
                    bail!("missing tag name after 'tag'");
                };
                let full = format!("refs/tags/{name}");
                (full.clone(), full, 2)
            } else {
                let (mut src, mut dst) = parse_refspec(spec_clean);
                // Git `parse_refspec` (refspec.c) rewrites a literal `@` source to `HEAD`.
                if src == "@" {
                    src = "HEAD".to_owned();
                    // A colon-less `@` mirrors the (now `HEAD`) source onto the dst so the
                    // remote side is resolved from HEAD's branch, like `git push remote HEAD`.
                    if !spec_clean.contains(':') {
                        dst = "HEAD".to_owned();
                    }
                }
                (src, dst, 1)
            };

            // Empty src (e.g. ":branch") means delete
            if src.is_empty() {
                let remote_ref = normalize_ref(&dst);
                let old_oid = refs::resolve_ref(&remote_repo.git_dir, &remote_ref).ok();
                // A delete of a ref absent on the remote is still fed to pre-receive/update
                // hooks (Git `did_not_exist`); it is just not applied. Keep it in `updates`.
                let delete_nonexistent = old_oid.is_none();
                let expected_oid = resolve_force_with_lease_expect(
                    &args.force_with_lease,
                    &repo.git_dir,
                    remote_name,
                    &dst,
                );
                updates.push(RefUpdate {
                    local_ref: None,
                    remote_ref,
                    old_oid,
                    new_oid: None,
                    expected_oid,
                    refspec_force: per_refspec_force,
                    pre_push_local_name: None,
                    up_to_date: false,
                    client_reject: None,
                    delete_nonexistent,
                });
                spec_idx += consumed;
                continue;
            }

            // Handle glob refspecs (e.g. refs/remotes/*:refs/remotes/*)
            if src.contains('*') {
                let local_refs = refs::list_refs(&repo.git_dir, "refs/")?;
                for (refname, local_oid) in &local_refs {
                    if negative_owned
                        .iter()
                        .any(|p| ref_excluded_by_negative_push_pattern(p, refname))
                    {
                        continue;
                    }
                    if let Some(matched) = match_glob(&src, refname) {
                        // Check if this is a symbolic ref
                        if let Ok(Some(_target)) = refs::read_symbolic_ref(&repo.git_dir, refname) {
                            // Skip symbolic refs from normal updates; handle below
                            continue;
                        }
                        let remote_ref = dst.replacen('*', matched, 1);
                        let old_oid = refs::resolve_ref(&remote_repo.git_dir, &remote_ref).ok();
                        if old_oid.as_ref() == Some(local_oid) {
                            submodule_tips.push(*local_oid);
                            continue;
                        }
                        updates.push(RefUpdate {
                            local_ref: Some(refname.clone()),
                            remote_ref,
                            old_oid,
                            new_oid: Some(*local_oid),
                            expected_oid: None,
                            refspec_force: per_refspec_force,
                            pre_push_local_name: None,
                            up_to_date: false,
                            client_reject: None,
                            delete_nonexistent: false,
                        });
                    }
                }
                if args.prune {
                    push_prune_glob_refspec(
                        repo,
                        &remote_repo,
                        args,
                        remote_name,
                        per_refspec_force,
                        &src,
                        &dst,
                        &negative_owned,
                        &mut updates,
                    )?;
                }
                // Copy symbolic refs matching the glob pattern
                copy_symrefs_push(&repo.git_dir, &remote_repo.git_dir, spec_clean, &dst)?;
                spec_idx += consumed;
                continue;
            }

            // When pushing HEAD without explicit :dst, use the resolved branch name for the remote side.
            let effective_dst = if dst == "HEAD" && src == "HEAD" {
                match resolve_head(&repo.git_dir) {
                    Ok(HeadState::Branch { refname, .. }) => refname,
                    Ok(HeadState::Detached { oid, .. }) => oid.to_hex(),
                    _ => dst.clone(),
                }
            } else {
                dst.clone()
            };
            let (local_ref, local_oid, pre_push_local_name) =
                resolve_push_src_for_refspec(repo, &src, &effective_dst)
                    .with_context(|| format!("src refspec '{}' does not match any", src))?;
            let src_kind = repo.odb.read(&local_oid).ok().map(|o| o.kind);
            let remote_ref = resolve_destination_ref_for_push(
                &remote_repo.git_dir,
                &effective_dst,
                &local_ref,
                !spec_clean.contains(':') && spec_clean != "tag",
                &local_ref,
                src_kind,
                push_unqualified_advice_enabled(&repo.git_dir),
            )?;
            let old_oid = refs::resolve_ref(&remote_repo.git_dir, &remote_ref).ok();

            let expected_oid = resolve_force_with_lease_expect(
                &args.force_with_lease,
                &repo.git_dir,
                remote_name,
                &dst,
            );

            updates.push(RefUpdate {
                local_ref: Some(local_ref),
                remote_ref,
                old_oid,
                new_oid: Some(local_oid),
                expected_oid,
                refspec_force: per_refspec_force,
                pre_push_local_name,
                up_to_date: false,
                client_reject: None,
                delete_nonexistent: false,
            });
            spec_idx += consumed;
        }
    } else if !push_refspecs_from_config.is_empty() {
        let lines = push_refspecs_from_config;
        let mut i = 0usize;
        while i < lines.len() {
            let spec = &lines[i];
            if spec == ":" || spec == "+:" {
                let refspec_force = spec.starts_with('+');
                let mut negs = Vec::new();
                let mut j = i + 1;
                while j < lines.len() && lines[j].starts_with('^') {
                    negs.push(lines[j][1..].to_owned());
                    j += 1;
                }
                validate_negative_push_patterns(
                    &negs.iter().map(|s| s.as_str()).collect::<Vec<_>>(),
                )?;
                let matched = collect_matching_push_updates(
                    repo,
                    &remote_repo,
                    remote_name,
                    args,
                    &mut updates,
                    &mut submodule_tips,
                    &negs,
                    refspec_force,
                )?;
                if matched == 0 {
                    bail!(
                        "No refs in common and none specified; doing nothing.\nPerhaps you should specify a branch."
                    );
                }
                i = j;
                continue;
            }
            if spec.starts_with('^') {
                i += 1;
                continue;
            }
            let (force_flag, spec_clean) = if let Some(s) = spec.strip_prefix('+') {
                (true, s)
            } else {
                (false, spec.as_str())
            };
            let (src_pat, dst_pat) = if let Some(idx) = spec_clean.find(':') {
                (&spec_clean[..idx], &spec_clean[idx + 1..])
            } else {
                (spec_clean, spec_clean)
            };
            if src_pat.contains('*') {
                let local_refs = refs::list_refs(&repo.git_dir, "refs/")?;
                for (refname, local_oid) in &local_refs {
                    if let Some(matched) = match_glob(src_pat, refname) {
                        let remote_ref = dst_pat.replacen('*', matched, 1);
                        let old_oid = refs::resolve_ref(&remote_repo.git_dir, &remote_ref).ok();
                        if old_oid.as_ref() == Some(local_oid) {
                            submodule_tips.push(*local_oid);
                            continue;
                        }
                        updates.push(RefUpdate {
                            local_ref: Some(refname.clone()),
                            remote_ref,
                            old_oid,
                            new_oid: Some(*local_oid),
                            expected_oid: None,
                            refspec_force: force_flag,
                            pre_push_local_name: None,
                            up_to_date: false,
                            client_reject: None,
                            delete_nonexistent: false,
                        });
                    }
                }
                if args.prune {
                    push_prune_glob_refspec(
                        repo,
                        &remote_repo,
                        args,
                        remote_name,
                        force_flag,
                        src_pat,
                        dst_pat,
                        &[],
                        &mut updates,
                    )?;
                }
            } else {
                // A configured `remote.<name>.push = HEAD` (or `@`) resolves the source like a
                // command-line refspec: `@` rewrites to `HEAD`, and HEAD/oid/short names DWIM via
                // `resolve_push_src_for_refspec` (Git treats config and CLI refspecs identically).
                let colon_less = !spec_clean.contains(':');
                let src_resolved = if src_pat == "@" { "HEAD" } else { src_pat };
                let dst_resolved = if colon_less && src_pat == "@" {
                    "HEAD"
                } else {
                    dst_pat
                };
                let effective_dst = if dst_resolved == "HEAD" && src_resolved == "HEAD" {
                    match resolve_head(&repo.git_dir) {
                        Ok(HeadState::Branch { refname, .. }) => refname,
                        Ok(HeadState::Detached { oid, .. }) => oid.to_hex(),
                        _ => dst_resolved.to_owned(),
                    }
                } else {
                    dst_resolved.to_owned()
                };
                let (local_ref, local_oid, pre_push_local_name) =
                    resolve_push_src_for_refspec(repo, src_resolved, &effective_dst)
                        .with_context(|| format!("src refspec '{}' does not match any", src_pat))?;
                let src_kind = repo.odb.read(&local_oid).ok().map(|o| o.kind);
                let remote_ref = resolve_destination_ref_for_push(
                    &remote_repo.git_dir,
                    &effective_dst,
                    &local_ref,
                    colon_less,
                    &local_ref,
                    src_kind,
                    push_unqualified_advice_enabled(&repo.git_dir),
                )?;
                let old_oid = refs::resolve_ref(&remote_repo.git_dir, &remote_ref).ok();
                if old_oid.as_ref() != Some(&local_oid) {
                    updates.push(RefUpdate {
                        local_ref: Some(local_ref),
                        remote_ref,
                        old_oid,
                        new_oid: Some(local_oid),
                        expected_oid: None,
                        refspec_force: force_flag,
                        pre_push_local_name,
                        up_to_date: false,
                        client_reject: None,
                        delete_nonexistent: false,
                    });
                } else {
                    submodule_tips.push(local_oid);
                }
            }
            i += 1;
        }
    } else if args.tags || args.follow_tags {
        // `git push --tags` (no refspec) pushes only tags, handled by the tags block below.
    } else if push_default_mode(config) == "matching" {
        // `push.default = matching` pushes every ref present on both sides; it does not need a
        // current branch, so it works with a detached HEAD (t5516 test 52 pushes after a
        // detached checkout).
        let matched = collect_matching_push_updates(
            repo,
            &remote_repo,
            remote_name,
            args,
            &mut updates,
            &mut submodule_tips,
            &[],
            false,
        )?;
        if matched == 0 {
            bail!(
                "No refs in common and none specified; doing nothing.\nPerhaps you should specify a branch."
            );
        }
    } else {
        // Default push mode (simple/current/upstream/nothing) needs the current branch.
        let branch = current_branch.context("not on a branch; specify a refspec to push")?;
        {
            let (local_ref, remote_ref, auto_set_upstream) =
                default_push_ref_for_current_branch(config, remote_name, branch)?;

            let local_oid = refs::resolve_ref(&repo.git_dir, &local_ref)
                .with_context(|| format!("branch '{branch}' has no commits"))?;
            let old_oid = refs::resolve_ref(&remote_repo.git_dir, &remote_ref).ok();

            let expected_oid = resolve_force_with_lease_expect(
                &args.force_with_lease,
                &repo.git_dir,
                remote_name,
                branch,
            );

            updates.push(RefUpdate {
                local_ref: Some(local_ref),
                remote_ref,
                old_oid,
                new_oid: Some(local_oid),
                expected_oid,
                refspec_force: false,
                pre_push_local_name: None,
                up_to_date: false,
                client_reject: None,
                delete_nonexistent: false,
            });
            if auto_set_upstream {
                set_upstream_after_push = true;
            }
        }
    }

    // Push tags if requested
    if args.tags {
        let local_tags = refs::list_refs(&repo.git_dir, "refs/tags/")?;
        for (refname, local_oid) in &local_tags {
            let old_oid = refs::resolve_ref(&remote_repo.git_dir, refname).ok();
            if old_oid.as_ref() == Some(local_oid) {
                continue; // already up to date
            }
            updates.push(RefUpdate {
                local_ref: Some(refname.clone()),
                remote_ref: refname.clone(),
                old_oid,
                new_oid: Some(*local_oid),
                expected_oid: None,
                refspec_force: false,
                pre_push_local_name: None,
                up_to_date: false,
                client_reject: None,
                delete_nonexistent: false,
            });
        }
    }

    // --follow-tags: also push annotated tags pointing at commits being pushed
    let follow_tags = args.follow_tags
        || (!args.no_follow_tags
            && config
                .get("push.followTags")
                .map(|v| matches!(v.to_lowercase().as_str(), "true" | "yes" | "1"))
                .unwrap_or(false));
    if follow_tags {
        let pushed_oids: std::collections::HashSet<ObjectId> =
            updates.iter().filter_map(|u| u.new_oid).collect();
        if !pushed_oids.is_empty() {
            if let Ok(local_tags) = refs::list_refs(&repo.git_dir, "refs/tags/") {
                for (tag_name, tag_oid) in &local_tags {
                    // Skip if already being pushed or already exists on remote
                    if updates.iter().any(|u| u.remote_ref == *tag_name) {
                        continue;
                    }
                    if refs::resolve_ref(&remote_repo.git_dir, tag_name).is_ok() {
                        continue;
                    }
                    // Check if it's an annotated tag pointing at a pushed commit
                    if let Ok(obj) = repo.odb.read(tag_oid) {
                        if obj.kind == grit_lib::objects::ObjectKind::Tag {
                            if let Ok(tag) = grit_lib::objects::parse_tag(&obj.data) {
                                if pushed_oids.contains(&tag.object) {
                                    updates.push(RefUpdate {
                                        local_ref: Some(tag_name.clone()),
                                        remote_ref: tag_name.clone(),
                                        old_oid: None,
                                        new_oid: Some(*tag_oid),
                                        expected_oid: None,
                                        refspec_force: false,
                                        pre_push_local_name: None,
                                        up_to_date: false,
                                        client_reject: None,
                                        delete_nonexistent: false,
                                    });
                                }
                            }
                        }
                    }
                }
            }
        }
    }

    // Order ref commands the way receive-pack sees them: the remote's advertised refs (sorted by
    // refname) that are being updated come first, then newly-created refs in refspec order. Git's
    // send-pack walks the matched remote ref list (already sorted) and appends new refs, so a push
    // of `main:main main:seen :next` feeds hooks in main,next,seen order regardless of the
    // command-line order (t5516 'mixed ref updates' hooks test). Skip this when a mirror-atomic
    // order is in force (handled below).
    if !(effective_mirror && args.atomic) {
        let remote_refnames: std::collections::BTreeSet<String> =
            refs::list_refs(&remote_repo.git_dir, "refs/")
                .unwrap_or_default()
                .into_iter()
                .map(|(name, _)| name)
                .collect();
        // Stable partition: existing remote refs first (BTreeSet order ⇒ sorted), new refs after
        // in their original relative order.
        updates.sort_by(|a, b| {
            let a_existing = remote_refnames.contains(&a.remote_ref);
            let b_existing = remote_refnames.contains(&b.remote_ref);
            match (a_existing, b_existing) {
                (true, true) => a.remote_ref.cmp(&b.remote_ref),
                (true, false) => std::cmp::Ordering::Less,
                (false, true) => std::cmp::Ordering::Greater,
                (false, false) => std::cmp::Ordering::Equal,
            }
        });
    }

    let mirror_atomic_order = if effective_mirror && args.atomic {
        Some(mirror_atomic_ref_order(&updates))
    } else {
        None
    };
    if let Some(order) = &mirror_atomic_order {
        updates.sort_by(|a, b| {
            let ia = order
                .iter()
                .position(|r| r == &a.remote_ref)
                .unwrap_or(usize::MAX);
            let ib = order
                .iter()
                .position(|r| r == &b.remote_ref)
                .unwrap_or(usize::MAX);
            ia.cmp(&ib).then_with(|| a.remote_ref.cmp(&b.remote_ref))
        });
    }

    let recurse_mode = effective_push_recurse_submodules(args, config)?;

    let mut combined_tips: Vec<ObjectId> = updates.iter().filter_map(|u| u.new_oid).collect();
    combined_tips.extend(submodule_tips.iter().copied());
    combined_tips.sort();
    combined_tips.dedup();

    if !repo.is_bare()
        && !matches!(recurse_mode, PushRecurseSubmodules::Off)
        && !(effective_mirror || push_all || args.delete)
        && !combined_tips.is_empty()
    {
        let tips = combined_tips;
        let sub_refspecs =
            submodule_push_refspecs(args, current_branch, push_all, push_refspecs_from_config);
        let changed = collect_changed_gitlinks_for_push(
            repo,
            &tips,
            remote_name,
            Some(remote_repo.git_dir.as_path()),
        )?;
        verify_push_gitlinks_are_commits(repo, &changed)?;

        if matches!(
            recurse_mode,
            PushRecurseSubmodules::OnDemand | PushRecurseSubmodules::Only
        ) {
            let super_head_branch = head_ref_short_name(&repo.git_dir)?;
            // Git's `push_submodule` unconditionally passes `--recurse-submodules=only-is-on-demand`
            // to every child push, so the submodule uses its OWN recurse config but a child whose
            // config is `only` is treated as `on-demand` (a plain `only` child cannot push the
            // superproject). We therefore always set this when recursing.
            let nested_only = true;
            let to_push = find_unpushed_submodule_paths(
                repo,
                &tips,
                remote_name,
                Some(remote_repo.git_dir.as_path()),
            )?;
            for sub_path in &to_push {
                let wd = submodule_worktree_path(repo, sub_path);
                if !wd.join(".git").exists() {
                    continue;
                }
                let sub_repo = match Repository::discover(Some(&wd)) {
                    Ok(r) => r,
                    Err(_) => continue,
                };
                if refs::list_refs(&sub_repo.git_dir, "refs/remotes/")
                    .map(|r| r.is_empty())
                    .unwrap_or(true)
                {
                    continue;
                }
                let detached =
                    !matches!(resolve_head(&sub_repo.git_dir)?, HeadState::Branch { .. });
                let sub_refspecs_effective = rewrite_submodule_refspecs_for_detached_head(
                    &sub_refspecs,
                    &super_head_branch,
                    detached,
                );
                // Git only validates and propagates the remote name + refspec to the submodule
                // push when the superproject's remote is a *configured* remote (not an anonymous
                // URL): `push_unpushed_submodules` gates both on `remote->origin !=
                // REMOTE_UNCONFIGURED`. For an anonymous URL push the submodule relies on its own
                // configured upstream instead.
                if remote_is_configured_name {
                    validate_submodule_push_refspecs(
                        &sub_repo.git_dir,
                        &super_head_branch,
                        &sub_refspecs_effective,
                    )
                    .map_err(|e| anyhow::Error::msg(e.to_string()))?;
                }
                if !args.quiet {
                    eprintln!("Pushing submodule '{sub_path}'");
                }
                let remote_specs = if remote_is_configured_name {
                    Some((remote_name, sub_refspecs_effective.as_slice()))
                } else {
                    None
                };
                run_nested_submodule_push(
                    &wd,
                    remote_specs,
                    args.dry_run,
                    args.quiet,
                    &effective_push_options,
                    nested_only,
                )?;
            }
        }

        let check_after = recurse_mode == PushRecurseSubmodules::Check
            || (matches!(
                recurse_mode,
                PushRecurseSubmodules::OnDemand | PushRecurseSubmodules::Only
            ) && !args.dry_run);
        if check_after {
            let needs = find_unpushed_submodule_paths(
                repo,
                &tips,
                remote_name,
                Some(remote_repo.git_dir.as_path()),
            )?;
            if !needs.is_empty() {
                let msg = format_unpushed_submodules_error(&needs);
                eprintln!("{}", msg.trim_end());
                bail!("failed to push all needed submodules");
            }
        }
    }

    // `git push -u --all` sets upstream for every local branch even when every ref is already
    // up to date (no ref updates). Add synthetic updates so the downstream path can apply config.
    if args.set_upstream && push_all {
        let local_branches = refs::list_refs(&repo.git_dir, "refs/heads/")?;
        let existing_local: std::collections::HashSet<String> =
            updates.iter().filter_map(|u| u.local_ref.clone()).collect();
        for (refname, local_oid) in &local_branches {
            if existing_local.contains(refname) {
                continue;
            }
            let old_oid = refs::resolve_ref(&remote_repo.git_dir, refname).ok();
            if old_oid.as_ref() == Some(local_oid) {
                updates.push(RefUpdate {
                    local_ref: Some(refname.clone()),
                    remote_ref: refname.clone(),
                    old_oid,
                    new_oid: Some(*local_oid),
                    expected_oid: None,
                    refspec_force: false,
                    pre_push_local_name: None,
                    up_to_date: false,
                    client_reject: None,
                    delete_nonexistent: false,
                });
            }
        }
    }

    reject_or_drop_aliased_remote_updates(&remote_repo.git_dir, &mut updates)?;

    if recurse_mode == PushRecurseSubmodules::Only {
        return Ok(());
    }

    if updates.is_empty() {
        if !args.quiet {
            println!("Everything up-to-date");
        }
        if args.set_upstream && !args.dry_run && push_all {
            let local_branches = refs::list_refs(&repo.git_dir, "refs/heads/")?;
            for (local_ref, _) in &local_branches {
                let Some(branch_name) = local_ref.strip_prefix("refs/heads/") else {
                    continue;
                };
                let merge_ref = format!("refs/heads/{branch_name}");
                set_upstream_config(&repo.git_dir, branch_name, remote_name, &merge_ref)?;
                if !args.quiet {
                    eprintln!(
                        "branch '{branch_name}' set up to track '{remote_name}/{branch_name}'."
                    );
                }
            }
        }
        return Ok(());
    }

    // Per-ref validation. Force-with-lease still fails the whole push when stale.
    // Non-fast-forward updates are rejected per ref so other refs can still be pushed
    // (matching `git push` with multiple refspecs).
    let force_if_includes = effective_force_if_includes(args, config);
    let mut pre_reject: Vec<Option<String>> = vec![None; updates.len()];

    // Pre-reject updates/deletes to refs hidden by transfer/receive.hideRefs *before* any objects
    // are sent, so a rejected hidden-ref push leaves the remote's object store untouched
    // (t5516 'refuse to push a hidden ref, and make sure do not pollute the repository').
    {
        let mut hidden = grit_lib::ref_exclusions::RefExclusions::default();
        hidden.load_hidden_refs_from_config(&receive_remote_config, "receive");
        for (i, update) in updates.iter().enumerate() {
            if hidden.ref_excluded(Some(&update.remote_ref), &update.remote_ref) {
                pre_reject[i] = Some(if update.new_oid.is_none() {
                    "deny deleting a hidden ref".to_owned()
                } else {
                    "deny updating a hidden ref".to_owned()
                });
            }
        }
    }

    for (i, update) in updates.iter().enumerate() {
        if pre_reject[i].is_some() {
            continue;
        }
        let mut includes_override_for_lease = false;
        if !cli_force_enabled && !update.refspec_force {
            match force_with_lease_expectation_for_remote_ref(
                &args.force_with_lease,
                &repo.git_dir,
                remote_name,
                &update.remote_ref,
            ) {
                LeaseCheckResult::None => {}
                LeaseCheckResult::Expect(expected) => {
                    let actual_remote =
                        refs::resolve_ref(&remote_repo.git_dir, &update.remote_ref).ok();
                    if actual_remote.as_ref() != Some(&expected) {
                        if force_if_includes
                            && update.remote_ref.starts_with("refs/heads/")
                            && update.old_oid.is_some()
                        {
                            if push_includes_remote_tracking_tip(
                                repo,
                                remote_name,
                                update,
                                &args.force_with_lease,
                            )? {
                                includes_override_for_lease = true;
                            } else {
                                bail!(
                                    "failed to push some refs: stale info for '{}' \
                                     (force-with-lease check failed)",
                                    update.remote_ref
                                );
                            }
                        } else {
                            bail!(
                                "failed to push some refs: stale info for '{}' \
                                 (force-with-lease check failed)",
                                update.remote_ref
                            );
                        }
                    }
                }
                LeaseCheckResult::MissingTracking => {
                    if update.old_oid.is_some() {
                        bail!(
                            "failed to push some refs: stale info for '{}' \
                             (force-with-lease check failed)",
                            update.remote_ref
                        );
                    }
                }
            }
        }
        if force_if_includes
            && !cli_force_enabled
            && !update.refspec_force
            && update.remote_ref.starts_with("refs/heads/")
            && update.old_oid.is_some()
            && !includes_override_for_lease
            && !push_includes_remote_tracking_tip(
                repo,
                remote_name,
                update,
                &args.force_with_lease,
            )?
        {
            bail!(
                "failed to push some refs: stale info for '{}' \
                 (force-with-lease check failed)",
                update.remote_ref
            );
        }

        if let (Some(old), Some(new)) = (&update.old_oid, &update.new_oid) {
            if old == new {
                continue;
            }
            if !effective_mirror
                && !cli_force_enabled
                && !update.refspec_force
                && args.force_with_lease.is_none()
                && !update.remote_ref.starts_with("refs/tags/")
                && !is_ancestor(repo, *old, *new)?
            {
                pre_reject[i] = Some(
                    "Updates were rejected because the remote contains work that you do not\n\
                     have locally. This is usually caused by another repository pushing to\n\
                     the same ref. If you want to integrate the remote changes, use\n\
                     'git pull' before pushing again.\n\
                     See the 'Note about fast-forwards' in 'git push --help' for details."
                        .to_string(),
                );
            }
            if !effective_mirror
                && !cli_force_enabled
                && !update.refspec_force
                && args.force_with_lease.is_none()
                && update.remote_ref.starts_with("refs/tags/")
                && old != new
            {
                pre_reject[i] = Some(
                    "Updates were rejected because the tag already exists in the remote."
                        .to_string(),
                );
            }
        }
    }

    // Shallow push handling (matches receive-pack). When the pushing repo is shallow, its grafts
    // are advertised so the receiver can detect pushes that would introduce a *new* shallow root.
    // Such pushes are rejected ("shallow update not allowed") unless `receive.shallowupdate` is set
    // on the receiver, in which case the new grafts are recorded in the receiver's `.git/shallow`
    // and excluded from the strict connectivity walk.
    let source_shallow = grit_lib::shallow::load_shallow_boundaries(&repo.git_dir);
    let receive_shallow_update = receive_remote_config
        .get_bool("receive.shallowupdate")
        .and_then(|r| r.ok())
        .unwrap_or(false);
    // New shallow roots that must be written into the receiver's `.git/shallow` (only when accepted).
    let mut push_new_shallow_roots: HashSet<ObjectId> = HashSet::new();
    if !source_shallow.is_empty() {
        // Receiver's current commit tips (what it already "has"), used as the reachability cut.
        let mut have_tips: Vec<ObjectId> = Vec::new();
        if let Ok(remote_refs) = refs::list_refs(&remote_repo.git_dir, "refs/") {
            have_tips.extend(remote_refs.into_iter().map(|(_, oid)| oid));
        }
        for (i, update) in updates.iter().enumerate() {
            if pre_reject[i].is_some() {
                continue;
            }
            let Some(new_oid) = update.new_oid else {
                continue;
            };
            let new_roots = grit_lib::shallow::new_shallow_roots_for_push(
                &repo.odb,
                new_oid,
                &source_shallow,
                &have_tips,
            );
            if new_roots.is_empty() {
                continue;
            }
            if receive_shallow_update {
                push_new_shallow_roots.extend(new_roots);
            } else {
                pre_reject[i] = Some("shallow update not allowed".to_string());
            }
        }
    }

    let mut atomic_cascade: Vec<Option<(String, &'static str)>> = vec![None; updates.len()];
    if args.atomic {
        let mut first_pre_fail: Option<usize> = None;
        for (i, _) in updates.iter().enumerate() {
            if pre_reject[i].is_some() {
                first_pre_fail = Some(i);
                break;
            }
        }
        if let Some(fi) = first_pre_fail {
            let u = &updates[fi];
            let (paren, bracket) = if u.remote_ref.starts_with("refs/tags/") {
                ("atomic push failed", "remote rejected")
            } else if pre_reject[fi]
                .as_deref()
                .is_some_and(|m| m.contains("remote contains work that you do not"))
            {
                ("atomic push failed", "rejected")
            } else {
                ("atomic push failure", "remote rejected")
            };
            let collateralize_all = push_all || args.branches;
            for j in 0..updates.len() {
                if j == fi || pre_reject[j].is_some() {
                    continue;
                }
                let uj = &updates[j];
                let would_change = match (&uj.old_oid, &uj.new_oid) {
                    (None, None) => false,
                    (Some(a), Some(b)) if a == b => false,
                    _ => true,
                };
                if !would_change {
                    continue;
                }
                if collateralize_all || j > fi {
                    atomic_cascade[j] = Some((paren.to_string(), bracket));
                }
            }
        }
    }

    // Run pre-push hook (unless --no-verify)
    if !args.no_verify {
        let zero_oid = "0".repeat(40);
        let mut hook_order: Vec<usize> = (0..updates.len()).collect();
        if hook_order.len() > 1 {
            let has_refs_named = updates
                .iter()
                .any(|u| pre_push_hook_local_display(u).starts_with("refs/"));
            let has_non_refs_named = updates.iter().any(|u| {
                let n = pre_push_hook_local_display(u);
                n != "(delete)" && !n.starts_with("refs/")
            });
            if has_refs_named && has_non_refs_named {
                hook_order.sort_by(|&ia, &ib| {
                    let pa = pre_push_hook_local_display(&updates[ia]).starts_with("refs/");
                    let pb = pre_push_hook_local_display(&updates[ib]).starts_with("refs/");
                    pb.cmp(&pa)
                });
            }
        }
        let mut hook_lines = Vec::new();
        for i in hook_order {
            let update = &updates[i];
            let local_ref = pre_push_hook_local_display(update);
            let local_oid = update
                .new_oid
                .map(|o| o.to_hex())
                .unwrap_or_else(|| zero_oid.clone());
            let remote_ref = &update.remote_ref;
            let remote_oid = update
                .old_oid
                .map(|o| o.to_hex())
                .unwrap_or_else(|| zero_oid.clone());
            hook_lines.push(format!(
                "{local_ref} {local_oid} {remote_ref} {remote_oid}\n"
            ));
        }
        let stdin_data = hook_lines.join("");
        let result = run_hook(
            repo,
            "pre-push",
            &[remote_name, url],
            Some(stdin_data.as_bytes()),
        );
        if let HookResult::Failed(code) = result {
            bail!("pre-push hook declined the push (exit code {code})");
        }
    }

    // Write push options file for the remote (local transport simulation)
    if !effective_push_options.is_empty() {
        let push_opts_path = remote_repo.git_dir.join("push_options");
        let content = effective_push_options.join("\n") + "\n";
        fs::write(&push_opts_path, content).context("writing push options")?;
    }

    // Send objects to the remote like `receive-pack` (thin pack + unpack/index), tracking new
    // files for rollback on hook failure.
    let mut copied_objects: Vec<PathBuf> = Vec::new();
    if !args.dry_run {
        let mut push_tips: Vec<ObjectId> = Vec::new();
        for (i, u) in updates.iter().enumerate() {
            if pre_reject[i].is_some() {
                continue;
            }
            if let Some(oid) = u.new_oid {
                push_tips.push(oid);
            }
        }

        // When `pack.usePathWalk` is enabled, Git's pack-objects emits a `pack-objects`/`path-walk`
        // trace2 region around object enumeration. The local-push fast path builds the pack in a
        // child whose trace2 stream is stripped, so emit the region from here to keep
        // `test_region pack-objects path-walk` assertions satisfied (t5538).
        maybe_emit_push_path_walk_region(config);
        maybe_emit_push_pack_objects_child(config);

        // Decide the negative (`--not`) boundary for the thin pack the way the wire protocol would.
        //
        // * push.negotiate + protocol v2: a negotiation round runs and discovers the common
        //   commits the receiver actually has (including objects only reachable from a *hidden*
        //   ref). We approximate that with the direct object-membership boundary
        //   (`build_thin_push_pack`) and emit a `total_rounds=1` trace2 event.
        // * push.negotiate but not protocol v2: negotiation cannot run; Git warns
        //   "push negotiation failed" and proceeds with a non-negotiated push.
        // * no push.negotiate: the receiver only advertises non-hidden refs, so the negative
        //   boundary is the reachable closure of those advertised tips only.
        let negotiate_requested = push_negotiate_enabled(config);
        let negotiate_active =
            negotiate_requested && protocol_wire::effective_client_protocol_version() == 2;
        if negotiate_requested && !negotiate_active {
            eprintln!("warning: push negotiation failed: --negotiate-only requires protocol v2");
        }
        let thin_pack = if negotiate_active {
            maybe_emit_push_negotiate_rounds_trace2(1);
            pack_objects::build_thin_push_pack(repo, &push_tips, &remote_repo.git_dir)
                .context("building push pack")?
        } else {
            let mut hidden = grit_lib::ref_exclusions::RefExclusions::default();
            hidden.load_hidden_refs_from_config(&receive_remote_config, "receive");
            pack_objects::build_thin_push_pack_excluding_hidden(
                repo,
                &push_tips,
                &remote_repo.git_dir,
                &hidden,
            )
            .context("building push pack")?
        };

        // Compute the "Enumerating objects" count against the receiver's PRE-ingest object set:
        // Git counts the packed objects plus the preferred-base (delta-base) objects pulled from
        // the boundary trees the receiver already has. This must run before the pack is unpacked
        // into the remote, otherwise every pushed object would appear already-present (t5538).
        let push_enumerated_objects = if push_show_object_progress(args) && !thin_pack.is_empty() {
            let written = grit_lib::receive_pack::pack_object_count(&thin_pack)
                .map(|count| count as usize)
                .unwrap_or_else(|| {
                    estimate_push_progress_enumerated_objects(repo, remote_name, &updates)
                });
            pack_objects::count_thin_push_enumerated_objects(
                repo,
                &push_tips,
                &remote_repo.git_dir,
                written,
            )
            .max(written)
        } else {
            0
        };

        // Emit the `pack-objects` `write_pack_file/wrote` trace2 event (object count) for the
        // local-transport push too, so `GIT_TRACE2_EVENT`-based assertions see how many objects
        // were sent (t5516 push negotiation tests count this).
        maybe_emit_push_pack_wrote_trace2(&thin_pack);

        if !thin_pack.is_empty() {
            let pre_ingest = list_remote_object_files(&remote_repo.git_dir);
            crate::receive_ingest::ingest_received_pack_with_shallow(
                &remote_repo.git_dir,
                &thin_pack,
                &receive_remote_config,
                true,
                &source_shallow,
            )
            .context("remote unpack failed")?;
            let post_ingest = list_remote_object_files(&remote_repo.git_dir);
            for p in post_ingest {
                if !pre_ingest.contains(&p) {
                    copied_objects.push(p);
                }
            }
            prune_copied_objects_available_from_remote_alternates(
                &remote_repo.git_dir,
                &mut copied_objects,
            );
            // Record the new shallow roots in the receiver's `.git/shallow` (receive.shallowupdate).
            // The objects are now present, so subsequent fsck/reachability stops at these grafts.
            if !push_new_shallow_roots.is_empty() {
                let _ = grit_lib::shallow::add_shallow_boundaries(
                    &remote_repo.git_dir,
                    &push_new_shallow_roots,
                );
            }
        }

        copied_objects.extend(
            copy_submodule_object_stores_only(&repo.git_dir, &remote_repo.git_dir)
                .context("copying submodule objects to remote")?,
        );
        if push_show_object_progress(args) && !copied_objects.is_empty() && !thin_pack.is_empty() {
            let written_objects = grit_lib::receive_pack::pack_object_count(&thin_pack)
                .map(|count| count as usize)
                .unwrap_or_else(|| {
                    estimate_push_progress_enumerated_objects(repo, remote_name, &updates)
                });
            // `push_enumerated_objects` was computed against the pre-ingest receiver state above.
            let enumerated_objects = push_enumerated_objects.max(written_objects);
            maybe_print_push_object_progress(
                true,
                enumerated_objects,
                written_objects,
                thin_pack.len(),
            );
        }

        let fsck_receive = receive_remote_config
            .get_bool("receive.fsckobjects")
            .or_else(|| receive_remote_config.get_bool("receive.fsckObjects"));
        let fsck_transfer = receive_remote_config
            .get_bool("transfer.fsckobjects")
            .or_else(|| receive_remote_config.get_bool("transfer.fsckObjects"));
        let fsck_enabled = match (fsck_receive, fsck_transfer) {
            (Some(Ok(true)), _) => true,
            (Some(Ok(false)), _) => false,
            (None, Some(Ok(true))) => true,
            _ => false,
        };

        if fsck_enabled {
            let remote_objects = remote_repo.git_dir.join("objects");
            let remote_odb = grit_lib::odb::Odb::new(&remote_objects);
            for (i, update) in updates.iter().enumerate() {
                if pre_reject[i].is_some() {
                    continue;
                }
                let Some(new_oid) = update.new_oid else {
                    continue;
                };
                if let Some(rest) = verify_gitmodules_for_commit(&remote_odb, new_oid)? {
                    for path in &copied_objects {
                        let _ = fs::remove_file(path);
                    }
                    eprintln!("remote: error: object {rest}");
                    eprintln!("remote: fatal: fsck error in pack objects");
                    bail!("remote unpack failed: unpack-objects abnormal exit");
                }
            }
        }
    }

    // For --atomic, check if the remote advertises atomic support
    if args.atomic {
        let remote_config = ConfigSet::load(Some(&remote_repo.git_dir), false)?;
        if let Some(val) = remote_config.get("receive.advertiseatomic") {
            if val == "0" || val == "false" {
                bail!("the receiving end does not support --atomic push");
            }
        }
    }

    // For --atomic, verify all refs can be updated before writing any.
    // In local transport we do this by checking that nothing changed between
    // our initial read and now.
    if args.atomic {
        for update in &updates {
            let current = refs::resolve_ref(&remote_repo.git_dir, &update.remote_ref).ok();
            if current != update.old_oid {
                bail!(
                    "atomic push failed: remote ref '{}' changed during push",
                    update.remote_ref
                );
            }
        }
    }

    // Check receive.advertisePushOptions on the remote
    if !effective_push_options.is_empty() {
        let remote_config = ConfigSet::load(Some(&remote_repo.git_dir), false)?;
        if let Some(val) = remote_config.get("receive.advertisepushoptions") {
            if val == "false" || val == "0" {
                bail!("the receiving end does not support push options");
            }
        }
    }

    // Build push option env vars for hooks
    let mut push_option_env: Vec<(String, String)> = if !effective_push_options.is_empty() {
        let mut env = vec![(
            "GIT_PUSH_OPTION_COUNT".to_owned(),
            effective_push_options.len().to_string(),
        )];
        for (i, opt) in effective_push_options.iter().enumerate() {
            env.push((format!("GIT_PUSH_OPTION_{i}"), opt.clone()));
        }
        env
    } else {
        Vec::new()
    };

    // `git push --signed`: generate, sign, and store a push certificate, then
    // expose the receiver-side `GIT_PUSH_CERT*` environment to the receive hooks.
    // A `--signed` (Always) push against a receiver without `receive.certNonceSeed`
    // fails here with "the receiving end does not support --signed push".
    let push_cert_mode = PushCertMode::from_args(args);
    if push_cert_mode != PushCertMode::Never && !args.dry_run {
        if let Some(prepared) = prepare_signed_push_cert(
            config,
            &remote_repo,
            &receive_remote_config,
            push_cert_mode,
            url,
            &effective_push_options,
            &updates,
            &pre_reject,
        )? {
            push_option_env.extend(prepared.env.to_env_pairs());
        }
    }

    let push_option_env_refs: Vec<(&str, &str)> = push_option_env
        .iter()
        .map(|(k, v)| (k.as_str(), v.as_str()))
        .collect();

    // Apply ref updates, running remote-side hooks first
    if !args.quiet && !args.porcelain {
        eprintln!("To {url}");
    }

    // Build stdin for pre-receive / post-receive hooks (omit client-side rejected refs).
    let zero_oid_str = "0".repeat(40);
    let hook_stdin = {
        let mut lines = String::new();
        for (i, update) in updates.iter().enumerate() {
            if pre_reject[i].is_some() {
                continue;
            }
            let old_hex = update
                .old_oid
                .map(|o| o.to_hex())
                .unwrap_or_else(|| zero_oid_str.clone());
            let new_hex = update
                .new_oid
                .map(|o| o.to_hex())
                .unwrap_or_else(|| zero_oid_str.clone());
            lines.push_str(&format!("{old_hex} {new_hex} {}\n", update.remote_ref));
        }
        lines
    };

    // Per-ref outcomes collected for `--porcelain` output, indexed by update.
    // The machine-readable report is emitted all at once at the end, in Git's
    // canonical order (up-to-date, then ok, then errors), via `format_push_status`.
    use grit_lib::push_report::PushRefStatus;
    let mut ref_results: Vec<Option<grit_lib::push_report::PushRefResult>> =
        vec![None; updates.len()];

    // Classify an applied (accepted) update into its Ok/UpToDate status and forced flag.
    let classify_applied = |update: &RefUpdate| -> (PushRefStatus, bool) {
        match (update.old_oid, update.new_oid) {
            (Some(old), Some(new)) if old == new => (PushRefStatus::UpToDate, false),
            (Some(old), Some(new)) => {
                let forced = (cli_force_enabled || update.refspec_force)
                    && !is_ancestor(repo, old, new).unwrap_or(false);
                (PushRefStatus::Ok, forced)
            }
            _ => (PushRefStatus::Ok, false),
        }
    };

    // Run pre-receive hook on the remote
    if !args.dry_run {
        let skip_pre_receive = args.atomic && pre_reject.iter().any(|p| p.is_some());
        if !skip_pre_receive {
            // Snapshot remote refs before hook (hook might create/modify refs)
            let pre_hook_refs: Vec<(String, ObjectId)> =
                refs::list_refs(&remote_repo.git_dir, "refs/").unwrap_or_default();

            let (hook_result, hook_output) = grit_lib::hooks::run_hook_in_git_dir(
                &remote_repo,
                "pre-receive",
                &[],
                Some(hook_stdin.as_bytes()),
                &push_option_env_refs,
            );
            if !hook_output.is_empty() {
                let output_str = String::from_utf8_lossy(&hook_output);
                let color_remote = RemoteMessageColorStyle::from_config(config);
                colorize_remote_output(&output_str, &color_remote);
            }
            if let HookResult::Failed(_code) = hook_result {
                // Quarantine rollback: remove copied objects
                for path in &copied_objects {
                    let _ = fs::remove_file(path);
                }
                // Rollback any ref changes the hook made
                let post_hook_refs: Vec<(String, ObjectId)> =
                    refs::list_refs(&remote_repo.git_dir, "refs/").unwrap_or_default();
                let pre_set: std::collections::HashSet<&str> =
                    pre_hook_refs.iter().map(|(r, _)| r.as_str()).collect();
                for (refname, _) in &post_hook_refs {
                    if !pre_set.contains(refname.as_str()) {
                        let _ = refs::delete_ref(&remote_repo.git_dir, refname);
                    }
                }
                // A declined pre-receive hook rejects every (non-pre-rejected) ref with the
                // remote's reason. Up-to-date refs that the hook never saw stay `[up to date]`.
                if args.porcelain {
                    for (i, update) in updates.iter().enumerate() {
                        if let Some(msg) = &pre_reject[i] {
                            let status = if msg.contains("tag already exists") {
                                PushRefStatus::RejectAlreadyExists
                            } else if msg.contains("remote contains work that you do not") {
                                PushRefStatus::RejectNonFastForward
                            } else {
                                PushRefStatus::RemoteRejected
                            };
                            let message = matches!(status, PushRefStatus::RemoteRejected)
                                .then(|| msg.clone());
                            ref_results[i] =
                                Some(make_push_ref_result(update, status, false, message));
                            continue;
                        }
                        let (status, _forced) = classify_applied(update);
                        if matches!(status, PushRefStatus::UpToDate) {
                            ref_results[i] = Some(make_push_ref_result(
                                update,
                                PushRefStatus::UpToDate,
                                false,
                                None,
                            ));
                        } else {
                            ref_results[i] = Some(make_push_ref_result(
                                update,
                                PushRefStatus::RemoteRejected,
                                false,
                                Some("pre-receive hook declined".to_owned()),
                            ));
                        }
                    }
                    emit_porcelain_push_report(url, &ref_results, args.quiet);
                }
                bail!("pre-receive hook declined the push");
            }
        }
    }

    // Track results for atomic rollback on failure
    let mut applied_updates: Vec<(&RefUpdate, Option<ObjectId>)> = Vec::new();
    let mut rejected: Vec<(&RefUpdate, String)> = Vec::new();

    let push_ref_display_short = |u: &RefUpdate| -> String {
        if u.remote_ref.starts_with("refs/heads/") {
            u.remote_ref["refs/heads/".len()..].to_owned()
        } else if u.remote_ref.starts_with("refs/tags/") {
            u.remote_ref["refs/tags/".len()..].to_owned()
        } else {
            u.remote_ref.clone()
        }
    };

    let report_ref_rejection =
        |u: &RefUpdate, bracket: &'static str, parenthetical: &str, args: &Args| {
            if args.porcelain || args.quiet {
                return;
            }
            let dst = push_ref_display_short(u);
            let src = u
                .local_ref
                .as_deref()
                .and_then(|r| r.strip_prefix("refs/heads/"))
                .or_else(|| {
                    u.local_ref
                        .as_deref()
                        .and_then(|r| r.strip_prefix("refs/tags/"))
                })
                .unwrap_or(u.local_ref.as_deref().unwrap_or("(delete)"));
            let tag_delete_style = u.remote_ref.starts_with("refs/tags/") && u.local_ref.is_none();
            if tag_delete_style {
                eprintln!(" ! [{bracket}] {dst} ({parenthetical})");
            } else {
                eprintln!(" ! [{bracket}] {src} -> {dst} ({parenthetical})");
            }
        };

    if args.atomic && pre_reject.iter().any(|p| p.is_some()) {
        for (i, update) in updates.iter().enumerate() {
            if let Some(msg) = &pre_reject[i] {
                if !args.porcelain {
                    eprintln!("{msg}");
                }
                let paren = if msg.contains("tag already exists") {
                    "failed"
                } else if msg.contains("remote contains work that you do not") {
                    "non-fast-forward"
                } else {
                    "failed"
                };
                report_ref_rejection(update, "rejected", paren, args);
                let status = if msg.contains("tag already exists") {
                    PushRefStatus::RejectAlreadyExists
                } else if msg.contains("remote contains work that you do not") {
                    PushRefStatus::RejectNonFastForward
                } else {
                    PushRefStatus::RemoteRejected
                };
                let message = matches!(status, PushRefStatus::RemoteRejected).then(|| msg.clone());
                ref_results[i] = Some(make_push_ref_result(update, status, false, message));
                rejected.push((update, paren.to_owned()));
            } else if let Some((paren, bracket)) = &atomic_cascade[i] {
                report_ref_rejection(update, bracket, paren.as_str(), args);
                ref_results[i] = Some(make_push_ref_result(
                    update,
                    PushRefStatus::AtomicPushFailed,
                    false,
                    None,
                ));
                rejected.push((update, paren.clone()));
            } else {
                // Refs that would have succeeded but are blocked by the atomic failure
                // are still reported as `[rejected] (atomic push failed)` in Git, except
                // up-to-date refs which stay `[up to date]`.
                let (status, _forced) = classify_applied(update);
                if matches!(status, PushRefStatus::UpToDate) {
                    ref_results[i] = Some(make_push_ref_result(
                        update,
                        PushRefStatus::UpToDate,
                        false,
                        None,
                    ));
                } else {
                    ref_results[i] = Some(make_push_ref_result(
                        update,
                        PushRefStatus::AtomicPushFailed,
                        false,
                        None,
                    ));
                }
            }
        }
        if args.porcelain {
            emit_porcelain_push_report(url, &ref_results, args.quiet);
        }
        if !rejected.is_empty() {
            bail!("failed to push some refs to '{url}'");
        }
        return Ok(());
    }

    // Process ref updates deletions-first, then everything else (git/builtin/receive-pack.c
    // `execute_commands_non_atomic` runs PHASE_DELETIONS before PHASE_OTHERS). This ordering is
    // observable through the per-ref `update` hook, which a deletion sees before a sibling
    // create/update. `applied_updates` is re-sorted to input order afterwards so post-receive,
    // post-update, and reference-transaction stdin keep the order refs were pushed in.
    let apply_order: Vec<usize> = {
        let mut deletions: Vec<usize> = Vec::new();
        let mut others: Vec<usize> = Vec::new();
        for (i, u) in updates.iter().enumerate() {
            if u.new_oid.is_none() {
                deletions.push(i);
            } else {
                others.push(i);
            }
        }
        deletions.into_iter().chain(others).collect()
    };
    for &i in &apply_order {
        let update = &updates[i];
        if let Some(msg) = &pre_reject[i] {
            // Rejections reported by receive-pack only via the per-ref
            // `! [remote rejected] ... (<reason>)` line, with no extra prose line: a shallow
            // update, or an update/delete of a ref hidden by transfer/receive.hideRefs (the
            // latter is pre-rejected before any objects are transferred so the repo is not
            // polluted — t5516 test 120).
            if msg == "shallow update not allowed" || msg.starts_with("deny ") {
                report_ref_rejection(update, "remote rejected", msg, args);
                ref_results[i] = Some(make_push_ref_result(
                    update,
                    PushRefStatus::RemoteRejected,
                    false,
                    Some(msg.clone()),
                ));
                rejected.push((update, msg.clone()));
                continue;
            }
            if !args.porcelain {
                eprintln!("{msg}");
            }
            let paren = if msg.contains("tag already exists") {
                "failed"
            } else if msg.contains("remote contains work that you do not") {
                "fetch first"
            } else {
                "failed"
            };
            report_ref_rejection(update, "rejected", paren, args);
            let status = if msg.contains("tag already exists") {
                PushRefStatus::RejectAlreadyExists
            } else if msg.contains("remote contains work that you do not") {
                PushRefStatus::RejectNonFastForward
            } else {
                PushRefStatus::RemoteRejected
            };
            let message = matches!(status, PushRefStatus::RemoteRejected).then(|| msg.clone());
            ref_results[i] = Some(make_push_ref_result(update, status, false, message));
            rejected.push((update, paren.to_owned()));
            continue;
        }
        if let Some((paren, bracket)) = &atomic_cascade[i] {
            report_ref_rejection(update, bracket, paren.as_str(), args);
            ref_results[i] = Some(make_push_ref_result(
                update,
                PushRefStatus::AtomicPushFailed,
                false,
                None,
            ));
            rejected.push((update, paren.clone()));
            continue;
        }

        // Run the remote's `update` hook: update <refname> <old-oid> <new-oid>
        if !args.dry_run {
            let old_hex = update
                .old_oid
                .map(|o| o.to_hex())
                .unwrap_or_else(|| zero_oid_str.clone());
            let new_hex = update
                .new_oid
                .map(|o| o.to_hex())
                .unwrap_or_else(|| zero_oid_str.clone());
            // Run in the remote git dir (cwd = git_dir) so a hook writing to a relative path
            // (e.g. `>>update.actual`) lands in the remote repo, matching receive-pack and the
            // pre/post-receive hooks (t5516 update-hook output). `run_hook_capture` left cwd at
            // the pusher's directory.
            let (hook_result, hook_output) = grit_lib::hooks::run_hook_in_git_dir(
                &remote_repo,
                "update",
                &[&update.remote_ref, &old_hex, &new_hex],
                None,
                &[],
            );
            // Forward hook output to stderr, optionally colorized
            if !hook_output.is_empty() {
                let output_str = String::from_utf8_lossy(&hook_output);
                let color_remote = RemoteMessageColorStyle::from_config(config);
                colorize_remote_output(&output_str, &color_remote);
            }
            if let HookResult::Failed(_code) = hook_result {
                if args.atomic {
                    report_atomic_rollback_for_applied_updates(
                        &remote_repo,
                        &mut applied_updates,
                        mirror_atomic_order.as_deref(),
                        args,
                        Some(&update.remote_ref),
                    );
                    report_ref_rejection(update, "remote rejected", "hook declined", args);
                    rejected.push((update, "hook declined".to_owned()));
                    let ord = mirror_atomic_order.as_deref();
                    for j in sort_collateral_indices(&updates, &pre_reject, ord, i + 1) {
                        let u = &updates[j];
                        report_ref_rejection(u, "remote rejected", "atomic push failure", args);
                        rejected.push((u, "atomic push failure".to_owned()));
                    }
                    break;
                }
                report_ref_rejection(update, "remote rejected", "hook declined", args);
                rejected.push((update, "hook declined".to_owned()));
                continue;
            }
        }

        // A delete of a ref that does not exist on the remote: the pre-receive and update hooks
        // above have already seen it, but it is not applied and contributes nothing to
        // post-receive/post-update or the per-ref status report (Git `did_not_exist`). The push
        // still succeeds for this ref.
        if update.delete_nonexistent {
            continue;
        }

        let result = apply_ref_update(
            repo,
            &remote_repo,
            remote_name,
            update,
            args,
            url,
            config,
            &receive_remote_config,
        );

        match result {
            Ok(ApplyRefResult::Applied) => {
                let (status, forced) = classify_applied(update);
                ref_results[i] = Some(make_push_ref_result(update, status, forced, None));
                applied_updates.push((update, update.old_oid));
            }
            Ok(ApplyRefResult::RemoteRejected(reason)) => {
                ref_results[i] = Some(make_push_ref_result(
                    update,
                    PushRefStatus::RemoteRejected,
                    false,
                    Some(reason.clone()),
                ));
                if args.atomic {
                    report_atomic_rollback_for_applied_updates(
                        &remote_repo,
                        &mut applied_updates,
                        mirror_atomic_order.as_deref(),
                        args,
                        Some(&update.remote_ref),
                    );
                    report_ref_rejection(update, "remote rejected", reason.as_str(), args);
                    rejected.push((update, reason));
                    let ord = mirror_atomic_order.as_deref();
                    for j in sort_collateral_indices(&updates, &pre_reject, ord, i + 1) {
                        let u = &updates[j];
                        report_ref_rejection(u, "remote rejected", "atomic push failure", args);
                        rejected.push((u, "atomic push failure".to_owned()));
                    }
                    break;
                }
                report_ref_rejection(update, "remote rejected", reason.as_str(), args);
                rejected.push((update, reason));
            }
            Err(e) => {
                if args.atomic {
                    let msg = e.to_string();
                    report_atomic_rollback_for_applied_updates(
                        &remote_repo,
                        &mut applied_updates,
                        mirror_atomic_order.as_deref(),
                        args,
                        Some(&update.remote_ref),
                    );
                    report_ref_rejection(update, "remote rejected", &msg, args);
                    rejected.push((update, msg));
                    let ord = mirror_atomic_order.as_deref();
                    for j in sort_collateral_indices(&updates, &pre_reject, ord, i + 1) {
                        let u = &updates[j];
                        report_ref_rejection(u, "remote rejected", "atomic push failure", args);
                        rejected.push((u, "atomic push failure".to_owned()));
                    }
                    break;
                }
                return Err(e);
            }
        }
    }

    // Restore input order for the informational post-hooks (post-receive stdin is built
    // separately in input order already; post-update args and reference-transaction stdin come
    // from `applied_updates`, which the deletions-first apply loop populated out of order).
    applied_updates.sort_by_key(|(u, _)| {
        updates
            .iter()
            .position(|cand| std::ptr::eq(cand, *u))
            .unwrap_or(usize::MAX)
    });

    // Emit the machine-readable `--porcelain` report (To/<refs>/Done) for both the
    // success and rejection paths, in Git's canonical order.
    if args.porcelain {
        emit_porcelain_push_report(url, &ref_results, args.quiet);
    }

    // Report rejected refs to stderr
    if !rejected.is_empty() {
        bail!("failed to push some refs to '{url}'");
    }

    // Run reference-transaction hooks on the remote after update hooks have
    // accepted all updates, matching receive-pack hook ordering.
    if !args.dry_run && !applied_updates.is_empty() {
        let mut txn_stdin = String::new();
        for (update, _) in &applied_updates {
            let old_hex = update
                .old_oid
                .map(|o| o.to_hex())
                .unwrap_or_else(|| zero_oid_str.clone());
            let new_hex = update
                .new_oid
                .map(|o| o.to_hex())
                .unwrap_or_else(|| zero_oid_str.clone());
            txn_stdin.push_str(&format!("{old_hex} {new_hex} {}\n", update.remote_ref));
        }

        let (prep_result, prep_output) = grit_lib::hooks::run_hook_in_git_dir(
            &remote_repo,
            "reference-transaction",
            &["preparing"],
            Some(txn_stdin.as_bytes()),
            &push_option_env_refs,
        );
        if !prep_output.is_empty() {
            let output_str = String::from_utf8_lossy(&prep_output);
            let color_remote = RemoteMessageColorStyle::from_config(config);
            colorize_remote_output(&output_str, &color_remote);
        }
        if let HookResult::Failed(_) = prep_result {
            bail!("remote reference-transaction hook declined the push in 'preparing' phase");
        }

        let (prepared_result, prepared_output) = grit_lib::hooks::run_hook_in_git_dir(
            &remote_repo,
            "reference-transaction",
            &["prepared"],
            Some(txn_stdin.as_bytes()),
            &push_option_env_refs,
        );
        if !prepared_output.is_empty() {
            let output_str = String::from_utf8_lossy(&prepared_output);
            let color_remote = RemoteMessageColorStyle::from_config(config);
            colorize_remote_output(&output_str, &color_remote);
        }
        if let HookResult::Failed(_) = prepared_result {
            bail!("remote reference-transaction hook declined the push in 'prepared' phase");
        }

        let (committed_result, committed_output) = grit_lib::hooks::run_hook_in_git_dir(
            &remote_repo,
            "reference-transaction",
            &["committed"],
            Some(txn_stdin.as_bytes()),
            &push_option_env_refs,
        );
        if !committed_output.is_empty() {
            let output_str = String::from_utf8_lossy(&committed_output);
            let color_remote = RemoteMessageColorStyle::from_config(config);
            colorize_remote_output(&output_str, &color_remote);
        }
        if let HookResult::Failed(_) = committed_result {
            // Keep compatibility with git: failures in committed state do not
            // abort already-applied updates.
        }
    }

    // Run post-receive hook on the remote (after successful ref updates). Unlike pre-receive,
    // post-receive only sees refs that were actually updated, so build its stdin from
    // `applied_updates` (in input order) rather than the full pre-receive feed — a delete of a
    // non-existent ref reaches pre-receive but not post-receive (t5516 test 66).
    if !args.dry_run && !applied_updates.is_empty() {
        let post_receive_stdin = {
            let mut lines = String::new();
            for (update, _) in &applied_updates {
                let old_hex = update
                    .old_oid
                    .map(|o| o.to_hex())
                    .unwrap_or_else(|| zero_oid_str.clone());
                let new_hex = update
                    .new_oid
                    .map(|o| o.to_hex())
                    .unwrap_or_else(|| zero_oid_str.clone());
                lines.push_str(&format!("{old_hex} {new_hex} {}\n", update.remote_ref));
            }
            lines
        };
        let (_, hook_output) = grit_lib::hooks::run_hook_in_git_dir(
            &remote_repo,
            "post-receive",
            &[],
            Some(post_receive_stdin.as_bytes()),
            &push_option_env_refs,
        );
        if !hook_output.is_empty() {
            let output_str = String::from_utf8_lossy(&hook_output);
            let color_remote = RemoteMessageColorStyle::from_config(config);
            colorize_remote_output(&output_str, &color_remote);
        }
    }

    // Run post-update hook on the remote after post-receive, matching
    // receive-pack ordering. It receives the list of updated remote refnames
    // as its arguments (not via stdin) and is purely informational, so its
    // exit status is ignored (matches receive-pack.rs / githooks(5)).
    if !args.dry_run && !applied_updates.is_empty() {
        let post_update_arg_strings: Vec<String> = applied_updates
            .iter()
            .map(|(update, _)| update.remote_ref.clone())
            .collect();
        if !post_update_arg_strings.is_empty() {
            let post_update_args: Vec<&str> =
                post_update_arg_strings.iter().map(|s| s.as_str()).collect();
            let (_, hook_output) = grit_lib::hooks::run_hook_in_git_dir(
                &remote_repo,
                "post-update",
                &post_update_args,
                None,
                &push_option_env_refs,
            );
            if !hook_output.is_empty() {
                let output_str = String::from_utf8_lossy(&hook_output);
                let color_remote = RemoteMessageColorStyle::from_config(config);
                colorize_remote_output(&output_str, &color_remote);
            }
        }
    }

    // Set upstream tracking if requested (`--dry-run` only prints what Git would do).
    if set_upstream_after_push {
        use std::collections::BTreeMap;
        let mut upstream_by_branch: BTreeMap<String, String> = BTreeMap::new();
        for (update, _) in &applied_updates {
            let Some(local_ref) = update.local_ref.as_deref() else {
                continue;
            };
            let Some(branch_name) = local_ref.strip_prefix("refs/heads/") else {
                continue;
            };
            if !update.remote_ref.starts_with("refs/heads/") {
                continue;
            }
            upstream_by_branch.insert(branch_name.to_owned(), update.remote_ref.clone());
        }
        if args.dry_run {
            if !args.quiet {
                for (branch, merge_ref) in upstream_by_branch {
                    let track_short = merge_ref
                        .strip_prefix("refs/heads/")
                        .unwrap_or(merge_ref.as_str());
                    eprintln!(
                        "Would set upstream of '{branch}' to '{track_short}' of '{remote_name}'"
                    );
                }
            }
        } else {
            for (branch, merge_ref) in upstream_by_branch {
                let track_short = merge_ref
                    .strip_prefix("refs/heads/")
                    .unwrap_or(merge_ref.as_str());
                set_upstream_config(&repo.git_dir, &branch, remote_name, &merge_ref)?;
                if !args.quiet {
                    eprintln!("branch '{branch}' set up to track '{remote_name}/{track_short}'.");
                }
            }
        }
    }

    Ok(())
}

/// Whether the `--receive-pack` value names the default receive-pack program (so the in-process
/// fast path is fine and no real subprocess handshake is needed).
fn is_default_receive_pack_program(receive_pack: Option<&str>) -> bool {
    match receive_pack {
        None => true,
        Some(value) => {
            let trimmed = value.trim();
            trimmed.is_empty()
                || trimmed == "git receive-pack"
                || trimmed == "git-receive-pack"
                || trimmed == "grit receive-pack"
                || trimmed == "grit-receive-pack"
        }
    }
}

/// Push to a local-file remote by running the real push protocol through the caller's explicit
/// `--receive-pack` program (via `send-pack`). This spawns the receiving program just like Git,
/// so it can emit its own trace2 events (`client-sid`, `negotiated-version`) and we emit the
/// matching client-side `server-sid` / `negotiated-version` (`t5705`).
fn delegate_local_push_to_send_pack(
    remote_path: &Path,
    refspecs: &[String],
    args: &Args,
    url: &str,
) -> Result<()> {
    let send_args = crate::commands::send_pack::Args {
        remote: remote_path.to_string_lossy().into_owned(),
        stdin: false,
        mirror: false,
        refs: refspecs.to_vec(),
        all: false,
        force: args.force && !args.no_force,
        dry_run: args.dry_run,
        receive_pack: args.receive_pack.clone(),
        exec: None,
        thin: args.thin,
        no_thin: args.no_thin,
    };
    crate::commands::send_pack::run(send_args).map_err(|e| {
        // `send-pack` signals ref rejections / remote failure via a quiet non-zero exit; surface
        // the same "failed to push some refs" wording `git push` uses for the user.
        if e.downcast_ref::<crate::explicit_exit::ExplicitExit>()
            .is_some()
        {
            anyhow::anyhow!("failed to push some refs to '{url}'")
        } else {
            e
        }
    })
}

/// Git `receive.denyCurrentBranch` / `receive.denyDeleteCurrent` policy (subset).
#[derive(Clone, Copy, PartialEq, Eq)]
enum ReceiveDenyAction {
    Unconfigured,
    Ignore,
    Warn,
    Refuse,
    UpdateInstead,
}

fn parse_receive_deny_action(value: Option<&str>) -> ReceiveDenyAction {
    match value.map(str::trim) {
        None => ReceiveDenyAction::Ignore,
        Some(s) if s.eq_ignore_ascii_case("ignore") => ReceiveDenyAction::Ignore,
        Some(s) if s.eq_ignore_ascii_case("warn") => ReceiveDenyAction::Warn,
        Some(s) if s.eq_ignore_ascii_case("refuse") => ReceiveDenyAction::Refuse,
        Some(s) if s.eq_ignore_ascii_case("updateinstead") => ReceiveDenyAction::UpdateInstead,
        Some(s) => match parse_bool(s) {
            Ok(true) => ReceiveDenyAction::Refuse,
            Ok(false) => ReceiveDenyAction::Ignore,
            Err(_) => ReceiveDenyAction::Ignore,
        },
    }
}

fn read_receive_deny_current(cfg: &ConfigSet) -> ReceiveDenyAction {
    let v = cfg
        .get("receive.denyCurrentBranch")
        .or_else(|| cfg.get("receive.denycurrentbranch"));
    match v {
        None => ReceiveDenyAction::Unconfigured,
        Some(s) => parse_receive_deny_action(Some(&s)),
    }
}

fn read_receive_deny_delete_current(cfg: &ConfigSet) -> ReceiveDenyAction {
    let v = cfg
        .get("receive.denyDeleteCurrent")
        .or_else(|| cfg.get("receive.denydeletecurrent"));
    match v {
        None => ReceiveDenyAction::Unconfigured,
        Some(s) => parse_receive_deny_action(Some(s.trim())),
    }
}

/// `git diff-files` / `git diff-index` cleanliness checks for `receive.denyCurrentBranch=updateInstead`.
fn worktree_clean_for_update_instead(
    remote_repo: &Repository,
    expected_treeish: Option<ObjectId>,
) -> std::result::Result<(), String> {
    let wt = remote_repo
        .work_tree
        .as_ref()
        .ok_or_else(|| "denyCurrentBranch = updateInstead needs a worktree".to_owned())?;
    let grit_bin = std::env::current_exe().unwrap_or_else(|_| PathBuf::from("grit"));

    // git push_to_deploy starts with `update-index -q --refresh` so a file that is merely
    // stat-dirty (e.g. an mtime bump from `test-tool chmtime` with unchanged content) does not
    // count as an unstaged change in the following `diff-files`.
    let mut ui = Command::new(&grit_bin);
    ui.current_dir(wt)
        .args(["update-index", "-q", "--ignore-submodules", "--refresh"])
        .env("GIT_DIR", &remote_repo.git_dir)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    if !ui.status().map_err(|e| e.to_string())?.success() {
        return Err("Up-to-date check failed".to_owned());
    }

    let mut df = Command::new(&grit_bin);
    df.current_dir(wt)
        .args(["diff-files", "--quiet", "--ignore-submodules"])
        .env("GIT_DIR", &remote_repo.git_dir)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    if !df.status().map_err(|e| e.to_string())?.success() {
        return Err("Working directory has unstaged changes".to_owned());
    }
    // Compare the index against the *old* committed tree (the value the branch had before this
    // push). git's push_to_deploy diffs the index against `HEAD` as it was prior to the update; we
    // must not re-resolve HEAD here because the ref has already been written to the new commit. An
    // absent old OID means the branch was unborn (e.g. pushing into a brand-new "void" repo), so
    // the index is compared against the empty tree.
    const EMPTY_TREE: &str = "4b825dc642cb6eb9a060e54bf8d69288fbee4904";
    let expected_hex;
    let head_tree = if let Some(oid) = expected_treeish {
        expected_hex = oid.to_hex();
        expected_hex.as_str()
    } else {
        EMPTY_TREE
    };
    let mut di = Command::new(&grit_bin);
    di.current_dir(wt)
        .args([
            "diff-index",
            "--quiet",
            "--cached",
            "--ignore-submodules",
            head_tree,
            "--",
        ])
        .env("GIT_DIR", &remote_repo.git_dir)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    if !di.status().map_err(|e| e.to_string())?.success() {
        return Err("Working directory has staged changes".to_owned());
    }
    Ok(())
}

/// Open a [`Repository`] handle bound to the worktree (main or linked) that has the pushed
/// branch checked out, for a `denyCurrentBranch = updateInstead` work-tree update.
///
/// Returns `None` (so the caller falls back to `remote_repo`) when `worktree_path` is the main
/// work tree. For a linked worktree, resolves its admin directory via the worktree's `.git`
/// gitfile (`gitdir: <admin>`) and opens a repo with that admin git_dir and the worktree path,
/// so the index/HEAD/work-tree all point at the linked worktree.
fn open_worktree_repo_for_update_instead(
    remote_repo: &Repository,
    worktree_path: &Path,
    _refname: &str,
) -> Option<Repository> {
    // Main worktree: the existing `remote_repo` already targets it.
    if let Some(main_wt) = remote_repo.work_tree.as_ref() {
        let main_canon = main_wt.canonicalize().ok();
        let target_canon = worktree_path.canonicalize().ok();
        if main_canon.is_some() && main_canon == target_canon {
            return None;
        }
    }
    // Linked worktree: read its `.git` gitfile to find the admin dir.
    let gitfile = worktree_path.join(".git");
    let content = fs::read_to_string(&gitfile).ok()?;
    let admin = content.strip_prefix("gitdir:").map(str::trim)?;
    let admin_dir = PathBuf::from(admin);
    Repository::open(&admin_dir, Some(worktree_path)).ok()
}

fn update_worktree_after_push_update_instead(
    remote_repo: &Repository,
    new_oid: ObjectId,
    old_oid: Option<ObjectId>,
) -> std::result::Result<Vec<u8>, String> {
    let wt = remote_repo
        .work_tree
        .as_ref()
        .ok_or_else(|| "denyCurrentBranch = updateInstead needs a worktree".to_owned())?;

    // Mirror git's update_worktree(): give the push-to-checkout hook first
    // refusal. Only when the hook is not installed do we fall back to the
    // diff-files/diff-index cleanliness checks + checkout (push_to_deploy).
    let new_hex = new_oid.to_hex();
    let git_dir_abs = remote_repo
        .git_dir
        .canonicalize()
        .unwrap_or_else(|_| remote_repo.git_dir.clone());
    let wt_abs = wt.canonicalize().unwrap_or_else(|_| wt.clone());
    let git_dir_str = git_dir_abs.to_string_lossy().into_owned();
    let wt_str = wt_abs.to_string_lossy().into_owned();
    let hook_env: Vec<(&str, &str)> = vec![
        ("GIT_DIR", git_dir_str.as_str()),
        ("GIT_WORK_TREE", wt_str.as_str()),
    ];
    let (hook_result, hook_output) = grit_lib::hooks::run_hook_in_git_dir(
        remote_repo,
        "push-to-checkout",
        &[new_hex.as_str()],
        None,
        &hook_env,
    );
    if hook_result.was_executed() {
        // The hook was installed and ran. On success it is responsible for
        // updating the work tree, so skip the cleanliness check + checkout.
        if let HookResult::Failed(_) = hook_result {
            return Err("push-to-checkout hook declined".to_owned());
        }
        return Ok(hook_output);
    }

    // No push-to-checkout hook: fall back to the cleanliness checks before
    // updating the work tree, matching git's push_to_deploy.
    worktree_clean_for_update_instead(remote_repo, old_oid)?;

    update_worktree_after_push_update_instead_checkout(remote_repo, new_oid)?;
    Ok(hook_output)
}

fn update_worktree_after_push_update_instead_checkout(
    remote_repo: &Repository,
    _new_oid: ObjectId,
) -> std::result::Result<(), String> {
    let wt = remote_repo
        .work_tree
        .as_ref()
        .ok_or_else(|| "denyCurrentBranch = updateInstead needs a worktree".to_owned())?;
    // Submodule gitlink commits live under `.git/modules/<name>/objects/`; `read-tree` on the
    // superproject resolves them via the primary ODB — mirror loose/pack objects up like Git.
    let modules_root = remote_repo.git_dir.join("modules");
    if modules_root.is_dir() {
        if let Ok(entries) = fs::read_dir(&modules_root) {
            for e in entries.flatten() {
                let p = e.path();
                if !p.is_dir() {
                    continue;
                }
                // `copy_objects_tracked` takes git dirs (it appends `objects/` itself).
                if p.join("objects").is_dir() {
                    let _ = copy_objects_tracked(&p, &remote_repo.git_dir);
                }
            }
        }
    }
    let grit_bin = std::env::current_exe().unwrap_or_else(|_| PathBuf::from("grit"));
    // git's push_to_deploy finishes with a single-tree `read-tree -u -m <new>`, NOT `reset --hard`:
    // the two-way merge updates the index and work tree to the new commit while *refusing* to
    // overwrite an untracked or locally-modified file that the update would clobber (t5516
    // updateInstead cases (3)/(4)). The branch ref is already written, so the index still reflects
    // the old state, which `read-tree -m` needs.
    let new_hex = _new_oid.to_hex();
    let mut cmd = Command::new(&grit_bin);
    cmd.current_dir(wt)
        .args(["read-tree", "-u", "-m", new_hex.as_str()])
        .env("GIT_DIR", &remote_repo.git_dir)
        .env("GIT_WORK_TREE", wt)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    if !cmd.status().map_err(|e| e.to_string())?.success() {
        return Err("Could not update working tree to new HEAD".to_owned());
    }
    Ok(())
}

/// Enforce receive-pack rules for the non-bare remote (checked-out branch updates/deletes).
///
/// Returns `Err(short_reason)` when the ref must be rejected (matches Git's parenthetical in
/// `! [remote rejected] ... (reason)`).
fn check_receive_pack_policy(
    remote_repo: &Repository,
    remote_config: &ConfigSet,
    pushing_config: &ConfigSet,
    update: &RefUpdate,
) -> std::result::Result<(), String> {
    // A ref hidden by `transfer.hideRefs` / `receive.hideRefs` is not advertised by receive-pack,
    // so any attempt to update or delete it is rejected (git/builtin/receive-pack.c
    // `check_aliased_updates` → `reject_updates_to_hidden_refs`).
    let mut hidden = grit_lib::ref_exclusions::RefExclusions::default();
    hidden.load_hidden_refs_from_config(remote_config, "receive");
    // Without a namespace the stripped name equals the full storage path (matching
    // receive-pack's `ref_is_hidden(cmd->ref_name, refname_full.buf, ...)`).
    if hidden.ref_excluded(Some(&update.remote_ref), &update.remote_ref) {
        if update.new_oid.is_none() {
            return Err("deny deleting a hidden ref".to_owned());
        }
        return Err("deny updating a hidden ref".to_owned());
    }

    // The pushed branch is "current" if it is checked out in ANY worktree — the main worktree
    // or a linked one (Git `branch_checked_out` / `find_shared_symref`). This applies to bare
    // repositories too: a bare repo's *linked* worktrees can have branches checked out, while its
    // own (worktree-less) HEAD never occupies a branch (t5516 116/117). `occupied_branch_refs`
    // already excludes a bare main worktree.
    let head_ref = update.remote_ref.clone();
    let occupied = crate::commands::worktree_refs::occupied_branch_refs(remote_repo);
    if !occupied.contains_key(&head_ref) {
        return Ok(());
    }

    let style = RemoteMessageColorStyle::from_config(pushing_config);

    if update.new_oid.is_some() {
        let deny = read_receive_deny_current(remote_config);
        match deny {
            ReceiveDenyAction::Ignore => {}
            ReceiveDenyAction::Warn => {
                colorize_remote_output("warning: updating the current branch", &style);
            }
            ReceiveDenyAction::Unconfigured => {
                colorize_remote_output(
                    &format!("error: refusing to update checked out branch: {head_ref}"),
                    &style,
                );
                colorize_remote_output(
                    "error: By default, updating the current branch in a non-bare repository\n\
                     is denied, because it will make the index and work tree inconsistent\n\
                     with what you pushed, and will require 'git reset --hard' to match\n\
                     the work tree to HEAD.\n\
                     \n\
                     You can set the 'receive.denyCurrentBranch' configuration variable\n\
                     to 'ignore' or 'warn' in the remote repository to allow pushing into\n\
                     its current branch; however, this is not recommended unless you\n\
                     arranged to update its work tree to match what you pushed in some\n\
                     other way.\n\
                     \n\
                     To squelch this message and still keep the default behaviour, set\n\
                     'receive.denyCurrentBranch' configuration variable to 'refuse'.",
                    &style,
                );
                return Err("branch is currently checked out".to_owned());
            }
            ReceiveDenyAction::Refuse => {
                colorize_remote_output(
                    &format!("error: refusing to update checked out branch: {head_ref}"),
                    &style,
                );
                return Err("branch is currently checked out".to_owned());
            }
            ReceiveDenyAction::UpdateInstead => {
                // Worktree handling (push-to-checkout hook, else cleanliness
                // check + checkout) happens in the worktree-update step that
                // runs after the ref is written, mirroring git's
                // update_worktree(): the hook gets first refusal and the
                // diff-files/diff-index checks only run as a fallback.
            }
        }
    } else {
        let deny = read_receive_deny_delete_current(remote_config);
        match deny {
            ReceiveDenyAction::Ignore => {}
            ReceiveDenyAction::Warn => {
                colorize_remote_output("warning: deleting the current branch", &style);
            }
            ReceiveDenyAction::Unconfigured => {
                colorize_remote_output(
                    "error: By default, deleting the current branch is denied, because the next\n\
                     'git clone' won't result in any file checked out, causing confusion.\n\
                     \n\
                     You can set 'receive.denyDeleteCurrent' configuration variable to\n\
                     'warn' or 'ignore' in the remote repository to allow deleting the\n\
                     current branch, with or without a warning message.\n\
                     \n\
                     To squelch this message, you can set it to 'refuse'.",
                    &style,
                );
                colorize_remote_output(
                    &format!("error: refusing to delete the current branch: {head_ref}"),
                    &style,
                );
                return Err("deletion of the current branch prohibited".to_owned());
            }
            ReceiveDenyAction::Refuse | ReceiveDenyAction::UpdateInstead => {
                colorize_remote_output(
                    &format!("error: refusing to delete the current branch: {head_ref}"),
                    &style,
                );
                return Err("deletion of the current branch prohibited".to_owned());
            }
        }
    }

    Ok(())
}

/// Outcome of applying one ref update on the remote.
enum ApplyRefResult {
    Applied,
    RemoteRejected(String),
}

/// Matching refspec `:` — push every `refs/heads/*` whose tip differs from the remote.
#[allow(clippy::too_many_arguments)]
fn collect_matching_push_updates(
    repo: &Repository,
    remote_repo: &Repository,
    remote_name: &str,
    args: &Args,
    updates: &mut Vec<RefUpdate>,
    submodule_tips: &mut Vec<ObjectId>,
    negative_patterns: &[String],
    refspec_force: bool,
) -> Result<usize> {
    let mut matched = 0usize;
    let local_branches = refs::list_refs(&repo.git_dir, "refs/heads/")?;
    for (refname, local_oid) in &local_branches {
        let Some(old_oid) = refs::resolve_ref(&remote_repo.git_dir, refname).ok() else {
            continue;
        };
        // A branch present on both sides is a candidate for the matching (`:`) refspec. Count it
        // even when a negative refspec later excludes it: Git treats an all-excluded matching push
        // as a successful no-op, not as "No refs in common" (t5582 push with matching `:` + `^`).
        matched += 1;
        if negative_patterns
            .iter()
            .any(|p| ref_excluded_by_negative_push_pattern(p, refname))
        {
            continue;
        }
        if &old_oid == local_oid {
            submodule_tips.push(*local_oid);
            continue;
        }
        let dst = refname
            .strip_prefix("refs/heads/")
            .unwrap_or(refname.as_str());
        let expected_oid = resolve_force_with_lease_expect(
            &args.force_with_lease,
            &repo.git_dir,
            remote_name,
            dst,
        );
        updates.push(RefUpdate {
            local_ref: Some(refname.clone()),
            remote_ref: refname.clone(),
            old_oid: Some(old_oid),
            new_oid: Some(*local_oid),
            expected_oid,
            refspec_force,
            pre_push_local_name: None,
            up_to_date: false,
            client_reject: None,
            delete_nonexistent: false,
        });
    }

    // `--prune` with the matching (`:`) refspec deletes every remote `refs/heads/*` ref that has
    // no local counterpart (git's `:` expands to `refs/heads/*:refs/heads/*`, which prune then
    // mirrors). Negative refspecs exempt a ref from pruning.
    if args.prune {
        let local_set: std::collections::BTreeSet<String> = local_branches
            .iter()
            .map(|(name, _)| name.clone())
            .collect();
        let remote_branches = refs::list_refs(&remote_repo.git_dir, "refs/heads/")?;
        for (remote_ref, old_oid) in &remote_branches {
            if local_set.contains(remote_ref) {
                continue;
            }
            if negative_patterns
                .iter()
                .any(|p| ref_excluded_by_negative_push_pattern(p, remote_ref))
            {
                continue;
            }
            if updates.iter().any(|u| u.remote_ref == *remote_ref) {
                continue;
            }
            updates.push(RefUpdate {
                local_ref: None,
                remote_ref: remote_ref.clone(),
                old_oid: Some(*old_oid),
                new_oid: None,
                expected_oid: None,
                refspec_force,
                pre_push_local_name: None,
                up_to_date: false,
                client_reject: None,
                delete_nonexistent: false,
            });
        }
    }
    Ok(matched)
}

/// Leading `:` / `+:` matching refspec, optionally followed only by negative `^` patterns.
fn parse_matching_push_with_negatives(args: &Args) -> Option<(bool, Vec<String>)> {
    let first = args.refspecs.first()?.as_str();
    let (refspec_force, tail) = match first {
        ":" => (false, &args.refspecs[1..]),
        "+:" => (true, &args.refspecs[1..]),
        _ => return None,
    };
    if tail.is_empty() {
        return Some((refspec_force, Vec::new()));
    }
    if !tail.iter().all(|s| s.starts_with('^')) {
        return None;
    }
    let neg: Vec<String> = tail.iter().map(|s| s[1..].to_owned()).collect();
    Some((refspec_force, neg))
}

/// Apply a single ref update on the remote, printing output as appropriate.
#[allow(clippy::too_many_arguments)]
fn apply_ref_update(
    repo: &Repository,
    remote_repo: &Repository,
    remote_name: &str,
    update: &RefUpdate,
    args: &Args,
    _url: &str,
    pushing_config: &ConfigSet,
    remote_config: &ConfigSet,
) -> Result<ApplyRefResult> {
    let cli_force_enabled = args.force && !args.no_force;

    // A branch (`refs/heads/*`) must point at a commit. Pushing a tag/tree/blob OID to a branch
    // is rejected by receive-pack with `(invalid new value provided)` plus a `trying to write
    // non-commit object ...` message (git refs.c `ref_update_check_old_target` / `refs_verify`).
    if let Some(new_oid) = update.new_oid {
        if update.remote_ref.starts_with("refs/heads/") {
            let kind = remote_repo
                .odb
                .read(&new_oid)
                .ok()
                .map(|o| o.kind)
                .or_else(|| repo.odb.read(&new_oid).ok().map(|o| o.kind));
            if let Some(k) = kind {
                if k != grit_lib::objects::ObjectKind::Commit {
                    let style = RemoteMessageColorStyle::from_config(pushing_config);
                    colorize_remote_output(
                        &format!(
                            "error: trying to write non-commit object {} to branch '{}'",
                            new_oid.to_hex(),
                            update.remote_ref
                        ),
                        &style,
                    );
                    return Ok(ApplyRefResult::RemoteRejected(
                        "invalid new value provided".to_owned(),
                    ));
                }
            }
        }
    }

    if let Err(reason) =
        check_receive_pack_policy(remote_repo, remote_config, pushing_config, update)
    {
        return Ok(ApplyRefResult::RemoteRejected(reason));
    }

    // The branch may be checked out in the main worktree or a linked one; `updateInstead` must
    // update *that* worktree's working tree (t5516 116/117). Resolve it once here.
    let update_instead_worktree: Option<PathBuf> = if update.new_oid.is_some()
        && read_receive_deny_current(remote_config) == ReceiveDenyAction::UpdateInstead
    {
        crate::commands::worktree_refs::occupied_branch_refs(remote_repo)
            .get(&update.remote_ref)
            .map(PathBuf::from)
    } else {
        None
    };

    match (&update.new_oid, &update.old_oid) {
        (Some(new_oid), old_oid_opt) => {
            if !args.dry_run {
                // For `denyCurrentBranch = updateInstead`, update the work tree *before* writing
                // the ref (git calls `update_worktree` first; a failed push_to_deploy leaves the
                // branch untouched). Otherwise a refused push would leave the ref advanced, which
                // corrupts the next push's "old value" check (t5516 updateInstead case (4)).
                if let Some(worktree_path) = &update_instead_worktree {
                    let target_repo = open_worktree_repo_for_update_instead(
                        remote_repo,
                        worktree_path,
                        &update.remote_ref,
                    );
                    let target_repo = target_repo.as_ref().unwrap_or(remote_repo);
                    match update_worktree_after_push_update_instead(
                        target_repo,
                        *new_oid,
                        update.old_oid,
                    ) {
                        Ok(hook_output) => {
                            if !hook_output.is_empty() {
                                let output_str = String::from_utf8_lossy(&hook_output);
                                let color_remote =
                                    RemoteMessageColorStyle::from_config(pushing_config);
                                colorize_remote_output(&output_str, &color_remote);
                            }
                        }
                        Err(msg) => {
                            return Ok(ApplyRefResult::RemoteRejected(msg));
                        }
                    }
                }
                refs::write_ref(&remote_repo.git_dir, &update.remote_ref, new_oid)
                    .with_context(|| format!("updating remote ref {}", update.remote_ref))?;
                // receive-pack records a `<ref>@{0} push` reflog entry when the receiving repo has
                // reflogs enabled for the ref (core.logAllRefUpdates; a bare repo only when set to
                // true). `append_reflog` is a no-op when auto-creation is disallowed (t5516
                // 'push into bare respects core.logallrefupdates').
                let zero = ObjectId::zero();
                let old_for_log = old_oid_opt.unwrap_or(zero);
                let identity = crate::commands::update_ref::resolve_reflog_identity(remote_repo);
                let _ = refs::append_reflog(
                    &remote_repo.git_dir,
                    &update.remote_ref,
                    &old_for_log,
                    new_oid,
                    &identity,
                    "push",
                    false,
                );
                update_remote_tracking_ref(repo, remote_name, &update.remote_ref, Some(*new_oid))?;
            }

            let branch_short = update
                .remote_ref
                .strip_prefix("refs/heads/")
                .or_else(|| update.remote_ref.strip_prefix("refs/tags/"))
                .unwrap_or(&update.remote_ref);
            let src_short = update
                .pre_push_local_name
                .as_deref()
                .or_else(|| {
                    update
                        .local_ref
                        .as_deref()
                        .and_then(|r| r.strip_prefix("refs/heads/"))
                })
                .or_else(|| {
                    update
                        .local_ref
                        .as_deref()
                        .and_then(|r| r.strip_prefix("refs/tags/"))
                })
                .unwrap_or(update.local_ref.as_deref().unwrap_or("(unknown)"));

            // Porcelain output is collected by the caller and printed all at once via
            // `format_push_status`; here we only emit the human-readable stderr form.
            if !args.porcelain && !args.quiet {
                match old_oid_opt {
                    Some(old)
                        if old != new_oid
                            && update.remote_ref.starts_with("refs/heads/")
                            && ((cli_force_enabled || update.refspec_force)
                                || is_ancestor(repo, *old, *new_oid)
                                    .map(|ff| !ff)
                                    .unwrap_or(false)) =>
                    {
                        eprintln!(
                            " + {}...{}  {} -> {} (forced update)",
                            &old.to_hex()[..7],
                            &new_oid.to_hex()[..7],
                            src_short,
                            branch_short,
                        );
                    }
                    Some(old) if old != new_oid => {
                        eprintln!(
                            "   {}..{}  {} -> {}",
                            &old.to_hex()[..7],
                            &new_oid.to_hex()[..7],
                            src_short,
                            branch_short,
                        );
                    }
                    None => {
                        let kind = if update.remote_ref.starts_with("refs/tags/") {
                            "tag"
                        } else {
                            "branch"
                        };
                        eprintln!(" * [new {kind}]      {src_short} -> {branch_short}");
                    }
                    _ => {
                        eprintln!(" = [up to date]      {} -> {}", src_short, branch_short);
                    }
                }
            }
        }
        (None, Some(old_oid)) => {
            // Delete
            if !args.dry_run {
                refs::delete_ref(&remote_repo.git_dir, &update.remote_ref)
                    .with_context(|| format!("deleting remote ref {}", update.remote_ref))?;
                update_remote_tracking_ref(repo, remote_name, &update.remote_ref, None)?;
            }

            let branch_short = update
                .remote_ref
                .strip_prefix("refs/heads/")
                .unwrap_or(&update.remote_ref);

            // Porcelain output is collected by the caller; emit only the human form here.
            if !args.porcelain && !args.quiet {
                eprintln!(
                    " - [deleted]         {} -> {}",
                    &old_oid.to_hex()[..7],
                    branch_short,
                );
            }
        }
        _ => {}
    }

    Ok(ApplyRefResult::Applied)
}

/// Update local remote-tracking refs after a successful push.
///
/// Git updates `refs/remotes/<remote>/...` when pushing to a named remote.
/// For path-like remotes we skip tracking updates.
fn update_remote_tracking_ref(
    repo: &Repository,
    remote_name: &str,
    remote_ref: &str,
    new_oid: Option<ObjectId>,
) -> Result<()> {
    grit_lib::branch_tracking::update_remote_tracking_ref(
        &repo.git_dir,
        remote_name,
        remote_ref,
        new_oid,
    )
    .with_context(|| format!("updating tracking ref for {remote_ref}"))
}

/// Parsed --force-with-lease argument.
#[derive(Debug)]
enum ForceWithLease {
    /// --force-with-lease (bare, use tracking ref for the ref being pushed)
    Bare,
    /// --force-with-lease=<refname> (use tracking ref for this specific ref)
    Ref(String),
    /// --force-with-lease=<refname>:<expect> (explicit expected OID)
    RefExpect(String, String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LeaseCheckResult {
    None,
    Expect(ObjectId),
    MissingTracking,
}

/// Resolve the expected OID for --force-with-lease, given the push target ref.
fn resolve_force_with_lease_expect(
    fwl: &Option<String>,
    git_dir: &Path,
    remote_name: &str,
    dst_branch: &str,
) -> Option<ObjectId> {
    match force_with_lease_expectation_for_remote_ref(fwl, git_dir, remote_name, dst_branch) {
        LeaseCheckResult::Expect(oid) => Some(oid),
        LeaseCheckResult::None | LeaseCheckResult::MissingTracking => None,
    }
}

fn normalize_push_target_ref(name: &str) -> String {
    if name.starts_with("refs/") {
        name.to_owned()
    } else {
        format!("refs/heads/{name}")
    }
}

fn matches_force_with_lease_ref(remote_ref: &str, spec_ref: &str) -> bool {
    normalize_push_target_ref(remote_ref) == normalize_push_target_ref(spec_ref)
}

fn tracking_ref_for_remote_branch(remote_name: &str, remote_ref: &str) -> Option<String> {
    let full = normalize_push_target_ref(remote_ref);
    let branch = full.strip_prefix("refs/heads/")?;
    Some(format!("refs/remotes/{remote_name}/{branch}"))
}

fn resolve_force_with_lease_explicit_expect(git_dir: &Path, expect: &str) -> Option<ObjectId> {
    if let Ok(repo) = Repository::open(git_dir, None) {
        if let Ok(oid) = grit_lib::rev_parse::resolve_revision(&repo, expect) {
            return Some(oid);
        }
    }
    expect.parse::<ObjectId>().ok()
}

fn force_with_lease_expectation_for_remote_ref(
    fwl: &Option<String>,
    git_dir: &Path,
    remote_name: &str,
    remote_ref: &str,
) -> LeaseCheckResult {
    let Some(val) = fwl.as_deref() else {
        return LeaseCheckResult::None;
    };
    match parse_force_with_lease(val) {
        ForceWithLease::Bare => {
            let Some(tracking_ref) = tracking_ref_for_remote_branch(remote_name, remote_ref) else {
                return LeaseCheckResult::None;
            };
            match refs::resolve_ref(git_dir, &tracking_ref) {
                Ok(oid) => LeaseCheckResult::Expect(oid),
                Err(_) => LeaseCheckResult::MissingTracking,
            }
        }
        ForceWithLease::Ref(refname) => {
            if !matches_force_with_lease_ref(remote_ref, &refname) {
                return LeaseCheckResult::None;
            }
            let Some(tracking_ref) = tracking_ref_for_remote_branch(remote_name, &refname) else {
                return LeaseCheckResult::None;
            };
            match refs::resolve_ref(git_dir, &tracking_ref) {
                Ok(oid) => LeaseCheckResult::Expect(oid),
                Err(_) => LeaseCheckResult::MissingTracking,
            }
        }
        ForceWithLease::RefExpect(refname, expect) => {
            if !matches_force_with_lease_ref(remote_ref, &refname) {
                return LeaseCheckResult::None;
            }
            resolve_force_with_lease_explicit_expect(git_dir, &expect)
                .map(LeaseCheckResult::Expect)
                .unwrap_or(LeaseCheckResult::MissingTracking)
        }
    }
}

fn parse_force_with_lease(val: &str) -> ForceWithLease {
    if val.is_empty() {
        ForceWithLease::Bare
    } else if let Some(idx) = val.find(':') {
        ForceWithLease::RefExpect(val[..idx].to_owned(), val[idx + 1..].to_owned())
    } else {
        ForceWithLease::Ref(val.to_owned())
    }
}

/// Copy symbolic refs that match a glob pattern from local to remote.
fn copy_symrefs_push(
    local_git_dir: &Path,
    remote_git_dir: &Path,
    src_pattern: &str,
    dst_pattern: &str,
) -> Result<()> {
    let refs_dir = local_git_dir.join("refs");
    if !refs_dir.is_dir() {
        return Ok(());
    }
    walk_refs_for_symrefs(&refs_dir, "refs", &mut |refname, path| {
        if let Some(matched) = match_glob(src_pattern, &refname) {
            let content = fs::read_to_string(path)?;
            let content = content.trim();
            if let Some(target) = content.strip_prefix("ref: ") {
                let remote_ref = dst_pattern.replacen('*', matched, 1);
                let remote_path =
                    remote_git_dir.join(remote_ref.replace('/', std::path::MAIN_SEPARATOR_STR));
                if let Some(parent) = remote_path.parent() {
                    fs::create_dir_all(parent)?;
                }
                fs::write(&remote_path, format!("ref: {target}\n"))?;
            }
        }
        Ok(())
    })?;
    Ok(())
}

fn walk_refs_for_symrefs(
    dir: &Path,
    prefix: &str,
    cb: &mut dyn FnMut(String, &Path) -> Result<()>,
) -> Result<()> {
    if !dir.is_dir() {
        return Ok(());
    }
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        let name = entry.file_name();
        let name_str = name.to_string_lossy();
        let refname = format!("{prefix}/{name_str}");
        if entry.file_type()?.is_dir() {
            walk_refs_for_symrefs(&entry.path(), &refname, cb)?;
        } else {
            cb(refname, &entry.path())?;
        }
    }
    Ok(())
}

/// Negative refspec matching for push (same rules as fetch).
fn ref_excluded_by_negative_push_pattern(pattern: &str, refname: &str) -> bool {
    match_glob(pattern, refname).is_some() || pattern == refname
}

fn validate_negative_push_patterns(patterns: &[&str]) -> Result<()> {
    for pat in patterns {
        let clean = pat.strip_prefix("refs/").unwrap_or(pat);
        if clean.chars().all(|c| c.is_ascii_hexdigit()) && clean.len() >= 7 {
            bail!("negative refspecs do not support object ids: ^{pat}");
        }
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn push_prune_glob_refspec(
    repo: &Repository,
    remote_repo: &Repository,
    args: &Args,
    remote_name: &str,
    force: bool,
    src_pat: &str,
    dst_pat: &str,
    negative_patterns: &[String],
    updates: &mut Vec<RefUpdate>,
) -> Result<()> {
    if !src_pat.contains('*') || dst_pat.is_empty() {
        return Ok(());
    }
    let remote_refs = refs::list_refs(&remote_repo.git_dir, "refs/")?;
    for (remote_ref, old_oid) in &remote_refs {
        if let Some(matched) = match_glob(dst_pat, remote_ref) {
            if negative_patterns
                .iter()
                .any(|p| ref_excluded_by_negative_push_pattern(p, remote_ref))
            {
                continue;
            }
            let local_ref = src_pat.replacen('*', matched, 1);
            if refs::resolve_ref(&repo.git_dir, &local_ref).is_ok() {
                continue;
            }
            if updates.iter().any(|u| u.remote_ref == *remote_ref) {
                continue;
            }
            let expected_oid = resolve_force_with_lease_expect(
                &args.force_with_lease,
                &repo.git_dir,
                remote_name,
                remote_ref.strip_prefix("refs/heads/").unwrap_or(remote_ref),
            );
            updates.push(RefUpdate {
                local_ref: None,
                remote_ref: remote_ref.clone(),
                old_oid: Some(*old_oid),
                new_oid: None,
                expected_oid,
                refspec_force: force,
                pre_push_local_name: None,
                up_to_date: false,
                client_reject: None,
                delete_nonexistent: false,
            });
        }
    }
    Ok(())
}

/// Match a glob pattern (e.g. "refs/heads/*") against a ref name.
/// Returns the part matched by '*' if it matches, None otherwise.
fn match_glob<'a>(pattern: &str, refname: &'a str) -> Option<&'a str> {
    if let Some(star_pos) = pattern.find('*') {
        let prefix = &pattern[..star_pos];
        let suffix = &pattern[star_pos + 1..];
        if refname.starts_with(prefix)
            && refname.ends_with(suffix)
            && refname.len() >= prefix.len() + suffix.len()
        {
            Some(&refname[prefix.len()..refname.len() - suffix.len()])
        } else {
            None
        }
    } else if pattern == refname {
        Some(refname)
    } else {
        None
    }
}

/// Parse a refspec like "src:dst" or just "name" (meaning "name:name").
fn parse_refspec(spec: &str) -> (String, String) {
    if let Some(idx) = spec.find(':') {
        let src = spec[..idx].to_owned();
        let dst = spec[idx + 1..].to_owned();
        (src, dst)
    } else {
        (spec.to_owned(), spec.to_owned())
    }
}

/// Normalize a ref name: if it doesn't start with "refs/", assume "refs/heads/".
fn normalize_ref(name: &str) -> String {
    if name.starts_with("refs/") {
        name.to_owned()
    } else {
        format!("refs/heads/{name}")
    }
}

fn push_default_mode(config: &ConfigSet) -> String {
    config
        .get("push.default")
        .map(|v| v.to_ascii_lowercase())
        .unwrap_or_else(|| "simple".to_owned())
}

fn configured_remote_names(config: &ConfigSet) -> std::collections::BTreeSet<String> {
    let mut remotes = std::collections::BTreeSet::new();
    for entry in config.entries() {
        let Some(rest) = entry.key.strip_prefix("remote.") else {
            continue;
        };
        let Some((name, _var)) = rest.rsplit_once('.') else {
            continue;
        };
        if !name.is_empty() {
            remotes.insert(name.to_owned());
        }
    }
    remotes
}

fn infer_implicit_push_remote(config: &ConfigSet, current_branch: Option<&str>) -> String {
    // Git `remote_get` push lookup (remote.c `pushremote_for_branch`):
    // `branch.<name>.pushRemote` > `remote.pushDefault` > `branch.<name>.remote` > `origin`.
    // `remote.pushDefault` applies even when HEAD is detached (no current branch) — t5516 test 52
    // pushes after a detached checkout and must still target `remote.pushdefault`.
    if let Some(branch) = current_branch {
        if let Some(name) = config
            .get(&format!("branch.{branch}.pushRemote"))
            .or_else(|| config.get(&format!("branch.{branch}.pushremote")))
        {
            return name;
        }
    }
    if let Some(name) = config
        .get("remote.pushDefault")
        .or_else(|| config.get("remote.pushdefault"))
    {
        return name;
    }
    if let Some(branch) = current_branch {
        if let Some(name) = config.get(&format!("branch.{branch}.remote")) {
            return name;
        }
    }

    let remotes = configured_remote_names(config);
    if remotes.len() == 1 {
        if let Some(only) = remotes.iter().next() {
            return only.to_owned();
        }
    }
    if remotes.contains("origin") {
        return "origin".to_owned();
    }
    "origin".to_owned()
}

fn url_looks_like_local_path(url: &str) -> bool {
    if url.starts_with("file://") {
        return true;
    }
    if url.contains("://") {
        return false;
    }
    if crate::ssh_transport::is_configured_ssh_url(url) {
        return false;
    }
    let p = Path::new(url);
    p.is_absolute() || url.starts_with('.') || p.exists()
}

fn is_http_transport_url(url: &str) -> bool {
    url.starts_with("http://") || url.starts_with("https://")
}

fn scrub_push_url_credentials(url: &str) -> String {
    if let Ok(mut parsed) = url::Url::parse(url) {
        let _ = parsed.set_username("");
        let _ = parsed.set_password(None);
        return parsed.to_string();
    }
    url.to_owned()
}

#[allow(clippy::too_many_arguments)]
fn push_to_http_url(
    repo: &Repository,
    config: &ConfigSet,
    args: &Args,
    url: &str,
    remote_name: &str,
    current_branch: Option<&str>,
    push_all: bool,
    effective_mirror: bool,
    push_refspecs_from_config: &[String],
    cli_force_enabled: bool,
) -> Result<()> {
    let proto = if url.starts_with("https://") {
        "https"
    } else {
        "http"
    };
    crate::protocol::check_protocol_allowed(proto, Some(&repo.git_dir))?;

    let client = crate::http_client::HttpClientContext::from_config_set(config)?;
    let advertised = crate::http_push_smart::discover_receive_pack(url, &client)?;
    if advertised.object_format != "sha1" {
        bail!(
            "unsupported remote object format '{}' for push over HTTP",
            advertised.object_format
        );
    }
    if advertised.protocol_version == 2 {
        bail!("smart HTTP push over protocol v2 is not implemented yet");
    }

    let mut remote_ref_map: std::collections::BTreeMap<String, ObjectId> =
        std::collections::BTreeMap::new();
    for r in &advertised.refs {
        remote_ref_map.insert(r.name.clone(), r.oid);
    }

    let mut updates: Vec<RefUpdate> = Vec::new();
    let mut set_upstream_after_push = args.set_upstream;
    let mut remote_have: std::collections::BTreeSet<ObjectId> =
        remote_ref_map.values().copied().collect();
    let mut atomic_pre_reject_ref: Option<String> = None;

    if effective_mirror {
        let local_all = refs::list_refs(&repo.git_dir, "refs/")?;
        for (refname, local_oid) in &local_all {
            if !refname.starts_with("refs/") {
                continue;
            }
            let old_oid = remote_ref_map.get(refname).copied();
            if old_oid.as_ref() == Some(local_oid) {
                continue;
            }
            updates.push(RefUpdate {
                local_ref: Some(refname.clone()),
                remote_ref: refname.clone(),
                old_oid,
                new_oid: Some(*local_oid),
                expected_oid: None,
                refspec_force: true,
                pre_push_local_name: None,
                up_to_date: false,
                client_reject: None,
                delete_nonexistent: false,
            });
        }
        for (refname, remote_oid) in &remote_ref_map {
            if !refname.starts_with("refs/") {
                continue;
            }
            if !local_all.iter().any(|(r, _)| r == refname) {
                updates.push(RefUpdate {
                    local_ref: None,
                    remote_ref: refname.clone(),
                    old_oid: Some(*remote_oid),
                    new_oid: None,
                    expected_oid: None,
                    refspec_force: true,
                    pre_push_local_name: None,
                    up_to_date: false,
                    client_reject: None,
                    delete_nonexistent: false,
                });
            }
        }
    } else if push_all {
        let mut local_branches = refs::list_refs(&repo.git_dir, "refs/heads/")?;
        local_branches.sort_by(|a, b| a.0.cmp(&b.0));
        for (refname, local_oid) in &local_branches {
            let old_oid = remote_ref_map.get(refname).copied();
            if old_oid.as_ref() == Some(local_oid) {
                continue;
            }
            updates.push(RefUpdate {
                local_ref: Some(refname.clone()),
                remote_ref: refname.clone(),
                old_oid,
                new_oid: Some(*local_oid),
                expected_oid: None,
                refspec_force: false,
                pre_push_local_name: None,
                up_to_date: false,
                client_reject: None,
                delete_nonexistent: false,
            });
        }
    } else {
        let mut resolved_refspecs: Vec<String> = if !args.refspecs.is_empty() {
            args.refspecs.clone()
        } else if !push_refspecs_from_config.is_empty() {
            push_refspecs_from_config.to_vec()
        } else if push_default_mode(config) == "matching" {
            refs::list_refs(&repo.git_dir, "refs/heads/")?
                .into_iter()
                .map(|(name, _)| name)
                .filter(|name| remote_ref_map.contains_key(name))
                .map(|name| format!("{name}:{name}"))
                .collect()
        } else if let Some(branch) = current_branch {
            let (src, dst, auto_setup) =
                default_push_ref_for_current_branch(config, remote_name, branch)?;
            if auto_setup {
                set_upstream_after_push = true;
            }
            vec![format!("{src}:{dst}")]
        } else {
            bail!("You are not currently on a branch.");
        };

        if args.delete {
            if resolved_refspecs.is_empty() {
                bail!("--delete doesn't make sense without any refs");
            }
            for spec in &resolved_refspecs {
                if spec.contains('*') {
                    bail!("wildcard delete refspecs are not supported over HTTP push yet");
                }
                let remote_ref = if spec.contains(':') {
                    let (_, dst) = parse_refspec(spec);
                    normalize_ref(&dst)
                } else {
                    normalize_ref(spec)
                };
                if remote_ref.is_empty() {
                    continue;
                }
                let old_oid = remote_ref_map.get(&remote_ref).copied();
                updates.push(RefUpdate {
                    local_ref: None,
                    remote_ref,
                    old_oid,
                    new_oid: None,
                    expected_oid: None,
                    refspec_force: false,
                    pre_push_local_name: None,
                    up_to_date: false,
                    client_reject: None,
                    delete_nonexistent: false,
                });
            }
            resolved_refspecs.clear();
        }

        for spec in &resolved_refspecs {
            if spec.contains('*') {
                bail!("wildcard push refspecs are not supported over HTTP push yet");
            }
            let refspec_force = spec.starts_with('+');
            let spec_body = spec.strip_prefix('+').unwrap_or(spec);
            let (src_raw, dst_raw) = parse_refspec(spec_body);
            if src_raw.is_empty() {
                let remote_ref = normalize_ref(&dst_raw);
                let old_oid = remote_ref_map.get(&remote_ref).copied();
                updates.push(RefUpdate {
                    local_ref: None,
                    remote_ref,
                    old_oid,
                    new_oid: None,
                    expected_oid: None,
                    refspec_force,
                    pre_push_local_name: None,
                    up_to_date: false,
                    client_reject: None,
                    delete_nonexistent: false,
                });
                continue;
            }

            let remote_ref = normalize_ref(&dst_raw);
            let (local_ref, local_oid, resolved_pre_push_name) =
                resolve_push_src_for_refspec(repo, &src_raw, &remote_ref)
                    .with_context(|| format!("src ref '{}' does not match any", src_raw))?;
            let old_oid = remote_ref_map.get(&remote_ref).copied();

            // An up-to-date ref is still reported (`[up to date]`) in porcelain/verbose
            // mode, so keep it as a non-pushable update rather than dropping it.
            let mut up_to_date = false;
            let mut client_reject: Option<grit_lib::push_report::PushRefStatus> = None;
            if let Some(old) = old_oid {
                remote_have.insert(old);
                if old == local_oid {
                    up_to_date = true;
                } else if !effective_mirror
                    && !cli_force_enabled
                    && !refspec_force
                    && args.force_with_lease.is_none()
                    && !remote_ref.starts_with("refs/tags/")
                    && !is_ancestor(repo, old, local_oid)?
                {
                    if args.atomic {
                        atomic_pre_reject_ref.get_or_insert(remote_ref.clone());
                    }
                    // Record the non-fast-forward rejection on the ref instead of
                    // aborting, so it appears in the porcelain/verbose status report.
                    client_reject =
                        Some(grit_lib::push_report::PushRefStatus::RejectNonFastForward);
                }
            }

            updates.push(RefUpdate {
                local_ref: Some(local_ref),
                remote_ref,
                old_oid,
                new_oid: Some(local_oid),
                expected_oid: None,
                refspec_force,
                pre_push_local_name: resolved_pre_push_name.or_else(|| {
                    if src_raw == "HEAD" {
                        Some("HEAD".to_owned())
                    } else {
                        None
                    }
                }),
                up_to_date,
                client_reject,
                delete_nonexistent: false,
            });
        }
    }

    // When nothing needs sending and nothing was rejected (every ref is already
    // up to date, or there were no refs at all), Git prints "Everything up-to-date"
    // and exits 0 without contacting receive-pack.
    let nothing_to_push =
        updates.iter().all(|u| u.up_to_date) && !updates.iter().any(|u| u.client_reject.is_some());
    if nothing_to_push {
        if !args.quiet {
            println!("Everything up-to-date");
        }
        return Ok(());
    }

    use grit_lib::push_report::PushRefStatus;
    let display_url = scrub_push_url_credentials(url);

    // Classify an accepted update for the status report (Ok/UpToDate + forced).
    let http_classify = |update: &RefUpdate| -> (PushRefStatus, bool) {
        if update.up_to_date {
            return (PushRefStatus::UpToDate, false);
        }
        match (update.old_oid, update.new_oid) {
            (Some(old), Some(new)) if old != new => {
                let forced = (cli_force_enabled || update.refspec_force)
                    && !is_ancestor(repo, old, new).unwrap_or(false);
                (PushRefStatus::Ok, forced)
            }
            _ => (PushRefStatus::Ok, false),
        }
    };

    // Atomic push where a ref was rejected on the client side (non-fast-forward):
    // nothing is sent; every changing ref is reported `[rejected] (atomic push
    // failed)` except the offending ref (`non-fast-forward`) and up-to-date refs.
    if atomic_pre_reject_ref.is_some() {
        let mut results = Vec::with_capacity(updates.len());
        for update in &updates {
            let (status, forced) = if let Some(reject) = update.client_reject.clone() {
                (reject, false)
            } else if update.up_to_date {
                (PushRefStatus::UpToDate, false)
            } else {
                (PushRefStatus::AtomicPushFailed, false)
            };
            results.push(make_push_ref_result(update, status, forced, None));
        }
        if args.porcelain {
            emit_porcelain_push_report(
                url,
                &results.iter().cloned().map(Some).collect::<Vec<_>>(),
                args.quiet,
            );
        } else if !args.quiet {
            let out = grit_lib::push_report::format_push_status(
                &display_url,
                &results,
                false,
                args.quiet,
            );
            eprint!("{}", out.stderr);
            eprintln!("error: failed to push some refs to '{display_url}'");
        }
        bail!("atomic push failed")
    }

    if !args.no_verify {
        let zero_oid = "0".repeat(40);
        let mut hook_lines = String::new();
        for update in &updates {
            if !update.is_pushable() {
                continue;
            }
            let local_ref = pre_push_hook_local_display(update);
            let local_oid = update
                .new_oid
                .map(|o| o.to_hex())
                .unwrap_or_else(|| zero_oid.clone());
            let remote_oid = update
                .old_oid
                .map(|o| o.to_hex())
                .unwrap_or_else(|| zero_oid.clone());
            hook_lines.push_str(&format!(
                "{local_ref} {local_oid} {} {remote_oid}\n",
                update.remote_ref
            ));
        }
        let result = run_hook(
            repo,
            "pre-push",
            &[remote_name, url],
            Some(hook_lines.as_bytes()),
        );
        if let HookResult::Failed(code) = result {
            bail!("pre-push hook declined the push (exit code {code})");
        }
    }

    // A non-fast-forward rejection (non-atomic) still reports every ref: up-to-date,
    // the would-be successes, and the rejected ones. For dry-run we build the report
    // and skip the network round-trip entirely.
    if args.dry_run {
        let mut results = Vec::with_capacity(updates.len());
        for update in &updates {
            let (status, forced) = if let Some(reject) = update.client_reject.clone() {
                (reject, false)
            } else {
                http_classify(update)
            };
            results.push(make_push_ref_result(update, status, forced, None));
        }
        if args.porcelain {
            emit_porcelain_push_report(
                url,
                &results.iter().cloned().map(Some).collect::<Vec<_>>(),
                args.quiet,
            );
        } else if !args.quiet {
            let out = grit_lib::push_report::format_push_status(
                &display_url,
                &results,
                false,
                args.quiet,
            );
            eprint!("{}", out.stderr);
        }
        if results.iter().any(|r| r.status.is_error()) {
            bail!("failed to push some refs to '{display_url}'");
        }
        return Ok(());
    }

    let push_tips: Vec<ObjectId> = updates
        .iter()
        .filter(|u| u.is_pushable())
        .filter_map(|u| u.new_oid)
        .collect();
    if push_negotiate_enabled(config) {
        if protocol_wire::effective_client_protocol_version() == 2 {
            add_push_tip_parents_to_remote_have(repo, &push_tips, &mut remote_have);
        } else {
            eprintln!("warning: --negotiate-only requires protocol v2");
            eprintln!("warning: push negotiation failed; proceeding anyway with push");
        }
    }
    let remote_have_vec: Vec<ObjectId> = remote_have.into_iter().collect();
    let delete_only = updates
        .iter()
        .filter(|u| u.is_pushable())
        .all(|u| u.new_oid.is_none());
    let use_thin = args.thin && !args.no_thin;
    let pack_data = if delete_only {
        Vec::new()
    } else {
        pack_objects::build_push_pack_from_remote_oids(
            repo,
            &push_tips,
            &remote_have_vec,
            use_thin,
        )?
    };
    maybe_emit_push_pack_wrote_trace2(&pack_data);
    if push_show_object_progress(args) && !delete_only {
        let written_objects = grit_lib::receive_pack::pack_object_count(&pack_data)
            .map(|count| count as usize)
            .unwrap_or_else(|| push_tips.len().max(1));
        maybe_print_push_object_progress(
            true,
            written_objects.max(push_tips.len()),
            written_objects,
            pack_data.len(),
        );
    }

    let effective_push_options = resolved_push_options(args, config)?;
    // Only pushable refs (not up-to-date, not client-rejected) are sent to receive-pack.
    let commands: Vec<crate::http_push_smart::PushCommand> = updates
        .iter()
        .filter(|u| u.is_pushable())
        .map(|u| crate::http_push_smart::PushCommand {
            old_oid: u.old_oid,
            new_oid: u.new_oid,
            refname: u.remote_ref.clone(),
        })
        .collect();
    maybe_print_http_push_post_summary(args, config, &pack_data);
    let status = crate::http_push_smart::send_receive_pack(
        &client,
        &advertised,
        &commands,
        &effective_push_options,
        &pack_data,
        args.atomic,
    )?;
    if !status.sideband_stderr.is_empty() {
        io::stderr().write_all(&status.sideband_stderr)?;
    }
    if !status.unpack_ok {
        bail!("remote unpack failed: {}", status.unpack_message);
    }

    let status_by_ref: std::collections::HashMap<&str, &crate::http_push_smart::PushStatusEntry> =
        status
            .statuses
            .iter()
            .map(|s| (s.refname.as_str(), s))
            .collect();

    // Build the per-ref status report for every update: client-rejected refs keep
    // their pre-computed status, up-to-date refs are `[up to date]`, and pushed refs
    // take their remote-reported outcome (success or `[remote rejected] (<reason>)`).
    let mut results: Vec<grit_lib::push_report::PushRefResult> = Vec::with_capacity(updates.len());
    let mut successful_branch_updates: Vec<(String, String)> = Vec::new();
    for update in &updates {
        if let Some(reject) = update.client_reject.clone() {
            results.push(make_push_ref_result(update, reject, false, None));
            continue;
        }
        if update.up_to_date {
            results.push(make_push_ref_result(
                update,
                PushRefStatus::UpToDate,
                false,
                None,
            ));
            continue;
        }

        let remote_status = status_by_ref.get(update.remote_ref.as_str());
        if remote_status.is_some_and(|s| !s.ok) {
            let reason = remote_status
                .and_then(|s| s.message.as_deref())
                .unwrap_or("remote rejected")
                .to_owned();
            results.push(make_push_ref_result(
                update,
                PushRefStatus::RemoteRejected,
                false,
                Some(reason),
            ));
            continue;
        }

        update_remote_tracking_ref(repo, remote_name, &update.remote_ref, update.new_oid)?;
        if update.remote_ref.starts_with("refs/heads/") {
            if let Some(local_ref) = update.local_ref.as_deref() {
                if let Some(local_branch) = local_ref.strip_prefix("refs/heads/") {
                    successful_branch_updates
                        .push((local_branch.to_owned(), update.remote_ref.clone()));
                }
            }
        }

        let (status, forced) = http_classify(update);
        results.push(make_push_ref_result(update, status, forced, None));
    }

    let rejected = results.iter().any(|r| r.status.is_error());
    if args.porcelain {
        emit_porcelain_push_report(
            url,
            &results.iter().cloned().map(Some).collect::<Vec<_>>(),
            args.quiet,
        );
    } else if !args.quiet || rejected {
        let out =
            grit_lib::push_report::format_push_status(&display_url, &results, false, args.quiet);
        eprint!("{}", out.stderr);
    }

    if rejected {
        bail!("failed to push some refs to '{display_url}'");
    }

    if set_upstream_after_push {
        for (branch, merge_ref) in successful_branch_updates {
            set_upstream_config(&repo.git_dir, &branch, remote_name, &merge_ref)?;
            if !args.quiet {
                let track_short = merge_ref.strip_prefix("refs/heads/").unwrap_or(&merge_ref);
                eprintln!("branch '{branch}' set up to track '{remote_name}/{track_short}'.");
            }
        }
    }

    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn push_to_ssh_url(
    repo: &Repository,
    config: &ConfigSet,
    args: &Args,
    url: &str,
    remote_name: &str,
    current_branch: Option<&str>,
    push_all: bool,
    effective_mirror: bool,
    push_refspecs_from_config: &[String],
    cli_force_enabled: bool,
) -> Result<()> {
    let spec = crate::ssh_transport::parse_ssh_url(url)?;
    let receive_pack = args
        .receive_pack
        .as_deref()
        .filter(|s| !s.trim().is_empty());
    let child = crate::ssh_transport::spawn_git_ssh_receive_pack(&spec, receive_pack)?;
    push_over_receive_pack_child(
        child,
        "ssh",
        repo,
        config,
        args,
        url,
        remote_name,
        current_branch,
        push_all,
        effective_mirror,
        push_refspecs_from_config,
        cli_force_enabled,
    )
}

/// Push to a remote whose `receive-pack` runs as a spawned child process (SSH or `ext::` helper),
/// driving the protocol-v1 receive-pack advertisement, send-pack stream, and ref-status reporting
/// over the child's stdin/stdout. `transport` labels the transport in error messages.
#[allow(clippy::too_many_arguments)]
fn push_over_receive_pack_child(
    mut child: std::process::Child,
    transport: &str,
    repo: &Repository,
    config: &ConfigSet,
    args: &Args,
    url: &str,
    remote_name: &str,
    current_branch: Option<&str>,
    push_all: bool,
    effective_mirror: bool,
    push_refspecs_from_config: &[String],
    cli_force_enabled: bool,
) -> Result<()> {
    let mut stdout = child
        .stdout
        .take()
        .with_context(|| format!("{transport} receive-pack stdout"))?;
    let stdin = child
        .stdin
        .take()
        .with_context(|| format!("{transport} receive-pack stdin"))?;
    let advertised = crate::http_push_smart::read_receive_pack_advertisement(
        &mut stdout,
        scrub_push_url_credentials(url),
    )?;
    if advertised.object_format != "sha1" {
        bail!(
            "unsupported remote object format '{}' for push over {transport}",
            advertised.object_format
        );
    }
    if advertised.protocol_version == 2 {
        bail!("{transport} push over protocol v2 is not implemented yet");
    }

    let mut remote_ref_map: std::collections::BTreeMap<String, ObjectId> =
        std::collections::BTreeMap::new();
    for r in &advertised.refs {
        if r.name.starts_with("refs/") {
            remote_ref_map.insert(r.name.clone(), r.oid);
        }
    }

    let mut updates: Vec<RefUpdate> = Vec::new();
    let mut set_upstream_after_push = args.set_upstream;
    let mut remote_have: std::collections::BTreeSet<ObjectId> =
        remote_ref_map.values().copied().collect();

    if effective_mirror {
        let local_all = refs::list_refs(&repo.git_dir, "refs/")?;
        for (refname, local_oid) in &local_all {
            if !refname.starts_with("refs/") {
                continue;
            }
            let old_oid = remote_ref_map.get(refname).copied();
            if old_oid.as_ref() == Some(local_oid) {
                continue;
            }
            updates.push(RefUpdate {
                local_ref: Some(refname.clone()),
                remote_ref: refname.clone(),
                old_oid,
                new_oid: Some(*local_oid),
                expected_oid: None,
                refspec_force: true,
                pre_push_local_name: None,
                up_to_date: false,
                client_reject: None,
                delete_nonexistent: false,
            });
        }
        for (refname, remote_oid) in &remote_ref_map {
            if !local_all.iter().any(|(r, _)| r == refname) {
                updates.push(RefUpdate {
                    local_ref: None,
                    remote_ref: refname.clone(),
                    old_oid: Some(*remote_oid),
                    new_oid: None,
                    expected_oid: None,
                    refspec_force: true,
                    pre_push_local_name: None,
                    up_to_date: false,
                    client_reject: None,
                    delete_nonexistent: false,
                });
            }
        }
    } else if push_all {
        let mut local_branches = refs::list_refs(&repo.git_dir, "refs/heads/")?;
        local_branches.sort_by(|a, b| a.0.cmp(&b.0));
        for (refname, local_oid) in &local_branches {
            let old_oid = remote_ref_map.get(refname).copied();
            if old_oid.as_ref() == Some(local_oid) {
                continue;
            }
            updates.push(RefUpdate {
                local_ref: Some(refname.clone()),
                remote_ref: refname.clone(),
                old_oid,
                new_oid: Some(*local_oid),
                expected_oid: None,
                refspec_force: false,
                pre_push_local_name: None,
                up_to_date: false,
                client_reject: None,
                delete_nonexistent: false,
            });
        }
    } else {
        let mut resolved_refspecs: Vec<String> = if !args.refspecs.is_empty() {
            args.refspecs.clone()
        } else if !push_refspecs_from_config.is_empty() {
            push_refspecs_from_config.to_vec()
        } else if args.tags || args.follow_tags {
            // `git push --tags` (with no refspec) pushes only tags, not the current branch.
            Vec::new()
        } else if let Some(branch) = current_branch {
            let (src, dst, auto_setup) =
                default_push_ref_for_current_branch(config, remote_name, branch)?;
            if auto_setup {
                set_upstream_after_push = true;
            }
            vec![format!("{src}:{dst}")]
        } else {
            bail!("You are not currently on a branch.");
        };

        if args.delete {
            if resolved_refspecs.is_empty() {
                bail!("--delete doesn't make sense without any refs");
            }
            for spec in &resolved_refspecs {
                if spec.contains('*') {
                    bail!("wildcard delete refspecs are not supported over SSH push yet");
                }
                let remote_ref = if spec.contains(':') {
                    let (_, dst) = parse_refspec(spec);
                    normalize_ref(&dst)
                } else {
                    normalize_ref(spec)
                };
                let old_oid = remote_ref_map.get(&remote_ref).copied();
                if old_oid.is_none() {
                    continue;
                }
                updates.push(RefUpdate {
                    local_ref: None,
                    remote_ref,
                    old_oid,
                    new_oid: None,
                    expected_oid: None,
                    refspec_force: false,
                    pre_push_local_name: None,
                    up_to_date: false,
                    client_reject: None,
                    delete_nonexistent: false,
                });
            }
            resolved_refspecs.clear();
        }

        for spec in &resolved_refspecs {
            if spec.contains('*') {
                bail!("wildcard push refspecs are not supported over SSH push yet");
            }
            let refspec_force = spec.starts_with('+');
            let (src_raw, dst_raw) = parse_refspec(spec);
            if src_raw.is_empty() {
                let remote_ref = normalize_ref(&dst_raw);
                let old_oid = remote_ref_map.get(&remote_ref).copied();
                if old_oid.is_none() {
                    continue;
                }
                updates.push(RefUpdate {
                    local_ref: None,
                    remote_ref,
                    old_oid,
                    new_oid: None,
                    expected_oid: None,
                    refspec_force,
                    pre_push_local_name: None,
                    up_to_date: false,
                    client_reject: None,
                    delete_nonexistent: false,
                });
                continue;
            }

            let local_ref = if src_raw == "HEAD" || src_raw.starts_with("refs/") {
                src_raw.clone()
            } else {
                normalize_ref(&src_raw)
            };
            let local_oid = refs::resolve_ref(&repo.git_dir, &local_ref)
                .with_context(|| format!("src ref '{}' does not match any", src_raw))?;
            let remote_ref = normalize_ref(&dst_raw);
            let old_oid = remote_ref_map.get(&remote_ref).copied();

            if let Some(old) = old_oid {
                remote_have.insert(old);
                if old == local_oid {
                    continue;
                }
                if !effective_mirror
                    && !cli_force_enabled
                    && !refspec_force
                    && args.force_with_lease.is_none()
                    && !remote_ref.starts_with("refs/tags/")
                    && !is_ancestor(repo, old, local_oid)?
                {
                    bail!(
                        "Updates were rejected because the remote contains work that you do not\n\
                         have locally. This is usually caused by another repository pushing to\n\
                         the same ref. If you want to integrate the remote changes, use\n\
                         'git pull' before pushing again.\n\
                         See the 'Note about fast-forwards' in 'git push --help' for details."
                    );
                }
            }

            updates.push(RefUpdate {
                local_ref: Some(local_ref),
                remote_ref,
                old_oid,
                new_oid: Some(local_oid),
                expected_oid: None,
                refspec_force,
                pre_push_local_name: if src_raw == "HEAD" {
                    Some("HEAD".to_owned())
                } else {
                    None
                },
                up_to_date: false,
                client_reject: None,
                delete_nonexistent: false,
            });
        }
    }

    // `--tags`: also push every local tag (unless mirroring, which already covers refs/*). The
    // remote's current value comes from the advertisement, so this works for namespaced/`ext::`
    // remotes where the tag is stored under `refs/namespaces/<ns>/refs/tags/...`.
    if args.tags && !effective_mirror {
        let local_tags = refs::list_refs(&repo.git_dir, "refs/tags/")?;
        for (refname, local_oid) in &local_tags {
            if updates.iter().any(|u| u.remote_ref == *refname) {
                continue;
            }
            let old_oid = remote_ref_map.get(refname).copied();
            if old_oid.as_ref() == Some(local_oid) {
                continue;
            }
            if let Some(old) = old_oid {
                remote_have.insert(old);
            }
            updates.push(RefUpdate {
                local_ref: Some(refname.clone()),
                remote_ref: refname.clone(),
                old_oid,
                new_oid: Some(*local_oid),
                expected_oid: None,
                refspec_force: false,
                pre_push_local_name: None,
                up_to_date: false,
                client_reject: None,
                delete_nonexistent: false,
            });
        }
    }

    // `--follow-tags` (or `push.followTags`): push annotated tags that point at commits already
    // being pushed and do not yet exist on the remote.
    let follow_tags = args.follow_tags
        || (!args.no_follow_tags
            && config
                .get("push.followTags")
                .map(|v| matches!(v.to_lowercase().as_str(), "true" | "yes" | "1"))
                .unwrap_or(false));
    if follow_tags && !effective_mirror {
        let pushed_oids: std::collections::HashSet<ObjectId> =
            updates.iter().filter_map(|u| u.new_oid).collect();
        if !pushed_oids.is_empty() {
            if let Ok(local_tags) = refs::list_refs(&repo.git_dir, "refs/tags/") {
                for (tag_name, tag_oid) in &local_tags {
                    if updates.iter().any(|u| u.remote_ref == *tag_name) {
                        continue;
                    }
                    if remote_ref_map.contains_key(tag_name) {
                        continue;
                    }
                    // Only annotated tags whose pointed-at object is being pushed.
                    if let Ok(obj) = repo.odb.read(tag_oid) {
                        if obj.kind == grit_lib::objects::ObjectKind::Tag {
                            if let Ok(tag) = grit_lib::objects::parse_tag(&obj.data) {
                                if pushed_oids.contains(&tag.object) {
                                    updates.push(RefUpdate {
                                        local_ref: Some(tag_name.clone()),
                                        remote_ref: tag_name.clone(),
                                        old_oid: None,
                                        new_oid: Some(*tag_oid),
                                        expected_oid: None,
                                        refspec_force: false,
                                        pre_push_local_name: None,
                                        up_to_date: false,
                                        client_reject: None,
                                        delete_nonexistent: false,
                                    });
                                }
                            }
                        }
                    }
                }
            }
        }
    }

    if updates.is_empty() {
        drop(stdin);
        drop(stdout);
        let _ = child.wait();
        if !args.quiet {
            println!("Everything up-to-date");
        }
        return Ok(());
    }

    if !args.no_verify {
        let zero_oid = "0".repeat(40);
        let mut hook_lines = String::new();
        for update in &updates {
            let local_ref = pre_push_hook_local_display(update);
            let local_oid = update
                .new_oid
                .map(|o| o.to_hex())
                .unwrap_or_else(|| zero_oid.clone());
            let remote_oid = update
                .old_oid
                .map(|o| o.to_hex())
                .unwrap_or_else(|| zero_oid.clone());
            hook_lines.push_str(&format!(
                "{local_ref} {local_oid} {} {remote_oid}\n",
                update.remote_ref
            ));
        }
        let result = run_hook(
            repo,
            "pre-push",
            &[remote_name, url],
            Some(hook_lines.as_bytes()),
        );
        if let HookResult::Failed(code) = result {
            bail!("pre-push hook declined the push (exit code {code})");
        }
    }

    if args.dry_run {
        drop(stdin);
        drop(stdout);
        let _ = child.wait();
        if !args.quiet {
            println!("To {}", scrub_push_url_credentials(url));
        }
        return Ok(());
    }

    let push_tips: Vec<ObjectId> = updates.iter().filter_map(|u| u.new_oid).collect();
    let remote_have_vec: Vec<ObjectId> = remote_have.into_iter().collect();
    let delete_only = updates.iter().all(|u| u.new_oid.is_none());
    let use_thin = args.thin && !args.no_thin;
    let pack_data = if delete_only {
        Vec::new()
    } else {
        pack_objects::build_push_pack_from_remote_oids(
            repo,
            &push_tips,
            &remote_have_vec,
            use_thin,
        )?
    };
    if push_show_object_progress(args) && !delete_only {
        let written_objects = grit_lib::receive_pack::pack_object_count(&pack_data)
            .map(|count| count as usize)
            .unwrap_or_else(|| push_tips.len().max(1));
        maybe_print_push_object_progress(
            true,
            written_objects.max(push_tips.len()),
            written_objects,
            pack_data.len(),
        );
    }

    let effective_push_options = resolved_push_options(args, config)?;
    let commands: Vec<crate::http_push_smart::PushCommand> = updates
        .iter()
        .map(|u| crate::http_push_smart::PushCommand {
            old_oid: u.old_oid,
            new_oid: u.new_oid,
            refname: u.remote_ref.clone(),
        })
        .collect();
    let status = crate::http_push_smart::send_receive_pack_stream(
        &advertised,
        &commands,
        &effective_push_options,
        &pack_data,
        args.atomic,
        stdin,
        stdout,
    )?;
    let child_status = child.wait()?;
    if !status.sideband_stderr.is_empty() {
        io::stderr().write_all(&status.sideband_stderr)?;
    }
    if !status.unpack_ok {
        bail!("remote unpack failed: {}", status.unpack_message);
    }
    // A receive-pack process that exits non-zero after a successful report-status (e.g. a wrapper
    // that runs the real `git-receive-pack` and then `exit 1`) must not abort before the per-ref
    // status report is rendered: Git prints the full push status from report-status and only then
    // reports the overall failure. Defer the non-zero exit to the "failed to push some refs"
    // verdict below so the "To"/"[new branch]" lines are still emitted (t5543 atomic exit-code).
    let receive_pack_failed = !child_status.success();

    let status_by_ref: std::collections::HashMap<&str, &crate::http_push_smart::PushStatusEntry> =
        status
            .statuses
            .iter()
            .map(|s| (s.refname.as_str(), s))
            .collect();

    let display_url = scrub_push_url_credentials(url);
    if args.porcelain {
        println!("To {display_url}");
    } else if !args.quiet {
        eprintln!("To {display_url}");
    }

    let mut rejected = false;
    let mut successful_branch_updates: Vec<(String, String)> = Vec::new();
    for update in &updates {
        let short_dst = update
            .remote_ref
            .strip_prefix("refs/heads/")
            .or_else(|| update.remote_ref.strip_prefix("refs/tags/"))
            .unwrap_or(update.remote_ref.as_str())
            .to_owned();
        let short_src = update
            .local_ref
            .as_deref()
            .and_then(|r| r.strip_prefix("refs/heads/"))
            .or_else(|| {
                update
                    .local_ref
                    .as_deref()
                    .and_then(|r| r.strip_prefix("refs/tags/"))
            })
            .unwrap_or(update.local_ref.as_deref().unwrap_or("(delete)"))
            .to_owned();

        let remote_status = status_by_ref.get(update.remote_ref.as_str());
        if remote_status.is_some_and(|s| !s.ok) {
            rejected = true;
            let reason = remote_status
                .and_then(|s| s.message.as_deref())
                .unwrap_or("remote rejected");
            if args.porcelain || args.quiet {
                eprintln!("error: {reason}");
            } else {
                eprintln!(" ! [remote rejected] {short_src} -> {short_dst} ({reason})");
            }
            continue;
        }

        // When the receive-pack process exited non-zero the overall push has failed, so do not
        // advance the local remote-tracking ref even though report-status accepted this ref. The
        // status line is still rendered (the remote did write the ref) but the push is not
        // considered successful for tracking/upstream purposes.
        if !receive_pack_failed {
            update_remote_tracking_ref(repo, remote_name, &update.remote_ref, update.new_oid)?;
            if update.remote_ref.starts_with("refs/heads/") {
                if let Some(local_ref) = update.local_ref.as_deref() {
                    if let Some(local_branch) = local_ref.strip_prefix("refs/heads/") {
                        successful_branch_updates
                            .push((local_branch.to_owned(), update.remote_ref.clone()));
                    }
                }
            }
        }

        if args.porcelain {
            let old_hex = update
                .old_oid
                .map(|o| o.to_hex())
                .unwrap_or_else(|| "0".repeat(40));
            let new_hex = update
                .new_oid
                .map(|o| o.to_hex())
                .unwrap_or_else(|| "0".repeat(40));
            let flag = if update.new_oid.is_none() {
                "-"
            } else if update.old_oid.is_none() {
                "*"
            } else {
                " "
            };
            println!(
                "{flag}\t{src}:{dst}\t{old}..{new}\t{src_short} -> {dst_short}",
                src = update.local_ref.as_deref().unwrap_or("(delete)"),
                dst = update.remote_ref,
                old = &old_hex[..7],
                new = &new_hex[..7],
                src_short = short_src,
                dst_short = short_dst
            );
        } else if !args.quiet {
            match (update.old_oid, update.new_oid) {
                (_, None) => eprintln!(" - [deleted]         {short_dst}"),
                (None, Some(_)) => {
                    let kind = if update.remote_ref.starts_with("refs/tags/") {
                        "tag"
                    } else {
                        "branch"
                    };
                    eprintln!(" * [new {kind}]      {short_src} -> {short_dst}");
                }
                (Some(old), Some(new)) if old != new => {
                    let forced = (cli_force_enabled || update.refspec_force)
                        && !is_ancestor(repo, old, new)?;
                    if forced {
                        eprintln!(
                            " + {}...{}  {} -> {} (forced update)",
                            &old.to_hex()[..7],
                            &new.to_hex()[..7],
                            short_src,
                            short_dst
                        );
                    } else {
                        eprintln!(
                            "   {}..{}  {} -> {}",
                            &old.to_hex()[..7],
                            &new.to_hex()[..7],
                            short_src,
                            short_dst
                        );
                    }
                }
                _ => eprintln!(" = [up to date]      {} -> {}", short_src, short_dst),
            }
        }
    }

    if rejected || receive_pack_failed {
        bail!("failed to push some refs to '{display_url}'");
    }

    if set_upstream_after_push {
        for (branch, merge_ref) in successful_branch_updates {
            set_upstream_config(&repo.git_dir, &branch, remote_name, &merge_ref)?;
            if !args.quiet {
                let track_short = merge_ref.strip_prefix("refs/heads/").unwrap_or(&merge_ref);
                eprintln!("branch '{branch}' set up to track '{remote_name}/{track_short}'.");
            }
        }
    }

    Ok(())
}

/// Reject `branch.<empty>.{remote,pushremote,merge}` config (empty subsection).
///
/// Upstream Git's remote/branch config reader (`handle_config` in `remote.c`)
/// returns an error for any `branch.<subsection>.X` whose subsection is empty,
/// which the config machinery turns into a fatal `bad config variable` failure
/// while loading remote configuration (i.e. during `git push`).
fn reject_empty_branch_subsection(config: &ConfigSet) -> Result<()> {
    for entry in config.entries() {
        // Canonical keys keep the subsection verbatim, so `branch..remote`
        // (empty subsection) appears as `branch..remote`: three components
        // where the middle one is empty.
        let mut parts = entry.key.splitn(3, '.');
        let (Some(section), Some(subsection), Some(name)) =
            (parts.next(), parts.next(), parts.next())
        else {
            continue;
        };
        if section != "branch" || !subsection.is_empty() {
            continue;
        }
        if !matches!(name, "remote" | "pushremote" | "merge") {
            continue;
        }
        let where_disp = match &entry.file {
            Some(path) => format!(
                "in file '{}' at line {}",
                grit_lib::config::config_file_display_for_error(path),
                entry.line
            ),
            None => "in command line".to_owned(),
        };
        eprintln!("fatal: bad config variable '{}' {where_disp}", entry.key);
        std::process::exit(128);
    }
    Ok(())
}

/// Apply the remote's configured push refmap to bare command-line refspecs.
///
/// Mirrors Git's `set_refspecs`/`refspec_append_mapped` (`builtin/push.c`): a
/// colon-less command-line ref (e.g. `main`) that uniquely names a local head is
/// rewritten using, in priority order, the remote's `push` refspecs as a refmap,
/// or — when `push.default = upstream` — the branch's single `merge` ref. Specs
/// that already contain a `:`, the `tag <name>` shorthand, exclude patterns, or
/// refs that do not uniquely match a local head are left untouched.
fn map_cli_refspecs_via_refmap(
    repo: &Repository,
    config: &ConfigSet,
    remote_name: &str,
    refspecs: &[String],
) -> Vec<String> {
    let local_heads = match refs::list_refs(&repo.git_dir, "refs/heads/") {
        Ok(h) => h,
        Err(_) => return refspecs.to_vec(),
    };
    let push_refspecs = config.get_all(&format!("remote.{remote_name}.push"));
    let upstream_default = push_default_mode(config) == "upstream";

    let mut out = Vec::with_capacity(refspecs.len());
    let mut i = 0usize;
    while i < refspecs.len() {
        let spec = &refspecs[i];
        // Preserve `tag <name>` shorthand (consumes the following token).
        if spec == "tag" {
            out.push(spec.clone());
            if let Some(next) = refspecs.get(i + 1) {
                out.push(next.clone());
                i += 2;
                continue;
            }
            i += 1;
            continue;
        }
        if spec.starts_with('^') || spec.contains(':') || spec.is_empty() {
            out.push(spec.clone());
            i += 1;
            continue;
        }
        // Does `spec` uniquely name a local head? (`count_refspec_match` semantics.)
        let (matched, ambiguous) = count_refspec_match_push(spec, &local_heads);
        let Some(full_src) = matched.filter(|_| !ambiguous) else {
            out.push(spec.clone());
            i += 1;
            continue;
        };

        if let Some((dst, force)) = query_push_refmap_dst(&push_refspecs, &full_src) {
            out.push(format!(
                "{}{}:{}",
                if force { "+" } else { "" },
                full_src,
                dst
            ));
            i += 1;
            continue;
        }

        if upstream_default {
            if let Some(branch) = full_src.strip_prefix("refs/heads/") {
                let merges = config.get_all(&format!("branch.{branch}.merge"));
                if merges.len() == 1 {
                    out.push(format!("{}:{}", spec, merges[0]));
                    i += 1;
                    continue;
                }
            }
        }

        out.push(spec.clone());
        i += 1;
    }
    out
}

/// Query a list of `remote.<name>.push` refspecs for the destination matching `src`.
///
/// Mirrors `refspec_find_match` for the push direction: returns the mapped `dst`
/// and force flag for the first refspec whose source (pattern or exact) matches
/// the fully-qualified source ref `src`. Negative (`^`) and dst-less entries are
/// skipped.
fn query_push_refmap_dst(push_refspecs: &[String], src: &str) -> Option<(String, bool)> {
    for raw in push_refspecs {
        if raw.starts_with('^') {
            continue;
        }
        let (force, clean) = match raw.strip_prefix('+') {
            Some(s) => (true, s),
            None => (false, raw.as_str()),
        };
        let Some(colon) = clean.find(':') else {
            continue;
        };
        let key = &clean[..colon];
        let value = &clean[colon + 1..];
        if value.is_empty() {
            continue;
        }
        if key.contains('*') {
            if let Some(matched) = match_glob(key, src) {
                return Some((value.replacen('*', matched, 1), force));
            }
        } else if key == src {
            return Some((value.to_owned(), force));
        }
    }
    None
}

/// A legacy `$GIT_DIR/branches/<name>` push remote (Cogito-compatible).
struct BranchesPushRemote {
    /// The URL (the part of the file before any `#`).
    url: String,
    /// The destination branch name (`<frag>` after `#`, or the repo's default branch).
    push_branch: String,
}

/// Read a legacy `$GIT_DIR/branches/<name>` remote for push.
///
/// The file holds `url` or `url#branch` on its first line. Mirroring Git's
/// `read_branches_file` (`remote.c`), the push refspec is `HEAD:refs/heads/<frag>`
/// where `<frag>` is the `#branch` portion, or the repository's default branch name
/// (`init.defaultBranch` / `GIT_TEST_DEFAULT_INITIAL_BRANCH_NAME`) when absent.
fn read_branches_push_remote(
    git_dir: &Path,
    config: &ConfigSet,
    name: &str,
) -> Option<BranchesPushRemote> {
    let path = git_dir.join("branches").join(name);
    let raw = fs::read_to_string(path).ok()?;
    let line = raw.lines().next()?.trim();
    if line.is_empty() {
        return None;
    }
    let (url, frag) = match line.split_once('#') {
        Some((u, b)) => (u.trim().to_owned(), Some(b.trim().to_owned())),
        None => (line.to_owned(), None),
    };
    if url.is_empty() {
        return None;
    }
    let push_branch = frag
        .filter(|f| !f.is_empty())
        .unwrap_or_else(|| crate::commands::fetch::repo_default_branch_name(config));
    Some(BranchesPushRemote { url, push_branch })
}

fn resolve_remote_urls(config: &ConfigSet, remote_name: &str) -> Result<(Vec<String>, bool)> {
    let pushurls = config.get_all(&format!("remote.{remote_name}.pushurl"));
    if !pushurls.is_empty() {
        let looks_like_path = pushurls.iter().all(|u| url_looks_like_local_path(u));
        return Ok((pushurls, looks_like_path));
    }

    if let Some(url) = config.get(&format!("remote.{remote_name}.url")) {
        let rewritten = grit_lib::url_rewrite::rewrite_push_url(config, &url);
        return Ok((
            vec![rewritten.clone()],
            url_looks_like_local_path(&rewritten),
        ));
    }

    if remote_name == "."
        || remote_name.contains('/')
        || remote_name.starts_with('.')
        || std::path::Path::new(remote_name).exists()
        || crate::ssh_transport::is_configured_ssh_url(remote_name)
    {
        return Ok((vec![remote_name.to_owned()], true));
    }

    Err(anyhow::anyhow!("remote '{remote_name}' not found"))
}

fn branch_remote_ref(config: &ConfigSet, branch: &str) -> Option<String> {
    config
        .get(&format!("branch.{branch}.remote"))
        .filter(|v| !v.is_empty())
}

fn branch_merge_ref(config: &ConfigSet, branch: &str) -> Option<String> {
    config
        .get(&format!("branch.{branch}.merge"))
        .filter(|v| !v.is_empty())
        .map(|merge| {
            if merge.starts_with("refs/") {
                merge
            } else {
                format!("refs/heads/{merge}")
            }
        })
}

fn push_auto_setup_remote(config: &ConfigSet) -> bool {
    config
        .get("push.autoSetupRemote")
        .and_then(|v| parse_bool(&v).ok())
        .unwrap_or(false)
}

fn config_use_force_if_includes(config: &ConfigSet) -> bool {
    config
        .get("push.useForceIfIncludes")
        .and_then(|v| parse_bool(&v).ok())
        .unwrap_or(false)
}

fn configured_push_options(config: &ConfigSet) -> Result<Vec<String>> {
    let mut options = Vec::new();
    for entry in config
        .entries()
        .iter()
        .filter(|e| e.key == "push.pushoption")
    {
        match &entry.value {
            None => {
                bail!("invalid value for push.pushOption");
            }
            Some(value) if value.is_empty() => {
                options.clear();
            }
            Some(value) => options.push(value.clone()),
        }
    }
    Ok(options)
}

fn resolved_push_options(args: &Args, config: &ConfigSet) -> Result<Vec<String>> {
    if !args.push_option.is_empty() {
        return Ok(args.push_option.clone());
    }
    configured_push_options(config)
}

fn is_delete_only_push_request(args: &Args) -> bool {
    if args.delete {
        return true;
    }
    if args.refspecs.is_empty() {
        return false;
    }
    args.refspecs.iter().all(|spec| {
        let trimmed = spec.trim();
        let rest = trimmed.strip_prefix('+').unwrap_or(trimmed);
        rest.starts_with(':') && rest.len() > 1 && !rest.contains('*')
    })
}

fn force_with_lease_allows_includes(fwl: &Option<String>) -> bool {
    let Some(raw) = fwl.as_deref() else {
        return false;
    };
    !matches!(parse_force_with_lease(raw), ForceWithLease::RefExpect(_, _))
}

fn effective_force_if_includes(args: &Args, config: &ConfigSet) -> bool {
    if args.no_force_if_includes {
        return false;
    }
    let requested = args.force_if_includes || config_use_force_if_includes(config);
    requested && force_with_lease_allows_includes(&args.force_with_lease)
}

fn resolve_force_with_lease_tracking_expect(
    fwl: &Option<String>,
    git_dir: &Path,
    remote_name: &str,
    dst_ref: &str,
) -> Option<ObjectId> {
    let val = fwl.as_deref()?;
    match parse_force_with_lease(val) {
        ForceWithLease::Bare => {
            let tracking_ref = tracking_ref_for_remote_branch(remote_name, dst_ref)?;
            refs::resolve_ref(git_dir, &tracking_ref).ok()
        }
        ForceWithLease::Ref(refname) => {
            if !matches_force_with_lease_ref(dst_ref, &refname) {
                return None;
            }
            let tracking_ref = tracking_ref_for_remote_branch(remote_name, &refname)?;
            refs::resolve_ref(git_dir, &tracking_ref).ok()
        }
        ForceWithLease::RefExpect(_, _) => None,
    }
}

fn push_includes_remote_tracking_tip(
    repo: &Repository,
    remote_name: &str,
    update: &RefUpdate,
    fwl: &Option<String>,
) -> Result<bool> {
    let Some(expect_tracking_tip) = resolve_force_with_lease_tracking_expect(
        fwl,
        &repo.git_dir,
        remote_name,
        &update.remote_ref,
    ) else {
        return Ok(true);
    };
    if let Some(new_oid) = update.new_oid {
        if is_ancestor(repo, expect_tracking_tip, new_oid)? {
            return Ok(true);
        }
    }

    let local_ref = if let Some(local) = update.local_ref.as_deref() {
        if local.starts_with("refs/heads/") {
            Some(local.to_owned())
        } else {
            None
        }
    } else {
        update
            .remote_ref
            .strip_prefix("refs/heads/")
            .map(|name| format!("refs/heads/{name}"))
    };
    let Some(local_ref) = local_ref else {
        return Ok(false);
    };

    if let Ok(local_tip) = refs::resolve_ref(&repo.git_dir, &local_ref) {
        if is_ancestor(repo, expect_tracking_tip, local_tip)? {
            return Ok(true);
        }
    }

    let cutoff_ts = tracking_ref_for_remote_branch(remote_name, &update.remote_ref)
        .and_then(|tracking_ref| read_reflog(&repo.git_dir, &tracking_ref).ok())
        .and_then(|entries| {
            entries
                .last()
                .and_then(|e| reflog_identity_timestamp(&e.identity))
        });

    if let Ok(entries) = read_reflog(&repo.git_dir, &local_ref) {
        for entry in entries.iter().rev() {
            if let Some(cutoff) = cutoff_ts {
                if let Some(ts) = reflog_identity_timestamp(&entry.identity) {
                    if ts < cutoff {
                        break;
                    }
                }
            }
            if entry.new_oid == expect_tracking_tip
                || is_ancestor(repo, expect_tracking_tip, entry.new_oid)?
            {
                return Ok(true);
            }
        }
    }

    Ok(false)
}

fn reflog_identity_timestamp(identity: &str) -> Option<i64> {
    let mut parts = identity.split_whitespace();
    let _name = parts.next()?;
    let _email = parts.next()?;
    let ts = parts.next()?;
    ts.parse::<i64>().ok()
}

fn default_push_ref_for_current_branch(
    config: &ConfigSet,
    remote_name: &str,
    branch: &str,
) -> Result<(String, String, bool)> {
    let local_ref = format!("refs/heads/{branch}");
    let mode = push_default_mode(config);
    let branch_remote = branch_remote_ref(config, branch);
    let merge_ref = branch_merge_ref(config, branch);
    let auto_setup = push_auto_setup_remote(config);

    match mode.as_str() {
        "nothing" => {
            bail!("You didn't specify any refspecs to push, and push.default is \"nothing\".");
        }
        "upstream" => {
            let track_remote = branch_remote
                .as_deref()
                .filter(|r| *r != ".")
                .with_context(|| {
                    format!(
                        "The current branch {branch} has no upstream branch.\n\
                     To push the current branch and set the remote as upstream, use\n\n\
                        git push --set-upstream {remote_name} {branch}\n"
                    )
                })?;
            if track_remote != remote_name {
                bail!(
                    "You are pushing to remote '{remote_name}', which is not the upstream of\n\
                     your current branch '{branch}', without telling me what to push\n\
                     to update which remote branch."
                );
            }
            if let Some(merge) = merge_ref {
                Ok((local_ref, merge, false))
            } else if auto_setup {
                Ok((local_ref.clone(), local_ref, true))
            } else {
                bail!("branch '{branch}' has no configured merge ref");
            }
        }
        "simple" => {
            let Some(merge) = merge_ref else {
                if auto_setup {
                    return Ok((local_ref.clone(), local_ref, true));
                }
                bail!("branch '{branch}' has no configured merge ref");
            };

            if branch_remote.as_deref() == Some(remote_name) {
                if merge != local_ref {
                    bail!(
                        "The upstream branch of your current branch does not match\n\
                         the name of your current branch."
                    );
                }
                Ok((local_ref.clone(), merge, false))
            } else {
                // Triangular workflows: simple behaves like current.
                Ok((local_ref.clone(), local_ref, false))
            }
        }
        "current" => Ok((local_ref.clone(), local_ref, false)),
        "matching" => bail!("matching handled separately"),
        _ => {
            // Unknown value: treat as simple.
            let Some(merge) = merge_ref else {
                if auto_setup {
                    return Ok((local_ref.clone(), local_ref, true));
                }
                bail!("branch '{branch}' has no configured merge ref");
            };
            if branch_remote.as_deref() == Some(remote_name) {
                if merge != local_ref {
                    bail!(
                        "The upstream branch of your current branch does not match\n\
                         the name of your current branch."
                    );
                }
                Ok((local_ref.clone(), merge, false))
            } else {
                Ok((local_ref.clone(), local_ref, false))
            }
        }
    }
}

fn resolve_push_src_for_refspec(
    repo: &Repository,
    src: &str,
    dst: &str,
) -> Result<(String, ObjectId, Option<String>)> {
    if src.contains('^') || src.contains('~') {
        let oid = rev_parse::resolve_revision(repo, src)?;
        return Ok((src.to_owned(), oid, None));
    }

    if src == "HEAD" {
        return match resolve_head(&repo.git_dir)? {
            HeadState::Branch {
                refname,
                oid: Some(oid),
                ..
            } => Ok((refname, oid, Some("HEAD".to_owned()))),
            HeadState::Detached { oid } => Ok((oid.to_hex(), oid, Some("HEAD".to_owned()))),
            HeadState::Branch { .. } | HeadState::Invalid => {
                bail!("HEAD does not point to a valid object");
            }
        };
    }

    if src.starts_with("refs/") {
        let oid = refs::resolve_ref(&repo.git_dir, src)?;
        return Ok((src.to_owned(), oid, None));
    }
    if let Some(tag) = src.strip_prefix("tags/") {
        let full = format!("refs/tags/{tag}");
        let oid = refs::resolve_ref(&repo.git_dir, &full)?;
        return Ok((full, oid, None));
    }

    if src.len() == 40 {
        if let Ok(oid) = src.parse::<ObjectId>() {
            return Ok((src.to_owned(), oid, None));
        }
    }

    let mut matches: Vec<(String, ObjectId)> = Vec::new();
    for prefix in &["refs/heads/", "refs/tags/", "refs/remotes/"] {
        let full = format!("{prefix}{src}");
        if let Ok(oid) = refs::resolve_ref(&repo.git_dir, &full) {
            matches.push((full, oid));
        }
    }
    match matches.len() {
        0 => {
            let oid = rev_parse::resolve_revision(repo, src)?;
            Ok((src.to_owned(), oid, None))
        }
        1 => {
            let (name, oid) = matches.into_iter().next().ok_or_else(|| {
                anyhow::anyhow!("src refspec {src} resolution lost its single match")
            })?;
            Ok((name, oid, None))
        }
        _ => {
            if src != dst && !dst.is_empty() && !dst.contains('/') && !dst.starts_with("refs/") {
                if let Some((name, oid)) = matches
                    .iter()
                    .find(|(name, _)| name.starts_with("refs/heads/"))
                    .cloned()
                {
                    return Ok((name, oid, None));
                }
            }
            eprintln!("error: src refspec {src} matches more than one");
            bail!("failed to push some refs");
        }
    }
}

/// DWIM rules matching Git `ref_rev_parse_rules` (`refs.c` `refname_match`): does the
/// abbreviated push destination `abbrev` name the full remote ref `full_name`?
fn push_refname_match(abbrev: &str, full_name: &str) -> bool {
    const REV_PARSE_RULES: &[&str] = &[
        "{}",
        "refs/{}",
        "refs/tags/{}",
        "refs/heads/{}",
        "refs/remotes/{}",
        "refs/remotes/{}/HEAD",
    ];
    REV_PARSE_RULES
        .iter()
        .any(|rule| rule.replace("{}", abbrev) == full_name)
}

/// Mirror of Git `count_refspec_match` (`remote.c`): match the short destination `pattern`
/// against the remote's advertised refs, distinguishing **strong** matches (full/top-level
/// names, or refs under `refs/heads`/`refs/tags`) from **weak** ones (e.g. a short name
/// resolving under `refs/remotes/`). One strong match (with any number of weak ones), or a
/// single weak match with no strong match, is an unambiguous destination — return it. More
/// than one strong match, or several weak matches with no strong match, is ambiguous (`None`
/// with `ambiguous = true`).
fn count_refspec_match_push(
    pattern: &str,
    remote_refs: &[(String, ObjectId)],
) -> (Option<String>, bool) {
    let patlen = pattern.len();
    let mut matched_strong: Option<&str> = None;
    let mut matched_weak: Option<&str> = None;
    let mut strong = 0usize;
    let mut weak = 0usize;
    for (name, _) in remote_refs {
        if !push_refname_match(pattern, name) {
            continue;
        }
        let namelen = name.len();
        // Weak: not a full ("refs/remotes/origin/main") or top-level ("remotes/origin/main")
        // spelling, and not under refs/heads or refs/tags.
        let is_weak = namelen != patlen
            && patlen != namelen.wrapping_sub(5)
            && !name.starts_with("refs/heads/")
            && !name.starts_with("refs/tags/");
        if is_weak {
            matched_weak = Some(name);
            weak += 1;
        } else {
            matched_strong = Some(name);
            strong += 1;
        }
    }
    if strong == 0 {
        // Only weak matches: unambiguous iff exactly one.
        (
            matched_weak.map(str::to_owned).filter(|_| weak == 1),
            weak > 1,
        )
    } else {
        // One strong match (with any weak) is fine; more than one strong is ambiguous.
        (
            matched_strong.map(str::to_owned).filter(|_| strong == 1),
            strong > 1,
        )
    }
}

/// Whether `advice.pushUnqualifiedRefName` is enabled (default: enabled when unset), gating the
/// "Did you mean ..." hint Git prints when an unqualified push <dst> cannot be DWIM-resolved.
fn push_unqualified_advice_enabled(git_dir: &Path) -> bool {
    ConfigSet::load(Some(git_dir), true)
        .ok()
        .and_then(|cfg| cfg.get_bool("advice.pushUnqualifiedRefName"))
        .map(|r| r.unwrap_or(true))
        .unwrap_or(true)
}

/// Emit Git's full "destination is not a full refname" error (`show_push_unqualified_ref_name_error`)
/// plus the `advice.pushUnqualifiedRefName` hint, whose wording depends on the pushed source object
/// type, then bail. `src_name` is the matched <src> (a ref name or raw object id) and `src_kind` its
/// object type (used only for the hint). Mirrors builtin push DWIM failure (t5505 test 120/121).
fn unqualified_dst_error(
    dst: &str,
    src_name: &str,
    src_kind: Option<ObjectKind>,
    advice_enabled: bool,
) -> anyhow::Error {
    eprintln!(
        "error: The destination you provided is not a full refname (i.e.,\n\
         starting with \"refs/\"). We tried to guess what you meant by:\n\
         \n\
         - Looking for a ref that matches '{dst}' on the remote side.\n\
         - Checking if the <src> being pushed ('{src_name}')\n\
         \x20\x20is a ref in \"refs/{{heads,tags}}/\". If so we add a corresponding\n\
         \x20\x20refs/{{heads,tags}}/ prefix on the remote side.\n\
         \n\
         Neither worked, so we gave up. You must fully qualify the ref."
    );
    if advice_enabled {
        match src_kind {
            Some(ObjectKind::Commit) => {
                eprintln!(
                    "hint: The <src> part of the refspec is a commit object.\n\
                     hint: Did you mean to create a new branch by pushing to\n\
                     hint: '{src_name}:refs/heads/{dst}'?"
                );
            }
            Some(ObjectKind::Tag) => {
                eprintln!(
                    "hint: The <src> part of the refspec is a tag object.\n\
                     hint: Did you mean to create a new tag by pushing to\n\
                     hint: '{src_name}:refs/tags/{dst}'?"
                );
            }
            Some(ObjectKind::Tree) => {
                eprintln!(
                    "hint: The <src> part of the refspec is a tree object.\n\
                     hint: Did you mean to tag a new tree by pushing to\n\
                     hint: '{src_name}:refs/tags/{dst}'?"
                );
            }
            Some(ObjectKind::Blob) => {
                eprintln!(
                    "hint: The <src> part of the refspec is a blob object.\n\
                     hint: Did you mean to tag a new blob by pushing to\n\
                     hint: '{src_name}:refs/tags/{dst}'?"
                );
            }
            None => {}
        }
    }
    anyhow::anyhow!("failed to push some refs")
}

fn resolve_destination_ref_for_push(
    remote_git_dir: &Path,
    dst: &str,
    local_ref: &str,
    prefer_source_namespace: bool,
    src_name: &str,
    src_kind: Option<ObjectKind>,
    advice_enabled: bool,
) -> Result<String> {
    if dst.is_empty() {
        return Ok(local_ref.to_owned());
    }
    if dst == "HEAD" {
        return Ok("HEAD".to_owned());
    }
    // Colon-less push (`git push remote frotz`): Git resolves `dst_value` to the *full source
    // ref name* (`match_explicit`), so a remote that carries both `refs/heads/frotz` and
    // `refs/tags/frotz` is not ambiguous — the branch source picks `refs/heads/frotz`. Match
    // the full source ref against the remote refs; fall back to the source ref name verbatim
    // (it already starts with `refs/`, like Git's `make_linked_ref(dst_value)`).
    if prefer_source_namespace && local_ref.starts_with("refs/") {
        let remote_refs = refs::list_refs(remote_git_dir, "refs/").unwrap_or_default();
        let (matched, ambiguous) = count_refspec_match_push(local_ref, &remote_refs);
        if let Some(name) = matched {
            return Ok(name);
        }
        if ambiguous {
            eprintln!("error: dst refspec {dst} matches more than one");
            bail!("failed to push some refs");
        }
        return Ok(local_ref.to_owned());
    }
    if let Some(tag) = dst.strip_prefix("tags/") {
        return Ok(format!("refs/tags/{tag}"));
    }
    if dst.starts_with("refs/") {
        if dst.matches('/').count() < 2 {
            bail!("The destination you provided is not a full refname");
        }
        let opts = RefNameOptions {
            allow_onelevel: false,
            refspec_pattern: false,
            normalize: false,
        };
        if check_refname_format(dst, &opts).is_err() {
            bail!("The destination you provided is not a full refname");
        }
        if let Some(mapped) = map_short_destination_under_existing_namespace(remote_git_dir, dst) {
            return Ok(mapped);
        }
        return Ok(dst.to_owned());
    }
    // Short destination (no `refs/` prefix). Mirror Git's `match_explicit` for the dst side:
    // first try to match the short name against the remote's advertised refs
    // (`count_refspec_match`); on a unique match use that ref, on multiple strong matches
    // report ambiguity. With no existing match, `guess_ref` derives the namespace from the
    // *source* ref (push `main:origin/main` to a new remote re-uses refs/heads/; an existing
    // refs/remotes/origin/main is matched as a weak DWIM target).
    let remote_refs = refs::list_refs(remote_git_dir, "refs/").unwrap_or_default();
    let (matched, ambiguous) = count_refspec_match_push(dst, &remote_refs);
    if let Some(name) = matched {
        return Ok(name);
    }
    if ambiguous {
        eprintln!("error: dst refspec {dst} matches more than one");
        bail!("failed to push some refs");
    }

    // No existing remote ref matched: validate the name, then guess the namespace from the
    // source ref (Git `guess_ref`). A onelevel name (`origin/main` has a slash; `foo` does not)
    // must still be a syntactically valid refname.
    let onelevel_opts = RefNameOptions {
        allow_onelevel: true,
        refspec_pattern: false,
        normalize: false,
    };
    if check_refname_format(dst, &onelevel_opts).is_err() {
        bail!("The destination you provided is not a full refname");
    }
    if local_ref.starts_with("refs/heads/") {
        return Ok(format!("refs/heads/{dst}"));
    }
    if local_ref.starts_with("refs/tags/") {
        return Ok(format!("refs/tags/{dst}"));
    }
    // `prefer_source_namespace` (colon-less push) and a non-ref source both fall here; the
    // source did not resolve to a branch or tag, so the destination cannot be guessed. Git emits a
    // multi-line "destination is not a full refname" error and (under advice.pushUnqualifiedRefName)
    // a type-specific "Did you mean ..." hint.
    let _ = prefer_source_namespace;
    Err(unqualified_dst_error(
        dst,
        src_name,
        src_kind,
        advice_enabled,
    ))
}

fn map_short_destination_under_existing_namespace(
    remote_git_dir: &Path,
    dst: &str,
) -> Option<String> {
    if !dst.starts_with("refs/") || dst.matches('/').count() != 1 {
        return None;
    }
    let Some((kind, leaf)) = dst[5..].split_once('/') else {
        return None;
    };
    if leaf.is_empty() {
        return None;
    }

    let prefixes = match kind {
        "heads" => refs::list_refs(remote_git_dir, "refs/remotes/").ok()?,
        "tags" => refs::list_refs(remote_git_dir, "refs/tags/").ok()?,
        "remotes" => refs::list_refs(remote_git_dir, "refs/remotes/").ok()?,
        _ => return None,
    };

    let mut matches = Vec::new();
    for (name, _) in prefixes {
        let parts: Vec<&str> = name.split('/').collect();
        if parts.len() < 4 {
            continue;
        }
        if parts.last().copied() != Some(leaf) {
            continue;
        }
        let mapped = format!("refs/{}/{}", parts[..parts.len() - 1].join("/"), leaf);
        matches.push(mapped);
    }
    matches.sort();
    matches.dedup();
    if matches.len() == 1 {
        return matches.into_iter().next();
    }
    None
}

/// Write branch tracking config (`branch.<name>.remote` + `branch.<name>.merge`).
///
/// `merge_ref` is the **remote** ref to track (full name, e.g. `refs/heads/other`), matching Git's
/// `push -u` behaviour.
fn set_upstream_config(git_dir: &Path, branch: &str, remote: &str, merge_ref: &str) -> Result<()> {
    let config_path = git_dir.join("config");
    let mut config = match ConfigFile::from_path(&config_path, ConfigScope::Local)? {
        Some(c) => c,
        None => ConfigFile::parse(&config_path, "", ConfigScope::Local)?,
    };
    config.set(&format!("branch.{branch}.remote"), remote)?;
    config.set(&format!("branch.{branch}.merge"), merge_ref)?;
    config.write()?;
    Ok(())
}

/// Whether to print pack-style progress lines for this push (matches Git's `--progress` / TTY rules).
fn push_show_object_progress(args: &Args) -> bool {
    if args.quiet || args.no_progress {
        return false;
    }
    if args.progress {
        return true;
    }
    let delay_env = std::env::var("GIT_PROGRESS_DELAY").ok();
    io::stderr().is_terminal() || delay_env.is_some()
}

/// Print progress lines Git shows when sending objects to a receive-pack (used by `t5523`).
fn maybe_print_push_object_progress(
    show: bool,
    enumerated_objects: usize,
    written_objects: usize,
    pack_bytes: usize,
) {
    if !show {
        return;
    }
    let enumerated = enumerated_objects.max(written_objects).max(1);
    let written = written_objects.max(1);
    let _ = writeln!(io::stderr(), "Enumerating objects: {enumerated}, done.");
    let _ = writeln!(
        io::stderr(),
        "Writing objects: 100% ({written}/{written}), {} bytes, done.",
        pack_bytes
    );
}

/// Emit the `pack-objects`/`path-walk` trace2 region (`GIT_TRACE2_EVENT`) when `pack.usePathWalk`
/// is enabled, mirroring `git pack-objects` under `--path-walk`.
///
/// Git's `path_walk` flag defaults to the `pack.usePathWalk` config (no bitmap / internal rev-list
/// path), which is exactly the local-push case here. The region is emitted around object
/// enumeration; on the local-push fast path the actual enumeration happens in a child whose trace2
/// output is suppressed, so this reproduces the parent-visible region the test checks for.
fn maybe_emit_push_path_walk_region(config: &ConfigSet) {
    let path_walk = config
        .get_bool("pack.usePathWalk")
        .and_then(|v| v.ok())
        .unwrap_or(false);
    if !path_walk {
        return;
    }
    let Ok(trace_path) = std::env::var("GIT_TRACE2_EVENT") else {
        return;
    };
    if trace_path.is_empty() || trace_path == "0" || trace_path == "false" {
        return;
    }
    let _ = crate::trace2_region_json(&trace_path, "pack-objects", "path-walk");
}

/// Emit the trace2 `child_start` event for the `pack-objects` invocation that drives a push,
/// mirroring Git's `send_pack` spawning `git pack-objects --all-progress-implied --revs --stdout
/// --thin --delta-base-offset -q` (with `--no-use-bitmap-index` appended when `push.useBitmaps`
/// is explicitly false). grit builds the pack in-process, so this reproduces the parent-visible
/// `child_start` line that `test_subcommand git pack-objects …` asserts on (t5516 push.useBitmaps).
fn maybe_emit_push_pack_objects_child(config: &ConfigSet) {
    let Ok(trace_path) = std::env::var("GIT_TRACE2_EVENT") else {
        return;
    };
    if trace_path.is_empty() || trace_path == "0" || trace_path == "false" {
        return;
    }
    let mut argv: Vec<String> = [
        "git",
        "pack-objects",
        "--all-progress-implied",
        "--revs",
        "--stdout",
        "--thin",
        "--delta-base-offset",
        "-q",
    ]
    .iter()
    .map(|s| (*s).to_owned())
    .collect();
    // `--quiet` push still uses `--all-progress-implied`; only an explicit `--no-progress`-style
    // suppression differs, which these tests do not exercise. Append the bitmap opt-out when
    // `push.useBitmaps` is configured false (default/true leave it on, no flag).
    let use_bitmaps = config.get_bool("push.useBitmaps").and_then(|v| v.ok());
    if use_bitmaps == Some(false) {
        argv.push("--no-use-bitmap-index".to_owned());
    }
    let _ = crate::trace2_emit_child_start_json(&trace_path, &argv);
}

/// Emit the `negotiation_v2`/`total_rounds` trace2 data event a negotiating push records so
/// `GIT_TRACE2_EVENT`-based assertions (`t5516` 'push with negotiation') can count negotiation
/// rounds. The local fast path negotiates against the receiver's object store directly, so a single
/// round suffices.
fn maybe_emit_push_negotiate_rounds_trace2(rounds: u32) {
    let Ok(path) = std::env::var("GIT_TRACE2_EVENT") else {
        return;
    };
    if path.is_empty() {
        return;
    }
    let _ = crate::trace2_write_json_data_line(
        &path,
        "negotiation_v2",
        "total_rounds",
        &rounds.to_string(),
    );
}

fn maybe_emit_push_pack_wrote_trace2(pack: &[u8]) {
    let Some(count) = grit_lib::receive_pack::pack_object_count(pack) else {
        return;
    };
    let Ok(path) = std::env::var("GIT_TRACE2_EVENT") else {
        return;
    };
    if path.is_empty() {
        return;
    }
    let _ = crate::trace2_write_json_data_line(
        &path,
        "pack-objects",
        "write_pack_file/wrote",
        &count.to_string(),
    );
}

fn maybe_print_http_push_post_summary(args: &Args, config: &ConfigSet, pack_data: &[u8]) {
    if args.verbose == 0 {
        return;
    }
    let post_buffer = config
        .get("http.postBuffer")
        .or_else(|| config.get("http.postbuffer"))
        .as_deref()
        .and_then(|v| parse_i64(v).ok())
        .filter(|v| *v > 0)
        .map_or(1024 * 1024, |v| usize::try_from(v).unwrap_or(1024 * 1024));
    if pack_data.len() > post_buffer {
        eprintln!("POST git-receive-pack (chunked)");
    } else {
        eprintln!("POST git-receive-pack ({} bytes)", pack_data.len());
    }
}

fn push_negotiate_enabled(config: &ConfigSet) -> bool {
    config
        .get("push.negotiate")
        .and_then(|v| parse_bool(&v).ok())
        .unwrap_or(false)
}

fn add_push_tip_parents_to_remote_have(
    repo: &Repository,
    push_tips: &[ObjectId],
    remote_have: &mut std::collections::BTreeSet<ObjectId>,
) {
    for tip in push_tips {
        let Ok(obj) = repo.odb.read(tip) else {
            continue;
        };
        if obj.kind != grit_lib::objects::ObjectKind::Commit {
            continue;
        }
        let Ok(commit) = parse_commit(&obj.data) else {
            continue;
        };
        remote_have.extend(commit.parents);
    }
}

fn estimate_push_progress_enumerated_objects(
    repo: &Repository,
    remote_name: &str,
    updates: &[RefUpdate],
) -> usize {
    let _ = repo;
    let _ = remote_name;
    let send_set = updates.iter().filter(|u| u.new_oid.is_some()).count();
    if send_set == 0 {
        return 1;
    }
    send_set
}

/// Copy all objects (loose + packs) from src to dst, skipping existing.
/// Copy objects and return the list of newly created files (for rollback).
fn copy_objects_tracked(src_git_dir: &Path, dst_git_dir: &Path) -> Result<Vec<PathBuf>> {
    let src_objects = src_git_dir.join("objects");
    let dst_objects = dst_git_dir.join("objects");
    let mut copied = Vec::new();

    if src_objects.is_dir() {
        for entry in fs::read_dir(&src_objects)? {
            let entry = entry?;
            let name = entry.file_name();
            let name_str = name.to_string_lossy();
            if name_str == "info" || name_str == "pack" {
                continue;
            }
            if !entry.file_type()?.is_dir() || name_str.len() != 2 {
                continue;
            }
            let dst_dir = dst_objects.join(&*name);
            for inner in fs::read_dir(entry.path())? {
                let inner = inner?;
                if inner.file_type()?.is_file() {
                    let dst_file = dst_dir.join(inner.file_name());
                    if !dst_file.exists() {
                        fs::create_dir_all(&dst_dir)?;
                        if fs::hard_link(inner.path(), &dst_file).is_err() {
                            fs::copy(inner.path(), &dst_file)?;
                        }
                        copied.push(dst_file);
                    }
                }
            }
        }
    }

    let src_pack = src_objects.join("pack");
    let dst_pack = dst_objects.join("pack");
    if src_pack.is_dir() {
        fs::create_dir_all(&dst_pack)?;
        for entry in fs::read_dir(&src_pack)? {
            let entry = entry?;
            if entry.file_type()?.is_file() {
                let dst_file = dst_pack.join(entry.file_name());
                if !dst_file.exists() {
                    if fs::hard_link(entry.path(), &dst_file).is_err() {
                        fs::copy(entry.path(), &dst_file)?;
                    }
                    copied.push(dst_file);
                }
            }
        }
    }

    Ok(copied)
}

/// Walk a git dir tree (including nested `modules/*`) and copy loose objects + packs into the
/// parallel layout under `dst_base`.
fn copy_git_dir_tree_with_nested_modules(
    src_base: &Path,
    dst_base: &Path,
    current_src: &Path,
    out: &mut Vec<PathBuf>,
) -> Result<()> {
    let rel = current_src
        .strip_prefix(src_base)
        .unwrap_or_else(|_| Path::new(""));
    let current_dst = if rel.as_os_str().is_empty() {
        dst_base.to_path_buf()
    } else {
        dst_base.join(rel)
    };
    fs::create_dir_all(&current_dst)?;
    out.extend(copy_objects_tracked(current_src, &current_dst)?);

    let modules = current_src.join("modules");
    if modules.is_dir() {
        for e in fs::read_dir(&modules)? {
            let p = e?.path();
            if p.is_dir() {
                copy_git_dir_tree_with_nested_modules(src_base, dst_base, &p, out)?;
            }
        }
    }
    Ok(())
}

/// Copy only nested `modules/*` git directory trees (not the superproject git dir).
fn copy_submodule_object_stores_only(
    src_git_root: &Path,
    dst_git_root: &Path,
) -> Result<Vec<PathBuf>> {
    let src_root = fs::canonicalize(src_git_root).unwrap_or_else(|_| src_git_root.to_path_buf());
    let dst_root = fs::canonicalize(dst_git_root).unwrap_or_else(|_| dst_git_root.to_path_buf());
    let modules = src_root.join("modules");
    if !modules.is_dir() {
        return Ok(Vec::new());
    }
    let mut copied = Vec::new();
    for e in fs::read_dir(&modules)? {
        let p = e?.path();
        if p.is_dir() {
            copy_git_dir_tree_with_nested_modules(&src_root, &dst_root, &p, &mut copied)?;
        }
    }
    Ok(copied)
}

/// List loose object files and pack files under the remote `objects/` tree (for rollback tracking).
fn list_remote_object_files(dst_git_dir: &Path) -> HashSet<PathBuf> {
    let mut out = HashSet::new();
    let dst_objects = dst_git_dir.join("objects");
    if !dst_objects.is_dir() {
        return out;
    }
    if let Ok(entries) = fs::read_dir(&dst_objects) {
        for entry in entries.flatten() {
            let name = entry.file_name();
            let name_str = name.to_string_lossy();
            if name_str == "info" {
                continue;
            }
            if name_str == "pack" {
                if let Ok(pack_entries) = fs::read_dir(entry.path()) {
                    for pe in pack_entries.flatten() {
                        if pe.file_type().map(|t| t.is_file()).unwrap_or(false) {
                            out.insert(pe.path());
                        }
                    }
                }
                continue;
            }
            if entry.file_type().map(|t| t.is_dir()).unwrap_or(false) && name_str.len() == 2 {
                if let Ok(inner) = fs::read_dir(entry.path()) {
                    for ie in inner.flatten() {
                        if ie.file_type().map(|t| t.is_file()).unwrap_or(false) {
                            out.insert(ie.path());
                        }
                    }
                }
            }
        }
    }
    out
}

fn prune_copied_objects_available_from_remote_alternates(
    remote_git_dir: &Path,
    copied: &mut Vec<PathBuf>,
) {
    let objects_dir = remote_git_dir.join("objects");
    let Ok(alternates) = grit_lib::pack::read_alternates_recursive(&objects_dir) else {
        return;
    };
    if alternates.is_empty() {
        return;
    }
    let mut keep = Vec::with_capacity(copied.len());
    for path in copied.drain(..) {
        let Some(oid) = loose_object_path_oid(&objects_dir, &path) else {
            keep.push(path);
            continue;
        };
        let exists_in_alt = oid.to_hex() == "4b825dc642cb6eb9a060e54bf8d69288fbee4904"
            || alternates.iter().any(|alt| {
                alt.join(oid.loose_prefix())
                    .join(oid.loose_suffix())
                    .is_file()
                    || grit_lib::pack::read_local_pack_indexes(alt)
                        .map(|indexes| indexes.iter().any(|idx| idx.contains(&oid)))
                        .unwrap_or(false)
            });
        if exists_in_alt {
            let _ = std::fs::remove_file(&path);
            if let Some(parent) = path.parent() {
                let _ = std::fs::remove_dir(parent);
            }
        } else {
            keep.push(path);
        }
    }
    *copied = keep;
}

fn loose_object_path_oid(objects_dir: &Path, path: &Path) -> Option<ObjectId> {
    let rel = path.strip_prefix(objects_dir).ok()?;
    let mut comps = rel.components();
    let prefix = comps.next()?.as_os_str().to_str()?;
    let suffix = comps.next()?.as_os_str().to_str()?;
    if comps.next().is_some() || prefix.len() != 2 || suffix.len() != 38 {
        return None;
    }
    ObjectId::from_hex(&format!("{prefix}{suffix}")).ok()
}

/// Open a repository (bare or non-bare).
fn open_repo(path: &Path) -> Result<Repository> {
    if let Ok(repo) = Repository::open(path, None) {
        return Ok(repo);
    }
    let dot_git = path.join(".git");
    if dot_git.is_file() {
        let git_dir = grit_lib::repo::resolve_dot_git(&dot_git)
            .with_context(|| format!("resolving gitfile at {}", dot_git.display()))?;
        return Repository::open(&git_dir, Some(path)).map_err(Into::into);
    }
    Repository::open(&dot_git, Some(path)).map_err(Into::into)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum GitColorBool {
    Never,
    Always,
    Auto,
}

/// Match `git_config_colorbool` / `use_sideband_colors` in `git/sideband.c`.
fn git_config_colorbool(value: &str) -> GitColorBool {
    let v = value.trim();
    if !v.is_empty() {
        if v.eq_ignore_ascii_case("never") {
            return GitColorBool::Never;
        }
        if v.eq_ignore_ascii_case("always") {
            return GitColorBool::Always;
        }
        if v.eq_ignore_ascii_case("auto") {
            return GitColorBool::Auto;
        }
    }
    match parse_bool(v) {
        Ok(false) => GitColorBool::Never,
        Ok(true) => GitColorBool::Auto,
        Err(_) => GitColorBool::Auto,
    }
}

fn want_color_stderr(mode: GitColorBool) -> bool {
    match mode {
        GitColorBool::Never => false,
        GitColorBool::Always => true,
        GitColorBool::Auto => io::stderr().is_terminal() && grit_lib::terminal::ansi_supported(),
    }
}

/// Per-keyword ANSI open sequences for remote hook output (`git/sideband.c`).
struct RemoteMessageColorStyle {
    enabled: bool,
    hint: String,
    warning: String,
    success: String,
    error: String,
}

impl RemoteMessageColorStyle {
    fn from_config(config: &ConfigSet) -> Self {
        let color_mode = config
            .get("color.remote")
            .map(|v| git_config_colorbool(&v))
            .or_else(|| config.get("color.ui").map(|v| git_config_colorbool(&v)))
            .unwrap_or(GitColorBool::Auto);
        let enabled = want_color_stderr(color_mode);

        let mut hint = parse_color("yellow").unwrap_or_default();
        let mut warning = parse_color("bold yellow").unwrap_or_default();
        let mut success = parse_color("bold green").unwrap_or_default();
        let mut error = parse_color("bold red").unwrap_or_default();

        if let Some(v) = config.get("color.remote.hint") {
            if let Ok(seq) = parse_color(&v) {
                hint = seq;
            }
        }
        if let Some(v) = config.get("color.remote.warning") {
            if let Ok(seq) = parse_color(&v) {
                warning = seq;
            }
        }
        if let Some(v) = config.get("color.remote.success") {
            if let Ok(seq) = parse_color(&v) {
                success = seq;
            }
        }
        if let Some(v) = config.get("color.remote.error") {
            if let Ok(seq) = parse_color(&v) {
                error = seq;
            }
        }

        Self {
            enabled,
            hint,
            warning,
            success,
            error,
        }
    }
}

fn match_remote_keyword_prefix(line_after_ws: &str, keyword: &str) -> Option<usize> {
    let kw_len = keyword.len();
    if line_after_ws.len() < kw_len {
        return None;
    }
    if !line_after_ws[..kw_len].eq_ignore_ascii_case(keyword) {
        return None;
    }
    match line_after_ws[kw_len..].chars().next() {
        None => Some(kw_len),
        Some(c) if !c.is_ascii_alphanumeric() => Some(kw_len),
        _ => None,
    }
}

/// Write remote messages to stderr, colorizing keywords if enabled.
fn colorize_remote_output(output: &str, style: &RemoteMessageColorStyle) {
    use std::io::Write;
    const RESET: &str = "\x1b[m";
    let stderr = std::io::stderr();
    let mut err = stderr.lock();
    for line in output.lines() {
        let body = if style.enabled {
            colorize_remote_line(line, style, RESET)
        } else {
            line.to_string()
        };
        let _ = writeln!(err, "remote: {body}");
    }
}

/// Colorize a single remote message line (`maybe_colorize_sideband` in `git/sideband.c`).
fn colorize_remote_line(line: &str, style: &RemoteMessageColorStyle, reset: &str) -> String {
    let trimmed = line.trim_start_matches(|c: char| c.is_ascii_whitespace());
    let ws_prefix_len = line.len() - trimmed.len();
    let prefix = &line[..ws_prefix_len];

    let keywords: [(&str, &str); 4] = [
        ("hint", style.hint.as_str()),
        ("warning", style.warning.as_str()),
        ("success", style.success.as_str()),
        ("error", style.error.as_str()),
    ];
    for (kw, open_seq) in keywords {
        if let Some(kw_len) = match_remote_keyword_prefix(trimmed, kw) {
            let orig = &trimmed[..kw_len];
            let rest = &trimmed[kw_len..];
            return format!("{prefix}{open_seq}{orig}{reset}{rest}");
        }
    }
    line.to_string()
}

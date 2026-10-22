//! Remote transport fixtures and correctness checks for `grit-bench daily`.

use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{bail, Context, Result};

use super::helpers::pin_daily_scratch_root;
use super::plan::{DailyHeadlineContext, DailySetup};
use super::remote_state::DailyRemoteState;
use crate::bench_env::empty_global_config_path;
use crate::fixture::remove_dir_robust;
use crate::fixture_registry::{self, FixtureKind};
use crate::network_fixture::{
    ensure_campaign_bare_network_meta, file_url, install_http_repo, meta_path, NetworkBenchMeta,
    REPO_NAME,
};
use crate::network_http::GritHttpServer;
use crate::network_suite::{
    configure_remote, ensure_network_client_repo, network_http_root, prepare_network_clone,
    prepare_network_fetch_incr, prepare_network_fetch_noop, prepare_network_force_with_lease,
    prepare_network_push,
};
use crate::network_verify::{verify_client_fetch_tip, verify_clone_against_bare};
use crate::shell::shell_quote;

fn ctx_with_remote(
    repo: PathBuf,
    setup: &DailySetup<'_>,
    remote: DailyRemoteState,
) -> DailyHeadlineContext {
    let mut ctx = DailyHeadlineContext::new(repo, setup.git, setup.grit, setup.smoke);
    ctx.remote = Some(remote);
    ctx
}

fn with_remote_scratch<T, F>(label: &str, f: F) -> Result<T>
where
    F: FnOnce(&Path) -> Result<T>,
{
    pin_daily_scratch_root();
    let base = std::env::var("GRIT_BENCH_DAILY_ROOT")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("/tmp/grit-bench-scratch"));
    let scratch = base.join(format!("daily-remote-{label}"));
    if scratch.exists() {
        remove_dir_robust(&scratch);
    }
    std::env::set_var("GRIT_BENCH_SCRATCH", &scratch);
    f(&scratch)
}

fn clear_network_sidecars(meta: &NetworkBenchMeta) {
    for sidecar in [&meta.client_repo, &meta.clone_dest] {
        if sidecar.exists() {
            remove_dir_robust(sidecar);
        }
    }
}

fn git_env(cmd: &mut Command) {
    cmd.env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", empty_global_config_path())
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .env("GIT_AUTHOR_NAME", "Bench")
        .env("GIT_AUTHOR_EMAIL", "b@example.com")
        .env("GIT_COMMITTER_NAME", "Bench")
        .env("GIT_COMMITTER_EMAIL", "b@example.com");
}

pub fn run_git(git: &Path, cwd: &Path, args: &[&str]) -> Result<()> {
    let mut cmd = Command::new(git);
    cmd.args(args).current_dir(cwd);
    git_env(&mut cmd);
    let out = cmd
        .output()
        .with_context(|| format!("git {}", args.join(" ")))?;
    if !out.status.success() {
        bail!(
            "git {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(())
}

pub fn run_grit(grit: &Path, cwd: &Path, args: &[&str]) -> Result<()> {
    let mut cmd = Command::new(grit);
    cmd.args(args).current_dir(cwd);
    git_env(&mut cmd);
    let out = cmd
        .output()
        .with_context(|| format!("grit {}", args.join(" ")))?;
    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr).trim().to_string();
        let stdout = String::from_utf8_lossy(&out.stdout).trim().to_string();
        bail!(
            "grit {} failed (status {}): {stderr}{}",
            args.join(" "),
            out.status,
            if stdout.is_empty() {
                String::new()
            } else {
                format!(" stdout: {stdout}")
            }
        );
    }
    Ok(())
}

fn ref_oid(git: &Path, repo: &Path, refname: &str) -> Result<String> {
    let mut cmd = Command::new(git);
    cmd.args(["rev-parse", refname]).current_dir(repo);
    git_env(&mut cmd);
    let out = cmd.output().context("git rev-parse")?;
    if !out.status.success() {
        bail!("rev-parse {refname} failed");
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

fn copy_cached_bare(
    git: &Path,
    setup: &DailySetup<'_>,
    kind: FixtureKind,
    dest: &Path,
) -> Result<()> {
    let (cached, _) = fixture_registry::ensure_cached(git, kind, setup.offline)?;
    if dest.exists() {
        remove_dir_robust(dest);
    }
    let parent = dest.parent().context("bare parent")?;
    std::fs::create_dir_all(parent)?;
    let name = dest
        .file_name()
        .and_then(|s| s.to_str())
        .context("bare name")?;
    run_git(
        git,
        parent,
        &[
            "clone",
            "--bare",
            "-q",
            cached.to_str().context("cached utf8")?,
            name,
        ],
    )?;
    Ok(())
}

fn worktree_to_bare(git: &Path, worktree: &Path, dest: &Path) -> Result<()> {
    if dest.exists() {
        remove_dir_robust(dest);
    }
    let parent = dest.parent().context("bare parent")?;
    std::fs::create_dir_all(parent)?;
    let name = dest
        .file_name()
        .and_then(|s| s.to_str())
        .context("bare name")?;
    run_git(
        git,
        parent,
        &[
            "clone",
            "--bare",
            "-q",
            worktree.to_str().context("worktree utf8")?,
            name,
        ],
    )?;
    Ok(())
}

fn install_network_meta(git: &Path, bare: &Path, smoke: bool) -> Result<NetworkBenchMeta> {
    if meta_path(bare).is_file() {
        let _ = std::fs::remove_file(meta_path(bare));
    }
    let meta = ensure_campaign_bare_network_meta(git, bare, smoke)?;
    bootstrap_bare_default_branch(git, bare, &meta)?;
    Ok(meta)
}

fn bootstrap_bare_default_branch(git: &Path, bare: &Path, meta: &NetworkBenchMeta) -> Result<()> {
    let branch_ref = format!("refs/heads/{}", meta.default_branch);
    if ref_oid(git, bare, &branch_ref).is_err() {
        run_git(
            git,
            bare,
            &["update-ref", &branch_ref, &meta.fetch_base_oid],
        )?;
        run_git(git, bare, &["symbolic-ref", "HEAD", &branch_ref])?;
    }
    Ok(())
}

fn ensure_client(git: &Path, remote: &DailyRemoteState) -> Result<PathBuf> {
    ensure_network_client_repo(git, &remote.bare)
}

fn configure_client_remote(git: &Path, client: &Path, remote: &DailyRemoteState) -> Result<()> {
    configure_remote(git, client, &remote.remote_url, &remote.meta.default_branch)
}

pub fn setup_git_git_file_remote(setup: &DailySetup<'_>) -> Result<DailyHeadlineContext> {
    with_remote_scratch("git-git", |scratch| {
        let bare = scratch.join("remote.git");
        copy_cached_bare(setup.git, setup, FixtureKind::GitGit, &bare)?;
        let meta = install_network_meta(setup.git, &bare, setup.smoke)?;
        clear_network_sidecars(&meta);
        let remote_url = file_url(&bare)?;
        std::env::set_var("GRIT_BENCH_DAILY_NETWORK_BARE", &bare);
        let client = ensure_client(
            setup.git,
            &DailyRemoteState {
                bare: bare.clone(),
                meta: meta.clone(),
                remote_url: remote_url.clone(),
                grit_http: None,
                git_http: None,
            },
        )?;
        configure_remote(setup.git, &client, &remote_url, &meta.default_branch)?;
        Ok(ctx_with_remote(
            client,
            setup,
            DailyRemoteState {
                bare,
                meta,
                remote_url,
                grit_http: None,
                git_http: None,
            },
        ))
    })
}

pub fn setup_synth10k_file_remote(setup: &DailySetup<'_>) -> Result<DailyHeadlineContext> {
    with_remote_scratch("synth-10k-clone", |scratch| {
        let kind = if setup.smoke {
            FixtureKind::Synth10kSmoke
        } else {
            FixtureKind::Synth10k
        };
        let (worktree, _) = fixture_registry::ensure_cached(setup.git, kind, setup.offline)?;
        let bare = scratch.join("remote.git");
        worktree_to_bare(setup.git, &worktree, &bare)?;
        let meta = install_network_meta(setup.git, &bare, setup.smoke)?;
        let remote_url = file_url(&bare)?;
        std::env::set_var("GRIT_BENCH_DAILY_NETWORK_BARE", &bare);
        Ok(ctx_with_remote(
            scratch.to_path_buf(),
            setup,
            DailyRemoteState {
                bare,
                meta,
                remote_url,
                grit_http: None,
                git_http: None,
            },
        ))
    })
}

pub fn setup_many_refs_file_remote(setup: &DailySetup<'_>) -> Result<DailyHeadlineContext> {
    with_remote_scratch("many-refs-ls", |scratch| {
        let kind = if setup.smoke {
            FixtureKind::ManyRefsSmoke
        } else {
            FixtureKind::ManyRefs
        };
        let (worktree, _) = fixture_registry::ensure_cached(setup.git, kind, setup.offline)?;
        let bare = scratch.join("remote.git");
        worktree_to_bare(setup.git, &worktree, &bare)?;
        let meta = install_network_meta(setup.git, &bare, setup.smoke)?;
        let remote_url = file_url(&bare)?;
        std::env::set_var("GRIT_BENCH_DAILY_NETWORK_BARE", &bare);
        Ok(ctx_with_remote(
            scratch.to_path_buf(),
            setup,
            DailyRemoteState {
                bare,
                meta,
                remote_url,
                grit_http: None,
                git_http: None,
            },
        ))
    })
}

pub fn setup_git_git_grit_http_remote(setup: &DailySetup<'_>) -> Result<DailyHeadlineContext> {
    setup_git_git_grit_http_remote_label(setup, "git-git-http")
}

pub fn setup_git_git_grit_http_fetch_remote(
    setup: &DailySetup<'_>,
) -> Result<DailyHeadlineContext> {
    setup_git_git_grit_http_remote_label(setup, "git-git-http-fetch")
}

pub fn setup_git_git_grit_http_push_remote(setup: &DailySetup<'_>) -> Result<DailyHeadlineContext> {
    setup_git_git_grit_http_remote_label(setup, "git-git-http-push")
}

fn setup_git_git_grit_http_remote_label(
    setup: &DailySetup<'_>,
    label: &str,
) -> Result<DailyHeadlineContext> {
    with_remote_scratch(label, |scratch| {
        let bare = scratch.join("remote.git");
        copy_cached_bare(setup.git, setup, FixtureKind::GitGit, &bare)?;
        let meta = install_network_meta(setup.git, &bare, setup.smoke)?;
        clear_network_sidecars(&meta);
        let http_root = network_http_root(&bare);
        install_http_repo(setup.git, &bare, &http_root)?;
        let grit_http = GritHttpServer::start(setup.grit, None, http_root.clone())?;
        let remote_url = grit_http.url(REPO_NAME);
        std::env::set_var("GRIT_BENCH_DAILY_NETWORK_BARE", &bare);
        let remote = DailyRemoteState {
            bare,
            meta: meta.clone(),
            remote_url: remote_url.clone(),
            grit_http: Some(grit_http),
            git_http: None,
        };
        let client = ensure_client(setup.git, &remote)?;
        configure_remote(setup.git, &client, &remote_url, &meta.default_branch)?;
        Ok(ctx_with_remote(client, setup, remote))
    })
}

pub fn setup_synth10k_grit_http_remote(setup: &DailySetup<'_>) -> Result<DailyHeadlineContext> {
    with_remote_scratch("synth-10k-http-clone", |scratch| {
        let kind = if setup.smoke {
            FixtureKind::Synth10kSmoke
        } else {
            FixtureKind::Synth10k
        };
        let (worktree, _) = fixture_registry::ensure_cached(setup.git, kind, setup.offline)?;
        let bare = scratch.join("remote.git");
        worktree_to_bare(setup.git, &worktree, &bare)?;
        let meta = install_network_meta(setup.git, &bare, setup.smoke)?;
        let http_root = network_http_root(&bare);
        install_http_repo(setup.git, &bare, &http_root)?;
        let grit_http = GritHttpServer::start(setup.grit, None, http_root)?;
        let remote_url = grit_http.url(REPO_NAME);
        std::env::set_var("GRIT_BENCH_DAILY_NETWORK_BARE", &bare);
        Ok(ctx_with_remote(
            scratch.to_path_buf(),
            setup,
            DailyRemoteState {
                bare,
                meta,
                remote_url,
                grit_http: Some(grit_http),
                git_http: None,
            },
        ))
    })
}

fn remote_state(ctx: &DailyHeadlineContext) -> Result<&DailyRemoteState> {
    ctx.remote.as_ref().context("daily remote state")
}

pub fn verify_fetch_incremental(ctx: &DailyHeadlineContext, cwd: &Path) -> Result<()> {
    let remote = remote_state(ctx)?;
    configure_client_remote(&ctx.git, cwd, remote)?;
    prepare_network_fetch_incr(&ctx.git, &remote.bare)?;
    run_git(&ctx.git, cwd, &["fetch", "-q", "origin"])?;
    verify_client_fetch_tip(&ctx.git, &remote.bare, cwd, &remote.meta.default_branch)?;
    prepare_network_fetch_incr(&ctx.git, &remote.bare)?;
    run_grit(&ctx.grit, cwd, &["fetch", "origin"])?;
    verify_client_fetch_tip(&ctx.git, &remote.bare, cwd, &remote.meta.default_branch)?;
    Ok(())
}

pub fn verify_fetch_noop(ctx: &DailyHeadlineContext, cwd: &Path) -> Result<()> {
    let remote = remote_state(ctx)?;
    configure_client_remote(&ctx.git, cwd, remote)?;
    prepare_network_fetch_noop(&ctx.git, &remote.bare)?;
    run_git(&ctx.git, cwd, &["fetch", "-q", "origin"])?;
    let git_tip = ref_oid(
        &ctx.git,
        cwd,
        &format!("refs/remotes/origin/{}", remote.meta.default_branch),
    )?;
    prepare_network_fetch_noop(&ctx.git, &remote.bare)?;
    run_grit(&ctx.grit, cwd, &["fetch", "origin"])?;
    let grit_tip = ref_oid(
        &ctx.git,
        cwd,
        &format!("refs/remotes/origin/{}", remote.meta.default_branch),
    )?;
    if git_tip != grit_tip {
        bail!("fetch noop tip mismatch git {git_tip} vs grit {grit_tip}");
    }
    Ok(())
}

pub fn verify_pull_fast_forward(ctx: &DailyHeadlineContext, cwd: &Path) -> Result<()> {
    let remote = remote_state(ctx)?;
    configure_client_remote(&ctx.git, cwd, remote)?;
    prepare_network_fetch_incr(&ctx.git, &remote.bare)?;
    run_git(
        &ctx.git,
        cwd,
        &[
            "pull",
            "--ff-only",
            "-q",
            "origin",
            &remote.meta.default_branch,
        ],
    )?;
    let git_head = ref_oid(&ctx.git, cwd, "HEAD")?;
    let bare_tip = ref_oid(
        &ctx.git,
        &remote.bare,
        &format!("refs/heads/{}", remote.meta.default_branch),
    )?;
    if git_head != bare_tip {
        bail!("git pull HEAD {git_head} != bare {bare_tip}");
    }
    prepare_network_fetch_incr(&ctx.git, &remote.bare)?;
    let bare_tip = ref_oid(
        &ctx.git,
        &remote.bare,
        &format!("refs/heads/{}", remote.meta.default_branch),
    )?;
    run_grit(&ctx.grit, cwd, &["pull"])?;
    let grit_head = ref_oid(&ctx.git, cwd, "HEAD")?;
    if grit_head != bare_tip {
        bail!("grit pull HEAD {grit_head} != bare {bare_tip}");
    }
    Ok(())
}

pub fn verify_push_force_with_lease(ctx: &DailyHeadlineContext, cwd: &Path) -> Result<()> {
    let remote = remote_state(ctx)?;
    configure_client_remote(&ctx.git, cwd, remote)?;
    prepare_network_force_with_lease(&ctx.git, &remote.bare)?;
    run_git(
        &ctx.git,
        cwd,
        &[
            "push",
            "--force-with-lease",
            "-q",
            "origin",
            remote.meta.default_branch.as_str(),
        ],
    )?;
    let bare_after_git = ref_oid(
        &ctx.git,
        &remote.bare,
        &format!("refs/heads/{}", remote.meta.default_branch),
    )?;
    let client_after_git = ref_oid(
        &ctx.git,
        cwd,
        &format!("refs/heads/{}", remote.meta.default_branch),
    )?;
    if bare_after_git != client_after_git {
        bail!("git push --force-with-lease: bare {bare_after_git} != client {client_after_git}");
    }
    prepare_network_force_with_lease(&ctx.git, &remote.bare)?;
    run_grit(&ctx.grit, cwd, &["push", "--force-with-lease"])?;
    let bare_after_grit = ref_oid(
        &ctx.git,
        &remote.bare,
        &format!("refs/heads/{}", remote.meta.default_branch),
    )?;
    let client_after_grit = ref_oid(
        &ctx.git,
        cwd,
        &format!("refs/heads/{}", remote.meta.default_branch),
    )?;
    if bare_after_grit != client_after_grit {
        bail!("grit push --force-with-lease: bare {bare_after_grit} != client {client_after_grit}");
    }
    Ok(())
}

pub fn verify_push_incremental(ctx: &DailyHeadlineContext, cwd: &Path) -> Result<()> {
    let remote = remote_state(ctx)?;
    configure_client_remote(&ctx.git, cwd, remote)?;
    prepare_network_push(&ctx.git, &remote.bare)?;
    run_git(
        &ctx.git,
        cwd,
        &["push", "-q", "origin", remote.meta.default_branch.as_str()],
    )?;
    let bare_after_git = ref_oid(
        &ctx.git,
        &remote.bare,
        &format!("refs/heads/{}", remote.meta.default_branch),
    )?;
    let client_after_git = ref_oid(
        &ctx.git,
        cwd,
        &format!("refs/heads/{}", remote.meta.default_branch),
    )?;
    if bare_after_git != client_after_git {
        bail!("git push: bare {bare_after_git} != client {client_after_git}");
    }
    prepare_network_push(&ctx.git, &remote.bare)?;
    run_grit(&ctx.grit, cwd, &["push"])?;
    let bare_after_grit = ref_oid(
        &ctx.git,
        &remote.bare,
        &format!("refs/heads/{}", remote.meta.default_branch),
    )?;
    let client_after_grit = ref_oid(
        &ctx.git,
        cwd,
        &format!("refs/heads/{}", remote.meta.default_branch),
    )?;
    if bare_after_grit != client_after_grit {
        bail!("grit push: bare {bare_after_grit} != client {client_after_grit}");
    }
    Ok(())
}

pub fn verify_clone_local(ctx: &DailyHeadlineContext, cwd: &Path) -> Result<()> {
    let remote = remote_state(ctx)?;
    let dest = remote.meta.clone_dest.clone();
    let dest_name = dest
        .file_name()
        .and_then(|s| s.to_str())
        .context("clone dest name")?;
    prepare_network_clone(&remote.bare)?;
    run_git(
        &ctx.git,
        cwd,
        &["clone", "-q", remote.remote_url.as_str(), dest_name],
    )?;
    verify_clone_against_bare(&ctx.git, &remote.bare, &dest, &remote.meta.default_branch)?;
    prepare_network_clone(&remote.bare)?;
    run_grit(
        &ctx.grit,
        cwd,
        &["clone", remote.remote_url.as_str(), dest_name],
    )?;
    verify_clone_against_bare(&ctx.git, &remote.bare, &dest, &remote.meta.default_branch)?;
    Ok(())
}

pub fn verify_clone_http(ctx: &DailyHeadlineContext, cwd: &Path) -> Result<()> {
    verify_clone_local(ctx, cwd)
}

pub fn verify_fetch_incremental_http(ctx: &DailyHeadlineContext, cwd: &Path) -> Result<()> {
    let remote = remote_state(ctx)?;
    configure_client_remote(&ctx.git, cwd, remote)?;
    prepare_network_fetch_incr(&ctx.git, &remote.bare)?;
    install_http_repo(&ctx.git, &remote.bare, &network_http_root(&remote.bare))?;
    run_git(&ctx.git, cwd, &["fetch", "-q", "origin"])?;
    verify_client_fetch_tip(&ctx.git, &remote.bare, cwd, &remote.meta.default_branch)?;
    prepare_network_fetch_incr(&ctx.git, &remote.bare)?;
    install_http_repo(&ctx.git, &remote.bare, &network_http_root(&remote.bare))?;
    run_grit(&ctx.grit, cwd, &["fetch", "origin"])?;
    verify_client_fetch_tip(&ctx.git, &remote.bare, cwd, &remote.meta.default_branch)?;
    Ok(())
}

pub fn verify_push_incremental_http(ctx: &DailyHeadlineContext, cwd: &Path) -> Result<()> {
    let remote = remote_state(ctx)?;
    configure_client_remote(&ctx.git, cwd, remote)?;
    let http_root = network_http_root(&remote.bare);
    let http_bare = http_root.join(REPO_NAME);
    let branch_ref = format!("refs/heads/{}", remote.meta.default_branch);

    prepare_network_push(&ctx.git, &remote.bare)?;
    install_http_repo(&ctx.git, &remote.bare, &http_root)?;
    let client_tip_git = ref_oid(&ctx.git, cwd, &branch_ref)?;
    run_git(
        &ctx.git,
        cwd,
        &["push", "-q", "origin", remote.meta.default_branch.as_str()],
    )?;
    let http_tip_after_git = ref_oid(&ctx.git, &http_bare, &branch_ref)?;
    if http_tip_after_git != client_tip_git {
        bail!("http push after git: served remote {http_tip_after_git} != client {client_tip_git}");
    }

    prepare_network_push(&ctx.git, &remote.bare)?;
    install_http_repo(&ctx.git, &remote.bare, &http_root)?;
    configure_client_remote(&ctx.git, cwd, remote)?;
    let client_tip_grit = ref_oid(&ctx.git, cwd, &branch_ref)?;
    run_grit(&ctx.grit, cwd, &["push"])?;
    let http_tip_after_grit = ref_oid(&ctx.git, &http_bare, &branch_ref)?;
    if http_tip_after_grit != client_tip_grit {
        bail!(
            "http push after grit: served remote {http_tip_after_grit} != client {client_tip_grit}"
        );
    }
    if http_tip_after_git != http_tip_after_grit {
        bail!(
            "http push parity: git remote tip {http_tip_after_git} != grit remote tip {http_tip_after_grit}"
        );
    }
    Ok(())
}

fn parse_ls_remote_map(text: &str) -> Result<Vec<(String, String)>> {
    let mut out = Vec::new();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let Some((oid, name)) = line.split_once('\t').or_else(|| line.split_once(' ')) else {
            continue;
        };
        out.push((oid.to_string(), name.to_string()));
    }
    out.sort_by(|a, b| a.1.cmp(&b.1));
    Ok(out)
}

pub fn verify_ls_remote_all(ctx: &DailyHeadlineContext, cwd: &Path) -> Result<()> {
    let remote = remote_state(ctx)?;
    let mut git_cmd = Command::new(&ctx.git);
    git_cmd
        .args(["ls-remote", remote.remote_url.as_str()])
        .current_dir(cwd);
    git_env(&mut git_cmd);
    let git_out = git_cmd.output().context("git ls-remote")?;
    if !git_out.status.success() {
        bail!(
            "git ls-remote failed: {}",
            String::from_utf8_lossy(&git_out.stderr).trim()
        );
    }
    let git_map = parse_ls_remote_map(&String::from_utf8_lossy(&git_out.stdout))?;

    let mut grit_cmd = Command::new(&ctx.grit);
    grit_cmd
        .args(["remote", "refs", remote.remote_url.as_str()])
        .current_dir(cwd);
    git_env(&mut grit_cmd);
    let grit_out = grit_cmd.output().context("grit remote refs")?;
    if !grit_out.status.success() {
        bail!(
            "grit remote refs failed: {}",
            String::from_utf8_lossy(&grit_out.stderr).trim()
        );
    }
    let grit_map = parse_ls_remote_map(&String::from_utf8_lossy(&grit_out.stdout))?;
    if git_map != grit_map {
        bail!(
            "ls-remote ref lists differ (git {} vs grit {})",
            git_map.len(),
            grit_map.len()
        );
    }
    Ok(())
}

pub fn clone_bench_script(tool: &Path, url: &str, dest: &Path) -> String {
    let tool_q = shell_quote(&tool.to_string_lossy());
    let url_q = shell_quote(url);
    let dest_q = shell_quote(&dest.to_string_lossy());
    let dest_name = dest.file_name().and_then(|s| s.to_str()).unwrap_or("clone");
    format!("rm -rf {dest_q} && {tool_q} clone -q {url_q} {dest_name}")
}

/// Ensure the smart HTTP server is running and the client remote URL is current.
pub fn refresh_grit_http_server(ctx: &mut DailyHeadlineContext) -> Result<()> {
    let Some(remote) = ctx.remote.as_mut() else {
        return Ok(());
    };
    if !remote.remote_url.starts_with("http://") {
        return Ok(());
    }
    let http_root = network_http_root(&remote.bare);
    install_http_repo(&ctx.git, &remote.bare, &http_root)?;
    let server = GritHttpServer::start(&ctx.grit, None, http_root)?;
    remote.remote_url = server.url(REPO_NAME);
    remote.grit_http = Some(server);
    if ctx.repo.join(".git").is_dir() || ctx.repo.join("HEAD").is_file() {
        configure_client_remote(&ctx.git, &ctx.repo, remote)?;
    }
    Ok(())
}

pub fn clone_bench_script_grit(tool: &Path, url: &str, dest: &Path) -> String {
    let tool_q = shell_quote(&tool.to_string_lossy());
    let url_q = shell_quote(url);
    let dest_q = shell_quote(&dest.to_string_lossy());
    let dest_name = dest.file_name().and_then(|s| s.to_str()).unwrap_or("clone");
    format!("rm -rf {dest_q} && {tool_q} clone {url_q} {dest_name}")
}

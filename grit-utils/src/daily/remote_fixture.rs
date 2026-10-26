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
    ensure_campaign_bare_network_meta, install_http_repo, meta_path, NetworkBenchMeta, REPO_NAME,
};
use crate::network_http::{GitHttpBackendServer, GritHttpServer};
use crate::network_suite::{
    assert_push_target_lacks_client_tip, configure_remote, ensure_network_client_repo,
    git_object_exists, network_http_root, prepare_network_clone, prepare_network_fetch_incr,
    prepare_network_fetch_noop, prepare_network_force_with_lease, prepare_network_pull_rebase,
    prepare_network_push,
};
use crate::network_verify::{
    verify_client_fetch_tip, verify_clone_against_bare, verify_shallow_depth_one_clone,
};
use crate::shell::shell_quote;

fn ctx_with_remote(
    repo: PathBuf,
    setup: &DailySetup<'_>,
    remote: DailyRemoteState,
) -> DailyHeadlineContext {
    let mut ctx = DailyHeadlineContext::new(repo, setup.git, setup.grit, setup.bench, setup.smoke);
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

fn tree_oid(git: &Path, repo: &Path, rev_expr: &str) -> Result<String> {
    ref_oid(git, repo, rev_expr)
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

fn is_ancestor(git: &Path, repo: &Path, ancestor: &str, descendant: &str) -> Result<bool> {
    let mut cmd = Command::new(git);
    cmd.args(["merge-base", "--is-ancestor", ancestor, descendant])
        .current_dir(repo);
    git_env(&mut cmd);
    let out = cmd.output().context("git merge-base --is-ancestor")?;
    match out.status.code() {
        Some(0) => Ok(true),
        Some(1) => Ok(false),
        _ => bail!(
            "git merge-base --is-ancestor failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        ),
    }
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

/// Serve `bare` with git's own smart HTTP server (`git http-backend`), so clients fetch, pull,
/// push and clone end to end over HTTP rather than through the `file://` shortcut. The bare
/// stays the one prepare and the checks work on; the server lives as long as the scenario.
fn serve_over_http(git: &Path, bare: &Path) -> Result<(String, GitHttpBackendServer)> {
    let root = bare.parent().context("bare repo parent")?.to_path_buf();
    let name = bare
        .file_name()
        .and_then(|n| n.to_str())
        .context("bare repo name")?
        .to_string();
    let server = GitHttpBackendServer::start(git, root)?;
    let url = server.url(&name);
    Ok((url, server))
}

fn ensure_client(git: &Path, remote: &DailyRemoteState) -> Result<PathBuf> {
    ensure_network_client_repo(git, &remote.bare)
}

fn configure_client_remote(git: &Path, client: &Path, remote: &DailyRemoteState) -> Result<()> {
    configure_remote(git, client, &remote.remote_url, &remote.meta.default_branch)
}

pub fn setup_synth10k_pull_rebase_remote(setup: &DailySetup<'_>) -> Result<DailyHeadlineContext> {
    with_remote_scratch("synth10k-pull-rebase", |scratch| {
        let kind = if setup.smoke {
            FixtureKind::Synth10kSmoke
        } else {
            FixtureKind::Synth10k
        };
        let (worktree, _) = fixture_registry::ensure_cached(setup.git, kind, setup.offline)?;
        let bare = scratch.join("remote.git");
        worktree_to_bare(setup.git, &worktree, &bare)?;
        let meta = install_network_meta(setup.git, &bare, setup.smoke)?;
        clear_network_sidecars(&meta);
        let (remote_url, git_http) = serve_over_http(setup.git, &bare)?;
        std::env::set_var("GRIT_BENCH_DAILY_NETWORK_BARE", &bare);
        let remote = DailyRemoteState {
            bare: bare.clone(),
            meta: meta.clone(),
            remote_url: remote_url.clone(),
            grit_http: None,
            git_http: Some(git_http),
        };
        let client = ensure_client(setup.git, &remote)?;
        configure_remote(setup.git, &client, &remote_url, &meta.default_branch)?;
        Ok(ctx_with_remote(client, setup, remote))
    })
}

fn configure_blobless_upload_pack(git: &Path, bare: &Path) -> Result<()> {
    run_git(git, bare, &["config", "uploadpack.allowFilter", "true"])?;
    run_git(
        git,
        bare,
        &["config", "uploadpack.allowAnySHA1InWant", "true"],
    )?;
    Ok(())
}

pub fn setup_git_git_remote_blobless(setup: &DailySetup<'_>) -> Result<DailyHeadlineContext> {
    let ctx = setup_git_git_remote(setup)?;
    let remote = ctx.remote.as_ref().context("remote state")?;
    configure_blobless_upload_pack(setup.git, &remote.bare)?;
    Ok(ctx)
}

pub fn setup_git_git_remote(setup: &DailySetup<'_>) -> Result<DailyHeadlineContext> {
    with_remote_scratch("git-git", |scratch| {
        let bare = scratch.join("remote.git");
        copy_cached_bare(setup.git, setup, FixtureKind::GitGit, &bare)?;
        let meta = install_network_meta(setup.git, &bare, setup.smoke)?;
        clear_network_sidecars(&meta);
        let (remote_url, git_http) = serve_over_http(setup.git, &bare)?;
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
                git_http: Some(git_http),
            },
        ))
    })
}

/// The git/git bare served by `git http-backend`, for full clones of a large real pack.
pub fn setup_git_git_clone_remote(setup: &DailySetup<'_>) -> Result<DailyHeadlineContext> {
    with_remote_scratch("git-git-clone", |scratch| {
        let bare = scratch.join("remote.git");
        copy_cached_bare(setup.git, setup, FixtureKind::GitGit, &bare)?;
        let meta = install_network_meta(setup.git, &bare, setup.smoke)?;
        clear_network_sidecars(&meta);
        let (remote_url, git_http) = serve_over_http(setup.git, &bare)?;
        std::env::set_var("GRIT_BENCH_DAILY_NETWORK_BARE", &bare);
        Ok(ctx_with_remote(
            scratch.to_path_buf(),
            setup,
            DailyRemoteState {
                bare,
                meta,
                remote_url,
                grit_http: None,
                git_http: Some(git_http),
            },
        ))
    })
}

pub fn setup_synth10k_remote(setup: &DailySetup<'_>) -> Result<DailyHeadlineContext> {
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
        let (remote_url, git_http) = serve_over_http(setup.git, &bare)?;
        std::env::set_var("GRIT_BENCH_DAILY_NETWORK_BARE", &bare);
        Ok(ctx_with_remote(
            scratch.to_path_buf(),
            setup,
            DailyRemoteState {
                bare,
                meta,
                remote_url,
                grit_http: None,
                git_http: Some(git_http),
            },
        ))
    })
}

/// Bare copy of the 100k-file synthetic fixture, cloned by plain path: both tools then link the
/// pack locally (git skips `upload-pack`), so the timed clone is dominated by the checkout.
pub fn setup_synth100k_path_remote(setup: &DailySetup<'_>) -> Result<DailyHeadlineContext> {
    with_remote_scratch("synth-100k-path-clone", |scratch| {
        let kind = if setup.smoke {
            FixtureKind::Synth100kSmoke
        } else {
            FixtureKind::Synth100k
        };
        let (worktree, _) = fixture_registry::ensure_cached(setup.git, kind, setup.offline)?;
        let bare = scratch.join("remote.git");
        worktree_to_bare(setup.git, &worktree, &bare)?;
        let meta = install_network_meta(setup.git, &bare, setup.smoke)?;
        let remote_url = bare.display().to_string();
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

pub fn setup_remote_remove_many_refs(setup: &DailySetup<'_>) -> Result<DailyHeadlineContext> {
    let mut ctx = setup_many_refs_remote(setup)?;
    let remote = ctx.remote.as_ref().context("remote state")?;
    let client = ensure_client(setup.git, remote)?;
    configure_client_remote(setup.git, &client, remote)?;
    run_git(setup.git, &client, &["fetch", "-q", "origin"])?;
    run_git(setup.git, &client, &["pack-refs", "--all"])?;
    crate::worktree_fixture::set_worktree_bench_cwd(&client)?;
    crate::worktree_fixture::record_worktree_bench_base(setup.git, &client)?;
    ctx.repo = client;
    Ok(ctx)
}

pub fn setup_many_refs_remote_show(setup: &DailySetup<'_>) -> Result<DailyHeadlineContext> {
    with_remote_scratch("many-refs-show", |scratch| {
        let kind = if setup.smoke {
            FixtureKind::ManyRefsSmoke
        } else {
            FixtureKind::ManyRefs
        };
        let (worktree, _) = fixture_registry::ensure_cached(setup.git, kind, setup.offline)?;
        let bare = scratch.join("remote.git");
        worktree_to_bare(setup.git, &worktree, &bare)?;
        let meta = install_network_meta(setup.git, &bare, setup.smoke)?;
        let (remote_url, git_http) = serve_over_http(setup.git, &bare)?;
        std::env::set_var("GRIT_BENCH_DAILY_NETWORK_BARE", &bare);
        let remote = DailyRemoteState {
            bare: bare.clone(),
            meta: meta.clone(),
            remote_url: remote_url.clone(),
            grit_http: None,
            git_http: Some(git_http),
        };
        let client = ensure_client(setup.git, &remote)?;
        configure_client_remote(setup.git, &client, &remote)?;
        Ok(ctx_with_remote(client, setup, remote))
    })
}

pub fn setup_many_refs_remote(setup: &DailySetup<'_>) -> Result<DailyHeadlineContext> {
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
        let (remote_url, git_http) = serve_over_http(setup.git, &bare)?;
        std::env::set_var("GRIT_BENCH_DAILY_NETWORK_BARE", &bare);
        Ok(ctx_with_remote(
            scratch.to_path_buf(),
            setup,
            DailyRemoteState {
                bare,
                meta,
                remote_url,
                grit_http: None,
                git_http: Some(git_http),
            },
        ))
    })
}

/// Packed 3,000-branch / 2,000-tag local transport fixture from issue #2037.
pub fn setup_local_fetch_many_refs(setup: &DailySetup<'_>) -> Result<DailyHeadlineContext> {
    use std::io::Write;
    use std::process::Stdio;

    with_remote_scratch("local-fetch-many-refs", |scratch| {
        std::fs::create_dir_all(scratch)?;
        run_git(setup.git, scratch, &["init", "-q", "-b", "main", "seed"])?;
        let seed = scratch.join("seed");
        std::fs::write(seed.join("f"), "base\n")?;
        run_git(setup.git, &seed, &["add", "f"])?;
        run_git(setup.git, &seed, &["commit", "-qm", "base"])?;
        run_git(
            setup.git,
            scratch,
            &["clone", "-q", "--bare", "seed", "remote.git"],
        )?;
        let bare = scratch.join("remote.git");
        let oid = ref_oid(setup.git, &bare, "HEAD")?;
        let mut cmd = Command::new(setup.git);
        cmd.args(["update-ref", "--stdin"])
            .current_dir(&bare)
            .stdin(Stdio::piped())
            .stdout(Stdio::null());
        git_env(&mut cmd);
        let mut child = cmd.spawn().context("create many refs")?;
        {
            let mut input = child.stdin.take().context("update-ref stdin")?;
            for i in 0..3000 {
                writeln!(input, "update refs/heads/team/{i:04} {oid}")?;
            }
            for i in 0..2000 {
                writeln!(input, "update refs/tags/v{i:04} {oid}")?;
            }
        }
        if !child.wait()?.success() {
            bail!("creating many-ref fixture failed");
        }
        run_git(setup.git, &bare, &["pack-refs", "--all"])?;
        run_git(setup.git, scratch, &["clone", "-q", "remote.git", "client"])?;
        Ok(DailyHeadlineContext::new(
            scratch.join("client"),
            setup.git,
            setup.grit,
            setup.bench,
            setup.smoke,
        ))
    })
}

/// Check both tag modes leave the fixture's refs unchanged and pass system Git fsck.
/// Returns an error if either client fails or changes a ref.
pub fn verify_local_fetch_many_refs(ctx: &DailyHeadlineContext) -> Result<()> {
    let before = bare_clone_for_each_ref(&ctx.git, &ctx.repo)?;
    for args in [
        ["fetch", "origin"].as_slice(),
        ["fetch", "--no-tags", "origin"].as_slice(),
    ] {
        run_git(&ctx.git, &ctx.repo, args)?;
        run_grit(&ctx.grit, &ctx.repo, args)?;
    }
    let after = bare_clone_for_each_ref(&ctx.git, &ctx.repo)?;
    if before != after {
        bail!("no-op local fetch changed refs");
    }
    run_git(&ctx.git, &ctx.repo, &["fsck", "--strict"])
}

/// Client repo with ~10k packed `refs/remotes/origin/*` after fetch from the many-refs bare remote.
pub fn setup_remote_rename_many_refs(setup: &DailySetup<'_>) -> Result<DailyHeadlineContext> {
    with_remote_scratch("remote-rename-many-refs", |scratch| {
        let kind = if setup.smoke {
            FixtureKind::ManyRefsSmoke
        } else {
            FixtureKind::ManyRefs
        };
        let (worktree, _) = fixture_registry::ensure_cached(setup.git, kind, setup.offline)?;
        let bare = scratch.join("remote.git");
        worktree_to_bare(setup.git, &worktree, &bare)?;
        let meta = install_network_meta(setup.git, &bare, setup.smoke)?;
        let (remote_url, git_http) = serve_over_http(setup.git, &bare)?;
        std::env::set_var("GRIT_BENCH_DAILY_NETWORK_BARE", &bare);
        let remote = DailyRemoteState {
            bare: bare.clone(),
            meta: meta.clone(),
            remote_url: remote_url.clone(),
            grit_http: None,
            git_http: Some(git_http),
        };
        let client = ensure_client(setup.git, &remote)?;
        configure_remote(setup.git, &client, &remote_url, &meta.default_branch)?;
        run_git(setup.git, &client, &["fetch", "-q", "origin"])?;
        run_git(setup.git, &client, &["pack-refs", "--all"])?;
        Ok(ctx_with_remote(client, setup, remote))
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

pub fn verify_fetch_one_branch(ctx: &DailyHeadlineContext, cwd: &Path) -> Result<()> {
    let remote = remote_state(ctx)?;
    configure_client_remote(&ctx.git, cwd, remote)?;
    let branch = remote.meta.default_branch.as_str();
    prepare_network_fetch_incr(&ctx.git, &remote.bare)?;
    run_git(&ctx.git, cwd, &["fetch", "-q", "origin", branch])?;
    verify_client_fetch_tip(&ctx.git, &remote.bare, cwd, branch)?;
    prepare_network_fetch_incr(&ctx.git, &remote.bare)?;
    run_grit(&ctx.grit, cwd, &["fetch", "origin", branch])?;
    verify_client_fetch_tip(&ctx.git, &remote.bare, cwd, branch)?;
    Ok(())
}

fn git_pull_check_ff_possible(git: &Path, cwd: &Path, upstream_ref: &str) -> Result<bool> {
    let head = ref_oid(git, cwd, "HEAD")?;
    let upstream = ref_oid(git, cwd, upstream_ref)?;
    if head == upstream {
        return Ok(true);
    }
    is_ancestor(git, cwd, head.as_str(), upstream_ref)
}

pub fn verify_pull_check(ctx: &DailyHeadlineContext, cwd: &Path) -> Result<()> {
    let remote = remote_state(ctx)?;
    configure_client_remote(&ctx.git, cwd, remote)?;
    let branch = remote.meta.default_branch.as_str();
    let tracking = format!("origin/{branch}");
    prepare_network_fetch_incr(&ctx.git, &remote.bare)?;
    run_git(&ctx.git, cwd, &["fetch", "-q", "origin", branch])?;
    let head_before = ref_oid(&ctx.git, cwd, "HEAD")?;
    if !git_pull_check_ff_possible(&ctx.git, cwd, &tracking)? {
        bail!("git baseline: expected fast-forward to be possible after fetch");
    }
    if ref_oid(&ctx.git, cwd, "HEAD")? != head_before {
        bail!("git fetch must not move HEAD");
    }
    prepare_network_fetch_incr(&ctx.git, &remote.bare)?;
    run_git(&ctx.git, cwd, &["fetch", "-q", "origin", branch])?;
    let head_before = ref_oid(&ctx.git, cwd, "HEAD")?;
    let out = std::process::Command::new(&ctx.grit)
        .args(["pull", "--check", "origin", branch])
        .current_dir(cwd)
        .output()
        .context("grit pull --check")?;
    if !out.status.success() {
        bail!(
            "grit pull --check should exit 0 when ff is possible: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }
    if ref_oid(&ctx.git, cwd, "HEAD")? != head_before {
        bail!("grit pull --check must not move HEAD");
    }
    Ok(())
}

pub fn verify_fetch_dry_run(ctx: &DailyHeadlineContext, cwd: &Path) -> Result<()> {
    let remote = remote_state(ctx)?;
    configure_client_remote(&ctx.git, cwd, remote)?;
    let branch = remote.meta.default_branch.as_str();
    let tracking = format!("refs/remotes/origin/{branch}");
    prepare_network_fetch_incr(&ctx.git, &remote.bare)?;
    run_git(&ctx.git, cwd, &["fetch", "-q", "origin", branch])?;
    prepare_network_fetch_incr(&ctx.git, &remote.bare)?;
    let before = ref_oid(&ctx.git, cwd, &tracking)?;
    run_git(
        &ctx.git,
        cwd,
        &["fetch", "--dry-run", "-q", "origin", branch],
    )?;
    let after_git = ref_oid(&ctx.git, cwd, &tracking)?;
    if before != after_git {
        bail!("git fetch --dry-run must not move {tracking}");
    }
    prepare_network_fetch_incr(&ctx.git, &remote.bare)?;
    run_git(&ctx.git, cwd, &["fetch", "-q", "origin", branch])?;
    prepare_network_fetch_incr(&ctx.git, &remote.bare)?;
    let before_grit = ref_oid(&ctx.git, cwd, &tracking)?;
    run_grit(&ctx.grit, cwd, &["fetch", "--dry-run", "origin", branch])?;
    let after_grit = ref_oid(&ctx.git, cwd, &tracking)?;
    if before_grit != after_grit {
        bail!("grit fetch --dry-run must not move {tracking}");
    }
    Ok(())
}

pub fn verify_fetch_deepen(ctx: &DailyHeadlineContext, cwd: &Path) -> Result<()> {
    let remote = remote_state(ctx)?;
    configure_client_remote(&ctx.git, cwd, remote)?;
    let branch = remote.meta.default_branch.as_str();
    run_git(
        &ctx.git,
        cwd,
        &["fetch", "--depth", "1", "-q", "origin", branch],
    )?;
    run_grit(&ctx.grit, cwd, &["fetch", "--depth", "1", "origin", branch])?;
    prepare_network_fetch_incr(&ctx.git, &remote.bare)?;
    run_git(
        &ctx.git,
        cwd,
        &["fetch", "--deepen", "50", "-q", "origin", branch],
    )?;
    prepare_network_fetch_incr(&ctx.git, &remote.bare)?;
    run_grit(
        &ctx.grit,
        cwd,
        &["fetch", "--deepen", "50", "origin", branch],
    )?;
    verify_client_fetch_tip(&ctx.git, &remote.bare, cwd, branch)?;
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

pub fn verify_pull_rebase(ctx: &DailyHeadlineContext, cwd: &Path) -> Result<()> {
    let remote = remote_state(ctx)?;
    configure_client_remote(&ctx.git, cwd, remote)?;
    prepare_network_pull_rebase(&ctx.git, &remote.bare)?;
    let starting_head = ref_oid(&ctx.git, cwd, "HEAD")?;
    run_git(
        &ctx.git,
        cwd,
        &[
            "pull",
            "-q",
            "--rebase",
            "origin",
            &remote.meta.default_branch,
        ],
    )?;
    let git_head = ref_oid(&ctx.git, cwd, "HEAD")?;
    let fork = remote.meta.fetch_base_oid.clone();
    if !is_ancestor(&ctx.git, cwd, &fork, &git_head)? {
        bail!("git pull --rebase HEAD {git_head} is not descended from fork {fork}");
    }
    if git_head == fork {
        bail!("git pull --rebase should replay local commits on top of upstream");
    }
    run_git(&ctx.git, cwd, &["reset", "--hard", &starting_head])?;
    run_grit(&ctx.grit, cwd, &["pull", "--rebase"])?;
    let grit_head = ref_oid(&ctx.git, cwd, "HEAD")?;
    if !is_ancestor(&ctx.git, cwd, &fork, &grit_head)? {
        bail!("grit pull --rebase HEAD {grit_head} is not descended from fork {fork}");
    }
    if grit_head == fork {
        bail!("grit pull --rebase should replay local commits on top of upstream");
    }
    let git_tree = tree_oid(&ctx.git, cwd, &format!("{git_head}^{{tree}}"))?;
    let grit_tree = tree_oid(&ctx.git, cwd, &format!("{grit_head}^{{tree}}"))?;
    if git_tree != grit_tree {
        bail!("pull rebase tree mismatch git {git_tree} vs grit {grit_tree} (git HEAD {git_head}, grit HEAD {grit_head})");
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

pub fn verify_push_refspec(ctx: &DailyHeadlineContext, cwd: &Path) -> Result<()> {
    let remote = remote_state(ctx)?;
    configure_client_remote(&ctx.git, cwd, remote)?;
    prepare_network_push(&ctx.git, &remote.bare)?;
    run_git(
        &ctx.git,
        cwd,
        &["push", "-q", "origin", "HEAD:refs/heads/bench"],
    )?;
    let git_oid = ref_oid(&ctx.git, &remote.bare, "refs/heads/bench")?;
    prepare_network_push(&ctx.git, &remote.bare)?;
    run_grit(&ctx.grit, cwd, &["push", "origin", "HEAD:refs/heads/bench"])?;
    let grit_oid = ref_oid(&ctx.git, &remote.bare, "refs/heads/bench")?;
    if git_oid != grit_oid {
        bail!("push refspec: git {git_oid} != grit {grit_oid}");
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

fn repo_has_object(git: &Path, repo: &Path, oid: &str) -> Result<()> {
    if git_object_exists(git, repo, oid) {
        return Ok(());
    }
    bail!("{} missing object {oid}", repo.display());
}

fn push_target_bare(ctx: &DailyHeadlineContext) -> Result<PathBuf> {
    let remote = remote_state(ctx)?;
    // grit-http-server serves a mirror of the bare; git http-backend serves the bare itself.
    if remote.grit_http.is_some() {
        Ok(network_http_root(&remote.bare).join(REPO_NAME))
    } else {
        Ok(remote.bare.clone())
    }
}

pub fn post_verify_fetch_incremental(ctx: &DailyHeadlineContext, _is_git: bool) -> Result<()> {
    let remote = remote_state(ctx)?;
    verify_client_fetch_tip(
        &ctx.git,
        &remote.bare,
        &ctx.repo,
        &remote.meta.default_branch,
    )?;
    let bare_tip = ref_oid(
        &ctx.git,
        &remote.bare,
        &format!("refs/heads/{}", remote.meta.default_branch),
    )?;
    repo_has_object(&ctx.git, &ctx.repo, &bare_tip)?;
    Ok(())
}

pub fn post_verify_pull_fast_forward(ctx: &DailyHeadlineContext, _is_git: bool) -> Result<()> {
    let remote = remote_state(ctx)?;
    let bare_tip = ref_oid(
        &ctx.git,
        &remote.bare,
        &format!("refs/heads/{}", remote.meta.default_branch),
    )?;
    let head = ref_oid(&ctx.git, &ctx.repo, "HEAD")?;
    if head != bare_tip {
        bail!("post pull: HEAD {head} != bare {bare_tip}");
    }
    Ok(())
}

pub fn post_verify_push_incremental(ctx: &DailyHeadlineContext, _is_git: bool) -> Result<()> {
    let remote = remote_state(ctx)?;
    let target = push_target_bare(ctx)?;
    let branch_ref = format!("refs/heads/{}", remote.meta.default_branch);
    let remote_tip = ref_oid(&ctx.git, &target, &branch_ref)?;
    let client_tip = ref_oid(&ctx.git, &ctx.repo, &branch_ref)?;
    if remote_tip != client_tip {
        bail!(
            "post push: remote {} tip {remote_tip} != client {client_tip}",
            target.display()
        );
    }
    if remote_tip == remote.meta.fetch_base_oid {
        bail!("post push: remote still at fetch base (no transfer)");
    }
    repo_has_object(&ctx.git, &target, &client_tip)?;
    Ok(())
}

/// Before a timed push iteration (after prepare), the push target must not already hold the client tip.
pub fn pre_verify_push_incremental(ctx: &DailyHeadlineContext) -> Result<()> {
    let target = push_target_bare(ctx)?;
    assert_push_target_lacks_client_tip(&ctx.git, &target, &ctx.repo)
}

/// Pre-timed verification (after prepare, before the timed command).
pub fn pre_timed_for(id: &str) -> Option<fn(&DailyHeadlineContext) -> Result<()>> {
    match id {
        "push/incremental@git-git" | "push/incremental-http@git-git" => {
            Some(pre_verify_push_incremental)
        }
        "diff/stat@synth-100k" => Some(super::matrix_helpers::verify_diff_stat),
        "log/stat-n50@git-git" => Some(super::matrix_helpers::verify_log_stat_n50),
        "log/json-filter@git-git" => Some(super::matrix_helpers::verify_log_json_filter),
        "notes/list-n100@synth-10k" => Some(super::matrix_helpers::verify_notes_list_n100),
        _ => None,
    }
}

/// Post-timed verification for remote scenarios (keyed by daily scenario id).
pub fn post_timed_for(id: &str) -> Option<fn(&DailyHeadlineContext, bool) -> Result<()>> {
    match id {
        "fetch/incremental@git-git" | "fetch/incremental-http@git-git" | "fetch/noop@git-git" => {
            Some(post_verify_fetch_incremental)
        }
        "pull/fast-forward@git-git" => Some(post_verify_pull_fast_forward),
        "push/incremental@git-git" | "push/incremental-http@git-git" => {
            Some(post_verify_push_incremental)
        }
        "clone/shallow-depth1@synth-10k" | "clone/shallow-depth1-http@synth-10k" => {
            Some(post_verify_clone_shallow_depth1)
        }
        _ => None,
    }
}

fn post_verify_clone_shallow_depth1(ctx: &DailyHeadlineContext, _is_git: bool) -> Result<()> {
    let remote = remote_state(ctx)?;
    verify_shallow_depth_one_clone(
        &ctx.git,
        &remote.bare,
        &remote.meta.clone_dest,
        &remote.meta.default_branch,
    )
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

pub fn verify_clone_bare(ctx: &DailyHeadlineContext, cwd: &Path) -> Result<()> {
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
        &[
            "clone",
            "-q",
            "--bare",
            "--no-local",
            remote.remote_url.as_str(),
            dest_name,
        ],
    )?;
    let git_refs = bare_clone_for_each_ref(&ctx.git, &dest)?;
    prepare_network_clone(&remote.bare)?;
    run_grit(
        &ctx.grit,
        cwd,
        &["clone", "--bare", remote.remote_url.as_str(), dest_name],
    )?;
    let grit_refs = bare_clone_for_each_ref(&ctx.git, &dest)?;
    if git_refs != grit_refs {
        bail!(
            "bare clone for-each-ref mismatch (git {} lines vs grit {})",
            git_refs.len(),
            grit_refs.len()
        );
    }
    Ok(())
}

fn bare_clone_for_each_ref(git: &Path, repo: &Path) -> Result<Vec<String>> {
    let out = Command::new(git)
        .args(["for-each-ref", "--format=%(refname) %(objectname)", "refs/"])
        .current_dir(repo)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .output()
        .with_context(|| format!("git for-each-ref in {}", repo.display()))?;
    if !out.status.success() {
        bail!(
            "git for-each-ref failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }
    let mut lines: Vec<String> = String::from_utf8_lossy(&out.stdout)
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .map(str::to_owned)
        .collect();
    lines.sort();
    Ok(lines)
}

pub fn verify_clone_shallow(ctx: &DailyHeadlineContext, cwd: &Path) -> Result<()> {
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
        &[
            "clone",
            "--depth",
            "1",
            "-q",
            remote.remote_url.as_str(),
            dest_name,
        ],
    )?;
    verify_shallow_depth_one_clone(&ctx.git, &remote.bare, &dest, &remote.meta.default_branch)?;
    prepare_network_clone(&remote.bare)?;
    run_grit(
        &ctx.grit,
        cwd,
        &["clone", "--shallow", remote.remote_url.as_str(), dest_name],
    )?;
    verify_shallow_depth_one_clone(&ctx.git, &remote.bare, &dest, &remote.meta.default_branch)?;
    Ok(())
}

pub fn verify_clone_shallow_http(ctx: &DailyHeadlineContext, cwd: &Path) -> Result<()> {
    verify_clone_shallow(ctx, cwd)
}

fn count_missing_objects(git: &Path, repo: &Path) -> Result<u64> {
    let out = Command::new(git)
        .args(["rev-list", "--objects", "--missing=print", "HEAD"])
        .current_dir(repo)
        .output()
        .context("git rev-list --missing")?;
    if !out.status.success() {
        bail!(
            "rev-list failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter(|line| line.starts_with('?'))
        .count() as u64)
}

fn verify_blobless_clone_tree(git: &Path, dest: &Path, default_branch: &str) -> Result<()> {
    run_git(git, dest, &["fsck", "--no-dangling"])?;
    let head_tree = tree_oid(git, dest, "HEAD")?;
    let branch_tree = tree_oid(git, dest, &format!("refs/heads/{default_branch}"))?;
    if head_tree != branch_tree {
        bail!("HEAD tree {head_tree} != branch tip tree {branch_tree}");
    }
    Ok(())
}

pub fn verify_clone_blobless(ctx: &DailyHeadlineContext, _cwd: &Path) -> Result<()> {
    let remote = remote_state(ctx)?;
    let dest = remote.meta.clone_dest.clone();
    let clone_cwd = dest.parent().context("clone dest parent directory")?;
    let dest_name = dest
        .file_name()
        .and_then(|s| s.to_str())
        .context("clone dest name")?;
    configure_blobless_upload_pack(&ctx.git, &remote.bare)?;
    prepare_network_clone(&remote.bare)?;
    run_git(
        &ctx.git,
        clone_cwd,
        &[
            "clone",
            "-q",
            "--filter=blob:none",
            remote.remote_url.as_str(),
            dest_name,
        ],
    )?;
    verify_blobless_clone_tree(&ctx.git, &dest, &remote.meta.default_branch)?;
    let git_missing = count_missing_objects(&ctx.git, &dest)?;
    prepare_network_clone(&remote.bare)?;
    run_grit(
        &ctx.grit,
        clone_cwd,
        &[
            "clone",
            "--blobs",
            "none",
            remote.remote_url.as_str(),
            dest_name,
        ],
    )?;
    verify_blobless_clone_tree(&ctx.git, &dest, &remote.meta.default_branch)?;
    let grit_missing = count_missing_objects(&ctx.git, &dest)?;
    if git_missing != grit_missing {
        bail!("missing blob count git {git_missing} != grit {grit_missing}");
    }
    Ok(())
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

pub fn clone_shallow_bench_script_git(tool: &Path, url: &str, dest: &Path) -> String {
    let tool_q = shell_quote(&tool.to_string_lossy());
    let url_q = shell_quote(url);
    let dest_q = shell_quote(&dest.to_string_lossy());
    let dest_name = dest.file_name().and_then(|s| s.to_str()).unwrap_or("clone");
    format!("rm -rf {dest_q} && {tool_q} clone --depth 1 -q {url_q} {dest_name}")
}

pub fn clone_shallow_bench_script_grit(tool: &Path, url: &str, dest: &Path) -> String {
    let tool_q = shell_quote(&tool.to_string_lossy());
    let url_q = shell_quote(url);
    let dest_q = shell_quote(&dest.to_string_lossy());
    let dest_name = dest.file_name().and_then(|s| s.to_str()).unwrap_or("clone");
    format!("rm -rf {dest_q} && {tool_q} clone --shallow {url_q} {dest_name}")
}

/// Ensure the smart HTTP server is running and the client remote URL is current.
pub fn refresh_grit_http_server(ctx: &mut DailyHeadlineContext) -> Result<()> {
    let Some(remote) = ctx.remote.as_mut() else {
        return Ok(());
    };
    // Only the grit-http-server scenarios: the others are served by git http-backend, which
    // reads the bare directly and needs no refresh.
    if remote.grit_http.is_none() {
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

pub fn clone_bare_bench_script(tool: &Path, url: &str, dest: &Path) -> String {
    let tool_q = shell_quote(&tool.to_string_lossy());
    let url_q = shell_quote(url);
    let dest_q = shell_quote(&dest.to_string_lossy());
    let dest_name = dest.file_name().and_then(|s| s.to_str()).unwrap_or("clone");
    format!("rm -rf {dest_q} && {tool_q} clone -q --bare --no-local {url_q} {dest_name}")
}

pub fn clone_bare_bench_script_grit(tool: &Path, url: &str, dest: &Path) -> String {
    let tool_q = shell_quote(&tool.to_string_lossy());
    let url_q = shell_quote(url);
    let dest_q = shell_quote(&dest.to_string_lossy());
    let dest_name = dest.file_name().and_then(|s| s.to_str()).unwrap_or("clone");
    format!("rm -rf {dest_q} && {tool_q} clone --bare {url_q} {dest_name}")
}

pub fn clone_bench_script_shallow(tool: &Path, url: &str, dest: &Path) -> String {
    let tool_q = shell_quote(&tool.to_string_lossy());
    let url_q = shell_quote(url);
    let dest_q = shell_quote(&dest.to_string_lossy());
    let dest_name = dest.file_name().and_then(|s| s.to_str()).unwrap_or("clone");
    format!("rm -rf {dest_q} && {tool_q} clone -q --depth 1 {url_q} {dest_name}")
}

fn clone_bench_script_in_dest_parent(
    tool: &Path,
    url: &str,
    dest: &Path,
    extra_clone_args: &str,
) -> String {
    let tool_q = shell_quote(&tool.to_string_lossy());
    let url_q = shell_quote(url);
    let dest_q = shell_quote(&dest.to_string_lossy());
    let parent_q = shell_quote(
        &dest
            .parent()
            .unwrap_or_else(|| Path::new("."))
            .to_string_lossy(),
    );
    let dest_name = dest.file_name().and_then(|s| s.to_str()).unwrap_or("clone");
    format!(
        "rm -rf {dest_q} && cd {parent_q} && {tool_q} clone {extra_clone_args} {url_q} {dest_name}"
    )
}

pub fn clone_blobless_bench_script_git(tool: &Path, url: &str, dest: &Path) -> String {
    clone_bench_script_in_dest_parent(tool, url, dest, "-q --filter=blob:none")
}

pub fn clone_blobless_bench_script_grit(tool: &Path, url: &str, dest: &Path) -> String {
    clone_bench_script_in_dest_parent(tool, url, dest, "--blobs none")
}

#[cfg(test)]
mod blobless_path_tests {
    use super::*;

    /// Clone commands must run in `clone_dest.parent()` so the checkout lands on the
    /// absolute sidecar path (not nested under `client_repo`).
    #[test]
    fn clone_dest_sidecar_not_under_client_repo() {
        let client = PathBuf::from("/tmp/daily-remote-git-git/remote.git-client");
        let dest = PathBuf::from("/tmp/daily-remote-git-git/remote-clone");
        let parent = dest.parent().expect("parent");
        let name = dest.file_name().and_then(|s| s.to_str()).expect("name");
        assert_eq!(parent.join(name), dest);
        assert_ne!(client.join(name), dest);
    }
}

pub fn clone_bench_script_grit_shallow(tool: &Path, url: &str, dest: &Path) -> String {
    let tool_q = shell_quote(&tool.to_string_lossy());
    let url_q = shell_quote(url);
    let dest_q = shell_quote(&dest.to_string_lossy());
    let dest_name = dest.file_name().and_then(|s| s.to_str()).unwrap_or("clone");
    format!("rm -rf {dest_q} && {tool_q} clone --depth 1 {url_q} {dest_name}")
}

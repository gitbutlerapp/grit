//! Network transport scenarios (clone, fetch, push, ls-remote).

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result};
use time::OffsetDateTime;

use crate::bench_env::isolated_env_prefix;
use crate::binary::{grit_source_commit, tool_version};
use crate::fixture::remove_dir_robust;
use crate::hyperfine::{run_hyperfine, HyperfineRun};
use crate::machine::{collect_machine_info, format_timestamp};
use crate::network_fixture::{
    ensure_fixture, file_url, install_http_repo, load_meta, NetworkBenchMeta, NetworkFixtureKind,
    NetworkProfile, REPO_NAME,
};
use crate::network_http::{GitHttpBackendServer, GritHttpServer};
use crate::scenarios::RunConfig;
use crate::schema::{BenchReport, DriverKind, ScenarioResult, ToolVersions, SCHEMA_VERSION};
use crate::shell::shell_command;
use crate::stats::{median_ratio, timing_from_hyperfine};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TransportKind {
    File,
    GitHttpBackend,
    GritHttp,
}

impl TransportKind {
    fn label(self) -> &'static str {
        match self {
            Self::File => "file",
            Self::GitHttpBackend => "git-http",
            Self::GritHttp => "grit-http",
        }
    }
}

/// Run all network scenarios for the given profile.
pub fn run_network_suite(
    hyperfine: &Path,
    git: &Path,
    grit: &Path,
    http_server: Option<&Path>,
    cfg: &RunConfig,
    profile: NetworkProfile,
    timestamp: OffsetDateTime,
) -> Result<BenchReport> {
    let deep = ensure_fixture(git, profile, NetworkFixtureKind::DeepHistory)?;
    let many_refs = ensure_fixture(git, profile, NetworkFixtureKind::ManyRefs)?;
    let _large = ensure_fixture(git, profile, NetworkFixtureKind::LargeBlobs)?;

    let mut scenarios = Vec::new();

    for transport in [
        TransportKind::File,
        TransportKind::GitHttpBackend,
        TransportKind::GritHttp,
    ] {
        scenarios.extend(run_clone_scenarios(
            hyperfine,
            git,
            grit,
            http_server,
            cfg,
            profile,
            &deep,
            transport,
        )?);
    }

    scenarios.extend(run_fetch_scenarios(
        hyperfine,
        git,
        grit,
        cfg,
        profile,
        &deep,
        TransportKind::File,
    )?);

    scenarios.extend(run_push_scenarios(
        hyperfine,
        git,
        grit,
        cfg,
        profile,
        &deep,
        TransportKind::File,
    )?);

    scenarios.extend(run_ls_remote_scenarios(
        hyperfine,
        git,
        grit,
        cfg,
        profile,
        &many_refs,
        TransportKind::File,
    )?);

    scenarios.push(run_server_side_clone_comparison(
        hyperfine,
        git,
        grit,
        http_server,
        cfg,
        profile,
        &deep,
    )?);

    Ok(BenchReport {
        schema_version: SCHEMA_VERSION,
        timestamp: format_timestamp(timestamp),
        machine: collect_machine_info(&crate::fixture::scratch_dir())?,
        tools: ToolVersions {
            git: tool_version(git),
            grit: tool_version(grit),
            grit_commit: grit_source_commit(),
        },
        scenarios,
    })
}

#[allow(clippy::too_many_arguments)]
fn run_clone_scenarios(
    hyperfine: &Path,
    git: &Path,
    grit: &Path,
    http_server: Option<&Path>,
    cfg: &RunConfig,
    profile: NetworkProfile,
    bare: &Path,
    transport: TransportKind,
) -> Result<Vec<ScenarioResult>> {
    let meta = load_meta(bare)?;
    let (url, _git_http, _grit_http) =
        resolve_transport_url(git, grit, http_server, bare, transport)?;
    let clone_dest = meta.clone_dest.clone();
    let parent = clone_dest
        .parent()
        .context("clone dest parent")?
        .to_path_buf();
    fs::create_dir_all(&parent)?;

    let id = format!("clone-{}-{}", transport.label(), profile.suffix());
    let prepare = prepare_network_command(cfg, git, bare, "prepare-network-clone");
    let dest_name = clone_dest
        .file_name()
        .and_then(|s| s.to_str())
        .context("clone dest name")?;

    let scenario = run_paired_cli_scenario(
        hyperfine,
        git,
        grit,
        cfg,
        &parent,
        &ScenarioSpec {
            id: id.clone(),
            group: "network-clone".into(),
            fixture: bare.display().to_string(),
            description: format!(
                "Clone deep-history fixture over {} (git vs grit client)",
                transport.label()
            ),
            git_argv: vec!["clone".into(), "-q".into(), url.clone(), dest_name.into()],
            grit_argv: vec!["clone".into(), url, dest_name.into()],
            prepare: Some(prepare),
        },
    )?;
    Ok(vec![scenario])
}

fn run_fetch_scenarios(
    hyperfine: &Path,
    git: &Path,
    grit: &Path,
    cfg: &RunConfig,
    profile: NetworkProfile,
    bare: &Path,
    transport: TransportKind,
) -> Result<Vec<ScenarioResult>> {
    let meta = load_meta(bare)?;
    ensure_client_repo(git, bare, &meta)?;
    let (url, _, _) = resolve_transport_url(git, grit, None, bare, transport)?;
    configure_remote(git, &meta.client_repo, &url)?;

    let mut out = Vec::new();

    let incr_prepare = prepare_network_command(cfg, git, bare, "prepare-network-fetch-incr");
    out.push(run_paired_cli_scenario(
        hyperfine,
        git,
        grit,
        cfg,
        &meta.client_repo,
        &ScenarioSpec {
            id: format!("fetch-incr-{}-{}", transport.label(), profile.suffix()),
            group: "network-fetch".into(),
            fixture: bare.display().to_string(),
            description: format!(
                "Incremental fetch of {} new commits over {}",
                meta.incremental_fetch_commits,
                transport.label()
            ),
            git_argv: vec!["fetch".into(), "-q".into(), "origin".into()],
            grit_argv: vec!["fetch".into(), "origin".into()],
            prepare: Some(incr_prepare),
        },
    )?);

    let noop_prepare = prepare_network_command(cfg, git, bare, "prepare-network-fetch-noop");
    out.push(run_paired_cli_scenario(
        hyperfine,
        git,
        grit,
        cfg,
        &meta.client_repo,
        &ScenarioSpec {
            id: format!("fetch-noop-{}-{}", transport.label(), profile.suffix()),
            group: "network-fetch".into(),
            fixture: bare.display().to_string(),
            description: format!(
                "No-op fetch when already up to date ({})",
                transport.label()
            ),
            git_argv: vec!["fetch".into(), "-q".into(), "origin".into()],
            grit_argv: vec!["fetch".into(), "origin".into()],
            prepare: Some(noop_prepare),
        },
    )?);

    Ok(out)
}

fn run_push_scenarios(
    hyperfine: &Path,
    git: &Path,
    grit: &Path,
    cfg: &RunConfig,
    profile: NetworkProfile,
    bare: &Path,
    transport: TransportKind,
) -> Result<Vec<ScenarioResult>> {
    let meta = load_meta(bare)?;
    ensure_client_repo(git, bare, &meta)?;
    let (url, _, _) = resolve_transport_url(git, grit, None, bare, transport)?;
    configure_remote(git, &meta.client_repo, &url)?;

    let prepare = prepare_network_command(cfg, git, bare, "prepare-network-push");
    let scenario = run_paired_cli_scenario(
        hyperfine,
        git,
        grit,
        cfg,
        &meta.client_repo,
        &ScenarioSpec {
            id: format!("push-{}-{}", transport.label(), profile.suffix()),
            group: "network-push".into(),
            fixture: bare.display().to_string(),
            description: format!(
                "Push {} commits to {} remote",
                meta.push_commits,
                transport.label()
            ),
            git_argv: vec!["push".into(), "-q".into(), "origin".into(), "main".into()],
            grit_argv: vec!["push".into()],
            prepare: Some(prepare),
        },
    )?;
    Ok(vec![scenario])
}

fn run_ls_remote_scenarios(
    hyperfine: &Path,
    git: &Path,
    grit: &Path,
    cfg: &RunConfig,
    profile: NetworkProfile,
    bare: &Path,
    transport: TransportKind,
) -> Result<Vec<ScenarioResult>> {
    let meta = load_meta(bare)?;
    let (url, _, _) = resolve_transport_url(git, grit, None, bare, transport)?;
    let scenario = run_paired_cli_scenario(
        hyperfine,
        git,
        grit,
        cfg,
        bare.parent().unwrap_or(Path::new("/tmp")),
        &ScenarioSpec {
            id: format!("ls-remote-{}-{}", transport.label(), profile.suffix()),
            group: "network-ls-remote".into(),
            fixture: bare.display().to_string(),
            description: format!(
                "git ls-remote vs grit remote refs ({}, {} refs)",
                transport.label(),
                meta.many_refs
            ),
            git_argv: vec!["ls-remote".into(), url.clone()],
            grit_argv: vec!["remote".into(), "refs".into(), url],
            prepare: None,
        },
    )?;
    Ok(vec![scenario])
}

/// Compare system `git clone` against git-http-backend vs grit-http-server (server-side).
#[allow(clippy::too_many_arguments)]
fn run_server_side_clone_comparison(
    hyperfine: &Path,
    git: &Path,
    grit: &Path,
    http_server: Option<&Path>,
    cfg: &RunConfig,
    profile: NetworkProfile,
    bare: &Path,
) -> Result<ScenarioResult> {
    let meta = load_meta(bare)?;
    let http_root = network_http_root(bare);
    fs::create_dir_all(&http_root)?;
    install_http_repo(git, bare, &http_root)?;

    let git_server = GitHttpBackendServer::start(git, http_root.clone())?;
    let grit_server = GritHttpServer::start(grit, http_server, http_root)?;

    let git_url = git_server.url(REPO_NAME);
    let grit_url = grit_server.url(REPO_NAME);

    let dest_git = meta
        .clone_dest
        .with_file_name(format!("server-git-{}", profile.suffix()));
    let dest_grit = meta
        .clone_dest
        .with_file_name(format!("server-grit-{}", profile.suffix()));
    let parent = dest_git.parent().context("server clone parent")?;
    fs::create_dir_all(parent)?;

    let env_prefix = cfg.isolated_config.then(isolated_env_prefix);

    let git_timing = run_hyperfine(
        hyperfine,
        &HyperfineRun {
            command: shell_command(
                git,
                &[
                    "clone".to_string(),
                    "-q".to_string(),
                    git_url,
                    dest_git
                        .file_name()
                        .context("server git clone dest name")?
                        .to_string_lossy()
                        .into_owned(),
                ],
            ),
            working_directory: parent.to_path_buf(),
            prepare: Some(prepare_server_clone(&dest_git, &dest_grit)),
            warmup: cfg.warmup,
            min_runs: cfg.min_runs,
            command_name: Some("git-clone-git-http".into()),
            env_prefix: env_prefix.clone(),
        },
    )?;
    let grit_server_timing = run_hyperfine(
        hyperfine,
        &HyperfineRun {
            command: shell_command(
                git,
                &[
                    "clone".to_string(),
                    "-q".to_string(),
                    grit_url,
                    dest_grit
                        .file_name()
                        .context("server grit-http clone dest name")?
                        .to_string_lossy()
                        .into_owned(),
                ],
            ),
            working_directory: parent.to_path_buf(),
            prepare: Some(prepare_server_clone(&dest_git, &dest_grit)),
            warmup: cfg.warmup,
            min_runs: cfg.min_runs,
            command_name: Some("git-clone-grit-http".into()),
            env_prefix,
        },
    )?;

    let git_stats = timing_from_hyperfine(&git_timing);
    let grit_stats = timing_from_hyperfine(&grit_server_timing);
    let ratio = median_ratio(git_stats.median_ms, grit_stats.median_ms);
    Ok(ScenarioResult {
        id: format!("server-clone-http-compare-{}", profile.suffix()),
        group: "network-server".into(),
        fixture: bare.display().to_string(),
        description: "System git clone: git http-backend vs grit-http-server (same client)".into(),
        driver: DriverKind::Cli,
        git: git_stats,
        grit: grit_stats,
        ratio,
        grit_failure: None,
    })
}

struct ScenarioSpec {
    id: String,
    group: String,
    fixture: String,
    description: String,
    git_argv: Vec<String>,
    grit_argv: Vec<String>,
    prepare: Option<String>,
}

fn run_paired_cli_scenario(
    hyperfine: &Path,
    git: &Path,
    grit: &Path,
    cfg: &RunConfig,
    cwd: &Path,
    spec: &ScenarioSpec,
) -> Result<ScenarioResult> {
    let env_prefix = cfg.isolated_config.then(isolated_env_prefix);
    let git_entry = run_hyperfine(
        hyperfine,
        &HyperfineRun {
            command: shell_command(git, &spec.git_argv),
            working_directory: cwd.to_path_buf(),
            prepare: spec.prepare.clone(),
            warmup: cfg.warmup,
            min_runs: cfg.min_runs,
            command_name: Some(format!("git-{}", spec.id)),
            env_prefix: env_prefix.clone(),
        },
    )?;
    let grit_entry = run_hyperfine(
        hyperfine,
        &HyperfineRun {
            command: shell_command(grit, &spec.grit_argv),
            working_directory: cwd.to_path_buf(),
            prepare: spec.prepare.clone(),
            warmup: cfg.warmup,
            min_runs: cfg.min_runs,
            command_name: Some(format!("grit-{}", spec.id)),
            env_prefix,
        },
    )?;
    let git_stats = timing_from_hyperfine(&git_entry);
    let grit_stats = timing_from_hyperfine(&grit_entry);
    let ratio = median_ratio(git_stats.median_ms, grit_stats.median_ms);
    Ok(ScenarioResult {
        id: spec.id.clone(),
        group: spec.group.clone(),
        fixture: spec.fixture.clone(),
        description: spec.description.clone(),
        driver: DriverKind::Cli,
        git: git_stats,
        grit: grit_stats,
        ratio,
        grit_failure: None,
    })
}

fn prepare_network_command(cfg: &RunConfig, git: &Path, bare: &Path, sub: &str) -> String {
    shell_command(
        &cfg.prepare_bin,
        &[
            sub.to_string(),
            "--git".to_string(),
            git.display().to_string(),
            "--bare".to_string(),
            bare.display().to_string(),
        ],
    )
}

fn prepare_server_clone(dest_git: &Path, dest_grit: &Path) -> String {
    format!(
        "rm -rf {} {}",
        shell_quote_path(dest_git),
        shell_quote_path(dest_grit)
    )
}

fn shell_quote_path(p: &Path) -> String {
    crate::shell::shell_quote(&p.to_string_lossy())
}

fn network_http_root(bare: &Path) -> PathBuf {
    let stem = bare
        .file_name()
        .map(|s| s.to_string_lossy())
        .unwrap_or_else(|| std::borrow::Cow::Borrowed("repo.git"));
    bare.with_file_name(format!("{stem}-http-root"))
}

fn resolve_transport_url(
    git: &Path,
    grit: &Path,
    http_server: Option<&Path>,
    bare: &Path,
    transport: TransportKind,
) -> Result<(String, Option<GitHttpBackendServer>, Option<GritHttpServer>)> {
    match transport {
        TransportKind::File => Ok((file_url(bare)?, None, None)),
        TransportKind::GitHttpBackend => {
            let root = network_http_root(bare);
            fs::create_dir_all(&root)?;
            install_http_repo(git, bare, &root)?;
            let server = GitHttpBackendServer::start(git, root)?;
            let url = server.url(REPO_NAME);
            Ok((url, Some(server), None))
        }
        TransportKind::GritHttp => {
            let root = network_http_root(bare);
            fs::create_dir_all(&root)?;
            install_http_repo(git, bare, &root)?;
            let server = GritHttpServer::start(grit, http_server, root)?;
            let url = server.url(REPO_NAME);
            Ok((url, None, Some(server)))
        }
    }
}

fn ensure_client_repo(git: &Path, bare: &Path, meta: &NetworkBenchMeta) -> Result<()> {
    if meta.client_repo.join(".git").is_dir() || meta.client_repo.join("HEAD").is_file() {
        return Ok(());
    }
    if meta.client_repo.exists() {
        remove_dir_robust(&meta.client_repo);
    }
    fs::create_dir_all(meta.client_repo.parent().unwrap_or(Path::new("/tmp")))?;
    run_git_cmd(
        git,
        None,
        &[
            "clone",
            "-q",
            bare.to_str().context("bare")?,
            meta.client_repo.to_str().context("client")?,
        ],
    )?;
    run_git_cmd(git, Some(&meta.client_repo), &["checkout", "-q", "main"])?;
    run_git_cmd(
        git,
        Some(&meta.client_repo),
        &["reset", "--hard", &meta.fetch_base_oid],
    )?;
    Ok(())
}

fn configure_remote(git: &Path, client: &Path, url: &str) -> Result<()> {
    let remote_file = client.join(".git/config");
    if remote_file.is_file() {
        run_git_cmd(git, Some(client), &["remote", "remove", "origin"]).ok();
    }
    run_git_cmd(git, Some(client), &["remote", "add", "origin", url])?;
    run_git_cmd(
        git,
        Some(client),
        &["config", "branch.main.remote", "origin"],
    )?;
    run_git_cmd(
        git,
        Some(client),
        &["config", "branch.main.merge", "refs/heads/main"],
    )?;
    Ok(())
}

fn run_git_cmd(git: &Path, cwd: Option<&Path>, args: &[&str]) -> Result<()> {
    let mut cmd = Command::new(git);
    cmd.args(args);
    if let Some(d) = cwd {
        cmd.current_dir(d);
    }
    crate::bench_env::isolated_env_prefix(); // ensure config exists
    cmd.env("GIT_CONFIG_NOSYSTEM", "1")
        .env(
            "GIT_CONFIG_GLOBAL",
            crate::bench_env::empty_global_config_path(),
        )
        .env("GIT_CONFIG_SYSTEM", "/dev/null");
    let out = cmd
        .output()
        .with_context(|| format!("git {}", args.join(" ")))?;
    if !out.status.success() {
        anyhow::bail!(
            "git {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(())
}

/// Hyperfine prepare: remove clone destination before each timed run.
pub fn prepare_network_clone(bare: &Path) -> Result<()> {
    let meta = load_meta(bare)?;
    if meta.clone_dest.exists() {
        remove_dir_robust(&meta.clone_dest);
    }
    Ok(())
}

/// Reset server ahead of client, then add incremental commits on the bare remote.
pub fn prepare_network_fetch_incr(git: &Path, bare: &Path) -> Result<()> {
    let meta = load_meta(bare)?;
    ensure_client_repo(git, bare, &meta)?;
    run_git_cmd(
        git,
        Some(bare),
        &["update-ref", "refs/heads/main", &meta.fetch_base_oid],
    )?;
    run_git_cmd(
        git,
        Some(&meta.client_repo),
        &["reset", "--hard", &meta.fetch_base_oid],
    )?;
    for i in 0..meta.incremental_fetch_commits {
        append_empty_commit_on_bare(git, bare, &format!("bench-fetch-incr-{i}"))?;
    }
    Ok(())
}

/// Client and server both at the same tip (no-op fetch).
pub fn prepare_network_fetch_noop(git: &Path, bare: &Path) -> Result<()> {
    let meta = load_meta(bare)?;
    ensure_client_repo(git, bare, &meta)?;
    let tip = run_git_output(git, Some(bare), &["rev-parse", "HEAD"])?;
    run_git_cmd(git, Some(&meta.client_repo), &["reset", "--hard", &tip])?;
    Ok(())
}

/// Reset push scenario: client ahead of bare by `push_commits`.
pub fn prepare_network_push(git: &Path, bare: &Path) -> Result<()> {
    let meta = load_meta(bare)?;
    prepare_network_fetch_noop(git, bare)?;
    run_git_cmd(
        git,
        Some(bare),
        &["update-ref", "refs/heads/main", &meta.fetch_base_oid],
    )?;
    run_git_cmd(
        git,
        Some(&meta.client_repo),
        &["reset", "--hard", &meta.fetch_base_oid],
    )?;
    for i in 0..meta.push_commits {
        run_git_cmd(
            git,
            Some(&meta.client_repo),
            &[
                "commit",
                "--allow-empty",
                "-q",
                "-m",
                &format!("bench-push-{i}"),
            ],
        )?;
    }
    Ok(())
}

/// Advance `refs/heads/main` on a bare repo with an empty commit (same tree).
fn append_empty_commit_on_bare(git: &Path, bare: &Path, message: &str) -> Result<()> {
    let tree = run_git_output(git, Some(bare), &["rev-parse", "HEAD^{tree}"])?;
    let parent = run_git_output(git, Some(bare), &["rev-parse", "HEAD"])?;
    let mut cmd = Command::new(git);
    cmd.args(["commit-tree", &tree, "-p", &parent, "-m", message])
        .current_dir(bare);
    cmd.env("GIT_CONFIG_NOSYSTEM", "1")
        .env(
            "GIT_CONFIG_GLOBAL",
            crate::bench_env::empty_global_config_path(),
        )
        .env("GIT_CONFIG_SYSTEM", "/dev/null");
    let out = cmd.output().context("git commit-tree")?;
    if !out.status.success() {
        anyhow::bail!(
            "git commit-tree failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    let new_oid = String::from_utf8_lossy(&out.stdout).trim().to_string();
    run_git_cmd(
        git,
        Some(bare),
        &["update-ref", "refs/heads/main", &new_oid],
    )?;
    Ok(())
}

fn run_git_output(git: &Path, cwd: Option<&Path>, args: &[&str]) -> Result<String> {
    let mut cmd = Command::new(git);
    cmd.args(args);
    if let Some(d) = cwd {
        cmd.current_dir(d);
    }
    cmd.env("GIT_CONFIG_NOSYSTEM", "1")
        .env(
            "GIT_CONFIG_GLOBAL",
            crate::bench_env::empty_global_config_path(),
        )
        .env("GIT_CONFIG_SYSTEM", "/dev/null");
    let out = cmd.output().context("git output")?;
    if !out.status.success() {
        anyhow::bail!("git {} failed", args.join(" "));
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

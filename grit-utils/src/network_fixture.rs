//! Cached bare repositories for network transport benchmarks.

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};

use crate::bench_env::empty_global_config_path;
use crate::fixture::remove_dir_robust;

/// Bare repository directory name exposed under HTTP docroots.
pub const REPO_NAME: &str = "fixture.git";

/// Scale for network fixtures (production vs smoke integration tests).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NetworkProfile {
    /// Full acceptance sizes (cached under `/tmp/grit-bench-network-cache`).
    Production,
    /// Tiny repos for `grit-utils` integration tests.
    Smoke,
}

impl NetworkProfile {
    pub fn deep_history(self) -> (usize, usize) {
        match self {
            Self::Production => (20_000, 50_000),
            Self::Smoke => (80, 80),
        }
    }

    pub fn many_refs(self) -> usize {
        match self {
            Self::Production => 10_000,
            Self::Smoke => 40,
        }
    }

    pub fn large_blob_bytes(self) -> usize {
        match self {
            Self::Production => 4 * 1024 * 1024,
            Self::Smoke => 32 * 1024,
        }
    }

    pub fn incremental_fetch_commits(self) -> usize {
        match self {
            Self::Production => 100,
            Self::Smoke => 5,
        }
    }

    pub fn push_commits(self) -> usize {
        match self {
            Self::Production => 50,
            Self::Smoke => 3,
        }
    }

    pub fn suffix(self) -> &'static str {
        match self {
            Self::Production => "prod",
            Self::Smoke => "smoke",
        }
    }
}

/// Which cached fixture variant to ensure.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NetworkFixtureKind {
    DeepHistory,
    ManyRefs,
    LargeBlobs,
}

/// Metadata for prepare hooks and scenario wiring.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NetworkBenchMeta {
    pub profile: String,
    pub kind: String,
    pub bare_repo: PathBuf,
    pub http_repo_name: String,
    pub many_refs: usize,
    pub incremental_fetch_commits: usize,
    pub push_commits: usize,
    /// Client clone used for fetch/push (not bare).
    pub client_repo: PathBuf,
    pub clone_dest: PathBuf,
    /// Tip OID on the server before incremental fetch prep.
    pub fetch_base_oid: String,
}

pub fn meta_path(bare: &Path) -> PathBuf {
    bare.join(".grit-bench-network-meta.json")
}

pub fn load_meta(bare: &Path) -> Result<NetworkBenchMeta> {
    let raw = fs::read_to_string(meta_path(bare)).context("read network bench meta")?;
    serde_json::from_str(&raw).context("parse network bench meta")
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

fn run_git(git: &Path, dir: Option<&Path>, args: &[&str]) -> Result<()> {
    let mut cmd = Command::new(git);
    cmd.args(args);
    if let Some(d) = dir {
        cmd.current_dir(d);
    }
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

fn rev_parse(git: &Path, dir: &Path, rev: &str) -> Result<String> {
    let mut cmd = Command::new(git);
    cmd.args(["rev-parse", rev]).current_dir(dir);
    git_env(&mut cmd);
    let out = cmd.output().context("git rev-parse")?;
    if !out.status.success() {
        bail!("rev-parse {rev} failed");
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

/// Root directory for cached network fixtures (`GRIT_BENCH_NETWORK_CACHE`).
pub fn network_cache_root() -> PathBuf {
    std::env::var("GRIT_BENCH_NETWORK_CACHE")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("/tmp/grit-bench-network-cache"))
}

fn ready_marker(profile: NetworkProfile, kind: NetworkFixtureKind) -> PathBuf {
    let name = match kind {
        NetworkFixtureKind::DeepHistory => format!("deep-history-{}.ready", profile.suffix()),
        NetworkFixtureKind::ManyRefs => format!("many-refs-{}.ready", profile.suffix()),
        NetworkFixtureKind::LargeBlobs => format!("large-blobs-{}.ready", profile.suffix()),
    };
    network_cache_root().join(name)
}

fn bare_path(profile: NetworkProfile, kind: NetworkFixtureKind) -> PathBuf {
    let name = match kind {
        NetworkFixtureKind::DeepHistory => format!("deep-history-{}.git", profile.suffix()),
        NetworkFixtureKind::ManyRefs => format!("many-refs-{}.git", profile.suffix()),
        NetworkFixtureKind::LargeBlobs => format!("large-blobs-{}.git", profile.suffix()),
    };
    network_cache_root().join(name)
}

/// Ensure the requested bare fixture exists (build once, reuse marker file).
pub fn ensure_fixture(
    git: &Path,
    profile: NetworkProfile,
    kind: NetworkFixtureKind,
) -> Result<PathBuf> {
    fs::create_dir_all(network_cache_root())?;
    let marker = ready_marker(profile, kind);
    let dest = bare_path(profile, kind);
    if marker.is_file() && dest.join("HEAD").is_file() {
        return Ok(dest);
    }
    if dest.exists() {
        remove_dir_robust(&dest);
    }
    match kind {
        NetworkFixtureKind::DeepHistory => build_deep_history(git, profile, &dest)?,
        NetworkFixtureKind::ManyRefs => build_many_refs(git, profile, &dest)?,
        NetworkFixtureKind::LargeBlobs => build_large_blobs(git, profile, &dest)?,
    }
    fs::write(&marker, b"ok")?;
    Ok(dest)
}

fn build_deep_history(git: &Path, profile: NetworkProfile, dest: &Path) -> Result<()> {
    let (file_count, commit_count) = profile.deep_history();
    eprintln!(
        "Building deep-history fixture ({file_count} files, {commit_count} commits) at {} …",
        dest.display()
    );
    fs::create_dir_all(dest)?;
    run_git(git, Some(dest), &["init", "--bare", "-q"])?;

    let mut import = Vec::new();
    write_deep_history_fast_import(&mut import, file_count, commit_count)?;
    run_git_fast_import(git, dest, &import)?;
    run_git(
        git,
        Some(dest),
        &["symbolic-ref", "HEAD", "refs/heads/main"],
    )?;

    write_meta(
        git,
        profile,
        NetworkFixtureKind::DeepHistory,
        dest,
        profile.many_refs(),
    )?;
    Ok(())
}

fn build_many_refs(git: &Path, profile: NetworkProfile, dest: &Path) -> Result<()> {
    let deep = ensure_fixture(git, profile, NetworkFixtureKind::DeepHistory)?;
    eprintln!(
        "Cloning deep-history to many-refs fixture at {} …",
        dest.display()
    );
    if dest.exists() {
        remove_dir_robust(dest);
    }
    run_git(
        git,
        None,
        &[
            "clone",
            "-q",
            "--bare",
            deep.to_str().context("deep path utf8")?,
            dest.to_str().context("dest path utf8")?,
        ],
    )?;

    let ref_count = profile.many_refs();
    eprintln!("Creating {ref_count} refs …");
    let mut oid_pool = Vec::new();
    for step in 0..128 {
        let offset = step * 400 + 1;
        let rev = format!("refs/heads/main~{offset}");
        if let Ok(oid) = rev_parse(git, dest, &rev) {
            oid_pool.push(oid);
        }
    }
    if oid_pool.is_empty() {
        oid_pool.push(rev_parse(git, dest, "refs/heads/main")?);
    }
    let mut batch = String::new();
    for i in 0..ref_count {
        let oid = &oid_pool[i % oid_pool.len()];
        batch.push_str(&format!("update refs/heads/bench-ref-{i:05} {oid}\n"));
    }
    let batch_path = dest.join(".grit-bench-refs-batch.txt");
    fs::write(&batch_path, batch).context("write refs batch")?;
    let mut cmd = Command::new(git);
    cmd.args(["update-ref", "--stdin"])
        .current_dir(dest)
        .stdin(Stdio::from(
            fs::File::open(&batch_path).context("open refs batch")?,
        ))
        .stdout(Stdio::null())
        .stderr(Stdio::piped());
    git_env(&mut cmd);
    let out = cmd.output().context("git update-ref --stdin")?;
    let _ = fs::remove_file(&batch_path);
    if !out.status.success() {
        bail!(
            "git update-ref --stdin failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    write_meta(git, profile, NetworkFixtureKind::ManyRefs, dest, ref_count)?;
    Ok(())
}

fn build_large_blobs(git: &Path, profile: NetworkProfile, dest: &Path) -> Result<()> {
    let blob_size = profile.large_blob_bytes();
    eprintln!(
        "Building large-blob fixture ({blob_size} bytes/blob) at {} …",
        dest.display()
    );
    fs::create_dir_all(dest)?;
    run_git(git, Some(dest), &["init", "--bare", "-q"])?;

    let mut import = Vec::new();
    writeln!(import, "blob")?;
    writeln!(import, "mark :1")?;
    writeln!(import, "data {blob_size}")?;
    import.write_all(&vec![b'x'; blob_size])?;

    for i in 2..=5 {
        writeln!(import, "blob")?;
        writeln!(import, "mark :{i}")?;
        writeln!(import, "data {blob_size}")?;
        import.write_all(&vec![b'a' + i as u8; blob_size])?;
    }

    writeln!(import, "commit refs/heads/main")?;
    writeln!(import, "mark :10")?;
    writeln!(import, "committer Bench <b@example.com> 1 +0000")?;
    let init_msg = "init\n";
    writeln!(import, "data {}", init_msg.len())?;
    import.write_all(init_msg.as_bytes())?;
    for i in 1..=5 {
        writeln!(import, "M 100644 :{i} large/blob{i}.bin")?;
    }

    run_git_fast_import(git, dest, &import)?;
    run_git(
        git,
        Some(dest),
        &["symbolic-ref", "HEAD", "refs/heads/main"],
    )?;
    write_meta(
        git,
        profile,
        NetworkFixtureKind::LargeBlobs,
        dest,
        profile.many_refs(),
    )?;
    Ok(())
}

fn write_deep_history_fast_import(
    out: &mut Vec<u8>,
    file_count: usize,
    commit_count: usize,
) -> Result<()> {
    for i in 0..file_count {
        let body = format!("content-{i}\n");
        writeln!(out, "blob")?;
        writeln!(out, "mark :{}", i + 1)?;
        writeln!(out, "data {}", body.len())?;
        write!(out, "{body}")?;
    }

    writeln!(out, "commit refs/heads/main")?;
    writeln!(out, "mark :{}", file_count + 1)?;
    writeln!(out, "committer Bench <b@example.com> 1 +0000")?;
    let initial_msg = "initial\n";
    writeln!(out, "data {}", initial_msg.len())?;
    write!(out, "{initial_msg}")?;
    for i in 0..file_count {
        let dir = i / 100;
        let file = i % 100;
        let path = format!("d{dir:04}/f{file:04}.txt");
        writeln!(out, "M 100644 :{} {path}", i + 1)?;
    }

    let mut prev_mark = file_count + 1;
    for c in 1..commit_count {
        let ts = 1 + c as u64;
        writeln!(out, "commit refs/heads/main")?;
        let mark = file_count + 1 + c;
        writeln!(out, "mark :{mark}")?;
        writeln!(out, "committer Bench <b@example.com> {ts} +0000")?;
        let msg = format!("msg {c}\n");
        writeln!(out, "data {}", msg.len())?;
        write!(out, "{msg}")?;
        writeln!(out, "from :{prev_mark}")?;
        prev_mark = mark;
    }
    Ok(())
}

fn run_git_fast_import(git: &Path, dest: &Path, payload: &[u8]) -> Result<()> {
    let mut cmd = Command::new(git);
    cmd.args(["fast-import", "--quiet"])
        .current_dir(dest)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::piped());
    git_env(&mut cmd);
    let mut child = cmd.spawn().context("spawn git fast-import")?;
    {
        let stdin = child.stdin.as_mut().context("fast-import stdin")?;
        stdin.write_all(payload)?;
    }
    let out = child.wait_with_output().context("wait fast-import")?;
    if !out.status.success() {
        bail!(
            "git fast-import failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(())
}

fn bare_repo_stem(bare: &Path) -> String {
    bare.file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| "repo.git".to_string())
}

fn write_meta(
    git: &Path,
    profile: NetworkProfile,
    kind: NetworkFixtureKind,
    bare: &Path,
    many_refs: usize,
) -> Result<()> {
    let fetch_base = rev_parse(
        git,
        bare,
        &format!("refs/heads/main~{}", profile.incremental_fetch_commits()),
    )
    .or_else(|_| rev_parse(git, bare, "refs/heads/main"))?;

    let meta = NetworkBenchMeta {
        profile: profile.suffix().to_string(),
        kind: format!("{kind:?}"),
        bare_repo: bare.to_path_buf(),
        http_repo_name: REPO_NAME.to_string(),
        many_refs,
        incremental_fetch_commits: profile.incremental_fetch_commits(),
        push_commits: profile.push_commits(),
        client_repo: bare.with_file_name(format!("{}-client", bare_repo_stem(bare))),
        clone_dest: bare.with_file_name(format!("{}-clone", bare_repo_stem(bare))),
        fetch_base_oid: fetch_base,
    };
    fs::write(meta_path(bare), serde_json::to_string_pretty(&meta)?)?;
    Ok(())
}

/// `file://` URL for a bare repository path.
pub fn file_url(bare: &Path) -> Result<String> {
    let abs = bare
        .canonicalize()
        .with_context(|| format!("canonicalize {}", bare.display()))?;
    Ok(format!("file://{}", abs.display()))
}

/// Copy bare repo to HTTP docroot as `fixture.git`.
pub fn install_http_repo(git: &Path, source_bare: &Path, http_root: &Path) -> Result<PathBuf> {
    let dest = http_root.join(REPO_NAME);
    if dest.exists() {
        remove_dir_robust(&dest);
    }
    fs::create_dir_all(http_root)?;
    run_git(
        git,
        None,
        &[
            "clone",
            "-q",
            "--bare",
            source_bare.to_str().context("source bare utf8")?,
            dest.to_str().context("http bare utf8")?,
        ],
    )?;
    Ok(dest)
}

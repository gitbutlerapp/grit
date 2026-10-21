//! Cached bare repositories with 100k refs for ref benchmarks.

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};

use crate::bench_env::{empty_global_config_path, isolated_env_prefix};
use crate::fixture::remove_dir_robust;

pub const REF_COUNT: usize = 100_000;
/// Smaller packed-ref fixture for scaling comparisons (10k vs 100k).
pub const REF_COUNT_10K: usize = 10_000;
const EMPTY_TREE: &str = "4b825dc642cb6eb9a060e54bf8d69288fbee4904";

/// Which cached 100k-ref layout to use.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RefsFixtureKind {
    Packed,
    Loose,
    Reftable,
}

/// Metadata and stdin payloads stored beside a cached fixture.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RefsFixtureMeta {
    pub base_commit: String,
    pub second_commit: String,
    pub create_stdin: String,
    pub update_stdin: String,
    pub delete_stdin: String,
}

impl RefsFixtureMeta {
    pub fn load(repo: &Path) -> Result<Self> {
        let raw = fs::read_to_string(meta_path(repo)).context("read refs fixture meta")?;
        serde_json::from_str(&raw).context("parse refs fixture meta")
    }
}

pub fn meta_path(repo: &Path) -> PathBuf {
    repo.join(".grit-bench-refs.json")
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

fn run_git(git: &Path, dir: &Path, args: &[&str]) -> Result<()> {
    let mut cmd = Command::new(git);
    cmd.args(args).current_dir(dir);
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

fn git_output(git: &Path, dir: &Path, args: &[&str]) -> Result<String> {
    let mut cmd = Command::new(git);
    cmd.args(args).current_dir(dir);
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
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_owned())
}

/// Root directory for cached ref benchmarks (override with `GRIT_BENCH_REFS_CACHE`).
pub fn refs_scratch_root() -> PathBuf {
    std::env::var("GRIT_BENCH_REFS_CACHE")
        .or_else(|_| std::env::var("GRIT_BENCH_SCRATCH"))
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("/tmp/grit-bench-refs-cache"))
}

pub fn refs_isolated_env() -> String {
    isolated_env_prefix()
}

/// Whether the resolved `git` binary supports `git init --ref-format=reftable` (Git ≥ 2.45).
#[must_use]
pub fn git_supports_reftable(git: &Path) -> bool {
    let Ok(dir) = tempfile::tempdir() else {
        return false;
    };
    Command::new(git)
        .current_dir(dir.path())
        .args(["init", "-q", "--ref-format=reftable"])
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

fn fixture_dir_name(kind: RefsFixtureKind) -> &'static str {
    match kind {
        RefsFixtureKind::Packed => "refs-100k-packed",
        RefsFixtureKind::Loose => "refs-100k-loose",
        RefsFixtureKind::Reftable => "refs-100k-reftable",
    }
}

fn ready_marker(kind: RefsFixtureKind) -> &'static str {
    match kind {
        RefsFixtureKind::Packed => ".grit-bench-refs-packed-ready",
        RefsFixtureKind::Loose => ".grit-bench-refs-loose-ready",
        RefsFixtureKind::Reftable => ".grit-bench-refs-reftable-ready",
    }
}

fn ref_name(i: usize) -> String {
    format!("refs/heads/b{i:05}")
}

fn write_stdin_files(
    repo: &Path,
    base_commit: &str,
    second_commit: &str,
) -> Result<RefsFixtureMeta> {
    let create_path = repo.join("create-100k.stdin");
    let update_path = repo.join("update-1k.stdin");
    let delete_path = repo.join("delete-1k.stdin");

    {
        let mut create = fs::File::create(&create_path).context("create stdin")?;
        for i in 0..REF_COUNT {
            writeln!(create, "create {} {base_commit}", ref_name(i))?;
        }
    }
    {
        let mut update = fs::File::create(&update_path).context("update stdin")?;
        for i in 0..1000 {
            writeln!(
                update,
                "update {} {second_commit} {base_commit}",
                ref_name(i)
            )?;
        }
    }
    {
        let mut delete = fs::File::create(&delete_path).context("delete stdin")?;
        for i in 0..1000 {
            writeln!(delete, "delete {} {base_commit}", ref_name(i))?;
        }
    }

    let meta = RefsFixtureMeta {
        base_commit: base_commit.to_owned(),
        second_commit: second_commit.to_owned(),
        create_stdin: create_path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| "create-100k.stdin".to_owned()),
        update_stdin: update_path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| "update-1k.stdin".to_owned()),
        delete_stdin: delete_path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| "delete-1k.stdin".to_owned()),
    };
    fs::write(meta_path(repo), serde_json::to_string_pretty(&meta)?)?;
    Ok(meta)
}

fn build_fixture(git: &Path, dest: &Path, kind: RefsFixtureKind) -> Result<()> {
    if dest.exists() {
        remove_dir_robust(dest);
    }
    fs::create_dir_all(dest.parent().unwrap_or(Path::new("/tmp")))?;

    let parent = dest.parent().unwrap_or(Path::new("/tmp"));
    fs::create_dir_all(parent)?;
    let name = dest
        .file_name()
        .and_then(|s| s.to_str())
        .ok_or_else(|| anyhow::anyhow!("bad fixture path"))?;
    let mut init_args: Vec<&str> = vec!["init", "--bare", "-q"];
    if kind == RefsFixtureKind::Reftable {
        init_args.push("--ref-format=reftable");
    }
    init_args.extend(["-b", "main", name]);
    run_git(git, parent, &init_args)?;

    let base_commit = git_output(
        git,
        dest,
        &["commit-tree", EMPTY_TREE, "-m", "refs-fixture-base"],
    )?;
    let second_commit = git_output(
        git,
        dest,
        &[
            "commit-tree",
            EMPTY_TREE,
            "-m",
            "refs-fixture-second",
            "-p",
            &base_commit,
        ],
    )?;

    eprintln!("Creating {REF_COUNT} refs in {} …", dest.display());
    let meta = write_stdin_files(dest, &base_commit, &second_commit)?;
    let stdin_path = dest.join(&meta.create_stdin);
    apply_stdin_via_shell(git, dest, &stdin_path)?;

    match kind {
        RefsFixtureKind::Packed | RefsFixtureKind::Reftable => {
            run_git(git, dest, &["pack-refs", "--all"])?;
        }
        RefsFixtureKind::Loose => {
            // Ensure no packed-refs file remains.
            let _ = fs::remove_file(grit_lib::refs::store::paths::packed_refs_path(dest));
        }
    }

    fs::write(dest.join(ready_marker(kind)), b"ok")?;
    Ok(())
}

fn apply_stdin_via_shell(git: &Path, repo: &Path, stdin_file: &Path) -> Result<()> {
    let mut cmd = Command::new(git);
    cmd.args(["update-ref", "--stdin"])
        .current_dir(repo)
        .stdin(std::fs::File::open(stdin_file).context("open stdin file")?);
    git_env(&mut cmd);
    let out = cmd.output().context("git update-ref --stdin")?;
    if !out.status.success() {
        bail!(
            "git update-ref --stdin failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(())
}

/// Ensure the cached fixture exists and return its path.
pub fn ensure_refs_fixture(git: &Path, kind: RefsFixtureKind) -> Result<PathBuf> {
    if kind == RefsFixtureKind::Reftable && !git_supports_reftable(git) {
        bail!("reftable fixture skipped: system git is older than 2.45 (no --ref-format=reftable)");
    }
    let dest = refs_scratch_root().join(fixture_dir_name(kind));
    if dest.join(ready_marker(kind)).is_file() {
        return Ok(dest);
    }
    build_fixture(git, &dest, kind)?;
    Ok(dest)
}

fn rsync_mirror(source: &Path, dest: &Path) -> Result<()> {
    if let Some(parent) = dest.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::create_dir_all(dest)?;
    if which_rsync().is_some() {
        let status = Command::new("rsync")
            .args(["-a", "--delete"])
            .arg(format!("{}/", source.display()))
            .arg(format!("{}/", dest.display()))
            .status()
            .context("spawn rsync")?;
        if status.success() {
            return Ok(());
        }
    }
    clear_dir_contents(dest)?;
    copy_tree_into(source, dest)
}

/// Remove every entry under `dir` but keep `dir` itself (safe when a shell has `cd`'d into it).
fn clear_dir_contents(dir: &Path) -> Result<()> {
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();
        if path.is_dir() {
            remove_dir_robust(&path);
        } else {
            fs::remove_file(&path)?;
        }
    }
    Ok(())
}

fn copy_tree_into(source: &Path, dest: &Path) -> Result<()> {
    for entry in fs::read_dir(source)? {
        let entry = entry?;
        let name = entry.file_name();
        let from = entry.path();
        let to = dest.join(&name);
        if from.is_dir() {
            fs::create_dir_all(&to)?;
            copy_tree_into(&from, &to)?;
        } else {
            fs::copy(&from, &to)?;
        }
    }
    Ok(())
}

fn which_rsync() -> Option<PathBuf> {
    std::env::var_os("PATH").and_then(|paths| {
        std::env::split_paths(&paths).find_map(|dir| {
            let candidate = dir.join("rsync");
            candidate.is_file().then_some(candidate)
        })
    })
}

fn remove_dir_contents(dir: &Path) -> Result<()> {
    if !dir.is_dir() {
        return Ok(());
    }
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();
        if path.is_dir() {
            remove_dir_robust(&path);
        } else {
            let _ = fs::remove_file(&path);
        }
    }
    Ok(())
}

fn clear_refs_subdir_or_file(git_dir: &Path, name: &str) -> Result<()> {
    let path = git_dir.join("refs").join(name);
    if path.is_dir() {
        remove_dir_contents(&path)?;
    } else if path.exists() {
        fs::remove_file(&path)?;
    }
    Ok(())
}

/// Strip branch/tag refs while keeping objects (detached `HEAD` at base commit).
pub fn strip_refs_to_empty(git_dir: &Path, base_commit: &str) -> Result<()> {
    let _ = fs::remove_file(grit_lib::refs::store::paths::packed_refs_path(git_dir));
    clear_refs_subdir_or_file(git_dir, "heads")?;
    clear_refs_subdir_or_file(git_dir, "tags")?;
    if git_dir.join("reftable").is_dir() {
        remove_dir_robust(&git_dir.join("reftable"));
        fs::create_dir_all(git_dir.join("reftable"))?;
    }
    for entry in fs::read_dir(git_dir.join("refs"))?.flatten() {
        let path = entry.path();
        if path.extension().is_some_and(|e| e == "ref") {
            let _ = fs::remove_file(path);
        }
    }
    fs::write(git_dir.join("HEAD"), format!("{base_commit}\n"))?;
    Ok(())
}

/// Keep the first `keep` ref lines in `packed-refs` (Git packed-refs format).
pub fn truncate_packed_refs(repo: &Path, keep: usize) -> Result<()> {
    let path = grit_lib::refs::store::paths::packed_refs_path(repo);
    let raw = fs::read_to_string(&path).with_context(|| format!("read {}", path.display()))?;
    let mut header = String::new();
    let mut ref_lines: Vec<&str> = Vec::new();
    for line in raw.lines() {
        if line.starts_with('#') || line.is_empty() {
            if !header.is_empty() {
                header.push('\n');
            }
            header.push_str(line);
        } else {
            ref_lines.push(line);
        }
    }
    if ref_lines.len() < keep {
        bail!(
            "packed-refs has {} refs, cannot truncate to {keep}",
            ref_lines.len()
        );
    }
    ref_lines.truncate(keep);
    let mut out = header;
    if !out.is_empty() && !out.ends_with('\n') {
        out.push('\n');
    }
    for line in ref_lines {
        out.push_str(line);
        out.push('\n');
    }
    fs::write(&path, out).with_context(|| format!("write {}", path.display()))?;
    Ok(())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RefsWorkMode {
    Full,
    Empty,
}

pub fn work_dir_for(kind: RefsFixtureKind, mode: RefsWorkMode) -> PathBuf {
    let suffix = match mode {
        RefsWorkMode::Full => "work",
        RefsWorkMode::Empty => "work-empty",
    };
    refs_scratch_root().join(format!("{}-{suffix}", fixture_dir_name(kind)))
}

/// Restore a benchmark working copy from the cached fixture.
pub fn restore_work_copy(source: &Path, work: &Path, mode: RefsWorkMode) -> Result<()> {
    rsync_mirror(source, work)?;
    if mode == RefsWorkMode::Empty {
        let meta = RefsFixtureMeta::load(source)?;
        strip_refs_to_empty(work, &meta.base_commit)?;
    }
    Ok(())
}

/// Hyperfine prepare hook: restore fixture working copy.
pub fn run_prepare_refs(git: &Path, source: &Path, work: &Path, mode: RefsWorkMode) -> Result<()> {
    let _ = git;
    restore_work_copy(source, work, mode)
}

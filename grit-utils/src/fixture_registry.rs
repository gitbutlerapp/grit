//! Cached benchmark fixtures: pinned `git.git` and grit-lib synthetic tiers.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{bail, Context, Result};
use grit_lib::bench_fixture::synth::{generate_synth_repo, SynthTier};

use crate::fixture::remove_dir_robust;
use crate::real_repo_fixture::{git_env, run_git, DEFAULT_GIT_GIT_TAG};
use crate::schema::FixtureRecord;

const GIT_GIT_URL: &str = "https://github.com/git/git.git";
const FIXTURE_READY: &str = ".grit-bench-fixture-ready";

/// Commit at [`DEFAULT_GIT_GIT_TAG`] (verified after clone; override tag via env only in prepare).
pub const DEFAULT_GIT_GIT_HEAD: &str = "777489f9e09c8d0dd6b12f9d90de6376330577a2";

/// Known cached fixture kinds.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, clap::ValueEnum)]
#[value(rename_all = "kebab-case")]
pub enum FixtureKind {
    /// Upstream git.git at the pinned tag (bare).
    GitGit,
    /// Synthetic tier S (grit-lib generator).
    SynthS,
    SynthM,
    SynthL,
}

impl FixtureKind {
    /// All fixtures the registry can prepare.
    pub const ALL: [Self; 4] = [Self::GitGit, Self::SynthS, Self::SynthM, Self::SynthL];

    fn synth_tier(self) -> Option<SynthTier> {
        match self {
            Self::GitGit => None,
            Self::SynthS => Some(SynthTier::S),
            Self::SynthM => Some(SynthTier::M),
            Self::SynthL => Some(SynthTier::L),
        }
    }

    fn cache_name(self) -> &'static str {
        match self {
            Self::GitGit => "git.git",
            Self::SynthS => "synth-s",
            Self::SynthM => "synth-m",
            Self::SynthL => "synth-l",
        }
    }
}

/// Root directory for prepared fixtures.
///
/// Resolution order: `GRIT_BENCH_CACHE`, then `$XDG_CACHE_HOME/grit-bench`, then `~/.cache/grit-bench`.
pub fn cache_root() -> PathBuf {
    if let Ok(p) = std::env::var("GRIT_BENCH_CACHE") {
        return PathBuf::from(p);
    }
    if let Ok(xdg) = std::env::var("XDG_CACHE_HOME") {
        return PathBuf::from(xdg).join("grit-bench");
    }
    dirs_fallback_cache()
}

fn dirs_fallback_cache() -> PathBuf {
    if let Ok(home) = std::env::var("HOME") {
        return PathBuf::from(home).join(".cache/grit-bench");
    }
    PathBuf::from("/tmp/grit-bench-cache")
}

fn cache_path(kind: FixtureKind) -> PathBuf {
    cache_root().join(kind.cache_name())
}

fn pinned_git_git_tag() -> String {
    std::env::var("GRIT_BENCH_GIT_GIT_TAG").unwrap_or_else(|_| DEFAULT_GIT_GIT_TAG.to_owned())
}

fn expected_git_git_head() -> String {
    std::env::var("GRIT_BENCH_GIT_GIT_HEAD").unwrap_or_else(|_| DEFAULT_GIT_GIT_HEAD.to_owned())
}

fn read_ready_marker(path: &Path) -> Result<Option<String>> {
    let marker = path.join(FIXTURE_READY);
    if !marker.is_file() {
        return Ok(None);
    }
    Ok(Some(
        fs::read_to_string(&marker)
            .context("read fixture ready marker")?
            .trim()
            .to_owned(),
    ))
}

fn write_ready_marker(path: &Path, head_oid: &str) -> Result<()> {
    fs::write(path.join(FIXTURE_READY), head_oid.as_bytes()).context("write fixture ready marker")
}

/// Return `HEAD` as hex via `git rev-parse`.
pub fn git_head_oid(git: &Path, repo: &Path) -> Result<String> {
    let mut cmd = Command::new(git);
    cmd.args(["rev-parse", "HEAD"]).current_dir(repo);
    git_env(&mut cmd);
    let out = cmd.output().context("git rev-parse HEAD")?;
    if !out.status.success() {
        bail!(
            "git rev-parse HEAD failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_owned())
}

/// Reject a fixture whose `HEAD` does not match the pinned tag oid.
pub fn verify_pinned_head(repo: &Path, expected_head: &str) -> Result<()> {
    let git = crate::binary::resolve_binary("git", None)?;
    let actual = git_head_oid(&git, repo)?;
    if actual != expected_head {
        bail!("fixture HEAD {actual} does not match pinned oid {expected_head}");
    }
    Ok(())
}

fn git_count_objects(git: &Path, repo: &Path) -> Result<u64> {
    let out = Command::new(git)
        .args(["rev-list", "--count", "--objects", "HEAD"])
        .current_dir(repo)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env(
            "GIT_CONFIG_GLOBAL",
            crate::bench_env::empty_global_config_path(),
        )
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .output()
        .context("git rev-list --count --objects")?;
    if !out.status.success() {
        bail!(
            "git rev-list --objects failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    String::from_utf8_lossy(&out.stdout)
        .trim()
        .parse::<u64>()
        .context("parse object count")
}

fn git_count_refs(git: &Path, repo: &Path) -> Result<u64> {
    let out = Command::new(git)
        .args(["for-each-ref", "--format=%(refname)"])
        .current_dir(repo)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env(
            "GIT_CONFIG_GLOBAL",
            crate::bench_env::empty_global_config_path(),
        )
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .output()
        .context("git for-each-ref")?;
    if !out.status.success() {
        bail!(
            "git for-each-ref failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(out
        .stdout
        .split(|b| *b == b'\n')
        .filter(|line| !line.is_empty())
        .count() as u64)
}

fn git_count_tracked_files(git: &Path, repo: &Path) -> Result<u64> {
    if repo.join("HEAD").is_file() && !repo.join(".git").exists() {
        // bare: count paths in HEAD tree
        let out = Command::new(git)
            .args(["ls-tree", "-r", "--name-only", "HEAD"])
            .current_dir(repo)
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env(
                "GIT_CONFIG_GLOBAL",
                crate::bench_env::empty_global_config_path(),
            )
            .env("GIT_CONFIG_SYSTEM", "/dev/null")
            .output()
            .context("git ls-tree")?;
        if !out.status.success() {
            bail!(
                "git ls-tree failed: {}",
                String::from_utf8_lossy(&out.stderr).trim()
            );
        }
        return Ok(out
            .stdout
            .split(|b| *b == b'\n')
            .filter(|l| !l.is_empty())
            .count() as u64);
    }
    let out = Command::new(git)
        .args(["ls-files"])
        .current_dir(repo)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env(
            "GIT_CONFIG_GLOBAL",
            crate::bench_env::empty_global_config_path(),
        )
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .output()
        .context("git ls-files")?;
    if !out.status.success() {
        bail!(
            "git ls-files failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(out
        .stdout
        .split(|b| *b == b'\n')
        .filter(|l| !l.is_empty())
        .count() as u64)
}

/// Collect identity fields for a prepared fixture directory.
pub fn collect_identity(git: &Path, kind: FixtureKind, repo: &Path) -> Result<FixtureRecord> {
    let head_oid = git_head_oid(git, repo)?;
    let tier = kind
        .synth_tier()
        .map(|t| format!("{t:?}").to_ascii_uppercase());
    Ok(FixtureRecord {
        name: kind.cache_name().to_owned(),
        tier,
        head_oid,
        object_count: git_count_objects(git, repo)?,
        ref_count: git_count_refs(git, repo)?,
        file_count: git_count_tracked_files(git, repo)?,
    })
}

fn git_git_cache_valid(repo: &Path, expected_head: &str) -> Result<bool> {
    let Some(stored) = read_ready_marker(repo)? else {
        return Ok(false);
    };
    if stored != expected_head || !repo.join("HEAD").is_file() {
        return Ok(false);
    }
    verify_pinned_head(repo, expected_head)?;
    Ok(true)
}

/// Ensure `kind` is present in the cache (clone or generate when missing).
pub fn ensure_cached(
    git: &Path,
    kind: FixtureKind,
    offline: bool,
) -> Result<(PathBuf, FixtureRecord)> {
    let dest = cache_path(kind);
    match kind {
        FixtureKind::GitGit => ensure_git_git_cached(git, &dest, offline),
        FixtureKind::SynthS | FixtureKind::SynthM | FixtureKind::SynthL => {
            ensure_synth_cached(kind, &dest)
        }
    }
}

fn ensure_git_git_cached(
    git: &Path,
    dest: &Path,
    offline: bool,
) -> Result<(PathBuf, FixtureRecord)> {
    let expected = expected_git_git_head();
    if git_git_cache_valid(dest, &expected)? {
        return Ok((
            dest.to_path_buf(),
            collect_identity(git, FixtureKind::GitGit, dest)?,
        ));
    }
    if offline {
        bail!("git.git fixture missing at {} (--offline)", dest.display());
    }
    if dest.exists() {
        remove_dir_robust(dest);
    }
    let parent = dest.parent().context("cache parent")?;
    fs::create_dir_all(parent)?;
    let name = dest
        .file_name()
        .and_then(|s| s.to_str())
        .context("cache dir name")?;
    let tag = pinned_git_git_tag();
    eprintln!(
        "Cloning {GIT_GIT_URL} at tag {tag} (bare) into {} …",
        dest.display()
    );
    run_git(
        git,
        parent,
        &[
            "clone",
            "--bare",
            "-q",
            "--single-branch",
            "--branch",
            &tag,
            GIT_GIT_URL,
            name,
        ],
    )?;
    verify_pinned_head(dest, &expected)?;
    write_ready_marker(dest, &expected)?;
    Ok((
        dest.to_path_buf(),
        collect_identity(git, FixtureKind::GitGit, dest)?,
    ))
}

fn ensure_synth_cached(kind: FixtureKind, dest: &Path) -> Result<(PathBuf, FixtureRecord)> {
    let Some(tier) = kind.synth_tier() else {
        bail!("internal: ensure_synth_cached called for {kind:?}");
    };
    let git = crate::binary::resolve_binary("git", None)?;
    if dest.join(".git").is_dir() {
        if let Some(stored) = read_ready_marker(dest)? {
            let head = git_head_oid(&git, dest)?;
            if stored == head {
                return Ok((dest.to_path_buf(), collect_identity(&git, kind, dest)?));
            }
        }
        remove_dir_robust(dest);
    } else if dest.exists() {
        remove_dir_robust(dest);
    }
    fs::create_dir_all(cache_root()).context("create cache root")?;
    let report = generate_synth_repo(dest, tier)?;
    write_ready_marker(dest, &report.head)?;
    Ok((dest.to_path_buf(), collect_identity(&git, kind, dest)?))
}

/// List fixtures present in the cache (partial entries when markers are missing).
pub fn list_cached(git: &Path) -> Result<Vec<FixtureRecord>> {
    let root = cache_root();
    if !root.is_dir() {
        return Ok(Vec::new());
    }
    let mut out = Vec::new();
    for kind in FixtureKind::ALL {
        let path = cache_path(kind);
        if !path.exists() {
            continue;
        }
        if kind == FixtureKind::GitGit && !path.join("HEAD").is_file() {
            continue;
        }
        if kind != FixtureKind::GitGit && !path.join(".git").is_dir() {
            continue;
        }
        out.push(collect_identity(git, kind, &path)?);
    }
    Ok(out)
}

/// Hardlinked local clone for a mutating scenario (does not alter the cache).
pub fn copy_for_use(git: &Path, cached: &Path, dest: &Path) -> Result<PathBuf> {
    if dest.exists() {
        remove_dir_robust(dest);
    }
    let parent = dest.parent().context("dest parent")?;
    fs::create_dir_all(parent)?;
    let name = dest
        .file_name()
        .and_then(|s| s.to_str())
        .context("dest name")?;
    let src = cached.to_str().context("cached path utf-8")?;
    run_git(git, parent, &["clone", "--local", "-q", src, name])?;
    Ok(dest.to_path_buf())
}

/// Prepare selected fixtures (default: all kinds).
pub fn prepare_all(git: &Path, kinds: &[FixtureKind], offline: bool) -> Result<Vec<FixtureRecord>> {
    let kinds = if kinds.is_empty() {
        FixtureKind::ALL.to_vec()
    } else {
        kinds.to_vec()
    };
    let mut records = Vec::with_capacity(kinds.len());
    for kind in kinds {
        let (_, record) = ensure_cached(git, kind, offline)?;
        records.push(record);
    }
    Ok(records)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;
    use tempfile::TempDir;

    #[test]
    fn cache_root_honors_grit_bench_cache() {
        let dir = TempDir::new().unwrap();
        let custom = dir.path().join("bench-cache");
        std::env::set_var("GRIT_BENCH_CACHE", custom.to_str().unwrap());
        assert_eq!(cache_root(), custom);
        std::env::remove_var("GRIT_BENCH_CACHE");
    }

    #[test]
    fn cache_root_honors_xdg_cache_home() {
        let dir = TempDir::new().unwrap();
        std::env::remove_var("GRIT_BENCH_CACHE");
        std::env::set_var("XDG_CACHE_HOME", dir.path().to_str().unwrap());
        assert_eq!(cache_root(), dir.path().join("grit-bench"));
        std::env::remove_var("XDG_CACHE_HOME");
    }

    #[test]
    fn verify_pinned_head_rejects_mismatched_bare_clone() -> Result<()> {
        let dir = TempDir::new()?;
        Command::new("git")
            .args(["init", "--bare", "-b", "main"])
            .current_dir(dir.path())
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .status()?;
        let wrong = "0000000000000000000000000000000000000000";
        let err = verify_pinned_head(dir.path(), wrong).unwrap_err();
        assert!(
            err.to_string().contains("does not match pinned oid"),
            "{err}"
        );
        Ok(())
    }

    #[test]
    fn copy_for_use_is_independent_of_cache() -> Result<()> {
        let git = crate::binary::resolve_binary("git", None)?;
        let cache = TempDir::new()?;
        let work = TempDir::new()?;
        generate_synth_repo(cache.path(), SynthTier::S)?;
        write_ready_marker(cache.path(), "placeholder")?;
        let copy_path = work.path().join("work");
        copy_for_use(&git, cache.path(), &copy_path)?;
        fs::write(copy_path.join("touch.txt"), b"mutated\n")?;
        assert!(!cache.path().join("touch.txt").exists());
        Ok(())
    }

    #[test]
    fn bench_report_includes_fixture_block() -> Result<()> {
        use time::OffsetDateTime;

        let git = crate::binary::resolve_binary("git", None)?;
        let dir = TempDir::new()?;
        generate_synth_repo(dir.path(), SynthTier::S)?;
        let record = collect_identity(&git, FixtureKind::SynthS, dir.path())?;
        let report = crate::scenarios::build_report_with_fixtures(
            &git,
            &PathBuf::from("/nonexistent/grit"),
            OffsetDateTime::now_utc(),
            vec![],
            vec![record.clone()],
        );
        let json = serde_json::to_value(&report)?;
        let fixtures = json
            .get("fixtures")
            .and_then(|v| v.as_array())
            .context("fixtures array")?;
        assert_eq!(fixtures.len(), 1);
        assert_eq!(fixtures[0]["name"], "synth-s");
        assert_eq!(fixtures[0]["head_oid"], record.head_oid);
        Ok(())
    }
}

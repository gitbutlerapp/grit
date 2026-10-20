//! Cached benchmark fixtures: pinned `git.git` and grit-lib synthetic tiers.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{bail, Context, Result};
use grit_lib::bench_fixture::daily_special::{generate_daily_special_repo, DailySpecialShape};
use grit_lib::bench_fixture::synth::{
    generate_synth_repo, generate_synth_repo_with_spec, RepoShape, SynthTier,
};

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
    #[value(name = "git-git", alias = "git.git")]
    GitGit,
    /// Worktree checkout of [`Self::GitGit`] at the pinned tag.
    #[value(name = "git-git-wt")]
    GitGitWt,
    /// ~100 files, ~50 commits.
    Small,
    /// [`Self::Small`] scaled to ~1/20 (smoke).
    #[value(name = "small-smoke")]
    SmallSmoke,
    /// 10k files in ~100 dirs, ~1000 commits.
    #[value(name = "synth-10k")]
    Synth10k,
    #[value(name = "synth-10k-smoke")]
    Synth10kSmoke,
    /// 100k-file wide layout, ~1000 dirs.
    #[value(name = "synth-100k")]
    Synth100k,
    #[value(name = "synth-100k-smoke")]
    Synth100kSmoke,
    /// Synthetic tier S (grit-lib generator).
    #[value(alias = "synth-S")]
    SynthS,
    #[value(alias = "synth-M")]
    SynthM,
    #[value(alias = "synth-L")]
    SynthL,
    /// ~1k commits, ~10k packed refs.
    #[value(name = "many-refs")]
    ManyRefs,
    #[value(name = "many-refs-smoke")]
    ManyRefsSmoke,
    /// Same refs as [`Self::ManyRefs`], stored loose.
    #[value(name = "many-refs-loose")]
    ManyRefsLoose,
    #[value(name = "many-refs-loose-smoke")]
    ManyRefsLooseSmoke,
    /// Large binary blobs in a packed repo.
    #[value(name = "big-blobs")]
    BigBlobs,
    #[value(name = "big-blobs-smoke")]
    BigBlobsSmoke,
    /// Very deep directory hierarchy (~5k files).
    #[value(name = "deep-tree")]
    DeepTree,
    #[value(name = "deep-tree-smoke")]
    DeepTreeSmoke,
}

impl FixtureKind {
    /// All fixtures the registry can prepare.
    pub const ALL: [Self; 19] = [
        Self::GitGit,
        Self::GitGitWt,
        Self::Small,
        Self::SmallSmoke,
        Self::Synth10k,
        Self::Synth10kSmoke,
        Self::Synth100k,
        Self::Synth100kSmoke,
        Self::SynthS,
        Self::SynthM,
        Self::SynthL,
        Self::ManyRefs,
        Self::ManyRefsSmoke,
        Self::ManyRefsLoose,
        Self::ManyRefsLooseSmoke,
        Self::BigBlobs,
        Self::BigBlobsSmoke,
        Self::DeepTree,
        Self::DeepTreeSmoke,
    ];

    fn synth_tier(self) -> Option<SynthTier> {
        match self {
            Self::GitGit | Self::GitGitWt => None,
            Self::Small | Self::SmallSmoke => Some(SynthTier::S),
            Self::Synth10k | Self::Synth10kSmoke => Some(SynthTier::M),
            Self::Synth100k | Self::Synth100kSmoke => Some(SynthTier::L),
            Self::SynthS => Some(SynthTier::S),
            Self::SynthM => Some(SynthTier::M),
            Self::SynthL => Some(SynthTier::L),
            Self::ManyRefs
            | Self::ManyRefsSmoke
            | Self::ManyRefsLoose
            | Self::ManyRefsLooseSmoke
            | Self::BigBlobs
            | Self::BigBlobsSmoke
            | Self::DeepTree
            | Self::DeepTreeSmoke => None,
        }
    }

    fn repo_shape(self) -> Option<(RepoShape, bool)> {
        match self {
            Self::Small => Some((RepoShape::Small, false)),
            Self::SmallSmoke => Some((RepoShape::Small, true)),
            Self::Synth10k => Some((RepoShape::Synth10k, false)),
            Self::Synth10kSmoke => Some((RepoShape::Synth10k, true)),
            Self::Synth100k => Some((RepoShape::Synth100k, false)),
            Self::Synth100kSmoke => Some((RepoShape::Synth100k, true)),
            _ => None,
        }
    }

    fn daily_special_shape(self) -> Option<(DailySpecialShape, bool)> {
        match self {
            Self::ManyRefs => Some((DailySpecialShape::ManyRefs, false)),
            Self::ManyRefsSmoke => Some((DailySpecialShape::ManyRefs, true)),
            Self::ManyRefsLoose => Some((DailySpecialShape::ManyRefsLoose, false)),
            Self::ManyRefsLooseSmoke => Some((DailySpecialShape::ManyRefsLoose, true)),
            Self::BigBlobs => Some((DailySpecialShape::BigBlobs, false)),
            Self::BigBlobsSmoke => Some((DailySpecialShape::BigBlobs, true)),
            Self::DeepTree => Some((DailySpecialShape::DeepTree, false)),
            Self::DeepTreeSmoke => Some((DailySpecialShape::DeepTree, true)),
            _ => None,
        }
    }

    /// Stable cache directory name (`synth-s`, `git-git`, …).
    pub fn cache_name(self) -> &'static str {
        self.public_cache_name()
    }

    /// Stable cache directory name for reports and paths (same as [`Self::cache_name`]).
    pub fn public_cache_name(self) -> &'static str {
        match self {
            Self::GitGit => "git-git",
            Self::GitGitWt => "git-git-wt",
            Self::Small => "small",
            Self::SmallSmoke => "small-smoke",
            Self::Synth10k => "synth-10k",
            Self::Synth10kSmoke => "synth-10k-smoke",
            Self::Synth100k => "synth-100k",
            Self::Synth100kSmoke => "synth-100k-smoke",
            Self::SynthS => "synth-s",
            Self::SynthM => "synth-m",
            Self::SynthL => "synth-l",
            Self::ManyRefs => "many-refs",
            Self::ManyRefsSmoke => "many-refs-smoke",
            Self::ManyRefsLoose => "many-refs-loose",
            Self::ManyRefsLooseSmoke => "many-refs-loose-smoke",
            Self::BigBlobs => "big-blobs",
            Self::BigBlobsSmoke => "big-blobs-smoke",
            Self::DeepTree => "deep-tree",
            Self::DeepTreeSmoke => "deep-tree-smoke",
        }
    }

    /// Map a synthetic tier to the registry fixture kind.
    pub fn from_synth_tier(tier: SynthTier) -> Self {
        match tier {
            SynthTier::S => Self::SynthS,
            SynthTier::M => Self::SynthM,
            SynthTier::L => Self::SynthL,
        }
    }
}

/// Environment inputs for [`cache_root_from_env`] (testable without mutating process globals).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CacheEnv {
    /// `GRIT_BENCH_CACHE` when set.
    pub grit_bench_cache: Option<String>,
    /// `XDG_CACHE_HOME` when set.
    pub xdg_cache_home: Option<String>,
    /// `HOME` when set.
    pub home: Option<String>,
}

impl CacheEnv {
    /// Snapshot the current process environment.
    #[must_use]
    pub fn capture() -> Self {
        Self {
            grit_bench_cache: std::env::var("GRIT_BENCH_CACHE").ok(),
            xdg_cache_home: std::env::var("XDG_CACHE_HOME").ok(),
            home: std::env::var("HOME").ok(),
        }
    }
}

/// Resolve the fixture cache root from `env` (pure; safe for parallel unit tests).
///
/// Order: non-empty `grit_bench_cache`, then absolute non-empty `xdg_cache_home/grit-bench`,
/// then `home/.cache/grit-bench`, then `/tmp/grit-bench-cache`.
#[must_use]
pub fn cache_root_from_env(env: &CacheEnv) -> PathBuf {
    if let Some(p) = env.grit_bench_cache.as_ref() {
        if !p.is_empty() {
            return PathBuf::from(p);
        }
    }
    if let Some(xdg) = env.xdg_cache_home.as_ref() {
        if !xdg.is_empty() {
            let path = PathBuf::from(xdg);
            if path.is_absolute() {
                return path.join("grit-bench");
            }
        }
    }
    if let Some(home) = env.home.as_ref() {
        if !home.is_empty() {
            return PathBuf::from(home).join(".cache/grit-bench");
        }
    }
    PathBuf::from("/tmp/grit-bench-cache")
}

/// Root directory for prepared fixtures.
///
/// Resolution order: `GRIT_BENCH_CACHE`, then `$XDG_CACHE_HOME/grit-bench`, then `~/.cache/grit-bench`.
pub fn cache_root() -> PathBuf {
    cache_root_from_env(&CacheEnv::capture())
}

fn cache_path(kind: FixtureKind) -> PathBuf {
    let path = cache_root().join(kind.cache_name());
    if kind == FixtureKind::GitGit {
        let legacy = cache_root().join("git.git");
        if !path.exists() && legacy.is_dir() {
            return legacy;
        }
    }
    path
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
        FixtureKind::GitGitWt => ensure_git_git_worktree_cached(git, &dest, offline),
        FixtureKind::SynthS | FixtureKind::SynthM | FixtureKind::SynthL => {
            ensure_synth_cached(kind, &dest)
        }
        FixtureKind::Small
        | FixtureKind::SmallSmoke
        | FixtureKind::Synth10k
        | FixtureKind::Synth10kSmoke
        | FixtureKind::Synth100k
        | FixtureKind::Synth100kSmoke => ensure_shape_cached(kind, &dest),
        FixtureKind::ManyRefs
        | FixtureKind::ManyRefsSmoke
        | FixtureKind::ManyRefsLoose
        | FixtureKind::ManyRefsLooseSmoke
        | FixtureKind::BigBlobs
        | FixtureKind::BigBlobsSmoke
        | FixtureKind::DeepTree
        | FixtureKind::DeepTreeSmoke => ensure_daily_special_cached(kind, &dest),
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

fn ensure_daily_special_cached(kind: FixtureKind, dest: &Path) -> Result<(PathBuf, FixtureRecord)> {
    let Some((shape, smoke)) = kind.daily_special_shape() else {
        bail!("internal: ensure_daily_special_cached called for {kind:?}");
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
    let report = generate_daily_special_repo(dest, shape, smoke)?;
    write_ready_marker(dest, &report.head)?;
    Ok((dest.to_path_buf(), collect_identity(&git, kind, dest)?))
}

fn ensure_shape_cached(kind: FixtureKind, dest: &Path) -> Result<(PathBuf, FixtureRecord)> {
    let Some((shape, smoke)) = kind.repo_shape() else {
        bail!("internal: ensure_shape_cached called for {kind:?}");
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
    let spec = shape.spec(smoke);
    let report = generate_synth_repo_with_spec(dest, spec)?;
    write_ready_marker(dest, &report.head)?;
    Ok((dest.to_path_buf(), collect_identity(&git, kind, dest)?))
}

fn ensure_git_git_worktree_cached(
    git: &Path,
    dest: &Path,
    offline: bool,
) -> Result<(PathBuf, FixtureRecord)> {
    let expected = expected_git_git_head();
    if dest.join(".git").is_dir() {
        if let Some(stored) = read_ready_marker(dest)? {
            if stored == expected {
                verify_pinned_head(dest, &expected)?;
                return Ok((
                    dest.to_path_buf(),
                    collect_identity(git, FixtureKind::GitGitWt, dest)?,
                ));
            }
        }
        remove_dir_robust(dest);
    } else if dest.exists() {
        remove_dir_robust(dest);
    }
    let (bare, _) = ensure_cached(git, FixtureKind::GitGit, offline)?;
    if offline && !bare.is_dir() {
        bail!(
            "git-git-wt fixture missing at {} (--offline)",
            dest.display()
        );
    }
    let parent = dest.parent().context("worktree cache parent")?;
    fs::create_dir_all(parent)?;
    let name = dest
        .file_name()
        .and_then(|s| s.to_str())
        .context("worktree cache dir name")?;
    let bare_str = bare.to_str().context("bare path utf-8")?;
    eprintln!(
        "Cloning bare {} into worktree {} …",
        bare.display(),
        dest.display()
    );
    run_git(git, parent, &["clone", "-q", "--local", bare_str, name])?;
    verify_pinned_head(dest, &expected)?;
    write_ready_marker(dest, &expected)?;
    Ok((
        dest.to_path_buf(),
        collect_identity(git, FixtureKind::GitGitWt, dest)?,
    ))
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
        if kind == FixtureKind::GitGitWt && !path.join(".git").is_dir() {
            continue;
        }
        if kind != FixtureKind::GitGit
            && kind != FixtureKind::GitGitWt
            && !path.join(".git").is_dir()
        {
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
    fn cache_root_from_env_honors_grit_bench_cache() {
        let custom = "/tmp/grit-bench-custom-root";
        let env = CacheEnv {
            grit_bench_cache: Some(custom.into()),
            ..CacheEnv::default()
        };
        assert_eq!(cache_root_from_env(&env), PathBuf::from(custom));
    }

    #[test]
    fn cache_root_from_env_honors_absolute_xdg_cache_home() {
        let xdg = "/var/tmp/xdg-cache";
        let env = CacheEnv {
            xdg_cache_home: Some(xdg.into()),
            ..CacheEnv::default()
        };
        assert_eq!(
            cache_root_from_env(&env),
            PathBuf::from(xdg).join("grit-bench")
        );
    }

    #[test]
    fn cache_root_from_env_skips_empty_or_relative_xdg() {
        let env = CacheEnv {
            xdg_cache_home: Some(String::new()),
            home: Some("/home/user".into()),
            ..CacheEnv::default()
        };
        assert_eq!(
            cache_root_from_env(&env),
            PathBuf::from("/home/user/.cache/grit-bench")
        );
        let env = CacheEnv {
            xdg_cache_home: Some("relative/cache".into()),
            home: Some("/home/user".into()),
            ..CacheEnv::default()
        };
        assert_eq!(
            cache_root_from_env(&env),
            PathBuf::from("/home/user/.cache/grit-bench")
        );
    }

    fn bare_clone_with_commit(parent: &Path) -> Result<PathBuf> {
        let work = parent.join("work");
        fs::create_dir_all(&work)?;
        Command::new("git")
            .args(["init", "-b", "main"])
            .current_dir(&work)
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .status()?;
        fs::write(work.join("f.txt"), b"seed\n")?;
        Command::new("git")
            .args(["add", "f.txt"])
            .current_dir(&work)
            .status()?;
        Command::new("git")
            .args(["commit", "-m", "seed"])
            .current_dir(&work)
            .env("GIT_AUTHOR_NAME", "t")
            .env("GIT_AUTHOR_EMAIL", "t@example.com")
            .env("GIT_COMMITTER_NAME", "t")
            .env("GIT_COMMITTER_EMAIL", "t@example.com")
            .status()?;
        let bare = parent.join("standin.git");
        let status = Command::new("git")
            .args([
                "clone",
                "--bare",
                work.to_str().context("work utf-8")?,
                bare.to_str().context("bare utf-8")?,
            ])
            .current_dir(parent)
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .status()?;
        anyhow::ensure!(status.success(), "git clone --bare failed");
        Ok(bare)
    }

    #[test]
    fn verify_pinned_head_rejects_mismatched_bare_clone() -> Result<()> {
        let parent = TempDir::new()?;
        let bare = bare_clone_with_commit(parent.path())?;
        let git = crate::binary::resolve_binary("git", None)?;
        let actual = git_head_oid(&git, &bare)?;
        assert_ne!(actual.len(), 0);
        assert!(actual.chars().all(|c| c.is_ascii_hexdigit()));
        let wrong = "0000000000000000000000000000000000000000";
        let err = verify_pinned_head(&bare, wrong).unwrap_err();
        assert!(
            err.to_string().contains("does not match pinned oid"),
            "{err}"
        );
        verify_pinned_head(&bare, &actual)?;
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

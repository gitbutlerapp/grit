//! Cached real-world and chromium-class benchmark repositories.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{bail, Context, Result};

use crate::bench_env::empty_global_config_path;
use crate::fixture::remove_dir_robust;
use crate::odb_fixture::odb_scratch_root;
use crate::serve_request::ensure_serve_clone_request;

const GIT_GIT_URL: &str = "https://github.com/git/git.git";
const LINUX_URL: &str = "https://github.com/torvalds/linux.git";

/// Default upstream tag for git.git (override with `GRIT_BENCH_GIT_GIT_TAG`).
pub const DEFAULT_GIT_GIT_TAG: &str = "v2.47.0";

/// Default linux tag (override with `GRIT_BENCH_LINUX_TAG`).
pub const DEFAULT_LINUX_TAG: &str = "v6.12";

const BITMAP_READY: &str = ".grit-bench-bitmaps-ready";
const NO_BITMAPS_READY: &str = ".grit-bench-no-bitmaps-ready";
const WORKTREE_READY: &str = ".grit-bench-worktree-ready";

/// Scratch root shared by real-repo fixtures (same as ODB cache unless overridden).
pub fn scratch_root() -> PathBuf {
    odb_scratch_root()
}

pub(crate) fn git_env(cmd: &mut Command) {
    cmd.env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", empty_global_config_path())
        .env("GIT_CONFIG_SYSTEM", "/dev/null");
}

pub(crate) fn run_git(git: &Path, dir: &Path, args: &[&str]) -> Result<()> {
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

fn rev_parse(git: &Path, dir: &Path, rev: &str) -> Result<String> {
    let mut cmd = Command::new(git);
    cmd.args(["rev-parse", rev]).current_dir(dir);
    git_env(&mut cmd);
    let out = cmd
        .output()
        .with_context(|| format!("git rev-parse {rev}"))?;
    if !out.status.success() {
        bail!(
            "git rev-parse {rev} failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

fn parse_worktree_marker(contents: &str) -> Result<(String, Option<String>)> {
    let lines: Vec<&str> = contents
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .collect();
    match lines.as_slice() {
        [tag] => Ok(((*tag).to_string(), None)),
        [tag, oid] if oid.len() >= 40 => Ok(((*tag).to_string(), Some((*oid).to_string()))),
        _ => bail!("invalid worktree marker format"),
    }
}

fn write_worktree_marker(marker: &Path, tag: &str, tip_oid: &str) -> Result<()> {
    fs::write(marker, format!("{tag}\n{tip_oid}")).context("write worktree marker")
}

fn bare_default_head_oid(git: &Path, bare: &Path) -> Result<String> {
    rev_parse(git, bare, "HEAD")
}

fn restore_cached_worktree_to_fixture_tip(
    git: &Path,
    bare: &Path,
    dest: &Path,
    tag: &str,
    marker_contents: &str,
) -> Result<()> {
    let (stored_tag, stored_oid) = parse_worktree_marker(marker_contents)?;
    if stored_tag != tag {
        bail!("worktree marker tag mismatch");
    }
    let tip = match stored_oid {
        Some(oid) => oid,
        None => bare_default_head_oid(git, bare)?,
    };
    run_git(
        git,
        dest,
        &["checkout", "-f", "-B", "campaign-main", tip.as_str()],
    )?;
    run_git(
        git,
        dest,
        &[
            "clean",
            "-fd",
            "-q",
            "-e",
            WORKTREE_READY,
            "-e",
            ".grit-bench-hot-path.json",
        ],
    )?;
    Ok(())
}

fn pinned_git_git_tag() -> String {
    std::env::var("GRIT_BENCH_GIT_GIT_TAG").unwrap_or_else(|_| DEFAULT_GIT_GIT_TAG.to_owned())
}

fn pinned_linux_tag() -> String {
    std::env::var("GRIT_BENCH_LINUX_TAG").unwrap_or_else(|_| DEFAULT_LINUX_TAG.to_owned())
}

/// Whether linux fixtures are enabled (`--with-linux` or `GRIT_BENCH_LINUX=1`).
pub fn linux_fixture_enabled(cli_flag: bool) -> bool {
    cli_flag
        || std::env::var("GRIT_BENCH_LINUX")
            .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
            .unwrap_or(false)
}

/// Path to the bitmap-prepared bare `git.git` clone.
pub fn git_git_bitmaps_cache_path() -> PathBuf {
    std::env::var("GRIT_BENCH_BITMAPS_REPO")
        .map(PathBuf::from)
        .unwrap_or_else(|_| scratch_root().join("git.git-bitmaps"))
}

fn stored_marker(dest: &Path, name: &str) -> Result<Option<String>> {
    let path = dest.join(name);
    if !path.is_file() {
        return Ok(None);
    }
    Ok(Some(
        fs::read_to_string(&path)
            .context("read fixture marker")?
            .trim()
            .to_owned(),
    ))
}

/// Branch refs that must not appear after a single-branch tag clone.
pub fn list_branch_head_refs(repo: &Path) -> Result<Vec<String>> {
    let mut refs = grit_lib::refs::list_refs(repo, "refs/heads/")?;
    refs.sort_by(|a, b| a.0.cmp(&b.0));
    Ok(refs.into_iter().map(|(name, _)| name).collect())
}

/// Remove branch tips introduced by campaign network scenarios on a tag-pinned bare remote.
pub fn clear_branch_head_refs(git: &Path, repo: &Path) -> Result<()> {
    for name in list_branch_head_refs(repo)? {
        let full = if name.starts_with("refs/") {
            name
        } else {
            format!("refs/heads/{name}")
        };
        run_git(git, repo, &["update-ref", "-d", &full])?;
    }
    Ok(())
}

/// A tag-only bitmap fixture must not retain upstream branch tips.
pub fn assert_tag_pinned_fixture_refs(repo: &Path) -> Result<()> {
    let heads = list_branch_head_refs(repo)?;
    if !heads.is_empty() {
        bail!(
            "tag-pinned fixture must not contain branch refs after --single-branch tag clone; found: {}",
            heads.join(", ")
        );
    }
    Ok(())
}

fn git_git_bitmaps_current(dest: &Path, tag: &str) -> Result<bool> {
    let Some(stored) = stored_marker(dest, BITMAP_READY)? else {
        return Ok(false);
    };
    if stored != tag || !dest.join("HEAD").is_file() {
        return Ok(false);
    }
    Ok(assert_tag_pinned_fixture_refs(dest).is_ok())
}

/// Ensure a bare clone of upstream `git.git` at a pinned tag, repacked with pack bitmaps and commit-graph.
pub fn ensure_git_git_bitmaps(git: &Path) -> Result<PathBuf> {
    let dest = git_git_bitmaps_cache_path();
    let tag = pinned_git_git_tag();
    if git_git_bitmaps_current(&dest, &tag)? {
        return Ok(dest);
    }
    if dest.exists() {
        if stored_marker(&dest, BITMAP_READY)?.as_deref() == Some(tag.as_str()) {
            clear_branch_head_refs(git, &dest).ok();
            if git_git_bitmaps_current(&dest, &tag)? {
                return Ok(dest);
            }
        }
        remove_dir_robust(&dest);
    }
    let parent = dest.parent().unwrap_or(Path::new("/tmp"));
    fs::create_dir_all(parent)?;
    let name = dest
        .file_name()
        .and_then(|s| s.to_str())
        .ok_or_else(|| anyhow::anyhow!("invalid git.git cache path {}", dest.display()))?;
    eprintln!(
        "Cloning {GIT_GIT_URL} at tag {tag} (bare, single-branch) into {} …",
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
    assert_tag_pinned_fixture_refs(&dest)?;
    eprintln!("Preparing git.git fixture (repack -adb, commit-graph write --reachable) …");
    run_git(git, &dest, &["repack", "-a", "-d", "-b"])?;
    run_git(git, &dest, &["commit-graph", "write", "--reachable"])?;
    ensure_serve_clone_request(&dest)?;
    fs::write(dest.join(BITMAP_READY), tag.as_bytes()).context("write bitmap ready marker")?;
    Ok(dest)
}

/// Path to the pinned `git.git` fixture copy without pack bitmaps.
pub fn git_git_no_bitmaps_cache_path() -> PathBuf {
    std::env::var("GRIT_BENCH_NO_BITMAPS_REPO")
        .map(PathBuf::from)
        .unwrap_or_else(|_| scratch_root().join("git.git-no-bitmaps"))
}

/// Ensure a copy of the [`ensure_git_git_bitmaps`] fixture with its pack bitmap
/// removed: one Git-written pack with a `.rev` reverse index, like a server that
/// never wrote bitmaps. Serving it exercises delta reuse without bitmap reuse.
pub fn ensure_git_git_no_bitmaps(git: &Path) -> Result<PathBuf> {
    let src = ensure_git_git_bitmaps(git)?;
    let dest = git_git_no_bitmaps_cache_path();
    let tag = pinned_git_git_tag();
    if stored_marker(&dest, NO_BITMAPS_READY)?.as_deref() == Some(tag.as_str()) {
        return Ok(dest);
    }
    if dest.exists() {
        remove_dir_robust(&dest);
    }
    fs::create_dir_all(&dest)?;
    crate::refs_fixture::copy_tree_into(&src, &dest)
        .with_context(|| format!("copy {} to {}", src.display(), dest.display()))?;
    let _ = fs::remove_file(dest.join(BITMAP_READY));
    for entry in fs::read_dir(dest.join("objects/pack"))? {
        let path = entry?.path();
        if path.extension().is_some_and(|ext| ext == "bitmap") {
            fs::remove_file(&path)?;
        }
    }
    ensure_serve_clone_request(&dest)?;
    fs::write(dest.join(NO_BITMAPS_READY), tag.as_bytes())
        .context("write no-bitmaps ready marker")?;
    Ok(dest)
}

/// Path to cached linux bare clone.
pub fn linux_bare_cache_path() -> PathBuf {
    std::env::var("GRIT_BENCH_LINUX_REPO")
        .map(PathBuf::from)
        .unwrap_or_else(|_| scratch_root().join("linux-bitmaps"))
}

const LINUX_READY: &str = ".grit-bench-linux-ready";

fn linux_current(dest: &Path, tag: &str) -> Result<bool> {
    let Some(stored) = stored_marker(dest, LINUX_READY)? else {
        return Ok(false);
    };
    Ok(stored == tag && dest.join("HEAD").is_file())
}

/// Ensure a bare linux kernel clone at a pinned tag, repacked with commit-graph.
pub fn ensure_linux_bare(git: &Path) -> Result<PathBuf> {
    let dest = linux_bare_cache_path();
    let tag = pinned_linux_tag();
    if linux_current(&dest, &tag)? {
        return Ok(dest);
    }
    if dest.exists() {
        remove_dir_robust(&dest);
    }
    let parent = dest.parent().unwrap_or(Path::new("/tmp"));
    fs::create_dir_all(parent)?;
    let name = dest
        .file_name()
        .and_then(|s| s.to_str())
        .ok_or_else(|| anyhow::anyhow!("invalid linux cache path {}", dest.display()))?;
    eprintln!(
        "Cloning {LINUX_URL} at tag {tag} (bare, single-branch) into {} …",
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
            LINUX_URL,
            name,
        ],
    )?;
    eprintln!("Preparing linux fixture (repack -adf, commit-graph write --reachable) …");
    run_git(git, &dest, &["repack", "-a", "-d", "-f"])?;
    run_git(git, &dest, &["commit-graph", "write", "--reachable"])?;
    fs::write(dest.join(LINUX_READY), tag.as_bytes()).context("write linux ready marker")?;
    Ok(dest)
}

/// Cached worktree clone of a bare fixture for CLI scenarios.
pub fn ensure_worktree_clone(
    git: &Path,
    bare: &Path,
    worktree_name: &str,
    tag: &str,
) -> Result<PathBuf> {
    let dest = scratch_root().join(worktree_name);
    let marker = dest.join(WORKTREE_READY);
    if marker.is_file() {
        let stored = fs::read_to_string(&marker).context("read worktree marker")?;
        if dest.join(".git").exists() {
            if let Ok((stored_tag, _)) = parse_worktree_marker(&stored) {
                if stored_tag == tag {
                    restore_cached_worktree_to_fixture_tip(git, bare, &dest, tag, &stored)?;
                    return Ok(dest);
                }
            } else if stored.trim() == tag {
                restore_cached_worktree_to_fixture_tip(git, bare, &dest, tag, &stored)?;
                return Ok(dest);
            }
        }
    }
    if dest.exists() {
        remove_dir_robust(&dest);
    }
    let parent = dest.parent().unwrap_or(Path::new("/tmp"));
    fs::create_dir_all(parent)?;
    let bare_str = bare.to_str().context("bare path utf-8")?;
    let name = dest
        .file_name()
        .and_then(|s| s.to_str())
        .context("worktree name")?;
    eprintln!("Cloning {bare_str} into worktree {} …", dest.display());
    run_git(
        git,
        parent,
        &["clone", "-q", "--single-branch", bare_str, name],
    )?;
    run_git(git, &dest, &["checkout", "-B", "campaign-main"])?;
    let tip = rev_parse(git, &dest, "HEAD")?;
    write_worktree_marker(&marker, tag, &tip)?;
    Ok(dest)
}

/// Isolated git config prefix for bitmap scenarios (matches ODB benchmarks).
pub fn bitmap_isolated_env() -> String {
    crate::bench_env::isolated_env_prefix()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;
    use tempfile::TempDir;

    fn git_init_bare_with_branches(dir: &Path) -> Result<()> {
        Command::new("git")
            .args(["init", "--bare", "-b", "master"])
            .current_dir(dir)
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .status()
            .context("git init bare")?
            .success()
            .then_some(())
            .ok_or_else(|| anyhow::anyhow!("git init bare failed"))?;
        Ok(())
    }

    #[test]
    fn single_branch_tag_clone_drops_other_branch_refs() -> Result<()> {
        let origin = TempDir::new()?;
        git_init_bare_with_branches(origin.path())?;
        let source = TempDir::new()?;
        Command::new("git")
            .args(["clone", origin.path().to_str().unwrap(), "work"])
            .current_dir(source.path())
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .status()?;
        let work = source.path().join("work");
        fs::write(work.join("file.txt"), b"v1")?;
        Command::new("git")
            .args(["add", "file.txt"])
            .current_dir(&work)
            .status()?;
        Command::new("git")
            .args(["commit", "-m", "one"])
            .current_dir(&work)
            .env("GIT_AUTHOR_NAME", "t")
            .env("GIT_AUTHOR_EMAIL", "t@t")
            .env("GIT_COMMITTER_NAME", "t")
            .env("GIT_COMMITTER_EMAIL", "t@t")
            .status()?;
        Command::new("git")
            .args(["tag", "v1.0"])
            .current_dir(&work)
            .status()?;
        Command::new("git")
            .args(["branch", "moving"])
            .current_dir(&work)
            .status()?;
        Command::new("git")
            .args(["push", "origin", "master", "moving", "v1.0"])
            .current_dir(&work)
            .status()?;

        let clone_parent = TempDir::new()?;
        let clone_path = clone_parent.path().join("dest.git");
        Command::new("git")
            .args([
                "clone",
                "--bare",
                "--single-branch",
                "--branch",
                "v1.0",
                origin.path().to_str().unwrap(),
                clone_path.to_str().unwrap(),
            ])
            .current_dir(clone_parent.path())
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .status()?;
        assert_tag_pinned_fixture_refs(&clone_path)?;
        let heads = list_branch_head_refs(&clone_path)?;
        assert!(heads.is_empty(), "unexpected heads: {heads:?}");
        Ok(())
    }

    #[test]
    fn cached_worktree_reuse_restores_fixture_tip_not_leftover_head() -> Result<()> {
        let git = PathBuf::from("git");
        let scratch = TempDir::new()?;
        std::env::set_var("GRIT_BENCH_ODB_CACHE", scratch.path());

        let origin = TempDir::new()?;
        run_git(&git, origin.path(), &["init", "-q", "-b", "campaign-main"])?;
        fs::write(origin.path().join("tracked.txt"), b"base\n")?;
        run_git(&git, origin.path(), &["add", "tracked.txt"])?;
        run_git_with_identity(&git, origin.path(), &["commit", "-qm", "base"])?;
        let base_oid = rev_parse(&git, origin.path(), "HEAD")?;

        run_git(&git, origin.path(), &["checkout", "-B", "topic"])?;
        fs::write(origin.path().join("tracked.txt"), b"topic\n")?;
        run_git(&git, origin.path(), &["add", "tracked.txt"])?;
        run_git_with_identity(&git, origin.path(), &["commit", "-qm", "topic"])?;
        let topic_oid = rev_parse(&git, origin.path(), "HEAD")?;
        assert_ne!(base_oid, topic_oid);
        run_git(&git, origin.path(), &["checkout", "-f", "campaign-main"])?;

        let bare = scratch.path().join("fixture.git");
        run_git(
            &git,
            scratch.path(),
            &[
                "clone",
                "--bare",
                "-q",
                origin.path().to_str().unwrap(),
                "fixture.git",
            ],
        )?;

        let tag = "fixture-v1";
        let dest = ensure_worktree_clone(&git, &bare, "reuse-wt", tag)?;
        assert_eq!(rev_parse(&git, &dest, "HEAD")?, base_oid);
        assert_eq!(rev_parse(&git, &dest, "campaign-main")?, base_oid);

        run_git(&git, &dest, &["checkout", "-f", "campaign-main"])?;
        run_git(&git, &dest, &["checkout", "--detach", topic_oid.as_str()])?;
        assert_eq!(rev_parse(&git, &dest, "HEAD")?, topic_oid);
        assert_eq!(
            rev_parse(&git, &dest, "campaign-main")?,
            base_oid,
            "campaign-main must stay on base before reuse"
        );

        let dest2 = ensure_worktree_clone(&git, &bare, "reuse-wt", tag)?;
        assert_eq!(dest, dest2);
        assert_eq!(rev_parse(&git, &dest2, "HEAD")?, base_oid);
        assert_eq!(rev_parse(&git, &dest2, "campaign-main")?, base_oid);
        assert_eq!(fs::read_to_string(dest2.join("tracked.txt"))?, "base\n");

        std::env::remove_var("GRIT_BENCH_ODB_CACHE");
        Ok(())
    }

    #[test]
    fn clear_branch_head_refs_deletes_full_ref_names() -> Result<()> {
        let bare = TempDir::new()?;
        git_init_bare_with_branches(bare.path())?;
        let work = TempDir::new()?;
        Command::new("git")
            .args(["clone", bare.path().to_str().unwrap(), "w"])
            .current_dir(work.path())
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .status()?;
        let w = work.path().join("w");
        fs::write(w.join("f"), b"x")?;
        Command::new("git")
            .args(["add", "f"])
            .current_dir(&w)
            .status()?;
        Command::new("git")
            .args(["commit", "-m", "c"])
            .current_dir(&w)
            .env("GIT_AUTHOR_NAME", "t")
            .env("GIT_AUTHOR_EMAIL", "t@t")
            .env("GIT_COMMITTER_NAME", "t")
            .env("GIT_COMMITTER_EMAIL", "t@t")
            .status()?;
        Command::new("git")
            .args(["push", "origin", "master:synthetic-main"])
            .current_dir(&w)
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .status()?;
        let before = list_branch_head_refs(bare.path())?;
        assert_eq!(before, vec!["refs/heads/synthetic-main".to_owned()]);
        clear_branch_head_refs(Path::new("git"), bare.path())?;
        assert_tag_pinned_fixture_refs(bare.path())?;
        Ok(())
    }

    fn run_git_with_identity(git: &Path, dir: &Path, args: &[&str]) -> Result<()> {
        let mut cmd = Command::new(git);
        cmd.args(args).current_dir(dir);
        git_env(&mut cmd);
        cmd.env("GIT_AUTHOR_NAME", "Bench")
            .env("GIT_AUTHOR_EMAIL", "b@example.com")
            .env("GIT_COMMITTER_NAME", "Bench")
            .env("GIT_COMMITTER_EMAIL", "b@example.com");
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

    #[test]
    fn assert_tag_pinned_fails_on_campaign_branch_pollution_before_clear() -> Result<()> {
        let origin = TempDir::new()?;
        git_init_bare_with_branches(origin.path())?;
        let source = TempDir::new()?;
        Command::new("git")
            .args(["clone", origin.path().to_str().unwrap(), "work"])
            .current_dir(source.path())
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .status()?;
        let work = source.path().join("work");
        fs::write(work.join("file.txt"), b"v1")?;
        Command::new("git")
            .args(["add", "file.txt"])
            .current_dir(&work)
            .status()?;
        Command::new("git")
            .args(["commit", "-m", "one"])
            .current_dir(&work)
            .env("GIT_AUTHOR_NAME", "t")
            .env("GIT_AUTHOR_EMAIL", "t@t")
            .env("GIT_COMMITTER_NAME", "t")
            .env("GIT_COMMITTER_EMAIL", "t@t")
            .status()?;
        Command::new("git")
            .args(["tag", "v1.0"])
            .current_dir(&work)
            .status()?;
        Command::new("git")
            .args(["push", "origin", "master", "v1.0"])
            .current_dir(&work)
            .status()?;

        let clone_parent = TempDir::new()?;
        let bare = clone_parent.path().join("bare.git");
        Command::new("git")
            .args([
                "clone",
                "--bare",
                "--single-branch",
                "--branch",
                "v1.0",
                origin.path().to_str().unwrap(),
                bare.to_str().unwrap(),
            ])
            .current_dir(clone_parent.path())
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .status()?;
        assert_tag_pinned_fixture_refs(&bare)?;

        let git = Path::new("git");
        let oid = Command::new("git")
            .args(["rev-parse", "v1.0"])
            .current_dir(&bare)
            .output()?;
        assert!(oid.status.success());
        let oid = String::from_utf8_lossy(&oid.stdout).trim().to_string();
        run_git(
            git,
            &bare,
            &["update-ref", "refs/heads/campaign-main", &oid],
        )?;
        assert!(
            assert_tag_pinned_fixture_refs(&bare).is_err(),
            "bitmap --repo must reject branch pollution before mutating refs"
        );
        clear_branch_head_refs(git, &bare)?;
        assert_tag_pinned_fixture_refs(&bare)?;
        Ok(())
    }
}

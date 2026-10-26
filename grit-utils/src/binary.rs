//! Resolve executables on `PATH` without shelling out to `which`.

use std::env;
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};

/// Resolve a binary: explicit user path, workspace `target/dist/grit` or `target/release/grit`, then `PATH`.
///
/// Explicit paths are made absolute and canonicalized so they remain valid after the
/// harness changes working directory for hyperfine runs.
pub fn resolve_binary(name: &str, user_path: Option<&Path>) -> Result<PathBuf> {
    if let Some(p) = user_path {
        return canonicalize_executable(p);
    }
    if name == "grit" {
        if let Some(p) = workspace_executable("grit") {
            return Ok(p);
        }
    }
    if name == "grit-http-server" {
        if let Some(p) = workspace_executable("grit-http-server") {
            return Ok(p);
        }
        let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        let debug = manifest.join("../target/debug/grit-http-server");
        if debug.is_file() {
            return debug
                .canonicalize()
                .context("canonicalize grit-http-server path");
        }
    }
    find_on_path(name).with_context(|| format!("could not find `{name}` on PATH"))
}

/// Prefer `target/dist/<name>` (shipped profile), then `target/release/<name>`.
fn workspace_executable(name: &str) -> Option<PathBuf> {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("..");
    for profile in ["dist", "release"] {
        let candidate = root.join("target").join(profile).join(name);
        if candidate.is_file() {
            return candidate
                .canonicalize()
                .context("canonicalize workspace executable")
                .ok();
        }
    }
    None
}

fn canonicalize_executable(p: &Path) -> Result<PathBuf> {
    let abs = if p.is_relative() {
        env::current_dir()
            .context("resolve relative executable path")?
            .join(p)
    } else {
        p.to_path_buf()
    };
    if !abs.is_file() {
        bail!("binary not found: {}", p.display());
    }
    abs.canonicalize()
        .with_context(|| format!("canonicalize executable {}", p.display()))
}

/// Locate `hyperfine` or fail with an actionable message.
/// Resolve `grit-http-server` (explicit path, workspace release build, then `PATH`).
pub fn resolve_http_server(user_path: Option<&Path>) -> Result<PathBuf> {
    resolve_binary("grit-http-server", user_path)
}

pub fn require_hyperfine() -> Result<PathBuf> {
    find_on_path("hyperfine").context(
        "hyperfine is required for grit-bench but was not found on PATH. \
         Install it from https://github.com/sharkdp/hyperfine",
    )
}

/// Run `program --version` (or `-V` for grit) and return stdout trimmed.
pub fn tool_version(program: &Path) -> String {
    let output = std::process::Command::new(program)
        .arg("--version")
        .output()
        .ok()
        .filter(|o| o.status.success())
        .or_else(|| {
            std::process::Command::new(program)
                .arg("-V")
                .output()
                .ok()
                .filter(|o| o.status.success())
        });
    output
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| s.trim().to_string())
        .unwrap_or_else(|| "unknown".into())
}

/// Resolve `spec` to a commit object id, or `None` when it is not a committish.
fn rev_parse_commit(repo: &Path, spec: &str) -> Option<String> {
    let out = std::process::Command::new("git")
        .args(["rev-parse", "--verify", &format!("{spec}^{{commit}}")])
        .current_dir(repo)
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    String::from_utf8(out.stdout)
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

fn git_current_branch(repo: &Path) -> Option<String> {
    let out = std::process::Command::new("git")
        .args(["branch", "--show-current"])
        .current_dir(repo)
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let name = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if name.is_empty() {
        None
    } else {
        Some(name)
    }
}

fn is_ancestor(repo: &Path, ancestor: &str, descendant: &str) -> bool {
    std::process::Command::new("git")
        .args(["merge-base", "--is-ancestor", ancestor, descendant])
        .current_dir(repo)
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// Among `refs/heads/factory/*` tips that are ancestors of `HEAD`, pick the newest by tip committer date.
fn factory_branch_tip_in_workspace(repo: &Path) -> Option<String> {
    let head = rev_parse_commit(repo, "HEAD")?;
    let out = std::process::Command::new("git")
        .args([
            "for-each-ref",
            "refs/heads/factory/",
            "--format=%(objectname)\t%(committerdate:unix)",
        ])
        .current_dir(repo)
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let mut best: Option<(i64, String)> = None;
    for line in String::from_utf8_lossy(&out.stdout).lines() {
        let Some((tip, date_str)) = line.split_once('\t') else {
            continue;
        };
        let tip = tip.trim();
        if tip.is_empty() {
            continue;
        }
        if rev_parse_commit(repo, tip).is_none() {
            continue;
        }
        if !is_ancestor(repo, tip, &head) {
            continue;
        }
        let Ok(date) = date_str.trim().parse::<i64>() else {
            continue;
        };
        match &best {
            None => best = Some((date, tip.to_owned())),
            Some((d, _)) if date > *d => best = Some((date, tip.to_owned())),
            Some(_) => {}
        }
    }
    best.map(|(_, tip)| tip)
}

/// Best-effort commit hash for the grit tree (when this crate is built from the grit repo).
///
/// Resolution order:
/// 1. `bench_branch` when set (e.g. `factory/refs-bench-774` via `GRIT_BENCH_BRANCH`)
/// 2. The checked-out branch tip when it is not `gitbutler/workspace`
/// 3. On GitButler workspace: the newest `refs/heads/factory/*` tip that is an ancestor of `HEAD`
/// 4. `HEAD` when it resolves to a commit object
pub(crate) fn grit_source_commit_from(repo: &Path, bench_branch: Option<&str>) -> Option<String> {
    if let Some(branch) = bench_branch.map(str::trim).filter(|b| !b.is_empty()) {
        if let Some(h) = rev_parse_commit(repo, &format!("refs/heads/{branch}")) {
            return Some(h);
        }
    }
    if let Some(branch) = git_current_branch(repo) {
        if branch != "gitbutler/workspace" {
            if let Some(h) = rev_parse_commit(repo, &format!("refs/heads/{branch}")) {
                return Some(h);
            }
        }
    }
    if let Some(h) = factory_branch_tip_in_workspace(repo) {
        return Some(h);
    }
    rev_parse_commit(repo, "HEAD")
}

/// Best-effort commit hash for the grit tree (when this crate is built from the grit repo).
pub fn grit_source_commit() -> Option<String> {
    let repo = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("..");
    let bench_branch = env::var("GRIT_BENCH_BRANCH").ok();
    grit_source_commit_from(&repo, bench_branch.as_deref())
}

/// Commit to record for the timed grit binary: `GRIT_BENCH_REPORT_GRIT_COMMIT` when set
/// (the binary under test was built elsewhere), else [`grit_source_commit`].
pub fn report_grit_commit() -> Option<String> {
    env::var("GRIT_BENCH_REPORT_GRIT_COMMIT")
        .ok()
        .filter(|s| !s.is_empty())
        .or_else(grit_source_commit)
}

fn find_on_path(name: &str) -> Result<PathBuf> {
    let paths = env::var_os("PATH").context("PATH is not set")?;
    for dir in env::split_paths(&paths) {
        let candidate = dir.join(name);
        if candidate.is_file() {
            return candidate
                .canonicalize()
                .with_context(|| format!("canonicalize `{}` on PATH", candidate.display()));
        }
    }
    bail!("`{name}` not found on PATH")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::process::Command;
    use tempfile::TempDir;

    #[cfg_attr(
        not(feature = "full-tests"),
        ignore = "starts system git or is heavy; run with --features full-tests"
    )]
    #[test]
    fn report_grit_commit_prefers_the_override() {
        env::set_var("GRIT_BENCH_REPORT_GRIT_COMMIT", "0123abcd-override");
        let with_override = report_grit_commit();
        env::set_var("GRIT_BENCH_REPORT_GRIT_COMMIT", "");
        let empty_override = report_grit_commit();
        env::remove_var("GRIT_BENCH_REPORT_GRIT_COMMIT");
        assert_eq!(with_override.as_deref(), Some("0123abcd-override"));
        assert_eq!(empty_override, grit_source_commit());
    }

    #[cfg_attr(
        not(feature = "full-tests"),
        ignore = "starts system git or is heavy; run with --features full-tests"
    )]
    #[test]
    fn grit_source_commit_is_committish() {
        let Some(commit) = grit_source_commit() else {
            return;
        };
        let repo = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("..");
        let ok = Command::new("git")
            .args(["cat-file", "-e", &format!("{commit}^{{commit}}")])
            .current_dir(&repo)
            .status()
            .expect("git cat-file");
        assert!(
            ok.success(),
            "grit_source_commit must resolve to a commit object"
        );
    }

    fn init_temp_repo_with_branch(branch: &str) -> (TempDir, String) {
        let dir = TempDir::new().expect("tempdir");
        let repo = dir.path();
        let run = |args: &[&str]| {
            let status = Command::new("git")
                .args(args)
                .current_dir(repo)
                .status()
                .expect("git");
            assert!(status.success(), "git {}", args.join(" "));
        };
        run(&["init", "-b", "main"]);
        run(&["config", "user.email", "test@example.com"]);
        run(&["config", "user.name", "Test"]);
        fs::write(repo.join("README"), "bench\n").expect("readme");
        run(&["add", "README"]);
        run(&["commit", "-m", "init"]);
        let tip = rev_parse_commit(repo, "HEAD").expect("head");
        run(&["branch", branch, &tip]);
        (dir, tip)
    }

    #[cfg_attr(
        not(feature = "full-tests"),
        ignore = "starts system git or is heavy; run with --features full-tests"
    )]
    #[test]
    fn grit_source_commit_honors_bench_branch() {
        let branch = "factory/bench-branch-for-test";
        let (dir, expected) = init_temp_repo_with_branch(branch);
        assert_eq!(
            grit_source_commit_from(dir.path(), Some(branch)).as_deref(),
            Some(expected.as_str())
        );
    }

    #[test]
    fn relative_explicit_path_is_canonicalized() {
        let original_cwd = env::current_dir().expect("initial cwd");
        let dir = TempDir::new().unwrap();
        let script = dir.path().join("fake-grit");
        fs::write(&script, b"#!/bin/sh\nexit 0\n").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&script, fs::Permissions::from_mode(0o755)).unwrap();
        }
        let rel = Path::new("fake-grit");
        env::set_current_dir(dir.path()).expect("set cwd for relative path test");
        let resolved = resolve_binary("grit", Some(rel)).unwrap();
        env::set_current_dir(&original_cwd).expect("restore cwd after relative path test");
        assert!(resolved.is_absolute());
        assert_eq!(resolved, script.canonicalize().unwrap());
    }
}

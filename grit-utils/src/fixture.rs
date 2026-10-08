//! Synthetic repository fixtures for benchmarks.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::thread;
use std::time::Duration;

use anyhow::{bail, Context, Result};

/// Default scratch directory for benchmark repos.
pub fn scratch_dir() -> PathBuf {
    PathBuf::from("/tmp/grit-bench-scratch")
}

/// Remove a directory tree, retrying transient failures.
pub fn remove_dir_robust(dir: &Path) {
    for _ in 0..5 {
        if !dir.exists() {
            return;
        }
        if fs::remove_dir_all(dir).is_ok() {
            return;
        }
        thread::sleep(Duration::from_millis(100));
    }
}

/// Create a committed repo with `file_count` text files spread across subdirectories.
pub fn create_repo(git: &Path, file_count: usize) -> Result<PathBuf> {
    let dir = scratch_dir();
    remove_dir_robust(&dir);
    fs::create_dir_all(&dir).context("create scratch dir")?;

    let out = Command::new(git)
        .args(["init", "-q"])
        .current_dir(&dir)
        .output()
        .context("git init")?;
    if !out.status.success() {
        bail!("git init failed");
    }

    let files_per_dir = 100;
    let num_dirs = file_count.div_ceil(files_per_dir);
    let mut created = 0;

    for d in 0..num_dirs {
        let subdir = dir.join(format!("d{d:04}"));
        fs::create_dir_all(&subdir)?;
        for f in 0..files_per_dir {
            if created >= file_count {
                break;
            }
            let path = subdir.join(format!("f{f:04}.txt"));
            fs::write(&path, format!("content {d}/{f}\nline 2\nline 3\n"))?;
            created += 1;
        }
    }

    run_git(git, &dir, &["add", "-A"])?;
    run_git(git, &dir, &["commit", "-q", "-m", "initial"])?;
    Ok(dir)
}

/// Dirty the worktree (~10% modified, ~5% untracked).
pub fn dirty_repo(dir: &Path, count: usize) -> Result<()> {
    let modify_count = count / 10;
    let untracked_count = count / 20;

    let mut modified = 0;
    for entry in walkdir(dir)? {
        if modified >= modify_count {
            break;
        }
        if entry.extension().is_some_and(|e| e == "txt") {
            let mut content = fs::read_to_string(&entry)?;
            content.push_str("modified\n");
            fs::write(&entry, content)?;
            modified += 1;
        }
    }

    for i in 0..untracked_count {
        let path = dir.join(format!("untracked_{i}.txt"));
        fs::write(&path, format!("untracked content {i}\n"))?;
    }
    Ok(())
}

/// Modify ~20% of tracked files for a `commit` iteration (append line), then reset index with **git**.
pub fn prepare_commit_iteration(dir: &Path, git: &Path) -> Result<()> {
    use std::io::Write as _;

    let files = walkdir(dir)?;
    let modify_count = (files.len() / 5).max(1);
    for f in files.iter().take(modify_count) {
        if f.extension().is_some_and(|e| e == "txt") {
            let mut file = std::fs::OpenOptions::new().append(true).open(f)?;
            writeln!(file, "commit bench change")?;
        }
    }
    let out = Command::new(git)
        .args(["reset", "-q", "HEAD"])
        .current_dir(dir)
        .output()
        .context("run git reset")?;
    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr);
        anyhow::bail!(
            "git reset -q HEAD failed in {}: {}",
            dir.display(),
            stderr.trim()
        );
    }
    Ok(())
}

/// Modify ~20% of tracked files and reset the index to HEAD with **git** (for `add` benchmarks).
pub fn prepare_add_iteration(dir: &Path, git: &Path) -> Result<()> {
    let files = walkdir(dir)?;
    let modify_count = (files.len() / 5).max(1);
    for f in files.iter().take(modify_count) {
        if f.extension().is_some_and(|e| e == "txt") {
            fs::write(f, "modified for add bench\n")?;
        }
    }
    let out = Command::new(git)
        .args(["reset", "-q", "HEAD"])
        .current_dir(dir)
        .output()
        .context("run git reset")?;
    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr);
        anyhow::bail!(
            "git reset -q HEAD failed in {}: {}",
            dir.display(),
            stderr.trim()
        );
    }
    Ok(())
}

fn run_git(git: &Path, dir: &Path, args: &[&str]) -> Result<()> {
    let out = Command::new(git).args(args).current_dir(dir).output()?;
    if !out.status.success() {
        bail!("git {} failed", args.join(" "));
    }
    Ok(())
}

fn walkdir(dir: &Path) -> Result<Vec<PathBuf>> {
    let mut files = Vec::new();
    walkdir_inner(dir, &mut files)?;
    Ok(files)
}

fn walkdir_inner(dir: &Path, files: &mut Vec<PathBuf>) -> Result<()> {
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();
        if path.file_name().is_some_and(|n| n == ".git") {
            continue;
        }
        if path.is_dir() {
            walkdir_inner(&path, files)?;
        } else {
            files.push(path);
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::env;
    use std::process::Command;
    use tempfile::TempDir;

    #[test]
    fn prepare_add_iteration_requires_git_reset() {
        let dir = TempDir::new().unwrap();
        let git = which_git();
        create_min_repo(&git, dir.path()).unwrap();
        prepare_add_iteration(dir.path(), &git).unwrap();
        let grit = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../target/release/grit");
        if !grit.is_file() {
            return;
        }
        let err = prepare_add_iteration(dir.path(), &grit).unwrap_err();
        assert!(
            err.to_string().contains("git reset"),
            "unexpected error: {err}"
        );
    }

    fn which_git() -> PathBuf {
        crate::binary::resolve_binary("git", None).expect("git")
    }

    fn create_min_repo(git: &Path, dir: &Path) -> Result<()> {
        std::fs::create_dir_all(dir)?;
        let out = Command::new(git)
            .args(["init", "-q"])
            .current_dir(dir)
            .output()?;
        if !out.status.success() {
            bail!("git init failed");
        }
        std::fs::write(dir.join("f.txt"), "a\n")?;
        run_git(git, dir, &["add", "f.txt"])?;
        run_git(git, dir, &["commit", "-qm", "init"])?;
        Ok(())
    }
}

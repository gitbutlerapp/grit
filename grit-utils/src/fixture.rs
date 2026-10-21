//! Synthetic repository fixtures for benchmarks.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::thread;
use std::time::Duration;

use anyhow::{bail, Context, Result};

/// Default scratch directory for benchmark repos.
pub fn scratch_dir() -> PathBuf {
    if let Ok(dir) = std::env::var("GRIT_BENCH_SCRATCH") {
        return PathBuf::from(dir);
    }
    #[cfg(test)]
    {
        use std::cell::OnceCell;
        std::thread_local! {
            static TEST_SCRATCH: OnceCell<PathBuf> = const { OnceCell::new() };
        }
        return TEST_SCRATCH.with(|cell| {
            cell.get_or_init(|| {
                tempfile::tempdir()
                    .expect("benchmark scratch tempdir")
                    .keep()
            })
            .clone()
        });
    }
    #[cfg(not(test))]
    {
        PathBuf::from("/tmp/grit-bench-scratch")
    }
}

/// Active worktree for hyperfine prepare hooks (campaign sets `GRIT_BENCH_CAMPAIGN_CWD`).
pub fn bench_work_dir() -> PathBuf {
    std::env::var("GRIT_BENCH_CAMPAIGN_CWD")
        .map(PathBuf::from)
        .unwrap_or_else(|_| scratch_dir())
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

/// Append `commit_count` empty commits on top of an existing repo (for log benchmarks).
pub fn extend_with_empty_commits(dir: &Path, git: &Path, commit_count: usize) -> Result<()> {
    for i in 0..commit_count {
        run_git(
            git,
            dir,
            &["commit", "--allow-empty", "-q", "-m", &format!("bench {i}")],
        )?;
    }
    Ok(())
}

/// Create a repo with `file_count` tracked files and at least `commit_count` commits.
pub fn create_repo_with_history(
    git: &Path,
    file_count: usize,
    commit_count: usize,
) -> Result<PathBuf> {
    let dir = create_repo(git, file_count)?;
    if commit_count > 1 {
        extend_with_empty_commits(&dir, git, commit_count - 1)?;
    }
    Ok(dir)
}

/// Modify the first `count` tracked `.txt` files (unstaged) for worktree diff benchmarks.
pub fn modify_files_for_diff(dir: &Path, count: usize) -> Result<()> {
    use std::io::Write as _;

    let files = walkdir(dir)?;
    let mut modified = 0;
    for path in files {
        if modified >= count {
            break;
        }
        if path.extension().is_some_and(|e| e == "txt") {
            let mut file = std::fs::OpenOptions::new().append(true).open(&path)?;
            writeln!(file, "diff bench line")?;
            modified += 1;
        }
    }
    anyhow::ensure!(
        modified >= count,
        "expected to modify at least {count} .txt files, modified {modified}"
    );
    Ok(())
}

/// Reset the worktree to `HEAD` and apply exactly `count` unstaged `.txt` modifications.
///
/// Used as a hyperfine `--prepare` hook so `git diff` and `grit diff` each run against the
/// same dirty fixture regardless of what the previous timed command did.
pub fn prepare_synthetic_diff_iteration(dir: &Path, git: &Path, count: usize) -> Result<()> {
    run_git(git, dir, &["reset", "--hard", "HEAD"])?;
    modify_files_for_diff(dir, count)
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

fn tracked_worktree_files(git: &Path, dir: &Path) -> Result<Vec<PathBuf>> {
    Ok(crate::campaign_worktree::list_tracked_files(git, dir)?
        .into_iter()
        .filter(|rel| rel.to_string_lossy() != ".gitmodules")
        .map(|rel| dir.join(rel))
        .collect())
}

/// Modify ~20% of tracked files for a `commit` iteration (append line), then reset index with **git**.
pub fn prepare_commit_iteration(dir: &Path, git: &Path) -> Result<()> {
    use std::io::Write as _;

    if std::env::var_os("GRIT_BENCH_CAMPAIGN_CWD").is_some() {
        for args in [
            &["reset", "--hard", "HEAD"][..],
            &[
                "clean",
                "-fd",
                "-q",
                "-e",
                ".grit-bench-worktree-ready",
                "-e",
                ".grit-bench-hot-path.json",
            ][..],
        ] {
            let mut cmd = Command::new(git);
            cmd.args(args).current_dir(dir);
            crate::real_repo_fixture::git_env(&mut cmd);
            let out = cmd.output().context("campaign prepare hard reset")?;
            if !out.status.success() {
                let stderr = String::from_utf8_lossy(&out.stderr);
                anyhow::bail!(
                    "git {} failed in {}: {}",
                    args.join(" "),
                    dir.display(),
                    stderr.trim()
                );
            }
        }
    }

    let files = tracked_worktree_files(git, dir)?;
    let modify_count = (files.len() / 5).max(1);
    for f in files.iter().take(modify_count) {
        let mut file = std::fs::OpenOptions::new().append(true).open(f)?;
        writeln!(file, "commit bench change")?;
    }
    git_reset_head_unstaged(git, dir)
}

fn git_reset_head_unstaged(git: &Path, dir: &Path) -> Result<()> {
    let mut cmd = Command::new(git);
    cmd.args(["reset", "-q", "HEAD"]).current_dir(dir);
    crate::real_repo_fixture::git_env(&mut cmd);
    let out = cmd.output().context("run git reset")?;
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

/// Modify ~20% of tracked files in the worktree only (for `restore` benchmarks).
pub fn prepare_restore_iteration(dir: &Path) -> Result<()> {
    let files = walkdir(dir)?;
    let modify_count = (files.len() / 5).max(1);
    for f in files.iter().take(modify_count) {
        if f.extension().is_some_and(|e| e == "txt") {
            fs::write(f, "restore bench dirty\n")?;
        }
    }
    Ok(())
}

/// Modify ~20% of tracked files and reset the index to HEAD with **git** (for `add` benchmarks).
/// Reset the worktree and apply a deterministic dirty diff (campaign `diff-dirty` hyperfine prepare).
pub fn prepare_diff_dirty_iteration(dir: &Path, git: &Path) -> Result<()> {
    let count = std::env::var("GRIT_BENCH_DIFF_DIRTY_COUNT")
        .context("GRIT_BENCH_DIFF_DIRTY_COUNT")?
        .parse::<usize>()
        .context("parse GRIT_BENCH_DIFF_DIRTY_COUNT")?;
    run_git(git, dir, &["reset", "--hard", "HEAD"])?;
    run_git(
        git,
        dir,
        &["clean", "-fd", "-e", ".grit-bench-hot-path.json"],
    )?;
    let mut rel = crate::campaign_worktree::list_tracked_files(git, dir)?
        .into_iter()
        .filter(|p| p.to_string_lossy() != ".gitmodules")
        .collect::<Vec<_>>();
    rel.sort_by(|a, b| a.as_os_str().cmp(b.as_os_str()));
    crate::campaign_worktree::modify_tracked_files(dir, &rel, count)?;
    Ok(())
}

pub fn prepare_add_iteration(dir: &Path, git: &Path) -> Result<()> {
    let files = tracked_worktree_files(git, dir)?;
    let modify_count = (files.len() / 5).max(1);
    for f in files.iter().take(modify_count) {
        fs::write(f, "modified for add bench\n")?;
    }
    git_reset_head_unstaged(git, dir)
}

/// Dogfood-style deep leaf worktree (issue #1017): CRLF `*.txt`, a large `.bin`, and tracked `.rs` files.
#[derive(Debug, Clone, Copy)]
pub struct DeepLeafDogfoodSpec {
    /// Count of deepest directories (each holds [`Self::files_per_leaf`] text files).
    pub leaf_dirs: usize,
    pub files_per_leaf: usize,
    pub rs_file_count: usize,
    pub bin_megabytes: usize,
}

impl DeepLeafDogfoodSpec {
    /// Small fixture for CI smoke and local ratio checks.
    #[must_use]
    pub fn smoke() -> Self {
        Self {
            leaf_dirs: 400,
            files_per_leaf: 3,
            rs_file_count: 20,
            bin_megabytes: 1,
        }
    }

    /// Matches the Windows dogfood layout (~50k tracked paths).
    #[must_use]
    pub fn production() -> Self {
        Self {
            leaf_dirs: 15_625,
            files_per_leaf: 3,
            rs_file_count: 100,
            bin_megabytes: 20,
        }
    }
}

/// Build and commit the deep-leaf dogfood repository under [`scratch_dir`].
pub fn create_deep_leaf_dogfood_repo(git: &Path, spec: DeepLeafDogfoodSpec) -> Result<PathBuf> {
    let dir = scratch_dir();
    remove_dir_robust(&dir);
    fs::create_dir_all(&dir).context("create scratch dir")?;
    run_git(git, &dir, &["init", "-q", "-b", "main"])?;
    fs::write(dir.join(".gitattributes"), "*.txt text eol=crlf\n")?;

    for leaf in 0..spec.leaf_dirs {
        let leaf_dir = format!("leaf{leaf:05}/a/b");
        let abs_leaf = dir.join(&leaf_dir);
        fs::create_dir_all(&abs_leaf)?;
        for f in 0..spec.files_per_leaf {
            let path = abs_leaf.join(format!("part{f}.txt"));
            fs::write(path, format!("leaf={leaf} part={f}\r\n"))?;
        }
    }

    for i in 0..spec.rs_file_count {
        let sub = format!("src{:02}", i / 10);
        fs::create_dir_all(dir.join(&sub))?;
        fs::write(
            dir.join(&sub).join(format!("module{i}.rs")),
            format!("fn f{i}() {{}}\n"),
        )?;
    }

    let bin_path = dir.join("large.bin");
    {
        use std::io::Write as _;
        let mut bin = fs::File::create(&bin_path)?;
        let chunk = vec![0_u8; 1024 * 1024];
        for _ in 0..spec.bin_megabytes {
            bin.write_all(&chunk)?;
        }
    }

    run_git(git, &dir, &["add", "-A"])?;
    run_git(git, &dir, &["commit", "-q", "-m", "deep-leaf dogfood seed"])?;
    Ok(dir)
}

/// Many small files per leaf directory (dogfood issue #1041): deep prefixes, 50 files/dir.
#[derive(Debug, Clone, Copy)]
pub struct DenseLeafDogfoodSpec {
    /// Deepest directories, each holding [`Self::files_per_leaf`] small text files.
    pub leaf_dirs: usize,
    pub files_per_leaf: usize,
    /// Extra deep-only chains (21-level style) with [`Self::deep_files_per_chain`] files each.
    pub deep_chains: usize,
    pub deep_depth: usize,
    pub deep_files_per_chain: usize,
}

impl DenseLeafDogfoodSpec {
    /// CI smoke (~4k tracked blobs).
    #[must_use]
    pub fn smoke() -> Self {
        Self {
            leaf_dirs: 80,
            files_per_leaf: 50,
            deep_chains: 1,
            deep_depth: 12,
            deep_files_per_chain: 10,
        }
    }

    /// Matches the Windows #1041 layout (~50k tracked paths).
    #[must_use]
    pub fn production() -> Self {
        Self {
            leaf_dirs: 1000,
            files_per_leaf: 50,
            deep_chains: 5,
            deep_depth: 21,
            deep_files_per_chain: 10,
        }
    }
}

fn dense_leaf_dir_prefix(leaf: usize) -> String {
    let depth = 5 + (leaf % 7);
    let mut path = String::new();
    for level in 0..depth {
        path.push_str(&format!(
            "p{}/q{}/r{}/s{}/n{}/",
            leaf % 10,
            level % 10,
            level % 8,
            level % 6,
            level % 5
        ));
    }
    path
}

/// Build and commit the dense-leaf dogfood repository under [`scratch_dir`].
pub fn create_dense_leaf_dogfood_repo(git: &Path, spec: DenseLeafDogfoodSpec) -> Result<PathBuf> {
    let dir = scratch_dir();
    remove_dir_robust(&dir);
    fs::create_dir_all(&dir).context("create scratch dir")?;
    run_git(git, &dir, &["init", "-q", "-b", "main"])?;

    for leaf in 0..spec.leaf_dirs {
        let prefix = dense_leaf_dir_prefix(leaf);
        let leaf_dir = format!("{prefix}leaf{leaf:04}");
        let abs_leaf = dir.join(&leaf_dir);
        fs::create_dir_all(&abs_leaf)?;
        for f in 0..spec.files_per_leaf {
            let path = abs_leaf.join(format!("f{f}-file.txt"));
            fs::write(path, format!("leaf={leaf} f={f}\n"))?;
        }
    }

    for chain in 0..spec.deep_chains {
        let mut rel = format!("deep{chain:02}");
        for level in 0..spec.deep_depth {
            rel.push_str(&format!("/d{level}"));
        }
        let abs = dir.join(&rel);
        fs::create_dir_all(&abs)?;
        for f in 0..spec.deep_files_per_chain {
            fs::write(abs.join(format!("f{f}.txt")), format!("deep {chain} {f}\n"))?;
        }
    }

    run_git(git, &dir, &["add", "-A"])?;
    run_git(
        git,
        &dir,
        &["commit", "-q", "-m", "dense-leaf dogfood seed"],
    )?;
    Ok(dir)
}

/// Reset the index and re-dirty the first `count` tracked `.txt` files (for hyperfine `--prepare`).
pub fn prepare_dense_leaf_add_iteration(dir: &Path, git: &Path, count: usize) -> Result<()> {
    run_git(git, dir, &["reset", "-q", "HEAD"])?;
    let mut paths: Vec<PathBuf> = walkdir(dir)?
        .into_iter()
        .filter(|p| p.extension().is_some_and(|e| e == "txt"))
        .collect();
    paths.sort();
    anyhow::ensure!(
        paths.len() >= count,
        "expected at least {count} .txt files, found {}",
        paths.len()
    );
    for path in paths.iter().take(count) {
        fs::write(path, "modified for dense-leaf add bench\n")?;
    }
    Ok(())
}

/// Append one line to the first `count` tracked `.txt` files (unstaged) for status/diff benches.
pub fn modify_txt_files_for_diff(dir: &Path, count: usize) -> Result<()> {
    use std::io::Write as _;

    let mut paths: Vec<PathBuf> = walkdir(dir)?
        .into_iter()
        .filter(|p| p.extension().is_some_and(|e| e == "txt"))
        .collect();
    paths.sort();
    anyhow::ensure!(
        paths.len() >= count,
        "expected at least {count} .txt files, found {}",
        paths.len()
    );
    for path in paths.iter().take(count) {
        let mut file = std::fs::OpenOptions::new().append(true).open(path)?;
        writeln!(file, "bench diff line")?;
    }
    Ok(())
}

/// Append one line to the first `count` tracked `.rs` files (unstaged) for diff/status benches.
pub fn modify_rs_files_for_diff(dir: &Path, count: usize) -> Result<()> {
    use std::io::Write as _;

    let mut paths: Vec<PathBuf> = walkdir(dir)?
        .into_iter()
        .filter(|p| p.extension().is_some_and(|e| e == "rs"))
        .collect();
    paths.sort();
    anyhow::ensure!(
        paths.len() >= count,
        "expected at least {count} .rs files, found {}",
        paths.len()
    );
    for path in paths.iter().take(count) {
        let mut file = fs::OpenOptions::new().append(true).open(path)?;
        writeln!(file, "// bench diff line")?;
    }
    Ok(())
}

/// Large file with deep history where each commit only changes the last line (cargo-like churn).
pub fn create_blame_large_tail_churn_repo(
    git: &Path,
    line_count: usize,
    commit_count: usize,
) -> Result<PathBuf> {
    let dir = scratch_dir();
    remove_dir_robust(&dir);
    fs::create_dir_all(&dir).context("create scratch dir")?;
    run_git(git, &dir, &["init", "-q"])?;
    let path = "large.txt";
    let mut lines: Vec<String> = (0..line_count).map(|i| format!("line {i}\n")).collect();
    fs::write(dir.join(path), lines.join(""))?;
    run_git(git, &dir, &["add", path])?;
    run_git(git, &dir, &["commit", "-qm", "init"])?;
    for i in 1..commit_count {
        if let Some(last) = lines.last_mut() {
            *last = format!("tail {i}\n");
        }
        fs::write(dir.join(path), lines.join(""))?;
        run_git(git, &dir, &["add", path])?;
        run_git(git, &dir, &["commit", "-qm", &format!("tail {i}")])?;
    }
    Ok(dir)
}

/// Repository with many sequential edits to a single tracked file (for blame benches).
pub fn create_blame_deep_history_repo(git: &Path, commit_count: usize) -> Result<PathBuf> {
    let dir = scratch_dir();
    remove_dir_robust(&dir);
    fs::create_dir_all(&dir).context("create scratch dir")?;
    run_git(git, &dir, &["init", "-q"])?;
    let path = "blame.txt";
    fs::write(dir.join(path), "seed\n")?;
    run_git(git, &dir, &["add", path])?;
    run_git(git, &dir, &["commit", "-qm", "init"])?;
    for i in 1..commit_count {
        let body = format!("line {i}\nseed\n");
        fs::write(dir.join(path), body)?;
        run_git(git, &dir, &["add", path])?;
        run_git(git, &dir, &["commit", "-qm", &format!("edit {i}")])?;
    }
    Ok(dir)
}

fn run_git(git: &Path, dir: &Path, args: &[&str]) -> Result<()> {
    let out = Command::new(git)
        .args(args)
        .current_dir(dir)
        .env("GIT_AUTHOR_NAME", "Bench")
        .env("GIT_AUTHOR_EMAIL", "b@example.com")
        .env("GIT_COMMITTER_NAME", "Bench")
        .env("GIT_COMMITTER_EMAIL", "b@example.com")
        .output()?;
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
        } else if !path.file_name().is_some_and(|n| n == ".gitmodules") {
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
            err.to_string().contains("git reset") || err.to_string().contains("ls-files"),
            "unexpected error: {err}"
        );
    }

    fn which_git() -> PathBuf {
        crate::binary::resolve_binary("git", None).expect("git")
    }

    #[test]
    fn prepare_synthetic_diff_iteration_restores_name_count() {
        let git = which_git();
        remove_dir_robust(&scratch_dir());
        let repo = create_repo(&git, 500).expect("create repo");
        modify_files_for_diff(&repo, 48).expect("seed diff");
        prepare_synthetic_diff_iteration(&repo, &git, 48).expect("prepare");
        let out = Command::new(&git)
            .args(["diff", "--name-only"])
            .current_dir(&repo)
            .output()
            .expect("git diff --name-only");
        assert!(out.status.success());
        let names = std::str::from_utf8(&out.stdout)
            .unwrap()
            .lines()
            .filter(|l| !l.is_empty())
            .count();
        assert_eq!(names, 48);
        remove_dir_robust(&scratch_dir());
    }

    #[test]
    fn create_repo_with_history_has_commit_count() {
        let git = which_git();
        let repo = create_repo_with_history(&git, 50, 100).unwrap();
        let out = Command::new(&git)
            .args(["rev-list", "--count", "HEAD"])
            .current_dir(&repo)
            .output()
            .unwrap();
        assert!(out.status.success());
        let n = String::from_utf8_lossy(&out.stdout)
            .trim()
            .parse::<usize>()
            .unwrap();
        assert_eq!(n, 100);
        remove_dir_robust(&scratch_dir());
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

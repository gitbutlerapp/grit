//! Issue #2575: worktree restore must not drop icase collision siblings from the index.

use std::error::Error;
use std::ffi::OsString;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};

type TestResult = Result<(), Box<dyn Error>>;

const GS: &str = env!("CARGO_BIN_EXE_grit");

struct Scratch {
    path: PathBuf,
}

impl Scratch {
    fn new(tag: &str) -> Result<Self, Box<dyn Error>> {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let n = NEXT.fetch_add(1, Ordering::SeqCst);
        let mut path = std::env::temp_dir();
        path.push(format!("grit-cli-{tag}-{}-{n}", std::process::id()));
        if path.exists() {
            fs::remove_dir_all(&path)?;
        }
        fs::create_dir_all(&path)?;
        Ok(Self { path })
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
    }
}

fn null_device() -> &'static str {
    if cfg!(windows) {
        "NUL"
    } else {
        "/dev/null"
    }
}

fn git_ok(dir: &Path, args: &[&str]) -> Result<(), Box<dyn Error>> {
    let out = Command::new("git")
        .current_dir(dir)
        .args(args)
        .env("GIT_AUTHOR_NAME", "Test User")
        .env("GIT_AUTHOR_EMAIL", "test@example.com")
        .env("GIT_COMMITTER_NAME", "Test User")
        .env("GIT_COMMITTER_EMAIL", "test@example.com")
        .env("GIT_AUTHOR_DATE", "1700000000 +0000")
        .env("GIT_COMMITTER_DATE", "1700000000 +0000")
        .env("GIT_CONFIG_GLOBAL", null_device())
        .env("GIT_CONFIG_SYSTEM", null_device())
        .output()?;
    assert!(out.status.success(), "git {args:?}: {}", out.status);
    Ok(())
}

fn gs_ok(dir: &Path, args: &[&str]) -> Result<(), Box<dyn Error>> {
    let args: Vec<OsString> = args.iter().map(|s| OsString::from(*s)).collect();
    let out = Command::new(GS)
        .args(&args)
        .current_dir(dir)
        .env("GIT_AUTHOR_NAME", "Test User")
        .env("GIT_AUTHOR_EMAIL", "test@example.com")
        .env("GIT_COMMITTER_NAME", "Test User")
        .env("GIT_COMMITTER_EMAIL", "test@example.com")
        .env("GIT_AUTHOR_DATE", "1700000000 +0000")
        .env("GIT_COMMITTER_DATE", "1700000000 +0000")
        .env("GIT_CONFIG_GLOBAL", null_device())
        .env("GIT_CONFIG_SYSTEM", null_device())
        .output()?;
    assert!(
        out.status.success(),
        "grit {args:?}: exit={:?}\nstdout:\n{}\nstderr:\n{}",
        out.status.code(),
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    Ok(())
}

fn worktree_filesystem_case_insensitive(root: &Path) -> bool {
    let probe = root.join(".grit_icase_probe");
    if fs::write(&probe, b"x").is_err() {
        return false;
    }
    let folds = root.join(".GRIT_ICASE_PROBE").exists();
    let _ = fs::remove_file(&probe);
    folds
}

fn init_collision_fixture(scratch: &Scratch) -> Result<(), Box<dyn Error>> {
    let repo = &scratch.path;
    git_ok(repo, &["init", "-q", "-b", "main", "."])?;
    git_ok(repo, &["config", "core.ignorecase", "true"])?;
    git_ok(repo, &["config", "user.email", "a@b"])?;
    git_ok(repo, &["config", "user.name", "A"])?;
    let b1 = {
        let out = Command::new("git")
            .current_dir(repo)
            .args(["hash-object", "-w", "--stdin"])
            .stdin(std::process::Stdio::piped())
            .env("GIT_CONFIG_GLOBAL", null_device())
            .env("GIT_CONFIG_SYSTEM", null_device())
            .spawn()?;
        let mut child = out;
        child.stdin.as_mut().unwrap().write_all(b"UPPER\n")?;
        let out = child.wait_with_output()?;
        assert!(out.status.success());
        String::from_utf8_lossy(&out.stdout).trim().to_owned()
    };
    let b2 = {
        let out = Command::new("git")
            .current_dir(repo)
            .args(["hash-object", "-w", "--stdin"])
            .stdin(std::process::Stdio::piped())
            .env("GIT_CONFIG_GLOBAL", null_device())
            .env("GIT_CONFIG_SYSTEM", null_device())
            .spawn()?;
        let mut child = out;
        child.stdin.as_mut().unwrap().write_all(b"lower\n")?;
        let out = child.wait_with_output()?;
        assert!(out.status.success());
        String::from_utf8_lossy(&out.stdout).trim().to_owned()
    };
    let b0 = {
        let out = Command::new("git")
            .current_dir(repo)
            .args(["hash-object", "-w", "--stdin"])
            .stdin(std::process::Stdio::piped())
            .env("GIT_CONFIG_GLOBAL", null_device())
            .env("GIT_CONFIG_SYSTEM", null_device())
            .spawn()?;
        let mut child = out;
        child.stdin.as_mut().unwrap().write_all(b"base\n")?;
        let out = child.wait_with_output()?;
        assert!(out.status.success());
        String::from_utf8_lossy(&out.stdout).trim().to_owned()
    };
    let tree = format!(
        "100644 blob {b1}\tReadme.TXT\n100644 blob {b0}\tbase.txt\n100644 blob {b2}\treadme.txt\n"
    );
    let tree_oid = {
        let out = Command::new("git")
            .current_dir(repo)
            .args(["mktree"])
            .stdin(std::process::Stdio::piped())
            .env("GIT_CONFIG_GLOBAL", null_device())
            .env("GIT_CONFIG_SYSTEM", null_device())
            .spawn()?;
        let mut child = out;
        child.stdin.as_mut().unwrap().write_all(tree.as_bytes())?;
        let out = child.wait_with_output()?;
        assert!(out.status.success());
        String::from_utf8_lossy(&out.stdout).trim().to_owned()
    };
    let commit = {
        let out = Command::new("git")
            .current_dir(repo)
            .args(["commit-tree", &tree_oid, "-m", "collide"])
            .env("GIT_CONFIG_GLOBAL", null_device())
            .env("GIT_CONFIG_SYSTEM", null_device())
            .output()?;
        assert!(out.status.success());
        String::from_utf8_lossy(&out.stdout).trim().to_owned()
    };
    git_ok(repo, &["update-ref", "refs/heads/main", &commit])?;
    git_ok(repo, &["reset", "-q", "--hard"])?;
    Ok(())
}

fn ls_files(repo: &Path) -> Result<String, Box<dyn Error>> {
    let out = Command::new("git")
        .current_dir(repo)
        .args(["ls-files"])
        .env("GIT_CONFIG_GLOBAL", null_device())
        .env("GIT_CONFIG_SYSTEM", null_device())
        .output()?;
    assert!(out.status.success());
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

#[test]
fn restore_icase_collision_keeps_index_on_case_insensitive_volume() -> TestResult {
    let probe = Scratch::new("issue-2575-probe")?;
    if !worktree_filesystem_case_insensitive(&probe.path) {
        return Ok(());
    }

    let scratch = Scratch::new("issue-2575-restore")?;
    init_collision_fixture(&scratch)?;
    let repo = &scratch.path;
    fs::write(repo.join("Readme.TXT"), "dirty\n")?;

    gs_ok(repo, &["restore", "Readme.TXT"])?;
    let listed = ls_files(repo)?;
    assert!(
        listed.contains("readme.txt"),
        "readme.txt must stay in the index: {listed}"
    );
    assert!(
        !listed.lines().any(|l| l.starts_with("D ")),
        "restore must not stage deletions: {listed}"
    );

    let copy = Scratch::new("issue-2575-dot")?;
    let copy_path = copy.path.join("repo");
    fs::create_dir_all(&copy_path)?;
    for entry in fs::read_dir(repo)? {
        let entry = entry?;
        let name = entry.file_name();
        if name == ".git" || name == ".." || name == "." {
            continue;
        }
        let from = entry.path();
        let to = copy_path.join(name);
        if from.is_dir() {
            copy_dir_recursive(&from, &to)?;
        } else {
            fs::copy(&from, &to)?;
        }
    }
    let git_src = repo.join(".git");
    copy_dir_recursive(&git_src, &copy_path.join(".git"))?;
    gs_ok(&copy_path, &["restore", "."])?;
    let listed = ls_files(&copy_path)?;
    assert!(
        listed.contains("Readme.TXT")
            && listed.contains("readme.txt")
            && listed.contains("base.txt"),
        "restore . must keep all index paths: {listed}"
    );
    Ok(())
}

fn copy_dir_recursive(from: &Path, to: &Path) -> Result<(), Box<dyn Error>> {
    fs::create_dir_all(to)?;
    for entry in fs::read_dir(from)? {
        let entry = entry?;
        let dest = to.join(entry.file_name());
        if entry.file_type()?.is_dir() {
            copy_dir_recursive(&entry.path(), &dest)?;
        } else {
            fs::copy(entry.path(), &dest)?;
        }
    }
    Ok(())
}

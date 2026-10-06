use std::error::Error;
use std::ffi::{OsStr, OsString};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};

type TestResult = Result<(), Box<dyn Error>>;

const GS: &str = env!("CARGO_BIN_EXE_grit");

#[derive(Debug)]
struct CmdOutput {
    status: Option<i32>,
    stdout: String,
    stderr: String,
}

impl CmdOutput {
    fn dump(&self) -> String {
        format!(
            "exit={:?}\n--- stdout ---\n{}\n--- stderr ---\n{}",
            self.status, self.stdout, self.stderr
        )
    }
}

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

    fn path(&self) -> &Path {
        &self.path
    }

    fn child(&self, name: &str) -> PathBuf {
        self.path.join(name)
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
    }
}

fn gs<I, S>(dir: &Path, args: I) -> Result<CmdOutput, Box<dyn Error>>
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    let args: Vec<OsString> = args
        .into_iter()
        .map(|arg| arg.as_ref().to_os_string())
        .collect();
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
    Ok(CmdOutput {
        status: out.status.code(),
        stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
    })
}

fn gs_ok<I, S>(dir: &Path, args: I) -> Result<CmdOutput, Box<dyn Error>>
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    let out = gs(dir, args)?;
    assert_eq!(out.status, Some(0), "{}", out.dump());
    Ok(out)
}

fn null_device() -> &'static str {
    if cfg!(windows) {
        "NUL"
    } else {
        "/dev/null"
    }
}

fn path_arg(path: &Path) -> Result<String, Box<dyn Error>> {
    path.to_str()
        .map(str::to_owned)
        .ok_or_else(|| format!("path is not valid UTF-8: {}", path.display()).into())
}

fn write_file(path: &Path, contents: &str) -> Result<(), Box<dyn Error>> {
    fs::write(path, contents)?;
    Ok(())
}

#[test]
fn local_edit_config_commit_status_and_log_workflow() -> TestResult {
    let scratch = Scratch::new("local")?;
    let repo = scratch.child("repo");
    fs::create_dir_all(&repo)?;
    gs_ok(&repo, ["init", "."])?;

    write_file(&repo.join("alpha.txt"), "alpha v1\n")?;
    let status = gs_ok(&repo, std::iter::empty::<&str>())?;
    assert!(status.stdout.contains("On main"));
    assert!(status.stdout.contains("Untracked"));
    assert!(status.stdout.contains("alpha.txt"));

    gs_ok(&repo, ["config", "user.name", "A Developer"])?;
    let name = gs_ok(&repo, ["config", "user.name"])?;
    assert_eq!(name.stdout.trim(), "A Developer");
    let listed = gs_ok(&repo, ["config", "--list"])?;
    assert!(listed.stdout.contains("user.name=A Developer"));
    gs_ok(&repo, ["config", "--unset", "user.name"])?;
    let missing = gs(&repo, ["config", "user.name"])?;
    assert_ne!(missing.status, Some(0), "{}", missing.dump());

    gs_ok(&repo, ["add", "alpha.txt"])?;
    write_file(&repo.join("alpha.txt"), "alpha v2\n")?;
    write_file(&repo.join("beta.txt"), "beta\n")?;
    let committed = gs_ok(&repo, ["commit", "initial commit"])?;
    assert!(committed.stdout.contains("initial commit"));
    assert!(committed.stdout.contains("2 changes committed"));

    let status = gs_ok(&repo, ["status"])?;
    assert!(status.stdout.contains("Nothing to commit"));
    let log = gs_ok(&repo, ["log"])?;
    assert!(log.stdout.contains("initial commit"));
    Ok(())
}

#[test]
fn branch_switch_and_merge_workflow() -> TestResult {
    let scratch = Scratch::new("branch")?;
    let repo = scratch.child("repo");
    fs::create_dir_all(&repo)?;
    gs_ok(&repo, ["init", "."])?;
    write_file(&repo.join("base.txt"), "base\n")?;
    gs_ok(&repo, ["commit", "base"])?;

    gs_ok(&repo, ["branch", "topic"])?;
    let branches = gs_ok(&repo, ["branch"])?;
    assert!(branches.stdout.contains("* main"));
    assert!(branches.stdout.contains("  topic"));

    gs_ok(&repo, ["switch", "topic"])?;
    write_file(&repo.join("feature.txt"), "feature\n")?;
    gs_ok(&repo, ["commit", "topic work"])?;

    gs_ok(&repo, ["switch", "main"])?;
    assert!(!repo.join("feature.txt").exists());
    write_file(&repo.join("main.txt"), "main\n")?;
    gs_ok(&repo, ["commit", "main work"])?;

    let merge = gs_ok(&repo, ["merge", "topic"])?;
    assert!(merge.stdout.contains("Merged topic"));
    assert_eq!(fs::read_to_string(repo.join("feature.txt"))?, "feature\n");
    assert_eq!(fs::read_to_string(repo.join("main.txt"))?, "main\n");

    let log = gs_ok(&repo, ["log"])?;
    assert!(log.stdout.contains("Merge topic"));
    let status = gs_ok(&repo, ["status"])?;
    assert!(status.stdout.contains("Nothing to commit"));

    gs_ok(&repo, ["branch", "-d", "topic"])?;
    let branches = gs_ok(&repo, ["branch"])?;
    assert!(!branches.stdout.contains("topic"));
    Ok(())
}

#[test]
fn local_remote_clone_push_fetch_and_pull_workflow() -> TestResult {
    let scratch = Scratch::new("remote")?;
    let seed = scratch.child("seed");
    let remote = scratch.child("remote.git");
    let clone = scratch.child("clone");
    fs::create_dir_all(&seed)?;

    gs_ok(&seed, ["init", "."])?;
    write_file(&seed.join("README.md"), "seed\n")?;
    gs_ok(&seed, ["commit", "seed commit"])?;

    gs_ok(
        scratch.path(),
        ["init", "--bare", path_arg(&remote)?.as_str()],
    )?;
    gs_ok(
        &seed,
        ["remote", "add", "origin", path_arg(&remote)?.as_str()],
    )?;
    let pushed = gs_ok(&seed, ["push"])?;
    assert!(pushed.stdout.contains("pushed main"));

    gs_ok(
        scratch.path(),
        [
            "clone",
            path_arg(&remote)?.as_str(),
            path_arg(&clone)?.as_str(),
        ],
    )?;
    assert_eq!(fs::read_to_string(clone.join("README.md"))?, "seed\n");
    let cloned_log = gs_ok(&clone, ["log"])?;
    assert!(cloned_log.stdout.contains("seed commit"));

    write_file(&clone.join("clone.txt"), "from clone\n")?;
    gs_ok(&clone, ["commit", "-am", "clone work"])?;
    gs_ok(&clone, ["push"])?;

    let status_after_push = gs_ok(&clone, ["status"])?;
    assert!(
        status_after_push.stdout.contains("even with origin/main"),
        "status after push should reflect updated remote-tracking ref:\n{}",
        status_after_push.dump()
    );

    let fetched = gs_ok(&seed, ["fetch"])?;
    assert!(fetched.stdout.contains("Fetched"));
    let pulled = gs_ok(&seed, ["pull"])?;
    assert!(
        pulled.stdout.contains("Fast-forwarded")
            || pulled.stdout.contains("Already up to date")
            || pulled.stdout.contains("Merged")
    );
    assert_eq!(fs::read_to_string(seed.join("clone.txt"))?, "from clone\n");
    Ok(())
}

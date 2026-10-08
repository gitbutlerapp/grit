//! Issue #928: merge and pick must not clobber untracked files that the incoming tree adds.

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

fn gs_ok(dir: &Path, args: &[&str]) -> Result<CmdOutput, Box<dyn Error>> {
    let out = gs(dir, args)?;
    assert_eq!(out.status, Some(0), "{}", out.dump());
    Ok(out)
}

fn read_file(path: &Path) -> Result<String, Box<dyn Error>> {
    Ok(fs::read_to_string(path)?)
}

fn git_head(dir: &Path) -> Result<String, Box<dyn Error>> {
    let out = Command::new("git")
        .current_dir(dir)
        .args(["rev-parse", "HEAD"])
        .env("GIT_CONFIG_GLOBAL", null_device())
        .env("GIT_CONFIG_SYSTEM", null_device())
        .output()?;
    assert!(out.status.success());
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_owned())
}

fn init_repo_with_branch_adding_new_file(
    scratch: &Scratch,
) -> Result<(String, String), Box<dyn Error>> {
    let repo = scratch.path();
    gs_ok(repo, &["init"])?;
    fs::write(repo.join("a.txt"), "base\n")?;
    gs_ok(repo, &["commit", "-m", "base"])?;
    let main_head = git_head(repo)?;
    gs_ok(repo, &["switch", "-c", "br"])?;
    fs::write(repo.join("new.txt"), "incoming\n")?;
    gs_ok(repo, &["commit", "-m", "add new"])?;
    gs_ok(repo, &["switch", "main"])?;
    Ok((main_head, "br".to_owned()))
}

#[test]
fn merge_fast_forward_refuses_untracked_collision() -> TestResult {
    let scratch = Scratch::new("merge-ff-untracked")?;
    let repo = scratch.path();
    let (main_head, branch) = init_repo_with_branch_adding_new_file(&scratch)?;
    fs::write(repo.join("new.txt"), "MY PRECIOUS UNTRACKED\n")?;

    let merge = gs(repo, ["merge", &branch])?;
    assert_ne!(merge.status, Some(0), "{}", merge.dump());
    assert!(
        merge
            .stderr
            .contains("untracked file 'new.txt' would be overwritten")
            || merge
                .stdout
                .contains("untracked file 'new.txt' would be overwritten"),
        "{}",
        merge.dump()
    );
    assert_eq!(read_file(&repo.join("new.txt"))?, "MY PRECIOUS UNTRACKED\n");
    assert_eq!(git_head(repo)?, main_head);
    Ok(())
}

#[test]
fn merge_three_way_refuses_untracked_collision() -> TestResult {
    let scratch = Scratch::new("merge-3way-untracked")?;
    let repo = scratch.path();
    gs_ok(repo, &["init"])?;
    fs::write(repo.join("a.txt"), "base\n")?;
    gs_ok(repo, &["commit", "-m", "base"])?;
    gs_ok(repo, &["switch", "-c", "br"])?;
    fs::write(repo.join("new.txt"), "incoming\n")?;
    gs_ok(repo, &["commit", "-m", "add new"])?;
    gs_ok(repo, &["switch", "main"])?;
    fs::write(repo.join("a.txt"), "main change\n")?;
    gs_ok(repo, &["commit", "-m", "main commit"])?;
    let head_before = git_head(repo)?;
    fs::write(repo.join("new.txt"), "MINE\n")?;

    let merge = gs(repo, ["merge", "br"])?;
    assert_ne!(merge.status, Some(0), "{}", merge.dump());
    assert!(
        merge
            .stderr
            .contains("untracked file 'new.txt' would be overwritten")
            || merge
                .stdout
                .contains("untracked file 'new.txt' would be overwritten"),
        "{}",
        merge.dump()
    );
    assert_eq!(read_file(&repo.join("new.txt"))?, "MINE\n");
    assert_eq!(git_head(repo)?, head_before);
    Ok(())
}

#[test]
fn pick_refuses_untracked_collision() -> TestResult {
    let scratch = Scratch::new("pick-untracked")?;
    let repo = scratch.path();
    let (main_head, branch) = init_repo_with_branch_adding_new_file(&scratch)?;
    let pick_oid = {
        let out = Command::new("git")
            .current_dir(repo)
            .args(["rev-parse", &format!("refs/heads/{branch}")])
            .env("GIT_CONFIG_GLOBAL", null_device())
            .env("GIT_CONFIG_SYSTEM", null_device())
            .output()?;
        assert!(out.status.success());
        String::from_utf8_lossy(&out.stdout).trim().to_owned()
    };
    fs::write(repo.join("new.txt"), "MINE-3\n")?;

    let pick = gs(repo, ["pick", &pick_oid])?;
    assert_ne!(pick.status, Some(0), "{}", pick.dump());
    assert!(
        pick.stderr
            .contains("untracked file 'new.txt' would be overwritten")
            || pick
                .stdout
                .contains("untracked file 'new.txt' would be overwritten"),
        "{}",
        pick.dump()
    );
    assert_eq!(read_file(&repo.join("new.txt"))?, "MINE-3\n");
    assert_eq!(git_head(repo)?, main_head);
    Ok(())
}

#[test]
fn switch_still_refuses_untracked_collision() -> TestResult {
    let scratch = Scratch::new("switch-untracked")?;
    let repo = scratch.path();
    let (_main_head, branch) = init_repo_with_branch_adding_new_file(&scratch)?;
    fs::write(repo.join("new.txt"), "MINE\n")?;

    let sw = gs(repo, ["switch", &branch])?;
    assert_ne!(sw.status, Some(0), "{}", sw.dump());
    assert!(
        sw.stderr
            .contains("untracked file 'new.txt' would be overwritten")
            || sw
                .stdout
                .contains("untracked file 'new.txt' would be overwritten"),
        "{}",
        sw.dump()
    );
    assert_eq!(read_file(&repo.join("new.txt"))?, "MINE\n");
    Ok(())
}

fn assert_refused_overwrite(out: &CmdOutput) {
    assert_ne!(out.status, Some(0), "{}", out.dump());
    assert!(
        out.stderr.contains("would be overwritten") || out.stdout.contains("would be overwritten"),
        "{}",
        out.dump()
    );
}

/// Branch tip adds a **file** at `slot`; main keeps an untracked directory `slot/`.
fn init_repo_branch_adds_file_over_untracked_dir(
    scratch: &Scratch,
) -> Result<(String, String), Box<dyn Error>> {
    let repo = scratch.path();
    gs_ok(repo, &["init"])?;
    fs::write(repo.join("a.txt"), "base\n")?;
    gs_ok(repo, &["commit", "-m", "base"])?;
    let main_head = git_head(repo)?;
    gs_ok(repo, &["switch", "-c", "br"])?;
    fs::write(repo.join("slot"), "incoming-file\n")?;
    gs_ok(repo, &["commit", "-m", "add slot file"])?;
    gs_ok(repo, &["switch", "main"])?;
    fs::create_dir_all(repo.join("slot"))?;
    fs::write(repo.join("slot/precious.txt"), "PRECIOUS\n")?;
    Ok((main_head, "br".to_owned()))
}

/// Branch tip adds `slot/incoming.txt`; main keeps an untracked **file** `slot`.
fn init_repo_branch_adds_path_under_untracked_file(
    scratch: &Scratch,
) -> Result<(String, String), Box<dyn Error>> {
    let repo = scratch.path();
    gs_ok(repo, &["init"])?;
    fs::write(repo.join("a.txt"), "base\n")?;
    gs_ok(repo, &["commit", "-m", "base"])?;
    let main_head = git_head(repo)?;
    gs_ok(repo, &["switch", "-c", "br"])?;
    fs::create_dir_all(repo.join("slot"))?;
    fs::write(repo.join("slot/incoming.txt"), "incoming\n")?;
    gs_ok(repo, &["commit", "-m", "add under slot"])?;
    gs_ok(repo, &["switch", "main"])?;
    fs::write(repo.join("slot"), "PRECIOUS-FILE\n")?;
    Ok((main_head, "br".to_owned()))
}

#[test]
fn merge_refuses_file_over_untracked_directory() -> TestResult {
    let scratch = Scratch::new("merge-file-over-dir")?;
    let repo = scratch.path();
    let (main_head, branch) = init_repo_branch_adds_file_over_untracked_dir(&scratch)?;

    assert_refused_overwrite(&gs(repo, ["merge", &branch])?);
    assert_eq!(read_file(&repo.join("slot/precious.txt"))?, "PRECIOUS\n");
    assert_eq!(git_head(repo)?, main_head);
    Ok(())
}

#[test]
fn merge_refuses_path_under_untracked_file() -> TestResult {
    let scratch = Scratch::new("merge-dir-over-file")?;
    let repo = scratch.path();
    let (main_head, branch) = init_repo_branch_adds_path_under_untracked_file(&scratch)?;

    assert_refused_overwrite(&gs(repo, ["merge", &branch])?);
    assert_eq!(read_file(&repo.join("slot"))?, "PRECIOUS-FILE\n");
    assert_eq!(git_head(repo)?, main_head);
    Ok(())
}

#[test]
fn pick_refuses_file_over_untracked_directory() -> TestResult {
    let scratch = Scratch::new("pick-file-over-dir")?;
    let repo = scratch.path();
    let (main_head, _branch) = init_repo_branch_adds_file_over_untracked_dir(&scratch)?;
    let pick_oid = {
        let out = Command::new("git")
            .current_dir(repo)
            .args(["rev-parse", "refs/heads/br"])
            .env("GIT_CONFIG_GLOBAL", null_device())
            .env("GIT_CONFIG_SYSTEM", null_device())
            .output()?;
        assert!(out.status.success());
        String::from_utf8_lossy(&out.stdout).trim().to_owned()
    };

    assert_refused_overwrite(&gs(repo, ["pick", &pick_oid])?);
    assert_eq!(read_file(&repo.join("slot/precious.txt"))?, "PRECIOUS\n");
    assert_eq!(git_head(repo)?, main_head);
    Ok(())
}

#[test]
fn switch_refuses_file_over_untracked_directory() -> TestResult {
    let scratch = Scratch::new("switch-file-over-dir")?;
    let repo = scratch.path();
    let (_main_head, branch) = init_repo_branch_adds_file_over_untracked_dir(&scratch)?;

    assert_refused_overwrite(&gs(repo, ["switch", &branch])?);
    assert_eq!(read_file(&repo.join("slot/precious.txt"))?, "PRECIOUS\n");
    Ok(())
}

#[test]
fn merge_allows_sibling_untracked_under_same_directory() -> TestResult {
    let scratch = Scratch::new("merge-sibling-ok")?;
    let repo = scratch.path();
    gs_ok(repo, &["init"])?;
    fs::write(repo.join("a.txt"), "base\n")?;
    gs_ok(repo, &["commit", "-m", "base"])?;
    gs_ok(repo, &["switch", "-c", "br"])?;
    fs::create_dir_all(repo.join("slot"))?;
    fs::write(repo.join("slot/incoming.txt"), "incoming\n")?;
    gs_ok(repo, &["commit", "-m", "add incoming"])?;
    gs_ok(repo, &["switch", "main"])?;
    fs::create_dir_all(repo.join("slot"))?;
    fs::write(repo.join("slot/precious.txt"), "PRECIOUS\n")?;

    gs_ok(repo, &["merge", "br"])?;
    assert_eq!(read_file(&repo.join("slot/precious.txt"))?, "PRECIOUS\n");
    assert_eq!(read_file(&repo.join("slot/incoming.txt"))?, "incoming\n");
    Ok(())
}

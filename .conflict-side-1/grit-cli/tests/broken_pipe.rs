//! Piping command output to a short reader must not panic (issue #923).
//!
//! Unix-only: restores default SIGPIPE handling and uses an external `head`.

#![cfg(unix)]

use std::error::Error;
use std::ffi::OsStr;
use std::fs::{self, File};
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};

type TestResult = Result<(), Box<dyn Error>>;

const GS: &str = env!("CARGO_BIN_EXE_grit");

/// Enough changed lines that `grit diff HEAD~1` exceeds a typical pipe buffer
/// before `head -n 1` finishes, forcing the writer to hit a closed pipe.
const LARGE_LINE_COUNT: usize = 80_000;

struct Scratch {
    path: PathBuf,
}

impl Scratch {
    fn new(tag: &str) -> Result<Self, Box<dyn Error>> {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let n = NEXT.fetch_add(1, Ordering::SeqCst);
        let mut path = std::env::temp_dir();
        path.push(format!("grit-broken-pipe-{tag}-{}-{n}", std::process::id()));
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

fn grit<I, S>(dir: &Path, args: I) -> Result<std::process::Output, Box<dyn Error>>
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    Ok(Command::new(GS)
        .args(args)
        .current_dir(dir)
        .env("GIT_AUTHOR_NAME", "Test User")
        .env("GIT_AUTHOR_EMAIL", "test@example.com")
        .env("GIT_COMMITTER_NAME", "Test User")
        .env("GIT_COMMITTER_EMAIL", "test@example.com")
        .env("GIT_AUTHOR_DATE", "1700000000 +0000")
        .env("GIT_COMMITTER_DATE", "1700000000 +0000")
        .output()?)
}

/// Run `grit … | head -n 1` and return grit's exit status and stderr.
fn grit_piped_to_head(
    dir: &Path,
    args: &[&str],
) -> Result<(std::process::ExitStatus, String), Box<dyn Error>> {
    let mut grit = Command::new(GS)
        .args(args)
        .current_dir(dir)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    let grit_stdout = grit
        .stdout
        .take()
        .ok_or_else(|| std::io::Error::other("missing piped stdout"))?;
    let head = Command::new("head")
        .arg("-n")
        .arg("1")
        .stdin(grit_stdout)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()?;
    if !head.success() && head.code() != Some(141) {
        return Err(format!("head failed: {head}").into());
    }
    let out = grit.wait_with_output()?;
    let stderr = String::from_utf8_lossy(&out.stderr).into_owned();
    Ok((out.status, stderr))
}

/// A closed pipe should terminate grit via SIGPIPE (141), not a Rust panic (101).
fn closed_pipe_exit(status: std::process::ExitStatus) -> bool {
    if status.code() == Some(141) || status.code() == Some(128 + 13) {
        return true;
    }
    use std::os::unix::process::ExitStatusExt;
    status.signal() == Some(libc::SIGPIPE)
}

fn write_large_blob(path: &Path, marker: &str) -> Result<(), Box<dyn Error>> {
    let mut file = File::create(path)?;
    for i in 0..LARGE_LINE_COUNT {
        writeln!(file, "{marker} line {i:08} padding padding padding")?;
    }
    Ok(())
}

fn seed_large_diff_repo(dir: &Path) -> TestResult {
    grit(dir, ["init"])?;
    let blob = dir.join("blob.txt");
    write_large_blob(&blob, "before")?;
    grit(dir, ["add", "blob.txt"])?;
    grit(dir, ["commit", "-m", "first"])?;
    write_large_blob(&blob, "after")?;
    grit(dir, ["add", "blob.txt"])?;
    grit(dir, ["commit", "-m", "second"])?;
    Ok(())
}

#[test]
fn large_diff_piped_to_head_does_not_panic() -> TestResult {
    let scratch = Scratch::new("diff")?;
    seed_large_diff_repo(&scratch.path)?;
    let (status, stderr) = grit_piped_to_head(&scratch.path, &["diff", "HEAD~1"])?;
    assert!(
        !stderr.contains("panicked"),
        "stderr contained a panic:\n{stderr}"
    );
    assert!(
        closed_pipe_exit(status),
        "expected SIGPIPE-style exit, got {status:?}, stderr:\n{stderr}"
    );
    Ok(())
}

#[test]
fn large_json_diff_piped_to_head_does_not_panic() -> TestResult {
    let scratch = Scratch::new("json-diff")?;
    seed_large_diff_repo(&scratch.path)?;
    let (status, stderr) = grit_piped_to_head(&scratch.path, &["--json", "diff", "HEAD~1"])?;
    assert!(
        !stderr.contains("panicked"),
        "stderr contained a panic:\n{stderr}"
    );
    assert!(
        closed_pipe_exit(status),
        "expected SIGPIPE-style exit, got {status:?}, stderr:\n{stderr}"
    );
    Ok(())
}

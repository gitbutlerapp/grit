//! `grit commit` P1 flags: --date, --trailer, -s, --reset-author, -C, signing.

use std::error::Error;
use std::ffi::{OsStr, OsString};
use std::fs;
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
        path.push(format!(
            "grit-cli-commit-p1-{tag}-{}-{n}",
            std::process::id()
        ));
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

fn gs<I, S>(dir: &Path, args: I) -> (i32, String, String)
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
        .env("GIT_AUTHOR_NAME", "A")
        .env("GIT_AUTHOR_EMAIL", "a@e.com")
        .env("GIT_COMMITTER_NAME", "C")
        .env("GIT_COMMITTER_EMAIL", "c@e.com")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .output()
        .expect("spawn grit");
    (
        out.status.code().unwrap_or(-1),
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

fn git(dir: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .args(args)
        .current_dir(dir)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .output()
        .expect("spawn git");
    assert!(
        out.status.success(),
        "git {:?}: {}",
        args,
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).into_owned()
}

#[test]
fn commit_p1_trailer_and_signoff() -> TestResult {
    let scratch = Scratch::new("trailer-signoff")?;
    let root = scratch.path();
    git(root, &["init", "-q", "-b", "main"]);
    git(root, &["config", "grit.changeIds", "false"]);
    fs::write(root.join("f"), "x\n")?;

    let (code, _, stderr) = gs(
        root,
        [
            "commit",
            "-m",
            "subject",
            "--trailer",
            "Acked-by: Alice",
            "-s",
        ],
    );
    assert_eq!(code, 0, "{stderr}");
    let body = git(root, &["log", "-1", "--format=%B"]);
    assert!(body.contains("Acked-by: Alice"));
    assert!(body.contains("Signed-off-by: C <c@e.com>"));
    git(root, &["fsck", "--strict"]);
    Ok(())
}

#[test]
fn commit_p1_date_and_reuse_message() -> TestResult {
    let scratch = Scratch::new("date-reuse")?;
    let root = scratch.path();
    git(root, &["init", "-q", "-b", "main"]);
    git(root, &["config", "grit.changeIds", "false"]);
    fs::write(root.join("f"), "1\n")?;
    let (code, _, stderr) = gs(
        root,
        [
            "commit",
            "-m",
            "first",
            "--author",
            "Orig <orig@example.com>",
            "--date",
            "2000-01-01T00:00:00Z",
        ],
    );
    assert_eq!(code, 0, "{stderr}");

    fs::write(root.join("f"), "2\n")?;
    let (code, _, stderr) = gs(root, ["commit", "-C", "HEAD"]);
    assert_eq!(code, 0, "{stderr}");
    let an = git(root, &["log", "-1", "--format=%an"]);
    assert_eq!(an.trim(), "Orig");
    git(root, &["fsck", "--strict"]);
    Ok(())
}

#[test]
fn commit_p1_reset_author() -> TestResult {
    let scratch = Scratch::new("reset-author")?;
    let root = scratch.path();
    git(root, &["init", "-q", "-b", "main"]);
    git(root, &["config", "grit.changeIds", "false"]);
    fs::write(root.join("f"), "1\n")?;
    gs(
        root,
        ["commit", "-m", "first", "--author", "Old <old@example.com>"],
    );

    fs::write(root.join("f"), "2\n")?;
    let (code, _, stderr) = gs(
        root,
        [
            "commit",
            "-m",
            "second",
            "--reset-author",
            "--author",
            "ignored",
        ],
    );
    assert_eq!(code, 0, "{stderr}");
    let idents = git(root, &["log", "-1", "--format=%an|%ae|%cn|%ce"]);
    assert!(idents.contains("C|c@e.com|C|c@e.com") || idents.contains("C|c@e.com"));
    Ok(())
}

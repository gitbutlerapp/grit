//! `grit commit -F`, `--author`, `--no-verify`, and JSON outcome fields.

use std::error::Error;
use std::ffi::{OsStr, OsString};
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
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
    fn success(&self) -> bool {
        self.status == Some(0)
    }

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
        path.push(format!(
            "grit-cli-commit-msg-author-{tag}-{}-{n}",
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

fn gs<I, S>(dir: &Path, args: I) -> CmdOutput
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
        .env("GIT_AUTHOR_NAME", "R")
        .env("GIT_AUTHOR_EMAIL", "r@e.com")
        .env("GIT_COMMITTER_NAME", "R")
        .env("GIT_COMMITTER_EMAIL", "r@e.com")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .output()
        .expect("spawn grit");
    CmdOutput {
        status: out.status.code(),
        stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
    }
}

fn git(dir: &Path, args: &[&str]) -> CmdOutput {
    let out = Command::new("git")
        .args(args)
        .current_dir(dir)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .output()
        .expect("spawn git");
    CmdOutput {
        status: out.status.code(),
        stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
    }
}

#[test]
fn message_file_stdin_and_json_author() -> TestResult {
    let scratch = Scratch::new("stdin-json")?;
    let root = scratch.path();
    git(root, &["init", "-q", "-b", "main"]);
    fs::write(root.join("a.txt"), "hi\n")?;

    let mut child = Command::new(GS)
        .args([
            "commit",
            "-F",
            "-",
            "--author",
            "Alt Author <alt@example.com>",
            "--json",
        ])
        .current_dir(root)
        .env("GIT_COMMITTER_NAME", "Committer")
        .env("GIT_COMMITTER_EMAIL", "committer@example.com")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    child
        .stdin
        .take()
        .expect("stdin")
        .write_all(b"  subject from stdin  \n\n")?;
    let out = child.wait_with_output()?;
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );

    let json: serde_json::Value = serde_json::from_slice(&out.stdout)?;
    assert_eq!(json["author"]["name"], "Alt Author");
    assert_eq!(json["author"]["email"], "alt@example.com");
    assert!(json["author"]["date"].is_string());
    assert!(json["parents"].is_array());
    assert_eq!(json["parents"].as_array().unwrap().len(), 0);

    let body = git(root, &["log", "-1", "--format=%B"]).stdout;
    assert!(
        body.starts_with("subject from stdin\n"),
        "unexpected body: {body:?}"
    );

    let idents = git(root, &["log", "-1", "--format=%an|%ae|%cn|%ce"]).stdout;
    assert_eq!(
        idents.trim(),
        "Alt Author|alt@example.com|Committer|committer@example.com"
    );
    assert!(git(root, &["fsck", "--strict"]).success());
    Ok(())
}

#[test]
fn message_sources_conflict() -> TestResult {
    let scratch = Scratch::new("conflict")?;
    let root = scratch.path();
    git(root, &["init", "-q", "-b", "main"]);
    fs::write(root.join("a.txt"), "x\n")?;
    let out = gs(root, ["commit", "-m", "one", "-F", "-", "--allow-empty"]);
    assert!(!out.success());
    assert!(
        out.stderr.contains("cannot be used with")
            || out.stderr.contains("conflict")
            || out.stderr.contains("message-file"),
        "{}",
        out.dump()
    );
    Ok(())
}

#[test]
fn no_verify_skips_blocking_hooks() -> TestResult {
    let scratch = Scratch::new("no-verify")?;
    let root = scratch.path();
    git(root, &["init", "-q", "-b", "main"]);
    fs::write(
        root.join(".git/config"),
        "[core]\n\trepositoryformatversion = 0\n[hook \"hc\"]\n\tcommand = exit 1\n\tevent = pre-commit\n",
    )?;
    fs::write(root.join("a.txt"), "y\n")?;
    let blocked = gs(root, ["commit", "-m", "fail"]);
    assert!(!blocked.success());
    let ok = gs(root, ["commit", "-m", "ok", "--no-verify", "--json"]);
    assert!(ok.success(), "{}", ok.dump());
    let json: serde_json::Value = serde_json::from_str(&ok.stdout)?;
    assert_eq!(json["hooks_run"], serde_json::json!([]));
    Ok(())
}

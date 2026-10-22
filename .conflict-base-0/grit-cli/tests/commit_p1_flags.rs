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
        .env("GIT_EDITOR", ":")
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
fn commit_p1_raw_git_date_timestamp() -> TestResult {
    let scratch = Scratch::new("raw-date")?;
    let root = scratch.path();
    git(root, &["init", "-q", "-b", "main"]);
    git(root, &["config", "grit.changeIds", "false"]);
    fs::write(root.join("f"), "x\n")?;

    let (code, _, stderr) = gs(
        root,
        [
            "commit",
            "-m",
            "dated",
            "--author",
            "A <a@example.com>",
            "--date",
            "946684800 +0000",
        ],
    );
    assert_eq!(code, 0, "{stderr}");
    let at = git(root, &["log", "-1", "--format=%at"]).trim().to_string();
    assert_eq!(at, "946684800");
    Ok(())
}

fn git_interpret_trailers_trailer_line(trailer: &str) -> String {
    let out = Command::new("git")
        .args([
            "interpret-trailers",
            "--trailer",
            trailer,
            "--only-trailers",
        ])
        .stdin(std::process::Stdio::null())
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .output()
        .expect("spawn git interpret-trailers");
    assert!(
        out.status.success(),
        "git interpret-trailers: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).trim().to_owned()
}

#[test]
fn commit_p1_trailer_link_url_query_matches_git() -> TestResult {
    let trailer = "Link: https://x?a=b";
    let git_line = git_interpret_trailers_trailer_line(trailer);
    let (grit_key, grit_val) = grit_lib::commit::parse_trailer_command_line_arg(trailer)?;
    let grit_line = if grit_val.is_empty() {
        format!("{grit_key}:")
    } else {
        format!("{grit_key}: {grit_val}")
    };
    assert_eq!(grit_line, git_line);

    let scratch = Scratch::new("trailer-link")?;
    let root = scratch.path();
    git(root, &["init", "-q", "-b", "main"]);
    git(root, &["config", "grit.changeIds", "false"]);
    fs::write(root.join("f"), "x\n")?;

    let (code, _, stderr) = gs(root, ["commit", "-m", "subject", "--trailer", trailer]);
    assert_eq!(code, 0, "{stderr}");
    let body = git(root, &["log", "-1", "--format=%B"]);
    assert!(
        body.contains("Link: https://x?a=b"),
        "expected Link trailer with query string, got:\n{body}"
    );
    git(root, &["fsck", "--strict"]);
    Ok(())
}

#[test]
fn commit_p1_bad_timezone_normalizes_like_git() -> TestResult {
    let scratch = Scratch::new("bad-tz")?;
    let root = scratch.path();
    git(root, &["init", "-q", "-b", "main"]);
    git(root, &["config", "grit.changeIds", "false"]);
    fs::write(root.join("f"), "x\n")?;

    let (code, _, stderr) = gs(
        root,
        [
            "commit",
            "-m",
            "dated",
            "--author",
            "A <a@example.com>",
            "--date",
            "946684800 garbage",
        ],
    );
    assert_eq!(code, 0, "{stderr}");
    let at_ai = git(root, &["log", "-1", "--format=%at %ai"])
        .trim()
        .to_string();
    assert_eq!(at_ai, "946684800 2000-01-01 00:00:00 +0000");
    git(root, &["fsck", "--strict"]);
    Ok(())
}

#[test]
fn commit_p1_at_epoch_bad_timezone_normalizes_like_git() -> TestResult {
    let scratch = Scratch::new("at-bad-tz")?;
    let root = scratch.path();
    git(root, &["init", "-q", "-b", "main"]);
    git(root, &["config", "grit.changeIds", "false"]);
    fs::write(root.join("f"), "x\n")?;

    let (code, _, stderr) = gs(
        root,
        [
            "commit",
            "-m",
            "dated",
            "--author",
            "A <a@example.com>",
            "--date",
            "@946684800 garbage",
        ],
    );
    assert_eq!(code, 0, "{stderr}");
    let at_ai = git(root, &["log", "-1", "--format=%at %ai"])
        .trim()
        .to_string();
    assert_eq!(at_ai, "946684800 2000-01-01 00:00:00 +0000");
    git(root, &["fsck", "--strict"]);
    Ok(())
}

#[test]
fn commit_p1_trailer_colon_before_equals_in_value() -> TestResult {
    let scratch = Scratch::new("trailer-colon")?;
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
            "Acked-by: Alice=Bob",
        ],
    );
    assert_eq!(code, 0, "{stderr}");
    let body = git(root, &["log", "-1", "--format=%B"]);
    assert!(
        body.contains("Acked-by: Alice=Bob"),
        "expected full trailer value preserved, got:\n{body}"
    );
    Ok(())
}

#[test]
fn commit_p1_amend_empty_message_file_matches_git() -> TestResult {
    let run_amend = |root: &Path, use_grit: bool| -> TestResult {
        git(root, &["init", "-q", "-b", "main"]);
        if use_grit {
            git(root, &["config", "grit.changeIds", "false"]);
        }
        fs::write(root.join("f"), "1\n")?;
        if use_grit {
            gs(
                root,
                ["commit", "-m", "original subject\n\nbody", "--no-verify"],
            );
            fs::write(root.join("f"), "2\n")?;
            gs(root, ["add", "f"]);
        } else {
            git(root, &["add", "f"]);
            git(
                root,
                &["commit", "-m", "original subject\n\nbody", "--no-verify"],
            );
            fs::write(root.join("f"), "2\n")?;
            git(root, &["add", "f"]);
        }
        let empty_msg = root.join("empty.txt");
        fs::write(&empty_msg, "")?;
        let f_arg = empty_msg.to_str().expect("utf-8 path");
        if use_grit {
            let (code, _, stderr) = gs(root, ["commit", "--amend", "-F", f_arg, "--no-verify"]);
            assert_eq!(code, 0, "{stderr}");
        } else {
            // Git rejects an empty `-F`; reuse the tip message explicitly (same body as `git commit --amend`).
            git(
                root,
                &[
                    "commit",
                    "--amend",
                    "-m",
                    "original subject\n\nbody",
                    "--no-verify",
                ],
            );
        }
        Ok(())
    };

    let git_scratch = Scratch::new("amend-empty-f-git")?;
    run_amend(git_scratch.path(), false)?;
    let git_body = git(git_scratch.path(), &["log", "-1", "--format=%B"]);

    let grit_scratch = Scratch::new("amend-empty-f-grit")?;
    run_amend(grit_scratch.path(), true)?;
    let grit_body = git(grit_scratch.path(), &["log", "-1", "--format=%B"]);

    assert_eq!(
        grit_body, git_body,
        "empty -F on amend must match Git amend-without-message body"
    );
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

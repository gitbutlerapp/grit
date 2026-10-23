//! CLI commands in a blobless clone with the default promisor fetcher installed.

use std::fs;
use std::path::Path;
use std::process::Command;

type TestResult = Result<(), Box<dyn std::error::Error>>;

const GRIT: &str = env!("CARGO_BIN_EXE_grit");

fn git_in(dir: &Path, args: &[&str]) -> TestResult {
    let status = Command::new("git")
        .args(args)
        .current_dir(dir)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .env("GIT_AUTHOR_NAME", "T")
        .env("GIT_AUTHOR_EMAIL", "t@example.com")
        .env("GIT_COMMITTER_NAME", "T")
        .env("GIT_COMMITTER_EMAIL", "t@example.com")
        .status()?;
    assert!(status.success(), "git {:?} in {}", args, dir.display());
    Ok(())
}

fn grit_in(dir: &Path, args: &[&str]) -> TestResult {
    let out = Command::new(GRIT)
        .args(args)
        .current_dir(dir)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .output()?;
    assert!(
        out.status.success(),
        "grit {:?} in {}: status={} stderr={}",
        args,
        dir.display(),
        out.status,
        String::from_utf8_lossy(&out.stderr)
    );
    Ok(())
}

#[test]
fn blobless_clone_grit_commands_fetch_on_demand() -> TestResult {
    let layout = tempfile::tempdir()?;
    let upstream = layout.path().join("upstream");
    git_in(layout.path(), &["init", "-q", "-b", "main", "upstream"])?;
    git_in(&upstream, &["config", "user.email", "t@t"])?;
    fs::write(upstream.join("one.txt"), b"one\n")?;
    git_in(&upstream, &["add", "one.txt"])?;
    git_in(&upstream, &["commit", "-m", "one", "-q"])?;
    fs::write(upstream.join("two.txt"), b"two\n")?;
    git_in(&upstream, &["add", "two.txt"])?;
    git_in(&upstream, &["commit", "-m", "two", "-q"])?;
    git_in(&upstream, &["branch", "other"])?;
    fs::write(upstream.join("other.txt"), b"other\n")?;
    git_in(&upstream, &["add", "other.txt"])?;
    git_in(&upstream, &["commit", "-m", "other", "-q"])?;
    git_in(&upstream, &["switch", "main"])?;

    let bare = layout.path().join("remote.git");
    git_in(
        layout.path(),
        &[
            "clone",
            "-q",
            "--bare",
            upstream.to_str().unwrap(),
            "remote.git",
        ],
    )?;
    git_in(&bare, &["config", "uploadpack.allowFilter", "true"])?;

    let url = format!("file://{}", bare.canonicalize()?.display());
    let clone = layout.path().join("blobless");
    git_in(
        layout.path(),
        &[
            "clone",
            "-q",
            "--filter=blob:none",
            "--no-local",
            &url,
            clone.to_str().unwrap(),
        ],
    )?;

    grit_in(&clone, &["diff", "HEAD~1"])?;
    grit_in(&clone, &["show", "HEAD:one.txt"])?;
    grit_in(&clone, &["switch", "other"])?;
    Ok(())
}

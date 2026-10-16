use std::path::Path;
use std::process::Command;

use anyhow::{Context, Result};

fn git(dir: &Path, args: &[&str]) -> Result<String> {
    let out = Command::new("git")
        .args(args)
        .current_dir(dir)
        .env("GIT_AUTHOR_NAME", "t")
        .env("GIT_AUTHOR_EMAIL", "t@t")
        .env("GIT_COMMITTER_NAME", "t")
        .env("GIT_COMMITTER_EMAIL", "t@t")
        .output()
        .context("run git")?;
    if !out.status.success() {
        anyhow::bail!(
            "git {args:?} failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_owned())
}

fn source_and_consumer(root: &Path) -> Result<(std::path::PathBuf, std::path::PathBuf)> {
    let work = root.join("work");
    std::fs::create_dir_all(&work)?;
    git(&work, &["init", "-q", "-b", "main", "."])?;
    git(&work, &["commit", "-q", "--allow-empty", "-m", "c1"])?;
    let bare = root.join("repo.git");
    git(
        root,
        &[
            "clone",
            "-q",
            "--bare",
            work.to_str().unwrap(),
            bare.to_str().unwrap(),
        ],
    )?;

    let consumer = root.join("consumer");
    std::fs::create_dir_all(&consumer)?;
    git(&consumer, &["init", "-q", "-b", "main", "."])?;
    let file_url = format!("file://{}", bare.canonicalize()?.display());
    git(&consumer, &["remote", "add", "origin", &file_url])?;
    Ok((consumer, bare))
}

#[test]
fn guide_network_fetch_push_fsck_clean() -> Result<()> {
    let tmp = tempfile::tempdir()?;
    let (consumer, bare) = source_and_consumer(tmp.path())?;

    let exe = env!("CARGO_BIN_EXE_guide_network");
    let output = Command::new(exe).arg(&consumer).output()?;
    assert!(
        output.status.success(),
        "guide_network failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let pushed = String::from_utf8(output.stdout)?
        .lines()
        .next()
        .context("guide_network printed no oid")?
        .trim()
        .to_owned();

    for dir in [&consumer, &bare] {
        let fsck = Command::new("git")
            .args(["fsck", "--strict"])
            .current_dir(dir)
            .output()?;
        assert!(
            fsck.status.success(),
            "git fsck --strict failed in {}: {}",
            dir.display(),
            String::from_utf8_lossy(&fsck.stderr)
        );
    }

    assert_eq!(
        git(&bare, &["rev-parse", "refs/heads/main"])?,
        pushed,
        "remote main should match pushed commit"
    );

    Ok(())
}

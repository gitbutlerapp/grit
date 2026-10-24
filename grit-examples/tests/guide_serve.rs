//! `guide_serve` as the server end of real `git clone` and `git push` runs.

use std::path::Path;
use std::process::{Command, Output};

use anyhow::{Context, Result};

const SERVE: &str = env!("CARGO_BIN_EXE_guide_serve");

fn git_output(dir: &Path, args: &[&str]) -> Result<Output> {
    Command::new("git")
        .args(args)
        .current_dir(dir)
        .env("GIT_AUTHOR_NAME", "t")
        .env("GIT_AUTHOR_EMAIL", "t@t")
        .env("GIT_COMMITTER_NAME", "t")
        .env("GIT_COMMITTER_EMAIL", "t@t")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .output()
        .context("run git")
}

fn git(dir: &Path, args: &[&str]) -> Result<String> {
    let out = git_output(dir, args)?;
    if !out.status.success() {
        anyhow::bail!(
            "git {args:?} failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_owned())
}

#[test]
fn clone_and_push_through_guide_serve() -> Result<()> {
    let tmp = tempfile::tempdir()?;
    let root = tmp.path();
    let server = root.join("server.git");
    std::fs::create_dir_all(&server)?;
    git(&server, &["init", "-q", "--bare", "-b", "main", "."])?;
    let seed = root.join("seed");
    std::fs::create_dir_all(&seed)?;
    git(&seed, &["init", "-q", "-b", "main", "."])?;
    std::fs::write(seed.join("a"), "a\n")?;
    git(&seed, &["add", "a"])?;
    git(&seed, &["commit", "-q", "-m", "seed"])?;
    let server_path = server.to_str().context("utf8 path")?;
    git(&seed, &["push", "-q", server_path, "main"])?;

    let upload = format!("--upload-pack={SERVE} upload-pack");
    let receive = format!("--receive-pack={SERVE} receive-pack");
    for protocol in ["version=0", "version=2"] {
        let clone = root.join(format!("clone-{protocol}"));
        let clone_path = clone.to_str().context("utf8 path")?;
        git(
            root,
            &[
                "-c",
                &format!("protocol.version={}", &protocol[8..]),
                "clone",
                "-q",
                &upload,
                server_path,
                clone_path,
            ],
        )?;
        assert_eq!(
            git(&clone, &["rev-parse", "HEAD"])?,
            git(&server, &["rev-parse", "main"])?
        );
    }

    let work = root.join("clone-version=2");
    std::fs::write(work.join("b"), "b\n")?;
    git(&work, &["add", "b"])?;
    git(&work, &["commit", "-q", "-m", "second"])?;
    git(&work, &["push", "-q", &receive, server_path, "main"])?;
    assert_eq!(
        git(&server, &["rev-parse", "main"])?,
        git(&work, &["rev-parse", "HEAD"])?
    );
    git(&server, &["fsck", "--strict", "--no-progress"])?;

    let refused = git_output(
        &work,
        &[
            "push",
            &receive,
            server_path,
            "HEAD:refs/heads/protected/release",
        ],
    )?;
    assert!(!refused.status.success());
    let stderr = String::from_utf8_lossy(&refused.stderr);
    assert!(
        stderr.contains("[remote rejected]") && stderr.contains("protected branch"),
        "{stderr}"
    );
    assert!(git(
        &server,
        &["rev-parse", "--verify", "-q", "protected/release"]
    )
    .is_err());
    Ok(())
}

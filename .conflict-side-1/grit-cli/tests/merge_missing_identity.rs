//! Clean merge must not partially apply when commit identity is missing (#1034 review).

use std::error::Error;
use std::fs;
use std::path::Path;
use std::process::Command;

type TestResult = Result<(), Box<dyn Error>>;

fn null_config() -> &'static str {
    if cfg!(windows) {
        "NUL"
    } else {
        "/dev/null"
    }
}

fn git(dir: &Path, args: &[&str]) -> Result<(), Box<dyn Error>> {
    let out = Command::new("git")
        .args(args)
        .current_dir(dir)
        .env("GIT_CONFIG_GLOBAL", null_config())
        .env("GIT_CONFIG_SYSTEM", null_config())
        .output()?;
    if !out.status.success() {
        return Err(format!("git {:?} failed", args).into());
    }
    Ok(())
}

fn index_bytes(dir: &Path) -> Vec<u8> {
    fs::read(dir.join(".git/index")).expect("index")
}

#[test]
fn merge_clean_failure_leaves_head_index_and_worktree() -> TestResult {
    let dir = tempfile::TempDir::new()?;
    let root = dir.path();

    git(root, &["init", "-q", "-b", "main"])?;
    git(root, &["config", "user.name", "T"])?;
    git(root, &["config", "user.email", "t@e.com"])?;
    fs::write(root.join("shared.txt"), "base\n")?;
    git(root, &["add", "shared.txt"])?;
    git(root, &["commit", "-qm", "base"])?;

    git(root, &["switch", "-c", "feature"])?;
    fs::write(root.join("feature.txt"), "feature\n")?;
    git(root, &["add", "feature.txt"])?;
    git(root, &["commit", "-qm", "feature"])?;

    git(root, &["switch", "main"])?;
    fs::write(root.join("main.txt"), "main\n")?;
    git(root, &["add", "main.txt"])?;
    git(root, &["commit", "-qm", "main"])?;

    git(root, &["config", "--unset", "user.name"])?;
    git(root, &["config", "--unset", "user.email"])?;

    let head_before = String::from_utf8(
        Command::new("git")
            .args(["rev-parse", "HEAD"])
            .current_dir(root)
            .output()?
            .stdout,
    )?
    .trim()
    .to_owned();
    let index_before = index_bytes(root);
    let main_before = fs::read(root.join("main.txt"))?;

    let merge = Command::new(env!("CARGO_BIN_EXE_grit"))
        .current_dir(root)
        .args(["merge", "feature"])
        .env("GIT_CONFIG_GLOBAL", null_config())
        .env("GIT_CONFIG_SYSTEM", null_config())
        .env_remove("GIT_AUTHOR_EMAIL")
        .env_remove("GIT_COMMITTER_EMAIL")
        .env_remove("GIT_AUTHOR_NAME")
        .env_remove("GIT_COMMITTER_NAME")
        .env_remove("EMAIL")
        .output()?;
    assert!(!merge.status.success(), "merge should fail: {:?}", merge);

    let head_after = String::from_utf8(
        Command::new("git")
            .args(["rev-parse", "HEAD"])
            .current_dir(root)
            .output()?
            .stdout,
    )?
    .trim()
    .to_owned();
    assert_eq!(head_after, head_before);
    assert!(!root.join(".git/MERGE_HEAD").exists());
    assert_eq!(index_bytes(root), index_before);
    assert_eq!(fs::read(root.join("main.txt"))?, main_before);
    assert!(!root.join("feature.txt").exists());

    let abort = Command::new(env!("CARGO_BIN_EXE_grit"))
        .current_dir(root)
        .args(["merge", "--abort"])
        .env("GIT_CONFIG_GLOBAL", null_config())
        .env("GIT_CONFIG_SYSTEM", null_config())
        .output()?;
    assert!(!abort.status.success());

    Ok(())
}

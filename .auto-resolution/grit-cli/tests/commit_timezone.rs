//! Commit author/committer lines must carry the local UTC offset (issue #922).

use std::error::Error;
use std::fs;
use std::path::Path;
use std::process::Command;

type TestResult = Result<(), Box<dyn Error>>;

const GS: &str = env!("CARGO_BIN_EXE_grit");

fn grit_commit(dir: &Path, tz: &str) -> Result<(), Box<dyn Error>> {
    fs::write(dir.join("a"), "a\n")?;
    let out = Command::new(GS)
        .args(["commit", "tz test"])
        .current_dir(dir)
        .env("TZ", tz)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .env_remove("GIT_AUTHOR_DATE")
        .env_remove("GIT_COMMITTER_DATE")
        .env("GIT_AUTHOR_NAME", "T")
        .env("GIT_AUTHOR_EMAIL", "t@example.com")
        .env("GIT_COMMITTER_NAME", "T")
        .env("GIT_COMMITTER_EMAIL", "t@example.com")
        .output()?;
    assert!(
        out.status.success(),
        "grit commit failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    Ok(())
}

fn git_var_author_ident(dir: &Path, tz: &str) -> Result<String, Box<dyn Error>> {
    let out = Command::new("git")
        .args(["var", "GIT_AUTHOR_IDENT"])
        .current_dir(dir)
        .env("TZ", tz)
        .output()?;
    assert!(out.status.success());
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_owned())
}

fn role_offset_from_commit(dir: &Path, role: &str) -> Result<String, Box<dyn Error>> {
    let prefix = format!("{role} ");
    let out = Command::new("git")
        .args(["cat-file", "-p", "HEAD"])
        .current_dir(dir)
        .output()?;
    assert!(out.status.success());
    let text = String::from_utf8_lossy(&out.stdout);
    let line = text
        .lines()
        .find(|l| l.starts_with(prefix.as_str()))
        .ok_or_else(|| format!("missing {role} line"))?;
    let offset = line.rsplit(' ').next().ok_or("missing offset")?;
    Ok(offset.to_owned())
}

fn git_var_committer_ident(dir: &Path, tz: &str) -> Result<String, Box<dyn Error>> {
    let out = Command::new("git")
        .args(["var", "GIT_COMMITTER_IDENT"])
        .current_dir(dir)
        .env("TZ", tz)
        .output()?;
    assert!(out.status.success());
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_owned())
}

#[test]
fn commit_identity_uses_local_timezone_offset() -> TestResult {
    let dir = tempfile::tempdir()?;
    let repo = dir.path();
    let tz = "Europe/Berlin";
    Command::new("git")
        .args(["init", "-q"])
        .current_dir(repo)
        .env("TZ", tz)
        .status()?;
    Command::new("git")
        .args(["config", "user.name", "T"])
        .current_dir(repo)
        .status()?;
    Command::new("git")
        .args(["config", "user.email", "t@example.com"])
        .current_dir(repo)
        .status()?;

    grit_commit(repo, tz)?;

    let expected_author = git_var_author_ident(repo, tz)?
        .rsplit(' ')
        .next()
        .ok_or("git var ident missing offset")?
        .to_owned();
    let actual_author = role_offset_from_commit(repo, "author")?;
    assert_eq!(
        actual_author, expected_author,
        "grit commit author offset should match git var GIT_AUTHOR_IDENT"
    );

    let expected_committer = git_var_committer_ident(repo, tz)?
        .rsplit(' ')
        .next()
        .ok_or("git var committer ident missing offset")?
        .to_owned();
    let actual_committer = role_offset_from_commit(repo, "committer")?;
    assert_eq!(
        actual_committer, expected_committer,
        "grit commit committer offset should match git var GIT_COMMITTER_IDENT"
    );

    let fsck = Command::new("git")
        .args(["fsck", "--strict"])
        .current_dir(repo)
        .output()?;
    assert!(
        fsck.status.success(),
        "git fsck failed: {}",
        String::from_utf8_lossy(&fsck.stderr)
    );
    Ok(())
}

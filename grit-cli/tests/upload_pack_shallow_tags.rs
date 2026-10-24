//! System `git clone --depth` served by `grit upload-pack` from a repository
//! with annotated tags matches `git-upload-pack` (issue #1734): one commit for
//! `--depth 1`, only tags on the sent commits, and commit-only shallow files.

#![cfg(unix)]

use std::error::Error;
use std::path::{Path, PathBuf};
use std::process::Command;

type TestResult = Result<(), Box<dyn Error>>;

const GRIT: &str = env!("CARGO_BIN_EXE_grit");

fn git(dir: &Path, args: &[&str]) -> Result<String, Box<dyn Error>> {
    let out = Command::new("git")
        .current_dir(dir)
        .args(args)
        .env("GIT_AUTHOR_NAME", "T")
        .env("GIT_AUTHOR_EMAIL", "t@example.com")
        .env("GIT_COMMITTER_NAME", "T")
        .env("GIT_COMMITTER_EMAIL", "t@example.com")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .output()?;
    if !out.status.success() {
        return Err(format!(
            "git {args:?} failed: {}",
            String::from_utf8_lossy(&out.stderr)
        )
        .into());
    }
    Ok(String::from_utf8(out.stdout)?)
}

/// Bare repo with 30 linear commits and annotated tags v10, v20, v30.
fn tagged_bare_repo(root: &Path) -> Result<PathBuf, Box<dyn Error>> {
    let work = root.join("work");
    std::fs::create_dir_all(&work)?;
    git(&work, &["init", "-q", "-b", "main"])?;
    for i in 1..=30 {
        std::fs::write(work.join(format!("f{i}")), format!("{i}\n"))?;
        git(&work, &["add", "."])?;
        git(&work, &["commit", "-q", "-m", &format!("c{i}")])?;
        if i % 10 == 0 {
            git(&work, &["tag", "-a", "-m", "t", &format!("v{i}")])?;
        }
    }
    let bare = root.join("src.git");
    git(
        root,
        &[
            "clone",
            "-q",
            "--bare",
            work.to_str().ok_or("utf8")?,
            bare.to_str().ok_or("utf8")?,
        ],
    )?;
    Ok(bare)
}

/// What a clone ended up with, for comparing grit's server against git's.
#[derive(Debug, PartialEq)]
struct CloneSummary {
    commits: usize,
    in_pack: usize,
    shallow: Vec<String>,
}

fn clone_with(
    root: &Path,
    bare: &Path,
    name: &str,
    upload_pack: &str,
    protocol: &str,
    extra: &[&str],
) -> Result<CloneSummary, Box<dyn Error>> {
    let dest = root.join(name);
    let url = format!("file://{}", bare.canonicalize()?.display());
    let proto = format!("protocol.version={protocol}");
    let upload = format!("--upload-pack={upload_pack}");
    let mut args = vec!["-c", &proto, "clone", "-q", "--depth", "1", &upload];
    args.extend(extra);
    args.extend([url.as_str(), dest.to_str().ok_or("utf8")?]);
    git(root, &args)?;
    git(&dest, &["fsck", "--no-dangling"])?;

    let git_dir = if dest.join(".git").is_dir() {
        dest.join(".git")
    } else {
        dest.clone()
    };
    let mut shallow: Vec<String> = std::fs::read_to_string(git_dir.join("shallow"))?
        .lines()
        .map(str::to_owned)
        .collect();
    shallow.sort();
    for oid in &shallow {
        assert_eq!(
            git(&dest, &["cat-file", "-t", oid])?.trim(),
            "commit",
            "{name}: shallow entry {oid} must be a commit"
        );
    }
    let in_pack = git(&dest, &["count-objects", "-v"])?
        .lines()
        .find_map(|l| l.strip_prefix("in-pack: "))
        .ok_or("no in-pack line")?
        .parse()?;
    let commits = git(&dest, &["rev-list", "--count", "--all"])?
        .trim()
        .parse()?;
    Ok(CloneSummary {
        commits,
        in_pack,
        shallow,
    })
}

fn assert_grit_matches_git(protocol: &str, extra: &[&str]) -> Result<CloneSummary, Box<dyn Error>> {
    let tmp = tempfile::tempdir()?;
    let bare = tagged_bare_repo(tmp.path())?;
    let grit_server = format!("{GRIT} upload-pack");
    let from_git = clone_with(tmp.path(), &bare, "git", "git-upload-pack", protocol, extra)?;
    let from_grit = clone_with(tmp.path(), &bare, "grit", &grit_server, protocol, extra)?;
    assert_eq!(from_grit, from_git, "protocol v{protocol} {extra:?}");
    Ok(from_grit)
}

#[test]
fn depth_one_bare_clone_sends_one_commit_v2() -> TestResult {
    let summary = assert_grit_matches_git("2", &["--bare"])?;
    assert_eq!(summary.commits, 1);
    assert_eq!(summary.in_pack, 33);
    Ok(())
}

#[test]
fn depth_one_bare_clone_sends_one_commit_v0() -> TestResult {
    let summary = assert_grit_matches_git("0", &["--bare"])?;
    assert_eq!(summary.commits, 1);
    assert_eq!(summary.in_pack, 33);
    Ok(())
}

#[test]
fn depth_one_no_single_branch_clone_succeeds() -> TestResult {
    let summary = assert_grit_matches_git("2", &["--no-single-branch"])?;
    assert_eq!(summary.commits, 3);
    assert_eq!(summary.in_pack, 39);
    assert_eq!(summary.shallow.len(), 3);
    assert_grit_matches_git("0", &["--no-single-branch"])?;
    Ok(())
}

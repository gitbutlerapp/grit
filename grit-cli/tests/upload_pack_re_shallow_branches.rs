//! Re-shallow with multiple wants (issue #2447): `fetch --depth 1` must not
//! unshallow boundaries outside the new cut or download full history.

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

fn issue_topology_bare(root: &Path) -> Result<PathBuf, Box<dyn Error>> {
    let work = root.join("work");
    std::fs::create_dir_all(&work)?;
    git(&work, &["init", "-q", "-b", "main"])?;
    for i in 1..=6 {
        std::fs::write(work.join("f"), format!("{i}\n"))?;
        git(&work, &["add", "f"])?;
        git(&work, &["commit", "-q", "-m", &format!("c{i}")])?;
    }
    git(&work, &["checkout", "-q", "-b", "a"])?;
    std::fs::write(work.join("g"), "a\n")?;
    git(&work, &["add", "g"])?;
    git(&work, &["commit", "-q", "-m", "a1"])?;
    git(&work, &["checkout", "-q", "-b", "z", "main"])?;
    std::fs::write(work.join("h"), "z\n")?;
    git(&work, &["add", "h"])?;
    git(&work, &["commit", "-q", "-m", "z1"])?;
    git(&work, &["checkout", "-q", "main"])?;
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

fn shallow_summary(clone_dir: &Path) -> Result<(usize, usize, Vec<String>), Box<dyn Error>> {
    let all = git(clone_dir, &["rev-list", "--count", "--all"])?
        .trim()
        .parse::<usize>()?;
    let head = git(clone_dir, &["rev-list", "--count", "HEAD"])?
        .trim()
        .parse::<usize>()?;
    let git_dir = if clone_dir.join(".git").is_dir() {
        clone_dir.join(".git")
    } else {
        clone_dir.to_path_buf()
    };
    let mut shallow: Vec<String> = std::fs::read_to_string(git_dir.join("shallow"))?
        .lines()
        .map(str::to_owned)
        .collect();
    shallow.sort();
    Ok((all, head, shallow))
}

fn clone_and_re_shallow(
    root: &Path,
    bare: &Path,
    name: &str,
    upload_pack: &str,
) -> Result<(usize, usize, Vec<String>), Box<dyn Error>> {
    let dest = root.join(name);
    let url = format!("file://{}", bare.canonicalize()?.display());
    let upload = format!("--upload-pack={upload_pack}");
    git(
        root,
        &[
            "-c",
            "protocol.version=2",
            "-c",
            "maintenance.auto=false",
            "clone",
            "-q",
            "--depth",
            "1",
            &upload,
            url.as_str(),
            dest.to_str().ok_or("utf8")?,
        ],
    )?;
    git(
        &dest,
        &[
            "-c",
            "maintenance.auto=false",
            "fetch",
            "-q",
            "--depth",
            "1",
            "origin",
            "+refs/heads/*:refs/remotes/origin/*",
        ],
    )?;
    shallow_summary(&dest)
}

#[test]
fn grit_matches_git_on_re_shallow_all_heads() -> TestResult {
    let root = std::env::temp_dir().join(format!("grit-re-shallow-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root)?;
    let bare = issue_topology_bare(&root)?;

    let git_up = "git-upload-pack";
    let grit_up = format!("{GRIT} upload-pack");

    let git_stats = clone_and_re_shallow(&root, &bare, "git-clone", git_up)?;
    let grit_stats = clone_and_re_shallow(&root, &bare, "grit-clone", &grit_up)?;

    assert_eq!(git_stats, grit_stats, "grit must match git upload-pack");
    assert_eq!(git_stats.0, 3, "rev-list --all");
    assert_eq!(git_stats.1, 1, "rev-list HEAD");
    assert_eq!(git_stats.2.len(), 3, ".git/shallow lines");

    Ok(())
}

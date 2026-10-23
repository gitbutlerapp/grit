//! Git parity for `git blame -w` / ignore-all-space line comparison.

use std::path::Path;
use std::process::Command;

use grit_lib::blame::{blame_file, BlameOptions};
use grit_lib::config::environment::{Environment, RepositoryOptions};
use grit_lib::diff::WhitespaceMode;
use grit_lib::objects::ObjectId;
use grit_lib::repo::Repository;

fn git_env(cmd: &mut Command) {
    cmd.env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_AUTHOR_NAME", "Blame WS Test")
        .env("GIT_AUTHOR_EMAIL", "blame-ws@example.com")
        .env("GIT_COMMITTER_NAME", "Blame WS Test")
        .env("GIT_COMMITTER_EMAIL", "blame-ws@example.com");
}

fn git_run(dir: &Path, args: &[&str]) {
    let mut cmd = Command::new("git");
    cmd.current_dir(dir).args(args);
    git_env(&mut cmd);
    let out = cmd.output().expect("spawn git");
    assert!(
        out.status.success(),
        "git {:?} failed: {}",
        args,
        String::from_utf8_lossy(&out.stderr)
    );
}

fn git_output(dir: &Path, args: &[&str]) -> String {
    let mut cmd = Command::new("git");
    cmd.current_dir(dir).args(args);
    git_env(&mut cmd);
    let out = cmd.output().expect("spawn git");
    assert!(
        out.status.success(),
        "git {:?} failed: {}",
        args,
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).to_string()
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct PorcelainLine {
    final_line: usize,
    commit: ObjectId,
    original_line: usize,
}

fn parse_git_blame_porcelain(output: &str) -> Vec<PorcelainLine> {
    let mut lines = Vec::new();
    let mut group: Option<(ObjectId, usize, usize)> = None;
    for raw in output.lines() {
        if raw.starts_with('\t') {
            let Some((commit, orig, final_line)) = group else {
                continue;
            };
            lines.push(PorcelainLine {
                final_line,
                commit,
                original_line: orig,
            });
            group = Some((commit, orig + 1, final_line + 1));
            continue;
        }
        let mut parts = raw.split_whitespace();
        let Some(sha) = parts.next() else {
            continue;
        };
        if !matches!(sha.len(), 40 | 64) || !sha.chars().all(|c| c.is_ascii_hexdigit()) {
            continue;
        }
        let Ok(commit) = sha.parse::<ObjectId>() else {
            continue;
        };
        let Some(orig_s) = parts.next() else {
            continue;
        };
        let Some(final_s) = parts.next() else {
            continue;
        };
        let Some(_count_s) = parts.next() else {
            continue;
        };
        let Ok(orig) = orig_s.parse::<usize>() else {
            continue;
        };
        let Ok(final_line) = final_s.parse::<usize>() else {
            continue;
        };
        group = Some((commit, orig, final_line));
    }
    lines.sort_by_key(|l| l.final_line);
    lines
}

fn git_blame_w(dir: &Path, file: &str) -> Vec<PorcelainLine> {
    let out = git_output(dir, &["blame", "--porcelain", "-w", "--", file]);
    parse_git_blame_porcelain(&out)
}

fn grit_blame_w(repo: &Repository, file: &str) -> Vec<PorcelainLine> {
    let result = blame_file(
        repo,
        file,
        &BlameOptions {
            whitespace: WhitespaceMode::ignore_all_space(),
            ..Default::default()
        },
    )
    .expect("blame_file");
    let mut out: Vec<PorcelainLine> = result
        .lines
        .into_iter()
        .map(|line| PorcelainLine {
            final_line: line.final_line,
            commit: line.commit,
            original_line: line.original_line,
        })
        .collect();
    out.sort_by_key(|l| l.final_line);
    out
}

fn assert_blame_matches(repo: &Repository, dir: &Path, file: &str) {
    let git_lines = git_blame_w(dir, file);
    let grit_lines = grit_blame_w(repo, file);
    assert_eq!(git_lines.len(), grit_lines.len(), "line count");
    for (g, r) in git_lines.iter().zip(grit_lines.iter()) {
        assert_eq!(g.final_line, r.final_line, "final line");
        assert_eq!(g.commit, r.commit, "commit at line {}", g.final_line);
        assert_eq!(g.original_line, r.original_line, "original line");
    }
}

fn open_repo(dir: &Path) -> Repository {
    Repository::open(
        &dir.join(".git"),
        &RepositoryOptions::empty().with_work_tree(dir),
    )
    .expect("open repo")
}

#[test]
fn ignore_whitespace_reindent_commit() {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path();
    git_run(root, &["init"]);
    std::fs::write(root.join("f.txt"), "alpha\nbeta\n").unwrap();
    git_run(root, &["add", "f.txt"]);
    git_run(root, &["commit", "-m", "initial"]);
    std::fs::write(root.join("f.txt"), "  alpha\n    beta\n").unwrap();
    git_run(root, &["add", "f.txt"]);
    git_run(root, &["commit", "-m", "reindent"]);
    let repo = open_repo(root);
    assert_blame_matches(&repo, root, "f.txt");
}

#[test]
fn ignore_whitespace_trailing_spaces() {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path();
    git_run(root, &["init"]);
    std::fs::write(root.join("f.txt"), "keep\n").unwrap();
    git_run(root, &["add", "f.txt"]);
    git_run(root, &["commit", "-m", "initial"]);
    std::fs::write(root.join("f.txt"), "keep   \n").unwrap();
    git_run(root, &["add", "f.txt"]);
    git_run(root, &["commit", "-m", "trail ws"]);
    let repo = open_repo(root);
    assert_blame_matches(&repo, root, "f.txt");
}

#[test]
fn ignore_whitespace_tab_space_swap() {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path();
    git_run(root, &["init"]);
    std::fs::write(root.join("f.txt"), "one\ttwo\n").unwrap();
    git_run(root, &["add", "f.txt"]);
    git_run(root, &["commit", "-m", "initial"]);
    std::fs::write(root.join("f.txt"), "one two\n").unwrap();
    git_run(root, &["add", "f.txt"]);
    git_run(root, &["commit", "-m", "spaces"]);
    let repo = open_repo(root);
    assert_blame_matches(&repo, root, "f.txt");
}

#[test]
fn ignore_whitespace_crlf_file() {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path();
    git_run(root, &["init"]);
    std::fs::write(root.join("f.txt"), "line\r\n").unwrap();
    git_run(root, &["add", "f.txt"]);
    git_run(root, &["commit", "-m", "initial"]);
    std::fs::write(root.join("f.txt"), "line  \r\n").unwrap();
    git_run(root, &["add", "f.txt"]);
    git_run(root, &["commit", "-m", "crlf trail"]);
    let repo = open_repo(root);
    assert_blame_matches(&repo, root, "f.txt");
}

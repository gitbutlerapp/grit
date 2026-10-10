//! Compare `blame_file` attributions with `git blame --porcelain`.

use std::path::Path;
use std::process::Command;

use grit_lib::blame::{blame_file, BlameOptions};
use grit_lib::objects::ObjectId;
use grit_lib::repo::Repository;

fn git_env(cmd: &mut Command) {
    cmd.env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_AUTHOR_NAME", "Blame Test")
        .env("GIT_AUTHOR_EMAIL", "blame@example.com")
        .env("GIT_COMMITTER_NAME", "Blame Test")
        .env("GIT_COMMITTER_EMAIL", "blame@example.com");
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
        if sha.len() != 40 || !sha.chars().all(|c| c.is_ascii_hexdigit()) {
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

fn git_blame(
    dir: &Path,
    file: &str,
    rev: Option<&str>,
    line_range: Option<(usize, usize)>,
) -> Vec<PorcelainLine> {
    let range_spec = line_range.map(|(start, end)| format!("{start},{end}"));
    let mut args: Vec<&str> = vec!["blame", "--porcelain"];
    if let Some(spec) = &range_spec {
        args.push("-L");
        args.push(spec);
    }
    if let Some(rev) = rev {
        args.push(rev);
    }
    args.push("--");
    args.push(file);
    let out = git_output(dir, &args);
    parse_git_blame_porcelain(&out)
}

fn grit_blame(
    repo: &Repository,
    file: &str,
    start: Option<ObjectId>,
    line_range: Option<std::ops::RangeInclusive<usize>>,
) -> Vec<PorcelainLine> {
    let result = blame_file(repo, file, &BlameOptions { start, line_range }).expect("blame_file");
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

fn assert_blame_matches(
    repo: &Repository,
    dir: &Path,
    file: &str,
    rev: Option<&str>,
    range: Option<(usize, usize)>,
) {
    let start = rev.map(|spec| {
        let hex = git_output(dir, &["rev-parse", spec]);
        hex.trim().parse::<ObjectId>().expect("rev-parse oid")
    });
    let range_inclusive = range.map(|(a, b)| a..=b);
    let git_lines = git_blame(dir, file, rev, range);
    let grit_lines = grit_blame(repo, file, start, range_inclusive);
    assert_eq!(
        git_lines.len(),
        grit_lines.len(),
        "line count mismatch for rev={rev:?} range={range:?}"
    );
    for (g, r) in git_lines.iter().zip(grit_lines.iter()) {
        assert_eq!(g.final_line, r.final_line, "final line");
        assert_eq!(g.commit, r.commit, "commit oid at line {}", g.final_line);
        assert_eq!(
            g.original_line, r.original_line,
            "original line at final {}",
            g.final_line
        );
    }
}

fn build_blame_fixture() -> (tempfile::TempDir, Repository, String) {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path().to_path_buf();
    git_run(&root, &["init", "-q", "-b", "main"]);
    git_run(&root, &["config", "user.name", "Blame Test"]);
    git_run(&root, &["config", "user.email", "blame@example.com"]);

    let file = "story.txt";
    std::fs::write(root.join(file), "alpha one\nbeta two\n gamma three\n").unwrap();
    git_run(&root, &["add", file]);
    git_run(&root, &["commit", "-m", "init"]);

    std::fs::write(root.join(file), "alpha ONE\nbeta two\n gamma three\n").unwrap();
    git_run(&root, &["add", file]);
    git_run(&root, &["commit", "-m", "edit first line"]);

    std::fs::write(
        root.join(file),
        "alpha ONE\ninserted\nbeta two\n gamma three\n",
    )
    .unwrap();
    git_run(&root, &["add", file]);
    git_run(&root, &["commit", "-m", "insert line"]);

    std::fs::write(root.join(file), "alpha ONE\ninserted\n gamma three\n").unwrap();
    git_run(&root, &["add", file]);
    git_run(&root, &["commit", "-m", "delete beta"]);

    std::fs::write(root.join(file), "alpha ONE\n gamma three\ninserted\n").unwrap();
    git_run(&root, &["add", file]);
    git_run(&root, &["commit", "-m", "move inserted block"]);

    let mid = git_output(&root, &["rev-parse", "HEAD~2"])
        .trim()
        .to_owned();
    let repo = Repository::discover(Some(&root)).expect("open grit repo");
    (tmp, repo, mid)
}

#[test]
fn blame_matches_git_porcelain_after_rotate_line_to_head() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();
    git_run(root, &["init", "-q", "-b", "main"]);
    git_run(root, &["config", "user.name", "Blame Test"]);
    git_run(root, &["config", "user.email", "blame@example.com"]);

    let file = "f.txt";
    std::fs::write(root.join(file), "a\nb\nc\nd\n").unwrap();
    git_run(root, &["add", file]);
    git_run(root, &["commit", "-m", "init"]);

    std::fs::write(root.join(file), "d\na\nb\nc\n").unwrap();
    git_run(root, &["add", file]);
    git_run(root, &["commit", "-m", "move"]);

    let repo = Repository::discover(Some(root)).expect("open");
    let dir = repo.work_tree.as_deref().expect("worktree");
    assert_blame_matches(&repo, dir, file, None, None);
}

#[test]
fn blame_matches_git_porcelain_at_head_rev_and_line_range() {
    let (_tmp, repo, mid_rev) = build_blame_fixture();
    let dir = repo.work_tree.as_deref().expect("worktree");
    let file = "story.txt";

    assert_blame_matches(&repo, dir, file, None, None);
    assert_blame_matches(&repo, dir, file, Some(&mid_rev), None);
    assert_blame_matches(&repo, dir, file, None, Some((2, 3)));
}

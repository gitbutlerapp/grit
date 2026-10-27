//! Compare `BlameResult::hunks()` and `original_path` with `git blame --line-porcelain`.

use std::path::Path;
use std::process::Command;

use grit_lib::blame::{blame_file, BlameHunk, BlameOptions};
use grit_lib::config::environment::RepositoryOptions;
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
struct PorcelainHunk {
    commit: ObjectId,
    original_start: usize,
    final_start: usize,
    count: usize,
    original_path: String,
}

fn flush_porcelain_hunk(
    hunks: &mut Vec<PorcelainHunk>,
    pending: &mut Option<(ObjectId, usize, usize, usize)>,
    path: &str,
) {
    let Some((commit, orig, final_start, count)) = pending.take() else {
        return;
    };
    hunks.push(PorcelainHunk {
        commit,
        original_start: orig,
        final_start,
        count,
        original_path: path.to_string(),
    });
}

fn parse_git_blame_porcelain_hunks(output: &str) -> Vec<PorcelainHunk> {
    let mut hunks = Vec::new();
    let mut pending: Option<(ObjectId, usize, usize, usize)> = None;
    for raw in output.lines() {
        if raw.starts_with('\t') {
            continue;
        }
        if let Some(path) = raw.strip_prefix("filename ") {
            flush_porcelain_hunk(&mut hunks, &mut pending, path);
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
        let Ok(orig) = orig_s.parse::<usize>() else {
            continue;
        };
        let Ok(final_start) = final_s.parse::<usize>() else {
            continue;
        };
        let Some(count_s) = parts.next() else {
            // Same hunk as the previous group header (`oid orig final` without count).
            continue;
        };
        let Ok(count) = count_s.parse::<usize>() else {
            continue;
        };
        pending = Some((commit, orig, final_start, count));
    }
    hunks
}

fn git_blame_hunks(
    dir: &Path,
    file: &str,
    rev: Option<&str>,
    line_range: Option<(usize, usize)>,
) -> Vec<PorcelainHunk> {
    let range_spec = line_range.map(|(start, end)| format!("{start},{end}"));
    let mut args: Vec<&str> = vec!["blame", "--line-porcelain"];
    if let Some(spec) = &range_spec {
        args.push("-L");
        args.push(spec);
    }
    if let Some(rev) = rev {
        args.push(rev);
    }
    args.push("--");
    args.push(file);
    parse_git_blame_porcelain_hunks(&git_output(dir, &args))
}

fn grit_blame_hunks(
    repo: &Repository,
    file: &str,
    start: Option<ObjectId>,
    line_range: Option<std::ops::RangeInclusive<usize>>,
) -> Vec<BlameHunk> {
    let result = blame_file(
        repo,
        file,
        &BlameOptions {
            start,
            line_range,
            ..Default::default()
        },
    )
    .expect("blame_file");
    result.hunks()
}

fn assert_hunks_match(
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
    let git_hunks = git_blame_hunks(dir, file, rev, range);
    let grit_hunks = grit_blame_hunks(repo, file, start, range_inclusive);
    assert_eq!(
        git_hunks.len(),
        grit_hunks.len(),
        "hunk count mismatch for rev={rev:?} range={range:?}"
    );
    for (g, r) in git_hunks.iter().zip(grit_hunks.iter()) {
        assert_eq!(g.commit, r.commit, "commit oid");
        assert_eq!(g.original_start, r.original_start, "original_start");
        assert_eq!(g.final_start, r.final_start, "final_start");
        assert_eq!(g.count, r.count, "count at final {}", g.final_start);
        assert_eq!(
            g.original_path, r.original_path,
            "original_path at final {}",
            g.final_start
        );
    }
}

#[test]
fn blame_hunks_match_git_after_rename() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();
    git_run(root, &["init", "-q", "-b", "main"]);
    git_run(root, &["config", "user.name", "Blame Test"]);
    git_run(root, &["config", "user.email", "blame@example.com"]);

    std::fs::write(root.join("before.txt"), "alpha\nbeta\ngamma\n").unwrap();
    git_run(root, &["add", "before.txt"]);
    git_run(root, &["commit", "-m", "add before.txt"]);

    git_run(root, &["mv", "before.txt", "after.txt"]);
    git_run(root, &["add", "-A"]);
    git_run(root, &["commit", "-m", "rename to after.txt"]);

    let repo = Repository::discover(Some(root), &RepositoryOptions::empty()).expect("open");
    let dir = repo.work_tree().expect("worktree");
    assert_hunks_match(&repo, dir, "after.txt", None, None);

    let result = blame_file(&repo, "after.txt", &BlameOptions::default()).expect("blame");
    assert_eq!(result.lines[0].original_path, "before.txt");
    let porcelain = git_output(dir, &["blame", "--porcelain", "after.txt"]);
    assert!(porcelain.contains("filename before.txt"));
}

#[test]
fn blame_hunks_match_git_on_merge_fixture() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();
    git_run(root, &["init", "-q", "-b", "main"]);
    git_run(root, &["config", "user.name", "Blame Test"]);
    git_run(root, &["config", "user.email", "blame@example.com"]);

    std::fs::write(root.join("start.txt"), "alpha\nbeta\ngamma\n").unwrap();
    git_run(root, &["add", "start.txt"]);
    git_run(root, &["commit", "-m", "add start.txt"]);

    git_run(root, &["switch", "-c", "side"]);
    std::fs::create_dir_all(root.join("side")).unwrap();
    std::fs::write(root.join("side/branch.txt"), "alpha\nbeta\ngamma\n").unwrap();
    git_run(root, &["add", "side/branch.txt"]);
    git_run(root, &["commit", "-m", "side copy"]);

    git_run(root, &["switch", "main"]);
    std::fs::create_dir_all(root.join("live")).unwrap();
    git_run(root, &["mv", "start.txt", "live/result.txt"]);
    git_run(root, &["add", "-A"]);
    git_run(root, &["commit", "-m", "rename to live/result.txt"]);
    git_run(root, &["merge", "--no-ff", "side", "-m", "merge side"]);

    let repo = Repository::discover(Some(root), &RepositoryOptions::empty()).expect("open");
    let dir = repo.work_tree().expect("worktree");
    assert_hunks_match(&repo, dir, "live/result.txt", None, None);
}

#[test]
fn blame_hunks_match_git_on_line_range() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();
    git_run(root, &["init", "-q", "-b", "main"]);
    git_run(root, &["config", "user.name", "Blame Test"]);
    git_run(root, &["config", "user.email", "blame@example.com"]);

    let mut body = String::new();
    for i in 1..=20 {
        body.push_str(&format!("line {i}\n"));
    }
    std::fs::write(root.join("wide.txt"), &body).unwrap();
    git_run(root, &["add", "wide.txt"]);
    git_run(root, &["commit", "-m", "init wide"]);

    std::fs::write(
        root.join("wide.txt"),
        body.replace("line 10\n", "LINE TEN\n"),
    )
    .unwrap();
    git_run(root, &["add", "wide.txt"]);
    git_run(root, &["commit", "-m", "edit line 10"]);

    let repo = Repository::discover(Some(root), &RepositoryOptions::empty()).expect("open");
    let dir = repo.work_tree().expect("worktree");
    assert_hunks_match(&repo, dir, "wide.txt", None, Some((8, 12)));
}

/// Selected unchanged edges cross a rename-and-edit after an exact subtree move.
#[test]
fn blame_hunks_match_git_across_rename_edit_and_subtree_move() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();
    git_run(root, &["init", "-q", "-b", "main"]);
    let initial: String = (1..=12).map(|i| format!("original line {i}\n")).collect();
    std::fs::write(root.join("original"), &initial).unwrap();
    git_run(root, &["add", "."]);
    git_run(root, &["commit", "-qm", "initial lines"]);
    let edited = initial
        .replace("original line 2\n", "second revision line 2\n")
        .replace("original line 11\n", "second revision line 11\n");
    std::fs::write(root.join("original"), &edited).unwrap();
    git_run(root, &["commit", "-qam", "edit prefix and suffix"]);
    git_run(root, &["mv", "original", "renamed"]);
    std::fs::write(
        root.join("renamed"),
        edited.replace("original line 6\n", "changed middle\ninserted middle\n"),
    )
    .unwrap();
    git_run(root, &["add", "-A"]);
    git_run(root, &["commit", "-qm", "rename and edit middle"]);
    std::fs::create_dir(root.join("vendor")).unwrap();
    git_run(root, &["mv", "renamed", "vendor/final"]);
    git_run(root, &["commit", "-qm", "move unchanged file into subtree"]);

    let repo = Repository::discover(Some(root), &RepositoryOptions::empty()).expect("open");
    for range in [Some((1, 3)), Some((11, 13)), None, Some((1, 13))] {
        assert_hunks_match(&repo, root, "vendor/final", None, range);
        let result = blame_file(
            &repo,
            "vendor/final",
            &BlameOptions {
                line_range: range.map(|(a, b)| a..=b),
                ..Default::default()
            },
        )
        .unwrap();
        // Commit, path and line must navigate to the attributed source, even after
        // the suffix shifted by one line in the rename-and-edit commit.
        for line in &result.lines {
            let source = git_output(
                root,
                &["show", &format!("{}:{}", line.commit, line.original_path)],
            );
            assert_eq!(
                source.lines().nth(line.original_line - 1),
                Some(line.content.as_str())
            );
        }
        let commits: std::collections::HashSet<_> = result.lines.iter().map(|l| l.commit).collect();
        assert!(
            commits.len() >= 2,
            "selected lines must have distinct origins"
        );
    }
    git_run(root, &["fsck", "--strict"]);
}

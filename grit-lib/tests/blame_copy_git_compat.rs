//! Compare copy detection with `git blame --porcelain -C` levels.

use std::path::Path;
use std::process::Command;

use grit_lib::blame::{blame_file, BlameOptions, CopyDetection};
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
struct PorcelainLine {
    final_line: usize,
    commit: ObjectId,
    original_line: usize,
    filename: String,
}

fn parse_git_blame_porcelain(output: &str, blame_path: &str) -> Vec<PorcelainLine> {
    let mut lines = Vec::new();
    let mut group: Option<(ObjectId, usize, usize)> = None;
    let mut group_filename = blame_path.to_owned();
    for raw in output.lines() {
        if raw.starts_with('\t') {
            let Some((commit, orig, final_line)) = group else {
                continue;
            };
            lines.push(PorcelainLine {
                final_line,
                commit,
                original_line: orig,
                filename: group_filename.clone(),
            });
            group = Some((commit, orig + 1, final_line + 1));
            continue;
        }
        if let Some(rest) = raw.strip_prefix("filename ") {
            group_filename = rest.to_owned();
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
        group_filename = blame_path.to_owned();
    }
    lines.sort_by_key(|l| l.final_line);
    lines
}

fn git_copy_args(level: CopyDetection) -> Vec<&'static str> {
    match level {
        CopyDetection::Off => Vec::new(),
        CopyDetection::FromModifiedFiles => vec!["-C"],
        CopyDetection::FromCreatingCommit => vec!["-C", "-C"],
        CopyDetection::FromAnyCommit => vec!["-C", "-C", "-C"],
    }
}

fn git_blame_copy(
    dir: &Path,
    file: &str,
    level: CopyDetection,
    line_range: Option<(usize, usize)>,
) -> Vec<PorcelainLine> {
    let mut args: Vec<String> = vec!["blame".into(), "--porcelain".into()];
    args.extend(git_copy_args(level).into_iter().map(str::to_owned));
    if let Some((start, end)) = line_range {
        args.push("-L".into());
        args.push(format!("{start},{end}"));
    }
    args.push("--".into());
    args.push(file.into());
    let arg_refs: Vec<&str> = args.iter().map(String::as_str).collect();
    let out = git_output(dir, &arg_refs);
    parse_git_blame_porcelain(&out, file)
}

fn grit_blame_copy(
    repo: &Repository,
    file: &str,
    level: CopyDetection,
    line_range: Option<std::ops::RangeInclusive<usize>>,
) -> Vec<PorcelainLine> {
    let result = blame_file(
        repo,
        file,
        &BlameOptions {
            line_range,
            copy_detection: level,
            ..Default::default()
        },
    )
    .expect("blame_file");
    let mut out = Vec::new();
    for line in result.lines {
        out.push(PorcelainLine {
            final_line: line.final_line,
            commit: line.commit,
            original_line: line.original_line,
            filename: line.original_path.clone(),
        });
    }
    out.sort_by_key(|l| l.final_line);
    out
}

fn assert_blame_copy_matches(
    repo: &Repository,
    dir: &Path,
    file: &str,
    level: CopyDetection,
    range: Option<(usize, usize)>,
) {
    let range_inclusive = range.map(|(a, b)| a..=b);
    let git_lines = git_blame_copy(dir, file, level, range);
    let grit_lines = grit_blame_copy(repo, file, level, range_inclusive);
    assert_eq!(
        git_lines.len(),
        grit_lines.len(),
        "line count mismatch for {file} level={level:?} range={range:?}"
    );
    for (g, r) in git_lines.iter().zip(grit_lines.iter()) {
        assert_eq!(g.final_line, r.final_line, "final line");
        assert_eq!(g.commit, r.commit, "commit at line {}", g.final_line);
        assert_eq!(
            g.original_line, r.original_line,
            "original line at final {}",
            g.final_line
        );
        assert_eq!(g.filename, r.filename, "filename at final {}", g.final_line);
    }
}

fn open_repo(root: &Path) -> Repository {
    Repository::discover(Some(root), &RepositoryOptions::empty()).expect("open repo")
}

fn copy_score_line() -> String {
    "A".repeat(69)
}

fn fixture_same_commit_modified_copy() -> (tempfile::TempDir, Repository) {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path().to_path_buf();
    let long = copy_score_line();
    git_run(&root, &["init", "-q", "-b", "main"]);
    std::fs::write(root.join("source.txt"), format!("{long}\n")).unwrap();
    git_run(&root, &["add", "source.txt"]);
    git_run(&root, &["commit", "-m", "init"]);
    std::fs::write(
        root.join("source.txt"),
        format!("modified prefix\n{long}\n"),
    )
    .unwrap();
    std::fs::write(root.join("dest.txt"), format!("{long}\n")).unwrap();
    git_run(&root, &["add", "source.txt", "dest.txt"]);
    git_run(&root, &["commit", "-m", "modify and copy"]);
    (tmp, open_repo(&root))
}

fn fixture_created_by_copy() -> (tempfile::TempDir, Repository) {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path().to_path_buf();
    let long = copy_score_line();
    git_run(&root, &["init", "-q", "-b", "main"]);
    std::fs::write(root.join("one"), format!("{long}\n")).unwrap();
    git_run(&root, &["add", "one"]);
    git_run(&root, &["commit", "-m", "Initial"]);
    std::fs::write(root.join("uno"), format!("{long}\n")).unwrap();
    git_run(&root, &["add", "uno"]);
    git_run(&root, &["commit", "-m", "Second"]);
    (tmp, open_repo(&root))
}

fn fixture_copy_from_older_commit() -> (tempfile::TempDir, Repository) {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path().to_path_buf();
    let long = copy_score_line();
    git_run(&root, &["init", "-q", "-b", "main"]);
    std::fs::write(root.join("one"), format!("{long}\n")).unwrap();
    std::fs::write(root.join("tres"), format!("{long}\n")).unwrap();
    git_run(&root, &["add", "one", "tres"]);
    git_run(&root, &["commit", "-m", "Initial"]);
    std::fs::write(root.join("uno"), format!("{long}\n")).unwrap();
    std::fs::write(root.join("tres"), format!("{long}\n{long}\n")).unwrap();
    git_run(&root, &["add", "uno", "tres"]);
    git_run(&root, &["commit", "-m", "Second"]);
    (tmp, open_repo(&root))
}

/// Issue #2239: short lines still score as a chunk (Git `blame_entry_score`).
fn fixture_short_lines_moved_between_files() -> (tempfile::TempDir, Repository) {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path().to_path_buf();
    git_run(&root, &["init", "-q", "-b", "main"]);
    let a_content: String = (1..=20).map(|n| format!("orig line {n}\n")).collect();
    std::fs::write(root.join("a.txt"), a_content).unwrap();
    git_run(&root, &["add", "."]);
    git_run(&root, &["commit", "-m", "add a"]);
    let a_tail: String = (11..=20).map(|n| format!("orig line {n}\n")).collect();
    std::fs::write(root.join("a.txt"), a_tail).unwrap();
    let b_content: String = (1..=10)
        .map(|n| format!("orig line {n}\n"))
        .chain(std::iter::once("extra\n".to_owned()))
        .collect();
    std::fs::write(root.join("b.txt"), b_content).unwrap();
    git_run(&root, &["add", "."]);
    git_run(&root, &["commit", "-m", "move lines from a to b"]);
    (tmp, open_repo(&root))
}

fn fixture_short_lines_whole_file_copy() -> (tempfile::TempDir, Repository) {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path().to_path_buf();
    git_run(&root, &["init", "-q", "-b", "main"]);
    let a_content: String = (1..=20).map(|n| format!("orig line {n}\n")).collect();
    std::fs::write(root.join("a.txt"), a_content).unwrap();
    git_run(&root, &["add", "."]);
    git_run(&root, &["commit", "-m", "add a"]);
    std::fs::write(root.join("o.txt"), "other\n").unwrap();
    git_run(&root, &["add", "o.txt"]);
    git_run(&root, &["commit", "-m", "other"]);
    std::fs::copy(root.join("a.txt"), root.join("c.txt")).unwrap();
    git_run(&root, &["add", "c.txt"]);
    git_run(&root, &["commit", "-m", "copy a to c"]);
    (tmp, open_repo(&root))
}

fn fixture_exact_rename_preserves_original_path() -> (tempfile::TempDir, Repository) {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path().to_path_buf();
    git_run(&root, &["init", "-q", "-b", "main"]);
    std::fs::write(root.join("source.txt"), "hello\n").unwrap();
    git_run(&root, &["add", "source.txt"]);
    git_run(&root, &["commit", "-m", "init"]);
    git_run(&root, &["rm", "source.txt"]);
    std::fs::write(root.join("dest.txt"), "hello\n").unwrap();
    git_run(&root, &["add", "dest.txt"]);
    git_run(&root, &["commit", "-m", "rename"]);
    (tmp, open_repo(&root))
}

fn fixture_two_hop_exact_rename_preserves_original_path() -> (tempfile::TempDir, Repository) {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path().to_path_buf();
    git_run(&root, &["init", "-q", "-b", "main"]);
    std::fs::write(root.join("source.txt"), "hello\n").unwrap();
    git_run(&root, &["add", "source.txt"]);
    git_run(&root, &["commit", "-m", "init"]);
    git_run(&root, &["rm", "source.txt"]);
    std::fs::write(root.join("middle.txt"), "hello\n").unwrap();
    git_run(&root, &["add", "middle.txt"]);
    git_run(&root, &["commit", "-m", "rename to middle"]);
    git_run(&root, &["rm", "middle.txt"]);
    std::fs::write(root.join("dest.txt"), "hello\n").unwrap();
    git_run(&root, &["add", "dest.txt"]);
    git_run(&root, &["commit", "-m", "rename to dest"]);
    (tmp, open_repo(&root))
}

#[test]
fn short_lines_moved_between_files_level1() {
    let (tmp, repo) = fixture_short_lines_moved_between_files();
    assert_blame_copy_matches(
        &repo,
        tmp.path(),
        "b.txt",
        CopyDetection::FromModifiedFiles,
        Some((1, 10)),
    );
}

#[test]
fn short_lines_whole_file_copy_level2() {
    let (tmp, repo) = fixture_short_lines_whole_file_copy();
    assert_blame_copy_matches(
        &repo,
        tmp.path(),
        "c.txt",
        CopyDetection::FromCreatingCommit,
        Some((1, 5)),
    );
}

#[test]
fn short_lines_whole_file_copy_level3() {
    let (tmp, repo) = fixture_short_lines_whole_file_copy();
    assert_blame_copy_matches(
        &repo,
        tmp.path(),
        "c.txt",
        CopyDetection::FromAnyCommit,
        Some((1, 5)),
    );
}

#[test]
fn copy_from_modified_file_in_same_commit_level1() {
    let (tmp, repo) = fixture_same_commit_modified_copy();
    assert_blame_copy_matches(
        &repo,
        tmp.path(),
        "dest.txt",
        CopyDetection::FromModifiedFiles,
        None,
    );
}

#[test]
fn copy_when_file_created_by_copy_level2() {
    let (tmp, repo) = fixture_created_by_copy();
    assert_blame_copy_matches(
        &repo,
        tmp.path(),
        "uno",
        CopyDetection::FromCreatingCommit,
        None,
    );
}

#[test]
fn copy_from_older_commit_level3() {
    let (tmp, repo) = fixture_copy_from_older_commit();
    assert_blame_copy_matches(
        &repo,
        tmp.path(),
        "tres",
        CopyDetection::FromAnyCommit,
        Some((1, 1)),
    );
}

#[test]
fn exact_rename_reports_original_path_like_git() {
    let (tmp, repo) = fixture_exact_rename_preserves_original_path();
    assert_blame_copy_matches(&repo, tmp.path(), "dest.txt", CopyDetection::Off, None);
}

#[test]
fn two_hop_exact_rename_reports_original_path_like_git() {
    let (tmp, repo) = fixture_two_hop_exact_rename_preserves_original_path();
    assert_blame_copy_matches(&repo, tmp.path(), "dest.txt", CopyDetection::Off, None);
}

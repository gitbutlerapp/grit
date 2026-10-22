//! Compare `blame_file` attributions with `git blame --porcelain`.

use std::path::Path;
use std::process::Command;

use grit_lib::blame::{blame_file, BlameOptions};
use grit_lib::config::environment::{Environment, RepositoryOptions};
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
    let repo =
        Repository::discover(Some(&root), &RepositoryOptions::empty()).expect("open grit repo");
    (tmp, repo, mid)
}

#[test]
fn blame_from_subdirectory_cwd_matches_git() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();
    git_run(root, &["init", "-q", "-b", "main"]);
    git_run(root, &["config", "user.name", "Blame Test"]);
    git_run(root, &["config", "user.email", "blame@example.com"]);

    std::fs::create_dir_all(root.join("sub")).unwrap();
    std::fs::write(root.join("sub/f.txt"), "line one\n").unwrap();
    git_run(root, &["add", "-A"]);
    git_run(root, &["commit", "-m", "add sub file"]);
    std::fs::write(root.join("sub/f.txt"), "line two\n").unwrap();
    git_run(root, &["commit", "-am", "edit sub file"]);

    let sub = root.join("sub");
    let mut env = Environment::empty();
    env.cwd = sub.clone();
    let mut opts = RepositoryOptions::empty();
    opts.environment = env;
    let repo = Repository::discover(Some(root), &opts).expect("open grit repo from subdir cwd");

    assert_blame_matches(&repo, &sub, "f.txt", None, None);
}

#[test]
fn blame_merge_commit_matches_git_porcelain() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();
    git_run(root, &["init", "-q", "-b", "main"]);
    git_run(root, &["config", "user.name", "Blame Test"]);
    git_run(root, &["config", "user.email", "blame@example.com"]);

    let file = "merge.txt";
    std::fs::write(root.join(file), "base line\nshared\n").unwrap();
    git_run(root, &["add", file]);
    git_run(root, &["commit", "-m", "base"]);

    git_run(root, &["branch", "side"]);
    git_run(root, &["checkout", "side"]);
    std::fs::write(root.join("side-only.txt"), "side file\n").unwrap();
    git_run(root, &["add", "side-only.txt"]);
    git_run(root, &["commit", "-m", "side file"]);

    git_run(root, &["checkout", "main"]);
    std::fs::write(root.join(file), "base line\nshared\nmain tail\n").unwrap();
    git_run(root, &["add", file]);
    git_run(root, &["commit", "-m", "main tail"]);

    git_run(root, &["merge", "--no-ff", "side", "-m", "merge side"]);

    let repo = Repository::discover(Some(root), &RepositoryOptions::empty()).expect("open");
    let dir = repo.work_tree().expect("worktree");
    assert_blame_matches(&repo, dir, file, None, None);
    assert_blame_matches(&repo, dir, file, Some("HEAD"), Some((1, 3)));
    assert_blame_matches(&repo, dir, "side-only.txt", None, None);
}

#[test]
fn blame_merge_side_only_file_matches_git() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();
    git_run(root, &["init", "-q", "-b", "main"]);
    git_run(root, &["config", "user.name", "Blame Test"]);
    git_run(root, &["config", "user.email", "blame@example.com"]);

    std::fs::write(root.join("base.txt"), "base\n").unwrap();
    git_run(root, &["add", "."]);
    git_run(root, &["commit", "-m", "base"]);

    git_run(root, &["switch", "-c", "side"]);
    std::fs::write(root.join("new.txt"), "one\ntwo\nthree\n").unwrap();
    git_run(root, &["add", "new.txt"]);
    git_run(root, &["commit", "-m", "side adds new.txt"]);

    git_run(root, &["switch", "main"]);
    std::fs::write(root.join("base.txt"), "base\nmore\n").unwrap();
    git_run(root, &["commit", "-am", "main change"]);

    git_run(root, &["merge", "-m", "merge side", "side"]);

    let repo = Repository::discover(Some(root), &RepositoryOptions::empty()).expect("open");
    let dir = repo.work_tree().expect("worktree");
    assert_blame_matches(&repo, dir, "new.txt", None, None);
}

#[test]
fn blame_merge_line_from_second_parent_matches_git_default() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();
    git_run(root, &["init", "-q", "-b", "main"]);
    git_run(root, &["config", "user.name", "Blame Test"]);
    git_run(root, &["config", "user.email", "blame@example.com"]);

    let file = "ten.txt";
    let mut base = String::new();
    for i in 1..=9 {
        base.push_str(&format!("line {i}\n"));
    }
    base.push_str("line ten on main\n");
    std::fs::write(root.join(file), &base).unwrap();
    git_run(root, &["add", file]);
    git_run(root, &["commit", "-m", "base ten lines"]);

    git_run(root, &["branch", "side"]);
    git_run(root, &["checkout", "side"]);
    let mut side_content = base.clone();
    side_content = side_content.replace("line ten on main\n", "line ten from side branch\n");
    std::fs::write(root.join(file), &side_content).unwrap();
    git_run(root, &["add", file]);
    git_run(root, &["commit", "-m", "side edits line ten"]);
    let side_commit = git_output(root, &["rev-parse", "HEAD"])
        .trim()
        .parse::<ObjectId>()
        .expect("side oid");

    git_run(root, &["checkout", "main"]);
    git_run(
        root,
        &["merge", "--no-ff", "side", "-m", "merge side line ten"],
    );

    let repo = Repository::discover(Some(root), &RepositoryOptions::empty()).expect("open");
    let dir = repo.work_tree().expect("worktree");
    let git_lines = git_blame(dir, file, None, Some((10, 10)));
    assert_eq!(git_lines.len(), 1);
    assert_eq!(
        git_lines[0].commit, side_commit,
        "git should attribute line 10 to the side-branch commit"
    );

    let grit_lines = grit_blame(&repo, file, None, Some(10..=10));
    assert_eq!(grit_lines.len(), 1);
    assert_eq!(
        grit_lines[0].commit, side_commit,
        "default grit blame should match git (second parent), not the merge commit"
    );
    assert_eq!(grit_lines[0].commit, git_lines[0].commit);
}

/// Merge result shorter than a parent: mapped parent line index can exceed the result length.
#[test]
fn blame_merge_maps_result_line_to_parent_line_beyond_result_length() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();
    git_run(root, &["init", "-q", "-b", "main"]);
    git_run(root, &["config", "user.name", "Blame Test"]);
    git_run(root, &["config", "user.email", "blame@example.com"]);

    let file = "sizes.txt";
    let mut main_body = String::new();
    for i in 1..=40 {
        main_body.push_str(&format!("line {i}\n"));
    }
    std::fs::write(root.join(file), &main_body).unwrap();
    git_run(root, &["add", file]);
    git_run(root, &["commit", "-m", "main forty lines"]);

    git_run(root, &["branch", "wide"]);
    git_run(root, &["checkout", "wide"]);
    let mut wide_body = String::new();
    for i in 1..=100 {
        if i == 80 {
            wide_body.push_str("B80\n");
        } else {
            wide_body.push_str(&format!("wide {i}\n"));
        }
    }
    std::fs::write(root.join(file), &wide_body).unwrap();
    git_run(root, &["add", file]);
    git_run(root, &["commit", "-m", "wide parent with B80 at line 80"]);
    let wide_commit = git_output(root, &["rev-parse", "HEAD"])
        .trim()
        .parse::<ObjectId>()
        .expect("wide oid");

    git_run(root, &["checkout", "main"]);
    let mut merge_result = main_body.clone();
    merge_result = merge_result.replace("line 20\n", "B80\n");
    git_run(root, &["merge", "--no-ff", "--no-commit", "wide"]);
    std::fs::write(root.join(file), &merge_result).unwrap();
    git_run(root, &["add", file]);
    git_run(root, &["commit", "-m", "merge: B80 on result line 20"]);

    let repo = Repository::discover(Some(root), &RepositoryOptions::empty()).expect("open");
    let dir = repo.work_tree().expect("worktree");
    let git_lines = git_blame(dir, file, None, Some((20, 20)));
    assert_eq!(git_lines.len(), 1);
    assert_eq!(
        git_lines[0].original_line, 80,
        "git should map result line 20 to parent line 80"
    );
    assert_eq!(
        git_lines[0].commit, wide_commit,
        "git should attribute to the wide (second) parent"
    );

    let grit_lines = grit_blame(&repo, file, None, Some(20..=20));
    assert_eq!(grit_lines.len(), 1);
    assert_eq!(
        grit_lines[0].original_line, git_lines[0].original_line,
        "grit original line should match git"
    );
    assert_eq!(
        grit_lines[0].commit, git_lines[0].commit,
        "grit should attribute merge line 20 to the wide parent, not the merge commit"
    );
}

/// Main renames the blamed file while the merge side keeps the same bytes at another path.
#[test]
fn blame_merge_and_rename_parent_path_matches_git() {
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
    git_run(root, &["commit", "-m", "side copy at side/branch.txt"]);

    git_run(root, &["switch", "main"]);
    std::fs::create_dir_all(root.join("live")).unwrap();
    git_run(root, &["mv", "start.txt", "live/result.txt"]);
    git_run(root, &["add", "-A"]);
    git_run(root, &["commit", "-m", "rename to live/result.txt"]);

    git_run(root, &["merge", "--no-ff", "side", "-m", "merge side path"]);

    let repo = Repository::discover(Some(root), &RepositoryOptions::empty()).expect("open");
    let dir = repo.work_tree().expect("worktree");
    assert_blame_matches(&repo, dir, "live/result.txt", None, None);
}

/// After a rename, Bloom skips must query the renamed path (not a stale pathspec).
#[test]
fn blame_bloom_skip_after_rename_matches_git() {
    use grit_lib::maintenance::{write_commit_graph, CommitGraphOptions};

    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();
    git_run(root, &["init", "-q", "-b", "main"]);
    git_run(root, &["config", "user.name", "Blame Test"]);
    git_run(root, &["config", "user.email", "blame@example.com"]);

    std::fs::write(root.join("before.txt"), "stable\n").unwrap();
    git_run(root, &["add", "before.txt"]);
    git_run(root, &["commit", "-m", "add before.txt"]);

    git_run(root, &["mv", "before.txt", "after.txt"]);
    git_run(root, &["add", "-A"]);
    git_run(root, &["commit", "-m", "rename to after.txt"]);

    for n in 0..12 {
        git_run(
            root,
            &["commit", "--allow-empty", "-m", &format!("noop {n}")],
        );
    }

    let repo = Repository::discover(Some(root), &RepositoryOptions::empty()).expect("open");
    write_commit_graph(&repo, &CommitGraphOptions::with_changed_paths(true)).expect("graph");
    let dir = repo.work_tree().expect("worktree");
    assert_blame_matches(&repo, dir, "after.txt", None, None);
}

/// `pass_whole_blame` may target a parent with the same blob even if a sibling lacks the path.
#[test]
fn blame_pass_whole_when_sibling_parent_lacks_path_matches_git() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();
    git_run(root, &["init", "-q", "-b", "main"]);
    git_run(root, &["config", "user.name", "Blame Test"]);
    git_run(root, &["config", "user.email", "blame@example.com"]);

    std::fs::write(root.join("shared.txt"), "payload\n").unwrap();
    git_run(root, &["add", "shared.txt"]);
    git_run(root, &["commit", "-m", "base"]);

    git_run(root, &["branch", "keep"]);
    git_run(root, &["branch", "drop"]);
    git_run(root, &["checkout", "drop"]);
    git_run(root, &["rm", "shared.txt"]);
    git_run(root, &["commit", "-m", "drop shared.txt"]);

    git_run(root, &["checkout", "keep"]);
    git_run(
        root,
        &["merge", "-s", "ours", "drop", "-m", "merge keep tree"],
    );

    let repo = Repository::discover(Some(root), &RepositoryOptions::empty()).expect("open");
    let dir = repo.work_tree().expect("worktree");
    assert_blame_matches(&repo, dir, "shared.txt", None, None);
}

/// Pad a short reorder fixture so the file exceeds internal heuristics thresholds.
fn write_padded_reorder_fixture(root: &Path, file: &str, body: &str) {
    let mut lines: Vec<String> = body.lines().map(|line| format!("{line}\n")).collect();
    while lines.len() < 65 {
        lines.insert(0, format!("pad {}\n", lines.len()));
    }
    std::fs::write(root.join(file), lines.join("")).unwrap();
}

#[test]
fn blame_matches_git_porcelain_after_adcb_permutation() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();
    git_run(root, &["init", "-q", "-b", "main"]);
    git_run(root, &["config", "user.name", "Blame Test"]);
    git_run(root, &["config", "user.email", "blame@example.com"]);

    let file = "f.txt";
    std::fs::write(root.join(file), "a\nb\nc\nd\n").unwrap();
    git_run(root, &["add", file]);
    git_run(root, &["commit", "-m", "init"]);

    std::fs::write(root.join(file), "a\nd\nc\nb\n").unwrap();
    git_run(root, &["add", file]);
    git_run(root, &["commit", "-m", "move"]);

    let repo = Repository::discover(Some(root), &RepositoryOptions::empty()).expect("open");
    let dir = repo.work_tree().expect("worktree");
    assert_blame_matches(&repo, dir, file, None, None);
}

#[test]
fn blame_matches_git_adcb_permutation_on_padded_file() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();
    git_run(root, &["init", "-q", "-b", "main"]);
    git_run(root, &["config", "user.name", "Blame Test"]);
    git_run(root, &["config", "user.email", "blame@example.com"]);

    let file = "f.txt";
    write_padded_reorder_fixture(root, file, "a\nb\nc\nd\n");
    git_run(root, &["add", file]);
    git_run(root, &["commit", "-m", "init"]);

    write_padded_reorder_fixture(root, file, "a\nd\nc\nb\n");
    git_run(root, &["add", file]);
    git_run(root, &["commit", "-m", "move"]);

    let repo = Repository::discover(Some(root), &RepositoryOptions::empty()).expect("open");
    let dir = repo.work_tree().expect("worktree");
    assert_blame_matches(&repo, dir, file, None, None);
}

#[test]
fn blame_matches_git_adcb_permutation_line_range() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();
    git_run(root, &["init", "-q", "-b", "main"]);
    git_run(root, &["config", "user.name", "Blame Test"]);
    git_run(root, &["config", "user.email", "blame@example.com"]);

    let file = "f.txt";
    std::fs::write(root.join(file), "a\nb\nc\nd\n").unwrap();
    git_run(root, &["add", file]);
    git_run(root, &["commit", "-m", "init"]);

    std::fs::write(root.join(file), "a\nd\nc\nb\n").unwrap();
    git_run(root, &["add", file]);
    git_run(root, &["commit", "-m", "move"]);

    let repo = Repository::discover(Some(root), &RepositoryOptions::empty()).expect("open");
    let dir = repo.work_tree().expect("worktree");
    assert_blame_matches(&repo, dir, file, None, Some((3, 3)));
}

#[test]
fn blame_matches_git_for_all_four_line_permutations() {
    let base = ["a", "b", "c", "d"];
    for perm in [
        ["b", "a", "c", "d"],
        ["b", "a", "d", "c"],
        ["b", "c", "a", "d"],
        ["b", "c", "d", "a"],
        ["b", "d", "a", "c"],
        ["b", "d", "c", "a"],
        ["c", "a", "b", "d"],
        ["c", "a", "d", "b"],
        ["c", "b", "a", "d"],
        ["c", "b", "d", "a"],
        ["c", "d", "a", "b"],
        ["c", "d", "b", "a"],
        ["d", "a", "b", "c"],
        ["d", "a", "c", "b"],
        ["d", "b", "a", "c"],
        ["d", "b", "c", "a"],
        ["d", "c", "a", "b"],
        ["d", "c", "b", "a"],
        ["a", "b", "d", "c"],
        ["a", "c", "b", "d"],
        ["a", "c", "d", "b"],
        ["a", "d", "b", "c"],
        ["a", "d", "c", "b"],
    ] {
        let tmp = tempfile::tempdir().expect("tempdir");
        let root = tmp.path();
        git_run(root, &["init", "-q", "-b", "main"]);
        git_run(root, &["config", "user.name", "Blame Test"]);
        git_run(root, &["config", "user.email", "blame@example.com"]);

        let file = "f.txt";
        std::fs::write(root.join(file), format!("{}\n", base.join("\n"))).unwrap();
        git_run(root, &["add", file]);
        git_run(root, &["commit", "-m", "init"]);

        std::fs::write(root.join(file), format!("{}\n", perm.join("\n"))).unwrap();
        git_run(root, &["add", file]);
        git_run(root, &["commit", "-m", "move"]);

        let repo = Repository::discover(Some(root), &RepositoryOptions::empty()).expect("open");
        let dir = repo.work_tree().expect("worktree");
        assert_blame_matches(&repo, dir, file, None, None);
    }
}

#[test]
fn blame_matches_git_porcelain_after_dacb_style_permutation() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();
    git_run(root, &["init", "-q", "-b", "main"]);
    git_run(root, &["config", "user.name", "Blame Test"]);
    git_run(root, &["config", "user.email", "blame@example.com"]);

    let file = "f.txt";
    std::fs::write(root.join(file), "a\nb\nc\nd\n").unwrap();
    git_run(root, &["add", file]);
    git_run(root, &["commit", "-m", "init"]);

    std::fs::write(root.join(file), "d\na\nc\nb\n").unwrap();
    git_run(root, &["add", file]);
    git_run(root, &["commit", "-m", "move"]);

    let repo = Repository::discover(Some(root), &RepositoryOptions::empty()).expect("open");
    let dir = repo.work_tree().expect("worktree");
    assert_blame_matches(&repo, dir, file, None, None);
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

    let repo = Repository::discover(Some(root), &RepositoryOptions::empty()).expect("open");
    let dir = repo.work_tree().expect("worktree");
    assert_blame_matches(&repo, dir, file, None, None);
}

#[test]
fn blame_follows_rename_chain_like_git() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();
    git_run(root, &["init", "-q", "-b", "main"]);
    git_run(root, &["config", "user.name", "Blame Test"]);
    git_run(root, &["config", "user.email", "blame@example.com"]);

    let mut lines: Vec<String> = (1..=20).map(|i| format!("line {i}")).collect();
    std::fs::write(root.join("a.txt"), format!("{}\n", lines.join("\n"))).unwrap();
    git_run(root, &["add", "a.txt"]);
    git_run(root, &["commit", "-m", "add a"]);

    git_run(root, &["mv", "a.txt", "b.txt"]);
    git_run(root, &["commit", "-m", "rename a to b"]);

    lines[4] = "line five".to_owned();
    std::fs::write(root.join("b.txt"), format!("{}\n", lines.join("\n"))).unwrap();
    git_run(root, &["add", "b.txt"]);
    git_run(root, &["commit", "-m", "edit line 5"]);

    std::fs::create_dir(root.join("dir")).unwrap();
    git_run(root, &["mv", "b.txt", "dir/c.txt"]);
    let body = std::fs::read_to_string(root.join("dir/c.txt")).unwrap();
    std::fs::write(root.join("dir/c.txt"), format!("{body}extra\n")).unwrap();
    git_run(root, &["add", "-A"]);
    git_run(root, &["commit", "-m", "move c into dir + extra"]);

    let repo = Repository::discover(Some(root), &RepositoryOptions::empty()).expect("open");
    let dir = repo.work_tree().expect("worktree");
    assert_blame_matches(&repo, dir, "dir/c.txt", None, None);
}

#[test]
fn blame_follows_rename_when_diff_renames_false() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();
    git_run(root, &["init", "-q", "-b", "main"]);
    git_run(root, &["config", "user.name", "Blame Test"]);
    git_run(root, &["config", "user.email", "blame@example.com"]);
    git_run(root, &["config", "diff.renames", "false"]);

    let lines: Vec<String> = (1..=20).map(|i| format!("line {i}")).collect();
    std::fs::write(root.join("a.txt"), format!("{}\n", lines.join("\n"))).unwrap();
    git_run(root, &["add", "a.txt"]);
    git_run(root, &["commit", "-m", "add a"]);

    let mut edited = lines.clone();
    edited[4] = "line five".to_owned();
    std::fs::write(root.join("a.txt"), format!("{}\n", edited.join("\n"))).unwrap();
    git_run(root, &["mv", "a.txt", "b.txt"]);
    git_run(root, &["add", "b.txt"]);
    git_run(root, &["commit", "-m", "rename and edit line 5"]);

    let repo = Repository::discover(Some(root), &RepositoryOptions::empty()).expect("open");
    let dir = repo.work_tree().expect("worktree");
    assert_blame_matches(&repo, dir, "b.txt", None, None);
}

#[test]
fn blame_follows_rename_with_diff_rename_limit_200() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();
    git_run(root, &["init", "-q", "-b", "main"]);
    git_run(root, &["config", "user.name", "Blame Test"]);
    git_run(root, &["config", "user.email", "blame@example.com"]);
    git_run(root, &["config", "diff.renameLimit", "200"]);

    let lines: Vec<String> = (1..=20).map(|i| format!("line {i}")).collect();
    std::fs::write(root.join("a.txt"), format!("{}\n", lines.join("\n"))).unwrap();
    git_run(root, &["add", "a.txt"]);
    git_run(root, &["commit", "-m", "add a"]);

    let mut edited = lines.clone();
    edited[4] = "line five".to_owned();
    std::fs::write(root.join("a.txt"), format!("{}\n", edited.join("\n"))).unwrap();
    git_run(root, &["mv", "a.txt", "b.txt"]);
    git_run(root, &["add", "b.txt"]);
    git_run(root, &["commit", "-m", "rename and edit line 5"]);

    let repo = Repository::discover(Some(root), &RepositoryOptions::empty()).expect("open");
    let dir = repo.work_tree().expect("worktree");
    assert_blame_matches(&repo, dir, "b.txt", None, None);
}

#[test]
fn blame_matches_git_porcelain_at_head_rev_and_line_range() {
    let (_tmp, repo, mid_rev) = build_blame_fixture();
    let dir = repo.work_tree().expect("worktree");
    let file = "story.txt";

    assert_blame_matches(&repo, dir, file, None, None);
    assert_blame_matches(&repo, dir, file, Some(&mid_rev), None);
    assert_blame_matches(&repo, dir, file, None, Some((2, 3)));
}

#[test]
fn blame_interior_blank_lines_match_git() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();
    git_run(root, &["init", "-q", "-b", "main"]);
    git_run(root, &["config", "user.name", "Blame Test"]);
    git_run(root, &["config", "user.email", "blame@example.com"]);

    let file = "blanks.txt";
    std::fs::write(root.join(file), "alpha\n\nbeta\n\ngamma\n").expect("write");
    git_run(root, &["add", file]);
    git_run(root, &["commit", "-m", "init with interior blank lines"]);

    std::fs::write(root.join(file), "alpha\n\nbeta changed\n\ngamma\n").expect("write");
    git_run(root, &["add", file]);
    git_run(root, &["commit", "-m", "edit beta"]);

    let repo = Repository::discover(Some(root), &RepositoryOptions::empty()).expect("open");
    assert_blame_matches(&repo, root, file, None, None);
}

#[test]
fn blame_consecutive_and_trailing_blank_lines_match_git() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();
    git_run(root, &["init", "-q", "-b", "main"]);
    git_run(root, &["config", "user.name", "Blame Test"]);
    git_run(root, &["config", "user.email", "blame@example.com"]);

    let file = "blanks2.txt";
    std::fs::write(root.join(file), "alpha\n\n\nbeta\n\n").expect("write");
    git_run(root, &["add", file]);
    git_run(root, &["commit", "-m", "consecutive and trailing blanks"]);

    std::fs::write(root.join(file), "alpha\n\n\nbeta edited\n\n").expect("write");
    git_run(root, &["add", file]);
    git_run(root, &["commit", "-m", "edit beta with blanks preserved"]);

    let repo = Repository::discover(Some(root), &RepositoryOptions::empty()).expect("open");
    assert_blame_matches(&repo, root, file, None, None);
}

/// Bloom must still skip when a sibling file in the same directory changed.
#[test]
fn blame_bloom_skip_when_sibling_in_directory_changed_matches_git() {
    use grit_lib::maintenance::{write_commit_graph, CommitGraphOptions};

    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();
    git_run(root, &["init", "-q", "-b", "main"]);
    git_run(root, &["config", "user.name", "Blame Test"]);
    git_run(root, &["config", "user.email", "blame@example.com"]);

    std::fs::create_dir_all(root.join("builtin")).expect("dir");
    std::fs::write(root.join("builtin/target.c"), "stable line\n").expect("write");
    std::fs::write(root.join("builtin/sibling.c"), "other\n").expect("write");
    git_run(root, &["add", "builtin"]);
    git_run(root, &["commit", "-m", "add sibling files"]);

    std::fs::write(root.join("builtin/sibling.c"), "other changed\n").expect("write");
    git_run(root, &["add", "builtin/sibling.c"]);
    git_run(root, &["commit", "-m", "touch sibling only"]);

    for n in 0..10 {
        git_run(
            root,
            &["commit", "--allow-empty", "-m", &format!("noop {n}")],
        );
    }

    let repo = Repository::discover(Some(root), &RepositoryOptions::empty()).expect("open");
    write_commit_graph(&repo, &CommitGraphOptions::with_changed_paths(true)).expect("graph");
    assert_blame_matches(&repo, root, "builtin/target.c", None, None);
}

#[test]
fn blame_respects_graft_over_commit_graph_bloom() {
    use grit_lib::maintenance::{write_commit_graph, CommitGraphOptions};

    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();
    git_run(root, &["init", "-q", "-b", "main"]);
    git_run(root, &["config", "user.name", "Blame Test"]);
    git_run(root, &["config", "user.email", "blame@example.com"]);

    std::fs::write(root.join("f.txt"), "same\n").unwrap();
    git_run(root, &["add", "f.txt"]);
    git_run(root, &["commit", "-m", "root"]);

    git_run(root, &["commit", "--allow-empty", "-m", "child same tree"]);
    let child = git_output(root, &["rev-parse", "HEAD"]).trim().to_string();

    let root_rev = git_output(root, &["rev-parse", "HEAD~1"])
        .trim()
        .to_string();
    git_run(root, &["checkout", "-b", "alt", &root_rev]);
    std::fs::write(root.join("f.txt"), "other\n").unwrap();
    git_run(root, &["add", "f.txt"]);
    git_run(root, &["commit", "-m", "different at graft parent"]);
    let graft_parent = git_output(root, &["rev-parse", "HEAD"]).trim().to_string();

    git_run(root, &["checkout", "main"]);
    std::fs::create_dir_all(root.join(".git/info")).expect("info dir");
    std::fs::write(
        root.join(".git/info/grafts"),
        format!("{child} {graft_parent}\n"),
    )
    .expect("graft");

    let repo = Repository::discover(Some(root), &RepositoryOptions::empty()).expect("open");
    write_commit_graph(&repo, &CommitGraphOptions::with_changed_paths(true)).expect("graph");

    assert_blame_matches(&repo, root, "f.txt", Some(&child), None);
}

#[test]
fn blame_annotated_tag_start_matches_git_porcelain() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();
    git_run(root, &["init", "-q", "-b", "main"]);
    git_run(root, &["config", "user.name", "Blame Test"]);
    git_run(root, &["config", "user.email", "blame@example.com"]);

    let file = "a.txt";
    std::fs::write(root.join(file), "hello\n").unwrap();
    git_run(root, &["add", file]);
    git_run(root, &["commit", "-m", "one"]);
    git_run(root, &["tag", "-a", "v1.1", "-m", "release"]);
    git_run(root, &["tag", "v1"]);

    let repo = Repository::discover(Some(root), &RepositoryOptions::empty()).expect("open");
    assert_blame_matches(&repo, root, file, Some("v1.1"), None);
    assert_blame_matches(&repo, root, file, Some("v1"), None);
}

fn commit_identity_and_summary(dir: &Path, commit: &ObjectId) -> (String, String) {
    let raw = git_output(dir, &["cat-file", "commit", &commit.to_hex()]);
    let (headers, message) = raw.split_once("\n\n").unwrap_or((raw.as_str(), ""));
    let author = headers
        .lines()
        .find_map(|line| line.strip_prefix("author "))
        .expect("author header")
        .trim()
        .to_owned();
    let summary = message
        .lines()
        .find(|line| !line.trim().is_empty())
        .unwrap_or("")
        .trim()
        .to_owned();
    (author, summary)
}

/// Every attributed commit carries the same author line and summary that `git` stores.
fn assert_commit_metadata_matches(repo: &Repository, dir: &Path, file: &str) {
    let result = blame_file(repo, file, &BlameOptions::default()).expect("blame_file");
    assert!(!result.lines.is_empty());
    for line in &result.lines {
        let info = result
            .commits
            .get(&line.commit)
            .expect("metadata for every blamed commit");
        let (author, summary) = commit_identity_and_summary(dir, &line.commit);
        assert_eq!(info.author, author, "author of {}", line.commit);
        assert_eq!(info.summary, summary, "summary of {}", line.commit);
    }
}

/// History where the blamed file sits two directories deep, siblings at every level churn,
/// a side branch edits the file, and an octopus merge has more parents than fit in a
/// fixed-size commit header read.
fn build_packed_walk_fixture(object_format: &str) -> tempfile::TempDir {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();
    git_run(
        root,
        &[
            "init",
            "-q",
            "-b",
            "main",
            &format!("--object-format={object_format}"),
        ],
    );
    git_run(root, &["config", "user.name", "Blame Test"]);
    git_run(root, &["config", "user.email", "blame@example.com"]);

    let file = "dir/sub/notes.txt";
    std::fs::create_dir_all(root.join("dir/sub")).unwrap();
    std::fs::write(root.join(file), "one\ntwo\nthree\nfour\nfive\n").unwrap();
    std::fs::write(root.join("top.txt"), "0\n").unwrap();
    git_run(root, &["add", "-A"]);
    git_run(root, &["commit", "-q", "-m", "init"]);

    for i in 0..12 {
        std::fs::write(root.join("top.txt"), format!("{i}\n")).unwrap();
        std::fs::write(root.join("dir/sibling.txt"), format!("{i}\n")).unwrap();
        std::fs::write(root.join("dir/sub/peer.txt"), format!("{i}\n")).unwrap();
        git_run(root, &["add", "-A"]);
        let message = format!("churn {i}\n\n{}", "long body line\n".repeat(64));
        git_run(root, &["commit", "-q", "-m", &message]);
    }

    std::fs::write(root.join(file), "one\nTWO\nthree\nfour\nfive\n").unwrap();
    git_run(root, &["commit", "-q", "-am", "edit two on main"]);

    git_run(root, &["checkout", "-q", "-b", "side", "HEAD~3"]);
    std::fs::write(root.join(file), "one\ntwo\nthree\nFOUR\nfive\n").unwrap();
    git_run(root, &["commit", "-q", "-am", "edit four on side"]);
    git_run(root, &["checkout", "-q", "main"]);
    git_run(root, &["merge", "-q", "--no-edit", "side"]);

    let mut branches = Vec::new();
    for i in 0..11 {
        let name = format!("octo{i}");
        git_run(root, &["checkout", "-q", "-b", &name, "main"]);
        std::fs::write(root.join(format!("octo{i}.txt")), format!("{i}\n")).unwrap();
        git_run(root, &["add", "-A"]);
        git_run(root, &["commit", "-q", "-m", &format!("octo {i}")]);
        branches.push(name);
    }
    git_run(root, &["checkout", "-q", "main"]);
    let mut merge_args = vec!["merge", "-q", "--no-ff", "--no-edit"];
    merge_args.extend(branches.iter().map(String::as_str));
    git_run(root, &merge_args);

    for i in 0..6 {
        std::fs::write(root.join("dir/sibling.txt"), format!("after {i}\n")).unwrap();
        git_run(
            root,
            &["commit", "-q", "-am", &format!("after octopus {i}")],
        );
    }
    std::fs::write(root.join(file), "zero\none\nTWO\nthree\nFOUR\nfive\n").unwrap();
    git_run(root, &["commit", "-q", "-am", "prepend zero"]);
    tmp
}

fn assert_packed_walk_matches_git(object_format: &str) {
    let tmp = build_packed_walk_fixture(object_format);
    let root = tmp.path();
    let file = "dir/sub/notes.txt";
    let parents = git_output(root, &["rev-list", "--parents", "-n", "1", "HEAD~7"]);
    assert_eq!(
        parents.split_whitespace().count(),
        13,
        "fixture needs a 12-parent octopus merge"
    );

    let loose_repo = Repository::discover(Some(root), &RepositoryOptions::empty()).expect("open");
    let loose = grit_blame(&loose_repo, file, None, None);
    assert_blame_matches(&loose_repo, root, file, None, None);
    drop(loose_repo);

    git_run(root, &["repack", "-adq", "--depth=50", "--window=50"]);
    git_run(root, &["prune-packed"]);
    let packs = std::fs::read_dir(root.join(".git/objects/pack"))
        .unwrap()
        .filter_map(|e| e.ok())
        .filter(|e| e.path().extension().is_some_and(|x| x == "pack"))
        .count();
    assert_eq!(packs, 1, "single pack after repack");

    let repo = Repository::discover(Some(root), &RepositoryOptions::empty()).expect("open");
    assert_blame_matches(&repo, root, file, None, None);
    assert_eq!(grit_blame(&repo, file, None, None), loose);
    assert_blame_matches(&repo, root, file, Some("HEAD~1"), None);
    assert_blame_matches(&repo, root, file, Some("HEAD"), Some((2, 4)));
    assert_blame_matches(&repo, root, "dir/sub/peer.txt", None, None);
    assert_blame_matches(&repo, root, "top.txt", None, None);
    assert_commit_metadata_matches(&repo, root, file);
}

#[test]
fn blame_packed_deep_path_through_octopus_matches_git() {
    assert_packed_walk_matches_git("sha1");
}

#[test]
fn blame_packed_sha256_history_matches_git() {
    assert_packed_walk_matches_git("sha256");
}

#[test]
fn blame_reports_git_commit_metadata_for_loose_history() {
    let (tmp, repo, _mid) = build_blame_fixture();
    assert_commit_metadata_matches(&repo, tmp.path(), "story.txt");
}

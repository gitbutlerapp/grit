//! Compare `blame_file` attributions with `git blame --line-porcelain`.

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
    original_path: String,
}

fn parse_git_blame_porcelain(output: &str) -> Vec<PorcelainLine> {
    let mut lines = Vec::new();
    let mut group: Option<(ObjectId, usize, usize)> = None;
    let mut filename = String::new();
    for raw in output.lines() {
        if let Some(path) = raw.strip_prefix("filename ") {
            filename = path.to_owned();
            continue;
        }
        if raw.starts_with('\t') {
            let Some((commit, orig, final_line)) = group.take() else {
                continue;
            };
            lines.push(PorcelainLine {
                final_line,
                commit,
                original_line: orig,
                original_path: filename.clone(),
            });
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
            original_path: line.original_path,
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
    let mut source_line_counts = std::collections::HashMap::new();
    for (g, r) in git_lines.iter().zip(grit_lines.iter()) {
        assert_eq!(g.final_line, r.final_line, "final line");
        assert_eq!(g.commit, r.commit, "commit oid at line {}", g.final_line);
        assert_eq!(
            g.original_line, r.original_line,
            "original line at final {}",
            g.final_line
        );
        assert_eq!(
            g.original_path, r.original_path,
            "original path at final {}",
            g.final_line
        );
        let source_line_count = source_line_counts
            .entry((r.commit, &r.original_path))
            .or_insert_with(|| {
                git_output(dir, &["show", &format!("{}:{}", r.commit, r.original_path)])
                    .lines()
                    .count()
            });
        assert!(
            (1..=*source_line_count).contains(&r.original_line),
            "original line must exist in {}:{}",
            r.commit,
            r.original_path
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

/// Equal-length middle edit: insert at the low end and delete at the high end of the changed
/// run (beyond the old 48-line window padding). `-L` must still match git.
#[test]
fn blame_matches_git_equal_length_shift_beyond_pad_line_range() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();
    git_run(root, &["init", "-q", "-b", "main"]);
    git_run(root, &["config", "user.name", "Blame Test"]);
    git_run(root, &["config", "user.email", "blame@example.com"]);

    let file = "f.txt";
    let mut old_tail = vec![
        "lineA\n".to_string(),
        "lineB\n".to_string(),
        "lineC\n".to_string(),
        "lineD\n".to_string(),
        "lineE\n".to_string(),
    ];
    let mut lines: Vec<String> = (0..60).map(|i| format!("pad {i}\n")).collect();
    lines.append(&mut old_tail);
    std::fs::write(root.join(file), lines.join("")).unwrap();
    git_run(root, &["add", file]);
    git_run(root, &["commit", "-m", "init"]);

    let mut new_tail = vec![
        "lineX\n".to_string(),
        "lineB\n".to_string(),
        "lineC\n".to_string(),
        "lineD\n".to_string(),
        "lineY\n".to_string(),
    ];
    let mut lines: Vec<String> = (0..60).map(|i| format!("pad {i}\n")).collect();
    lines.append(&mut new_tail);
    std::fs::write(root.join(file), lines.join("")).unwrap();
    git_run(root, &["add", file]);
    git_run(root, &["commit", "-m", "shift"]);

    let repo = Repository::discover(Some(root), &RepositoryOptions::empty()).expect("open");
    let dir = repo.work_tree().expect("worktree");
    // Final line 62 is "lineB" (1-based); focus stays inside the edited tail, far from pads.
    assert_blame_matches(&repo, dir, file, None, Some((62, 62)));
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

/// Per-commit `(author, author-mail, author-time, summary)` from `git blame --porcelain`.
fn git_porcelain_metadata(
    dir: &Path,
    file: &str,
) -> std::collections::HashMap<ObjectId, (String, String, i64, String)> {
    let out = git_output(dir, &["blame", "--porcelain", "--", file]);
    let mut map = std::collections::HashMap::new();
    let mut current: Option<ObjectId> = None;
    let mut fields: std::collections::HashMap<&str, String> = std::collections::HashMap::new();
    for line in out.lines() {
        if line.starts_with('\t') {
            if let Some(oid) = current.take() {
                if !fields.is_empty() {
                    map.insert(
                        oid,
                        (
                            fields.get("author").cloned().unwrap_or_default(),
                            fields.get("author-mail").cloned().unwrap_or_default(),
                            fields
                                .get("author-time")
                                .and_then(|t| t.parse().ok())
                                .unwrap_or(-1),
                            fields.get("summary").cloned().unwrap_or_default(),
                        ),
                    );
                }
            }
            fields.clear();
            continue;
        }
        let (key, value) = line.split_once(' ').unwrap_or((line, ""));
        if key.len() == 40 && key.chars().all(|c| c.is_ascii_hexdigit()) {
            current = key.parse().ok();
            continue;
        }
        fields.insert(
            match key {
                "author" => "author",
                "author-mail" => "author-mail",
                "author-time" => "author-time",
                "summary" => "summary",
                _ => continue,
            },
            value.to_owned(),
        );
    }
    map
}

/// Grit's metadata rendered the way `git blame --porcelain` reports it.
fn grit_metadata_as_porcelain(
    info: &grit_lib::blame::BlameCommitInfo,
) -> (String, String, i64, String) {
    let author = info.author.as_str();
    let (name, mail) = match (author.find('<'), author.find('>')) {
        (Some(lt), Some(gt)) if lt < gt => {
            (author[..lt].trim().to_owned(), author[lt..=gt].to_owned())
        }
        _ => ("(unknown)".to_owned(), "(unknown)".to_owned()),
    };
    (name, mail, info.author_time, info.summary.clone())
}

fn git_hash_commit_literally(dir: &Path, body: &str) -> String {
    use std::io::Write as _;
    let mut cmd = Command::new("git");
    cmd.current_dir(dir)
        .args([
            "hash-object",
            "-t",
            "commit",
            "-w",
            "--literally",
            "--stdin",
        ])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped());
    git_env(&mut cmd);
    let mut child = cmd.spawn().expect("spawn git hash-object");
    child
        .stdin
        .take()
        .expect("stdin")
        .write_all(body.as_bytes())
        .expect("write commit body");
    let out = child.wait_with_output().expect("git hash-object");
    assert!(out.status.success(), "git hash-object --literally failed");
    String::from_utf8(out.stdout)
        .expect("utf-8")
        .trim()
        .to_owned()
}

/// `f.txt` gains one line in a commit without `committer` and one in a commit without
/// `author`; `g.txt` is untouched since the root, so its walk passes through both.
fn build_malformed_commit_fixture() -> tempfile::TempDir {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();
    git_run(root, &["init", "-q", "-b", "main"]);
    git_run(root, &["config", "user.name", "Blame Test"]);
    git_run(root, &["config", "user.email", "blame@example.com"]);
    std::fs::write(root.join("f.txt"), "hello\n").unwrap();
    std::fs::write(root.join("g.txt"), "steady\n").unwrap();
    git_run(root, &["add", "-A"]);
    git_run(root, &["commit", "-q", "-m", "base"]);
    let base = git_output(root, &["rev-parse", "HEAD"]).trim().to_owned();

    std::fs::write(root.join("f.txt"), "hello\nworld\n").unwrap();
    git_run(root, &["add", "f.txt"]);
    let tree1 = git_output(root, &["write-tree"]).trim().to_owned();
    let no_committer = git_hash_commit_literally(
        root,
        &format!("tree {tree1}\nparent {base}\nauthor A <a@x> 1700000000 +0000\n\nmessage\n"),
    );

    std::fs::write(root.join("f.txt"), "hello\nworld\nagain\n").unwrap();
    git_run(root, &["add", "f.txt"]);
    let tree2 = git_output(root, &["write-tree"]).trim().to_owned();
    let no_author = git_hash_commit_literally(
        root,
        &format!(
            "tree {tree2}\nparent {no_committer}\ncommitter C <c@x> 1700000100 +0000\n\n\nno author subject\nbody\n"
        ),
    );
    git_run(root, &["update-ref", "refs/heads/main", &no_author]);
    git_run(root, &["reset", "-q", "--hard"]);
    std::fs::write(root.join("h.txt"), "later\n").unwrap();
    git_run(root, &["add", "h.txt"]);
    git_run(root, &["commit", "-q", "-m", "well-formed tip"]);
    tmp
}

fn git_git_worktree() -> Option<std::path::PathBuf> {
    if let Ok(p) = std::env::var("GRIT_GIT_GIT_PATH") {
        let path = std::path::PathBuf::from(p);
        if path.is_dir() {
            return Some(path);
        }
    }
    let default = std::path::PathBuf::from("/tmp/gitgit");
    default.is_dir().then_some(default)
}

fn git_git_has_full_history(root: &Path) -> bool {
    let mut cmd = Command::new("git");
    cmd.current_dir(root)
        .args(["cat-file", "-e", "ae12e59a8c:builtin-add.c"]);
    git_env(&mut cmd);
    cmd.output().map(|o| o.status.success()).unwrap_or(false)
}

/// Dogfood fixture: `builtin/add.c` at HEAD and historical revs (#1610).
#[test]
fn blame_git_git_builtin_add_c_matches_git_porcelain() {
    let Some(root) = git_git_worktree() else {
        eprintln!("SKIP: set GRIT_GIT_GIT_PATH or clone git/git to /tmp/gitgit");
        return;
    };
    if !git_git_has_full_history(&root) {
        eprintln!("SKIP: git/git worktree lacks full history for blame walk");
        return;
    }
    let repo = Repository::discover(Some(&root), &RepositoryOptions::empty()).expect("open");
    assert_blame_matches(&repo, &root, "builtin/add.c", None, Some((6, 8)));
    assert_blame_matches(&repo, &root, "builtin/add.c", None, Some((47, 47)));
    assert_blame_matches(&repo, &root, "builtin-add.c", Some("ae12e59a8c"), None);
}

#[test]
fn blame_through_commits_missing_author_or_committer_matches_git_loose_and_packed() {
    let tmp = build_malformed_commit_fixture();
    let root = tmp.path();
    let blame_all = |repo: &Repository| {
        ["f.txt", "g.txt"]
            .map(|file| blame_file(repo, file, &BlameOptions::default()).expect("blame_file"))
    };
    let check = |repo: &Repository| {
        for file in ["f.txt", "g.txt"] {
            assert_blame_matches(repo, root, file, None, None);
            let git_meta = git_porcelain_metadata(root, file);
            let result = blame_file(repo, file, &BlameOptions::default()).expect("blame_file");
            for line in &result.lines {
                let info = result.commits.get(&line.commit).expect("metadata");
                assert_eq!(
                    Some(&grit_metadata_as_porcelain(info)),
                    git_meta.get(&line.commit),
                    "metadata for {} in {file}",
                    line.commit
                );
            }
        }
    };

    let loose_repo = Repository::discover(Some(root), &RepositoryOptions::empty()).expect("open");
    check(&loose_repo);
    let loose = blame_all(&loose_repo);
    let f = &loose[0];
    let summaries: Vec<&str> = f
        .lines
        .iter()
        .map(|l| f.commits[&l.commit].summary.as_str())
        .collect();
    assert_eq!(summaries, ["base", "message", "no author subject"]);
    drop(loose_repo);

    git_run(root, &["repack", "-adq"]);
    git_run(root, &["prune-packed"]);
    let packed_repo = Repository::discover(Some(root), &RepositoryOptions::empty()).expect("open");
    check(&packed_repo);
    assert_eq!(
        blame_all(&packed_repo),
        loose,
        "blame must not depend on whether objects are loose or packed"
    );
}

/// Write `lines` as the only file of a commit with `parents`, dated `time`, via plumbing.
fn git_commit_lines(
    dir: &Path,
    file: &str,
    lines: &[String],
    parents: &[String],
    time: i64,
) -> String {
    use std::io::Write;
    use std::process::Stdio;

    let pipe = |args: &[&str], input: &[u8], date: Option<i64>| -> String {
        let mut cmd = Command::new("git");
        cmd.current_dir(dir)
            .args(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        git_env(&mut cmd);
        if let Some(date) = date {
            let stamp = format!("{date} +0000");
            cmd.env("GIT_AUTHOR_DATE", &stamp)
                .env("GIT_COMMITTER_DATE", &stamp);
        }
        let mut child = cmd.spawn().expect("spawn git");
        child
            .stdin
            .take()
            .expect("stdin")
            .write_all(input)
            .expect("write stdin");
        let out = child.wait_with_output().expect("wait git");
        assert!(
            out.status.success(),
            "git {args:?} failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8_lossy(&out.stdout).trim().to_owned()
    };
    let content: String = lines.iter().map(|l| format!("{l}\n")).collect();
    let blob = pipe(&["hash-object", "-w", "--stdin"], content.as_bytes(), None);
    let tree = pipe(
        &["mktree"],
        format!("100644 blob {blob}\t{file}\n").as_bytes(),
        None,
    );
    let mut args = vec!["commit-tree".to_owned(), tree];
    for parent in parents {
        args.push("-p".to_owned());
        args.push(parent.clone());
    }
    args.push("-m".to_owned());
    args.push(format!("commit at {time}"));
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    pipe(&args, b"", Some(time))
}

/// Deterministic xorshift generator so the merge-heavy history is reproducible.
struct Rng(u64);

impl Rng {
    fn next(&mut self, bound: usize) -> usize {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        (self.0 % bound as u64) as usize
    }
}

/// Build a history with three long-lived branches that fork from each other's tips, edit
/// (replace, insert, delete and move lines), and merge back and forth — diamond and
/// criss-cross merges, merges that keep one side's older text, evil merges, and committer
/// dates with clock skew. Returns the branch tips.
///
/// Every line text stays unique within a version, so the attribution depends only on how
/// blame walks merges, not on how a diff pairs up repeated lines.
fn build_merge_heavy_history(root: &Path, file: &str, seed: u64) -> Vec<String> {
    git_run(root, &["init", "-q", "-b", "main"]);
    let mut rng = Rng(seed);
    let mut counter = 0usize;
    let mut fresh = |rng: &mut Rng| {
        counter += 1;
        format!("line {counter} {}", rng.next(1000))
    };
    let base: Vec<String> = (0..30).map(|_| fresh(&mut rng)).collect();
    let mut time = 1_600_000_000_i64;
    let root_commit = git_commit_lines(root, file, &base, &[], time);
    let mut tips: Vec<(String, Vec<String>)> = vec![(root_commit, base); 3];

    for _ in 0..90 {
        time += 60;
        let date = match rng.next(8) {
            0 => time - rng.next(3600) as i64,
            1 => time - 60,
            _ => time,
        };
        let b = rng.next(3);
        match rng.next(10) {
            0..=6 => {
                let mut lines = tips[b].1.clone();
                for _ in 0..1 + rng.next(3) {
                    let len = lines.len().max(1);
                    match rng.next(5) {
                        0 | 1 if !lines.is_empty() => {
                            let i = rng.next(len);
                            lines[i] = fresh(&mut rng);
                        }
                        2 => {
                            let i = rng.next(len + 1).min(lines.len());
                            let line = fresh(&mut rng);
                            lines.insert(i, line);
                        }
                        3 if lines.len() > 5 => {
                            let line = lines.remove(rng.next(len));
                            let i = rng.next(lines.len() + 1);
                            if rng.next(2) == 0 {
                                lines.insert(i, line);
                            }
                        }
                        _ if !lines.is_empty() => {
                            let i = rng.next(len);
                            let line = lines.remove(i);
                            lines.push(line);
                        }
                        _ => {}
                    }
                }
                let oid = git_commit_lines(root, file, &lines, &[tips[b].0.clone()], date);
                tips[b] = (oid, lines);
            }
            7 | 8 => {
                let other = (b + 1 + rng.next(2)) % 3;
                if tips[other].0 == tips[b].0 {
                    continue;
                }
                let ours = &tips[b].1;
                let theirs = &tips[other].1;
                let mut lines = ours.clone();
                let start = rng.next(theirs.len().max(1));
                let end = (start + 1 + rng.next(8)).min(theirs.len());
                for i in start..end {
                    if lines.contains(&theirs[i]) {
                        continue;
                    }
                    if i < lines.len() {
                        lines[i] = theirs[i].clone();
                    } else {
                        lines.push(theirs[i].clone());
                    }
                }
                if rng.next(4) == 0 && !lines.is_empty() {
                    let i = rng.next(lines.len());
                    lines[i] = fresh(&mut rng);
                }
                let parents = [tips[b].0.clone(), tips[other].0.clone()];
                let oid = git_commit_lines(root, file, &lines, &parents, date);
                tips[b] = (oid, lines);
            }
            _ => {
                let other = (b + 1 + rng.next(2)) % 3;
                tips[b] = tips[other].clone();
            }
        }
    }
    tips.into_iter().map(|(oid, _)| oid).collect()
}

fn assert_merge_heavy_history_matches_git(seed: u64) {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();
    let file = "merged.txt";
    let tips = build_merge_heavy_history(root, file, seed);
    let check = || {
        let repo = Repository::discover(Some(root), &RepositoryOptions::empty()).expect("open");
        for tip in &tips {
            assert_blame_matches(&repo, root, file, Some(tip), None);
        }
    };
    check();
    // `--reachable` only covers commits reachable from refs; the history is built with
    // plumbing, so point a branch at every tip first.
    for (i, tip) in tips.iter().enumerate() {
        git_run(root, &["update-ref", &format!("refs/heads/tip-{i}"), tip]);
    }
    git_run(
        root,
        &["commit-graph", "write", "--reachable", "--changed-paths"],
    );
    assert!(
        root.join(".git/objects/info/commit-graph").is_file(),
        "commit-graph was not written"
    );
    check();
}

#[test]
fn blame_merge_heavy_history_matches_git_with_and_without_commit_graph() {
    for seed in [
        0x9e37_79b9_7f4a_7c15,
        0x2545_f491_4f6c_dd1d,
        0x1234_5678_9abc_def1,
    ] {
        assert_merge_heavy_history_matches_git(seed);
    }
}

/// A merge commit that introduces the blamed path (no parent has it), on the mainline and on
/// a side branch that a later mainline merge brings in (#1785 review).
#[test]
fn blame_merge_introducing_path_matches_git() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();
    git_run(root, &["init", "-q", "-b", "main"]);
    std::fs::write(root.join("base.txt"), "base\n").unwrap();
    git_run(root, &["add", "."]);
    git_run(root, &["commit", "-qm", "base"]);

    git_run(root, &["checkout", "-qb", "side"]);
    std::fs::write(root.join("s.txt"), "s\n").unwrap();
    git_run(root, &["add", "."]);
    git_run(root, &["commit", "-qm", "side"]);
    git_run(root, &["checkout", "-q", "main"]);
    std::fs::write(root.join("m.txt"), "m\n").unwrap();
    git_run(root, &["add", "."]);
    git_run(root, &["commit", "-qm", "main"]);
    git_run(root, &["merge", "-q", "--no-ff", "--no-commit", "side"]);
    std::fs::write(root.join("new.txt"), "one\ntwo\nthree\n").unwrap();
    git_run(root, &["add", "new.txt"]);
    git_run(root, &["commit", "-qm", "merge adds new.txt"]);
    let mainline_merge = git_output(root, &["rev-parse", "HEAD"]).trim().to_owned();

    // The same shape on a side branch: its merge adds `late.txt`, and the mainline edits the
    // file after merging the side branch, so the side merge is reached as a queued suspect.
    git_run(root, &["checkout", "-qb", "topic", "HEAD~1"]);
    git_run(root, &["checkout", "-qb", "topic-inner"]);
    std::fs::write(root.join("t.txt"), "t\n").unwrap();
    git_run(root, &["add", "."]);
    git_run(root, &["commit", "-qm", "topic inner"]);
    git_run(root, &["checkout", "-q", "topic"]);
    std::fs::write(root.join("u.txt"), "u\n").unwrap();
    git_run(root, &["add", "."]);
    git_run(root, &["commit", "-qm", "topic"]);
    git_run(
        root,
        &["merge", "-q", "--no-ff", "--no-commit", "topic-inner"],
    );
    std::fs::write(root.join("late.txt"), "alpha\nbeta\ngamma\ndelta\n").unwrap();
    git_run(root, &["add", "late.txt"]);
    git_run(root, &["commit", "-qm", "topic merge adds late.txt"]);
    git_run(root, &["checkout", "-q", "main"]);
    std::fs::write(root.join("main2.txt"), "m2\n").unwrap();
    git_run(root, &["add", "."]);
    git_run(root, &["commit", "-qm", "main again"]);
    git_run(
        root,
        &["merge", "-q", "--no-ff", "-m", "merge topic", "topic"],
    );
    std::fs::write(root.join("late.txt"), "alpha\nBETA\ngamma\ndelta\n").unwrap();
    git_run(root, &["commit", "-qam", "edit late.txt on main"]);

    let repo = Repository::discover(Some(root), &RepositoryOptions::empty()).expect("open");
    assert_blame_matches(&repo, root, "new.txt", Some(&mainline_merge), None);
    assert_blame_matches(&repo, root, "new.txt", None, None);
    assert_blame_matches(&repo, root, "late.txt", None, None);
}

/// A merge that renames the blamed file still follows it into the first parent.
#[test]
fn blame_merge_renaming_path_matches_git() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();
    git_run(root, &["init", "-q", "-b", "main"]);
    std::fs::write(root.join("old.txt"), "a\nb\nc\nd\ne\n").unwrap();
    git_run(root, &["add", "."]);
    git_run(root, &["commit", "-qm", "base"]);
    git_run(root, &["checkout", "-qb", "side"]);
    std::fs::write(root.join("s.txt"), "s\n").unwrap();
    git_run(root, &["add", "."]);
    git_run(root, &["commit", "-qm", "side"]);
    git_run(root, &["checkout", "-q", "main"]);
    std::fs::write(root.join("old.txt"), "a\nB\nc\nd\ne\n").unwrap();
    git_run(root, &["commit", "-qam", "main edit"]);
    git_run(root, &["merge", "-q", "--no-ff", "--no-commit", "side"]);
    git_run(root, &["mv", "old.txt", "new.txt"]);
    git_run(root, &["commit", "-qm", "merge renames old.txt"]);

    let repo = Repository::discover(Some(root), &RepositoryOptions::empty()).expect("open");
    assert_blame_matches(&repo, root, "new.txt", None, None);
}

/// Subtree merge (`read-tree --prefix=vendor/`): blame follows rename into merged-in parent.
#[test]
fn blame_subtree_merge_vendor_prefix_matches_git() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();
    let lib = root.join("lib");
    let proj = root.join("proj");
    std::fs::create_dir_all(&lib).unwrap();
    std::fs::create_dir_all(&proj).unwrap();

    git_run(&lib, &["init", "-q", "-b", "main"]);
    git_run(&lib, &["config", "user.name", "Blame Test"]);
    git_run(&lib, &["config", "user.email", "blame@example.com"]);
    std::fs::write(lib.join("app.sh"), "a1\na2\na3\n").unwrap();
    git_run(&lib, &["add", "."]);
    git_run(&lib, &["commit", "-qm", "lib: first"]);
    std::fs::write(lib.join("app.sh"), "a1\na2\na3\nb4 by bob\n").unwrap();
    git_run(&lib, &["add", "app.sh"]);
    git_run(&lib, &["commit", "-qam", "lib: bob adds line"]);

    git_run(&lib, &["mv", "app.sh", "renamed.sh"]);
    std::fs::write(
        lib.join("renamed.sh"),
        "a1\na2\nchanged middle\na3\nb4 by bob\n",
    )
    .unwrap();
    git_run(&lib, &["add", "-A"]);
    git_run(
        &lib,
        &["commit", "-qm", "lib: rename and insert middle line"],
    );

    git_run(&proj, &["init", "-q", "-b", "main"]);
    git_run(&proj, &["config", "user.name", "Blame Test"]);
    git_run(&proj, &["config", "user.email", "blame@example.com"]);
    std::fs::write(proj.join("README"), "readme\n").unwrap();
    git_run(&proj, &["add", "."]);
    git_run(&proj, &["commit", "-qm", "proj init"]);

    git_run(&proj, &["remote", "add", "lib", &lib.to_string_lossy()]);
    git_run(&proj, &["fetch", "-q", "lib"]);
    git_run(
        &proj,
        &[
            "merge",
            "-q",
            "-s",
            "ours",
            "--no-commit",
            "--allow-unrelated-histories",
            "lib/main",
        ],
    );
    git_run(&proj, &["read-tree", "--prefix=vendor/", "-u", "lib/main"]);
    git_run(&proj, &["commit", "-qm", "Merge lib as subtree"]);

    let repo = Repository::discover(Some(&proj), &RepositoryOptions::empty()).expect("open");
    for range in [None, Some((1, 2)), Some((4, 5)), Some((1, 5))] {
        assert_blame_matches(&repo, &proj, "vendor/renamed.sh", None, range);
    }
    git_run(&proj, &["fsck", "--strict"]);
}

/// One merge parent has the blamed path; another carries the same result bytes under a renamed path.
#[test]
fn blame_merge_same_path_and_renamed_sibling_parent_matches_git() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();
    git_run(root, &["init", "-q", "-b", "main"]);
    git_run(root, &["config", "user.name", "Blame Test"]);
    git_run(root, &["config", "user.email", "blame@example.com"]);

    std::fs::write(root.join("README"), "base\n").unwrap();
    git_run(root, &["add", "README"]);
    git_run(root, &["commit", "-qm", "common base"]);
    let base = git_output(root, &["rev-parse", "HEAD"]).trim().to_owned();

    git_run(root, &["checkout", "-qb", "side", &base]);
    std::fs::write(root.join("old.txt"), "line one\nline two\n").unwrap();
    git_run(root, &["add", "old.txt"]);
    git_run(root, &["commit", "-qm", "side adds old.txt only"]);

    git_run(root, &["checkout", "-q", "main"]);
    std::fs::write(root.join("target.txt"), "placeholder\n").unwrap();
    git_run(root, &["add", "target.txt"]);
    git_run(root, &["commit", "-qm", "main adds target.txt"]);
    git_run(root, &["merge", "-q", "--no-ff", "--no-commit", "side"]);
    git_run(root, &["rm", "-f", "target.txt"]);
    git_run(root, &["mv", "old.txt", "target.txt"]);
    git_run(
        root,
        &["commit", "-qm", "merge: rename old.txt to target.txt"],
    );

    let repo = Repository::discover(Some(root), &RepositoryOptions::empty()).expect("open");
    assert_blame_matches(&repo, root, "target.txt", None, None);
}

#[test]
fn blame_final_newline_changes_match_git() {
    // Include repeated bodies and transitions that change line boundaries as well as EOF.
    let histories: &[&[&str]] = &[
        &["hello", "hello\n", "hello"],
        &["first\nlast", "first\nlast\n", "first\nlast"],
        &["same\nsame", "same\nsame\n", "same\nsame"],
        &["first\r\nlast\r", "first\r\nlast\r\n", "first\r\nlast\r"],
        &["", "\n", "", "hello", "hello\n", ""],
        &[
            "first\nlast",
            "first\nlast\nappended\n",
            "first\nlast\nappended",
        ],
        &[
            "first\nhello world",
            "first\nhello\nworld\n",
            "first\nhello world",
        ],
        &["first\n\n", "first\n", "first", "first\n"],
    ];
    for history in histories {
        let tmp = tempfile::tempdir().expect("tempdir");
        let root = tmp.path();
        git_run(root, &["init", "-q", "-b", "main"]);
        git_run(root, &["config", "core.autocrlf", "false"]);
        git_run(root, &["config", "gc.auto", "0"]);
        for (step, text) in history.iter().enumerate() {
            std::fs::write(root.join("a.txt"), text).unwrap();
            git_run(root, &["add", "a.txt"]);
            git_run(root, &["commit", "-qm", &format!("version {step}")]);
            // Walk through a commit whose tree changes but whose blamed blob does not.
            std::fs::write(root.join("other"), format!("unrelated {step}\n")).unwrap();
            git_run(root, &["add", "other"]);
            git_run(root, &["commit", "-qm", "unrelated"]);
            for algorithm in ["myers", "minimal", "patience", "histogram"] {
                git_run(root, &["config", "diff.algorithm", algorithm]);
                let repo = Repository::discover(Some(root), &RepositoryOptions::empty()).unwrap();
                assert_blame_matches(&repo, root, "a.txt", None, None);
                let count = text.split_inclusive('\n').count();
                if count > 0 {
                    assert_blame_matches(&repo, root, "a.txt", None, Some((count, count)));
                    assert_blame_matches(&repo, root, "a.txt", None, Some((1, 1)));
                }
                let result = blame_file(&repo, "a.txt", &BlameOptions::default()).unwrap();
                let bodies: Vec<_> = text
                    .split_inclusive('\n')
                    .map(|line| line.strip_suffix('\n').unwrap_or(line))
                    .collect();
                assert_eq!(
                    result
                        .lines
                        .iter()
                        .map(|line| line.content.as_str())
                        .collect::<Vec<_>>(),
                    bodies
                );
            }
        }
        git_run(root, &["fsck", "--strict"]);
    }
}

#[test]
fn blame_final_newline_from_merge_parent_matches_git() {
    for (before, after) in [
        ("first\nlast", "first\nlast\n"),
        ("first\nlast\n", "first\nlast"),
    ] {
        let tmp = tempfile::tempdir().expect("tempdir");
        let root = tmp.path();
        git_run(root, &["init", "-q", "-b", "main"]);
        std::fs::write(root.join("a.txt"), before).unwrap();
        git_run(root, &["add", "a.txt"]);
        git_run(root, &["commit", "-qm", "root"]);
        git_run(root, &["checkout", "-qb", "side"]);
        std::fs::write(root.join("a.txt"), after).unwrap();
        git_run(root, &["commit", "-qam", "add newline"]);
        git_run(root, &["checkout", "-q", "main"]);
        std::fs::write(root.join("other"), "unrelated\n").unwrap();
        git_run(root, &["add", "other"]);
        git_run(root, &["commit", "-qm", "main"]);
        git_run(root, &["merge", "--no-ff", "side", "-m", "merge newline"]);
        let repo = Repository::discover(Some(root), &RepositoryOptions::empty()).unwrap();
        assert_blame_matches(&repo, root, "a.txt", None, None);
        assert_blame_matches(&repo, root, "a.txt", None, Some((2, 2)));
        git_run(root, &["fsck", "--strict"]);
    }
}

#[test]
fn blame_copy_matching_preserves_final_newline() {
    use grit_lib::blame::CopyDetection;
    let anchor = "anchoralphabetabcdefghijklmnopqrstuvwxyz0123456789";
    let tail = "endingalphabetabcdefghijklmnopqrstuvwxyz0123456789";
    for existing_target in [false, true] {
        for (source_ending, target_ending) in [("", "\n"), ("\n", ""), ("\n", "\n")] {
            let tmp = tempfile::tempdir().unwrap();
            let root = tmp.path();
            git_run(root, &["init", "-q", "-b", "main"]);
            std::fs::write(
                root.join("source"),
                format!("{anchor}\n{tail}{source_ending}"),
            )
            .unwrap();
            if existing_target {
                std::fs::write(root.join("target"), "existing\n").unwrap();
            }
            git_run(root, &["add", "."]);
            git_run(root, &["commit", "-qm", "base"]);
            let prefix = if existing_target { "existing\n" } else { "" };
            std::fs::write(
                root.join("target"),
                format!("{prefix}{anchor}\n{tail}{target_ending}"),
            )
            .unwrap();
            git_run(root, &["add", "."]);
            git_run(root, &["commit", "-qm", "copy"]);
            let repo = Repository::discover(Some(root), &RepositoryOptions::empty()).unwrap();
            for ignore_space in [false, true] {
                let mut args = vec!["blame", "--line-porcelain", "-C", "-C", "-C"];
                if ignore_space {
                    args.push("-w");
                }
                args.extend(["--", "target"]);
                let oracle = parse_git_blame_porcelain(&git_output(root, &args));
                let result = blame_file(
                    &repo,
                    "target",
                    &BlameOptions {
                        copy_detection: CopyDetection::FromAnyCommit,
                        whitespace: if ignore_space {
                            grit_lib::diff::WhitespaceMode::ignore_all_space()
                        } else {
                            Default::default()
                        },
                        ..Default::default()
                    },
                )
                .unwrap();
                let lines: Vec<_> = result
                    .lines
                    .iter()
                    .map(|line| PorcelainLine {
                        commit: line.commit,
                        final_line: line.final_line,
                        original_line: line.original_line,
                        original_path: line.original_path.clone(),
                    })
                    .collect();
                assert_eq!(lines, oracle, "existing={existing_target} endings={source_ending:?}/{target_ending:?} -w={ignore_space}");
                assert_eq!(
                    result.lines[usize::from(existing_target)].original_path,
                    "source"
                );
            }
            git_run(root, &["fsck", "--strict"]);
        }
    }
}

#[test]
fn packed_churning_trees_full_blame_matches_git_and_alternate() {
    let source = tempfile::tempdir().expect("source");
    grit_test_support::blame::packed_history(source.path(), 180, 256).expect("history");
    let repo =
        Repository::discover(Some(source.path()), &RepositoryOptions::empty()).expect("open");
    assert_blame_matches(&repo, source.path(), "target.txt", Some("HEAD"), None);
    assert_blame_matches(
        &repo,
        source.path(),
        "target.txt",
        Some("HEAD"),
        Some((1, 3)),
    );
    let alternate = tempfile::tempdir().expect("alternate");
    git_run(
        alternate.path(),
        &[
            "clone",
            "-q",
            "--shared",
            source.path().to_str().expect("path"),
            ".",
        ],
    );
    let repo = Repository::discover(Some(alternate.path()), &RepositoryOptions::empty())
        .expect("alternate open");
    assert_blame_matches(&repo, alternate.path(), "target.txt", Some("HEAD"), None);
    assert_blame_matches(
        &repo,
        alternate.path(),
        "target.txt",
        Some("HEAD"),
        Some((1, 3)),
    );
    git_run(alternate.path(), &["fsck", "--strict"]);
}

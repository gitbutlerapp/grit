//! Git compatibility tests for diff target selection and `Diff::files`.

use grit_lib::config::environment::RepositoryOptions;
use grit_lib::diff::{resolve_diff_arguments, DiffTarget, ResolveDiffArgumentsError};
use grit_lib::objects::ObjectId;
use grit_lib::repo::Repository;
use grit_test_support::git;
use tempfile::TempDir;

fn init_repo(dir: &std::path::Path) {
    git(dir, &["init", "-q", "-b", "main", "."]);
    git(dir, &["config", "user.email", "u@example.com"]);
    git(dir, &["config", "user.name", "U"]);
}

fn commit_all(dir: &std::path::Path, msg: &str) -> ObjectId {
    git(dir, &["add", "-A"]);
    git(dir, &["commit", "-qm", msg]);
    git(dir, &["rev-parse", "HEAD"])
        .trim()
        .parse()
        .expect("oid")
}

fn open_repo(root: &std::path::Path) -> Repository {
    Repository::open(
        &root.join(".git"),
        &RepositoryOptions::empty().with_work_tree(root),
    )
    .expect("open")
}

#[test]
fn resolve_diff_default_target_is_worktree_vs_head() {
    let dir = TempDir::new().expect("tempdir");
    init_repo(dir.path());
    std::fs::write(dir.path().join("a.txt"), "x\n").expect("write");
    commit_all(dir.path(), "c1");
    let repo = open_repo(dir.path());
    std::fs::write(dir.path().join("a.txt"), "y\n").expect("write");

    let args = resolve_diff_arguments(&repo, None, false, false, None, &[], &[]).expect("resolve");
    assert_eq!(args.target, DiffTarget::WorktreeVsHead);
    assert!(args.pathspecs.is_empty());
}

#[test]
fn resolve_diff_staged_and_unstaged_conflict() {
    let dir = TempDir::new().expect("tempdir");
    init_repo(dir.path());
    let repo = open_repo(dir.path());
    let err = resolve_diff_arguments(&repo, None, true, true, None, &[], &[]).unwrap_err();
    assert_eq!(err, ResolveDiffArgumentsError::StagedAndUnstaged);
}

#[test]
fn files_staged_matches_git_name_only() {
    let dir = TempDir::new().expect("tempdir");
    init_repo(dir.path());
    std::fs::write(dir.path().join("f.txt"), "one\n").expect("write");
    commit_all(dir.path(), "c1");
    std::fs::write(dir.path().join("f.txt"), "two\n").expect("write");
    git(dir.path(), &["add", "f.txt"]);

    let repo = open_repo(dir.path());
    let mut opts = grit_lib::diff::DiffOptions::default();
    opts.word_level_inline = false;
    let args = resolve_diff_arguments(&repo, None, true, false, None, &[], &[]).expect("resolve");
    let files = repo
        .diff()
        .files(&args.target, &args.pathspecs, &opts)
        .expect("files");
    let grit_paths: Vec<_> = files.iter().map(|f| f.path.as_str()).collect();

    let git_out = git(dir.path(), &["diff", "--staged", "--name-only"]);
    let git_paths: Vec<_> = git_out.lines().filter(|l| !l.is_empty()).collect();
    assert_eq!(grit_paths, git_paths);
}

#[test]
fn files_commit_range_matches_git_name_only() {
    let dir = TempDir::new().expect("tempdir");
    init_repo(dir.path());
    std::fs::write(dir.path().join("a.txt"), "1\n").expect("write");
    commit_all(dir.path(), "c1");
    std::fs::write(dir.path().join("b.txt"), "2\n").expect("write");
    let tip = commit_all(dir.path(), "c2");
    let base: ObjectId = git(dir.path(), &["rev-parse", "HEAD~1"])
        .trim()
        .parse()
        .expect("oid");

    let repo = open_repo(dir.path());
    let mut opts = grit_lib::diff::DiffOptions::default();
    opts.word_level_inline = false;
    let target = DiffTarget::Trees {
        old: Some(base),
        new: tip,
    };
    let files = repo.diff().files(&target, &[], &opts).expect("files");
    let mut grit_paths: Vec<_> = files.iter().map(|f| f.path.clone()).collect();
    grit_paths.sort();

    let git_out = git(
        dir.path(),
        &["diff", "--name-only", &base.to_hex(), &tip.to_hex()],
    );
    let mut git_paths: Vec<_> = git_out
        .lines()
        .filter(|l| !l.is_empty())
        .map(str::to_owned)
        .collect();
    git_paths.sort();
    assert_eq!(grit_paths, git_paths);
}

fn grit_files(
    repo: &Repository,
    target: DiffTarget,
    pathspecs: &[String],
) -> Vec<grit_lib::diff::FileDiff> {
    let mut opts = grit_lib::diff::DiffOptions::default();
    opts.word_level_inline = false;
    repo.diff().files(&target, pathspecs, &opts).expect("files")
}

fn git_raw(repo: &std::path::Path, args: &[&str]) -> Vec<String> {
    let mut cmd_args = vec!["-c", "core.abbrev=40"];
    cmd_args.extend(args);
    git(repo, &cmd_args)
        .lines()
        .filter(|l| l.starts_with(':'))
        .map(str::to_owned)
        .collect()
}

fn grit_raw(repo: &Repository, target: DiffTarget) -> Vec<String> {
    use grit_lib::diff::{format_raw, DiffEntry, DiffStatus};
    grit_files(repo, target, &[])
        .into_iter()
        .map(|f| {
            let status = f.status;
            let path = f.path.clone();
            let mut e = DiffEntry {
                status,
                old_path: f.old_path.clone(),
                new_path: if status == DiffStatus::Deleted {
                    None
                } else {
                    Some(path)
                },
                old_mode: f.old_mode.clone().unwrap_or_else(|| "000000".into()),
                new_mode: f.new_mode.clone().unwrap_or_else(|| "000000".into()),
                old_oid: f.old_oid,
                new_oid: f.new_oid,
                score: None,
            };
            if status == DiffStatus::Deleted {
                e.old_path = Some(f.path);
            }
            format_raw(&e)
        })
        .collect()
}

#[test]
fn worktree_vs_head_matches_git_diff_head() {
    let dir = TempDir::new().expect("tempdir");
    init_repo(dir.path());
    std::fs::write(dir.path().join("a.txt"), "v1\n").expect("write");
    commit_all(dir.path(), "c1");
    std::fs::write(dir.path().join("a.txt"), "v2\n").expect("write");
    std::fs::write(dir.path().join("new.txt"), "n\n").expect("write");

    let repo = open_repo(dir.path());
    let grit = grit_raw(&repo, DiffTarget::WorktreeVsHead);
    let git = git_raw(dir.path(), &["diff", "--raw", "-M50", "HEAD"]);
    assert_eq!(grit, git);
}

#[test]
fn index_vs_head_matches_git_diff_cached() {
    let dir = TempDir::new().expect("tempdir");
    init_repo(dir.path());
    std::fs::write(dir.path().join("a.txt"), "v1\n").expect("write");
    commit_all(dir.path(), "c1");
    std::fs::write(dir.path().join("a.txt"), "v2\n").expect("write");
    git(dir.path(), &["add", "a.txt"]);

    let repo = open_repo(dir.path());
    let grit = grit_raw(&repo, DiffTarget::IndexVsHead);
    let git = git_raw(dir.path(), &["diff", "--raw", "-M50", "--cached"]);
    assert_eq!(grit, git);
}

#[test]
fn worktree_vs_index_matches_git_diff() {
    let dir = TempDir::new().expect("tempdir");
    init_repo(dir.path());
    std::fs::write(dir.path().join("a.txt"), "v1\n").expect("write");
    commit_all(dir.path(), "c1");
    std::fs::write(dir.path().join("a.txt"), "v2\n").expect("write");
    git(dir.path(), &["add", "a.txt"]);
    std::fs::write(dir.path().join("a.txt"), "v3\n").expect("write");

    let repo = open_repo(dir.path());
    let grit = grit_raw(&repo, DiffTarget::WorktreeVsIndex);
    let git = git_raw(dir.path(), &["diff", "--raw", "-M50"]);
    assert_eq!(grit, git);
}

#[test]
fn commit_matches_git_show() {
    let dir = TempDir::new().expect("tempdir");
    init_repo(dir.path());
    std::fs::write(dir.path().join("a.txt"), "1\n").expect("write");
    commit_all(dir.path(), "c1");
    std::fs::write(dir.path().join("a.txt"), "2\n").expect("write");
    let c2 = commit_all(dir.path(), "c2");

    let repo = open_repo(dir.path());
    let grit = grit_raw(&repo, DiffTarget::Commit(c2));
    let git = git_raw(
        dir.path(),
        &["show", "--raw", "-M50", "--format=", &c2.to_hex()],
    );
    assert_eq!(grit, git);
}

#[test]
fn trees_match_git_diff_a_b() {
    let dir = TempDir::new().expect("tempdir");
    init_repo(dir.path());
    std::fs::write(dir.path().join("a.txt"), "1\n").expect("write");
    let c1 = commit_all(dir.path(), "c1");
    std::fs::write(dir.path().join("b.txt"), "2\n").expect("write");
    let c2 = commit_all(dir.path(), "c2");

    let repo = open_repo(dir.path());
    let target = DiffTarget::Trees {
        old: Some(c1),
        new: c2,
    };
    let grit = grit_raw(&repo, target);
    let git = git_raw(
        dir.path(),
        &["diff", "--raw", "-M50", &c1.to_hex(), &c2.to_hex()],
    );
    assert_eq!(grit, git);
}

#[test]
fn triple_dot_uses_merge_base_like_git() {
    let dir = TempDir::new().expect("tempdir");
    init_repo(dir.path());
    std::fs::write(dir.path().join("base.txt"), "b\n").expect("write");
    commit_all(dir.path(), "root");
    git(dir.path(), &["switch", "-c", "topic"]);
    std::fs::write(dir.path().join("topic.txt"), "t\n").expect("write");
    let topic_tip = commit_all(dir.path(), "topic");
    git(dir.path(), &["switch", "main"]);
    std::fs::write(dir.path().join("main.txt"), "m\n").expect("write");
    let main_tip = commit_all(dir.path(), "main");

    let repo = open_repo(dir.path());
    let args = resolve_diff_arguments(
        &repo,
        None,
        false,
        false,
        None,
        &["main...topic".into()],
        &[],
    )
    .expect("resolve");
    let grit = grit_raw(&repo, args.target);
    let git = git_raw(
        dir.path(),
        &[
            "diff",
            "--raw",
            "-M50",
            &format!("{}...{}", main_tip.to_hex(), topic_tip.to_hex()),
        ],
    );
    assert_eq!(grit, git);
}

#[test]
fn pathspec_filter_matches_git() {
    let dir = TempDir::new().expect("tempdir");
    init_repo(dir.path());
    std::fs::write(dir.path().join("keep.txt"), "k\n").expect("write");
    std::fs::write(dir.path().join("skip.txt"), "s\n").expect("write");
    commit_all(dir.path(), "c1");
    std::fs::write(dir.path().join("keep.txt"), "k2\n").expect("write");
    std::fs::write(dir.path().join("skip.txt"), "s2\n").expect("write");

    let repo = open_repo(dir.path());
    let specs = vec!["keep.txt".to_owned()];
    let grit = grit_files(&repo, DiffTarget::WorktreeVsHead, &specs);
    let git = git_raw(
        dir.path(),
        &["diff", "--raw", "-M50", "HEAD", "--", "keep.txt"],
    );
    assert_eq!(grit.len(), git.len());
    assert!(grit.iter().all(|f| f.path == "keep.txt"));
}

#[test]
fn resolve_diff_arguments_too_many_revisions() {
    let dir = TempDir::new().expect("tempdir");
    init_repo(dir.path());
    std::fs::write(dir.path().join("f.txt"), "a\n").expect("write");
    commit_all(dir.path(), "a");
    std::fs::write(dir.path().join("f.txt"), "b\n").expect("write");
    commit_all(dir.path(), "b");
    std::fs::write(dir.path().join("f.txt"), "c\n").expect("write");
    commit_all(dir.path(), "c");
    let repo = open_repo(dir.path());
    let err = resolve_diff_arguments(
        &repo,
        None,
        false,
        false,
        None,
        &["HEAD~2".into(), "HEAD~1".into(), "HEAD".into()],
        &[],
    )
    .unwrap_err();
    assert_eq!(err, ResolveDiffArgumentsError::TooManyRevisions);
}

#[test]
fn worktree_vs_head_skips_assume_unchanged_with_staged_sibling() {
    let dir = TempDir::new().expect("tempdir");
    init_repo(dir.path());
    std::fs::write(dir.path().join("a.txt"), "a-v1\n").expect("write");
    std::fs::write(dir.path().join("b.txt"), "b-v1\n").expect("write");
    commit_all(dir.path(), "base");
    git(dir.path(), &["update-index", "--assume-unchanged", "a.txt"]);
    std::fs::write(dir.path().join("a.txt"), "a-dirty\n").expect("write");
    std::fs::write(dir.path().join("b.txt"), "b-v2\n").expect("write");
    git(dir.path(), &["add", "b.txt"]);

    let repo = open_repo(dir.path());
    let files = grit_files(&repo, DiffTarget::WorktreeVsHead, &[]);
    let grit_paths: Vec<_> = files.iter().map(|f| f.path.as_str()).collect();

    let git_out = git(dir.path(), &["diff", "--name-only", "HEAD"]);
    let git_paths: Vec<_> = git_out.lines().filter(|l| !l.is_empty()).collect();
    assert_eq!(grit_paths, git_paths);
    assert_eq!(grit_paths, vec!["b.txt"]);
}

#[test]
fn resolve_diff_arguments_against_with_commits() {
    let dir = TempDir::new().expect("tempdir");
    init_repo(dir.path());
    let repo = open_repo(dir.path());
    let err = resolve_diff_arguments(
        &repo,
        None,
        false,
        false,
        Some("HEAD"),
        &["HEAD~1".into()],
        &[],
    )
    .unwrap_err();
    assert_eq!(err, ResolveDiffArgumentsError::AgainstWithCommits);
}

//! s7-style differential: full library pick, revert, and merge vs system git (tree and worktree).

use std::fs;
use std::process::Command;

use grit_lib::config::environment::RepositoryOptions;
use grit_lib::objects::ObjectId;
use grit_lib::porcelain::merge::integrate_branch;
use grit_lib::porcelain::replay::{replay_commit, ReplayDirection, ReplayOutcome, ReplayRequest};
use grit_lib::repo::Repository;
use grit_test_support::git;

const IDENT: &str = "Test User <t@example.com> 1700000100 +0000";

fn init_repo(root: &std::path::Path) {
    git(root, &["init", "-q", "-b", "main", "."]);
    git(root, &["config", "user.email", "t@example.com"]);
    git(root, &["config", "user.name", "Test"]);
}

fn open_repo(root: &std::path::Path) -> Repository {
    Repository::open(
        &root.join(".git"),
        &RepositoryOptions::empty().with_work_tree(root),
    )
    .expect("open")
}

fn copy_repo(from: &std::path::Path, to: &std::path::Path) {
    fs::create_dir_all(to).expect("mkdir");
    let status = Command::new("cp")
        .args([
            "-a",
            &format!("{}/.", from.display()),
            &to.to_string_lossy(),
        ])
        .status()
        .expect("cp");
    assert!(status.success());
    git(to, &["read-tree", "HEAD"]);
    git(to, &["checkout-index", "-f", "-u", "-q"]);
}

fn commit_file(root: &std::path::Path, path: &str, content: &str, msg: &str) {
    if let Some(parent) = root.join(path).parent() {
        fs::create_dir_all(parent).expect("mkdir");
    }
    fs::write(root.join(path), content).expect("write");
    git(root, &["add", path]);
    git(root, &["commit", "-qm", msg]);
}

fn head_tree_hex(wt: &std::path::Path) -> String {
    git(wt, &["rev-parse", "HEAD^{tree}"]).trim().to_owned()
}

fn git_index_stage0_fingerprint(wt: &std::path::Path) -> Vec<String> {
    git(wt, &["ls-files", "-s"])
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .map(str::to_owned)
        .collect()
}

fn index_stage0_fingerprint(wt: &std::path::Path) -> Vec<String> {
    git_index_stage0_fingerprint(wt)
}

/// Stage-0 index rows and git's cached/staged diff vs HEAD (matches post-pick/revert/merge expectations).
fn assert_git_index_matches_head_and_clean(wt: &std::path::Path) {
    assert!(
        git(wt, &["diff-index", "--cached", "HEAD"])
            .trim()
            .is_empty(),
        "expected no staged diff vs HEAD: {}",
        git(wt, &["diff-index", "--cached", "HEAD"])
    );
    assert!(
        git(wt, &["status", "--porcelain"]).trim().is_empty(),
        "expected clean porcelain status: {}",
        git(wt, &["status", "--porcelain"])
    );
}

fn worktree_fingerprint(wt: &std::path::Path) -> Vec<u8> {
    let mut out = Vec::new();
    fn walk(dir: &std::path::Path, prefix: &str, out: &mut Vec<u8>) {
        for entry in fs::read_dir(dir).expect("read_dir") {
            let entry = entry.expect("entry");
            let name = entry.file_name();
            if name == ".git" {
                continue;
            }
            let path = entry.path();
            let rel = if prefix.is_empty() {
                name.to_string_lossy().into_owned()
            } else {
                format!("{prefix}/{name}", name = name.to_string_lossy())
            };
            if path.is_dir() {
                walk(&path, &rel, out);
            } else {
                out.extend(rel.as_bytes());
                out.push(0);
                out.extend(fs::read(&path).expect("read file"));
            }
        }
    }
    walk(wt, "", &mut out);
    out
}

fn replay_pick(repo: &Repository, commit: ObjectId) -> ReplayOutcome {
    replay_commit(
        repo,
        &ReplayRequest {
            commit,
            direction: ReplayDirection::Pick,
            committer: IDENT.to_owned(),
            message_override: None,
        },
    )
    .expect("replay pick")
}

#[test]
fn s7_differential_pick_matches_git() {
    let base = tempfile::tempdir().expect("tempdir");
    init_repo(base.path());
    commit_file(base.path(), "a.txt", "base\n", "base");
    git(base.path(), &["switch", "-c", "topic"]);
    commit_file(base.path(), "b.txt", "topic\n", "topic");
    let pick_source = git(base.path(), &["rev-parse", "HEAD"]).trim().to_owned();
    git(base.path(), &["switch", "main"]);
    commit_file(base.path(), "c.txt", "main\n", "main");

    let grit_dir = tempfile::tempdir().expect("grit copy");
    copy_repo(base.path(), grit_dir.path());
    replay_pick(
        &open_repo(grit_dir.path()),
        ObjectId::from_hex(&pick_source).expect("oid"),
    );
    let git_dir = tempfile::tempdir().expect("git copy");
    copy_repo(base.path(), git_dir.path());
    git(git_dir.path(), &["cherry-pick", &pick_source]);

    assert_eq!(
        head_tree_hex(grit_dir.path()),
        head_tree_hex(git_dir.path())
    );
    assert_eq!(
        worktree_fingerprint(grit_dir.path()),
        worktree_fingerprint(git_dir.path())
    );
    assert_eq!(
        index_stage0_fingerprint(grit_dir.path()),
        git_index_stage0_fingerprint(git_dir.path())
    );
    assert_git_index_matches_head_and_clean(grit_dir.path());
}

#[test]
fn s7_differential_revert_matches_git() {
    let base = tempfile::tempdir().expect("tempdir");
    init_repo(base.path());
    commit_file(base.path(), "a.txt", "v1\n", "initial");
    commit_file(base.path(), "a.txt", "v2\n", "bad");
    let bad_oid = ObjectId::from_hex(git(base.path(), &["rev-parse", "HEAD"]).trim()).expect("oid");

    let grit_dir = tempfile::tempdir().expect("grit copy");
    copy_repo(base.path(), grit_dir.path());
    replay_commit(
        &open_repo(grit_dir.path()),
        &ReplayRequest {
            commit: bad_oid,
            direction: ReplayDirection::Revert,
            committer: IDENT.to_owned(),
            message_override: None,
        },
    )
    .expect("revert");

    let git_dir = tempfile::tempdir().expect("git copy");
    copy_repo(base.path(), git_dir.path());
    git(git_dir.path(), &["revert", "--no-edit", &bad_oid.to_hex()]);

    assert_eq!(
        head_tree_hex(grit_dir.path()),
        head_tree_hex(git_dir.path())
    );
    assert_eq!(
        fs::read_to_string(grit_dir.path().join("a.txt")).expect("grit a"),
        fs::read_to_string(git_dir.path().join("a.txt")).expect("git a"),
    );
    assert_eq!(
        index_stage0_fingerprint(grit_dir.path()),
        git_index_stage0_fingerprint(git_dir.path())
    );
    assert_git_index_matches_head_and_clean(grit_dir.path());
}

#[test]
fn s7_differential_merge_matches_git() {
    let base = tempfile::tempdir().expect("tempdir");
    init_repo(base.path());
    commit_file(base.path(), "base.txt", "base\n", "base");
    git(base.path(), &["switch", "-c", "feature"]);
    commit_file(base.path(), "feat.txt", "f\n", "feature");
    let feature = git(base.path(), &["rev-parse", "HEAD"]).trim().to_owned();
    git(base.path(), &["switch", "main"]);
    commit_file(base.path(), "main.txt", "m\n", "main");
    let main = git(base.path(), &["rev-parse", "HEAD"]).trim().to_owned();

    let grit_dir = tempfile::tempdir().expect("grit copy");
    copy_repo(base.path(), grit_dir.path());
    let grit_repo = open_repo(grit_dir.path());
    integrate_branch(
        &grit_repo,
        "refs/heads/main",
        ObjectId::from_hex(&main).expect("main"),
        ObjectId::from_hex(&feature).expect("feature"),
        "feature",
    )
    .expect("merge");

    let git_dir = tempfile::tempdir().expect("git copy");
    copy_repo(base.path(), git_dir.path());
    git(git_dir.path(), &["merge", "--no-edit", "feature"]);

    assert_eq!(
        head_tree_hex(grit_dir.path()),
        head_tree_hex(git_dir.path())
    );
    assert_eq!(
        worktree_fingerprint(grit_dir.path()),
        worktree_fingerprint(git_dir.path())
    );
    assert_eq!(
        index_stage0_fingerprint(grit_dir.path()),
        git_index_stage0_fingerprint(git_dir.path())
    );
    assert_git_index_matches_head_and_clean(grit_dir.path());
}

#[test]
fn s7_differential_pick_refuses_dirty_worktree() {
    let base = tempfile::tempdir().expect("tempdir");
    init_repo(base.path());
    commit_file(base.path(), "a.txt", "base\n", "base");
    git(base.path(), &["switch", "-c", "topic"]);
    commit_file(base.path(), "b.txt", "topic\n", "topic");
    let pick_source =
        ObjectId::from_hex(git(base.path(), &["rev-parse", "HEAD"]).trim()).expect("oid");
    git(base.path(), &["switch", "main"]);
    commit_file(base.path(), "c.txt", "main\n", "main");

    let grit_dir = tempfile::tempdir().expect("grit copy");
    copy_repo(base.path(), grit_dir.path());
    fs::write(grit_dir.path().join("a.txt"), "dirty\n").expect("dirty");
    let err = replay_commit(
        &open_repo(grit_dir.path()),
        &ReplayRequest {
            commit: pick_source,
            direction: ReplayDirection::Pick,
            committer: IDENT.to_owned(),
            message_override: None,
        },
    )
    .expect_err("dirty");
    assert!(
        matches!(
            &err,
            grit_lib::error::Error::DirtyWorktree {
                operation: grit_lib::error::WorktreeOperation::Pick,
                paths,
            } if paths == &["a.txt"]
        ),
        "{err:?}"
    );
}

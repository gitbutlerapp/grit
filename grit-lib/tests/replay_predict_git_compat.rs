//! In-memory replay prediction and pick merge options vs system `git`.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::fs;
use std::path::Path;
use std::process::Command;
use std::time::SystemTime;

use grit_lib::config::environment::RepositoryOptions;
use grit_lib::diff::merge_file::{ConflictStyle, MergeFavor};
use grit_lib::objects::ObjectId;
use grit_lib::porcelain::replay::ReplayDirection;
use grit_lib::porcelain::replay_sequence::{
    predict_replay_sequence, start_replay_sequence, ReplayOptions, ReplaySequenceRequest,
    SequenceOutcome, SequencePrediction,
};
use grit_lib::repo::Repository;

const IDENT: &str = "Test User <t@example.com> 1700000000 +0000";

fn git(root: &Path, args: &[&str]) -> String {
    let home = root.join(".git-home");
    let _ = fs::create_dir_all(&home);
    let out = Command::new("git")
        .current_dir(root)
        .args(args)
        .env("HOME", &home)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .output()
        .expect("git spawn");
    assert!(
        out.status.success(),
        "git {:?} failed: {}",
        args,
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).into_owned()
}

fn init_repo(root: &Path) {
    git(root, &["init", "-q", "-b", "main", "."]);
    git(root, &["config", "user.email", "t@example.com"]);
    git(root, &["config", "user.name", "Test"]);
    git(root, &["config", "grit.changeIds", "false"]);
    git(root, &["config", "commit.gpgsign", "false"]);
}

fn open_repo(root: &Path) -> Repository {
    Repository::open(
        &root.join(".git"),
        &RepositoryOptions::empty().with_work_tree(root),
    )
    .expect("open")
}

fn commit_file(root: &Path, path: &str, contents: &str, msg: &str) -> ObjectId {
    let full = root.join(path);
    if let Some(parent) = full.parent() {
        fs::create_dir_all(parent).expect("mkdir");
    }
    fs::write(&full, contents).expect("write");
    git(root, &["add", path]);
    git(root, &["commit", "-qm", msg]);
    oid_from_rev(root, "HEAD")
}

fn oid_from_rev(root: &Path, rev: &str) -> ObjectId {
    let hex = git(root, &["rev-parse", rev]).trim().to_owned();
    ObjectId::from_hex(&hex).expect("oid")
}

fn head_tree(_repo: &Repository, root: &Path) -> ObjectId {
    let hex = git(root, &["rev-parse", "HEAD^{tree}"]).trim().to_owned();
    ObjectId::from_hex(&hex).expect("tree oid")
}

fn count_loose_objects(git_dir: &Path) -> usize {
    let objects = git_dir.join("objects");
    let mut count = 0usize;
    if let Ok(entries) = fs::read_dir(&objects) {
        for entry in entries.flatten() {
            let name = entry.file_name();
            let name = name.to_string_lossy();
            if name.len() != 2 || name == "pack" || name == "info" {
                continue;
            }
            if let Ok(sub) = fs::read_dir(entry.path()) {
                count += sub.flatten().count();
            }
        }
    }
    count
}

fn index_mtime(git_dir: &Path) -> SystemTime {
    fs::metadata(git_dir.join("index"))
        .expect("index metadata")
        .modified()
        .expect("index mtime")
}

fn pick_req(commits: Vec<ObjectId>, opts: ReplayOptions) -> ReplaySequenceRequest {
    ReplaySequenceRequest {
        commits,
        direction: ReplayDirection::Pick,
        committer: IDENT.to_owned(),
        options: opts,
    }
}

#[test]
fn predict_pick_range_first_conflict() {
    let root = tempfile::tempdir().expect("tempdir");
    init_repo(root.path());
    commit_file(root.path(), "base.txt", "base\n", "base");
    git(root.path(), &["switch", "-c", "topic"]);
    commit_file(root.path(), "b.txt", "b\n", "add b");
    commit_file(root.path(), "f.txt", "topic\n", "topic f");
    let c1 = oid_from_rev(root.path(), "topic~1");
    let c2 = oid_from_rev(root.path(), "topic");
    git(root.path(), &["switch", "main"]);
    fs::write(root.path().join("f.txt"), "main\n").unwrap();
    git(root.path(), &["add", "f.txt"]);
    git(root.path(), &["commit", "-qm", "main f"]);

    let repo = open_repo(root.path());
    let prediction =
        predict_replay_sequence(&repo, &pick_req(vec![c1, c2], ReplayOptions::default()))
            .expect("predict");
    match prediction {
        SequencePrediction::Conflicts { commit, conflicts } => {
            assert_eq!(commit, c2);
            assert!(conflicts.iter().any(|c| c.path == "f.txt"));
        }
        other => panic!("expected conflict on second pick, got {other:?}"),
    }
}

#[test]
fn predict_replay_writes_no_objects() {
    let root = tempfile::tempdir().expect("tempdir");
    init_repo(root.path());
    commit_file(root.path(), "a.txt", "base\n", "base");
    git(root.path(), &["switch", "-c", "topic"]);
    commit_file(root.path(), "b.txt", "one\n", "one");
    let pick = oid_from_rev(root.path(), "HEAD");
    git(root.path(), &["switch", "main"]);
    commit_file(root.path(), "c.txt", "main\n", "main");

    let repo = open_repo(root.path());
    let git_dir = repo.git_dir();
    let loose_before = count_loose_objects(git_dir);
    let index_before = index_mtime(git_dir);

    predict_replay_sequence(&repo, &pick_req(vec![pick], ReplayOptions::default()))
        .expect("predict");

    assert_eq!(count_loose_objects(git_dir), loose_before);
    assert_eq!(index_mtime(git_dir), index_before);
}

#[test]
fn x_theirs_pick_matches_git_cherry_pick_X_theirs_tree() {
    let root = tempfile::tempdir().expect("tempdir");
    init_repo(root.path());
    commit_file(root.path(), "conflict.txt", "base\n", "base");
    git(root.path(), &["switch", "-c", "topic"]);
    commit_file(root.path(), "conflict.txt", "topic\n", "topic side");
    let pick = oid_from_rev(root.path(), "HEAD");
    git(root.path(), &["switch", "main"]);
    commit_file(root.path(), "conflict.txt", "main\n", "main side");

    let git_wt = tempfile::tempdir().expect("git copy");
    fs::create_dir_all(git_wt.path()).unwrap();
    Command::new("cp")
        .args([
            "-a",
            &format!("{}/.", root.path().display()),
            &git_wt.path().to_string_lossy(),
        ])
        .status()
        .unwrap();
    git(
        git_wt.path(),
        &["cherry-pick", "-X", "theirs", &pick.to_hex()],
    );

    let repo = open_repo(root.path());
    let opts = ReplayOptions {
        favor: MergeFavor::Theirs,
        ..ReplayOptions::default()
    };
    let out = start_replay_sequence(&repo, &pick_req(vec![pick], opts)).expect("pick");
    assert!(matches!(out, SequenceOutcome::Completed { .. }));

    assert_eq!(
        head_tree(&repo, root.path()),
        head_tree(&open_repo(git_wt.path()), git_wt.path())
    );
}

#[test]
fn diff3_pick_markers_byte_equal_git() {
    let root = tempfile::tempdir().expect("tempdir");
    init_repo(root.path());
    git(root.path(), &["config", "merge.conflictStyle", "diff3"]);
    commit_file(root.path(), "f.txt", "base\n", "base");
    git(root.path(), &["switch", "-c", "topic"]);
    fs::write(root.path().join("f.txt"), "topic\n").unwrap();
    git(root.path(), &["commit", "-qam", "topic"]);
    let pick = oid_from_rev(root.path(), "HEAD");
    git(root.path(), &["switch", "main"]);
    fs::write(root.path().join("f.txt"), "main\n").unwrap();
    git(root.path(), &["commit", "-qam", "main"]);

    let git_wt = tempfile::tempdir().expect("git copy");
    fs::create_dir_all(git_wt.path()).unwrap();
    Command::new("cp")
        .args([
            "-a",
            &format!("{}/.", root.path().display()),
            &git_wt.path().to_string_lossy(),
        ])
        .status()
        .unwrap();
    git(git_wt.path(), &["config", "merge.conflictStyle", "diff3"]);
    let cp = Command::new("git")
        .current_dir(git_wt.path())
        .args(["cherry-pick", &pick.to_hex()])
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .output()
        .expect("cherry-pick");
    assert!(!cp.status.success(), "expected git conflict");

    let repo = open_repo(root.path());
    let opts = ReplayOptions {
        conflict_style: ConflictStyle::Diff3,
        ..ReplayOptions::default()
    };
    let out = start_replay_sequence(&repo, &pick_req(vec![pick], opts)).expect("pick");
    assert!(matches!(out, SequenceOutcome::Conflicts { .. }));

    let grit_markers = fs::read_to_string(root.path().join("f.txt")).expect("grit f");
    let git_markers = fs::read_to_string(git_wt.path().join("f.txt")).expect("git f");
    assert_eq!(grit_markers, git_markers);
}

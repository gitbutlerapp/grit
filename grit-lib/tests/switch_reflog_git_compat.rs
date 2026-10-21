//! Switch and branch creation write Git-compatible reflogs (issue #979).

use grit_lib::diff::zero_oid;
use grit_lib::index::entry_from_stat;
use grit_lib::index::write_tree::{write_tree_update_index, WriteTreeFlags};
use grit_lib::objects::{serialize_commit, CommitData, ObjectId, ObjectKind};
use grit_lib::porcelain::branch::create_branch;
use grit_lib::porcelain::switch::{
    resolve_create_branch, resolve_revision_spec, resolve_switch_target, switch_repository,
    SwitchMode, SwitchOptions,
};
use grit_lib::porcelain::tag::{create_tag, CreateTagRequest};
use grit_lib::refs;
use grit_lib::refs::reflog::read_reflog;
use grit_lib::repo::{InitOptions, Repository};
use grit_lib::state::{resolve_head, HeadState};
use grit_lib::RefStorageFormat;
use std::os::unix::fs::PermissionsExt;
use std::process::Command;
use tempfile::TempDir;

fn init_repo() -> (TempDir, Repository) {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = Repository::init(dir.path(), &InitOptions::default()).expect("init");
    (dir, repo)
}

fn git_config(repo_path: &std::path::Path) {
    for args in [
        ["config", "user.email", "a@b.c"],
        ["config", "user.name", "A"],
    ] {
        Command::new("git")
            .args(args)
            .current_dir(repo_path)
            .status()
            .expect("git config");
    }
}

fn grit_commit(
    repo: &Repository,
    path: &str,
    contents: &str,
    parent: Option<ObjectId>,
) -> ObjectId {
    let work_tree = repo.work_tree().expect("wt");
    std::fs::write(work_tree.join(path), contents).expect("write");
    let mut index = repo.index_handle().load().expect("index");
    let oid = repo
        .odb()
        .write(ObjectKind::Blob, contents.as_bytes())
        .expect("blob");
    index.add_or_replace(
        entry_from_stat(
            &work_tree.join(path),
            path.as_bytes(),
            oid,
            grit_lib::index::MODE_REGULAR,
        )
        .expect("entry"),
    );
    index.sort();
    let tree = write_tree_update_index(repo.odb(), &mut index, "", WriteTreeFlags::silent())
        .expect("tree");
    repo.index_handle().write(&mut index).expect("write index");
    let commit = CommitData {
        tree,
        parents: parent.into_iter().collect(),
        author: "A <a@b.c> 0 +0000".into(),
        committer: "A <a@b.c> 0 +0000".into(),
        author_raw: vec![],
        committer_raw: vec![],
        encoding: None,
        message: "c\n".into(),
        raw_message: None,
        extra_headers: Vec::new(),
    };
    let co = repo
        .odb()
        .write(ObjectKind::Commit, &serialize_commit(&commit))
        .expect("commit");
    refs::write_ref(repo.git_dir(), "refs/heads/main", &co).expect("branch");
    refs::write_symbolic_ref(repo.git_dir(), "HEAD", "refs/heads/main").expect("head");
    co
}

fn grit_commit_to_ref(
    repo: &Repository,
    branch_ref: &str,
    path: &str,
    contents: &str,
    parent: Option<ObjectId>,
) -> ObjectId {
    let work_tree = repo.work_tree().expect("wt");
    std::fs::write(work_tree.join(path), contents).expect("write");
    let mut index = repo.index_handle().load().expect("index");
    let oid = repo
        .odb()
        .write(ObjectKind::Blob, contents.as_bytes())
        .expect("blob");
    index.add_or_replace(
        entry_from_stat(
            &work_tree.join(path),
            path.as_bytes(),
            oid,
            grit_lib::index::MODE_REGULAR,
        )
        .expect("entry"),
    );
    index.sort();
    let tree = write_tree_update_index(repo.odb(), &mut index, "", WriteTreeFlags::silent())
        .expect("tree");
    repo.index_handle().write(&mut index).expect("write index");
    let commit = CommitData {
        tree,
        parents: parent.into_iter().collect(),
        author: "A <a@b.c> 0 +0000".into(),
        committer: "A <a@b.c> 0 +0000".into(),
        author_raw: vec![],
        committer_raw: vec![],
        encoding: None,
        message: "c\n".into(),
        raw_message: None,
        extra_headers: Vec::new(),
    };
    let co = repo
        .odb()
        .write(ObjectKind::Commit, &serialize_commit(&commit))
        .expect("commit");
    refs::write_ref(repo.git_dir(), branch_ref, &co).expect("branch");
    co
}

fn git_reflog_lines(repo_path: &std::path::Path) -> Vec<String> {
    let out = Command::new("git")
        .args(["reflog"])
        .current_dir(repo_path)
        .output()
        .expect("git reflog");
    assert!(out.status.success(), "git reflog failed");
    String::from_utf8(out.stdout)
        .expect("utf8")
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .map(|l| l.to_owned())
        .collect()
}

fn git_reflog_show(repo_path: &std::path::Path, refname: &str) -> Vec<String> {
    let out = Command::new("git")
        .args(["reflog", "show", refname])
        .current_dir(repo_path)
        .output()
        .expect("git reflog show");
    assert!(out.status.success(), "git reflog show failed");
    String::from_utf8(out.stdout)
        .expect("utf8")
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .map(|l| l.to_owned())
        .collect()
}

fn run_repro_sequence(repo: &Repository) {
    let c1 = grit_commit(repo, "f", "1\n", None);
    let _c2 = grit_commit(repo, "f", "1\n2\n", Some(c1));

    switch_repository(
        repo,
        &SwitchOptions {
            mode: resolve_create_branch(repo, "topic", None).unwrap(),
        },
    )
    .expect("switch -c topic");

    let main_oid = refs::resolve_ref(repo.git_dir(), "refs/heads/main").unwrap();
    switch_repository(
        repo,
        &SwitchOptions {
            mode: SwitchMode::Existing {
                branch: "main".into(),
                oid: main_oid,
            },
        },
    )
    .expect("switch main");

    let detach = resolve_revision_spec(repo, "HEAD~1").unwrap();
    switch_repository(
        repo,
        &SwitchOptions {
            mode: SwitchMode::Detached {
                commit: detach,
                reflog_to_label: "HEAD~1".into(),
            },
        },
    )
    .expect("detach");

    switch_repository(
        repo,
        &SwitchOptions {
            mode: SwitchMode::Existing {
                branch: "main".into(),
                oid: main_oid,
            },
        },
    )
    .expect("switch main");

    create_branch(repo, "b2").expect("branch b2");
}

#[test]
fn detach_at_annotated_tag_matches_git() {
    let (dir, repo) = init_repo();
    git_config(dir.path());
    let tip = grit_commit(&repo, "a.txt", "hi\n", None);
    create_tag(
        &repo,
        &CreateTagRequest {
            name: "v1".into(),
            target: tip,
            message: Some("annotated".into()),
            tagger: Some("A <a@b.c> 0 +0000".into()),
            sign: None,
            signing_key: None,
        },
    )
    .expect("annotated tag");

    let commit = resolve_revision_spec(&repo, "v1").expect("resolve tag to commit");
    assert_eq!(commit, tip);

    switch_repository(
        &repo,
        &SwitchOptions {
            mode: SwitchMode::Detached {
                commit,
                reflog_to_label: "v1".into(),
            },
        },
    )
    .expect("grit switch --detach v1");

    let grit_head = match resolve_head(repo.git_dir()).expect("head") {
        HeadState::Detached { oid } => oid,
        other => panic!("expected detached HEAD, got {other:?}"),
    };
    assert_eq!(grit_head, tip);

    let git_head = Command::new("git")
        .args(["rev-parse", "HEAD"])
        .current_dir(dir.path())
        .output()
        .expect("git rev-parse");
    assert!(git_head.status.success(), "git rev-parse HEAD failed");
    let git_oid = String::from_utf8(git_head.stdout)
        .expect("utf8")
        .trim()
        .to_owned();
    assert_eq!(git_oid, tip.to_string());

    let fsck = Command::new("git")
        .args(["fsck"])
        .current_dir(dir.path())
        .output()
        .expect("git fsck");
    assert!(
        fsck.status.success(),
        "git fsck failed: {}",
        String::from_utf8_lossy(&fsck.stderr)
    );

    refs::write_symbolic_ref(repo.git_dir(), "HEAD", "refs/heads/main").expect("reattach");
    let git_switch = Command::new("git")
        .args(["switch", "--detach", "v1"])
        .current_dir(dir.path())
        .output()
        .expect("git switch --detach");
    assert!(
        git_switch.status.success(),
        "git switch --detach v1 failed: {}",
        String::from_utf8_lossy(&git_switch.stderr)
    );
    let git_detached = Command::new("git")
        .args(["rev-parse", "HEAD"])
        .current_dir(dir.path())
        .output()
        .expect("git rev-parse after switch");
    assert!(git_detached.status.success());
    assert_eq!(
        String::from_utf8(git_detached.stdout).expect("utf8").trim(),
        tip.to_string()
    );
}

#[test]
fn switch_sequence_reflog_matches_git() {
    let (dir, repo) = init_repo();
    git_config(dir.path());
    run_repro_sequence(&repo);

    let head_log = read_reflog(repo.git_dir(), "HEAD").expect("head reflog");
    let checkout_msgs: Vec<_> = head_log
        .iter()
        .filter_map(|e| {
            e.message
                .strip_prefix("checkout: moving from ")
                .map(|s| s.to_string())
        })
        .collect();
    assert_eq!(checkout_msgs.len(), 4);
    assert!(checkout_msgs.iter().any(|m| m.contains(" to HEAD~1")));

    let topic_log = read_reflog(repo.git_dir(), "refs/heads/topic").expect("topic log");
    assert_eq!(topic_log.len(), 1);
    assert_eq!(topic_log[0].message, "branch: Created from HEAD");

    let b2_log = read_reflog(repo.git_dir(), "refs/heads/b2").expect("b2 log");
    assert_eq!(b2_log.len(), 1);
    assert_eq!(b2_log[0].message, "branch: Created from main");

    let grit_lines = git_reflog_lines(dir.path());
    let checkout_lines: Vec<_> = grit_lines
        .iter()
        .filter(|l| l.contains("checkout: moving"))
        .collect();
    assert_eq!(checkout_lines.len(), 4);

    let topic_show = git_reflog_show(dir.path(), "topic");
    assert_eq!(topic_show.len(), 1);
    assert!(topic_show[0].contains("branch: Created from HEAD"));

    let b2_show = git_reflog_show(dir.path(), "b2");
    assert_eq!(b2_show.len(), 1);
    assert!(b2_show[0].contains("branch: Created from main"));
}

#[test]
fn switch_minus_and_at_minus_one_return_previous_branch() {
    let (_dir, repo) = init_repo();
    let c1 = grit_commit(&repo, "f", "1\n", None);
    let _c2 = grit_commit(&repo, "f", "1\n2\n", Some(c1));

    switch_repository(
        &repo,
        &SwitchOptions {
            mode: resolve_create_branch(&repo, "topic", None).unwrap(),
        },
    )
    .expect("topic");
    let main_oid = refs::resolve_ref(repo.git_dir(), "refs/heads/main").unwrap();
    switch_repository(
        &repo,
        &SwitchOptions {
            mode: SwitchMode::Existing {
                branch: "main".into(),
                oid: main_oid,
            },
        },
    )
    .expect("main");

    let mode_minus = resolve_switch_target(&repo, "-").expect("@{-1} via -");
    match &mode_minus {
        SwitchMode::Existing { branch, .. } => assert_eq!(branch, "topic"),
        other => panic!("expected existing topic, got {other:?}"),
    }

    switch_repository(&repo, &SwitchOptions { mode: mode_minus }).expect("switch -");

    switch_repository(
        &repo,
        &SwitchOptions {
            mode: SwitchMode::Existing {
                branch: "main".into(),
                oid: main_oid,
            },
        },
    )
    .expect("main again");

    let mode_at = resolve_switch_target(&repo, "@{-1}").expect("@{-1}");
    match mode_at {
        SwitchMode::Existing { branch, .. } => assert_eq!(branch, "topic"),
        other => panic!("expected topic, got {other:?}"),
    }
}

#[test]
fn checkout_appends_existing_head_reflog_when_log_all_ref_updates_false() {
    let (dir, repo) = init_repo();
    git_config(dir.path());
    std::fs::write(dir.path().join("f"), "1\n").expect("write");
    Command::new("git")
        .args(["add", "f"])
        .current_dir(dir.path())
        .status()
        .expect("git add");
    Command::new("git")
        .args(["commit", "-qm", "c1"])
        .current_dir(dir.path())
        .status()
        .expect("git commit");
    Command::new("git")
        .args(["config", "core.logAllRefUpdates", "false"])
        .current_dir(dir.path())
        .status()
        .expect("git config");

    let before = read_reflog(repo.git_dir(), "HEAD").expect("head log");
    assert_eq!(before.len(), 1, "git commit should seed HEAD reflog");

    switch_repository(
        &repo,
        &SwitchOptions {
            mode: resolve_create_branch(&repo, "topic", None).expect("create mode"),
        },
    )
    .expect("switch -c topic");

    let after = read_reflog(repo.git_dir(), "HEAD").expect("head log after");
    assert_eq!(
        after.len(),
        2,
        "checkout must append to existing HEAD reflog"
    );
    assert_eq!(after[1].message, "checkout: moving from main to topic");
    assert!(
        !dir.path().join(".git/logs/refs/heads/topic").exists(),
        "new branch reflog must not be auto-created when logAllRefUpdates=false"
    );
}

#[test]
fn same_branch_switch_appends_head_checkout_reflog() {
    let (_dir, repo) = init_repo();
    let c1 = grit_commit(&repo, "f", "1\n", None);
    let _c2 = grit_commit(&repo, "f", "1\n2\n", Some(c1));
    let main_oid = refs::resolve_ref(repo.git_dir(), "refs/heads/main").unwrap();

    switch_repository(
        &repo,
        &SwitchOptions {
            mode: SwitchMode::Existing {
                branch: "main".into(),
                oid: main_oid,
            },
        },
    )
    .expect("same-branch switch");

    let head_log = read_reflog(repo.git_dir(), "HEAD").expect("head log");
    assert!(
        head_log
            .iter()
            .any(|e| e.message == "checkout: moving from main to main"),
        "expected same-branch checkout reflog, got {:?}",
        head_log
            .iter()
            .map(|e| e.message.as_str())
            .collect::<Vec<_>>()
    );
}

#[test]
fn remote_dwim_create_uses_remote_ref_in_branch_reflog() {
    let (dir, repo) = init_repo();
    let tip = grit_commit(&repo, "f", "1\n", None);
    refs::write_ref(repo.git_dir(), "refs/remotes/origin/topic", &tip).expect("remote");

    let mode = resolve_switch_target(&repo, "topic").expect("resolve");
    match &mode {
        SwitchMode::Create {
            create_source_label,
            ..
        } => assert_eq!(create_source_label, "refs/remotes/origin/topic"),
        other => panic!("expected create from remote, got {other:?}"),
    }

    switch_repository(&repo, &SwitchOptions { mode }).expect("switch");

    let topic_log = read_reflog(repo.git_dir(), "refs/heads/topic").expect("topic log");
    assert_eq!(topic_log.len(), 1);
    assert_eq!(
        topic_log[0].message,
        "branch: Created from refs/remotes/origin/topic"
    );

    let config = repo.config().expect("config");
    assert_eq!(config.get("branch.topic.remote").as_deref(), Some("origin"));
    assert_eq!(
        config.get("branch.topic.merge").as_deref(),
        Some("refs/heads/topic")
    );

    let git_remote = Command::new("git")
        .current_dir(dir.path())
        .args(["config", "--get", "branch.topic.remote"])
        .output()
        .expect("git config remote");
    assert!(git_remote.status.success());
    assert_eq!(String::from_utf8_lossy(&git_remote.stdout).trim(), "origin");

    let git_merge = Command::new("git")
        .current_dir(dir.path())
        .args(["config", "--get", "branch.topic.merge"])
        .output()
        .expect("git config merge");
    assert!(git_merge.status.success());
    assert_eq!(
        String::from_utf8_lossy(&git_merge.stdout).trim(),
        "refs/heads/topic"
    );
}

#[test]
fn unborn_symbolic_head_switch_writes_zero_to_tip_checkout_reflog() {
    let (dir, repo) = init_repo();
    git_config(dir.path());
    refs::write_symbolic_ref(repo.git_dir(), "HEAD", "refs/heads/main").expect("symhead");
    let _ = std::fs::remove_file(repo.git_dir().join("refs/heads/main"));
    let head = resolve_head(repo.git_dir()).expect("head");
    assert!(matches!(
        head,
        HeadState::Branch {
            oid: None,
            short_name,
            ..
        } if short_name == "main"
    ));

    let tip = grit_commit_to_ref(&repo, "refs/heads/feature", "f", "1\n", None);

    switch_repository(
        &repo,
        &SwitchOptions {
            mode: SwitchMode::Existing {
                branch: "feature".into(),
                oid: tip,
            },
        },
    )
    .expect("switch to feature");

    let head_after = resolve_head(repo.git_dir()).expect("head after");
    assert!(matches!(
        head_after,
        HeadState::Branch {
            ref refname,
            oid: Some(oid),
            ..
        } if refname == "refs/heads/feature" && oid == tip
    ));

    let head_log = read_reflog(repo.git_dir(), "HEAD").expect("head log");
    assert_eq!(head_log.len(), 1);
    assert!(head_log[0].old_oid.is_zero() || head_log[0].old_oid == zero_oid());
    assert_eq!(head_log[0].new_oid, tip);
    assert_eq!(head_log[0].message, "checkout: moving from main to feature");

    let git_show = Command::new("git")
        .args(["reflog", "-1", "--format=%gs"])
        .current_dir(dir.path())
        .output()
        .expect("git reflog");
    assert!(git_show.status.success());
    let line = String::from_utf8(git_show.stdout).expect("utf8");
    assert_eq!(line.trim(), "checkout: moving from main to feature");
}

#[test]
fn create_branch_leaves_no_ref_when_reflog_dir_not_writable() {
    let (dir, repo) = init_repo();
    let _tip = grit_commit(&repo, "f", "1\n", None);
    let logs_heads = dir.path().join(".git/logs/refs/heads");
    std::fs::create_dir_all(&logs_heads).expect("logs dir");
    std::fs::set_permissions(&logs_heads, std::fs::Permissions::from_mode(0o500)).expect("chmod");

    let err = create_branch(&repo, "fail").expect_err("reflog should fail");
    assert!(!err.to_string().is_empty());

    std::fs::set_permissions(&logs_heads, std::fs::Permissions::from_mode(0o755))
        .expect("restore chmod");
    assert!(
        refs::resolve_ref(repo.git_dir(), "refs/heads/fail").is_err(),
        "branch ref must not exist when reflog write fails"
    );
}

//! Switch and branch creation write Git-compatible reflogs (issue #979).

use grit_lib::index::entry_from_stat;
use grit_lib::objects::{serialize_commit, CommitData, ObjectId, ObjectKind};
use grit_lib::porcelain::branch::create_branch;
use grit_lib::porcelain::switch::{
    resolve_create_branch, resolve_revision_spec, resolve_switch_target, switch_repository,
    SwitchMode, SwitchOptions,
};
use grit_lib::reflog::read_reflog;
use grit_lib::refs;
use grit_lib::repo::{init_repository, Repository};
use grit_lib::write_tree::{write_tree_update_index, WriteTreeFlags};
use grit_lib::RefStorageFormat;
use std::process::Command;
use tempfile::TempDir;

fn init_repo() -> (TempDir, Repository) {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo =
        init_repository(dir.path(), false, "main", None, RefStorageFormat::Files).expect("init");
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
    let wt = repo.work_tree.as_ref().expect("wt");
    std::fs::write(wt.join(path), contents).expect("write");
    let mut index = repo.load_index().expect("index");
    let oid = repo
        .odb
        .write(ObjectKind::Blob, contents.as_bytes())
        .expect("blob");
    index.add_or_replace(
        entry_from_stat(
            &wt.join(path),
            path.as_bytes(),
            oid,
            grit_lib::index::MODE_REGULAR,
        )
        .expect("entry"),
    );
    index.sort();
    let tree =
        write_tree_update_index(&repo.odb, &mut index, "", WriteTreeFlags::silent()).expect("tree");
    repo.write_index(&mut index).expect("write index");
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
        .odb
        .write(ObjectKind::Commit, &serialize_commit(&commit))
        .expect("commit");
    refs::write_ref(&repo.git_dir, "refs/heads/main", &co).expect("branch");
    refs::write_symbolic_ref(&repo.git_dir, "HEAD", "refs/heads/main").expect("head");
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
            create_start_point: None,
        },
    )
    .expect("switch -c topic");

    let main_oid = refs::resolve_ref(&repo.git_dir, "refs/heads/main").unwrap();
    switch_repository(
        repo,
        &SwitchOptions {
            mode: SwitchMode::Existing {
                branch: "main".into(),
                oid: main_oid,
            },
            create_start_point: None,
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
            create_start_point: None,
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
            create_start_point: None,
        },
    )
    .expect("switch main");

    create_branch(repo, "b2").expect("branch b2");
}

#[test]
fn switch_sequence_reflog_matches_git() {
    let (dir, repo) = init_repo();
    git_config(dir.path());
    run_repro_sequence(&repo);

    let head_log = read_reflog(&repo.git_dir, "HEAD").expect("head reflog");
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

    let topic_log = read_reflog(&repo.git_dir, "refs/heads/topic").expect("topic log");
    assert_eq!(topic_log.len(), 1);
    assert_eq!(topic_log[0].message, "branch: Created from HEAD");

    let b2_log = read_reflog(&repo.git_dir, "refs/heads/b2").expect("b2 log");
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
            create_start_point: None,
        },
    )
    .expect("topic");
    let main_oid = refs::resolve_ref(&repo.git_dir, "refs/heads/main").unwrap();
    switch_repository(
        &repo,
        &SwitchOptions {
            mode: SwitchMode::Existing {
                branch: "main".into(),
                oid: main_oid,
            },
            create_start_point: None,
        },
    )
    .expect("main");

    let mode_minus = resolve_switch_target(&repo, "-").expect("@{-1} via -");
    match &mode_minus {
        SwitchMode::Existing { branch, .. } => assert_eq!(branch, "topic"),
        other => panic!("expected existing topic, got {other:?}"),
    }

    switch_repository(
        &repo,
        &SwitchOptions {
            mode: mode_minus,
            create_start_point: None,
        },
    )
    .expect("switch -");

    switch_repository(
        &repo,
        &SwitchOptions {
            mode: SwitchMode::Existing {
                branch: "main".into(),
                oid: main_oid,
            },
            create_start_point: None,
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

    let before = read_reflog(&repo.git_dir, "HEAD").expect("head log");
    assert_eq!(before.len(), 1, "git commit should seed HEAD reflog");

    switch_repository(
        &repo,
        &SwitchOptions {
            mode: resolve_create_branch(&repo, "topic", None).expect("create mode"),
            create_start_point: None,
        },
    )
    .expect("switch -c topic");

    let after = read_reflog(&repo.git_dir, "HEAD").expect("head log after");
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

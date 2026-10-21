//! Regression for issue #980: missing-but-registered worktree paths must not get a second admin dir.

use grit_lib::config::environment::RepositoryOptions;
use grit_lib::config::git_path::path_for_disk_compare;
use grit_lib::objects::ObjectKind;
use grit_lib::repo::Repository;
use grit_lib::worktree::{
    add_linked_worktree, list_worktrees, prune_linked_worktrees, AddLinkedWorktreeOptions,
};
use std::process::Command;

fn git(dir: &std::path::Path, args: &[&str]) {
    let out = Command::new("git")
        .args(args)
        .current_dir(dir)
        .output()
        .expect("git");
    assert!(
        out.status.success(),
        "git {:?}: {}",
        args,
        String::from_utf8_lossy(&out.stderr)
    );
}

#[test]
fn missing_registered_worktree_rejects_duplicate_add() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();
    let main = root.join("main");
    std::fs::create_dir_all(&main).unwrap();
    git(&main, &["init", "-q", "-b", "main"]);
    std::fs::write(main.join("f"), "1\n").unwrap();
    git(&main, &["add", "f"]);
    git(&main, &["commit", "-qm", "init"]);

    let repo = Repository::discover(Some(&main), &RepositoryOptions::empty()).expect("discover");
    let wt = root.join("w9");
    add_linked_worktree(
        &repo,
        &AddLinkedWorktreeOptions {
            path: wt.clone(),
            branch: Some("feat9".into()),
            new_branch: true,
            detach: false,
            start_point: Some("HEAD".into()),
            force: false,
        },
    )
    .expect("first add");

    std::fs::remove_dir_all(&wt).expect("simulate user deleting worktree dir");

    let list = list_worktrees(&repo).expect("list");
    assert_eq!(list.len(), 2);
    let wt_key = path_for_disk_compare(&wt);
    assert!(list.iter().any(|e| e.path == wt_key && e.is_prunable));

    let err = add_linked_worktree(
        &repo,
        &AddLinkedWorktreeOptions {
            path: wt.clone(),
            branch: Some("feat10".into()),
            new_branch: true,
            detach: false,
            start_point: Some("HEAD".into()),
            force: false,
        },
    )
    .expect_err("must not register a second admin dir");
    assert!(err.to_string().contains("already registered"));

    let admins: Vec<_> = std::fs::read_dir(main.join(".git/worktrees"))
        .unwrap()
        .filter_map(|e| e.ok())
        .collect();
    assert_eq!(admins.len(), 1, "only one admin dir must remain");

    let entry = add_linked_worktree(
        &repo,
        &AddLinkedWorktreeOptions {
            path: wt.clone(),
            branch: Some("feat10".into()),
            new_branch: true,
            detach: false,
            start_point: Some("HEAD".into()),
            force: true,
        },
    )
    .expect("force add");
    assert_eq!(
        entry.head.branch_name(),
        Some("feat10"),
        "returned entry must reflect the new branch, not the stale registration"
    );

    let admins: Vec<_> = std::fs::read_dir(main.join(".git/worktrees"))
        .unwrap()
        .filter_map(|e| e.ok())
        .collect();
    assert_eq!(admins.len(), 1);

    let git_list = Command::new("git")
        .args(["worktree", "list"])
        .current_dir(&main)
        .output()
        .unwrap();
    assert!(git_list.status.success());
    let git_list_text = String::from_utf8_lossy(&git_list.stdout);
    let lines: Vec<_> = git_list_text.lines().filter(|l| l.contains("w9")).collect();
    assert_eq!(lines.len(), 1, "git must see a single w9 registration");
}

#[test]
fn forced_add_keeps_stale_registration_when_validation_fails() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();
    let main = root.join("main");
    std::fs::create_dir_all(&main).unwrap();
    git(&main, &["init", "-q", "-b", "main"]);
    std::fs::write(main.join("f"), "1\n").unwrap();
    git(&main, &["add", "f"]);
    git(&main, &["commit", "-qm", "init"]);

    let repo = Repository::discover(Some(&main), &RepositoryOptions::empty()).expect("discover");
    let wt = root.join("w9");
    add_linked_worktree(
        &repo,
        &AddLinkedWorktreeOptions {
            path: wt.clone(),
            branch: Some("feat9".into()),
            new_branch: true,
            detach: false,
            start_point: Some("HEAD".into()),
            force: false,
        },
    )
    .expect("first add");
    std::fs::remove_dir_all(&wt).unwrap();

    add_linked_worktree(
        &repo,
        &AddLinkedWorktreeOptions {
            path: wt.clone(),
            branch: Some("feat10".into()),
            new_branch: true,
            detach: false,
            start_point: Some("no-such-revision".into()),
            force: true,
        },
    )
    .expect_err("invalid revision must not drop stale registration");

    let admins: Vec<_> = std::fs::read_dir(main.join(".git/worktrees"))
        .unwrap()
        .filter_map(|e| e.ok())
        .collect();
    assert_eq!(admins.len(), 1);

    let git_list = Command::new("git")
        .args(["worktree", "list"])
        .current_dir(&main)
        .output()
        .unwrap();
    assert!(git_list.status.success());
    let git_list_text = String::from_utf8_lossy(&git_list.stdout);
    let rows: Vec<_> = git_list_text.lines().filter(|l| l.contains("w9")).collect();
    assert_eq!(rows.len(), 1);
    assert!(rows[0].contains("prunable"));
}

#[test]
fn forced_add_restores_stale_registration_when_checkout_fails() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();
    let main = root.join("main");
    std::fs::create_dir_all(&main).unwrap();
    git(&main, &["init", "-q", "-b", "main"]);
    std::fs::write(main.join("f"), "1\n").unwrap();
    git(&main, &["add", "f"]);
    git(&main, &["commit", "-qm", "init"]);

    let repo = Repository::discover(Some(&main), &RepositoryOptions::empty()).expect("discover");
    let wt = root.join("w9");
    add_linked_worktree(
        &repo,
        &AddLinkedWorktreeOptions {
            path: wt.clone(),
            branch: Some("feat9".into()),
            new_branch: true,
            detach: false,
            start_point: Some("HEAD".into()),
            force: false,
        },
    )
    .expect("first add");
    std::fs::remove_dir_all(&wt).unwrap();

    let missing_blob = "0101010101010101010101010101010101010101";
    let tree_body = format!("100644 blob {missing_blob}\tmissing.txt\n");
    let tree_oid = repo
        .odb()
        .write_loose_materialize(ObjectKind::Tree, tree_body.as_bytes())
        .expect("tree");
    let commit_body = format!(
        "tree {tree_oid}\nauthor T <t@example.com> 0 +0000\ncommitter T <t@example.com> 0 +0000\n\nbroken\n"
    );
    let bad_commit = repo
        .odb()
        .write_loose_materialize(ObjectKind::Commit, commit_body.as_bytes())
        .expect("commit");

    add_linked_worktree(
        &repo,
        &AddLinkedWorktreeOptions {
            path: wt.clone(),
            branch: Some("feat10".into()),
            new_branch: true,
            detach: false,
            start_point: Some(bad_commit.to_hex()),
            force: true,
        },
    )
    .expect_err("checkout must fail on missing blob");

    assert!(
        !wt.exists(),
        "failed replacement must not leave a worktree directory with a stale gitfile"
    );

    let admins: Vec<_> = std::fs::read_dir(main.join(".git/worktrees"))
        .unwrap()
        .filter_map(|e| e.ok())
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .collect();
    assert_eq!(admins.len(), 1);
    assert!(
        admins.iter().any(|n| n == "w9"),
        "expected basename admin reuse, got {admins:?}"
    );

    let list = list_worktrees(&repo).expect("list");
    let wt_key = path_for_disk_compare(&wt);
    let row = list
        .iter()
        .find(|e| e.path == wt_key)
        .expect("stale registration");
    assert!(row.is_prunable, "restored registration must stay prunable");
}

#[test]
fn prune_drops_missing_worktree_registration() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();
    let main = root.join("main");
    std::fs::create_dir_all(&main).unwrap();
    git(&main, &["init", "-q", "-b", "main"]);
    std::fs::write(main.join("f"), "1\n").unwrap();
    git(&main, &["add", "f"]);
    git(&main, &["commit", "-qm", "init"]);

    let repo = Repository::discover(Some(&main), &RepositoryOptions::empty()).expect("discover");
    let wt = root.join("gone");
    add_linked_worktree(
        &repo,
        &AddLinkedWorktreeOptions {
            path: wt.clone(),
            branch: Some("side".into()),
            new_branch: true,
            detach: false,
            start_point: Some("HEAD".into()),
            force: false,
        },
    )
    .unwrap();
    std::fs::remove_dir_all(&wt).unwrap();

    let pruned = prune_linked_worktrees(&repo).expect("prune");
    assert_eq!(pruned.len(), 1);
    assert_eq!(list_worktrees(&repo).unwrap().len(), 1);
}

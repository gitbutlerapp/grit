//! `worktree move` / `worktree repair` vs system Git.

use grit_lib::config::environment::RepositoryOptions;
use grit_lib::config::git_path::path_for_disk_compare;
use grit_lib::repo::Repository;
use grit_lib::worktree::{AddLinkedWorktreeOptions, MoveWorktreeOptions, WorktreeBranch};
use std::path::{Path, PathBuf};
use std::process::Command;

fn git(dir: &Path, args: &[&str]) {
    let out = Command::new("git")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
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

fn git_worktree_porcelain(main: &Path) -> String {
    git(main, &["worktree", "list", "--porcelain"]);
    String::from_utf8_lossy(
        &Command::new("git")
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_SYSTEM", "/dev/null")
            .args(["worktree", "list", "--porcelain"])
            .current_dir(main)
            .output()
            .unwrap()
            .stdout,
    )
    .into_owned()
}

fn git_fsck(main: &Path) {
    git(main, &["fsck"]);
}

fn seed_repo(main: &Path) -> Repository {
    std::fs::create_dir_all(main).unwrap();
    git(main, &["init", "-q", "-b", "main"]);
    std::fs::write(main.join("tracked.txt"), "1\n").unwrap();
    git(main, &["add", "tracked.txt"]);
    git(main, &["commit", "-qm", "seed"]);
    Repository::discover(Some(main), &RepositoryOptions::empty()).expect("discover")
}

fn add_linked(repo: &Repository, path: PathBuf, branch: &str) {
    repo.worktrees()
        .add(&AddLinkedWorktreeOptions {
            path,
            branch: Some(WorktreeBranch::Create(branch.to_owned())),
            start_point: Some("HEAD".to_owned()),
            force: false,
            lock: None,
            no_checkout: false,
            track: false,
        })
        .expect("add");
}

#[test]
fn worktree_move_then_git_status_and_worktree_list_agree() {
    let tmp = tempfile::tempdir().unwrap();
    let main = tmp.path().join("main");
    let repo = seed_repo(&main);
    let linked = tmp.path().join("linked");
    add_linked(&repo, linked.clone(), "linked");
    let dest = tmp.path().join("moved");
    repo.worktrees()
        .move_linked(&linked, &dest, &MoveWorktreeOptions::default())
        .expect("move");
    assert!(!linked.exists());
    git_fsck(&main);
    let porcelain = git_worktree_porcelain(&main);
    assert!(porcelain.contains(&user_path(&dest)));
    assert!(!porcelain.contains(&user_path(&linked)));
    git(&dest, &["status"]);
}

#[test]
fn worktree_move_refuses_main_locked_and_existing_destination() {
    let tmp = tempfile::tempdir().unwrap();
    let main = tmp.path().join("main");
    let repo = seed_repo(&main);
    let linked = tmp.path().join("linked");
    add_linked(&repo, linked.clone(), "linked");
    assert!(repo
        .worktrees()
        .move_linked(
            &main,
            &tmp.path().join("x"),
            &MoveWorktreeOptions::default()
        )
        .is_err());
    repo.worktrees()
        .lock(
            &linked,
            &grit_lib::repo_handles::LockWorktreeOptions {
                reason: Some("hold".into()),
            },
        )
        .unwrap();
    assert!(repo
        .worktrees()
        .move_linked(
            &linked,
            &tmp.path().join("m2"),
            &MoveWorktreeOptions::default()
        )
        .is_err());
    assert!(repo
        .worktrees()
        .move_linked(
            &linked,
            &tmp.path().join("m2"),
            &MoveWorktreeOptions {
                force: true,
                dry_run: false,
            }
        )
        .is_ok());
    repo.worktrees().unlock(&tmp.path().join("m2")).ok();
    std::fs::write(tmp.path().join("exists.txt"), b"x").unwrap();
    assert!(repo
        .worktrees()
        .move_linked(
            &tmp.path().join("m2"),
            &tmp.path().join("exists.txt"),
            &MoveWorktreeOptions::default()
        )
        .is_err());
}

#[test]
fn worktree_repair_fixes_moved_linked_tree_git_agrees() {
    let tmp = tempfile::tempdir().unwrap();
    let main = tmp.path().join("main");
    let repo = seed_repo(&main);
    let linked = tmp.path().join("orig");
    add_linked(&repo, linked.clone(), "side");
    let moved = tmp.path().join("moved");
    std::fs::rename(&linked, &moved).unwrap();
    let repairs = repo
        .worktrees()
        .repair(&[moved.as_path()], false)
        .expect("repair");
    assert!(!repairs.is_empty());
    git_fsck(&main);
    git(&moved, &["status"]);
    let porcelain = git_worktree_porcelain(&main);
    assert!(porcelain.contains(&user_path(&moved)));
}

#[test]
fn worktree_repair_no_paths_restores_broken_gitfile_without_touching_other_worktree() {
    let tmp = tempfile::tempdir().unwrap();
    let main = tmp.path().join("main");
    let repo = seed_repo(&main);
    let wt_a = tmp.path().join("tree-a");
    let wt_b = tmp.path().join("tree-b");
    add_linked(&repo, wt_a.clone(), "branch-a");
    add_linked(&repo, wt_b.clone(), "branch-b");

    let canon_a = path_for_disk_compare(&wt_a);
    let canon_b = path_for_disk_compare(&wt_b);
    let entries = repo.worktrees().list().expect("list");
    let admin_a = entries
        .iter()
        .find(|e| e.path == canon_a)
        .expect("admin a")
        .admin_dir
        .clone();
    let admin_b = entries
        .iter()
        .find(|e| e.path == canon_b)
        .expect("admin b")
        .admin_dir
        .clone();

    let broken_gitfile = std::fs::read_to_string(wt_b.join(".git")).unwrap();
    std::fs::write(wt_a.join(".git"), broken_gitfile).unwrap();

    let gitdir_b_before = std::fs::read_to_string(admin_b.join("gitdir")).unwrap();
    repo.worktrees().repair(&[], false).expect("repair all");
    repo.worktrees()
        .repair(&[], false)
        .expect("repair again (order-independent idempotence)");

    let gitdir_a = std::fs::read_to_string(admin_a.join("gitdir")).unwrap();
    let gitdir_b = std::fs::read_to_string(admin_b.join("gitdir")).unwrap();
    assert!(
        gitdir_a.contains(&user_path(&wt_a)),
        "admin A gitdir must point at tree-a: {gitdir_a}"
    );
    assert_eq!(
        gitdir_b, gitdir_b_before,
        "admin B gitdir must stay unchanged"
    );

    git_fsck(&main);
    let porcelain = git_worktree_porcelain(&main);
    assert!(porcelain.contains(&user_path(&wt_a)));
    assert!(porcelain.contains(&user_path(&wt_b)));
    assert_eq!(
        porcelain.matches("worktree ").count(),
        3,
        "main plus two linked worktrees: {porcelain}"
    );

    let branch_a = Command::new("git")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .args(["branch", "--show-current"])
        .current_dir(&wt_a)
        .output()
        .expect("branch a");
    assert_eq!(String::from_utf8_lossy(&branch_a.stdout).trim(), "branch-a");
    let branch_b = Command::new("git")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .args(["branch", "--show-current"])
        .current_dir(&wt_b)
        .output()
        .expect("branch b");
    assert_eq!(String::from_utf8_lossy(&branch_b.stdout).trim(), "branch-b");
}

#[test]
fn worktree_repair_no_paths_finds_sibling_moved_linked_tree() {
    let tmp = tempfile::tempdir().unwrap();
    let main = tmp.path().join("main");
    let repo = seed_repo(&main);
    let linked = tmp.path().join("orig");
    add_linked(&repo, linked.clone(), "side");
    let moved = tmp.path().join("moved");
    std::fs::rename(&linked, &moved).unwrap();
    let repairs = repo.worktrees().repair(&[], false).expect("repair all");
    assert!(!repairs.is_empty());
    git_fsck(&main);
    git(&moved, &["status"]);
    let porcelain = git_worktree_porcelain(&main);
    assert!(porcelain.contains(&user_path(&moved)));
    assert!(!porcelain.contains("prunable"));
}

#[test]
fn worktree_repair_fixes_moved_main_repo_git_agrees() {
    let tmp = tempfile::tempdir().unwrap();
    let main = tmp.path().join("main");
    let repo = seed_repo(&main);
    let linked = tmp.path().join("side");
    add_linked(&repo, linked.clone(), "side");
    let main_moved = tmp.path().join("mainmoved");
    let side_moved = tmp.path().join("sidemoved");
    std::fs::rename(&main, &main_moved).unwrap();
    std::fs::rename(&linked, &side_moved).unwrap();
    let repo_moved =
        Repository::discover(Some(&main_moved), &RepositoryOptions::empty()).expect("discover");
    let repairs = repo_moved
        .worktrees()
        .repair(&[side_moved.as_path()], false)
        .expect("repair");
    assert!(!repairs.is_empty());
    git_fsck(&main_moved);
    git(&side_moved, &["status"]);
    let porcelain = git_worktree_porcelain(&main_moved);
    assert!(porcelain.contains(&user_path(&side_moved)));
}

#[test]
fn worktree_move_repair_dry_run_changes_nothing() {
    let tmp = tempfile::tempdir().unwrap();
    let main = tmp.path().join("main");
    let repo = seed_repo(&main);
    let linked = tmp.path().join("linked");
    add_linked(&repo, linked.clone(), "linked");
    let before = std::fs::read_to_string(linked.join(".git")).unwrap();
    repo.worktrees()
        .move_linked(
            &linked,
            &tmp.path().join("dest"),
            &MoveWorktreeOptions {
                dry_run: true,
                ..Default::default()
            },
        )
        .expect("dry move");
    assert!(linked.exists());
    assert_eq!(
        before,
        std::fs::read_to_string(linked.join(".git")).unwrap()
    );
    let renamed = tmp.path().join("renamed");
    std::fs::rename(&linked, &renamed).unwrap();
    let gitdir_before = std::fs::read_to_string(
        main.join(".git/worktrees")
            .read_dir()
            .unwrap()
            .next()
            .unwrap()
            .unwrap()
            .path()
            .join("gitdir"),
    )
    .unwrap();
    let repairs = repo
        .worktrees()
        .repair(&[renamed.as_path()], true)
        .expect("dry repair");
    assert!(!repairs.is_empty());
    let gitdir_after = std::fs::read_to_string(
        main.join(".git/worktrees")
            .read_dir()
            .unwrap()
            .next()
            .unwrap()
            .unwrap()
            .path()
            .join("gitdir"),
    )
    .unwrap();
    assert_eq!(gitdir_before, gitdir_after);
}

#[test]
fn git_move_then_grit_repair_worktree_list_agrees() {
    let tmp = tempfile::tempdir().unwrap();
    let main = tmp.path().join("main");
    seed_repo(&main);
    let linked = tmp.path().join("linked");
    git(
        &main,
        &["worktree", "add", linked.to_str().unwrap(), "-b", "side"],
    );
    let moved = tmp.path().join("moved");
    git(
        &main,
        &[
            "worktree",
            "move",
            linked.to_str().unwrap(),
            moved.to_str().unwrap(),
        ],
    );
    // Break links as if only the directory moved on disk.
    let admin = std::fs::read_dir(main.join(".git/worktrees"))
        .unwrap()
        .next()
        .unwrap()
        .unwrap()
        .path();
    let dotgit = moved.join(".git");
    let gitdir = admin.join("gitdir");
    std::fs::write(&gitdir, format!("{}\n", linked.join(".git").display())).unwrap();
    std::fs::write(
        &dotgit,
        format!(
            "gitdir: {}\n",
            admin.display().to_string().replace('\\', "/")
        ),
    )
    .unwrap();
    let repo = Repository::discover(Some(&main), &RepositoryOptions::empty()).unwrap();
    repo.worktrees()
        .repair(&[moved.as_path()], false)
        .expect("repair");
    git_fsck(&main);
    git(&moved, &["status"]);
    let porcelain = git_worktree_porcelain(&main);
    assert!(porcelain.contains(&user_path(&moved)));
}

fn user_path(path: &Path) -> String {
    grit_lib::config::git_path::user_display_path(
        &grit_lib::config::git_path::path_for_disk_compare(path),
    )
}

#[cfg(unix)]
fn paths_on_different_devices(a: &Path, b: &Path) -> bool {
    use std::os::unix::fs::MetadataExt;
    let parent_a = a.parent().filter(|p| p.exists()).unwrap_or(a);
    let parent_b = b.parent().filter(|p| p.exists()).unwrap_or(b);
    let Ok(ma) = std::fs::metadata(parent_a) else {
        return false;
    };
    let Ok(mb) = std::fs::metadata(parent_b) else {
        return false;
    };
    ma.dev() != mb.dev()
}

/// Cross-device rename uses copy+remove; symlinks must stay symlinks (including dangling).
#[test]
#[cfg(unix)]
fn worktree_move_cross_device_preserves_tracked_and_dangling_symlinks() {
    let alt_root = PathBuf::from("/dev/shm");
    if !alt_root.is_dir() {
        return;
    }
    let tmp = tempfile::tempdir().unwrap();
    let dest_parent = alt_root.join(format!(
        "grit-wt-move-{}-{}",
        std::process::id(),
        tmp.path()
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| "wt".into())
    ));
    if !paths_on_different_devices(tmp.path(), &dest_parent) {
        let _ = std::fs::remove_dir_all(&dest_parent);
        return;
    }
    std::fs::create_dir_all(&dest_parent).unwrap();

    let main = tmp.path().join("main");
    let repo = seed_repo(&main);
    let linked = tmp.path().join("linked");
    add_linked(&repo, linked.clone(), "linked");

    std::os::unix::fs::symlink("tracked.txt", linked.join("tracked-link")).unwrap();
    git(&linked, &["add", "tracked-link"]);
    git(&linked, &["commit", "-qm", "symlink"]);
    std::os::unix::fs::symlink("missing-target", linked.join("dangling-link")).unwrap();

    let dest = dest_parent.join("moved-wt");
    repo.worktrees()
        .move_linked(&linked, &dest, &MoveWorktreeOptions::default())
        .expect("cross-device move");

    assert!(dest.join("tracked-link").is_symlink());
    assert!(dest.join("dangling-link").is_symlink());
    assert_eq!(
        std::fs::read_link(dest.join("tracked-link")).unwrap(),
        PathBuf::from("tracked.txt")
    );
    assert_eq!(
        std::fs::read_link(dest.join("dangling-link")).unwrap(),
        PathBuf::from("missing-target")
    );

    let status = Command::new("git")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .args(["status", "--porcelain"])
        .current_dir(&dest)
        .output()
        .expect("git status");
    assert!(
        status.status.success(),
        "git status: {}",
        String::from_utf8_lossy(&status.stderr)
    );
    let porcelain = String::from_utf8_lossy(&status.stdout);
    assert!(
        !porcelain.contains("tracked-link"),
        "tracked symlink must not show as modified: {porcelain}"
    );
    git_fsck(&main);
    let _ = std::fs::remove_dir_all(&dest_parent);
}

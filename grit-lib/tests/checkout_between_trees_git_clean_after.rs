//! After `checkout_between_trees`, system `git` sees a clean index and worktree.

use std::process::Command;

use grit_lib::objects::ObjectId;
use grit_lib::porcelain::checkout::checkout_between_trees;
use grit_lib::refs;
use grit_lib::repo::Repository;
use grit_test_support::git;

const FILE_COUNT: usize = 3000;

fn git_porcelain(repo: &std::path::Path) -> String {
    git(repo, &["status", "--porcelain"])
}

fn git_diff_files(repo: &std::path::Path) -> String {
    git(repo, &["diff-files"])
}

fn git_fsck(repo: &std::path::Path) {
    let out = Command::new("git")
        .current_dir(repo)
        .args(["fsck", "--strict"])
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .output()
        .expect("git fsck");
    assert!(
        out.status.success(),
        "git fsck failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

fn tree_oid_of_head(repo: &std::path::Path) -> ObjectId {
    let hex = git(repo, &["rev-parse", "HEAD^{tree}"]).trim().to_owned();
    ObjectId::from_hex(&hex).expect("tree oid")
}

fn populate_initial(repo_root: &std::path::Path) {
    for i in 0..FILE_COUNT {
        let dir = repo_root.join(format!("d{:04}", i % 100));
        std::fs::create_dir_all(&dir).expect("mkdir");
        std::fs::write(dir.join(format!("f{i:05}.txt")), format!("seed {i}\n")).expect("write");
    }
    std::fs::write(repo_root.join("special1.txt"), b"file-before\n").expect("special1");
    std::fs::create_dir_all(repo_root.join("special2")).expect("special2 dir");
    std::fs::write(repo_root.join("special2/nested.txt"), b"nested\n").expect("special2 nested");
    git(repo_root, &["add", "-A"]);
    git(repo_root, &["commit", "-qm", "initial"]);
}

fn build_target_commit(repo_root: &std::path::Path) {
    for i in 0..FILE_COUNT {
        let dir = repo_root.join(format!("d{:04}", i % 100));
        let path = dir.join(format!("f{i:05}.txt"));
        if i % 17 == 0 {
            let _ = std::fs::remove_file(&path);
        } else if i % 13 == 0 {
            std::fs::write(&path, format!("changed {i}\n")).expect("modify");
        } else if i % 11 == 0 {
            std::fs::write(&path, format!("chmod {i}\n")).expect("modify");
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                let mut perms = std::fs::metadata(&path).expect("meta").permissions();
                perms.set_mode(0o755);
                std::fs::set_permissions(&path, perms).expect("chmod");
            }
        }
    }

    for i in 0..200 {
        let dir = repo_root.join(format!("added{:03}", i % 20));
        std::fs::create_dir_all(&dir).expect("mkdir");
        std::fs::write(dir.join(format!("new{i}.txt")), format!("added {i}\n")).expect("write");
    }

    std::fs::remove_file(repo_root.join("special1.txt")).expect("remove special1 file");
    std::fs::create_dir_all(repo_root.join("special1")).expect("special1 dir");
    std::fs::write(repo_root.join("special1/inner.txt"), b"inner\n").expect("special1 inner");

    let special2 = repo_root.join("special2");
    if special2.is_dir() {
        std::fs::remove_dir_all(&special2).expect("clear special2 tree");
    }
    std::fs::write(repo_root.join("special2"), b"now file\n").expect("dir to file");

    git(repo_root, &["add", "-A"]);
    git(repo_root, &["commit", "-qm", "target"]);
}

#[test]
fn checkout_between_trees_git_clean_after() {
    let tmp = tempfile::tempdir().expect("tempdir");
    git(tmp.path(), &["init", "-q", "-b", "main", "."]);
    git(tmp.path(), &["config", "user.email", "t@example.com"]);
    git(tmp.path(), &["config", "user.name", "Test"]);
    // Thousands of loose objects can trigger a detached `git gc --auto` after a commit, which
    // prunes loose objects while the final `git fsck` is reading them.
    git(tmp.path(), &["config", "gc.auto", "0"]);
    git(tmp.path(), &["config", "maintenance.auto", "false"]);

    populate_initial(tmp.path());
    let from_commit_hex = git(tmp.path(), &["rev-parse", "HEAD"]).trim().to_owned();
    let from_tree = tree_oid_of_head(tmp.path());

    build_target_commit(tmp.path());
    let to_commit_hex = git(tmp.path(), &["rev-parse", "HEAD"]).trim().to_owned();
    let to_tree = tree_oid_of_head(tmp.path());

    git(tmp.path(), &["reset", "--hard", &from_commit_hex]);

    let grit_repo =
        Repository::open(&tmp.path().join(".git"), Some(tmp.path())).expect("open grit repo");
    checkout_between_trees(&grit_repo, Some(&from_tree), &to_tree).expect("checkout");

    let to_commit = ObjectId::from_hex(&to_commit_hex).expect("commit oid");
    refs::write_ref(&grit_repo.git_dir, "HEAD", &to_commit).expect("head");

    assert!(
        git_porcelain(tmp.path()).trim().is_empty(),
        "git status --porcelain must be empty after grit checkout, got:\n{}",
        git_porcelain(tmp.path())
    );
    assert!(
        git_diff_files(tmp.path()).trim().is_empty(),
        "git diff-files must be empty (valid stat data), got:\n{}",
        git_diff_files(tmp.path())
    );
    git_fsck(tmp.path());
}

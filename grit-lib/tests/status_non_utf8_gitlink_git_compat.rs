//! Stage-0 gitlinks with non-UTF-8 index paths must not appear as untracked (issue #1091).

#![cfg(unix)]

use std::ffi::OsStr;
use std::os::unix::ffi::OsStrExt;
use std::process::Command;

use grit_lib::config::environment::RepositoryOptions;
use grit_lib::index::MODE_GITLINK;
use grit_lib::porcelain::status::{status, StatusOptions};
use grit_lib::progress::NeverCancel;
use grit_lib::progress::NullProgress;
use grit_lib::repo::Repository;
use grit_test_support::git;

const GITLINK_OID: &str = "855827c583bc30645ba427885caa40c5b81764d2";

fn git_porcelain(repo_root: &std::path::Path) -> String {
    let out = Command::new("git")
        .current_dir(repo_root)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .args(["status", "--porcelain"])
        .output()
        .expect("git status");
    assert!(
        out.status.success(),
        "git status: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).into_owned()
}

#[test]
fn non_utf8_gitlink_path_is_not_untracked() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let repo_root = tmp.path();
    git(repo_root, &["init", "-q", "-b", "main", "."]);
    git(repo_root, &["config", "user.email", "t@example.com"]);
    git(repo_root, &["config", "user.name", "Test"]);
    std::fs::write(repo_root.join("tracked.txt"), b"ok\n").expect("write tracked");
    git(repo_root, &["add", "tracked.txt"]);
    git(repo_root, &["commit", "-qm", "initial"]);

    let gitlink_path_bytes: &[u8] = b"sub-\xff";
    let gitlink_os = OsStr::from_bytes(gitlink_path_bytes);
    let gitlink_dir = repo_root.join(gitlink_os);
    std::fs::create_dir(&gitlink_dir).expect("gitlink worktree dir");
    std::fs::write(gitlink_dir.join("nested.txt"), b"inside gitlink\n")
        .expect("nonempty gitlink tree so untracked scan exercises the directory");
    let cacheinfo = format!("160000,{GITLINK_OID},");
    let mut cacheinfo_os = std::ffi::OsString::from(cacheinfo);
    cacheinfo_os.push(gitlink_os);
    let update = Command::new("git")
        .current_dir(repo_root)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .arg("update-index")
        .arg("--add")
        .arg("--cacheinfo")
        .arg(cacheinfo_os)
        .output()
        .expect("git update-index");
    assert!(
        update.status.success(),
        "git update-index: {}",
        String::from_utf8_lossy(&update.stderr)
    );
    git(repo_root, &["commit", "-qm", "record gitlink"]);

    let repo = Repository::open(
        &repo_root.join(".git"),
        &RepositoryOptions::empty().with_work_tree(repo_root),
    )
    .expect("open");
    let index = repo.index_handle().load().expect("load index");
    let entry = index
        .get(gitlink_path_bytes, 0)
        .expect("gitlink path bytes in index");
    assert_eq!(entry.mode, MODE_GITLINK);

    assert!(
        git_porcelain(repo_root).trim().is_empty(),
        "system git must treat committed non-UTF-8 gitlink as tracked, not untracked"
    );

    let model = status(
        &repo,
        &StatusOptions::default(),
        &mut NullProgress,
        &NeverCancel,
    )
    .expect("status");
    assert!(
        model.untracked.is_empty(),
        "non-UTF-8 gitlink directory must not be listed as untracked; got {:?}",
        model.untracked
    );
}

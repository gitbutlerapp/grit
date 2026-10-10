//! Checkout materializes uninitialized gitlinks as empty directories (issue #934).

use std::process::Command;

use grit_lib::diff::{diff_trees, DiffStatus};
use grit_lib::index::MODE_GITLINK;
use grit_lib::objects::ObjectId;
use grit_lib::porcelain::checkout::checkout_between_trees;
use grit_lib::repo::Repository;
use grit_test_support::git;

const GITLINK_OID: &str = "855827c583bc30645ba427885caa40c5b81764d2";

fn git_porcelain_worktree(git_dir: &std::path::Path, work_tree: &std::path::Path) -> String {
    let out = Command::new("git")
        .env("GIT_DIR", git_dir)
        .env("GIT_WORK_TREE", work_tree)
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
fn checkout_uninitialized_gitlink_matches_git_clone() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();
    git(root, &["init", "-q", "-b", "main", "gl"]);
    let gl = root.join("gl");
    git(&gl, &["config", "user.email", "a@x"]);
    git(&gl, &["config", "user.name", "a"]);
    std::fs::write(gl.join("a"), b"a\n").expect("write a");
    git(&gl, &["add", "a"]);
    git(
        &gl,
        &[
            "update-index",
            "--add",
            "--cacheinfo",
            &format!("160000,{GITLINK_OID},sub"),
        ],
    );
    git(&gl, &["commit", "-qm", "gl"]);
    git(root, &["clone", "-q", "--bare", "gl", "gl.git"]);

    let grit_dest = root.join("grit-clone");
    std::fs::create_dir_all(&grit_dest).expect("mkdir grit clone");
    let repo = Repository::open(&root.join("gl.git"), Some(&grit_dest)).expect("open bare+wt");
    let head_oid = grit_lib::refs::resolve_ref(&repo.git_dir, "HEAD").expect("head");
    let head_obj = repo.odb.read(&head_oid).expect("read");
    let commit = grit_lib::objects::parse_commit(&head_obj.data).expect("commit");
    checkout_between_trees(&repo, None, &commit.tree).expect("checkout with gitlink");

    let sub = grit_dest.join("sub");
    assert!(
        sub.is_dir(),
        "gitlink path should be a directory, not a file: {:?}",
        sub.symlink_metadata()
    );
    let git_meta = sub.join(".git");
    assert!(
        !git_meta.exists(),
        "uninitialized gitlink must not have nested .git"
    );

    let index = repo.load_index().expect("index");
    let entry = index.get(b"sub", 0).expect("sub in index");
    assert_eq!(entry.mode, MODE_GITLINK);
    assert_eq!(entry.oid.to_string(), GITLINK_OID);

    let porcelain = git_porcelain_worktree(&root.join("gl.git"), &grit_dest);
    assert!(
        porcelain.trim().is_empty(),
        "git status should be clean after grit checkout: {porcelain:?}"
    );

    let git_ref = root.join("ok-git");
    git(root, &["clone", "-q", "gl.git", "ok-git"]);
    assert!(git_ref.join("sub").is_dir());
    assert_eq!(
        git(&git_ref, &["status", "--porcelain"]).trim(),
        "",
        "reference git clone should also be clean"
    );
}

#[test]
fn checkout_removes_uninitialized_gitlink_on_delete() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();
    git(root, &["init", "-q", "-b", "main", "wt"]);
    let wt = root.join("wt");
    git(&wt, &["config", "user.email", "a@x"]);
    git(&wt, &["config", "user.name", "a"]);
    std::fs::write(wt.join("a"), b"a\n").expect("write");
    git(&wt, &["add", "a"]);
    git(
        &wt,
        &[
            "update-index",
            "--add",
            "--cacheinfo",
            &format!("160000,{GITLINK_OID},sub"),
        ],
    );
    git(&wt, &["commit", "-qm", "with-sub"]);
    let with_sub_commit = git(&wt, &["rev-parse", "HEAD"]).trim().to_owned();
    let with_sub_tree =
        ObjectId::from_hex(git(&wt, &["rev-parse", "HEAD^{tree}"]).trim()).expect("tree");

    git(&wt, &["rm", "--cached", "sub"]);
    std::fs::write(wt.join("b"), b"b\n").expect("write b");
    git(&wt, &["add", "b"]);
    git(&wt, &["commit", "-qm", "no-sub"]);
    let no_sub_tree =
        ObjectId::from_hex(git(&wt, &["rev-parse", "HEAD^{tree}"]).trim()).expect("tree");

    git(&wt, &["reset", "--hard", &with_sub_commit]);
    assert!(
        wt.join("sub").is_dir(),
        "git reset should materialize gitlink dir"
    );

    let repo = Repository::open(&wt.join(".git"), Some(&wt)).expect("open");
    let changes =
        diff_trees(&repo.odb, Some(&with_sub_tree), Some(&no_sub_tree), "").expect("diff");
    let sub_delete = changes
        .iter()
        .find(|c| c.status == DiffStatus::Deleted && c.old_path.as_deref() == Some("sub"))
        .unwrap_or_else(|| panic!("deleted sub in diff; got {changes:?}"));
    assert_eq!(
        sub_delete.old_mode, "160000",
        "gitlink delete should carry mode 160000"
    );
    checkout_between_trees(&repo, Some(&with_sub_tree), &no_sub_tree)
        .expect("checkout delete gitlink");

    assert!(
        !wt.join("sub").exists(),
        "deleted gitlink directory should be removed"
    );
    assert!(wt.join("b").is_file());
    let index = repo.load_index().expect("index");
    assert!(index.get(b"sub", 0).is_none());
    assert!(index.get(b"b", 0).is_some());
}

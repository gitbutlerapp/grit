//! `stash push --staged`, `--keep-index`, and `--dry-run` vs system `git`.

use grit_lib::config::environment::RepositoryOptions;
use grit_lib::objects::ObjectId;
use grit_lib::porcelain::stash::{preview_stash, push_stash, StashCreateOptions, StashMode};
use grit_lib::repo::Repository;
use std::path::{Path, PathBuf};
use std::process::Command;

fn git_cmd(repo: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .args(args)
        .current_dir(repo)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .env("GIT_AUTHOR_NAME", "Test")
        .env("GIT_AUTHOR_EMAIL", "t@example.com")
        .env("GIT_COMMITTER_NAME", "Test")
        .env("GIT_COMMITTER_EMAIL", "t@example.com")
        .output()
        .expect("git");
    assert!(
        out.status.success(),
        "git {:?} failed: {}",
        args,
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).into_owned()
}

fn init_repo(root: &Path) {
    git_cmd(root, &["init", "-q", "-b", "main", "."]);
    git_cmd(root, &["config", "user.email", "t@example.com"]);
    git_cmd(root, &["config", "user.name", "Test"]);
}

fn open_grit(root: &Path) -> Repository {
    Repository::open(
        &root.join(".git"),
        &RepositoryOptions::empty().with_work_tree(root),
    )
    .expect("open grit repo")
}

fn stash_opts(mode: StashMode, include_untracked: bool) -> StashCreateOptions {
    StashCreateOptions {
        message: None,
        include_untracked,
        identity: "Test <t@example.com> 1000000000 +0000".to_owned(),
        selection: None,
        pathspecs: Vec::new(),
        dry_run: false,
        mode,
    }
}

fn porcelain_v2(root: &Path) -> String {
    git_cmd(root, &["status", "--porcelain=v2"])
        .trim()
        .to_owned()
}

fn index_tree_oid(root: &Path) -> ObjectId {
    let hex = git_cmd(root, &["write-tree"]).trim().to_owned();
    ObjectId::from_hex(&hex).expect("write-tree")
}

fn read_repo_file(root: &Path, rel: &str) -> Option<String> {
    let path = root.join(rel);
    std::fs::read_to_string(path).ok()
}

fn count_loose_objects(git_dir: &Path) -> usize {
    let objects = git_dir.join("objects");
    let mut n = 0usize;
    for ent in std::fs::read_dir(&objects).expect("objects dir") {
        let ent = ent.expect("entry");
        if ent.file_type().expect("type").is_dir() {
            let name = ent.file_name();
            let name = name.to_string_lossy();
            if name == "info" || name == "pack" {
                continue;
            }
            for obj in std::fs::read_dir(ent.path()).expect("shard") {
                n += 1;
                let _ = obj;
            }
        }
    }
    n
}

fn stash_ref_oid(root: &Path) -> Option<ObjectId> {
    let path = root.join(".git/refs/stash");
    let text = std::fs::read_to_string(path).ok()?;
    ObjectId::from_hex(text.trim()).ok()
}

fn setup_staged_fixture(root: &Path) {
    init_repo(root);
    std::fs::write(root.join("tracked.txt"), "base\n").expect("write");
    std::fs::write(root.join("other.txt"), "base\n").expect("write");
    git_cmd(root, &["add", "tracked.txt", "other.txt"]);
    git_cmd(root, &["commit", "-qm", "init"]);
    std::fs::write(root.join("tracked.txt"), "unstaged\n").expect("modify tracked");
    std::fs::write(root.join("staged_new.txt"), "staged\n").expect("new staged");
    git_cmd(root, &["add", "staged_new.txt"]);
}

fn setup_keep_index_fixture(root: &Path) {
    init_repo(root);
    std::fs::write(root.join("f.txt"), "base\n").expect("write");
    git_cmd(root, &["add", "f.txt"]);
    git_cmd(root, &["commit", "-qm", "init"]);
    std::fs::write(root.join("f.txt"), "unstaged\n").expect("unstaged");
    std::fs::write(root.join("staged.txt"), "new\n").expect("staged new");
    git_cmd(root, &["add", "staged.txt"]);
}

fn twin_dirs(tag: &str) -> (tempfile::TempDir, PathBuf, tempfile::TempDir, PathBuf) {
    let grit_tmp = tempfile::tempdir().expect("tempdir");
    let git_tmp = tempfile::TempDir::new().expect("tempdir");
    let grit_path = grit_tmp.path().join(tag);
    let git_path = git_tmp.path().join(tag);
    std::fs::create_dir_all(&grit_path).expect("mkdir");
    std::fs::create_dir_all(&git_path).expect("mkdir");
    (grit_tmp, grit_path, git_tmp, git_path)
}

#[test]
fn stash_staged_only_matches_git_trees() {
    let (_g, grit_root, _gi, git_root) = twin_dirs("staged");
    setup_staged_fixture(&grit_root);
    setup_staged_fixture(&git_root);

    let repo = open_grit(&grit_root);
    push_stash(&repo, &stash_opts(StashMode::StagedOnly, false))
        .expect("push")
        .expect("had staged changes");
    git_cmd(&git_root, &["stash", "push", "--staged"]);

    assert_eq!(
        porcelain_v2(&grit_root),
        porcelain_v2(&git_root),
        "post-push porcelain must match git stash --staged"
    );
    assert_eq!(
        index_tree_oid(&grit_root),
        index_tree_oid(&git_root),
        "index tree must match git"
    );
    assert_eq!(
        read_repo_file(&grit_root, "tracked.txt"),
        read_repo_file(&git_root, "tracked.txt")
    );
    assert!(!grit_root.join("staged_new.txt").exists());
    assert!(!git_root.join("staged_new.txt").exists());

    let show = git_cmd(&grit_root, &["stash", "show", "-p", "stash@{0}"]);
    let git_show = git_cmd(&git_root, &["stash", "show", "-p", "stash@{0}"]);
    assert_eq!(show.trim(), git_show.trim(), "git reads grit stash patch");
}

#[test]
fn stash_keep_index_leaves_index_and_worktree_like_git() {
    let (_g, grit_root, _gi, git_root) = twin_dirs("keep");
    setup_keep_index_fixture(&grit_root);
    setup_keep_index_fixture(&git_root);

    let repo = open_grit(&grit_root);
    push_stash(&repo, &stash_opts(StashMode::KeepIndex, false))
        .expect("push")
        .expect("had changes");
    git_cmd(&git_root, &["stash", "push", "--keep-index"]);

    assert_eq!(
        porcelain_v2(&grit_root),
        porcelain_v2(&git_root),
        "post-push porcelain must match git --keep-index"
    );
    assert_eq!(
        index_tree_oid(&grit_root),
        index_tree_oid(&git_root),
        "index tree must match git"
    );
    assert_eq!(
        read_repo_file(&grit_root, "f.txt"),
        read_repo_file(&git_root, "f.txt")
    );
    assert_eq!(
        read_repo_file(&grit_root, "staged.txt"),
        read_repo_file(&git_root, "staged.txt")
    );
}

#[test]
fn stash_keep_index_pop_matches_git() {
    let (_g, grit_root, _gi, git_root) = twin_dirs("keep-pop");
    setup_keep_index_fixture(&grit_root);
    setup_keep_index_fixture(&git_root);

    let repo = open_grit(&grit_root);
    push_stash(&repo, &stash_opts(StashMode::KeepIndex, false))
        .expect("push")
        .expect("had changes");
    git_cmd(&git_root, &["stash", "push", "--keep-index"]);

    git_cmd(&grit_root, &["stash", "pop"]);
    git_cmd(&git_root, &["stash", "pop"]);

    assert_eq!(
        porcelain_v2(&grit_root),
        porcelain_v2(&git_root),
        "after pop, porcelain must match git"
    );
}

#[test]
fn stash_dry_run_writes_no_objects_or_refs() {
    let dir = tempfile::tempdir().expect("tempdir");
    setup_staged_fixture(dir.path());
    let repo = open_grit(dir.path());
    let git_dir = dir.path().join(".git");
    let objects_before = count_loose_objects(&git_dir);
    let stash_before = stash_ref_oid(dir.path());

    let mut opts = stash_opts(StashMode::All, false);
    opts.dry_run = true;
    let preview = preview_stash(&repo, &opts)
        .expect("preview")
        .expect("would stash");
    assert!(
        preview.paths.iter().any(|p| p == "staged_new.txt"),
        "preview lists staged path"
    );

    assert_eq!(count_loose_objects(&git_dir), objects_before);
    assert_eq!(stash_ref_oid(dir.path()), stash_before);
    assert_eq!(
        porcelain_v2(dir.path()),
        git_cmd(dir.path(), &["status", "--porcelain=v2"]).trim(),
        "dry-run leaves worktree unchanged"
    );
}

#[test]
fn stash_staged_rejects_untracked_combo() {
    let dir = tempfile::tempdir().expect("tempdir");
    init_repo(dir.path());
    std::fs::write(dir.path().join("f"), "x\n").expect("write");
    git_cmd(dir.path(), &["add", "f"]);
    git_cmd(dir.path(), &["commit", "-qm", "i"]);
    git_cmd(dir.path(), &["add", "f"]);
    std::fs::write(dir.path().join("u.txt"), "u\n").expect("untracked");

    let repo = open_grit(dir.path());
    let err = push_stash(&repo, &stash_opts(StashMode::StagedOnly, true))
        .expect_err("staged + untracked");
    assert!(
        err.to_string().contains("include-untracked"),
        "expected staged/untracked conflict, got: {err}"
    );
}

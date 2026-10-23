//! Stash push path limits and `--index` apply parity with system `git`.

use grit_lib::error::Error;
use grit_lib::objects::{parse_commit, ObjectId};
use grit_lib::porcelain::stash::{
    apply_stash_entry, pop_stash_entry, push_stash, StashApplyOptions, StashCreateOptions,
};
use grit_lib::repo::Repository;
use grit_test_support::git;

const IDENTITY: &str = "Test <t@example.com> 1000000000 +0000";

fn git_cmd(repo: &std::path::Path, args: &[&str]) -> String {
    git(repo, args)
}

fn init_repo(root: &std::path::Path) {
    git_cmd(root, &["init", "-q", "-b", "main", "."]);
    git_cmd(root, &["config", "user.email", "t@example.com"]);
    git_cmd(root, &["config", "user.name", "Test"]);
}

fn open_grit(root: &std::path::Path) -> Repository {
    Repository::discover(
        Some(root),
        &grit_lib::config::environment::RepositoryOptions::empty(),
    )
    .expect("discover")
}

fn stash_opts(include_untracked: bool, pathspecs: Vec<String>) -> StashCreateOptions {
    StashCreateOptions {
        message: Some("pathspec".into()),
        include_untracked,
        identity: IDENTITY.into(),
        selection: None,
        pathspecs,
        dry_run: false,
    }
}

fn ls_files_stage(repo: &std::path::Path) -> String {
    git_cmd(repo, &["ls-files", "-s"])
}

fn worktree_status(repo: &std::path::Path) -> String {
    git_cmd(repo, &["status", "--porcelain"])
}

fn stash_parent_tree(repo: &Repository, stash_oid: &ObjectId, parent: usize) -> ObjectId {
    let obj = repo.odb().read(stash_oid).expect("stash");
    let commit = parse_commit(&obj.data).expect("parse");
    commit.parents[parent]
}

fn stash_w_tree(repo: &Repository, stash_oid: &ObjectId) -> ObjectId {
    let obj = repo.odb().read(stash_oid).expect("stash");
    parse_commit(&obj.data).expect("parse").tree
}

fn flat_tree_map(repo: &Repository, tree: &ObjectId) -> std::collections::BTreeMap<String, String> {
    use grit_lib::porcelain::stash::flatten_tree_full;
    let protect =
        grit_lib::path_validate::ProtectOptions::from_config(&*repo.config().expect("config"));
    let entries = flatten_tree_full(repo.odb(), tree, "", protect).expect("flatten");
    entries
        .into_iter()
        .map(|e| (e.path, e.oid.to_hex()))
        .collect()
}

fn setup_ab_fixture() -> (tempfile::TempDir, tempfile::TempDir) {
    let base = tempfile::tempdir().expect("tempdir");
    init_repo(base.path());
    std::fs::write(base.path().join("a.txt"), "a\n").expect("write");
    std::fs::write(base.path().join("b.txt"), "b\n").expect("write");
    git_cmd(base.path(), &["add", "."]);
    git_cmd(base.path(), &["commit", "-qm", "init"]);
    std::fs::write(base.path().join("a.txt"), "A\n").expect("modify a");
    std::fs::write(base.path().join("b.txt"), "B\n").expect("modify b");
    git_cmd(base.path(), &["add", "b.txt"]);

    let grit_dir = tempfile::tempdir().expect("grit copy");
    std::process::Command::new("cp")
        .args([
            "-a",
            &format!("{}/.", base.path().display()),
            &grit_dir.path().to_string_lossy(),
        ])
        .status()
        .expect("cp");
    (base, grit_dir)
}

#[test]
fn stash_push_paths_saves_only_matching_and_keeps_others() {
    let (_base, grit_dir) = setup_ab_fixture();
    let repo = open_grit(grit_dir.path());
    let pushed = push_stash(&repo, &stash_opts(false, vec!["a.txt".into()]))
        .expect("push")
        .expect("some");
    assert_eq!(worktree_status(grit_dir.path()).trim(), "M  b.txt");
    let w_tree = stash_w_tree(&repo, &pushed.oid);
    let w_map = flat_tree_map(&repo, &w_tree);
    assert!(w_map.contains_key("a.txt"));
    assert!(w_map.contains_key("b.txt"));
    let a_oid = ObjectId::from_hex(w_map["a.txt"].as_str()).expect("hex");
    let a_blob = repo.odb().read(&a_oid).expect("blob").data;
    assert_eq!(a_blob, b"A\n");
}

#[test]
fn stash_push_paths_with_untracked() {
    let dir = tempfile::tempdir().expect("tempdir");
    init_repo(dir.path());
    std::fs::write(dir.path().join("tracked.txt"), "t\n").expect("write");
    git_cmd(dir.path(), &["add", "tracked.txt"]);
    git_cmd(dir.path(), &["commit", "-qm", "init"]);
    std::fs::write(dir.path().join("tracked.txt"), "T\n").expect("modify");
    std::fs::write(dir.path().join("u.txt"), "u\n").expect("untracked");
    std::fs::write(dir.path().join("keep.txt"), "k\n").expect("other untracked");

    let repo = open_grit(dir.path());
    push_stash(&repo, &stash_opts(true, vec!["u.txt".into()]))
        .expect("push")
        .expect("some");

    assert!(dir.path().join("keep.txt").exists());
    assert!(!dir.path().join("u.txt").exists());
    assert_eq!(
        std::fs::read_to_string(dir.path().join("tracked.txt")).unwrap(),
        "T\n"
    );
}

#[test]
fn stash_push_paths_no_match_is_typed_error() {
    let dir = tempfile::tempdir().expect("tempdir");
    init_repo(dir.path());
    std::fs::write(dir.path().join("f.txt"), "x\n").expect("write");
    git_cmd(dir.path(), &["add", "f.txt"]);
    git_cmd(dir.path(), &["commit", "-qm", "init"]);

    let repo = open_grit(dir.path());
    let err =
        push_stash(&repo, &stash_opts(false, vec!["missing.txt".into()])).expect_err("no match");
    assert!(matches!(err, Error::PathspecNoMatch { .. }));
}

#[test]
fn grit_path_stash_matches_git_path_stash() {
    let (base, grit_dir) = setup_ab_fixture();
    let repo = open_grit(grit_dir.path());

    let grit_push = push_stash(&repo, &stash_opts(false, vec!["a.txt".into()]))
        .expect("grit push")
        .expect("oid");

    git_cmd(
        base.path(),
        &["stash", "push", "-m", "pathspec", "--", "a.txt"],
    );
    let git_stash = ObjectId::from_hex(git_cmd(base.path(), &["rev-parse", "refs/stash"]).trim())
        .expect("git stash oid");

    let git_repo = open_grit(base.path());
    fn tree_for_stash_parent(repo: &Repository, stash: &ObjectId, parent: usize) -> ObjectId {
        let parent_commit = stash_parent_tree(repo, stash, parent);
        let obj = repo.odb().read(&parent_commit).expect("read");
        parse_commit(&obj.data).expect("parse").tree
    }

    for (label, grit_tree, git_tree) in [
        (
            "W",
            stash_w_tree(&repo, &grit_push.oid),
            stash_w_tree(&git_repo, &git_stash),
        ),
        (
            "I",
            tree_for_stash_parent(&repo, &grit_push.oid, 1),
            tree_for_stash_parent(&git_repo, &git_stash, 1),
        ),
    ] {
        assert_eq!(
            flat_tree_map(&repo, &grit_tree),
            flat_tree_map(&git_repo, &git_tree),
            "{label} tree must match git"
        );
    }

    assert_eq!(
        worktree_status(grit_dir.path()),
        worktree_status(base.path()),
        "worktree/index after push"
    );
}

#[test]
fn stash_apply_index_restores_staged_state() {
    let base = tempfile::tempdir().expect("tempdir");
    init_repo(base.path());
    std::fs::write(base.path().join("s.txt"), "base\n").expect("write");
    git_cmd(base.path(), &["add", "s.txt"]);
    git_cmd(base.path(), &["commit", "-qm", "init"]);
    let head = git_cmd(base.path(), &["rev-parse", "HEAD"])
        .trim()
        .to_owned();

    std::fs::write(base.path().join("s.txt"), "staged\n").expect("modify");
    git_cmd(base.path(), &["add", "s.txt"]);
    let repo = open_grit(base.path());
    push_stash(&repo, &stash_opts(false, Vec::new()))
        .expect("push")
        .expect("some");

    let grit_dir = tempfile::tempdir().expect("grit copy");
    let git_dir = tempfile::tempdir().expect("git copy");
    for dest in [grit_dir.path(), git_dir.path()] {
        std::process::Command::new("cp")
            .args([
                "-a",
                &format!("{}/.", base.path().display()),
                &dest.to_string_lossy(),
            ])
            .status()
            .expect("cp");
        git_cmd(dest, &["reset", "--hard", &head]);
    }

    apply_stash_entry(
        &open_grit(grit_dir.path()),
        grit_dir.path(),
        0,
        &StashApplyOptions {
            restore_index: true,
        },
    )
    .expect("apply");
    git_cmd(git_dir.path(), &["stash", "apply", "--index", "stash@{0}"]);

    assert_eq!(
        ls_files_stage(grit_dir.path()),
        ls_files_stage(git_dir.path())
    );
}

#[test]
fn stash_pop_index_conflict_keeps_entry() {
    let dir = tempfile::tempdir().expect("tempdir");
    init_repo(dir.path());
    std::fs::write(dir.path().join("file"), "base\n").expect("write");
    std::fs::write(dir.path().join("other-file"), "other\n").expect("write");
    git_cmd(dir.path(), &["add", "."]);
    git_cmd(dir.path(), &["commit", "-qm", "init"]);

    std::fs::write(dir.path().join("other-file"), "test\n").expect("write");
    git_cmd(dir.path(), &["add", "other-file"]);
    std::fs::write(dir.path().join("file"), "to stash\n").expect("write");
    git_cmd(dir.path(), &["stash", "push", "-qm", "s"]);
    std::fs::write(dir.path().join("file"), "unrelated\n").expect("write");
    std::fs::write(dir.path().join("another-file"), "x\n").expect("write");
    git_cmd(dir.path(), &["add", "another-file"]);
    std::fs::write(dir.path().join("other-file"), "conflict\n").expect("write");
    git_cmd(dir.path(), &["add", "other-file"]);

    let before_wt = git_cmd(dir.path(), &["diff-files", "-p"]);
    let before_idx = git_cmd(dir.path(), &["diff-index", "--cached", "HEAD"]);

    let repo = open_grit(dir.path());
    let err = pop_stash_entry(
        &repo,
        dir.path(),
        0,
        &StashApplyOptions {
            restore_index: true,
        },
        IDENTITY,
    )
    .expect_err("index conflict");
    assert!(
        matches!(err, Error::StashIndexConflict),
        "expected index conflict, got {err:?}"
    );

    assert_eq!(git_cmd(dir.path(), &["diff-files", "-p"]), before_wt);
    assert_eq!(
        git_cmd(dir.path(), &["diff-index", "--cached", "HEAD"]),
        before_idx
    );
    assert!(
        !git_cmd(dir.path(), &["stash", "list"]).trim().is_empty(),
        "stash entry kept"
    );
}

#[test]
fn git_stash_apply_index_on_grit_stash() {
    let dir = tempfile::tempdir().expect("tempdir");
    init_repo(dir.path());
    std::fs::write(dir.path().join("y.txt"), "base\n").expect("write");
    git_cmd(dir.path(), &["add", "y.txt"]);
    git_cmd(dir.path(), &["commit", "-qm", "init"]);
    std::fs::write(dir.path().join("y.txt"), "staged\n").expect("write");
    git_cmd(dir.path(), &["add", "y.txt"]);

    let repo = open_grit(dir.path());
    push_stash(&repo, &stash_opts(false, Vec::new()))
        .expect("push")
        .expect("some");
    git_cmd(dir.path(), &["reset", "--hard", "HEAD"]);

    git_cmd(dir.path(), &["stash", "apply", "--index", "stash@{0}"]);
    let porcelain = git_cmd(dir.path(), &["status", "--porcelain"]);
    assert!(
        porcelain.starts_with("M  y.txt"),
        "expected staged modify, got:\n{porcelain}"
    );
}

#[test]
fn grit_apply_index_on_git_stash() {
    let base = tempfile::tempdir().expect("tempdir");
    init_repo(base.path());
    std::fs::write(base.path().join("z.txt"), "base\n").expect("write");
    git_cmd(base.path(), &["add", "z.txt"]);
    git_cmd(base.path(), &["commit", "-qm", "init"]);
    std::fs::write(base.path().join("z.txt"), "staged\n").expect("write");
    git_cmd(base.path(), &["add", "z.txt"]);
    git_cmd(base.path(), &["stash", "push", "-qm", "s"]);

    let grit_dir = tempfile::tempdir().expect("grit copy");
    let git_dir = tempfile::tempdir().expect("git copy");
    for dest in [grit_dir.path(), git_dir.path()] {
        std::process::Command::new("cp")
            .args([
                "-a",
                &format!("{}/.", base.path().display()),
                &dest.to_string_lossy(),
            ])
            .status()
            .expect("cp");
        git_cmd(dest, &["reset", "--hard", "HEAD"]);
    }

    apply_stash_entry(
        &open_grit(grit_dir.path()),
        grit_dir.path(),
        0,
        &StashApplyOptions {
            restore_index: true,
        },
    )
    .expect("apply");
    git_cmd(git_dir.path(), &["stash", "apply", "--index", "stash@{0}"]);
    assert_eq!(
        ls_files_stage(grit_dir.path()),
        ls_files_stage(git_dir.path())
    );
}

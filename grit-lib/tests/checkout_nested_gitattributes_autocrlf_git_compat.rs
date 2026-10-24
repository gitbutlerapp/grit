//! Nested `.gitattributes` `binary` must disable `core.autocrlf` smudge on checkout (clone/switch).

use std::fs;
use std::process::Command;

use grit_lib::config::environment::RepositoryOptions;
use grit_lib::objects::ObjectId;
use grit_lib::porcelain::checkout::checkout_between_trees;
use grit_lib::porcelain::switch::{switch_repository, SwitchMode, SwitchOptions};
use grit_lib::repo::Repository;

fn git(repo: &std::path::Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .current_dir(repo)
        .args(args)
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

fn tree_of_head(repo: &std::path::Path) -> ObjectId {
    let hex = git(repo, &["rev-parse", "HEAD^{tree}"]).trim().to_string();
    ObjectId::from_hex(&hex).expect("tree oid")
}

fn init_nested_binary_repo(root: &std::path::Path) {
    git(root, &["init", "-q", "-b", "main", "."]);
    git(root, &["config", "user.email", "t@example.com"]);
    git(root, &["config", "user.name", "Test"]);
    git(root, &["config", "core.autocrlf", "true"]);
    fs::create_dir_all(root.join("sub")).expect("mkdir sub");
    fs::write(root.join("sub/.gitattributes"), "bin-* binary\n").expect("attrs");
    fs::write(root.join("sub/bin-1"), b"a\nb\n").expect("bin file");
    git(root, &["add", "-A"]);
    git(root, &["commit", "-qm", "c1"]);
}

fn assert_clean_git_status(root: &std::path::Path, context: &str) {
    let porcelain = git(root, &["status", "--porcelain"]);
    assert!(
        porcelain.trim().is_empty(),
        "{context}: expected clean git status, got:\n{porcelain}"
    );
}

#[test]
fn checkout_between_trees_honours_nested_binary_with_autocrlf() {
    let tmp = tempfile::tempdir().expect("tempdir");
    init_nested_binary_repo(tmp.path());

    let src = tmp.path();
    let clone_dir = tempfile::tempdir().expect("clone dir");
    git(
        clone_dir.path(),
        &[
            "clone",
            "-q",
            "--no-checkout",
            src.to_str().expect("utf8"),
            "repo",
        ],
    );
    let repo_root = clone_dir.path().join("repo");
    // `core.autocrlf` is not part of the committed tree; grit clone applies the
    // caller/environment config (issue #1609 repro uses system autocrlf=true).
    git(&repo_root, &["config", "core.autocrlf", "true"]);

    let grit_repo =
        Repository::discover(Some(&repo_root), &RepositoryOptions::empty()).expect("open");
    grit_repo.reload_config().expect("reload autocrlf config");
    let head_tree = tree_of_head(&repo_root);
    checkout_between_trees(&grit_repo, None, &head_tree).expect("populate checkout");

    let bytes = fs::read(repo_root.join("sub/bin-1")).expect("read worktree");
    assert_eq!(
        bytes.len(),
        4,
        "binary file must stay LF-only (4 bytes), got {bytes:?}"
    );
    assert_clean_git_status(&repo_root, "after grit checkout_between_trees");
}

#[test]
fn bulk_populated_checkout_honours_nested_binary_with_autocrlf() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let repo_root = tmp.path();
    git(repo_root, &["init", "-q", "-b", "main", "."]);
    git(repo_root, &["config", "user.email", "t@example.com"]);
    git(repo_root, &["config", "user.name", "Test"]);
    git(repo_root, &["config", "core.autocrlf", "true"]);
    fs::create_dir_all(repo_root.join("sub")).expect("mkdir sub");
    fs::write(repo_root.join("sub/.gitattributes"), "bin-* binary\n").expect("attrs");
    for i in 0..40 {
        fs::write(
            repo_root.join(format!("sub/bin-{i:02}")),
            format!("x{i}\ny{i}\n"),
        )
        .expect("bin");
    }
    git(repo_root, &["add", "-A"]);
    git(repo_root, &["commit", "-qm", "bulk"]);
    let tree = tree_of_head(repo_root);

    let repo = Repository::open(
        &repo_root.join(".git"),
        &RepositoryOptions::empty().with_work_tree(repo_root),
    )
    .expect("open");
    for i in 0..40 {
        fs::remove_file(repo_root.join(format!("sub/bin-{i:02}"))).expect("remove");
    }
    fs::remove_file(repo_root.join("sub/.gitattributes")).expect("remove attrs");

    checkout_between_trees(&repo, None, &tree).expect("bulk populated checkout");

    let sample = fs::read(repo_root.join("sub/bin-00")).expect("read");
    assert!(
        !sample.windows(2).any(|w| w == b"\r\n"),
        "bulk checkout must not CRLF-smudge nested binary files"
    );
    assert_clean_git_status(repo_root, "after bulk populated checkout");
}

#[test]
fn switch_honours_nested_binary_with_autocrlf() {
    let tmp = tempfile::tempdir().expect("tempdir");
    init_nested_binary_repo(tmp.path());

    git(tmp.path(), &["checkout", "-q", "-b", "older"]);
    fs::remove_file(tmp.path().join("sub/bin-1")).expect("remove");
    git(tmp.path(), &["add", "-A"]);
    git(tmp.path(), &["commit", "-qm", "older"]);

    git(tmp.path(), &["checkout", "-q", "main"]);
    let main_oid =
        ObjectId::from_hex(git(tmp.path(), &["rev-parse", "main"]).trim()).expect("main");

    let grit_repo =
        Repository::discover(Some(tmp.path()), &RepositoryOptions::empty()).expect("open");

    switch_repository(
        &grit_repo,
        &SwitchOptions {
            mode: SwitchMode::Existing {
                branch: "older".into(),
                oid: ObjectId::from_hex(git(tmp.path(), &["rev-parse", "older"]).trim())
                    .expect("older"),
            },
            dry_run: false,
            hooks: grit_lib::PostOperationHooks::Run,
        },
    )
    .expect("switch older");

    switch_repository(
        &grit_repo,
        &SwitchOptions {
            mode: SwitchMode::Existing {
                branch: "main".into(),
                oid: main_oid,
            },
            dry_run: false,
            hooks: grit_lib::PostOperationHooks::Run,
        },
    )
    .expect("switch main");

    let bytes = fs::read(tmp.path().join("sub/bin-1")).expect("read");
    assert_eq!(
        bytes.len(),
        4,
        "binary file must stay LF-only after switch, got {bytes:?}"
    );
    assert_clean_git_status(tmp.path(), "after grit switch back to main");
}

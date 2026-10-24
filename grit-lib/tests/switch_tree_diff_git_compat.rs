//! After `switch_repository`, system `git` sees a clean index and worktree.

mod switch_overwrite_git_helpers;

use std::process::Command;

use grit_lib::error::{Error, OverwriteReason};
use switch_overwrite_git_helpers::git_switch_refused_paths;

use grit_lib::config::environment::RepositoryOptions;
use grit_lib::objects::ObjectId;
use grit_lib::porcelain::switch::{switch_repository, SwitchMode, SwitchOptions};
use grit_lib::refs;
use grit_lib::repo::Repository;
use grit_test_support::git;

fn git_porcelain(repo: &std::path::Path) -> String {
    git(repo, &["status", "--porcelain"])
}

fn git_diff_index_head(repo: &std::path::Path) -> String {
    git(repo, &["diff-index", "HEAD"])
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

fn populate_initial(repo_root: &std::path::Path) {
    for i in 0..500 {
        let dir = repo_root.join(format!("d{:03}", i % 20));
        std::fs::create_dir_all(&dir).expect("mkdir");
        std::fs::write(dir.join(format!("f{i:04}.txt")), format!("seed {i}\n")).expect("write");
    }
    std::fs::write(repo_root.join("swap.txt"), b"file-before\n").expect("swap");
    git(repo_root, &["add", "-A"]);
    git(repo_root, &["commit", "-qm", "initial"]);
}

fn build_target_branch(repo_root: &std::path::Path) {
    git(repo_root, &["checkout", "-q", "-b", "target"]);
    for i in (0..500).step_by(17) {
        let dir = repo_root.join(format!("d{:03}", i % 20));
        let path = dir.join(format!("f{i:04}.txt"));
        std::fs::write(&path, format!("changed {i}\n")).expect("modify");
    }
    std::fs::remove_file(repo_root.join("swap.txt")).expect("remove file");
    std::fs::create_dir_all(repo_root.join("swap.txt")).expect("file to dir");
    std::fs::write(repo_root.join("swap.txt/leaf.txt"), b"leaf\n").expect("dir leaf");
    git(repo_root, &["add", "-A"]);
    git(repo_root, &["commit", "-qm", "target"]);
    git(repo_root, &["checkout", "-q", "main"]);
}

#[test]
fn switch_nested_gitattributes_crlf_clean_worktree_matches_git() {
    let tmp = tempfile::tempdir().expect("tempdir");
    git(tmp.path(), &["init", "-q", "-b", "main", "."]);
    git(tmp.path(), &["config", "user.email", "t@example.com"]);
    git(tmp.path(), &["config", "user.name", "Test"]);
    git(tmp.path(), &["config", "gc.auto", "0"]);
    git(tmp.path(), &["config", "core.autocrlf", "false"]);

    std::fs::create_dir_all(tmp.path().join("sub")).expect("mkdir sub");
    std::fs::write(
        tmp.path().join("sub/.gitattributes"),
        "*.txt text eol=crlf\n",
    )
    .expect("nested gitattributes");
    std::fs::write(tmp.path().join("sub/file.txt"), "main line\r\n").expect("write");
    git(tmp.path(), &["add", "-A"]);
    git(tmp.path(), &["commit", "-qm", "main"]);

    assert!(
        git_porcelain(tmp.path()).trim().is_empty(),
        "expected clean git status on main, got:\n{}",
        git_porcelain(tmp.path())
    );

    git(tmp.path(), &["checkout", "-q", "-b", "target"]);
    std::fs::write(tmp.path().join("sub/file.txt"), "target line\r\n").expect("write target");
    git(tmp.path(), &["add", "-A"]);
    git(tmp.path(), &["commit", "-qm", "target"]);
    git(tmp.path(), &["checkout", "-q", "main"]);

    assert!(
        git_porcelain(tmp.path()).trim().is_empty(),
        "expected clean git status before switch, got:\n{}",
        git_porcelain(tmp.path())
    );

    git(tmp.path(), &["switch", "-q", "target"]);
    assert!(
        git_porcelain(tmp.path()).trim().is_empty(),
        "system git switch to target must leave clean tree"
    );
    git(tmp.path(), &["switch", "-q", "main"]);

    let grit_repo = Repository::open(
        &tmp.path().join(".git"),
        &RepositoryOptions::empty().with_work_tree(tmp.path()),
    )
    .expect("open grit repo");
    let target_oid =
        ObjectId::from_hex(git(tmp.path(), &["rev-parse", "target"]).trim()).expect("target oid");

    switch_repository(
        &grit_repo,
        &SwitchOptions {
            mode: SwitchMode::Existing {
                branch: "target".into(),
                oid: target_oid,
            },
            dry_run: false,
            hooks: grit_lib::PostOperationHooks::Run,
        },
    )
    .expect("grit switch must succeed on clean nested-eol=crlf worktree like git");

    assert!(
        git_porcelain(tmp.path()).trim().is_empty(),
        "git status --porcelain must be empty after grit switch, got:\n{}",
        git_porcelain(tmp.path())
    );
    assert!(
        git_diff_index_head(tmp.path()).trim().is_empty(),
        "git diff-index HEAD must be empty, got:\n{}",
        git_diff_index_head(tmp.path())
    );
    git_fsck(tmp.path());
}

#[test]
fn switch_wholesale_deleted_gitattributes_does_not_apply_stale_eol() {
    let tmp = tempfile::tempdir().expect("tempdir");
    git(tmp.path(), &["init", "-q", "-b", "main", "."]);
    git(tmp.path(), &["config", "user.email", "t@example.com"]);
    git(tmp.path(), &["config", "user.name", "Test"]);
    git(tmp.path(), &["config", "gc.auto", "0"]);
    git(tmp.path(), &["config", "core.autocrlf", "false"]);

    std::fs::create_dir_all(tmp.path().join("d")).expect("mkdir d");
    std::fs::write(tmp.path().join("d/.gitattributes"), "*.txt text eol=crlf\n")
        .expect("gitattributes");
    for i in 0..7 {
        std::fs::write(
            tmp.path().join(format!("d/f{i}.txt")),
            format!("main {i}\r\n"),
        )
        .expect("tracked");
    }
    git(tmp.path(), &["add", "-A"]);
    git(tmp.path(), &["commit", "-qm", "main"]);

    git(tmp.path(), &["checkout", "-q", "-b", "target"]);
    let _ = std::fs::remove_dir_all(tmp.path().join("d"));
    std::fs::create_dir_all(tmp.path().join("d")).expect("recreate d");
    std::fs::write(tmp.path().join("d/new.txt"), "new\n").expect("lf new file");
    git(tmp.path(), &["add", "-A"]);
    git(tmp.path(), &["commit", "-qm", "drop d tree and add new"]);
    git(tmp.path(), &["checkout", "-q", "main"]);

    git(tmp.path(), &["switch", "-q", "target"]);
    let git_bytes = std::fs::read(tmp.path().join("d/new.txt")).expect("git new.txt");
    assert_eq!(
        git_bytes, b"new\n",
        "system git must write LF bytes for new.txt after wholesale dir replace"
    );
    assert!(
        git_porcelain(tmp.path()).trim().is_empty(),
        "system git must stay clean, got:\n{}",
        git_porcelain(tmp.path())
    );
    git(tmp.path(), &["switch", "-q", "main"]);

    let grit_repo = Repository::open(
        &tmp.path().join(".git"),
        &RepositoryOptions::empty().with_work_tree(tmp.path()),
    )
    .expect("open grit repo");
    let target_oid =
        ObjectId::from_hex(git(tmp.path(), &["rev-parse", "target"]).trim()).expect("target oid");

    switch_repository(
        &grit_repo,
        &SwitchOptions {
            mode: SwitchMode::Existing {
                branch: "target".into(),
                oid: target_oid,
            },
            dry_run: false,
            hooks: grit_lib::PostOperationHooks::Run,
        },
    )
    .expect("grit switch");

    let grit_bytes = std::fs::read(tmp.path().join("d/new.txt")).expect("grit new.txt");
    assert_eq!(
        grit_bytes, b"new\n",
        "grit must match git LF worktree bytes (not CRLF-smudged), got {:?}",
        grit_bytes
    );
    assert_eq!(
        grit_bytes, git_bytes,
        "grit and git must agree on on-disk bytes"
    );
    assert!(
        git_porcelain(tmp.path()).trim().is_empty(),
        "git status --porcelain must be empty after grit switch, got:\n{}",
        git_porcelain(tmp.path())
    );
}

#[test]
fn switch_preserves_untracked_in_wholesale_deleted_directory() {
    let tmp = tempfile::tempdir().expect("tempdir");
    git(tmp.path(), &["init", "-q", "-b", "main", "."]);
    git(tmp.path(), &["config", "user.email", "t@example.com"]);
    git(tmp.path(), &["config", "user.name", "Test"]);
    git(tmp.path(), &["config", "gc.auto", "0"]);

    std::fs::create_dir_all(tmp.path().join("d")).expect("mkdir d");
    for i in 0..8 {
        std::fs::write(
            tmp.path().join(format!("d/f{i}.txt")),
            format!("main {i}\n"),
        )
        .expect("write");
    }
    git(tmp.path(), &["add", "-A"]);
    git(tmp.path(), &["commit", "-qm", "main"]);

    git(tmp.path(), &["checkout", "-q", "-b", "target"]);
    git(tmp.path(), &["rm", "-r", "d"]);
    git(tmp.path(), &["commit", "-qm", "drop d"]);
    git(tmp.path(), &["checkout", "-q", "main"]);

    std::fs::write(tmp.path().join("d/untracked.txt"), b"precious\n").expect("untracked");
    assert!(
        std::fs::read(tmp.path().join("d/untracked.txt")).is_ok(),
        "untracked file must exist before switch"
    );

    git(tmp.path(), &["switch", "-q", "target"]);
    assert!(
        std::fs::read(tmp.path().join("d/untracked.txt")).is_ok(),
        "system git must preserve untracked file under deleted directory"
    );
    git(tmp.path(), &["switch", "-q", "main"]);

    std::fs::write(tmp.path().join("d/untracked.txt"), b"precious\n").expect("untracked again");

    let grit_repo = Repository::open(
        &tmp.path().join(".git"),
        &RepositoryOptions::empty().with_work_tree(tmp.path()),
    )
    .expect("open grit repo");
    let target_oid =
        ObjectId::from_hex(git(tmp.path(), &["rev-parse", "target"]).trim()).expect("target oid");

    switch_repository(
        &grit_repo,
        &SwitchOptions {
            mode: SwitchMode::Existing {
                branch: "target".into(),
                oid: target_oid,
            },
            dry_run: false,
            hooks: grit_lib::PostOperationHooks::Run,
        },
    )
    .expect("grit switch to target");

    let kept = std::fs::read_to_string(tmp.path().join("d/untracked.txt")).expect("untracked kept");
    assert_eq!(kept, "precious\n");
    let porcelain = git_porcelain(tmp.path());
    assert!(
        porcelain.contains("?? d/") || porcelain.contains("?? d/untracked.txt"),
        "git must report the preserved untracked path, got:\n{porcelain}"
    );
}

#[test]
#[cfg(unix)]
fn switch_preserves_untracked_fifo_under_fast_wholesale_delete_scan() {
    use std::process::Command;

    let tmp = tempfile::tempdir().expect("tempdir");
    git(tmp.path(), &["init", "-q", "-b", "main", "."]);
    git(tmp.path(), &["config", "user.email", "t@example.com"]);
    git(tmp.path(), &["config", "user.name", "Test"]);
    git(tmp.path(), &["config", "gc.auto", "0"]);

    std::fs::create_dir_all(tmp.path().join("bulk")).expect("mkdir bulk");
    for i in 0..40 {
        std::fs::write(
            tmp.path().join(format!("bulk/f{i:02}.txt")),
            format!("main {i}\n"),
        )
        .expect("write");
    }
    git(tmp.path(), &["add", "-A"]);
    git(tmp.path(), &["commit", "-qm", "main"]);

    git(tmp.path(), &["checkout", "-q", "-b", "target"]);
    git(tmp.path(), &["rm", "-r", "bulk"]);
    git(tmp.path(), &["commit", "-qm", "drop bulk"]);
    git(tmp.path(), &["checkout", "-q", "main"]);

    let fifo = tmp.path().join("bulk/untracked.pipe");
    let status = Command::new("mkfifo").arg(&fifo).status().expect("mkfifo");
    assert!(status.success(), "mkfifo must succeed on unix");

    git(tmp.path(), &["switch", "-q", "target"]);
    assert!(
        fifo.exists(),
        "system git must preserve untracked FIFO under wholesale-deleted directory"
    );
    git(tmp.path(), &["switch", "-q", "main"]);

    let _ = std::fs::remove_file(&fifo);
    let status = Command::new("mkfifo")
        .arg(&fifo)
        .status()
        .expect("mkfifo again");
    assert!(status.success());

    let grit_repo = Repository::open(
        &tmp.path().join(".git"),
        &RepositoryOptions::empty().with_work_tree(tmp.path()),
    )
    .expect("open grit repo");
    let target_oid =
        ObjectId::from_hex(git(tmp.path(), &["rev-parse", "target"]).trim()).expect("target oid");

    switch_repository(
        &grit_repo,
        &SwitchOptions {
            mode: SwitchMode::Existing {
                branch: "target".into(),
                oid: target_oid,
            },
            dry_run: false,
            hooks: grit_lib::PostOperationHooks::Run,
        },
    )
    .expect("grit switch to target");

    assert!(
        fifo.exists(),
        "grit must preserve untracked FIFO (fast wholesale scan must not trust removal)"
    );
}

#[test]
fn switch_preserves_ignored_file_in_wholesale_deleted_directory() {
    let tmp = tempfile::tempdir().expect("tempdir");
    git(tmp.path(), &["init", "-q", "-b", "main", "."]);
    git(tmp.path(), &["config", "user.email", "t@example.com"]);
    git(tmp.path(), &["config", "user.name", "Test"]);
    git(tmp.path(), &["config", "gc.auto", "0"]);

    std::fs::write(tmp.path().join(".gitignore"), "*.log\n").expect("gitignore");
    std::fs::create_dir_all(tmp.path().join("d")).expect("mkdir d");
    for i in 0..8 {
        std::fs::write(
            tmp.path().join(format!("d/f{i}.txt")),
            format!("main {i}\n"),
        )
        .expect("write");
    }
    git(tmp.path(), &["add", "-A"]);
    git(tmp.path(), &["commit", "-qm", "main"]);

    git(tmp.path(), &["checkout", "-q", "-b", "target"]);
    git(tmp.path(), &["rm", "-r", "d"]);
    git(tmp.path(), &["commit", "-qm", "drop d"]);
    git(tmp.path(), &["checkout", "-q", "main"]);

    std::fs::write(tmp.path().join("d/ignored.log"), b"keep\n").expect("ignored");
    assert!(
        std::fs::read(tmp.path().join("d/ignored.log")).is_ok(),
        "ignored file must exist before switch"
    );

    git(tmp.path(), &["switch", "-q", "target"]);
    assert!(
        std::fs::read(tmp.path().join("d/ignored.log")).is_ok(),
        "system git must preserve ignored file under wholesale-deleted directory"
    );
    git(tmp.path(), &["switch", "-q", "main"]);

    std::fs::write(tmp.path().join("d/ignored.log"), b"keep\n").expect("ignored again");

    let grit_repo = Repository::open(
        &tmp.path().join(".git"),
        &RepositoryOptions::empty().with_work_tree(tmp.path()),
    )
    .expect("open grit repo");
    let target_oid =
        ObjectId::from_hex(git(tmp.path(), &["rev-parse", "target"]).trim()).expect("target oid");

    switch_repository(
        &grit_repo,
        &SwitchOptions {
            mode: SwitchMode::Existing {
                branch: "target".into(),
                oid: target_oid,
            },
            dry_run: false,
            hooks: grit_lib::PostOperationHooks::Run,
        },
    )
    .expect("grit switch to target");

    let kept = std::fs::read_to_string(tmp.path().join("d/ignored.log")).expect("ignored kept");
    assert_eq!(kept, "keep\n");
}

#[test]
fn switch_preserves_ignored_file_from_nested_tracked_gitignore() {
    let tmp = tempfile::tempdir().expect("tempdir");
    git(tmp.path(), &["init", "-q", "-b", "main", "."]);
    git(tmp.path(), &["config", "user.email", "t@example.com"]);
    git(tmp.path(), &["config", "user.name", "Test"]);
    git(tmp.path(), &["config", "gc.auto", "0"]);

    std::fs::create_dir_all(tmp.path().join("d")).expect("mkdir d");
    std::fs::write(tmp.path().join("d/.gitignore"), "*.log\n").expect("nested gitignore");
    for i in 0..8 {
        std::fs::write(
            tmp.path().join(format!("d/f{i}.txt")),
            format!("main {i}\n"),
        )
        .expect("write");
    }
    git(tmp.path(), &["add", "-A"]);
    git(tmp.path(), &["commit", "-qm", "main"]);

    git(tmp.path(), &["checkout", "-q", "-b", "target"]);
    git(tmp.path(), &["rm", "-r", "d"]);
    git(tmp.path(), &["commit", "-qm", "drop d"]);
    git(tmp.path(), &["checkout", "-q", "main"]);

    std::fs::write(tmp.path().join("d/ignored.log"), b"keep\n").expect("ignored");
    assert!(
        std::fs::read(tmp.path().join("d/ignored.log")).is_ok(),
        "ignored file must exist before switch"
    );

    git(tmp.path(), &["switch", "-q", "target"]);
    assert!(
        std::fs::read(tmp.path().join("d/ignored.log")).is_ok(),
        "system git must preserve ignored file when rules live in nested tracked .gitignore"
    );
    git(tmp.path(), &["switch", "-q", "main"]);

    std::fs::write(tmp.path().join("d/ignored.log"), b"keep\n").expect("ignored again");

    let grit_repo = Repository::open(
        &tmp.path().join(".git"),
        &RepositoryOptions::empty().with_work_tree(tmp.path()),
    )
    .expect("open grit repo");
    let target_oid =
        ObjectId::from_hex(git(tmp.path(), &["rev-parse", "target"]).trim()).expect("target oid");

    switch_repository(
        &grit_repo,
        &SwitchOptions {
            mode: SwitchMode::Existing {
                branch: "target".into(),
                oid: target_oid,
            },
            dry_run: false,
            hooks: grit_lib::PostOperationHooks::Run,
        },
    )
    .expect("grit switch to target");

    let kept = std::fs::read_to_string(tmp.path().join("d/ignored.log")).expect("ignored kept");
    assert_eq!(kept, "keep\n");
}

#[test]
fn switch_preserves_ignored_file_from_deep_nested_tracked_gitignore() {
    let tmp = tempfile::tempdir().expect("tempdir");
    git(tmp.path(), &["init", "-q", "-b", "main", "."]);
    git(tmp.path(), &["config", "user.email", "t@example.com"]);
    git(tmp.path(), &["config", "user.name", "Test"]);
    git(tmp.path(), &["config", "gc.auto", "0"]);

    std::fs::create_dir_all(tmp.path().join("d/e")).expect("mkdir d/e");
    std::fs::write(tmp.path().join("d/e/.gitignore"), "*.log\n").expect("deep nested gitignore");
    for i in 0..8 {
        std::fs::write(
            tmp.path().join(format!("d/f{i}.txt")),
            format!("main {i}\n"),
        )
        .expect("write");
    }
    git(tmp.path(), &["add", "-A"]);
    git(tmp.path(), &["commit", "-qm", "main"]);

    git(tmp.path(), &["checkout", "-q", "-b", "target"]);
    git(tmp.path(), &["rm", "-r", "d"]);
    git(tmp.path(), &["commit", "-qm", "drop d"]);
    git(tmp.path(), &["checkout", "-q", "main"]);

    std::fs::write(tmp.path().join("d/e/x.log"), b"keep\n").expect("ignored");
    assert!(
        std::fs::read(tmp.path().join("d/e/x.log")).is_ok(),
        "ignored file must exist before switch"
    );

    git(tmp.path(), &["switch", "-q", "target"]);
    assert!(
        std::fs::read(tmp.path().join("d/e/x.log")).is_ok(),
        "system git must preserve ignored file when rules live in d/e/.gitignore"
    );
    git(tmp.path(), &["switch", "-q", "main"]);

    std::fs::write(tmp.path().join("d/e/x.log"), b"keep\n").expect("ignored again");

    let grit_repo = Repository::open(
        &tmp.path().join(".git"),
        &RepositoryOptions::empty().with_work_tree(tmp.path()),
    )
    .expect("open grit repo");
    let target_oid =
        ObjectId::from_hex(git(tmp.path(), &["rev-parse", "target"]).trim()).expect("target oid");

    switch_repository(
        &grit_repo,
        &SwitchOptions {
            mode: SwitchMode::Existing {
                branch: "target".into(),
                oid: target_oid,
            },
            dry_run: false,
            hooks: grit_lib::PostOperationHooks::Run,
        },
    )
    .expect("grit switch to target");

    let kept = std::fs::read_to_string(tmp.path().join("d/e/x.log")).expect("ignored kept");
    assert_eq!(kept, "keep\n");
}

#[test]
fn switch_preserves_populated_gitlink_under_wholesale_deleted_directory() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();
    git(root, &["init", "-q", "-b", "main", "."]);
    git(root, &["config", "user.email", "t@example.com"]);
    git(root, &["config", "user.name", "Test"]);
    git(root, &["config", "gc.auto", "0"]);

    let inner = root.join("d/sub");
    std::fs::create_dir_all(&inner).expect("mkdir d/sub");
    init_nested_inner_repo(&inner);
    std::fs::write(inner.join("nested.txt"), b"submodule payload\n").expect("inner file");
    git(&inner, &["add", "nested.txt"]);
    git(&inner, &["commit", "-qm", "inner"]);

    for i in 0..8 {
        std::fs::write(root.join(format!("d/f{i}.txt")), format!("main {i}\n")).expect("write");
        git(root, &["add", &format!("d/f{i}.txt")]);
    }
    let sub_oid = git(&inner, &["rev-parse", "HEAD"]).trim().to_owned();
    git(
        root,
        &[
            "update-index",
            "--add",
            "--cacheinfo",
            &format!("160000,{sub_oid},d/sub"),
        ],
    );
    std::fs::write(
        root.join(".gitmodules"),
        "[submodule \"d/sub\"]\n\tpath = d/sub\n\turl = file:///dev/null\n",
    )
    .expect("gitmodules");
    git(root, &["add", ".gitmodules"]);
    git(root, &["commit", "-qm", "main with populated gitlink"]);

    git(root, &["checkout", "-q", "-b", "target"]);
    git(root, &["rm", "-r", "d"]);
    git(root, &["commit", "-qm", "drop d"]);
    git(root, &["checkout", "-q", "main"]);
    git(root, &["submodule", "update", "--init", "d/sub"]);

    let inner_payload = std::fs::read_to_string(inner.join("nested.txt")).expect("inner before");
    assert!(
        inner.join(".git").is_file() || inner.join(".git").is_dir(),
        "nested repo must stay initialized before switch"
    );

    git(root, &["switch", "-q", "target"]);
    assert!(
        std::fs::read(inner.join("nested.txt")).is_ok(),
        "system git must preserve populated gitlink worktree under wholesale-deleted d"
    );
    git(root, &["checkout", "-q", "main"]);

    let grit_repo = Repository::open(
        &root.join(".git"),
        &RepositoryOptions::empty().with_work_tree(root),
    )
    .expect("open grit repo");
    let target_oid =
        ObjectId::from_hex(git(root, &["rev-parse", "target"]).trim()).expect("target oid");

    switch_repository(
        &grit_repo,
        &SwitchOptions {
            mode: SwitchMode::Existing {
                branch: "target".into(),
                oid: target_oid,
            },
            dry_run: false,
            hooks: grit_lib::PostOperationHooks::Run,
        },
    )
    .expect("grit switch to target");

    let after = std::fs::read_to_string(inner.join("nested.txt")).expect("inner after grit");
    assert_eq!(after, inner_payload);
    assert!(
        inner.join(".git").exists(),
        "grit must leave initialized submodule directory on disk"
    );
    let ls_files = git(root, &["ls-files", "-s"]);
    assert!(
        !ls_files.lines().any(|line| line.ends_with(" d/sub")),
        "index must not retain d/sub gitlink after switch, got:\n{ls_files}"
    );
}

#[test]
fn switch_refuses_racy_same_size_dirty_file_like_git() {
    use std::path::Path;
    use std::process::Command;

    use filetime::FileTime;
    use grit_lib::index::{entry_from_metadata, Index, MODE_REGULAR};
    use grit_lib::objects::ObjectKind;

    fn pin_mtime(path: &Path, sec: u32, nsec: u32) {
        filetime::set_file_mtime(path, FileTime::from_unix_time(i64::from(sec), nsec))
            .expect("set mtime");
    }

    const CONTENT_A: &str = "0123456789";
    const CONTENT_B: &str = "9876543210";
    const CONTENT_C: &str = "5555555555";
    assert_eq!(CONTENT_A.len(), CONTENT_B.len());
    assert_eq!(CONTENT_A.len(), CONTENT_C.len());

    let tmp = tempfile::tempdir().expect("tempdir");
    git(tmp.path(), &["init", "-q", "-b", "main", "."]);
    git(tmp.path(), &["config", "user.email", "t@example.com"]);
    git(tmp.path(), &["config", "user.name", "Test"]);
    git(tmp.path(), &["config", "core.trustctime", "false"]);
    git(tmp.path(), &["config", "core.checkstat", "minimal"]);
    git(tmp.path(), &["config", "core.usenanosec", "false"]);

    std::fs::write(tmp.path().join("hot.txt"), CONTENT_A).expect("write");
    git(tmp.path(), &["add", "hot.txt"]);
    git(tmp.path(), &["commit", "-qm", "main"]);

    git(tmp.path(), &["checkout", "-q", "-b", "target"]);
    std::fs::write(tmp.path().join("hot.txt"), CONTENT_B).expect("write target");
    git(tmp.path(), &["add", "hot.txt"]);
    git(tmp.path(), &["commit", "-qm", "target"]);
    git(tmp.path(), &["checkout", "-q", "main"]);

    let grit_repo = Repository::open(
        &tmp.path().join(".git"),
        &RepositoryOptions::empty().with_work_tree(tmp.path()),
    )
    .expect("open grit repo");
    let loaded = grit_repo.index_handle().load().expect("index");
    let entry = loaded.get(b"hot.txt", 0).expect("hot.txt indexed");
    let index_mtime = loaded.source_mtime.expect("index source mtime");
    let entry_mtime = (entry.mtime_sec, entry.mtime_nsec);

    let hot_path = tmp.path().join("hot.txt");
    std::fs::write(&hot_path, CONTENT_C).expect("dirty same-size");
    pin_mtime(&hot_path, entry_mtime.0, entry_mtime.1);

    let meta = std::fs::symlink_metadata(&hot_path).expect("meta");
    let stale_oid = grit_repo
        .odb()
        .hash(ObjectKind::Blob, CONTENT_A.as_bytes())
        .expect("hash");
    let mut racy_index = Index::new();
    racy_index.add_or_replace(entry_from_metadata(
        &meta,
        b"hot.txt",
        stale_oid,
        MODE_REGULAR,
    ));
    racy_index.sort();
    let index_path = tmp.path().join(".git").join("index");
    racy_index.write(&index_path).expect("write racy index");
    pin_mtime(&index_path, index_mtime.0, index_mtime.1);

    let git_diff_files = git(tmp.path(), &["diff-files"]);
    assert!(
        !git_diff_files.trim().is_empty(),
        "git diff-files must report dirty racy file, got:\n{git_diff_files}"
    );

    let git_switch = Command::new("git")
        .current_dir(tmp.path())
        .args(["switch", "target"])
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .status()
        .expect("git switch");
    assert!(
        !git_switch.success(),
        "system git must refuse switch on racy dirty worktree"
    );

    let main_before = git(tmp.path(), &["rev-parse", "HEAD"]);
    let dirty_before = std::fs::read_to_string(&hot_path).expect("read dirty");
    assert_eq!(dirty_before, CONTENT_C);

    let target_oid =
        ObjectId::from_hex(git(tmp.path(), &["rev-parse", "target"]).trim()).expect("target oid");
    let git_paths = git_switch_refused_paths(tmp.path(), "target");
    let err = switch_repository(
        &grit_repo,
        &SwitchOptions {
            mode: SwitchMode::Existing {
                branch: "target".into(),
                oid: target_oid,
            },
            dry_run: false,
            hooks: grit_lib::PostOperationHooks::Run,
        },
    )
    .expect_err("grit switch must refuse racy dirty worktree like git");
    match err {
        Error::WouldOverwrite {
            reason: OverwriteReason::LocalChanges,
            paths,
            ..
        } => {
            assert_eq!(paths, git_paths);
            assert!(paths.contains(&"hot.txt".to_owned()));
        }
        other => panic!("unexpected error: {other:?}"),
    }

    assert_eq!(git(tmp.path(), &["rev-parse", "HEAD"]), main_before);
    assert_eq!(
        std::fs::read_to_string(&hot_path).expect("dirty preserved"),
        CONTENT_C
    );
}

#[test]
fn switch_tree_diff_git_clean_after() {
    let tmp = tempfile::tempdir().expect("tempdir");
    git(tmp.path(), &["init", "-q", "-b", "main", "."]);
    git(tmp.path(), &["config", "user.email", "t@example.com"]);
    git(tmp.path(), &["config", "user.name", "Test"]);
    git(tmp.path(), &["config", "gc.auto", "0"]);

    populate_initial(tmp.path());
    build_target_branch(tmp.path());

    let grit_repo = Repository::open(
        &tmp.path().join(".git"),
        &RepositoryOptions::empty().with_work_tree(tmp.path()),
    )
    .expect("open grit repo");

    let target_oid =
        ObjectId::from_hex(git(tmp.path(), &["rev-parse", "target"]).trim()).expect("target oid");

    switch_repository(
        &grit_repo,
        &SwitchOptions {
            mode: SwitchMode::Existing {
                branch: "target".into(),
                oid: target_oid,
            },
            dry_run: false,
            hooks: grit_lib::PostOperationHooks::Run,
        },
    )
    .expect("switch to target");

    assert!(
        git_porcelain(tmp.path()).trim().is_empty(),
        "git status --porcelain must be empty after grit switch, got:\n{}",
        git_porcelain(tmp.path())
    );
    assert!(
        git_diff_index_head(tmp.path()).trim().is_empty(),
        "git diff-index HEAD must be empty, got:\n{}",
        git_diff_index_head(tmp.path())
    );
    git_fsck(tmp.path());

    let _ = refs::resolve_ref(grit_repo.git_dir(), "HEAD").expect("head");
}

#[test]
fn switch_refuses_dirty_conflicting_path_and_leaves_file() {
    let tmp = tempfile::tempdir().expect("tempdir");
    git(tmp.path(), &["init", "-q", "-b", "main", "."]);
    git(tmp.path(), &["config", "user.email", "t@example.com"]);
    git(tmp.path(), &["config", "user.name", "Test"]);

    std::fs::write(tmp.path().join("hot.txt"), b"main\n").expect("write");
    git(tmp.path(), &["add", "hot.txt"]);
    git(tmp.path(), &["commit", "-qm", "main"]);

    git(tmp.path(), &["checkout", "-q", "-b", "other"]);
    std::fs::write(tmp.path().join("hot.txt"), b"other\n").expect("write");
    git(tmp.path(), &["commit", "-am", "other"]);
    git(tmp.path(), &["checkout", "-q", "main"]);

    std::fs::write(tmp.path().join("hot.txt"), b"dirty local\n").expect("dirty");
    let before = std::fs::read(tmp.path().join("hot.txt")).expect("read");

    let grit_repo = Repository::open(
        &tmp.path().join(".git"),
        &RepositoryOptions::empty().with_work_tree(tmp.path()),
    )
    .expect("open grit repo");
    let other_oid =
        ObjectId::from_hex(git(tmp.path(), &["rev-parse", "other"]).trim()).expect("other oid");

    let git_paths = git_switch_refused_paths(tmp.path(), "other");
    let err = switch_repository(
        &grit_repo,
        &SwitchOptions {
            mode: SwitchMode::Existing {
                branch: "other".into(),
                oid: other_oid,
            },
            dry_run: false,
            hooks: grit_lib::PostOperationHooks::Run,
        },
    )
    .expect_err("dirty switch must fail");
    match err {
        Error::WouldOverwrite {
            reason: OverwriteReason::LocalChanges,
            paths,
            ..
        } => assert_eq!(paths, git_paths),
        other => panic!("expected WouldOverwrite LocalChanges, got {other:?}"),
    }
    let after = std::fs::read(tmp.path().join("hot.txt")).expect("read after");
    assert_eq!(before, after, "conflicting dirty file must be untouched");
}

/// Stat drift on one tree-diff path must not hide a real local edit on another (issue #1613 review).
#[test]
fn switch_refuses_dirty_second_path_when_first_has_stat_drift_only_like_git() {
    let tmp = tempfile::tempdir().expect("tempdir");
    git(tmp.path(), &["init", "-q", "-b", "main", "."]);
    git(tmp.path(), &["config", "user.email", "t@example.com"]);
    git(tmp.path(), &["config", "user.name", "Test"]);

    std::fs::write(tmp.path().join("a.txt"), b"main a\n").expect("write a");
    std::fs::write(tmp.path().join("b.txt"), b"main b\n").expect("write b");
    git(tmp.path(), &["add", "a.txt", "b.txt"]);
    git(tmp.path(), &["commit", "-qm", "main"]);

    git(tmp.path(), &["checkout", "-q", "-b", "target"]);
    std::fs::write(tmp.path().join("a.txt"), b"target a\n").expect("write a target");
    std::fs::write(tmp.path().join("b.txt"), b"target b\n").expect("write b target");
    git(tmp.path(), &["commit", "-am", "target"]);
    git(tmp.path(), &["checkout", "-q", "main"]);

    let future = filetime::FileTime::from_unix_time(1_900_000_000, 0);
    filetime::set_file_mtime(tmp.path().join("a.txt"), future).expect("future mtime on a.txt");
    std::fs::write(tmp.path().join("b.txt"), b"dirty local b\n").expect("dirty b");
    let b_before = std::fs::read(tmp.path().join("b.txt")).expect("read b before");

    let git_switch = Command::new("git")
        .current_dir(tmp.path())
        .args(["switch", "target"])
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .status()
        .expect("git switch");
    assert!(
        !git_switch.success(),
        "system git must refuse switch when b.txt is dirty"
    );
    assert_eq!(
        std::fs::read(tmp.path().join("b.txt")).expect("read b after git"),
        b_before,
        "git must preserve dirty b.txt"
    );
    assert_eq!(
        git(tmp.path(), &["rev-parse", "--abbrev-ref", "HEAD"]).trim(),
        "main",
        "git must stay on main"
    );

    let grit_repo = Repository::open(
        &tmp.path().join(".git"),
        &RepositoryOptions::empty().with_work_tree(tmp.path()),
    )
    .expect("open grit repo");
    let target_oid =
        ObjectId::from_hex(git(tmp.path(), &["rev-parse", "target"]).trim()).expect("target oid");

    let git_paths = git_switch_refused_paths(tmp.path(), "target");
    let err = switch_repository(
        &grit_repo,
        &SwitchOptions {
            mode: SwitchMode::Existing {
                branch: "target".into(),
                oid: target_oid,
            },
            dry_run: false,
            hooks: grit_lib::PostOperationHooks::Run,
        },
    )
    .expect_err("grit switch must refuse when b.txt is dirty");
    match err {
        Error::WouldOverwrite {
            reason: OverwriteReason::LocalChanges,
            paths,
            ..
        } => assert_eq!(paths, git_paths),
        other => panic!("expected WouldOverwrite LocalChanges, got {other:?}"),
    }
    assert_eq!(
        std::fs::read(tmp.path().join("b.txt")).expect("read b after grit"),
        b_before,
        "grit must preserve dirty b.txt"
    );
    assert_eq!(
        git(tmp.path(), &["rev-parse", "--abbrev-ref", "HEAD"]).trim(),
        "main",
        "grit must stay on main"
    );
}

/// Stat drift on one path must not skip racy content hashing on another (issue #1613 review round 2).
#[test]
fn switch_refuses_racy_second_path_when_first_has_stat_drift_only_like_git() {
    use std::path::Path;

    use filetime::FileTime;
    use grit_lib::index::{entry_from_metadata, Index, MODE_REGULAR};
    use grit_lib::objects::ObjectKind;

    fn pin_mtime(path: &Path, sec: u32, nsec: u32) {
        filetime::set_file_mtime(path, FileTime::from_unix_time(i64::from(sec), nsec))
            .expect("set mtime");
    }

    const CONTENT_MAIN_A: &str = "0123456789";
    const CONTENT_MAIN_B: &str = "1111111111";
    const CONTENT_TARGET_A: &str = "9876543210";
    const CONTENT_TARGET_B: &str = "2222222222";
    const CONTENT_DIRTY_B: &str = "5555555555";
    assert_eq!(CONTENT_MAIN_B.len(), CONTENT_DIRTY_B.len());

    let tmp = tempfile::tempdir().expect("tempdir");
    git(tmp.path(), &["init", "-q", "-b", "main", "."]);
    git(tmp.path(), &["config", "user.email", "t@example.com"]);
    git(tmp.path(), &["config", "user.name", "Test"]);
    git(tmp.path(), &["config", "core.trustctime", "false"]);
    git(tmp.path(), &["config", "core.checkstat", "minimal"]);
    git(tmp.path(), &["config", "core.usenanosec", "false"]);

    std::fs::write(tmp.path().join("a.txt"), CONTENT_MAIN_A).expect("write a");
    std::fs::write(tmp.path().join("b.txt"), CONTENT_MAIN_B).expect("write b");
    git(tmp.path(), &["add", "a.txt", "b.txt"]);
    git(tmp.path(), &["commit", "-qm", "main"]);

    git(tmp.path(), &["checkout", "-q", "-b", "target"]);
    std::fs::write(tmp.path().join("a.txt"), CONTENT_TARGET_A).expect("write a target");
    std::fs::write(tmp.path().join("b.txt"), CONTENT_TARGET_B).expect("write b target");
    git(tmp.path(), &["add", "a.txt", "b.txt"]);
    git(tmp.path(), &["commit", "-qm", "target"]);
    git(tmp.path(), &["checkout", "-q", "main"]);

    let grit_repo = Repository::open(
        &tmp.path().join(".git"),
        &RepositoryOptions::empty().with_work_tree(tmp.path()),
    )
    .expect("open grit repo");
    let loaded = grit_repo.index_handle().load().expect("index");
    let entry_a = loaded.get(b"a.txt", 0).expect("a indexed").clone();
    let entry_b = loaded.get(b"b.txt", 0).expect("b indexed");
    let index_mtime = loaded.source_mtime.expect("index source mtime");
    let entry_b_mtime = (entry_b.mtime_sec, entry_b.mtime_nsec);

    let a_path = tmp.path().join("a.txt");
    let b_path = tmp.path().join("b.txt");
    std::fs::write(&a_path, CONTENT_MAIN_A).expect("main a bytes");
    pin_mtime(&a_path, 1_900_000_000, 0);
    std::fs::write(&b_path, CONTENT_DIRTY_B).expect("dirty same-size b");
    pin_mtime(&b_path, entry_b_mtime.0, entry_b_mtime.1);

    let meta_b = std::fs::symlink_metadata(&b_path).expect("b meta");
    let stale_b_oid = grit_repo
        .odb()
        .hash(ObjectKind::Blob, CONTENT_MAIN_B.as_bytes())
        .expect("hash main b");
    let mut racy_index = Index::new();
    racy_index.add_or_replace(entry_a);
    racy_index.add_or_replace(entry_from_metadata(
        &meta_b,
        b"b.txt",
        stale_b_oid,
        MODE_REGULAR,
    ));
    racy_index.sort();
    let index_path = tmp.path().join(".git").join("index");
    racy_index.write(&index_path).expect("write racy index");
    pin_mtime(&index_path, index_mtime.0, index_mtime.1);

    let git_switch = Command::new("git")
        .current_dir(tmp.path())
        .args(["switch", "target"])
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .status()
        .expect("git switch");
    assert!(
        !git_switch.success(),
        "system git must refuse switch when b.txt is racily dirty"
    );
    let b_before = std::fs::read_to_string(&b_path).expect("read b before");
    assert_eq!(b_before, CONTENT_DIRTY_B);

    let target_oid =
        ObjectId::from_hex(git(tmp.path(), &["rev-parse", "target"]).trim()).expect("target oid");
    let git_paths = git_switch_refused_paths(tmp.path(), "target");
    let err = switch_repository(
        &grit_repo,
        &SwitchOptions {
            mode: SwitchMode::Existing {
                branch: "target".into(),
                oid: target_oid,
            },
            dry_run: false,
            hooks: grit_lib::PostOperationHooks::Run,
        },
    )
    .expect_err("grit switch must refuse racy dirty b.txt");
    match err {
        Error::WouldOverwrite {
            reason: OverwriteReason::LocalChanges,
            paths,
            ..
        } => assert_eq!(paths, git_paths),
        other => panic!("expected WouldOverwrite LocalChanges, got {other:?}"),
    }
    assert_eq!(
        std::fs::read_to_string(&b_path).expect("read b after grit"),
        CONTENT_DIRTY_B,
        "grit must preserve racily dirty b.txt"
    );
    assert_eq!(
        git(tmp.path(), &["rev-parse", "--abbrev-ref", "HEAD"]).trim(),
        "main",
        "grit must stay on main"
    );
}

/// Non-racy index entry must not skip worktree validation when the target tree deletes the path.
#[test]
fn switch_refuses_dirty_file_deleted_on_target_like_git() {
    use std::thread;
    use std::time::Duration;

    let tmp = tempfile::tempdir().expect("tempdir");
    git(tmp.path(), &["init", "-q", "-b", "main", "."]);
    git(tmp.path(), &["config", "user.email", "t@example.com"]);
    git(tmp.path(), &["config", "user.name", "Test"]);

    std::fs::write(tmp.path().join("victim.txt"), b"tracked\n").expect("write");
    git(tmp.path(), &["add", "victim.txt"]);
    git(tmp.path(), &["commit", "-qm", "main"]);

    git(tmp.path(), &["checkout", "-q", "-b", "target"]);
    git(tmp.path(), &["rm", "victim.txt"]);
    git(tmp.path(), &["commit", "-qm", "drop victim"]);
    git(tmp.path(), &["checkout", "-q", "main"]);

    thread::sleep(Duration::from_secs(2));
    let refresh = Command::new("git")
        .current_dir(tmp.path())
        .args(["update-index", "--refresh"])
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .status()
        .expect("git update-index --refresh");
    assert!(
        refresh.success() || refresh.code() == Some(1),
        "git update-index --refresh failed: {refresh:?}"
    );
    std::fs::write(tmp.path().join("victim.txt"), b"dirty local\n").expect("dirty");
    let before = std::fs::read(tmp.path().join("victim.txt")).expect("read before");

    let git_switch = Command::new("git")
        .current_dir(tmp.path())
        .args(["switch", "target"])
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .status()
        .expect("git switch");
    assert!(
        !git_switch.success(),
        "system git must refuse switch when victim.txt is dirty but deleted on target"
    );
    assert_eq!(
        std::fs::read(tmp.path().join("victim.txt")).expect("read after git"),
        before,
        "git must preserve dirty victim.txt"
    );
    let head_after_git = git(tmp.path(), &["rev-parse", "--abbrev-ref", "HEAD"]);
    assert!(
        head_after_git.trim() == "main",
        "git must stay on main, got {head_after_git}"
    );

    git(tmp.path(), &["checkout", "-q", "main"]);
    thread::sleep(Duration::from_secs(2));
    let refresh = Command::new("git")
        .current_dir(tmp.path())
        .args(["update-index", "--refresh"])
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .status()
        .expect("git update-index --refresh");
    assert!(
        refresh.success() || refresh.code() == Some(1),
        "git update-index --refresh failed: {refresh:?}"
    );
    std::fs::write(tmp.path().join("victim.txt"), b"dirty local\n").expect("dirty again");

    let grit_repo = Repository::open(
        &tmp.path().join(".git"),
        &RepositoryOptions::empty().with_work_tree(tmp.path()),
    )
    .expect("open grit repo");
    let target_oid =
        ObjectId::from_hex(git(tmp.path(), &["rev-parse", "target"]).trim()).expect("target oid");

    let git_paths = git_switch_refused_paths(tmp.path(), "target");
    let err = switch_repository(
        &grit_repo,
        &SwitchOptions {
            mode: SwitchMode::Existing {
                branch: "target".into(),
                oid: target_oid,
            },
            dry_run: false,
            hooks: grit_lib::PostOperationHooks::Run,
        },
    )
    .expect_err("grit must refuse switch like git");
    match err {
        Error::WouldOverwrite {
            reason: OverwriteReason::LocalChanges,
            paths,
            ..
        } => assert_eq!(paths, git_paths),
        other => panic!("expected WouldOverwrite LocalChanges, got {other:?}"),
    }
    assert_eq!(
        std::fs::read(tmp.path().join("victim.txt")).expect("read after grit"),
        before,
        "grit must preserve dirty victim.txt"
    );
    let head_after_grit = git(tmp.path(), &["rev-parse", "--abbrev-ref", "HEAD"]);
    assert!(
        head_after_grit.trim() == "main",
        "grit must stay on main, got {head_after_grit}"
    );
}

#[cfg(unix)]
#[test]
fn switch_tree_diff_type_and_mode_round_trip_git_clean() {
    use std::os::unix::fs::symlink;

    let tmp = tempfile::tempdir().expect("tempdir");
    git(tmp.path(), &["init", "-q", "-b", "main", "."]);
    git(tmp.path(), &["config", "user.email", "t@example.com"]);
    git(tmp.path(), &["config", "user.name", "Test"]);

    std::fs::write(tmp.path().join("run.sh"), b"#!/bin/sh\n").expect("write");
    std::fs::write(tmp.path().join("plain.txt"), b"file\n").expect("write");
    git(tmp.path(), &["add", "-A"]);
    git(tmp.path(), &["commit", "-qm", "main"]);

    git(tmp.path(), &["checkout", "-q", "-b", "target"]);
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = std::fs::metadata(tmp.path().join("run.sh"))
            .expect("meta")
            .permissions();
        perms.set_mode(0o100755);
        std::fs::set_permissions(tmp.path().join("run.sh"), perms).expect("chmod");
    }
    std::fs::remove_file(tmp.path().join("plain.txt")).expect("remove file");
    std::fs::create_dir_all(tmp.path().join("plain.txt")).expect("file to dir");
    std::fs::write(tmp.path().join("plain.txt/inner.txt"), b"inner\n").expect("inner");
    symlink("plain.txt/inner.txt", tmp.path().join("link.txt")).expect("symlink");
    git(tmp.path(), &["add", "-A"]);
    git(tmp.path(), &["commit", "-qm", "target"]);
    git(tmp.path(), &["checkout", "-q", "main"]);

    let grit_repo = Repository::open(
        &tmp.path().join(".git"),
        &RepositoryOptions::empty().with_work_tree(tmp.path()),
    )
    .expect("open grit repo");
    let target_oid =
        ObjectId::from_hex(git(tmp.path(), &["rev-parse", "target"]).trim()).expect("target oid");

    switch_repository(
        &grit_repo,
        &SwitchOptions {
            mode: SwitchMode::Existing {
                branch: "target".into(),
                oid: target_oid,
            },
            dry_run: false,
            hooks: grit_lib::PostOperationHooks::Run,
        },
    )
    .expect("switch");

    assert!(
        git_porcelain(tmp.path()).trim().is_empty(),
        "git status --porcelain must be empty after type/mode switch, got:\n{}",
        git_porcelain(tmp.path())
    );
    assert!(
        git_diff_index_head(tmp.path()).trim().is_empty(),
        "git diff-index HEAD must be empty, got:\n{}",
        git_diff_index_head(tmp.path())
    );
    git_fsck(tmp.path());

    switch_repository(
        &grit_repo,
        &SwitchOptions {
            mode: SwitchMode::Existing {
                branch: "main".into(),
                oid: ObjectId::from_hex(git(tmp.path(), &["rev-parse", "main"]).trim())
                    .expect("main oid"),
            },
            dry_run: false,
            hooks: grit_lib::PostOperationHooks::Run,
        },
    )
    .expect("switch back");
    assert!(
        git_porcelain(tmp.path()).trim().is_empty(),
        "git status must be clean after switch back"
    );
}

fn gitlink_commit_oid(root: &std::path::Path) -> String {
    use std::io::Write;
    use std::process::Command;
    let mut child = Command::new("git")
        .current_dir(root)
        .args(["hash-object", "-w", "--stdin"])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .spawn()
        .expect("hash blob");
    child
        .stdin
        .take()
        .expect("stdin")
        .write_all(b"sub\n")
        .expect("write blob");
    let blob = String::from_utf8(child.wait_with_output().expect("blob").stdout)
        .expect("utf8")
        .trim()
        .to_owned();
    let tree = format!("100644 blob {blob}\tf\n");
    let mut mktree = Command::new("git")
        .current_dir(root)
        .args(["mktree"])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .spawn()
        .expect("mktree");
    mktree
        .stdin
        .take()
        .expect("stdin")
        .write_all(tree.as_bytes())
        .expect("tree");
    let sub_tree = String::from_utf8(mktree.wait_with_output().expect("tree out").stdout)
        .expect("utf8")
        .trim()
        .to_owned();
    let commit = Command::new("git")
        .current_dir(root)
        .args(["commit-tree", &sub_tree, "-m", "sub"])
        .env("GIT_AUTHOR_NAME", "Test")
        .env("GIT_AUTHOR_EMAIL", "t@example.com")
        .env("GIT_COMMITTER_NAME", "Test")
        .env("GIT_COMMITTER_EMAIL", "t@example.com")
        .stdout(std::process::Stdio::piped())
        .spawn()
        .expect("commit-tree");
    String::from_utf8(commit.wait_with_output().expect("sub commit").stdout)
        .expect("utf8")
        .trim()
        .to_owned()
}

#[test]
fn switch_removes_gitlink_when_parent_directory_dropped() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();
    git(root, &["init", "-q", "-b", "main", "."]);
    git(root, &["config", "user.email", "t@example.com"]);
    git(root, &["config", "user.name", "Test"]);
    git(root, &["config", "gc.auto", "0"]);

    std::fs::create_dir_all(root.join("d/sub")).expect("mkdir d/sub");
    std::fs::write(root.join("d/a.txt"), b"in d\n").expect("write a");
    let sub_oid = gitlink_commit_oid(root);
    git(root, &["add", "d/a.txt"]);
    git(
        root,
        &[
            "update-index",
            "--add",
            "--cacheinfo",
            &format!("160000,{sub_oid},d/sub"),
        ],
    );
    git(root, &["commit", "-qm", "main with d and gitlink"]);

    git(root, &["checkout", "-q", "-b", "target"]);
    git(root, &["rm", "-f", "d/a.txt", "d/sub"]);
    git(root, &["commit", "-qm", "drop d"]);
    git(root, &["checkout", "-q", "main"]);

    git(root, &["switch", "-q", "target"]);
    assert!(
        git_porcelain(root).trim().is_empty(),
        "git switch must leave clean index after removing d with gitlink"
    );
    git(root, &["checkout", "-q", "main"]);

    let grit_repo = Repository::open(
        &root.join(".git"),
        &RepositoryOptions::empty().with_work_tree(root),
    )
    .expect("open grit repo");
    let target_oid =
        ObjectId::from_hex(git(root, &["rev-parse", "target"]).trim()).expect("target oid");

    switch_repository(
        &grit_repo,
        &SwitchOptions {
            mode: SwitchMode::Existing {
                branch: "target".into(),
                oid: target_oid,
            },
            dry_run: false,
            hooks: grit_lib::PostOperationHooks::Run,
        },
    )
    .expect("grit switch");

    let porcelain = git_porcelain(root);
    assert!(
        porcelain.trim().is_empty(),
        "git status --porcelain must be empty after grit removes d/sub gitlink, got:\n{porcelain}"
    );
    let ls_files = git(root, &["ls-files", "-s"]);
    assert!(
        !ls_files.lines().any(|line| line.ends_with(" d/sub")),
        "index must not retain d/sub gitlink entry, got:\n{ls_files}"
    );
}

fn init_nested_inner_repo(inner: &std::path::Path) {
    git(inner, &["init", "-q", "-b", "main"]);
    git(inner, &["config", "user.email", "t@example.com"]);
    git(inner, &["config", "user.name", "Test"]);
}

#[test]
fn switch_dirty_gitlink_on_changing_gitlink_matches_git() {
    use std::process::Command;

    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();
    git(root, &["init", "-q", "-b", "main", "."]);
    git(root, &["config", "user.email", "t@example.com"]);
    git(root, &["config", "user.name", "Test"]);

    let inner = root.join("inner");
    std::fs::create_dir_all(&inner).expect("mkdir inner");
    init_nested_inner_repo(&inner);
    std::fs::write(inner.join("f"), b"v1\n").expect("write");
    git(&inner, &["add", "f"]);
    git(&inner, &["commit", "-qm", "v1"]);
    let sub1 = git(&inner, &["rev-parse", "HEAD"]).trim().to_owned();
    std::fs::write(inner.join("f"), b"v1\nv2\n").expect("v2");
    git(&inner, &["add", "f"]);
    git(&inner, &["commit", "-qm", "v2"]);
    let sub2 = git(&inner, &["rev-parse", "HEAD"]).trim().to_owned();
    git(&inner, &["checkout", "-q", &sub1]);

    git(root, &["add", "inner"]);
    git(root, &["commit", "-qm", "main"]);

    git(root, &["checkout", "-q", "-b", "target"]);
    git(&inner, &["checkout", "-q", &sub2]);
    git(root, &["add", "inner"]);
    git(root, &["commit", "-qm", "target"]);
    git(root, &["checkout", "-q", "main"]);

    git(&inner, &["checkout", "-q", &sub2]);
    std::fs::write(inner.join("f"), b"v1\nv2\ndirty\n").expect("dirty inner");

    let git_status_before = git_porcelain(root);
    assert!(
        git_status_before.contains("inner"),
        "git must see dirty gitlink before switch, got:\n{git_status_before}"
    );

    let git_switch = Command::new("git")
        .current_dir(root)
        .args(["switch", "target"])
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .status()
        .expect("git switch");
    assert!(
        git_switch.success(),
        "system git switch must succeed like dirty submodule case"
    );
    let git_inner_after = std::fs::read_to_string(inner.join("f")).expect("git inner");
    assert!(
        git_inner_after.contains("dirty"),
        "git must preserve dirty submodule content"
    );
    git(root, &["checkout", "-q", "main"]);

    git(&inner, &["checkout", "-q", &sub2]);
    std::fs::write(inner.join("f"), b"v1\nv2\ndirty\n").expect("dirty again");

    let grit_repo = Repository::open(
        &root.join(".git"),
        &RepositoryOptions::empty().with_work_tree(root),
    )
    .expect("open grit repo");
    let target_oid =
        ObjectId::from_hex(git(root, &["rev-parse", "target"]).trim()).expect("target oid");

    switch_repository(
        &grit_repo,
        &SwitchOptions {
            mode: SwitchMode::Existing {
                branch: "target".into(),
                oid: target_oid,
            },
            dry_run: false,
            hooks: grit_lib::PostOperationHooks::Run,
        },
    )
    .expect("grit switch must match git and preserve dirty gitlink worktree");

    let grit_inner_after = std::fs::read_to_string(inner.join("f")).expect("grit inner");
    assert_eq!(
        grit_inner_after, git_inner_after,
        "grit must preserve inner worktree like git"
    );
    assert!(
        git_porcelain(root).contains("inner"),
        "porcelain must still report modified inner after grit switch"
    );
}

#[test]
fn switch_scoped_worktree_diff_limits_metadata_probes() {
    use grit_lib::{reset_worktree_metadata_probe, worktree_metadata_probe_count};

    let tmp = tempfile::tempdir().expect("tempdir");
    git(tmp.path(), &["init", "-q", "-b", "main", "."]);
    git(tmp.path(), &["config", "user.email", "t@example.com"]);
    git(tmp.path(), &["config", "user.name", "Test"]);
    git(tmp.path(), &["config", "gc.auto", "0"]);

    populate_initial(tmp.path());
    build_target_branch(tmp.path());
    std::fs::write(tmp.path().join("d000/f9999.txt"), b"off-tree dirty\n").expect("off-tree dirty");

    let grit_repo = Repository::open(
        &tmp.path().join(".git"),
        &RepositoryOptions::empty().with_work_tree(tmp.path()),
    )
    .expect("open grit repo");
    let target_oid =
        ObjectId::from_hex(git(tmp.path(), &["rev-parse", "target"]).trim()).expect("target oid");

    let _probe = reset_worktree_metadata_probe(tmp.path());
    switch_repository(
        &grit_repo,
        &SwitchOptions {
            mode: SwitchMode::Existing {
                branch: "target".into(),
                oid: target_oid,
            },
            dry_run: false,
            hooks: grit_lib::PostOperationHooks::Run,
        },
    )
    .expect("switch");
    const FILE_COUNT: usize = 500;
    const TOUCH: usize = 40;
    let calls = worktree_metadata_probe_count();
    assert!(
        calls <= TOUCH * 3 + 24,
        "switch scoped worktree diff must not stat all {FILE_COUNT} paths (probes {calls})"
    );
}

#[cfg(unix)]
#[test]
fn switch_tracked_deletes_match_git_when_wholesale_dir_removal_fails() {
    use std::os::unix::fs::PermissionsExt;
    use std::process::Command;

    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();
    git(root, &["init", "-q", "-b", "main", "."]);
    git(root, &["config", "user.email", "t@example.com"]);
    git(root, &["config", "user.name", "Test"]);

    std::fs::create_dir_all(root.join("d005")).expect("mkdir");
    for i in 0..10 {
        std::fs::write(root.join(format!("d005/f{i}.txt")), format!("v{i}\n")).expect("write");
    }
    git(root, &["add", "d005"]);
    git(root, &["commit", "-qm", "main"]);

    git(root, &["checkout", "-q", "-b", "target"]);
    git(root, &["rm", "-r", "d005"]);
    git(root, &["commit", "-qm", "drop d005"]);
    git(root, &["checkout", "-q", "main"]);

    let mut perms = std::fs::metadata(root.join("d005"))
        .expect("meta")
        .permissions();
    perms.set_mode(0o555);
    std::fs::set_permissions(root.join("d005"), perms).expect("chmod d005");

    let git_switch = Command::new("git")
        .current_dir(root)
        .args(["switch", "target"])
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .status()
        .expect("git switch");
    assert!(git_switch.success());
    let git_left: Vec<_> = std::fs::read_dir(root.join("d005"))
        .map(|rd| rd.flatten().map(|e| e.file_name()).collect())
        .unwrap_or_default();

    {
        use std::os::unix::fs::PermissionsExt;
        let mut restore = std::fs::metadata(root.join("d005"))
            .expect("meta")
            .permissions();
        restore.set_mode(0o755);
        std::fs::set_permissions(root.join("d005"), restore).expect("restore d005");
    }
    let _ = std::fs::remove_dir_all(root.join("d005"));
    git(root, &["checkout", "-q", "main"]);
    std::fs::create_dir_all(root.join("d005")).expect("mkdir d005");
    for i in 0..10 {
        std::fs::write(root.join(format!("d005/f{i}.txt")), format!("v{i}\n")).expect("rewrite");
    }
    let mut perms2 = std::fs::metadata(root.join("d005"))
        .expect("meta")
        .permissions();
    perms2.set_mode(0o555);
    std::fs::set_permissions(root.join("d005"), perms2).expect("chmod again");

    let grit_repo = Repository::open(
        &root.join(".git"),
        &RepositoryOptions::empty().with_work_tree(root),
    )
    .expect("open");
    let target_oid =
        ObjectId::from_hex(git(root, &["rev-parse", "target"]).trim()).expect("target oid");
    switch_repository(
        &grit_repo,
        &SwitchOptions {
            mode: SwitchMode::Existing {
                branch: "target".into(),
                oid: target_oid,
            },
            dry_run: false,
            hooks: grit_lib::PostOperationHooks::Run,
        },
    )
    .expect("grit switch");
    let grit_left: Vec<_> = std::fs::read_dir(root.join("d005"))
        .map(|rd| rd.flatten().map(|e| e.file_name()).collect())
        .unwrap_or_default();
    assert_eq!(
        git_left, grit_left,
        "grit must leave the same tracked remnants as git when wholesale removal fails"
    );
}

fn setup_issue_1160_side_worktree(root: &std::path::Path) {
    git(root, &["init", "-q", "-b", "main"]);
    git(root, &["config", "user.email", "t@t"]);
    git(root, &["config", "user.name", "t"]);
    std::fs::write(root.join("a.txt"), b"a\n").expect("a");
    git(root, &["add", "a.txt"]);
    git(root, &["commit", "-qm", "base"]);

    git(root, &["checkout", "-q", "-b", "side"]);
    std::fs::write(root.join("extra.txt"), b"e\n").expect("extra");
    git(root, &["add", "extra.txt"]);
    git(root, &["commit", "-qm", "extra"]);

    std::fs::remove_file(root.join("extra.txt")).expect("unstaged delete extra");
    std::fs::write(root.join("new.txt"), b"n\n").expect("new");
    git(root, &["add", "new.txt"]);
    std::fs::remove_file(root.join("new.txt")).expect("staged add then delete new");
}

fn assert_issue_1160_combined_porcelain(root: &std::path::Path) {
    let porcelain = git_porcelain(root);
    assert!(
        porcelain.contains(" D extra.txt") && porcelain.contains("AD new.txt"),
        "expected git status D extra + AD new, got:\n{porcelain}"
    );
}

/// Regression for <https://github.com/gitbutlerapp/grit/issues/1160>.
#[test]
fn issue_1160_unstaged_delete_with_staged_add_deleted_allows_switch() {
    let git_tmp = tempfile::tempdir().expect("tempdir");
    let git_root = git_tmp.path();
    setup_issue_1160_side_worktree(git_root);
    assert_issue_1160_combined_porcelain(git_root);

    let git_switch = Command::new("git")
        .current_dir(git_root)
        .args(["switch", "main"])
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .status()
        .expect("git switch");
    assert!(git_switch.success(), "system git must allow switch to main");
    let git_after = git_porcelain(git_root);

    let grit_tmp = tempfile::tempdir().expect("tempdir");
    let grit_root = grit_tmp.path();
    setup_issue_1160_side_worktree(grit_root);
    assert_issue_1160_combined_porcelain(grit_root);

    let grit_repo = Repository::open(
        &grit_root.join(".git"),
        &RepositoryOptions::empty().with_work_tree(grit_root),
    )
    .expect("open grit repo");
    let main_oid =
        ObjectId::from_hex(git(grit_root, &["rev-parse", "main"]).trim()).expect("main oid");

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
    .expect("grit switch main with unstaged delete + AD path");

    assert_eq!(
        git_porcelain(grit_root),
        git_after,
        "post-switch porcelain must match git"
    );
    assert_eq!(
        git_diff_index_head(grit_root),
        git(grit_root, &["diff-index", "HEAD"]),
        "post-switch diff-index must match git"
    );
    git_fsck(grit_root);
}

fn setup_issue_1160_dangling_symlink_side_worktree(root: &std::path::Path) {
    use std::os::unix::fs::symlink;

    git(root, &["init", "-q", "-b", "main"]);
    git(root, &["config", "user.email", "t@t"]);
    git(root, &["config", "user.name", "t"]);
    std::fs::write(root.join("a.txt"), b"a\n").expect("a");
    git(root, &["add", "a.txt"]);
    git(root, &["commit", "-qm", "base"]);

    git(root, &["checkout", "-q", "-b", "side"]);
    std::fs::write(root.join("original-target"), b"o\n").expect("target");
    symlink("original-target", root.join("extra-link")).expect("symlink");
    git(root, &["add", "extra-link", "original-target"]);
    git(root, &["commit", "-qm", "extra link"]);

    std::fs::remove_file(root.join("extra-link")).expect("remove link");
    symlink("changed-target", root.join("extra-link")).expect("dangling link");
    std::fs::write(root.join("new.txt"), b"n\n").expect("new");
    git(root, &["add", "new.txt"]);
    std::fs::remove_file(root.join("new.txt")).expect("staged add then delete new");
}

/// Dangling symlink retarget must not be treated as a missing scheduled-delete path.
#[cfg(unix)]
#[test]
fn issue_1160_dangling_symlink_with_staged_add_deleted_refuses_switch() {
    let git_tmp = tempfile::tempdir().expect("tempdir");
    let git_root = git_tmp.path();
    setup_issue_1160_dangling_symlink_side_worktree(git_root);
    let link_before = std::fs::read_link(git_root.join("extra-link")).expect("readlink");

    let git_switch = Command::new("git")
        .current_dir(git_root)
        .args(["switch", "main"])
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .status()
        .expect("git switch");
    assert!(
        !git_switch.success(),
        "system git must refuse switch when extra-link was retargeted"
    );
    assert_eq!(
        std::fs::read_link(git_root.join("extra-link")).expect("readlink"),
        link_before
    );

    let grit_tmp = tempfile::tempdir().expect("tempdir");
    let grit_root = grit_tmp.path();
    setup_issue_1160_dangling_symlink_side_worktree(grit_root);
    let porcelain = git_porcelain(grit_root);
    assert!(
        porcelain.contains("AD new.txt")
            && (porcelain.contains(" M extra-link") || porcelain.contains(" T extra-link")),
        "expected AD new and local extra-link change, got:\n{porcelain}"
    );

    let grit_repo = Repository::open(
        &grit_root.join(".git"),
        &RepositoryOptions::empty().with_work_tree(grit_root),
    )
    .expect("open grit repo");
    let main_oid =
        ObjectId::from_hex(git(grit_root, &["rev-parse", "main"]).trim()).expect("main oid");
    let git_paths = git_switch_refused_paths(grit_root, "main");
    let grit_err = switch_repository(
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
    .expect_err("grit must refuse switch for retargeted dangling symlink");
    match grit_err {
        Error::WouldOverwrite {
            reason: OverwriteReason::LocalChanges,
            paths,
            ..
        } => assert_eq!(paths, git_paths),
        other => panic!("expected WouldOverwrite LocalChanges, got {other:?}"),
    }
    assert_eq!(
        std::fs::read_link(grit_root.join("extra-link")).expect("readlink"),
        std::path::PathBuf::from("changed-target")
    );
}

#[test]
fn switch_bulk_add_mod_delete_leaves_git_clean() {
    let tmp = tempfile::tempdir().expect("tempdir");
    git(tmp.path(), &["init", "-q", "-b", "main", "."]);
    git(tmp.path(), &["config", "user.email", "t@example.com"]);
    git(tmp.path(), &["config", "user.name", "Test"]);
    git(tmp.path(), &["config", "gc.auto", "0"]);

    const N: usize = 40;
    std::fs::create_dir_all(tmp.path().join("tracked")).expect("mkdir tracked");
    for i in 0..N {
        std::fs::write(
            tmp.path().join(format!("tracked/f{i:02}.txt")),
            format!("main {i}\n"),
        )
        .expect("write tracked");
    }
    git(tmp.path(), &["add", "-A"]);
    git(tmp.path(), &["commit", "-qm", "main"]);

    git(tmp.path(), &["checkout", "-q", "-b", "target"]);
    for i in 0..N / 2 {
        std::fs::write(
            tmp.path().join(format!("tracked/f{i:02}.txt")),
            format!("target mod {i}\n"),
        )
        .expect("modify");
    }
    for i in 0..N / 2 {
        std::fs::remove_file(tmp.path().join(format!("tracked/f{:02}.txt", N - 1 - i)))
            .expect("delete");
    }
    std::fs::create_dir_all(tmp.path().join("added")).expect("mkdir added");
    for i in 0..N / 2 {
        std::fs::write(
            tmp.path().join(format!("added/f{i:02}.txt")),
            format!("new {i}\n"),
        )
        .expect("add");
    }
    git(tmp.path(), &["add", "-A"]);
    git(tmp.path(), &["commit", "-qm", "target"]);
    git(tmp.path(), &["checkout", "-q", "main"]);

    let grit_repo = Repository::open(
        &tmp.path().join(".git"),
        &RepositoryOptions::empty().with_work_tree(tmp.path()),
    )
    .expect("open grit repo");
    let target_oid =
        ObjectId::from_hex(git(tmp.path(), &["rev-parse", "target"]).trim()).expect("target oid");
    let main_oid =
        ObjectId::from_hex(git(tmp.path(), &["rev-parse", "main"]).trim()).expect("main oid");

    switch_repository(
        &grit_repo,
        &SwitchOptions {
            mode: SwitchMode::Existing {
                branch: "target".into(),
                oid: target_oid,
            },
            dry_run: false,
            hooks: grit_lib::PostOperationHooks::Run,
        },
    )
    .expect("grit switch to target");
    assert!(
        git_porcelain(tmp.path()).trim().is_empty(),
        "git must be clean after grit switch to target:\n{}",
        git_porcelain(tmp.path())
    );
    for i in 0..N / 2 {
        let body = std::fs::read_to_string(tmp.path().join(format!("added/f{i:02}.txt")))
            .expect("added file");
        assert_eq!(body, format!("new {i}\n"));
    }

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
    .expect("grit switch back to main");
    assert!(
        git_porcelain(tmp.path()).trim().is_empty(),
        "git must be clean after grit switch back to main:\n{}",
        git_porcelain(tmp.path())
    );
    git_fsck(tmp.path());
}

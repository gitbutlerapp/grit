//! Non-UTF-8 file names: status, diff, add, clone, and restore (issue #1399).

#![cfg(unix)]

use std::ffi::OsStr;
use std::io::Write;
use std::os::unix::ffi::OsStrExt;
use std::process::Command;

use grit_lib::config::environment::{Environment, RepositoryOptions};
use grit_lib::diff::diff_index_to_worktree;
use grit_lib::objects::parse_commit;
use grit_lib::objects::ObjectId;
use grit_lib::porcelain::add::{stage, StageMode, StageOptions};
use grit_lib::porcelain::checkout::checkout_between_trees;
use grit_lib::porcelain::restore::{restore_paths, RestoreOptions, RestoreSource};
use grit_lib::porcelain::status::{status, StatusOptions};
use grit_lib::progress::NullProgress;
use grit_lib::remote::clone::{clone as clone_repo, CloneOptions};
use grit_lib::remote::fetch::NoProgress;
use grit_lib::repo::Repository;
use grit_lib::repo_relpath;
use grit_test_support::git;

const BAD_NAME: &[u8] = b"bad\xffname.txt";
const BAD_GITLINK: &[u8] = b"bad\xffsub";
const EMPTY_TREE: &str = "4b825dc642cb6eb9a060e54bf8d69288fbee4904";
const GITLINK_OID: &str = "855827c583bc30645ba427885caa40c5b81764d2";
const RESERVED_LITERAL: &str = "grit+pathhex:6162";

fn bad_path(root: &std::path::Path) -> std::path::PathBuf {
    root.join(OsStr::from_bytes(BAD_NAME))
}

fn open_repo(root: &std::path::Path) -> Repository {
    Repository::open(
        &root.join(".git"),
        &RepositoryOptions::empty().with_work_tree(root),
    )
    .expect("open grit repo")
}

fn path_is_bad_name(stored: &str) -> bool {
    repo_relpath::storage_bytes(stored) == BAD_NAME
}

fn git_stdout(root: &std::path::Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .current_dir(root)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .args(args)
        .output()
        .expect("git");
    assert!(
        out.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).trim().to_owned()
}

fn git_porcelain(root: &std::path::Path) -> String {
    let out = Command::new("git")
        .current_dir(root)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .args(["status", "--short"])
        .output()
        .expect("git status");
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).into_owned()
}

#[test]
fn non_utf8_gitlink_added_by_checkout_between_trees() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path().join("gitlink-add");
    std::fs::create_dir(&root).expect("mkdir");
    git(&root, &["init", "-q", "-b", "main"]);
    git(&root, &["config", "user.email", "t@example.com"]);
    git(&root, &["config", "user.name", "Test"]);
    let cacheinfo = format!("160000,{GITLINK_OID},");
    let mut cacheinfo_os = std::ffi::OsString::from(cacheinfo);
    cacheinfo_os.push(OsStr::from_bytes(BAD_GITLINK));
    let update = Command::new("git")
        .current_dir(&root)
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
    let tree_with_gitlink =
        ObjectId::from_hex(&git_stdout(&root, &["write-tree"])).expect("tree oid");
    let empty_tree = ObjectId::from_hex(EMPTY_TREE).expect("empty tree");

    let repo = open_repo(&root);
    checkout_between_trees(&repo, Some(&empty_tree), &tree_with_gitlink)
        .expect("checkout must materialize non-UTF-8 gitlink");

    let gitlink_dir = root.join(OsStr::from_bytes(BAD_GITLINK));
    assert!(
        gitlink_dir.is_dir(),
        "non-UTF-8 gitlink path must exist as directory: {:?}",
        gitlink_dir
    );
    let loaded = repo.index_handle().load().expect("index");
    assert!(
        loaded.get(BAD_GITLINK, 0).is_some(),
        "index must record gitlink at raw byte path"
    );
}

#[test]
fn non_utf8_up_to_date_blob_skipped_on_checkout_between_trees() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path().join("checkout-skip");
    std::fs::create_dir(&root).expect("mkdir");
    git(&root, &["init", "-q", "-b", "main"]);
    git(&root, &["config", "user.email", "t@example.com"]);
    git(&root, &["config", "user.name", "Test"]);
    std::fs::write(bad_path(&root), b"payload\n").expect("write bad");
    git(&root, &["add", "-A"]);
    git(&root, &["commit", "-qm", "with bad"]);
    let tree_with_bad =
        ObjectId::from_hex(&git_stdout(&root, &["rev-parse", "HEAD^{tree}"])).expect("tree oid");
    let empty_tree = ObjectId::from_hex(EMPTY_TREE).expect("empty tree");

    let repo = open_repo(&root);
    std::fs::remove_file(bad_path(&root)).expect("clear worktree");
    checkout_between_trees(&repo, Some(&empty_tree), &tree_with_bad)
        .expect("first checkout creates non-UTF-8 file");
    checkout_between_trees(&repo, Some(&empty_tree), &tree_with_bad)
        .expect("second checkout must skip up-to-date non-UTF-8 blob");
    assert_eq!(
        std::fs::read(bad_path(&root)).expect("read bad"),
        b"payload\n"
    );
}

#[test]
fn non_utf8_file_deleted_by_checkout_between_trees() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path().join("checkout-del");
    std::fs::create_dir(&root).expect("mkdir");
    git(&root, &["init", "-q", "-b", "main"]);
    git(&root, &["config", "user.email", "t@example.com"]);
    git(&root, &["config", "user.name", "Test"]);
    std::fs::write(bad_path(&root), b"v1\n").expect("write bad");
    git(&root, &["add", "-A"]);
    git(&root, &["commit", "-qm", "with bad"]);
    let tree_with_bad =
        ObjectId::from_hex(&git_stdout(&root, &["rev-parse", "HEAD^{tree}"])).expect("tree oid");

    std::fs::remove_file(bad_path(&root)).expect("remove bad");
    git(&root, &["add", "-A"]);
    git(&root, &["commit", "-qm", "without bad"]);
    let tree_without_bad =
        ObjectId::from_hex(&git_stdout(&root, &["rev-parse", "HEAD^{tree}"])).expect("tree oid");

    git(&root, &["reset", "--hard", "HEAD~1"]);
    assert!(
        bad_path(&root).exists(),
        "setup: non-UTF-8 file must exist after reset to first commit"
    );

    let repo = open_repo(&root);
    checkout_between_trees(&repo, Some(&tree_with_bad), &tree_without_bad)
        .expect("checkout_between_trees old→new");
    assert!(
        !bad_path(&root).exists(),
        "deleted non-UTF-8 path must be removed from worktree"
    );
    assert!(
        repo.index_handle()
            .load()
            .expect("index")
            .get(BAD_NAME, 0)
            .is_none(),
        "deleted non-UTF-8 path must be removed from index"
    );
}

#[test]
fn non_utf8_tracked_status_diff_add_clone_and_restore() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let r1 = tmp.path().join("r1");
    std::fs::create_dir(&r1).expect("mkdir");
    git(&r1, &["init", "-q", "-b", "main"]);
    git(&r1, &["config", "user.email", "t@example.com"]);
    git(&r1, &["config", "user.name", "Test"]);
    std::fs::write(bad_path(&r1), b"one\n").expect("write bad");
    std::fs::write(r1.join("ok.txt"), b"ok\n").expect("write ok");
    git(&r1, &["add", "-A"]);
    git(&r1, &["commit", "-qm", "base"]);

    let mut bad = std::fs::OpenOptions::new()
        .append(true)
        .open(bad_path(&r1))
        .expect("open bad");
    bad.write_all(b"two\n").expect("append");
    std::fs::remove_file(r1.join("ok.txt")).expect("rm ok");

    let repo = open_repo(&r1);
    let model = status(&repo, &StatusOptions::default(), &mut NullProgress).expect("status");
    let unstaged_paths: Vec<_> = model.unstaged.iter().map(|e| e.path().to_owned()).collect();
    assert!(
        unstaged_paths.iter().any(|p| path_is_bad_name(p)),
        "modified non-UTF-8 path must appear in status: {unstaged_paths:?}"
    );
    assert!(
        unstaged_paths.iter().any(|p| p == "ok.txt"),
        "deleted ok.txt must appear: {unstaged_paths:?}"
    );

    let mut index = repo.index_handle().load().expect("index");
    let diff = diff_index_to_worktree(
        repo.odb(),
        &mut index,
        repo.work_tree().expect("work tree"),
        false,
        false,
    )
    .expect("diff index to worktree");
    assert!(
        diff.iter().any(|e| path_is_bad_name(e.path())),
        "diff must include non-UTF-8 path: {:?}",
        diff.iter().map(|e| e.path()).collect::<Vec<_>>()
    );

    let clone_dest = tmp.path().join("r1c");
    let outcome = clone_repo(
        CloneOptions {
            url: r1.to_string_lossy().into_owned(),
            dest: clone_dest.clone(),
            environment: Environment::capture_process(),
            remote_name: grit_lib::remote::DEFAULT_REMOTE.to_owned(),

            thread_policy: grit_lib::parallel::ThreadPolicy::default(),
            depth: None,
        },
        &mut NoProgress,
        None,
    )
    .expect("grit clone");
    let commit = outcome
        .repo
        .odb()
        .read(&outcome.checkout_oid)
        .expect("tip commit");
    let parsed = parse_commit(&commit.data).expect("parse commit");
    checkout_between_trees(&outcome.repo, None, &parsed.tree).expect("checkout");
    assert!(
        bad_path(&clone_dest).exists(),
        "clone must create file with original byte name"
    );
    assert!(
        git_porcelain(&clone_dest).trim().is_empty(),
        "clone worktree must be clean for git"
    );

    restore_paths(
        &repo,
        &RestoreOptions {
            pathspecs: vec![".".to_owned()],
            pathspec_sources: vec![".".to_owned()],
            source: RestoreSource::Index,
            staged: false,
            worktree: true,
            stage: None,
            remerge: None,
        },
    )
    .expect("restore worktree from index");
    assert_eq!(
        std::fs::read(bad_path(&r1)).expect("read restored bad"),
        b"one\n",
        "restore must revert non-UTF-8 tracked file to index blob"
    );
    assert!(
        git_porcelain(&r1).trim().is_empty(),
        "worktree must be clean after restore: {}",
        git_porcelain(&r1)
    );

    let r1b = tmp.path().join("r1b");
    std::fs::create_dir(&r1b).expect("mkdir r1b");
    git(&r1b, &["init", "-q", "-b", "main"]);
    std::fs::write(bad_path(&r1b), b"x\n").expect("bad");
    std::fs::write(r1b.join("good.txt"), b"y\n").expect("good");
    let r1b_repo = open_repo(&r1b);
    stage(
        &r1b_repo,
        &StageOptions {
            pathspecs: Vec::new(),
            pathspec_sources: Vec::new(),
            mode: StageMode::All,
            ..StageOptions::default()
        },
        &mut NullProgress,
    )
    .expect("stage all");
    let porcelain = git_porcelain(&r1b);
    assert!(
        porcelain.contains("good.txt"),
        "good.txt should be staged: {porcelain}"
    );
    assert!(
        porcelain.lines().count() >= 2,
        "both paths should be staged: {porcelain}"
    );

    std::fs::write(r1b.join(RESERVED_LITERAL), b"z\n").expect("reserved name file");
    stage(
        &r1b_repo,
        &StageOptions {
            pathspecs: Vec::new(),
            pathspec_sources: Vec::new(),
            mode: StageMode::All,
            ..StageOptions::default()
        },
        &mut NullProgress,
    )
    .expect("stage file whose name looks like hex encoding");
    assert!(
        r1b_repo
            .index_handle()
            .load()
            .expect("index")
            .get(RESERVED_LITERAL.as_bytes(), 0)
            .is_some(),
        "literal grit+pathhex:* filename must stage under its UTF-8 spelling"
    );
}

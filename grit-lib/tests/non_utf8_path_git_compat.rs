//! Non-UTF-8 file names: status, diff, add, clone, and restore (issue #1399).

#![cfg(unix)]

use grit_lib::diff::TreeDiffOptions;
use grit_lib::worktree::WorktreeError;
use std::ffi::OsStr;
use std::io::Write;
use std::os::unix::ffi::OsStrExt;
use std::process::Command;

use grit_lib::command_runner::hooks::PostOperationHooks;
use grit_lib::config::environment::RepositoryOptions;
use grit_lib::index::untracked_cache::{parse_untracked_extension, UntrackedCacheDir};
use grit_lib::objects::parse_commit;
use grit_lib::objects::ObjectId;
use grit_lib::porcelain::add::{stage, StageMode, StageOptions};
use grit_lib::porcelain::checkout::checkout_between_trees;
use grit_lib::porcelain::commit::{commit_worktree, CommitRequest};
use grit_lib::porcelain::merge::{integrate_branch, IntegrateBranchOutcome};
use grit_lib::porcelain::restore::{restore_paths, RestoreOptions, RestoreSource};
use grit_lib::porcelain::status::{status, StatusOptions, UntrackedMode};
use grit_lib::porcelain::switch::{switch_repository, SwitchMode, SwitchOptions};
use grit_lib::progress::CancelToken;
use grit_lib::progress::NullProgress;
use grit_lib::progress::NullProgress;
use grit_lib::remote::clone::{clone as clone_repo, CloneOptions};
use grit_lib::repo::Repository;
use grit_lib::repo_relpath;
use grit_test_support::git;

const BAD_NAME: &[u8] = b"bad\xffname.txt";
const BAD_GITLINK: &[u8] = b"bad\xffsub";
const UNTRACKED_CAFE: &[u8] = b"caf\xe9.txt";
const UNTRACKED_SUB_FILE: &[u8] = b"sub/\xff.txt";
const UNTRACKED_NON_UTF8_DIR_FILE: &[u8] = b"\xfe/a.txt";
const EMPTY_TREE: &str = "4b825dc642cb6eb9a060e54bf8d69288fbee4904";
const GITLINK_OID: &str = "855827c583bc30645ba427885caa40c5b81764d2";
const RESERVED_LITERAL: &str = "grit+pathhex:6162";
const ISSUE_2513_FILE: &[u8] = b"base\xff.txt";

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

fn stored_path_is(stored: &str, raw: &[u8]) -> bool {
    repo_relpath::storage_bytes(stored) == raw
}

fn write_non_utf8_path(root: &std::path::Path, rel: &[u8], contents: &[u8]) {
    let path = root.join(OsStr::from_bytes(rel));
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).expect("mkdir parents");
    }
    std::fs::write(path, contents).expect("write non-utf8 path");
}

fn git_ls_files_z(root: &std::path::Path) -> Vec<Vec<u8>> {
    let out = Command::new("git")
        .current_dir(root)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .args(["ls-files", "-z"])
        .output()
        .expect("git ls-files");
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    out.stdout
        .split(|&b| b == 0)
        .filter(|p| !p.is_empty())
        .map(|p| p.to_vec())
        .collect()
}

fn index_has_untr_extension(index_path: &std::path::Path) -> bool {
    let data = std::fs::read(index_path).expect("read index");
    data.windows(4).any(|w| w == b"UNTR")
}

fn untr_extension_names_have_lossy_replacement(index_path: &std::path::Path) -> bool {
    let data = std::fs::read(index_path).expect("read index");
    let sig = b"UNTR";
    let Some(start) = data.windows(4).position(|w| w == sig) else {
        return false;
    };
    let size = u32::from_be_bytes(data[start + 4..start + 8].try_into().expect("size"));
    let body = &data[start + 8..start + 8 + size as usize];
    let Some(uc) = parse_untracked_extension(body) else {
        return true;
    };
    fn walk(dir: &UntrackedCacheDir) -> bool {
        if dir.name.contains('\u{FFFD}') {
            return true;
        }
        for n in &dir.untracked {
            if n.contains('\u{FFFD}') {
                return true;
            }
        }
        dir.dirs.iter().any(walk)
    }
    uc.root.as_ref().is_some_and(walk)
}

fn exercise_non_utf8_untracked_paths(untracked_cache_enabled: bool) {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path().join("untracked-non-utf8");
    std::fs::create_dir(&root).expect("mkdir");
    git(&root, &["init", "-q", "-b", "main"]);
    git(&root, &["config", "user.email", "t@example.com"]);
    git(&root, &["config", "user.name", "Test"]);
    if untracked_cache_enabled {
        git(&root, &["config", "core.untrackedCache", "true"]);
    } else {
        git(&root, &["config", "core.untrackedCache", "false"]);
    }
    std::fs::write(root.join("base"), b"base\n").expect("base");
    git(&root, &["add", "base"]);
    git(&root, &["commit", "-qm", "base"]);
    if untracked_cache_enabled {
        assert!(
            index_has_untr_extension(&root.join(".git/index")),
            "baseline commit with core.untrackedCache=true must populate UNTR"
        );
    }

    write_non_utf8_path(&root, UNTRACKED_CAFE, b"x\n");
    write_non_utf8_path(&root, UNTRACKED_SUB_FILE, b"y\n");

    let repo = open_repo(&root);
    let model = status(
        &repo,
        &StatusOptions {
            untracked: UntrackedMode::All,
            ..StatusOptions::default()
        },
        &mut NullProgress,
        &CancelToken::never(),
    )
    .expect("status");
    assert!(
        model
            .untracked
            .iter()
            .any(|p| stored_path_is(p, UNTRACKED_CAFE)),
        "café untracked must use pathhex encoding: {:?}",
        model.untracked
    );
    assert!(
        model
            .untracked
            .iter()
            .any(|p| stored_path_is(p, UNTRACKED_SUB_FILE)),
        "sub non-utf8 untracked must use pathhex encoding: {:?}",
        model.untracked
    );

    let mut dry = CommitRequest::new("dry-run");
    dry.dry_run = true;
    let preview = commit_worktree(&repo, &dry, &mut NullProgress, &CancelToken::never())
        .expect("commit dry-run before staging");
    assert!(
        preview
            .files
            .iter()
            .any(|f| stored_path_is(&f.path, UNTRACKED_CAFE)),
        "dry-run file list must use pathhex, not lossy UTF-8: {:?}",
        preview.files
    );

    let index_path = root.join(".git/index");
    let _ = status(
        &repo,
        &StatusOptions::default(),
        &mut NullProgress,
        &CancelToken::never(),
    )
    .expect("second status while paths remain untracked");
    if untracked_cache_enabled {
        assert!(
            !untr_extension_names_have_lossy_replacement(&index_path),
            "UNTR extension must not store U+FFFD replacement names after refresh"
        );
    }

    stage(
        &repo,
        &StageOptions {
            pathspecs: Vec::new(),
            pathspec_sources: Vec::new(),
            mode: StageMode::All,
            ..StageOptions::default()
        },
        &mut NullProgress,
        &CancelToken::never(),
    )
    .expect("stage all untracked");

    commit_worktree(
        &repo,
        &CommitRequest::new("add non-utf8 untracked"),
        &mut NullProgress,
        &CancelToken::never(),
    )
    .expect("commit staged non-utf8 untracked");

    let ls = git_ls_files_z(&root);
    assert!(
        ls.iter().any(|p| p == UNTRACKED_CAFE),
        "git ls-files must list café under raw bytes: {ls:?}"
    );
    assert!(
        ls.iter().any(|p| p == UNTRACKED_SUB_FILE),
        "git ls-files must list sub file under raw bytes: {ls:?}"
    );
    git(&root, &["fsck", "--strict"]);
}

#[test]
fn non_utf8_untracked_directory_component_with_untr() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path().join("untr-non-utf8-dir");
    std::fs::create_dir(&root).expect("mkdir");
    git(&root, &["init", "-q", "-b", "main"]);
    git(&root, &["config", "user.email", "t@example.com"]);
    git(&root, &["config", "user.name", "Test"]);
    git(&root, &["config", "core.untrackedCache", "true"]);
    std::fs::write(root.join("base"), b"base\n").expect("base");
    git(&root, &["add", "base"]);
    git(&root, &["commit", "-qm", "base"]);
    assert!(
        index_has_untr_extension(&root.join(".git/index")),
        "UNTR must exist after baseline commit"
    );

    write_non_utf8_path(&root, UNTRACKED_NON_UTF8_DIR_FILE, b"inside\n");

    let repo = open_repo(&root);
    let model = status(
        &repo,
        &StatusOptions::default(),
        &mut NullProgress,
        &CancelToken::never(),
    )
    .expect("status");
    let collapsed = repo_relpath::directory_marker_stored(&repo_relpath::storage_string(b"\xfe"));
    assert!(
        model.untracked.iter().any(|p| p == &collapsed),
        "collapsed untracked dir must be reversible pathhex (fe2f), got {:?}",
        model.untracked
    );

    stage(
        &repo,
        &StageOptions {
            pathspecs: Vec::new(),
            pathspec_sources: Vec::new(),
            mode: StageMode::All,
            ..StageOptions::default()
        },
        &mut NullProgress,
        &CancelToken::never(),
    )
    .expect("stage must not fail on collapsed non-UTF-8 directory path");

    let staged = repo.index().load().expect("index after stage");
    assert!(
        staged.get(UNTRACKED_NON_UTF8_DIR_FILE, 0).is_some(),
        "index must record the file under raw bytes, not only a directory placeholder"
    );
    assert!(
        !staged
            .entries()
            .iter()
            .any(|e| e.path == b"\xfe/" && e.mode != 0o040000),
        "must not stage collapsed marker as an empty blob at \\xfe/"
    );

    commit_worktree(
        &repo,
        &CommitRequest::new("add non-utf8 dir file"),
        &mut NullProgress,
        &CancelToken::never(),
    )
    .expect("commit staged non-UTF-8 path under directory");

    let ls = git_ls_files_z(&root);
    assert!(
        ls.iter().any(|p| p == UNTRACKED_NON_UTF8_DIR_FILE),
        "git ls-files must list raw byte path after commit: {ls:?}"
    );
    git(&root, &["fsck", "--strict"]);
}

#[test]
fn non_utf8_untracked_paths_with_untracked_cache() {
    exercise_non_utf8_untracked_paths(true);
}

#[test]
fn non_utf8_untracked_paths_untracked_cache_disabled() {
    exercise_non_utf8_untracked_paths(false);
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
    let loaded = repo.index().load().expect("index");
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
        repo.index()
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
    let model = status(
        &repo,
        &StatusOptions::default(),
        &mut NullProgress,
        &CancelToken::never(),
    )
    .expect("status");
    let unstaged_paths: Vec<_> = model.unstaged.iter().map(|e| e.path().to_owned()).collect();
    assert!(
        unstaged_paths.iter().any(|p| path_is_bad_name(p)),
        "modified non-UTF-8 path must appear in status: {unstaged_paths:?}"
    );
    assert!(
        unstaged_paths.iter().any(|p| p == "ok.txt"),
        "deleted ok.txt must appear: {unstaged_paths:?}"
    );

    let mut index = repo.index().load().expect("index");
    let diff = repo
        .diff()
        .index_to_worktree(&mut index, &TreeDiffOptions::default())
        .map(|r| r.entries)
        .expect("diff index to worktree");
    assert!(
        diff.iter().any(|e| path_is_bad_name(e.path())),
        "diff must include non-UTF-8 path: {:?}",
        diff.iter().map(|e| e.path()).collect::<Vec<_>>()
    );

    let clone_dest = tmp.path().join("r1c");
    let outcome = clone_repo(
        CloneOptions {
            ..CloneOptions::new(r1.to_string_lossy(), clone_dest.clone())
        },
        &mut NoProgress,
        &CancelToken::never(),
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
        &mut NullProgress,
        &CancelToken::never(),
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
        &CancelToken::never(),
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
    let staged = r1b_repo.index().load().expect("index");
    assert!(
        staged.get(BAD_NAME, 0).is_some(),
        "non-UTF-8 filename must stage under its raw bytes, not the hex-encoded spelling"
    );
    assert!(
        staged
            .entries()
            .iter()
            .all(|e| !e.path.starts_with(b"grit+pathhex:")),
        "no index path may carry the internal hex encoding"
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
        &CancelToken::never(),
    )
    .expect("stage file whose name looks like hex encoding");
    assert!(
        r1b_repo
            .index()
            .load()
            .expect("index")
            .get(RESERVED_LITERAL.as_bytes(), 0)
            .is_some(),
        "literal grit+pathhex:* filename must stage under its UTF-8 spelling"
    );
    let ls = git(&r1b, &["ls-files", "-z"]);
    assert!(
        ls.split('\0').any(|p| p == RESERVED_LITERAL),
        "git must see the literal filename in the index: {ls:?}"
    );
}

/// Switch, merge, and restore on a clean tree must not treat non-UTF-8 index paths as dirty (issue #2513).
#[test]
fn switch_merge_and_restore_non_utf8_tracked_paths_on_clean_tree() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path().join("issue-2513");
    std::fs::create_dir(&root).expect("mkdir");
    git(&root, &["init", "-q", "-b", "main"]);
    git(&root, &["config", "user.email", "a@b"]);
    git(&root, &["config", "user.name", "T"]);
    write_non_utf8_path(&root, ISSUE_2513_FILE, b"a");
    std::fs::write(root.join("p"), b"p\n").expect("p");
    git(&root, &["add", "-A"]);
    git(&root, &["commit", "-qm", "one"]);
    git(&root, &["switch", "-qc", "br"]);
    write_non_utf8_path(&root, ISSUE_2513_FILE, b"br");
    git(&root, &["add", "-A"]);
    git(&root, &["commit", "-qm", "two"]);
    git(&root, &["switch", "-q", "main"]);
    assert!(
        git_porcelain(&root).trim().is_empty(),
        "git status must be clean before grit switch"
    );

    let repo = open_repo(&root);
    let br_oid = ObjectId::from_hex(git(&root, &["rev-parse", "br"]).trim()).expect("br oid");
    switch_repository(
        &repo,
        &SwitchOptions {
            mode: SwitchMode::Existing {
                branch: "br".to_owned(),
                oid: br_oid,
            },
            dry_run: false,
            hooks: PostOperationHooks::Skip,
        },
        &mut NullProgress,
        &CancelToken::never(),
    )
    .expect("grit switch br on clean tree");
    assert_eq!(
        std::fs::read(root.join(OsStr::from_bytes(ISSUE_2513_FILE))).expect("read switched"),
        b"br"
    );
    assert!(
        git_porcelain(&root).trim().is_empty(),
        "worktree clean after grit switch"
    );
    git(&root, &["switch", "-q", "main"]);

    write_non_utf8_path(&root, ISSUE_2513_FILE, b"dirty");
    let dirty_err = switch_repository(
        &open_repo(&root),
        &SwitchOptions {
            mode: SwitchMode::Existing {
                branch: "br".to_owned(),
                oid: br_oid,
            },
            dry_run: false,
            hooks: PostOperationHooks::Skip,
        },
        &mut NullProgress,
        &CancelToken::never(),
    )
    .expect_err("switch must refuse real local edit");
    assert!(
        matches!(
            dirty_err,
            grit_lib::error::Error::Worktree(WorktreeError::WouldOverwrite { .. })
        ),
        "expected WouldOverwrite, got {dirty_err:?}"
    );
    write_non_utf8_path(&root, ISSUE_2513_FILE, b"a");

    std::fs::write(root.join("q"), b"q\n").expect("q");
    git(&root, &["add", "q"]);
    git(&root, &["commit", "-qm", "three"]);
    let main_oid = ObjectId::from_hex(git(&root, &["rev-parse", "HEAD"]).trim()).expect("main oid");
    assert!(git_porcelain(&root).trim().is_empty());

    write_non_utf8_path(&root, ISSUE_2513_FILE, b"dirty");
    let merge_dirty = integrate_branch(
        &open_repo(&root),
        "refs/heads/main",
        main_oid,
        br_oid,
        "br",
        &mut NullProgress,
        &CancelToken::never(),
    )
    .expect_err("merge must refuse unstaged edit");
    assert!(
        matches!(
            merge_dirty,
            grit_lib::error::Error::Worktree(WorktreeError::WouldOverwrite { .. })
        ),
        "expected WouldOverwrite on merge, got {merge_dirty:?}"
    );
    write_non_utf8_path(&root, ISSUE_2513_FILE, b"a");

    let merge_repo = open_repo(&root);
    let merge_out = integrate_branch(
        &merge_repo,
        "refs/heads/main",
        main_oid,
        br_oid,
        "br",
        &mut NullProgress,
        &CancelToken::never(),
    )
    .expect("grit merge br on clean tree");
    let IntegrateBranchOutcome::Merged { .. } = merge_out else {
        panic!("expected merge commit, got {merge_out:?}");
    };
    assert_eq!(
        std::fs::read(root.join(OsStr::from_bytes(ISSUE_2513_FILE))).expect("read merged"),
        b"br"
    );
    assert!(
        git_porcelain(&root).trim().is_empty(),
        "git status after grit merge"
    );
    git(&root, &["fsck", "--strict"]);

    git(&root, &["reset", "-q", "--hard", &main_oid.to_hex()]);
    let br_tree = {
        let repo = open_repo(&root);
        let commit = repo.odb().read(&br_oid).expect("br commit");
        parse_commit(&commit.data).expect("parse br").tree
    };
    restore_paths(
        &open_repo(&root),
        &RestoreOptions {
            pathspecs: vec![".".to_owned()],
            pathspec_sources: vec![".".to_owned()],
            source: RestoreSource::Tree(br_tree),
            staged: false,
            worktree: true,
            stage: None,
            remerge: None,
        },
        &mut NullProgress,
        &CancelToken::never(),
    )
    .expect("restore --source br");
    assert_eq!(
        std::fs::read(root.join(OsStr::from_bytes(ISSUE_2513_FILE))).expect("restored main file"),
        b"br"
    );
    assert!(
        !root
            .join(OsStr::from_bytes(b"base\xef\xbf\xbd.txt"))
            .exists(),
        "restore must not create a U+FFFD replacement filename"
    );
    assert!(
        git_ls_files_z(&root).contains(&ISSUE_2513_FILE.to_vec()),
        "git index must still track raw byte path"
    );
    let grit_status = status(
        &open_repo(&root),
        &StatusOptions::default(),
        &mut NullProgress,
        &CancelToken::never(),
    )
    .expect("grit status after restore");
    assert!(
        grit_status.untracked.is_empty(),
        "restore must not leave a U+FFFD replacement as untracked: {:?}",
        grit_status.untracked
    );
}

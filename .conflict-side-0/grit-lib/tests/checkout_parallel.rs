//! Serial vs parallel checkout must produce identical worktrees and index files.

use filetime::FileTime;
use grit_lib::command_runner::{CommandStdin, RecordedResponse, RecordingRunner};
use grit_lib::config::environment::RepositoryOptions;
use grit_lib::diff::diff_trees;
use grit_lib::index::write_tree::{write_tree_update_index, WriteTreeFlags};
use grit_lib::index::{
    entry_from_stat, Index, IndexEntry, MODE_EXECUTABLE, MODE_GITLINK, MODE_REGULAR,
};
use grit_lib::objects::{serialize_commit, CommitData, ObjectId, ObjectKind};
use grit_lib::parallel::ThreadPolicy;
use grit_lib::porcelain::checkout::checkout_between_trees;
use grit_lib::porcelain::checkout::checkout_tree_changes;
use grit_lib::progress::NullProgress;
use grit_lib::refs;
use grit_lib::repo::{InitOptions, Repository};
use grit_test_support::git_fsck;
use std::collections::BTreeMap;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::process::Command;

const PINNED_MTIME: FileTime = FileTime::from_unix_time(1_700_000_000, 0);
const TEST_COMMITTER: &str = "Test <t@example.com> 1700000000 +0000";

fn policy(threads: usize, min_items: Option<usize>) -> ThreadPolicy {
    ThreadPolicy::with_threads_override(Some(threads)).min_items_override(min_items)
}

fn open_with_policy(root: &Path, threads: usize, min_items: Option<usize>) -> Repository {
    let git_dir = root.join(".git");
    let mut options = RepositoryOptions::empty();
    options.thread_policy = policy(threads, min_items);
    Repository::open(&git_dir, &options.clone().with_work_tree(root)).expect("open repo")
}

fn enable_parallel_checkout_config(root: &Path) {
    let cfg_path = root.join(".git/config");
    let mut cfg = fs::read_to_string(&cfg_path).expect("read config");
    if !cfg.contains("thresholdForParallelism") {
        cfg.push_str("\n[checkout]\n\tworkers = 4\n\tthresholdForParallelism = 0\n");
        fs::write(&cfg_path, cfg).expect("write config");
    }
}

fn open_with_forced_parallel_pool(root: &Path, threads: usize) -> Repository {
    enable_parallel_checkout_config(root);
    open_with_policy(root, threads, Some(0))
}

fn commit_tree(repo: &Repository, message: &str) -> ObjectId {
    use grit_lib::porcelain::add::{stage, StageOptions};
    stage(repo, &StageOptions::default(), &mut NullProgress).expect("stage");
    let mut index = repo.index_handle().load().expect("index");
    let tree = write_tree_update_index(repo.odb(), &mut index, "", WriteTreeFlags::silent())
        .expect("tree");
    repo.index_handle().write(&mut index).expect("write index");
    let parent = refs::resolve_ref(repo.git_dir(), "HEAD").ok();
    let commit_data = CommitData {
        tree,
        parents: parent.into_iter().collect(),
        author: TEST_COMMITTER.to_owned(),
        committer: TEST_COMMITTER.to_owned(),
        author_raw: Vec::new(),
        committer_raw: Vec::new(),
        encoding: None,
        message: format!("{message}\n"),
        raw_message: None,
        extra_headers: Vec::new(),
    };
    let bytes = serialize_commit(&commit_data);
    let commit_oid = repo
        .odb()
        .write(ObjectKind::Commit, &bytes)
        .expect("commit");
    refs::write_ref(repo.git_dir(), "HEAD", &commit_oid).expect("head");
    tree
}

#[derive(Debug, PartialEq, Eq)]
struct WorktreeFile {
    bytes: Vec<u8>,
    mode: u32,
    symlink_target: Option<String>,
}

fn snapshot_worktree(wt: &Path) -> BTreeMap<String, WorktreeFile> {
    let mut out = BTreeMap::new();
    walk(wt, wt, &mut out);
    out
}

fn walk(base: &Path, dir: &Path, out: &mut BTreeMap<String, WorktreeFile>) {
    for ent in fs::read_dir(dir).expect("read_dir") {
        let ent = ent.expect("dirent");
        let path = ent.path();
        let rel = path
            .strip_prefix(base)
            .expect("rel")
            .to_string_lossy()
            .into_owned();
        if rel.starts_with(".git/") || rel == ".git" {
            continue;
        }
        let meta = fs::symlink_metadata(&path).expect("lstat");
        if meta.is_dir() {
            walk(base, &path, out);
            continue;
        }
        let mode = meta.permissions().mode() & 0o7777;
        let git_mode = if meta.file_type().is_symlink() {
            0o120000
        } else if mode & 0o111 != 0 {
            MODE_EXECUTABLE
        } else {
            MODE_REGULAR
        };
        let symlink_target = if meta.file_type().is_symlink() {
            Some(
                fs::read_link(&path)
                    .expect("readlink")
                    .to_string_lossy()
                    .into_owned(),
            )
        } else {
            None
        };
        let bytes = if symlink_target.is_some() {
            Vec::new()
        } else {
            fs::read(&path).expect("read file")
        };
        out.insert(
            rel,
            WorktreeFile {
                bytes,
                mode: git_mode,
                symlink_target,
            },
        );
    }
}

fn pin_worktree_mtrees(wt: &Path) {
    for ent in fs::read_dir(wt).expect("read_dir") {
        let ent = ent.expect("dirent");
        let path = ent.path();
        if path.is_dir() {
            if path.file_name().and_then(|n| n.to_str()) != Some(".git") {
                pin_worktree_mtrees(&path);
            }
            continue;
        }
        filetime::set_file_mtime(&path, PINNED_MTIME).expect("pin mtime");
    }
}

fn assert_trees_match(a: &BTreeMap<String, WorktreeFile>, b: &BTreeMap<String, WorktreeFile>) {
    assert_eq!(a, b, "worktree snapshots must match");
}

fn assert_index_bytes_equal(a: &[u8], b: &[u8]) {
    assert_eq!(a, b, "index bytes must match when mtimes are pinned");
}

fn assert_index_semantically_equal(a: &[u8], b: &[u8]) {
    let left = Index::parse(a).expect("parse index a");
    let right = Index::parse(b).expect("parse index b");
    assert_eq!(
        left.entries().len(),
        right.entries().len(),
        "index entry count"
    );
    for (ea, eb) in left.entries().iter().zip(right.entries().iter()) {
        assert_eq!(ea.path, eb.path, "path");
        assert_eq!(
            ea.oid,
            eb.oid,
            "oid for {}",
            String::from_utf8_lossy(&ea.path)
        );
        assert_eq!(ea.mode, eb.mode, "mode");
    }
}

fn configure_git_user(root: &Path) {
    Command::new("git")
        .args([
            "-C",
            root.to_str().expect("utf8"),
            "config",
            "user.email",
            "t@example.com",
        ])
        .status()
        .expect("email");
    Command::new("git")
        .args([
            "-C",
            root.to_str().expect("utf8"),
            "config",
            "user.name",
            "Test",
        ])
        .status()
        .expect("name");
}

fn assert_git_clean(root: &Path) {
    configure_git_user(root);
    let status = Command::new("git")
        .args(["-C", root.to_str().expect("utf8"), "status", "--porcelain"])
        .output()
        .expect("git status");
    assert!(status.status.success());
    assert!(
        status.stdout.is_empty(),
        "git status --porcelain must be empty: {}",
        String::from_utf8_lossy(&status.stdout)
    );
    let fsck = git_fsck(root, false);
    assert!(
        fsck.ok,
        "git fsck must succeed (msg_ids={:?})",
        fsck.msg_ids
    );
}

fn checkout_diff_with_policy(
    root: &Path,
    from: Option<ObjectId>,
    to: ObjectId,
    threads: usize,
    min_items: Option<usize>,
    pin_mtimes: bool,
) -> (BTreeMap<String, WorktreeFile>, Vec<u8>) {
    let repo = open_with_policy(root, threads, min_items);
    if pin_mtimes {
        if let Some(wt) = repo.work_tree() {
            pin_worktree_mtrees(wt);
        }
    }
    let changes = diff_trees(repo.odb(), from.as_ref(), Some(&to), "").expect("diff");
    checkout_tree_changes(&repo, &changes).expect("checkout");
    let wt = repo.work_tree().expect("wt");
    let snap = snapshot_worktree(wt);
    let index = fs::read(repo.index_handle().path()).expect("index");
    (snap, index)
}

fn reset_worktree_to_tree(root: &Path, from: ObjectId, current: ObjectId) {
    checkout_between_trees(&open_with_policy(root, 1, None), Some(&current), &from)
        .expect("reset tree");
}

fn assert_serial_parallel_match(
    root: &Path,
    from: ObjectId,
    to: ObjectId,
    pin_mtimes: bool,
    index_bytes: bool,
) {
    reset_worktree_to_tree(root, from, to);
    let (serial_wt, serial_idx) =
        checkout_diff_with_policy(root, Some(from), to, 1, None, pin_mtimes);
    reset_worktree_to_tree(root, from, to);
    let (par_wt, par_idx) = checkout_diff_with_policy(root, Some(from), to, 4, Some(0), pin_mtimes);
    assert_trees_match(&serial_wt, &par_wt);
    if index_bytes {
        assert_index_bytes_equal(&serial_idx, &par_idx);
    } else {
        assert_index_semantically_equal(&serial_idx, &par_idx);
    }
}

#[test]
fn parallel_matches_serial_crlf_and_ident() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();
    let repo = Repository::init(
        root,
        &InitOptions::new()
            .with_initial_branch("main".to_owned())
            .with_ref_storage(grit_lib::RefStorageFormat::Files),
    )
    .expect("init");
    fs::write(
        root.join(".gitattributes"),
        "* ident\n*.txt text eol=crlf\n",
    )
    .expect("attrs");
    fs::write(root.join(".git/config"), "[core]\n\tautocrlf = false\n").expect("cfg");
    repo.reload_config().expect("reload");
    let wt = repo.work_tree().expect("wt");
    fs::write(wt.join("a.txt"), b"hello $Id$\n").expect("a");
    fs::write(wt.join("b.txt"), b"line\n").expect("b");
    let from = commit_tree(&repo, "base");
    fs::write(wt.join("a.txt"), b"changed $Id$\n").expect("a2");
    fs::write(wt.join("c.txt"), b"new\n").expect("c");
    let to = commit_tree(&repo, "tip");
    assert_serial_parallel_match(root, from, to, true, false);
    assert_git_clean(root);
}

#[test]
fn parallel_matches_serial_gitlink_and_replacements() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();
    let repo = Repository::init(
        root,
        &InitOptions::new()
            .with_initial_branch("main".to_owned())
            .with_ref_storage(grit_lib::RefStorageFormat::Files),
    )
    .expect("init");
    let wt = repo.work_tree().expect("wt");
    let gitlink: ObjectId = "855827c583bc30645ba427885caa40c5b81764d2"
        .parse()
        .expect("oid");
    fs::create_dir_all(wt.join("sub")).expect("sub");
    fs::write(wt.join("file"), b"v1\n").expect("file");
    let mut index = repo.index_handle().load().expect("index");
    index.add_or_replace(
        entry_from_stat(
            &wt.join("file"),
            b"file",
            repo.odb().write(ObjectKind::Blob, b"v1\n").unwrap(),
            MODE_REGULAR,
        )
        .unwrap(),
    );
    index.add_or_replace(IndexEntry {
        ctime_sec: 0,
        ctime_nsec: 0,
        mtime_sec: 0,
        mtime_nsec: 0,
        dev: 0,
        ino: 0,
        mode: MODE_GITLINK,
        uid: 0,
        gid: 0,
        size: 0,
        oid: gitlink,
        flags: 3,
        flags_extended: None,
        path: b"sub".to_vec(),
        base_index_pos: 0,
    });
    index.sort();
    let base_tree = write_tree_update_index(repo.odb(), &mut index, "", WriteTreeFlags::silent())
        .expect("tree");
    fs::remove_dir_all(wt.join("sub")).ok();
    fs::remove_file(wt.join("file")).ok();
    fs::create_dir_all(wt.join("file")).expect("dir file");
    fs::write(wt.join("file/inner"), b"x\n").expect("inner");
    fs::write(wt.join("link"), b"target\n").expect("link");
    let link_oid = repo
        .odb()
        .write(ObjectKind::Blob, b"target\n")
        .expect("blob");
    index = repo.index_handle().load().expect("reload");
    index.remove(b"sub");
    index.remove(b"file");
    index.add_or_replace(
        entry_from_stat(
            &wt.join("file/inner"),
            b"file/inner",
            repo.odb().write(ObjectKind::Blob, b"x\n").unwrap(),
            MODE_REGULAR,
        )
        .unwrap(),
    );
    index.add_or_replace(entry_from_stat(&wt.join("link"), b"link", link_oid, 0o120000).unwrap());
    index.sort();
    let to_tree = write_tree_update_index(repo.odb(), &mut index, "", WriteTreeFlags::silent())
        .expect("tree2");
    checkout_between_trees(&repo, Some(&base_tree), &to_tree).expect("seed serial");
    let snap_serial = snapshot_worktree(wt);
    checkout_between_trees(
        &open_with_policy(root, 4, Some(0)),
        Some(&base_tree),
        &to_tree,
    )
    .expect("parallel checkout");
    let snap_par = snapshot_worktree(wt);
    assert_trees_match(&snap_serial, &snap_par);
}

#[test]
fn external_smudge_filter_runs_once_via_recording_runner() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();
    let runner = RecordingRunner::always_success();
    runner.push_response(RecordedResponse::Output {
        stdout: b"SMUDGED\n".to_vec(),
        code: 0,
    });
    let git_dir = root.join(".git");
    fs::create_dir_all(git_dir.join("objects")).expect("objects");
    fs::create_dir_all(git_dir.join("refs/heads")).expect("refs");
    fs::write(git_dir.join("HEAD"), "ref: refs/heads/main\n").expect("head");
    fs::write(
        git_dir.join("config"),
        "[core]\n\trepositoryformatversion = 0\n\tbare = false\n[filter \"mark\"]\n\tsmudge = grit-test-smudge\n",
    )
    .expect("config");
    fs::write(root.join(".gitattributes"), "filtered filter=mark\n").expect("attrs");
    let mut options = RepositoryOptions::empty();
    options.command_runner = runner.clone();
    options.thread_policy = policy(4, Some(0));
    let repo = Repository::open(&git_dir, &options.clone().with_work_tree(root)).expect("open");
    let wt = repo.work_tree().expect("wt");
    fs::write(wt.join("filtered"), b"blob\n").expect("write");
    let blob = repo.odb().write(ObjectKind::Blob, b"blob\n").expect("blob");
    let mut index = repo.index_handle().load().expect("index");
    index.add_or_replace(
        entry_from_stat(&wt.join("filtered"), b"filtered", blob, MODE_REGULAR).expect("e"),
    );
    index.sort();
    let _tree = write_tree_update_index(repo.odb(), &mut index, "", WriteTreeFlags::silent())
        .expect("tree");
    let tree1 = commit_tree(&repo, "c1");
    fs::write(wt.join("filtered"), b"blob2\n").expect("new blob for checkout");
    let tree2 = commit_tree(&repo, "c2");
    checkout_between_trees(&repo, None, &tree1).expect("sync to tree1");
    runner.push_response(RecordedResponse::Output {
        stdout: b"SMUDGED\n".to_vec(),
        code: 0,
    });
    let smudge_calls_before = runner.specs().len();
    checkout_between_trees(&repo, Some(&tree1), &tree2).expect("checkout with smudge");
    let body = fs::read(wt.join("filtered")).expect("read filtered");
    assert_eq!(body, b"SMUDGED\n", "smudge output must land on disk");
    assert_eq!(
        runner.specs().len(),
        smudge_calls_before + 1,
        "single-shot smudge must run exactly once for this checkout"
    );
    let spec = runner.specs().into_iter().last().expect("spec");
    let script = match &spec.shell {
        Some(grit_lib::command_runner::ShellInvocation::DashC { script, .. }) => script.as_str(),
        _ => "",
    };
    assert!(
        script.contains("grit-test-smudge"),
        "expected smudge command in shell script, got {script:?}"
    );
    match &spec.stdin {
        CommandStdin::Pipe(bytes) => {
            assert_eq!(bytes, b"blob2\n", "smudge must receive blob bytes");
        }
        other => panic!("expected piped smudge stdin, got {other:?}"),
    }
}

#[test]
fn ignorecase_collision_uses_serial_path_and_matches() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();
    let repo = Repository::init(
        root,
        &InitOptions::new()
            .with_initial_branch("main".to_owned())
            .with_ref_storage(grit_lib::RefStorageFormat::Files),
    )
    .expect("init");
    fs::write(
        root.join(".git/config"),
        "[core]\n\trepositoryformatversion = 0\n\tbare = false\n\tignorecase = true\n",
    )
    .expect("preconfig");
    repo.reload_config().expect("reload");
    let wt = repo.work_tree().expect("wt");
    let empty = repo.odb().write(ObjectKind::Blob, b"").expect("empty");
    let mut index = repo.index_handle().load().expect("index");
    for name in ["FILE_x", "file_x", "other.txt"] {
        let path = wt.join(name);
        fs::write(&path, b"").expect("write");
        index.add_or_replace(
            entry_from_stat(&path, name.as_bytes(), empty, MODE_REGULAR).expect("e"),
        );
    }
    index.sort();
    let from = write_tree_update_index(repo.odb(), &mut index, "", WriteTreeFlags::silent())
        .expect("tree");
    fs::write(wt.join("other.txt"), b"changed\n").expect("chg");
    let to = commit_tree(&repo, "tip");
    assert_serial_parallel_match(root, from, to, true, false);
}

#[test]
fn parallel_pool_error_does_not_write_jobs_after_failure() {
    use grit_lib::diff::{DiffEntry, DiffStatus};
    const N: usize = 128;
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();
    let repo = Repository::init(
        root,
        &InitOptions::new()
            .with_initial_branch("main".to_owned())
            .with_ref_storage(grit_lib::RefStorageFormat::Files),
    )
    .expect("init");
    enable_parallel_checkout_config(root);
    let wt = repo.work_tree().expect("wt");
    let good_oid = repo.odb().write(ObjectKind::Blob, b"ok\n").expect("blob");
    let missing: ObjectId = "1111111111111111111111111111111111111111"
        .parse()
        .expect("missing");

    let mut changes = Vec::with_capacity(N);
    changes.push(DiffEntry {
        status: DiffStatus::Added,
        old_path: None,
        new_path: Some("f0.txt".into()),
        old_mode: "000000".into(),
        new_mode: "100644".into(),
        old_oid: ObjectId::zero(),
        new_oid: good_oid,
        score: None,
    });
    changes.push(DiffEntry {
        status: DiffStatus::Added,
        old_path: None,
        new_path: Some("f1.txt".into()),
        old_mode: "000000".into(),
        new_mode: "100644".into(),
        old_oid: ObjectId::zero(),
        new_oid: missing,
        score: None,
    });
    for i in 2..N {
        changes.push(DiffEntry {
            status: DiffStatus::Added,
            old_path: None,
            new_path: Some(format!("f{i}.txt")),
            old_mode: "000000".into(),
            new_mode: "100644".into(),
            old_oid: ObjectId::zero(),
            new_oid: good_oid,
            score: None,
        });
    }

    let err =
        checkout_tree_changes(&open_with_policy(root, 1, None), &changes).expect_err("serial fail");
    assert!(wt.join("f0.txt").is_file(), "serial must write f0: {err}");
    assert!(!wt.join("f1.txt").exists());
    for i in 2..N {
        assert!(
            !wt.join(format!("f{i}.txt")).exists(),
            "serial must not write f{i} after missing blob"
        );
    }

    for i in 0..N {
        let _ = fs::remove_file(wt.join(format!("f{i}.txt")));
    }

    let err_parallel = checkout_tree_changes(&open_with_forced_parallel_pool(root, 4), &changes)
        .expect_err("parallel fail");
    assert!(
        wt.join("f0.txt").is_file(),
        "parallel must write f0 before error: {err_parallel}"
    );
    assert!(!wt.join("f1.txt").exists());
    for i in 2..N {
        assert!(
            !wt.join(format!("f{i}.txt")).exists(),
            "parallel must not write f{i} after error (got side effects from later chunk)"
        );
    }
}

#[test]
fn missing_blob_errors_after_earlier_write() {
    use grit_lib::diff::{DiffEntry, DiffStatus};
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();
    let repo = Repository::init(
        root,
        &InitOptions::new()
            .with_initial_branch("main".to_owned())
            .with_ref_storage(grit_lib::RefStorageFormat::Files),
    )
    .expect("init");
    let wt = repo.work_tree().expect("wt");
    let good_oid = repo.odb().write(ObjectKind::Blob, b"ok\n").expect("blob");
    let missing: ObjectId = "1111111111111111111111111111111111111111"
        .parse()
        .expect("missing");
    let changes = vec![
        DiffEntry {
            status: DiffStatus::Added,
            old_path: None,
            new_path: Some("first.txt".into()),
            old_mode: "000000".into(),
            new_mode: "100644".into(),
            old_oid: ObjectId::zero(),
            new_oid: good_oid,
            score: None,
        },
        DiffEntry {
            status: DiffStatus::Added,
            old_path: None,
            new_path: Some("second.txt".into()),
            old_mode: "000000".into(),
            new_mode: "100644".into(),
            old_oid: ObjectId::zero(),
            new_oid: missing,
            score: None,
        },
    ];
    let err = checkout_tree_changes(&open_with_policy(root, 1, None), &changes)
        .expect_err("missing object must fail");
    assert!(
        wt.join("first.txt").is_file(),
        "first path must be written before error: {err}"
    );

    fs::remove_file(wt.join("first.txt")).ok();
    let err_parallel = checkout_tree_changes(&open_with_policy(root, 4, Some(0)), &changes)
        .expect_err("parallel path must fail the same way");
    assert!(
        wt.join("first.txt").is_file(),
        "parallel path must write first file before error: {err_parallel}"
    );
}

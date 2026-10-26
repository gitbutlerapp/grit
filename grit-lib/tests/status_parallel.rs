//! Serial vs parallel status must match (`threads=1` vs `threads=4`, forced batch thresholds).

use std::fs;
use std::path::Path;
use std::process::Command;

use grit_lib::config::environment::RepositoryOptions;
use grit_lib::porcelain::status::{status, IgnoredMode, StatusModel, StatusOptions, UntrackedMode};
use grit_lib::progress::NeverCancel;
use grit_lib::progress::NullProgress;
use grit_lib::repo::{InitOptions, Repository};
use grit_lib::RefStorageFormat;
use grit_test_support::forced_parallel_policy;
use grit_test_support::git;

fn normalize_model(model: &StatusModel) -> (Vec<String>, Vec<String>, Vec<String>, Vec<String>) {
    let mut staged: Vec<_> = model.staged.iter().map(|e| e.path().to_owned()).collect();
    let mut unstaged: Vec<_> = model.unstaged.iter().map(|e| e.path().to_owned()).collect();
    let mut untracked = model.untracked.clone();
    let mut ignored = model.ignored.clone();
    staged.sort();
    unstaged.sort();
    untracked.sort();
    ignored.sort();
    (staged, unstaged, untracked, ignored)
}

fn assert_models_equal(a: &StatusModel, b: &StatusModel) {
    assert_eq!(
        normalize_model(a),
        normalize_model(b),
        "status paths must match between thread counts"
    );
    assert_eq!(a.conflicts, b.conflicts, "conflicts must match");
}

fn baseline_index_bytes(repo: &Repository) -> Vec<u8> {
    match fs::read(repo.index_handle().path()) {
        Ok(bytes) => bytes,
        Err(_) => {
            let mut empty = grit_lib::index::Index::new();
            let index_path = repo.index_handle().path();
            empty.write(&index_path).expect("write empty index");
            fs::read(index_path).expect("read empty index")
        }
    }
}

fn run_status_pair(
    repo: &Repository,
    opts: &StatusOptions,
    baseline_index: &[u8],
) -> (StatusModel, StatusModel, Vec<u8>, Vec<u8>) {
    fs::write(repo.index_handle().path(), baseline_index).expect("restore index");
    let mut serial_opts = opts.clone();
    serial_opts.stat_parallel_threads = Some(1);
    let serial =
        status(repo, &serial_opts, &mut NullProgress, &NeverCancel).expect("serial status");
    let serial_index = fs::read(repo.index_handle().path()).expect("serial index");

    fs::write(repo.index_handle().path(), baseline_index).expect("restore index");
    let parallel_repo = Repository::open(
        repo.git_dir(),
        &RepositoryOptions::empty()
            .with_work_tree(repo.work_tree().expect("non-bare").to_path_buf()),
    )
    .expect("reopen repo")
    .with_thread_policy(forced_parallel_policy());
    let mut parallel_opts = opts.clone();
    parallel_opts.stat_parallel_threads = Some(4);
    let parallel = status(
        &parallel_repo,
        &parallel_opts,
        &mut NullProgress,
        &NeverCancel,
    )
    .expect("parallel status");
    let parallel_index = fs::read(parallel_repo.index_handle().path()).expect("parallel index");

    (serial, parallel, serial_index, parallel_index)
}

fn assert_git_diff_files_clean(repo_root: &Path) {
    let root = repo_root.to_str().expect("utf-8 path");
    let _ = Command::new("git")
        .args(["-C", root, "update-index", "--refresh"])
        .status()
        .expect("spawn git update-index --refresh");
    let st = Command::new("git")
        .args(["-C", root, "diff-files", "--quiet"])
        .status()
        .expect("spawn git diff-files");
    assert!(
        st.success(),
        "git diff-files --quiet must succeed after grit status refresh"
    );
}

#[test]
fn nested_ignores_and_negation_match_serial() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();
    let repo = Repository::init(
        root,
        &InitOptions::new()
            .with_initial_branch("main".to_owned())
            .with_ref_storage(RefStorageFormat::Files),
    )
    .expect("init");
    fs::create_dir_all(root.join("src/deep")).expect("mkdir");
    fs::write(root.join(".gitignore"), "!/keep.txt\n*.log\nbuild/\n").expect("ignore");
    fs::write(root.join("src/deep/a.log"), b"log\n").expect("write");
    fs::write(root.join("keep.txt"), b"keep\n").expect("write");
    fs::write(root.join("src/deep/visible.txt"), b"v\n").expect("write");
    fs::create_dir_all(root.join("build/out")).expect("mkdir");
    fs::write(root.join("build/out/x.txt"), b"x\n").expect("write");

    let opts = StatusOptions {
        untracked: UntrackedMode::Normal,
        ignored: IgnoredMode::Traditional,
        ahead_behind: false,
        ..StatusOptions::default()
    };
    let baseline = baseline_index_bytes(&repo);
    let (serial, parallel, _, _) = run_status_pair(&repo, &opts, &baseline);
    assert_models_equal(&serial, &parallel);
}

#[test]
fn untracked_directory_markers_match_serial() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();
    let repo = Repository::init(
        root,
        &InitOptions::new()
            .with_initial_branch("main".to_owned())
            .with_ref_storage(RefStorageFormat::Files),
    )
    .expect("init");
    fs::create_dir_all(root.join("new/pkg")).expect("mkdir");
    fs::write(root.join("new/pkg/file.txt"), b"f\n").expect("write");

    let opts = StatusOptions {
        untracked: UntrackedMode::Normal,
        ignored: IgnoredMode::No,
        ahead_behind: false,
        ..StatusOptions::default()
    };
    let baseline = baseline_index_bytes(&repo);
    let (serial, parallel, _, _) = run_status_pair(&repo, &opts, &baseline);
    assert_models_equal(&serial, &parallel);
    assert!(
        serial.untracked.iter().any(|p| p == "new/"),
        "expected directory marker, got {:?}",
        serial.untracked
    );
}

#[cfg(unix)]
#[test]
fn symlinked_directory_untracked_match_serial() {
    use std::os::unix::fs::symlink;

    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();
    let repo = Repository::init(
        root,
        &InitOptions::new()
            .with_initial_branch("main".to_owned())
            .with_ref_storage(RefStorageFormat::Files),
    )
    .expect("init");
    fs::create_dir_all(root.join("target/dir")).expect("mkdir");
    fs::write(root.join("target/dir/u.txt"), b"u\n").expect("write");
    symlink(root.join("target"), root.join("link")).expect("symlink");

    let opts = StatusOptions::default();
    let baseline = baseline_index_bytes(&repo);
    let (serial, parallel, _, _) = run_status_pair(&repo, &opts, &baseline);
    assert_models_equal(&serial, &parallel);
}

#[test]
fn racy_timestamp_refresh_matches_serial_and_git() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();
    git(root, &["init", "-q", "-b", "main", "."]);
    git(root, &["config", "user.email", "t@example.com"]);
    git(root, &["config", "user.name", "Test"]);
    fs::write(root.join("tracked.txt"), b"one\n").expect("write");
    git(root, &["add", "tracked.txt"]);
    git(root, &["commit", "-qm", "c1"]);

    let repo = Repository::open(
        &root.join(".git"),
        &RepositoryOptions::empty().with_work_tree(root),
    )
    .expect("open");
    filetime::set_file_mtime(root.join("tracked.txt"), filetime::FileTime::now()).expect("touch");

    let opts = StatusOptions {
        untracked: UntrackedMode::No,
        ahead_behind: false,
        ..StatusOptions::default()
    };
    let baseline = fs::read(repo.index_handle().path()).expect("index");
    let (serial, parallel, serial_index, parallel_index) = run_status_pair(&repo, &opts, &baseline);
    assert_models_equal(&serial, &parallel);
    assert_eq!(serial_index, parallel_index, "index bytes must match");
    assert_git_diff_files_clean(root);
}

#[test]
fn skip_worktree_sparse_entry_status_matches_serial() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();
    git(root, &["init", "-q", "-b", "main", "."]);
    git(root, &["config", "user.email", "t@example.com"]);
    git(root, &["config", "user.name", "Test"]);
    fs::write(root.join("present.txt"), b"p\n").expect("write");
    fs::write(root.join("skipped.txt"), b"s\n").expect("write");
    git(root, &["add", "."]);
    git(root, &["commit", "-qm", "seed"]);
    git(root, &["update-index", "--skip-worktree", "skipped.txt"]);

    let repo = Repository::open(
        &root.join(".git"),
        &RepositoryOptions::empty().with_work_tree(root),
    )
    .expect("open");
    fs::write(root.join("skipped.txt"), b"changed\n").expect("rewrite skipped");
    fs::write(root.join("present.txt"), b"changed\n").expect("rewrite present");

    let opts = StatusOptions {
        untracked: UntrackedMode::No,
        ahead_behind: false,
        ..StatusOptions::default()
    };
    let baseline = fs::read(repo.index_handle().path()).expect("index");
    let (serial, parallel, serial_index, parallel_index) = run_status_pair(&repo, &opts, &baseline);
    assert_models_equal(&serial, &parallel);
    assert_eq!(serial_index, parallel_index);
}

#[test]
fn gitlink_submodule_untracked_scan_matches_serial() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();
    git(root, &["init", "-q", "-b", "main", "."]);
    git(root, &["config", "user.email", "t@example.com"]);
    git(root, &["config", "user.name", "Test"]);

    let sub = root.join("sub");
    fs::create_dir_all(&sub).expect("mkdir");
    git(&sub, &["init", "-q", "-b", "main", "."]);
    git(&sub, &["config", "user.email", "t@example.com"]);
    git(&sub, &["config", "user.name", "Test"]);
    fs::write(sub.join("in-sub.txt"), b"x\n").expect("write");
    git(&sub, &["add", "."]);
    git(&sub, &["commit", "-qm", "sub"]);

    git(root, &["submodule", "add", sub.to_str().unwrap(), "sub"]);
    git(root, &["commit", "-qm", "add sub"]);
    fs::write(root.join("top.txt"), b"top\n").expect("write");

    let repo = Repository::open(
        &root.join(".git"),
        &RepositoryOptions::empty().with_work_tree(root),
    )
    .expect("open");
    let opts = StatusOptions {
        ahead_behind: false,
        ..StatusOptions::default()
    };
    let baseline = fs::read(repo.index_handle().path()).expect("index");
    let (serial, parallel, _, _) = run_status_pair(&repo, &opts, &baseline);
    assert_models_equal(&serial, &parallel);
}

#[test]
fn untracked_cache_enabled_matches_serial() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();
    git(root, &["init", "-q", "-b", "main", "."]);
    git(root, &["config", "user.email", "t@example.com"]);
    git(root, &["config", "user.name", "Test"]);
    git(root, &["config", "core.untrackedCache", "true"]);
    fs::write(root.join("tracked.txt"), b"t\n").expect("write");
    git(root, &["add", "."]);
    git(root, &["commit", "-qm", "c1"]);
    fs::write(root.join("fresh.txt"), b"f\n").expect("write");

    let repo = Repository::open(
        &root.join(".git"),
        &RepositoryOptions::empty().with_work_tree(root),
    )
    .expect("open");
    let opts = StatusOptions {
        ahead_behind: false,
        ..StatusOptions::default()
    };
    let baseline = fs::read(repo.index_handle().path()).expect("index");
    let (serial, parallel, _, _) = run_status_pair(&repo, &opts, &baseline);
    assert_models_equal(&serial, &parallel);

    // Warm UNTR: second pair must still match.
    let baseline2 = fs::read(repo.index_handle().path()).expect("index after first run");
    let (serial2, parallel2, _, _) = run_status_pair(&repo, &opts, &baseline2);
    assert_models_equal(&serial2, &parallel2);
}

#[test]
fn large_index_stat_refresh_parallel_matches_serial() {
    use grit_lib::PARALLEL_STAT_MIN_ENTRIES;

    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();
    let repo = Repository::init(
        root,
        &InitOptions::new()
            .with_initial_branch("main".to_owned())
            .with_ref_storage(RefStorageFormat::Files),
    )
    .expect("init");
    fs::create_dir_all(root.join("mix")).expect("mkdir");
    let count = PARALLEL_STAT_MIN_ENTRIES + 500;
    for i in 0..count {
        fs::write(
            root.join(format!("mix/{i:05}.txt")),
            format!("payload {i}\n"),
        )
        .expect("write");
    }
    grit_lib::porcelain::add::stage(
        &repo,
        &grit_lib::porcelain::add::StageOptions::default(),
        &mut NullProgress,
    )
    .expect("stage");
    let req = grit_lib::porcelain::commit::CommitRequest {
        message: "seed".into(),
        author: Some("T <t@e.com> 1 +0000".parse().expect("signature")),
        committer: Some("T <t@e.com> 1 +0000".parse().expect("signature")),
        allow_empty: false,
        sign_override: None,
        amend: false,
        assume_worktree_synced: false,
        skip_pre_commit_tree_match: false,
        staged_change_count: None,
        ..Default::default()
    };
    grit_lib::porcelain::commit::create_commit(&repo, &req, &mut NullProgress).expect("commit");
    for rel in ["mix/00030.txt", "mix/00040.txt", "mix/00050.txt"] {
        filetime::set_file_mtime(root.join(rel), filetime::FileTime::now()).expect("touch");
    }

    let opts = StatusOptions {
        untracked: UntrackedMode::No,
        ahead_behind: false,
        ..StatusOptions::default()
    };
    let baseline = fs::read(repo.index_handle().path()).expect("index");
    let (serial, parallel, serial_index, parallel_index) = run_status_pair(&repo, &opts, &baseline);
    assert_models_equal(&serial, &parallel);
    assert_eq!(serial_index, parallel_index);
}

/// Untracked (`??`) and ignored (`!!`) paths from system `git status --porcelain`.
fn git_untracked_and_ignored(root: &Path, untracked_files: &str) -> (Vec<String>, Vec<String>) {
    let out = git(
        root,
        &[
            "status",
            "--porcelain=v1",
            "--ignored",
            &format!("--untracked-files={untracked_files}"),
        ],
    );
    let mut untracked = Vec::new();
    let mut ignored = Vec::new();
    for line in out.lines() {
        if let Some(path) = line.strip_prefix("?? ") {
            untracked.push(path.to_owned());
        } else if let Some(path) = line.strip_prefix("!! ") {
            ignored.push(path.to_owned());
        }
    }
    untracked.sort();
    ignored.sort();
    (untracked, ignored)
}

/// Every tracked file sits under one top-level directory, so the parallel untracked walk has
/// to split below the top level; its result must match the serial walk and system `git`.
#[test]
fn single_top_level_directory_split_walk_matches_serial_and_git() {
    use grit_lib::PARALLEL_STAT_MIN_ENTRIES;

    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();
    git(root, &["init", "-q", "-b", "main", "."]);
    git(root, &["config", "user.email", "t@example.com"]);
    git(root, &["config", "user.name", "Test"]);

    let dirs = 72;
    let files_per_dir = PARALLEL_STAT_MIN_ENTRIES.div_ceil(dirs) + 2;
    for d in 0..dirs {
        let dir = root.join(format!("pkg/d{d:03}"));
        fs::create_dir_all(&dir).expect("mkdir");
        for f in 0..files_per_dir {
            fs::write(dir.join(format!("f{f:03}.txt")), format!("{d} {f}\n")).expect("write");
        }
    }
    fs::write(root.join("pkg/d020/.gitignore"), "*.log\n").expect("write ignore");
    fs::write(root.join("pkg/README"), "top of pkg\n").expect("write");
    git(root, &["add", "."]);
    git(root, &["commit", "-qm", "seed"]);

    let emb = root.join("pkg/d040/emb");
    fs::create_dir_all(&emb).expect("mkdir");
    git(&emb, &["init", "-q", "-b", "main", "."]);
    git(&emb, &["config", "user.email", "t@example.com"]);
    git(&emb, &["config", "user.name", "Test"]);
    fs::write(emb.join("e.txt"), b"e\n").expect("write");
    git(&emb, &["add", "."]);
    git(&emb, &["commit", "-qm", "emb"]);

    fs::write(root.join("top.txt"), b"top\n").expect("write");
    fs::write(root.join("pkg/d005/new.txt"), b"new\n").expect("write");
    fs::create_dir_all(root.join("pkg/d010/fresh/deeper")).expect("mkdir");
    fs::write(root.join("pkg/d010/fresh/deeper/x.txt"), b"x\n").expect("write");
    fs::create_dir_all(root.join("pkg/brand")).expect("mkdir");
    fs::write(root.join("pkg/brand/y.txt"), b"y\n").expect("write");
    fs::write(root.join("pkg/d020/a.log"), b"log\n").expect("write");
    fs::create_dir_all(root.join("pkg/d020/logs")).expect("mkdir");
    fs::write(root.join("pkg/d020/logs/z.log"), b"z\n").expect("write");
    fs::write(root.join("pkg/d030/f000.txt"), b"modified\n").expect("modify");
    fs::remove_file(root.join("pkg/d031/f001.txt")).expect("delete");

    let repo = Repository::open(
        &root.join(".git"),
        &RepositoryOptions::empty().with_work_tree(root),
    )
    .expect("open");
    let baseline = fs::read(repo.index_handle().path()).expect("index");

    for (untracked, git_mode) in [
        (UntrackedMode::Normal, "normal"),
        (UntrackedMode::All, "all"),
    ] {
        let opts = StatusOptions {
            untracked,
            ignored: IgnoredMode::Traditional,
            ahead_behind: false,
            ..StatusOptions::default()
        };
        let (serial, parallel, _, _) = run_status_pair(&repo, &opts, &baseline);
        assert_models_equal(&serial, &parallel);

        let (_, _, grit_untracked, grit_ignored) = normalize_model(&parallel);
        let (git_untracked, git_ignored) = git_untracked_and_ignored(root, git_mode);
        assert_eq!(
            grit_untracked, git_untracked,
            "untracked ({git_mode}) vs git"
        );
        assert_eq!(grit_ignored, git_ignored, "ignored ({git_mode}) vs git");
        assert_eq!(
            normalize_model(&parallel).1,
            vec![
                "pkg/d030/f000.txt".to_owned(),
                "pkg/d031/f001.txt".to_owned()
            ],
            "unstaged ({git_mode})"
        );
    }
}

//! Status persists stat-refreshed index entries after mtime-only worktree bumps (issue #924).
//!
//! After checkout or branch switch, Git often leaves tracked files with newer mtimes while
//! content still matches the index. `git status` re-verifies content once, writes the refreshed
//! stat data back to `.git/index`, and later status runs trust stat without re-hashing.

use std::fs;
use std::path::Path;
use std::process::Command;
use std::time::{Duration, Instant};

use grit_lib::config::environment::{Environment, RepositoryOptions};
use grit_lib::diff::stat_matches;
use grit_lib::index::Index;
use grit_lib::porcelain::status::{status, StatusOptions};
use grit_lib::progress::NeverCancel;
use grit_lib::progress::NullProgress;
use grit_lib::repo::Repository;
use grit_lib::PARALLEL_STAT_MIN_ENTRIES;
use grit_test_support::git;

fn sparse_directory_placeholder_count(index: &Index) -> usize {
    index
        .entries()
        .iter()
        .filter(|e| e.is_sparse_directory_placeholder())
        .count()
}

#[test]
fn status_trusts_git_refreshed_index_without_rehashing() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let repo_root = tmp.path();
    git(repo_root, &["init", "-q", "-b", "main", "."]);
    git(repo_root, &["config", "user.email", "t@example.com"]);
    git(repo_root, &["config", "user.name", "Test"]);

    const FILE_COUNT: usize = 256;
    for i in 0..FILE_COUNT {
        let name = format!("file-{i:03}.txt");
        fs::write(repo_root.join(&name), format!("content {i}\n")).expect("write");
    }
    git(repo_root, &["add", "."]);
    git(repo_root, &["commit", "-qm", "initial"]);
    git(repo_root, &["update-index", "--refresh"]);

    let grit_repo = Repository::open(
        &repo_root.join(".git"),
        &RepositoryOptions::empty().with_work_tree(repo_root),
    )
    .expect("open grit repo");
    let index_path = grit_repo.index_handle().path();
    let index_mtime_before = fs::metadata(&index_path)
        .expect("index metadata")
        .modified()
        .expect("index mtime");

    let model = status(
        &grit_repo,
        &StatusOptions::default(),
        &mut NullProgress,
        &NeverCancel,
    )
    .expect("status on git-refreshed index");
    assert!(
        model.unstaged.is_empty(),
        "git-refreshed index must be clean in grit status"
    );

    let index_mtime_after = fs::metadata(&index_path)
        .expect("index metadata")
        .modified()
        .expect("index mtime");
    assert_eq!(
        index_mtime_after, index_mtime_before,
        "grit status must not rewrite index when git stat cache already matches the worktree"
    );

    let second_start = Instant::now();
    let _second = status(
        &grit_repo,
        &StatusOptions::default(),
        &mut NullProgress,
        &NeverCancel,
    )
    .expect("second status");
    let second_elapsed = second_start.elapsed();
    assert!(
        second_elapsed < Duration::from_millis(150),
        "warm status on git-refreshed index should stay fast (got {:?})",
        second_elapsed
    );
}

const DEEP_TREE_TIMING_ISOLATED: &str = "GRIT_DEEP_TREE_TIMING_ISOLATED";

fn deep_tree_timing_lock_path() -> std::path::PathBuf {
    std::env::temp_dir().join("grit-deep-tree-status-timing.lock")
}

fn touch_all_tracked(repo: &Path) {
    let out = git(repo, &["ls-files", "-z"]);
    for path in out.split('\0').filter(|p| !p.is_empty()) {
        let abs = repo.join(path);
        let now = filetime::FileTime::now();
        filetime::set_file_mtime(&abs, now).expect("touch tracked file");
    }
}

#[test]
fn refreshed_index_matches_git_ls_files_debug() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let repo_root = tmp.path();
    git(repo_root, &["init", "-q", "-b", "main", "."]);
    git(repo_root, &["config", "user.email", "t@example.com"]);
    git(repo_root, &["config", "user.name", "Test"]);
    fs::write(repo_root.join("tracked.txt"), "payload\n").expect("write");
    git(repo_root, &["add", "tracked.txt"]);
    git(repo_root, &["commit", "-qm", "initial"]);

    std::thread::sleep(Duration::from_secs(1));
    filetime::set_file_mtime(repo_root.join("tracked.txt"), filetime::FileTime::now())
        .expect("touch");

    let grit_repo = Repository::open(
        &repo_root.join(".git"),
        &RepositoryOptions::empty().with_work_tree(repo_root),
    )
    .expect("open grit repo");
    let _model = status(
        &grit_repo,
        &StatusOptions::default(),
        &mut NullProgress,
        &NeverCancel,
    )
    .expect("status");

    let git_debug = Command::new("git")
        .args(["ls-files", "-s", "--debug"])
        .current_dir(repo_root)
        .output()
        .expect("git ls-files --debug");
    assert!(
        git_debug.status.success(),
        "git ls-files --debug must read grit-written index: {}",
        String::from_utf8_lossy(&git_debug.stderr)
    );
    let porcelain = git(repo_root, &["status", "--porcelain"]);
    assert!(
        porcelain.trim().is_empty(),
        "git status --porcelain must stay clean after grit refresh"
    );
}

#[test]
fn status_refreshes_index_stat_after_touch_and_second_run_is_faster() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let repo_root = tmp.path();
    git(repo_root, &["init", "-q", "-b", "main", "."]);
    git(repo_root, &["config", "user.email", "t@example.com"]);
    git(repo_root, &["config", "user.name", "Test"]);

    const FILE_COUNT: usize = 512;
    for i in 0..FILE_COUNT {
        let name = format!("file-{i:03}.txt");
        fs::write(repo_root.join(&name), format!("content {i}\n")).expect("write");
    }
    git(repo_root, &["add", "."]);
    git(repo_root, &["commit", "-qm", "initial"]);

    // Git's default stat match uses whole seconds (`core.usenanosec` off); bump into a new second
    // so mtime-only changes are visible.
    std::thread::sleep(Duration::from_secs(1));
    touch_all_tracked(repo_root);

    let grit_repo = Repository::open(
        &repo_root.join(".git"),
        &RepositoryOptions::empty().with_work_tree(repo_root),
    )
    .expect("open grit repo");
    let index_path = grit_repo.index_handle().path();
    let index_mtime_before = fs::metadata(&index_path)
        .expect("index metadata")
        .modified()
        .expect("index mtime");

    let opts = StatusOptions::default();
    let first_start = Instant::now();
    let model = status(&grit_repo, &opts, &mut NullProgress, &NeverCancel).expect("first status");
    let first_elapsed = first_start.elapsed();
    assert!(
        model.unstaged.is_empty(),
        "touched files with unchanged content must not appear unstaged"
    );

    let index_mtime_after = fs::metadata(&index_path)
        .expect("index metadata")
        .modified()
        .expect("index mtime");
    assert!(
        index_mtime_after > index_mtime_before,
        "status must write refreshed stat data to the index"
    );

    let reloaded = grit_repo.index_handle().load().expect("reload index");
    for entry in reloaded.entries() {
        if entry.stage() != 0 {
            continue;
        }
        let rel = std::str::from_utf8(&entry.path).expect("utf-8 path");
        let meta = fs::symlink_metadata(repo_root.join(rel)).expect("worktree metadata");
        assert!(
            stat_matches(entry, &meta),
            "index entry stat must match worktree after refresh for {rel}"
        );
    }

    let git_diff = git(repo_root, &["diff-files"]);
    assert!(
        git_diff.trim().is_empty(),
        "system git diff-files must be clean after grit refreshed the index"
    );

    let second_start = Instant::now();
    let _second =
        status(&grit_repo, &opts, &mut NullProgress, &NeverCancel).expect("second status");
    let second_elapsed = second_start.elapsed();

    assert!(
        second_elapsed <= first_elapsed,
        "second status should not do more work than the refresh pass (first {:?}, second {:?})",
        first_elapsed,
        second_elapsed
    );
    if first_elapsed > Duration::from_millis(25) {
        assert!(
            second_elapsed * 2 + Duration::from_millis(2) < first_elapsed,
            "second status should avoid full re-hash on a warm index (first {:?}, second {:?})",
            first_elapsed,
            second_elapsed
        );
    }
}

#[test]
fn status_succeeds_when_index_lock_held_after_touch() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let repo_root = tmp.path();
    git(repo_root, &["init", "-q", "-b", "main", "."]);
    git(repo_root, &["config", "user.email", "t@example.com"]);
    git(repo_root, &["config", "user.name", "Test"]);

    fs::write(repo_root.join("tracked.txt"), "same content\n").expect("write");
    git(repo_root, &["add", "tracked.txt"]);
    git(repo_root, &["commit", "-qm", "initial"]);

    filetime::set_file_mtime(repo_root.join("tracked.txt"), filetime::FileTime::now())
        .expect("touch");

    let git_dir = repo_root.join(".git");
    let lock_path = git_dir.join("index.lock");
    fs::OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(&lock_path)
        .expect("pre-create index.lock");

    let git_exit = Command::new("git")
        .arg("status")
        .current_dir(repo_root)
        .status()
        .expect("git status");
    assert!(
        git_exit.success(),
        "system git status must succeed with index.lock"
    );

    let grit_repo = Repository::open(
        &git_dir,
        &RepositoryOptions::empty().with_work_tree(repo_root),
    )
    .expect("open grit repo");
    let index_path = grit_repo.index_handle().path();
    let index_mtime_before = fs::metadata(&index_path)
        .expect("index metadata")
        .modified()
        .expect("index mtime");

    let model = status(
        &grit_repo,
        &StatusOptions::default(),
        &mut NullProgress,
        &NeverCancel,
    )
    .expect("grit status must succeed when index.lock blocks refresh write");

    assert!(
        model.unstaged.is_empty(),
        "unchanged content after touch must not appear unstaged"
    );

    let index_mtime_after = fs::metadata(&index_path)
        .expect("index metadata")
        .modified()
        .expect("index mtime");
    assert_eq!(
        index_mtime_after, index_mtime_before,
        "index must not be rewritten while index.lock is held"
    );

    let _ = fs::remove_file(&lock_path);
}

/// Mirrors a fresh `git clone` index where every entry mtime shares the index file second
/// (all racily clean) — grit must verify content once, not again in the parallel diff pass.
#[test]
fn status_on_racy_git_index_verifies_content_once() {
    if cfg!(coverage) || std::env::var_os("CARGO_LLVM_COV").is_some() {
        eprintln!("SKIP: timing oracle under llvm-cov instrumentation");
        return;
    }
    let tmp = tempfile::tempdir().expect("tempdir");
    let repo_root = tmp.path();
    git(repo_root, &["init", "-q", "-b", "main", "."]);
    git(repo_root, &["config", "user.email", "t@example.com"]);
    git(repo_root, &["config", "user.name", "Test"]);
    git(repo_root, &["config", "core.filemode", "false"]);

    const FILE_COUNT: usize = 1200;
    for i in 0..FILE_COUNT {
        let name = format!("file-{i:04}.txt");
        fs::write(repo_root.join(&name), format!("payload {i}\n")).expect("write");
    }
    git(repo_root, &["add", "."]);
    git(repo_root, &["commit", "-qm", "initial"]);

    let index_path = repo_root.join(".git/index");
    let index_mtime = fs::metadata(&index_path)
        .expect("index metadata")
        .modified()
        .expect("index mtime");
    let pinned = filetime::FileTime::from_system_time(index_mtime);
    for path in git(repo_root, &["ls-files", "-z"])
        .split('\0')
        .filter(|p| !p.is_empty())
    {
        filetime::set_file_mtime(repo_root.join(path), pinned).expect("pin worktree mtime");
    }
    filetime::set_file_mtime(&index_path, pinned).expect("pin index mtime");

    let grit_repo = Repository::open(
        &repo_root.join(".git"),
        &RepositoryOptions::empty().with_work_tree(repo_root),
    )
    .expect("open grit repo");
    let index_mtime_before = fs::metadata(&index_path)
        .expect("index metadata")
        .modified()
        .expect("index mtime");

    let first_start = Instant::now();
    let model = status(
        &grit_repo,
        &StatusOptions::default(),
        &mut NullProgress,
        &NeverCancel,
    )
    .expect("first status on racy git index");
    let first_elapsed = first_start.elapsed();

    assert!(
        model.unstaged.is_empty(),
        "racy clone-like index must be clean in grit status"
    );
    let git_diff = git(repo_root, &["diff-files"]);
    assert!(
        git_diff.trim().is_empty(),
        "system git diff-files must agree the tree is clean"
    );

    let second_start = Instant::now();
    let _second = status(
        &grit_repo,
        &StatusOptions::default(),
        &mut NullProgress,
        &NeverCancel,
    )
    .expect("second status");
    let second_elapsed = second_start.elapsed();

    assert!(
        first_elapsed < Duration::from_secs(2),
        "first status on racy index should not re-hash every path twice (got {first_elapsed:?})"
    );
    assert!(
        second_elapsed <= first_elapsed,
        "warm status should not do more work than the first pass (first {first_elapsed:?}, second {second_elapsed:?})"
    );

    let index_mtime_after = fs::metadata(&index_path)
        .expect("index metadata")
        .modified()
        .expect("index mtime");
    assert!(
        index_mtime_after >= index_mtime_before,
        "stat refresh may persist updated entries after the first pass"
    );
}

#[test]
fn status_on_git_created_sparse_index_after_touch() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let repo_root = tmp.path().join("gg");
    let clone = Command::new("git")
        .args(["clone", "--depth", "1", "https://github.com/git/git"])
        .arg(&repo_root)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .status()
        .expect("spawn git clone");
    assert!(
        clone.success(),
        "git clone git.git for sparse-index fixture"
    );

    git(&repo_root, &["config", "user.email", "t@example.com"]);
    git(&repo_root, &["config", "user.name", "Test"]);
    git(&repo_root, &["update-index", "--refresh"]);
    git(
        &repo_root,
        &["sparse-checkout", "init", "--cone", "--sparse-index"],
    );
    git(&repo_root, &["sparse-checkout", "set", "Documentation"]);

    for path in git(&repo_root, &["ls-files", "Documentation"])
        .lines()
        .filter(|l| !l.is_empty())
        .take(64)
    {
        filetime::set_file_mtime(repo_root.join(path), filetime::FileTime::now())
            .expect("touch materialized sparse file");
    }

    let git_dir = repo_root.join(".git");
    let grit_repo = Repository::open(
        &git_dir,
        &RepositoryOptions::empty().with_work_tree(&repo_root),
    )
    .expect("open grit repo");
    let index_before = Index::load(&grit_repo.index_handle().path()).expect("load sparse index");
    let placeholders_before = sparse_directory_placeholder_count(&index_before);
    assert!(
        placeholders_before > 0,
        "Git cone sparse-index must leave sparse-directory placeholders in the index"
    );

    let had_cache_tree_test = std::env::var("GIT_TEST_CHECK_CACHE_TREE").is_ok();
    std::env::set_var("GIT_TEST_CHECK_CACHE_TREE", "1");

    let model = status(
        &grit_repo,
        &StatusOptions::default(),
        &mut NullProgress,
        &NeverCancel,
    )
    .expect("status must succeed on Git sparse index after mtime-only touches");

    if !had_cache_tree_test {
        std::env::remove_var("GIT_TEST_CHECK_CACHE_TREE");
    }

    assert!(
        model.unstaged.is_empty(),
        "mtime-only touches on materialized sparse paths must not appear unstaged"
    );

    let index_after =
        Index::load(&grit_repo.index_handle().path()).expect("reload index after status");
    assert_eq!(
        sparse_directory_placeholder_count(&index_after),
        placeholders_before,
        "status stat refresh must not expand or drop sparse-directory placeholders"
    );

    let git_diff_files = git(&repo_root, &["diff-files"]);
    assert!(
        git_diff_files.trim().is_empty(),
        "system git diff-files must be clean after grit refreshed stats"
    );
}

fn status_deep_linear_tree_timing_oracle_once() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let repo_root = tmp.path();
    git(repo_root, &["init", "-q", "-b", "main", "."]);
    git(repo_root, &["config", "user.email", "t@example.com"]);
    git(repo_root, &["config", "user.name", "Test"]);
    git(repo_root, &["config", "core.autocrlf", "true"]);

    const CHAINS: usize = 150;
    const DEPTH: usize = 10;
    // Files from an earlier second, as after a checkout a moment ago. Written in the same
    // second as the index, every entry would stay racy through both status runs: the first
    // could not persist a non-racy index and the second would legitimately re-hash all files
    // and rewrite the index, which is what this oracle must not see.
    let past = filetime::FileTime::from_unix_time(filetime::FileTime::now().unix_seconds() - 10, 0);
    for chain in 0..CHAINS {
        let mut rel = format!("chain{chain:03}");
        for d in 0..DEPTH {
            rel.push_str(&format!("/dir{d}"));
        }
        rel.push_str("/file.txt");
        let abs = repo_root.join(&rel);
        if let Some(parent) = abs.parent() {
            fs::create_dir_all(parent).expect("mkdirs");
        }
        fs::write(&abs, format!("line {chain}\n")).expect("write");
        filetime::set_file_mtime(&abs, past).expect("pin mtime");
    }
    git(repo_root, &["add", "-A"]);
    git(repo_root, &["commit", "-qm", "big"]);
    let grit_repo = Repository::open(
        &repo_root.join(".git"),
        &RepositoryOptions::empty().with_work_tree(repo_root),
    )
    .expect("open grit repo");
    let opts = StatusOptions::default();
    let index_mtime_before = fs::metadata(grit_repo.index_handle().path())
        .expect("index metadata")
        .modified()
        .expect("index mtime");

    let first_start = Instant::now();
    let first = status(&grit_repo, &opts, &mut NullProgress, &NeverCancel).expect("first status");
    let first_elapsed = first_start.elapsed();
    assert!(first.unstaged.is_empty());
    assert!(first.untracked.is_empty());

    let index_mtime_after_first = fs::metadata(grit_repo.index_handle().path())
        .expect("index metadata")
        .modified()
        .expect("index mtime");

    let second_start = Instant::now();
    let second = status(&grit_repo, &opts, &mut NullProgress, &NeverCancel).expect("second status");
    let second_elapsed = second_start.elapsed();
    assert!(second.unstaged.is_empty());

    let index_mtime_after_second = fs::metadata(grit_repo.index_handle().path())
        .expect("index metadata")
        .modified()
        .expect("index mtime");
    assert_eq!(
        index_mtime_after_second, index_mtime_after_first,
        "clean second status must not rewrite the index"
    );

    assert!(
        second_elapsed <= first_elapsed + Duration::from_millis(5),
        "second status should not regress (first {first_elapsed:?}, second {second_elapsed:?})"
    );
    // The warm pass can only be markedly faster when the first one had stat refresh
    // work to do, which it records by rewriting the index.
    let first_refreshed = index_mtime_after_first != index_mtime_before;
    let instrumented = std::env::var_os("CARGO_LLVM_COV").is_some();
    if first_refreshed && !instrumented && first_elapsed > Duration::from_millis(40) {
        assert!(
            second_elapsed * 2 + Duration::from_millis(5) < first_elapsed,
            "warm deep-tree status should be faster than the first pass (first {first_elapsed:?}, second {second_elapsed:?})"
        );
    }
}

fn status_deep_linear_tree_timing_oracle() {
    const MAX_ATTEMPTS: usize = 4;
    for attempt in 0..MAX_ATTEMPTS {
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(
            status_deep_linear_tree_timing_oracle_once,
        ));
        match result {
            Ok(()) => return,
            Err(payload) => {
                let msg = payload
                    .downcast_ref::<&str>()
                    .map(|s| (*s).to_owned())
                    .or_else(|| {
                        payload
                            .downcast_ref::<String>()
                            .map(std::string::String::clone)
                    })
                    .unwrap_or_else(|| "timing oracle failed".to_owned());
                let timing_only = msg.contains("warm deep-tree status should be faster");
                if !timing_only || attempt + 1 == MAX_ATTEMPTS {
                    std::panic::resume_unwind(payload);
                }
                if attempt + 1 < MAX_ATTEMPTS {
                    std::thread::sleep(Duration::from_millis(100));
                }
                eprintln!(
                    "deep-tree timing oracle attempt {} failed ({msg}), retrying",
                    attempt + 1
                );
            }
        }
    }
}

#[test]
fn status_deep_linear_tree_second_run_stays_fast_with_autocrlf() {
    if cfg!(coverage) {
        eprintln!("SKIP: timing oracle under llvm-cov instrumentation");
        return;
    }
    if std::env::var_os(DEEP_TREE_TIMING_ISOLATED).is_some() {
        status_deep_linear_tree_timing_oracle();
        return;
    }
    let exe = std::env::current_exe().expect("integration test executable");
    let lock_path = deep_tree_timing_lock_path();
    let mut flock = Command::new("flock");
    flock
        .arg("-x")
        .arg(&lock_path)
        .arg(&exe)
        .env(DEEP_TREE_TIMING_ISOLATED, "1")
        .arg("status_deep_linear_tree_second_run_stays_fast_with_autocrlf")
        .arg("--exact")
        .arg("--test-threads=1")
        .arg("--nocapture");
    let output = flock.output().unwrap_or_else(|err| {
        panic!("spawn flock-isolated deep-tree timing oracle: {err}");
    });
    assert!(
        output.status.success(),
        "isolated deep-tree status timing oracle failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

fn open_grit_with_git_index_file(repo_root: &Path, git_index_file: &Path) -> Repository {
    let mut env = Environment::empty();
    env.cwd = repo_root.to_path_buf();
    env.git_index_file = Some(
        git_index_file
            .canonicalize()
            .unwrap_or_else(|_| git_index_file.to_path_buf())
            .to_string_lossy()
            .into_owned(),
    );
    Repository::open(
        &repo_root.join(".git"),
        &RepositoryOptions::with_environment(env).with_work_tree(repo_root),
    )
    .expect("open grit repo with GIT_INDEX_FILE")
}

#[test]
fn status_with_git_index_file_stat_refresh_does_not_rewrite_default_index() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let repo_root = tmp.path();
    git(repo_root, &["init", "-q", "-b", "main", "."]);
    git(repo_root, &["config", "user.email", "t@example.com"]);
    git(repo_root, &["config", "user.name", "Test"]);
    fs::write(repo_root.join("a"), "one\n").expect("write a");
    fs::write(repo_root.join("b"), "two\n").expect("write b");
    git(repo_root, &["add", "-A"]);
    git(repo_root, &["commit", "-qm", "initial"]);

    let git_dir = repo_root.join(".git");
    let default_index = git_dir.join("index");
    let alt_index = git_dir.join("index.alt");
    fs::copy(&default_index, &alt_index).expect("copy index");

    let remove_b = Command::new("git")
        .args(["update-index", "--force-remove", "b"])
        .env("GIT_INDEX_FILE", &alt_index)
        .current_dir(repo_root)
        .output()
        .expect("spawn git");
    assert!(
        remove_b.status.success(),
        "git update-index on alternate index: {}",
        String::from_utf8_lossy(&remove_b.stderr)
    );

    let default_before = fs::read(&default_index).expect("read default index");

    std::thread::sleep(Duration::from_secs(1));
    filetime::set_file_mtime(&repo_root.join("a"), filetime::FileTime::now()).expect("touch a");

    let grit_repo = open_grit_with_git_index_file(repo_root, &alt_index);
    let _model = status(
        &grit_repo,
        &StatusOptions::default(),
        &mut NullProgress,
        &NeverCancel,
    )
    .expect("status");

    let default_after = fs::read(&default_index).expect("read default index after status");
    assert_eq!(
        default_after, default_before,
        "stat refresh with GIT_INDEX_FILE must not rewrite the default .git/index"
    );

    let git_status = Command::new("git")
        .args(["status", "--porcelain"])
        .env("GIT_INDEX_FILE", &alt_index)
        .current_dir(repo_root)
        .output()
        .expect("git status");
    assert!(git_status.status.success());
    let default_after_git = fs::read(&default_index).expect("read default after git status");
    assert_eq!(
        default_after_git, default_before,
        "default index must stay unchanged under git status oracle too"
    );
}

#[test]
fn worktree_guard_with_git_index_file_stat_refresh_does_not_rewrite_default_index() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let repo_root = tmp.path();
    git(repo_root, &["init", "-q", "-b", "main", "."]);
    git(repo_root, &["config", "user.email", "t@example.com"]);
    git(repo_root, &["config", "user.name", "Test"]);
    fs::write(repo_root.join("a"), "one\n").expect("write a");
    fs::write(repo_root.join("b"), "two\n").expect("write b");
    git(repo_root, &["add", "-A"]);
    git(repo_root, &["commit", "-qm", "initial"]);

    let git_dir = repo_root.join(".git");
    let default_index = git_dir.join("index");
    let alt_index = git_dir.join("index.alt");
    fs::copy(&default_index, &alt_index).expect("copy index");

    let default_before = fs::read(&default_index).expect("read default index");

    std::thread::sleep(Duration::from_secs(1));
    filetime::set_file_mtime(&repo_root.join("a"), filetime::FileTime::now()).expect("touch a");

    let grit_repo = open_grit_with_git_index_file(repo_root, &alt_index);
    grit_lib::porcelain::worktree_guard::ensure_worktree_clean_for_merge(&grit_repo)
        .expect("merge guard on clean tree");

    let default_after = fs::read(&default_index).expect("read default index after guard");
    assert_eq!(
        default_after, default_before,
        "worktree guard stat refresh with GIT_INDEX_FILE must not rewrite default index"
    );
}

/// Racily clean parallel-sized indexes must still detect a same-size edit when stat seconds
/// match (Git `ie_match_stat` + `is_racy_timestamp` content check; issue #1702 review).
#[test]
fn status_on_racy_parallel_index_reports_same_size_stat_match_edit() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let repo_root = tmp.path();
    git(repo_root, &["init", "-q", "-b", "main", "."]);
    git(repo_root, &["config", "user.email", "t@example.com"]);
    git(repo_root, &["config", "user.name", "Test"]);
    git(repo_root, &["config", "core.filemode", "false"]);

    const LINE: &str = "0123456789abcdef0123456789abcdef\n";
    let file_count = PARALLEL_STAT_MIN_ENTRIES + 1;
    for i in 0..file_count {
        let name = format!("f{i:05}.txt");
        fs::write(repo_root.join(&name), LINE).expect("write");
    }
    git(repo_root, &["add", "-A"]);
    git(repo_root, &["commit", "-qm", "seed"]);

    let index_path = repo_root.join(".git/index");
    let index_mtime = fs::metadata(&index_path)
        .expect("index metadata")
        .modified()
        .expect("index mtime");
    let pinned = filetime::FileTime::from_system_time(index_mtime);
    for path in git(repo_root, &["ls-files", "-z"])
        .split('\0')
        .filter(|p| !p.is_empty())
    {
        filetime::set_file_mtime(repo_root.join(path), pinned).expect("pin worktree mtime");
    }
    filetime::set_file_mtime(&index_path, pinned).expect("pin index mtime");

    let dirty_rel = "f00000.txt";
    let same_len = "XXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXX\n";
    assert_eq!(same_len.len(), LINE.len());
    fs::write(repo_root.join(dirty_rel), same_len).expect("same-size edit");
    filetime::set_file_mtime(repo_root.join(dirty_rel), pinned).expect("re-pin dirty mtime");

    let git_porcelain = git(repo_root, &["status", "--porcelain"]);
    assert!(
        git_porcelain.lines().any(|l| l.contains(dirty_rel)),
        "system git must report the same-size racy edit: {git_porcelain:?}"
    );

    let grit_repo = Repository::open(
        &repo_root.join(".git"),
        &RepositoryOptions::empty().with_work_tree(repo_root),
    )
    .expect("open grit repo");
    let model = status(
        &grit_repo,
        &StatusOptions::default(),
        &mut NullProgress,
        &NeverCancel,
    )
    .expect("status on racy parallel index");
    assert!(
        model
            .unstaged
            .iter()
            .any(|e| e.old_path.as_deref() == Some(dirty_rel)),
        "grit status must report the same-size racy edit (unstaged {:?})",
        model.unstaged
    );
}

/// Racily clean git indexes (index mtime equals entry mtimes) with large blobs must stay clean
/// without re-reading every byte on repeated status runs (issue #1702).
#[test]
fn status_on_racy_git_index_with_large_blobs_persists_and_warms() {
    if cfg!(coverage) || std::env::var_os("CARGO_LLVM_COV").is_some() {
        eprintln!("SKIP: timing oracle under llvm-cov instrumentation");
        return;
    }
    let tmp = tempfile::tempdir().expect("tempdir");
    let repo_root = tmp.path();
    git(repo_root, &["init", "-q", "-b", "main", "."]);
    git(repo_root, &["config", "user.email", "t@example.com"]);
    git(repo_root, &["config", "user.name", "Test"]);

    const FILE_COUNT: usize = 48;
    const PAYLOAD_LEN: usize = 256 * 1024;
    let payload: Vec<u8> = (0..PAYLOAD_LEN).map(|i| (i % 251) as u8).collect();
    for i in 0..FILE_COUNT {
        let name = format!("f{i:02}.bin");
        fs::write(repo_root.join(&name), &payload).expect("write blob");
    }
    git(repo_root, &["add", "-A"]);
    git(repo_root, &["commit", "-qm", "big"]);

    let index_path = repo_root.join(".git/index");
    let index_mtime = fs::metadata(&index_path)
        .expect("index metadata")
        .modified()
        .expect("index mtime");
    // A past second: status only persists the refreshed index once the clock has left the
    // racy entries' second, and this test is about the run after that rewrite.
    let pinned = filetime::FileTime::from_unix_time(
        filetime::FileTime::from_system_time(index_mtime).unix_seconds() - 10,
        0,
    );
    for path in git(repo_root, &["ls-files", "-z"])
        .split('\0')
        .filter(|p| !p.is_empty())
    {
        filetime::set_file_mtime(repo_root.join(path), pinned).expect("pin worktree mtime");
    }
    filetime::set_file_mtime(&index_path, pinned).expect("pin index mtime");

    let grit_repo = Repository::open(
        &repo_root.join(".git"),
        &RepositoryOptions::empty().with_work_tree(repo_root),
    )
    .expect("open grit repo");

    let first_start = Instant::now();
    let model = status(
        &grit_repo,
        &StatusOptions::default(),
        &mut NullProgress,
        &NeverCancel,
    )
    .expect("first status on racy large index");
    let first_elapsed = first_start.elapsed();
    assert!(
        model.unstaged.is_empty(),
        "racy git index with large blobs must be clean in grit status"
    );
    let persisted = filetime::FileTime::from_last_modification_time(
        &fs::metadata(&index_path).expect("index metadata"),
    );
    assert!(
        persisted.unix_seconds() > pinned.unix_seconds(),
        "first status must persist the verified index so its entries stop being racy"
    );

    let second_start = Instant::now();
    let _second = status(
        &grit_repo,
        &StatusOptions::default(),
        &mut NullProgress,
        &NeverCancel,
    )
    .expect("second status");
    let second_elapsed = second_start.elapsed();

    assert!(
        second_elapsed <= first_elapsed,
        "warm status must not re-verify every large blob (first {first_elapsed:?}, second {second_elapsed:?})"
    );
    if first_elapsed > Duration::from_millis(80) {
        assert!(
            second_elapsed * 3 + Duration::from_millis(5) < first_elapsed,
            "second status should avoid full content re-read on warm index (first {first_elapsed:?}, second {second_elapsed:?})"
        );
    }
}

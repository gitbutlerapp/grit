//! Index↔worktree diff on a clean tree matches system `git diff-files`.

use std::collections::BTreeSet;
use std::process::Command;

use filetime::FileTime;
use grit_lib::config::environment::RepositoryOptions;
use grit_lib::diff::{diff_index_to_worktree_with_options, DiffIndexToWorktreeOptions};
use grit_lib::repo::Repository;
use grit_test_support::git;

const PINNED_MTIME: (u32, u32) = (1_577_836_800, 0);

fn pin_mtime(path: &std::path::Path, sec: u32, nsec: u32) {
    filetime::set_file_mtime(path, FileTime::from_unix_time(i64::from(sec), nsec))
        .expect("set mtime");
}

fn git_diff_names(root: &std::path::Path) -> BTreeSet<String> {
    git(root, &["diff", "--name-only"])
        .lines()
        .filter(|line| !line.is_empty())
        .map(str::to_owned)
        .collect()
}

fn grit_diff_names_cli(root: &std::path::Path, grit_bin: &std::path::Path) -> BTreeSet<String> {
    let out = Command::new(grit_bin)
        .args(["diff", "--json"])
        .current_dir(root)
        .output()
        .expect("grit diff --json");
    assert!(
        out.status.success(),
        "grit diff --json failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).expect("json");
    v.get("files")
        .and_then(|f| f.as_array())
        .into_iter()
        .flatten()
        .filter_map(|entry| entry.get("path").and_then(|p| p.as_str()))
        .map(str::to_owned)
        .collect()
}

fn library_unstaged_paths(root: &std::path::Path) -> BTreeSet<String> {
    let repo = Repository::open(
        &root.join(".git"),
        &RepositoryOptions::empty().with_work_tree(root),
    )
    .expect("open");
    let mut index = repo.index_handle().load().expect("index");
    let index_mtime = grit_lib::diff::index_mtime_for_diff(&index, None);
    let config = repo.config().expect("config");
    let (entries, _) = diff_index_to_worktree_with_options(
        repo.odb(),
        &mut index,
        root,
        DiffIndexToWorktreeOptions {
            repository_git_dir: Some(repo.git_dir().to_path_buf()),
            index_mtime,
            refresh_index_stat_in_pass: false,
            for_status: true,
            config: Some(config),
            thread_policy: Some(repo.thread_policy()),
            ..DiffIndexToWorktreeOptions::default()
        },
    )
    .expect("diff");
    entries
        .into_iter()
        .filter_map(|e| e.old_path.or(e.new_path))
        .collect()
}

fn assert_git_diff_files_clean(repo_root: &std::path::Path) {
    let root = repo_root.to_str().expect("utf-8 path");
    let status = Command::new("git")
        .args(["-C", root, "diff-files", "--quiet"])
        .status()
        .expect("spawn git diff-files");
    assert!(
        status.success(),
        "git diff-files --quiet must succeed on clean tree"
    );
}

#[test]
fn grit_diff_index_worktree_empty_on_deep_clean_tree() {
    const N: usize = 2_000;
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();
    git(root, &["init", "-q", "-b", "main", "."]);
    git(root, &["config", "user.email", "t@example.com"]);
    git(root, &["config", "user.name", "Test"]);
    for i in 0..N {
        let dir = format!("d{}/sub{}", i % 40, i % 17);
        std::fs::create_dir_all(root.join(&dir)).expect("mkdir");
        std::fs::write(
            root.join(dir).join(format!("f{i:05}.txt")),
            format!("body {i}\n"),
        )
        .expect("write");
    }
    git(root, &["add", "."]);
    git(root, &["commit", "-qm", "base"]);

    let repo = Repository::open(
        &root.join(".git"),
        &RepositoryOptions::empty().with_work_tree(root),
    )
    .expect("open");
    let mut index = repo.index_handle().load().expect("index");
    let index_mtime = grit_lib::diff::index_mtime_for_diff(&index, None);
    let config = repo.config().expect("config");
    let (entries, _) = diff_index_to_worktree_with_options(
        repo.odb(),
        &mut index,
        root,
        DiffIndexToWorktreeOptions {
            repository_git_dir: Some(repo.git_dir().to_path_buf()),
            index_mtime,
            refresh_index_stat_in_pass: false,
            for_status: true,
            config: Some(config),
            thread_policy: Some(repo.thread_policy()),
            ..DiffIndexToWorktreeOptions::default()
        },
    )
    .expect("diff");
    assert!(
        entries.is_empty(),
        "expected no unstaged diff, got {}",
        entries.len()
    );
    assert_git_diff_files_clean(root);
}

#[test]
fn grit_diff_matches_git_after_non_racy_post_index_edit() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();
    git(root, &["init", "-q", "-b", "main", "."]);
    git(root, &["config", "user.email", "t@example.com"]);
    git(root, &["config", "user.name", "Test"]);

    let tracked = root.join("tracked.txt");
    std::fs::write(&tracked, b"version one\n").expect("write");
    pin_mtime(&tracked, PINNED_MTIME.0, PINNED_MTIME.1);
    git(root, &["add", "tracked.txt"]);
    git(root, &["commit", "-qm", "init"]);

    std::fs::write(&tracked, b"version two\n").expect("rewrite");

    let expected = git_diff_names(root);
    assert_eq!(
        expected,
        BTreeSet::from([String::from("tracked.txt")]),
        "git must report the post-index edit"
    );
    assert_eq!(
        library_unstaged_paths(root),
        expected,
        "library diff must match git --name-only"
    );
    if let Ok(grit_bin) = std::env::var("CARGO_BIN_EXE_grit") {
        let grit = std::path::PathBuf::from(grit_bin);
        assert_eq!(
            grit_diff_names_cli(root, &grit),
            expected,
            "grit diff --json must match git --name-only"
        );
    }
}

#[test]
fn grit_diff_matches_git_after_non_racy_post_index_deletion() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();
    git(root, &["init", "-q", "-b", "main", "."]);
    git(root, &["config", "user.email", "t@example.com"]);
    git(root, &["config", "user.name", "Test"]);

    let tracked = root.join("tracked.txt");
    std::fs::write(&tracked, b"payload\n").expect("write");
    pin_mtime(&tracked, PINNED_MTIME.0, PINNED_MTIME.1);
    git(root, &["add", "tracked.txt"]);
    git(root, &["commit", "-qm", "init"]);

    std::fs::remove_file(&tracked).expect("delete");

    let expected = git_diff_names(root);
    assert_eq!(
        expected,
        BTreeSet::from([String::from("tracked.txt")]),
        "git must report the deletion"
    );
    assert_eq!(library_unstaged_paths(root), expected);
    if let Ok(grit_bin) = std::env::var("CARGO_BIN_EXE_grit") {
        let grit = std::path::PathBuf::from(grit_bin);
        assert_eq!(grit_diff_names_cli(root, &grit), expected);
    }
}

#[test]
#[ignore = "manual clean-tree diff latency vs system git"]
fn clean_tree_diff_latency_within_git_on_large_fixture() {
    use std::time::{Duration, Instant};

    const N: usize = 10_000;
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();
    git(root, &["init", "-q", "-b", "main", "."]);
    git(root, &["config", "user.email", "t@example.com"]);
    git(root, &["config", "user.name", "Test"]);
    for i in 0..N {
        let dir = format!("d{}/sub{}", i % 80, i % 31);
        std::fs::create_dir_all(root.join(&dir)).expect("mkdir");
        std::fs::write(
            root.join(dir).join(format!("f{i:05}.txt")),
            format!("body {i}\n"),
        )
        .expect("write");
    }
    git(root, &["add", "."]);
    git(root, &["commit", "-qm", "base"]);

    let grit_bin = std::env::var("GRIT_BIN").unwrap_or_else(|_| {
        std::env::var("CARGO_BIN_EXE_grit").expect("GRIT_BIN or CARGO_BIN_EXE_grit")
    });
    let root_s = root.to_str().expect("utf-8");

    let mut git_times = Vec::new();
    let mut grit_times = Vec::new();
    for _ in 0..5 {
        let t0 = Instant::now();
        let git_status = Command::new("git")
            .args(["-C", root_s, "diff"])
            .status()
            .expect("git diff");
        assert!(git_status.success());
        git_times.push(t0.elapsed());

        let t0 = Instant::now();
        let grit_status = Command::new(&grit_bin)
            .args(["diff"])
            .current_dir(root)
            .status()
            .expect("grit diff");
        assert!(grit_status.success());
        grit_times.push(t0.elapsed());
    }
    git_times.sort();
    grit_times.sort();
    let git_med = git_times[git_times.len() / 2];
    let grit_med = grit_times[grit_times.len() / 2];
    let ratio = grit_med.as_secs_f64() / git_med.as_secs_f64();
    eprintln!("git median {git_med:?}, grit median {grit_med:?}, ratio {ratio:.2}");
    assert!(
        ratio < 3.0,
        "grit diff should stay within 3× system git on a warm clean tree (got {ratio:.2}×)"
    );
    assert!(git_med < Duration::from_secs(30));
}

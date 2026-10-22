//! FSMN index extension compatibility with the system `git` binary and fsmonitor hook v2.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use grit_lib::config::environment::RepositoryOptions;
use grit_lib::index::{Index, IndexEntry, MODE_REGULAR};
use grit_lib::objects::{HashAlgo, ObjectKind};
use grit_lib::repo::Repository;
use grit_test_support::git;

fn isolated_git(dir: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .current_dir(dir)
        .args(args)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .env("GIT_AUTHOR_NAME", "Test")
        .env("GIT_AUTHOR_EMAIL", "t@example.com")
        .env("GIT_COMMITTER_NAME", "Test")
        .env("GIT_COMMITTER_EMAIL", "t@example.com")
        .output()
        .expect("spawn git");
    assert!(
        out.status.success(),
        "git {:?} failed: {}",
        args,
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).to_string()
}

fn write_fsmonitor_hook(dir: &Path) -> PathBuf {
    let hook = dir.join("fsmonitor-hook.sh");
    let script = r#"#!/bin/sh
if [ "$1" = "2" ]; then
  while read -r line; do
    [ -z "$line" ] && break
  done
  printf '%s\0' 'bench-token'
  exit 0
fi
exit 0
"#;
    fs::write(&hook, script).expect("write hook");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = fs::metadata(&hook).expect("meta").permissions();
        perms.set_mode(0o755);
        fs::set_permissions(&hook, perms).expect("chmod hook");
    }
    hook
}

fn fsck_clean(repo: &Path) {
    let out = Command::new("git")
        .current_dir(repo)
        .args(["fsck", "--strict"])
        .env("GIT_CONFIG_NOSYSTEM", "1")
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

fn ls_files_fsmonitor_flags(repo: &Path) -> Vec<(char, String)> {
    let out = isolated_git(repo, &["ls-files", "-f"]);
    out.lines()
        .filter_map(|line| {
            let (flag, path) = line.split_once(' ')?;
            flag.chars().next().map(|c| (c, path.to_string()))
        })
        .collect()
}

fn fsmn_token(repo: &Path) -> String {
    Index::load(&repo.join(".git/index"))
        .expect("load index")
        .fsmonitor_last_update
        .expect("FSMN token")
}

fn init_repo_with_fsmn(tmp: &Path) -> PathBuf {
    git(tmp, &["init", "-q", "-b", "main", "."]);
    git(tmp, &["config", "user.email", "t@example.com"]);
    git(tmp, &["config", "user.name", "Test"]);
    let hook = write_fsmonitor_hook(tmp);
    git(
        tmp,
        &[
            "config",
            "core.fsmonitor",
            hook.to_str().expect("hook utf8"),
        ],
    );
    for i in 0..40 {
        fs::write(tmp.join(format!("file-{i:02}.txt")), format!("v1 {i}\n")).expect("write");
    }
    git(tmp, &["add", "."]);
    git(tmp, &["commit", "-qm", "initial"]);
    isolated_git(tmp, &["status", "-s"]);
    let index = fs::read(tmp.join(".git/index")).expect("read index");
    assert!(
        index.windows(4).any(|w| w == b"FSMN"),
        "git must write FSMN after fsmonitor status"
    );
    tmp.to_path_buf()
}

fn make_replacement_entry(path: &[u8], content: &[u8]) -> IndexEntry {
    let oid = HashAlgo::Sha1
        .hash_object(ObjectKind::Blob, content)
        .expect("hash");
    IndexEntry {
        ctime_sec: 1,
        ctime_nsec: 0,
        mtime_sec: 1,
        mtime_nsec: 0,
        dev: 0,
        ino: 0,
        mode: MODE_REGULAR,
        uid: 0,
        gid: 0,
        size: content.len() as u32,
        oid,
        flags: path.len().min(0xFFF) as u16,
        flags_extended: None,
        path: path.to_vec(),
        base_index_pos: 0,
    }
}

#[test]
fn grit_fsmn_index_round_trips_with_git() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let repo_root = init_repo_with_fsmn(tmp.path());
    let token_before = fsmn_token(&repo_root);

    let grit_repo = Repository::open(
        &repo_root.join(".git"),
        &RepositoryOptions::empty().with_work_tree(&repo_root),
    )
    .expect("open grit repo");
    let mut index = grit_repo.index_handle().load().expect("load index");
    assert_eq!(
        index.fsmonitor_last_update.as_deref(),
        Some(token_before.as_str())
    );

    for i in 0..5 {
        let path = format!("new-{i}.txt");
        fs::write(repo_root.join(&path), format!("added {i}\n")).expect("write worktree");
        let data = fs::read(repo_root.join(&path)).expect("read");
        let oid = grit_repo
            .odb()
            .write(ObjectKind::Blob, &data)
            .expect("blob");
        let mut entry = make_replacement_entry(path.as_bytes(), &data);
        entry.oid = oid;
        entry.set_fsmonitor_valid(true);
        index.add_or_replace(entry);
    }
    index.sort();
    grit_repo
        .index_handle()
        .write(&mut index)
        .expect("write index");

    let tracked = isolated_git(&repo_root, &["ls-files"]).lines().count();
    let flags = ls_files_fsmonitor_flags(&repo_root);
    assert_eq!(flags.len(), tracked);
    assert!(
        flags.iter().all(|(c, _)| *c == 'h'),
        "all tracked paths must be fsmonitor-valid: {flags:?}"
    );

    let porcelain = isolated_git(&repo_root, &["status", "--porcelain"]);
    let mut porcelain_lines: Vec<_> = porcelain.lines().filter(|l| !l.is_empty()).collect();
    porcelain_lines.sort();
    let expected: Vec<String> = (0..5).map(|i| format!("A  new-{i}.txt")).collect();
    assert_eq!(porcelain_lines, expected, "unexpected porcelain output");
    fsck_clean(&repo_root);

    let token_after = fsmn_token(&repo_root);
    assert_eq!(token_after, token_before);
}

#[test]
fn git_fsmn_index_rewritten_by_grit_preserves_token() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let repo_root = init_repo_with_fsmn(tmp.path());
    let token_before = fsmn_token(&repo_root);

    let grit_repo = Repository::open(
        &repo_root.join(".git"),
        &RepositoryOptions::empty().with_work_tree(&repo_root),
    )
    .expect("open grit repo");
    let mut index = grit_repo.index_handle().load().expect("load");
    grit_repo
        .index_handle()
        .write(&mut index)
        .expect("rewrite index");

    let token_after = fsmn_token(&repo_root);
    assert_eq!(token_after, token_before);

    let flags_before = ls_files_fsmonitor_flags(&repo_root);
    fsck_clean(&repo_root);
    assert_eq!(
        isolated_git(&repo_root, &["status", "--porcelain"]).trim(),
        ""
    );
    assert!(!flags_before.is_empty());
}

#[test]
fn grit_status_skips_worktree_stat_when_fsmonitor_valid() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let repo_root = init_repo_with_fsmn(tmp.path());

    let hook = write_fsmonitor_hook(tmp.path());
    let no_change_script = r#"#!/bin/sh
if [ "$1" = "2" ]; then
  printf '%s\0' 'bench-token'
  exit 0
fi
exit 0
"#;
    std::fs::write(&hook, no_change_script).expect("write hook");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = std::fs::metadata(&hook).expect("meta").permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&hook, perms).expect("chmod");
    }

    let grit_repo = Repository::open(
        &repo_root.join(".git"),
        &RepositoryOptions::empty().with_work_tree(&repo_root),
    )
    .expect("open grit repo");

    isolated_git(&repo_root, &["status", "-s"]);

    let model = grit_lib::porcelain::status::status(
        &grit_repo,
        &grit_lib::porcelain::status::StatusOptions::default(),
        &mut grit_lib::progress::NullProgress,
    )
    .expect("status");
    assert!(
        model.staged.is_empty() && model.unstaged.is_empty(),
        "expected clean status, got staged={:?} unstaged={:?}",
        model.staged,
        model.unstaged
    );
}

fn write_fsmonitor_v1_hook(dir: &Path) -> PathBuf {
    let hook = dir.join("fsmonitor-v1-hook.sh");
    let script = r#"#!/bin/sh
if [ "$1" = "1" ]; then
  printf '%s\0' 'tracked.txt'
  exit 0
fi
exit 0
"#;
    fs::write(&hook, script).expect("write hook");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = fs::metadata(&hook).expect("meta").permissions();
        perms.set_mode(0o755);
        fs::set_permissions(&hook, perms).expect("chmod hook");
    }
    hook
}

#[test]
fn grit_status_honors_fsmonitor_hook_v1_changed_paths() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let repo_root = tmp.path();
    git(repo_root, &["init", "-q", "-b", "main", "."]);
    git(repo_root, &["config", "user.email", "t@example.com"]);
    git(repo_root, &["config", "user.name", "Test"]);
    let hook = write_fsmonitor_v1_hook(repo_root);
    git(
        repo_root,
        &[
            "config",
            "core.fsmonitor",
            hook.to_str().expect("hook utf8"),
        ],
    );
    git(repo_root, &["config", "core.fsmonitorhookversion", "1"]);
    fs::write(repo_root.join("tracked.txt"), "v1\n").expect("write");
    git(repo_root, &["add", "tracked.txt"]);
    git(repo_root, &["commit", "-qm", "initial"]);
    isolated_git(repo_root, &["status", "-s"]);

    fs::write(repo_root.join("tracked.txt"), "v1 modified\n").expect("modify");

    let grit_repo = Repository::open(
        &repo_root.join(".git"),
        &RepositoryOptions::empty().with_work_tree(repo_root),
    )
    .expect("open grit repo");
    let model = grit_lib::porcelain::status::status(
        &grit_repo,
        &grit_lib::porcelain::status::StatusOptions::default(),
        &mut grit_lib::progress::NullProgress,
    )
    .expect("status");

    assert!(
        model.unstaged.iter().any(|e| e.path() == "tracked.txt"),
        "grit must report unstaged change after v1 hook invalidates path: {:?}",
        model.unstaged
    );
    let git_porcelain = isolated_git(repo_root, &["status", "--porcelain"]);
    assert!(
        git_porcelain.contains("tracked.txt"),
        "git baseline should show modification: {git_porcelain}"
    );
}

fn write_fsmonitor_v1_race_hook(dir: &Path) -> PathBuf {
    let hook = dir.join("fsmonitor-v1-race-hook.sh");
    let script = r#"#!/bin/sh
if [ "$1" != "1" ]; then
  exit 0
fi
TOKEN="$2"
if [ -z "$TOKEN" ]; then
  exit 0
fi
# First hook query (previous token = first boundary): modify during the query, then
# report when mtime is at/after that token (real mtimes exceed injected test tokens).
if [ "$TOKEN" = "1000000" ]; then
  echo >> tracked.txt
  MTIME=$(python3 -c 'import os; print(os.stat("tracked.txt").st_mtime_ns)' 2>/dev/null || echo 0)
  if [ "$MTIME" -ge "$TOKEN" ] 2>/dev/null; then
    printf '%s\0' 'tracked.txt'
  fi
  exit 0
fi
MTIME=$(python3 -c 'import os; print(os.stat("tracked.txt").st_mtime_ns)' 2>/dev/null || echo 0)
if [ "$MTIME" -ge "$TOKEN" ] 2>/dev/null; then
  printf '%s\0' 'tracked.txt'
fi
exit 0
"#;
    fs::write(&hook, script).expect("write hook");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = fs::metadata(&hook).expect("meta").permissions();
        perms.set_mode(0o755);
        fs::set_permissions(&hook, perms).expect("chmod hook");
    }
    hook
}

#[test]
fn grit_status_v1_two_query_race_after_hook_modifies_during_first_query() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let repo_root = tmp.path();
    git(repo_root, &["init", "-q", "-b", "main", "."]);
    git(repo_root, &["config", "user.email", "t@example.com"]);
    git(repo_root, &["config", "user.name", "Test"]);
    let hook = write_fsmonitor_v1_race_hook(repo_root);
    git(
        repo_root,
        &[
            "config",
            "core.fsmonitor",
            hook.to_str().expect("hook utf8"),
        ],
    );
    git(repo_root, &["config", "core.fsmonitorhookversion", "1"]);
    fs::write(repo_root.join("tracked.txt"), "base\n").expect("write");
    git(repo_root, &["add", "tracked.txt"]);
    git(repo_root, &["commit", "-qm", "initial"]);
    // Git commit leaves an FSMN token; drop it so the first grit status seeds the boundary.
    let index_path = repo_root.join(".git/index");
    let mut disk_index = grit_lib::index::Index::load(&index_path).expect("load index");
    disk_index.clear_fsmonitor_extension();
    disk_index.write(&index_path).expect("write index");

    let grit_repo = Repository::open(
        &repo_root.join(".git"),
        &RepositoryOptions::empty().with_work_tree(repo_root),
    )
    .expect("open grit repo");
    let mut progress = grit_lib::progress::NullProgress;
    let boundary_first = grit_lib::worktree::fsmonitor::FsmonitorQueryBoundary(1_000_000);
    let boundary_second = grit_lib::worktree::fsmonitor::FsmonitorQueryBoundary(2_000_000);
    let mut opts = grit_lib::porcelain::status::StatusOptions::default();
    opts.fsmonitor_query_boundary = Some(boundary_first);

    let first = grit_lib::porcelain::status::status(&grit_repo, &opts, &mut progress)
        .expect("first status");
    assert!(
        first.unstaged.is_empty(),
        "first query seeds FSMN token without hook: {:?}",
        first.unstaged
    );
    assert_eq!(
        first.index.fsmonitor_last_update.as_deref(),
        Some("1000000"),
        "first query must persist pre-hook v1 token"
    );

    opts.fsmonitor_query_boundary = Some(boundary_second);
    let second = grit_lib::porcelain::status::status(&grit_repo, &opts, &mut progress)
        .expect("second status");
    assert!(
        second.unstaged.iter().any(|e| e.path() == "tracked.txt"),
        "second query must invalidate via v1 token window: {:?}",
        second.unstaged
    );

    let git_porcelain = isolated_git(repo_root, &["status", "--porcelain"]);
    assert!(
        git_porcelain.contains("tracked.txt"),
        "git baseline should show modification: {git_porcelain}"
    );
}

fn write_fsmonitor_v2_report_all_hook(dir: &Path) -> PathBuf {
    let hook = dir.join("fsmonitor-report-hook.sh");
    let script = r#"#!/bin/sh
if [ "$1" = "2" ]; then
  printf 'hook-token\0tracked.txt\0'
  exit 0
fi
exit 0
"#;
    fs::write(&hook, script).expect("write hook");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = fs::metadata(&hook).expect("meta").permissions();
        perms.set_mode(0o755);
        fs::set_permissions(&hook, perms).expect("chmod hook");
    }
    hook
}

#[test]
fn grit_status_does_not_skip_stat_on_fsmn_bootstrap_with_stale_valid_bits() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let repo_root = tmp.path();
    git(repo_root, &["init", "-q", "-b", "main", "."]);
    git(repo_root, &["config", "user.email", "t@example.com"]);
    git(repo_root, &["config", "user.name", "Test"]);
    let hook = write_fsmonitor_v2_report_all_hook(repo_root);
    git(
        repo_root,
        &[
            "config",
            "core.fsmonitor",
            hook.to_str().expect("hook utf8"),
        ],
    );
    fs::write(repo_root.join("tracked.txt"), "base\n").expect("write");
    git(repo_root, &["add", "tracked.txt"]);
    git(repo_root, &["commit", "-qm", "initial"]);

    let index_path = repo_root.join(".git/index");
    let mut disk_index = Index::load(&index_path).expect("load index");
    disk_index.clear_fsmonitor_extension();
    for entry in disk_index.entries_mut() {
        if entry.stage() == 0 {
            entry.set_fsmonitor_valid(true);
        }
    }
    disk_index.write(&index_path).expect("write index");

    fs::write(repo_root.join("tracked.txt"), "modified\n").expect("modify");

    let grit_repo = Repository::open(
        &repo_root.join(".git"),
        &RepositoryOptions::empty().with_work_tree(repo_root),
    )
    .expect("open grit repo");
    let model = grit_lib::porcelain::status::status(
        &grit_repo,
        &grit_lib::porcelain::status::StatusOptions::default(),
        &mut grit_lib::progress::NullProgress,
    )
    .expect("status");

    assert!(
        model.unstaged.iter().any(|e| e.path() == "tracked.txt"),
        "bootstrap must stat despite stale valid bits: {:?}",
        model.unstaged
    );
}

#[test]
fn grit_git_fsmonitor_hook_round_trip_index_and_status() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let repo_root = tmp.path();
    git(repo_root, &["init", "-q", "-b", "main", "."]);
    git(repo_root, &["config", "user.email", "t@example.com"]);
    git(repo_root, &["config", "user.name", "Test"]);
    let hook = write_fsmonitor_v2_report_all_hook(repo_root);
    git(
        repo_root,
        &[
            "config",
            "core.fsmonitor",
            hook.to_str().expect("hook utf8"),
        ],
    );
    fs::write(repo_root.join("tracked.txt"), "base\n").expect("write");
    git(repo_root, &["add", "tracked.txt"]);
    git(repo_root, &["commit", "-qm", "initial"]);
    isolated_git(repo_root, &["status", "-s"]);

    fs::write(repo_root.join("tracked.txt"), "modified\n").expect("modify");

    let grit_repo = Repository::open(
        &repo_root.join(".git"),
        &RepositoryOptions::empty().with_work_tree(repo_root),
    )
    .expect("open grit repo");
    let model = grit_lib::porcelain::status::status(
        &grit_repo,
        &grit_lib::porcelain::status::StatusOptions::default(),
        &mut grit_lib::progress::NullProgress,
    )
    .expect("grit status");

    assert!(
        model.unstaged.iter().any(|e| e.path() == "tracked.txt"),
        "grit must report hook-invalidated path: {:?}",
        model.unstaged
    );

    let git_porcelain = isolated_git(repo_root, &["status", "--porcelain"]);
    assert!(
        git_porcelain.contains("tracked.txt"),
        "git should agree on modified path: {git_porcelain}"
    );

    fsck_clean(repo_root);
}

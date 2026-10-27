//! Parallel worktree scans compare symlinks as link-target blobs (#2076).
#![cfg(unix)]
#![allow(clippy::expect_used)]

use std::collections::BTreeMap;
use std::fs;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::symlink;
use std::path::Path;

use grit_lib::config::environment::RepositoryOptions;
use grit_lib::diff::{
    diff_index_to_worktree_for_staging, diff_index_to_worktree_with_options, DiffEntry,
    DiffIndexToWorktreeOptions,
};
use grit_lib::index::Index;
use grit_lib::parallel::ThreadPolicy;
use grit_lib::repo::Repository;
use grit_test_support::git;
use tempfile::TempDir;

fn fixture() -> TempDir {
    let tmp = TempDir::new().expect("tempdir");
    let root = tmp.path();
    git(root, &["init", "-q", "-b", "main"]);
    git(root, &["config", "user.name", "Test"]);
    git(root, &["config", "user.email", "test@example.com"]);
    fs::create_dir(root.join("files")).expect("files directory");
    for n in 0..1024 {
        fs::write(root.join(format!("files/{n}")), b"data\n").expect("ordinary file");
    }
    for (name, target) in [
        ("linkdir", "files"),
        ("linkfile", "files/0"),
        ("linkmissing", "missing"),
    ] {
        symlink(target, root.join(name)).expect("symlink");
    }
    git(root, &["add", "."]);
    git(root, &["commit", "-qm", "initial"]);
    tmp
}

fn open(root: &Path) -> Repository {
    Repository::open(
        &root.join(".git"),
        &RepositoryOptions::empty().with_work_tree(root),
    )
    .expect("repository")
}

#[derive(Clone, Copy, Debug)]
enum StatState {
    Unset,
    Stale,
    Racy,
}

fn force_verification(index: &mut Index, state: StatState) -> Option<(u32, u32)> {
    for entry in index.entries_mut() {
        match state {
            StatState::Unset => {
                entry.mtime_sec = 0;
                entry.mtime_nsec = 0;
                entry.size = 0;
            }
            StatState::Stale => entry.mtime_sec = entry.mtime_sec.wrapping_sub(10),
            StatState::Racy => {}
        }
    }
    index.sort();
    // Every recorded mtime is at or after this boundary, without sleeps or clock races.
    matches!(state, StatState::Racy).then_some((1, 0))
}

fn git_statuses(root: &Path) -> BTreeMap<String, char> {
    git(root, &["diff", "--no-renames", "--name-status"])
        .lines()
        .map(|line| {
            let (status, path) = line.split_once('\t').expect("name status");
            (path.to_owned(), status.chars().next().expect("status"))
        })
        .collect()
}

fn statuses(entries: &[DiffEntry]) -> BTreeMap<String, char> {
    entries
        .iter()
        .map(|entry| (entry.path().to_owned(), entry.status.letter()))
        .collect()
}

#[derive(Clone, Copy, Debug)]
enum Scan {
    Diff,
    Status,
    Preview,
    Stage,
}

/// Exercise both public scans, one and four threads, and all forced stat states.
fn check_scans(root: &Path) {
    let expected = git_statuses(root);
    let repo = open(root);
    let original = repo.index_handle().load().expect("index");
    let index_bytes = fs::read(root.join(".git/index")).expect("disk index");
    for state in [StatState::Unset, StatState::Stale, StatState::Racy] {
        for refresh in [false, true] {
            for threads in [1, 4] {
                // Hash-only comes before materialization to prove it writes no missing blobs.
                for scan in [Scan::Diff, Scan::Status, Scan::Preview, Scan::Stage] {
                    let mut index = original.clone();
                    let index_mtime = force_verification(&mut index, state);
                    let options = DiffIndexToWorktreeOptions {
                        repository_git_dir: Some(repo.git_dir().to_owned()),
                        thread_policy: Some(ThreadPolicy::with_threads_override(Some(threads))),
                        index_mtime,
                        refresh_index_stat_in_pass: refresh,
                        hash_blobs_without_write: matches!(scan, Scan::Preview),
                        // Status enables parallel content hashing on the non-staging API.
                        // An unset cache also forces status past its size-only shortcut.
                        for_status: matches!(scan, Scan::Status),
                        refresh_fsmonitor: false,
                        ..Default::default()
                    };
                    let before = object_files(&root.join(".git/objects"));
                    let entries = if matches!(scan, Scan::Diff | Scan::Status) {
                        diff_index_to_worktree_with_options(repo.odb(), &mut index, root, options)
                            .expect("worktree diff")
                            .0
                    } else {
                        diff_index_to_worktree_for_staging(
                            repo.odb(),
                            repo.git_dir(),
                            &mut index,
                            root,
                            options,
                        )
                        .expect("staging scan")
                        .0
                    };
                    assert_eq!(
                        statuses(&entries),
                        expected,
                        "{state:?}, refresh={refresh}, threads={threads}, scan={scan:?}"
                    );
                    if !matches!(scan, Scan::Stage) {
                        assert_eq!(object_files(&root.join(".git/objects")), before);
                    }
                    if matches!(scan, Scan::Preview | Scan::Stage) {
                        for entry in &entries {
                            if entry.new_mode == "120000" {
                                let path = root.join(entry.path());
                                let bytes = if fs::symlink_metadata(&path)
                                    .expect("metadata")
                                    .file_type()
                                    .is_symlink()
                                {
                                    fs::read_link(path)
                                        .expect("target")
                                        .as_os_str()
                                        .as_bytes()
                                        .to_vec()
                                } else {
                                    fs::read(path).expect("placeholder")
                                };
                                let bytes = bytes.as_slice();
                                let oid = repo
                                    .odb()
                                    .hash_algo()
                                    .hash_object(grit_lib::objects::ObjectKind::Blob, bytes)
                                    .expect("link hash");
                                assert_eq!(entry.new_oid, oid, "link-target bytes");
                                if matches!(scan, Scan::Stage) {
                                    assert_eq!(
                                        repo.odb().read(&oid).expect("stored link").data,
                                        bytes
                                    );
                                    assert_eq!(
                                        git(root, &["cat-file", "blob", &oid.to_hex()]).as_bytes(),
                                        bytes
                                    );
                                }
                            }
                        }
                    }
                }
            }
        }
    }
    assert_eq!(
        fs::read(root.join(".git/index")).expect("disk index"),
        index_bytes
    );
    git(root, &["fsck", "--strict"]);
}

fn object_files(dir: &Path) -> Vec<std::path::PathBuf> {
    let mut paths = Vec::new();
    for item in fs::read_dir(dir).expect("object directory") {
        let path = item.expect("object entry").path();
        if path.is_dir() {
            paths.extend(object_files(&path));
        } else {
            paths.push(path);
        }
    }
    paths.sort();
    paths
}

#[test]
fn clean_directory_file_and_dangling_symlinks_match_git() {
    let tmp = fixture();
    assert!(git_statuses(tmp.path()).is_empty());
    check_scans(tmp.path());
}

#[test]
fn retargeted_symlinks_hash_target_bytes_without_conversion() {
    let tmp = fixture();
    let root = tmp.path();
    // Rules apply only to links. Neither clean filters nor CRLF conversion may touch targets.
    fs::write(
        root.join(".git/info/attributes"),
        "link* text filter=reject\n",
    )
    .expect("attributes");
    git(root, &["config", "core.autocrlf", "true"]);
    git(root, &["config", "filter.reject.clean", "false"]);
    git(root, &["config", "filter.reject.required", "true"]);
    for (name, target) in [
        ("linkdir", "."),
        ("linkfile", "files/1"),
        ("linkmissing", "absent\r\n"),
    ] {
        fs::remove_file(root.join(name)).expect("remove link");
        symlink(target, root.join(name)).expect("retarget link");
    }
    assert_eq!(git_statuses(root).len(), 3);
    check_scans(root);
}

#[test]
fn regular_file_and_symlink_type_changes_match_git() {
    let tmp = fixture();
    let root = tmp.path();
    fs::remove_file(root.join("files/0")).expect("remove regular file");
    symlink("1", root.join("files/0")).expect("file to link");
    fs::remove_file(root.join("linkfile")).expect("remove link");
    fs::write(root.join("linkfile"), b"regular replacement\n").expect("link to file");
    assert_eq!(
        git_statuses(root).values().copied().collect::<Vec<_>>(),
        vec!['T', 'T']
    );
    check_scans(root);
}

#[test]
fn ordinary_directory_replacements_are_deletions() {
    let tmp = fixture();
    let root = tmp.path();
    for name in ["files/0", "linkdir"] {
        fs::remove_file(root.join(name)).expect("remove tracked path");
        fs::create_dir(root.join(name)).expect("replace with directory");
    }
    assert_eq!(
        git_statuses(root).values().copied().collect::<Vec<_>>(),
        vec!['D', 'D']
    );
    check_scans(root);
}

#[test]
fn symlinks_false_regular_placeholders_keep_link_mode() {
    let tmp = fixture();
    let root = tmp.path();
    git(root, &["config", "core.symlinks", "false"]);
    for name in ["linkdir", "linkfile", "linkmissing"] {
        let target = fs::read_link(root.join(name)).expect("target");
        fs::remove_file(root.join(name)).expect("remove symlink");
        fs::write(root.join(name), target.as_os_str().as_bytes()).expect("placeholder");
    }
    assert!(git_statuses(root).is_empty());
    check_scans(root);
    fs::write(root.join("linkdir"), b"other directory").expect("edit placeholder");
    check_scans(root);
}

#[test]
fn uninitialized_gitlink_stays_on_its_separate_scan_path() {
    let tmp = fixture();
    let root = tmp.path();
    for name in ["linkdir", "linkfile", "linkmissing"] {
        fs::remove_file(root.join(name)).expect("remove control symlink");
    }
    git(root, &["add", "-u"]);
    let head = git(root, &["rev-parse", "HEAD"]);
    git(
        root,
        &[
            "update-index",
            "--add",
            "--cacheinfo",
            &format!("160000,{},submodule", head.trim()),
        ],
    );
    git(root, &["commit", "-qm", "gitlink"]);
    fs::create_dir(root.join("submodule")).expect("uninitialized checkout");
    assert!(git_statuses(root).is_empty());
    check_scans(root);
}

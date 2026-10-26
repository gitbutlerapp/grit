//! Reusing scan paths must preserve raw names, file types, and shorter successors.
#![cfg(unix)]

use std::collections::BTreeMap;
use std::ffi::OsStr;
use std::fs;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{symlink, PermissionsExt};
use std::path::Path;
use std::process::Command;

use grit_lib::config::environment::RepositoryOptions;
use grit_lib::porcelain::status::{status, StatusOptions, UntrackedMode};
use grit_lib::progress::{NeverCancel, NullProgress};
use grit_lib::repo::Repository;
use grit_test_support::{forced_parallel_policy, git};

fn git_changes(root: &Path) -> BTreeMap<Vec<u8>, char> {
    let output = Command::new("git")
        .current_dir(root)
        .env("GIT_OPTIONAL_LOCKS", "0")
        .args(["diff", "--name-status", "--no-renames", "-z"])
        .output()
        .expect("git diff");
    assert!(output.status.success());
    let fields: Vec<_> = output.stdout.split(|&b| b == 0).collect();
    fields
        .chunks_exact(2)
        .map(|row| (row[1].to_vec(), char::from(row[0][0])))
        .collect()
}

#[test]
fn serial_and_parallel_status_preserve_mixed_raw_paths() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();
    git(root, &["init", "-q", "-b", "main"]);
    git(root, &["config", "user.name", "Test"]);
    git(root, &["config", "user.email", "test@example.com"]);
    let mut paths = Vec::new();
    for n in 0..1_200 {
        let suffix = if n % 2 == 0 {
            "long-name-".repeat(12)
        } else {
            "s".into()
        };
        let path = format!("d{}/nested/{n:04}-{suffix}", n % 4);
        let absolute = root.join(&path);
        fs::create_dir_all(absolute.parent().expect("parent")).expect("mkdir");
        fs::write(&absolute, b"original\n").expect("write file");
        paths.push(absolute);
    }
    let raw_dir = root.join(OsStr::from_bytes(b"d0/raw-\xff"));
    fs::create_dir(&raw_dir).expect("raw directory");
    let raw = raw_dir.join("tracked");
    fs::write(&raw, b"raw\n").expect("raw file");
    let raw_leaf = root.join(OsStr::from_bytes(b"d0/nested/leaf-\xfe"));
    fs::write(&raw_leaf, b"raw leaf\n").expect("raw filename in dense directory");
    let link = root.join("d0/nested/link");
    symlink("0000-long-name", &link).expect("symlink");
    git(root, &["add", "-A"]);
    git(root, &["commit", "-qm", "mixed paths"]);

    fs::write(&paths[0], b"different size\n").expect("modify long path");
    fs::remove_file(&paths[1]).expect("delete short path");
    fs::remove_file(&paths[2]).expect("replace long path");
    symlink("missing", &paths[2]).expect("file to symlink");
    fs::set_permissions(&paths[3], fs::Permissions::from_mode(0o755)).expect("executable");
    fs::write(&raw, b"changed raw name\n").expect("modify raw file");
    fs::write(&raw_leaf, b"changed raw leaf\n").expect("modify raw filename");
    fs::remove_file(&link).expect("remove link");
    fs::write(&link, b"regular now\n").expect("symlink to file");
    for name in ["untracked-long-name", "u"] {
        fs::write(root.join("d0/nested").join(name), b"untracked\n").expect("untracked");
    }
    fs::write(raw_dir.join(OsStr::from_bytes(b"untracked-\xfe")), b"raw\n").expect("raw untracked");
    let output = Command::new("git")
        .current_dir(root)
        .args(["ls-files", "--others", "--exclude-standard", "-z"])
        .output()
        .expect("git untracked");
    assert!(output.status.success());
    let mut expected_untracked: Vec<_> = output
        .stdout
        .split(|&b| b == 0)
        .filter(|name| !name.is_empty())
        .map(<[u8]>::to_vec)
        .collect();
    expected_untracked.sort();
    assert_eq!(expected_untracked.len(), 3);
    let expected = git_changes(root);
    assert_eq!(expected.len(), 7);
    // The oracle may refresh index caches. Force a content-clean stat mismatch afterward
    // so only grit's status can persist the new timestamp below.
    let index_path = root.join(".git/index");
    let original_index = fs::read(&index_path).expect("index before status");
    let before = grit_lib::index::Index::load(&index_path).expect("original index");
    assert_ne!(
        before
            .get(b"d1/nested/0005-s", 0)
            .expect("tracked path")
            .mtime_sec,
        2_000
    );
    filetime::set_file_mtime(&paths[5], filetime::FileTime::from_unix_time(2_000, 12_345))
        .expect("change timestamp without changing content");

    for threads in [1, 4] {
        let repo = Repository::open(
            &root.join(".git"),
            &RepositoryOptions::empty().with_work_tree(root),
        )
        .expect("open")
        .with_thread_policy(forced_parallel_policy());
        let options = StatusOptions {
            stat_parallel_threads: Some(threads),
            untracked: UntrackedMode::All,
            ..StatusOptions::default()
        };
        for _ in 0..2 {
            let model = status(&repo, &options, &mut NullProgress, &NeverCancel).expect("status");
            assert!(model.staged.is_empty());
            let mut untracked: Vec<_> = model
                .untracked
                .iter()
                .map(|path| grit_lib::repo_relpath::storage_bytes(path).into_owned())
                .collect();
            untracked.sort();
            assert_eq!(untracked, expected_untracked, "threads={threads}");
            let changes: BTreeMap<_, _> = model
                .unstaged
                .iter()
                .map(|e| {
                    (
                        grit_lib::repo_relpath::storage_bytes(e.path()).into_owned(),
                        e.status.letter(),
                    )
                })
                .collect();
            assert_eq!(changes, expected, "threads={threads}");
        }
    }
    // Git must still read the index after grit's stat refresh and the object store it references.
    assert_ne!(
        fs::read(&index_path).expect("refreshed index"),
        original_index
    );
    let refreshed = grit_lib::index::Index::load(&index_path).expect("refreshed index");
    assert_eq!(
        refreshed
            .get(b"d1/nested/0005-s", 0)
            .expect("tracked path")
            .mtime_sec,
        2_000
    );
    assert_eq!(git_changes(root), expected);
    git(root, &["fsck", "--strict"]);
}

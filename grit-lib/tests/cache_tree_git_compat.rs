//! Cache-tree / write-tree compatibility with the system `git` binary.
//!
//! Builds a multi-file tree with `git`, loads the index through `grit-lib`,
//! updates the cache-tree incrementally, and checks that the resulting tree OID
//! matches `git write-tree` on the same index bytes. Objects are validated with
//! `git fsck` and `git cat-file`.

use std::path::Path;
use std::process::Command;

use grit_lib::index::Index;
use grit_lib::objects::ObjectId;
use grit_lib::odb::Odb;
use grit_lib::repo::Repository;
use grit_lib::write_tree::{cache_tree_fully_valid, write_tree_update_index, WriteTreeFlags};
use grit_test_support::git;

fn git_out(dir: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .current_dir(dir)
        .args(args)
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
    String::from_utf8_lossy(&out.stdout).trim().to_owned()
}

fn fsck_clean(repo_dir: &Path) {
    let out = Command::new("git")
        .current_dir(repo_dir)
        .args(["fsck", "--strict"])
        .output()
        .expect("git fsck");
    assert!(
        out.status.success(),
        "git fsck failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

#[test]
fn grit_cache_tree_matches_git_write_tree_after_single_path_change() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let repo_dir = tmp.path().join("repo");
    std::fs::create_dir_all(&repo_dir).unwrap();
    git(&repo_dir, &["init", "-q", "-b", "main", "."]);

    for d in 0..5 {
        let dir = repo_dir.join(format!("d{d}"));
        std::fs::create_dir_all(&dir).unwrap();
        for f in 0..40 {
            std::fs::write(dir.join(format!("f{f}.txt")), format!("{d}/{f}\n")).unwrap();
        }
    }
    git(&repo_dir, &["add", "-A"]);
    git(&repo_dir, &["commit", "-q", "-m", "initial"]);

    std::fs::write(repo_dir.join("d2/f10.txt"), "changed\n").unwrap();
    git(&repo_dir, &["add", "d2/f10.txt"]);

    let git_tree_hex = git_out(&repo_dir, &["write-tree"]);
    let git_tree = ObjectId::from_hex(&git_tree_hex).expect("git write-tree oid");

    let grit_repo = Repository::discover(Some(&repo_dir)).expect("open repo");
    let mut index = grit_repo.load_index().expect("load index");
    assert!(
        index.cache_tree.is_some(),
        "git-produced index should carry a TREE extension"
    );

    index.invalidate_cache_tree_for_path(b"d2");
    let grit_tree =
        write_tree_update_index(&grit_repo.odb, &mut index, "", WriteTreeFlags::default())
            .expect("grit write-tree");
    assert_eq!(grit_tree, git_tree);
    assert!(cache_tree_fully_valid(
        &grit_repo.odb,
        index.cache_tree.as_ref()
    ));

    grit_repo
        .write_index(&mut index)
        .expect("persist index with cache-tree");
    let git_tree_after_index = ObjectId::from_hex(&git_out(&repo_dir, &["write-tree"]))
        .expect("git write-tree after grit index write");
    assert_eq!(git_tree_after_index, git_tree);

    fsck_clean(&repo_dir);

    let kind = git_out(&repo_dir, &["cat-file", "-t", &grit_tree.to_hex()]);
    assert_eq!(kind, "tree");
}

#[test]
fn grit_written_tree_round_trips_through_git_read() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let objects = tmp.path().join("objects");
    std::fs::create_dir_all(&objects).unwrap();
    let odb = Odb::new(&objects);

    let mut index = Index::new();
    let blob = odb
        .write(grit_lib::objects::ObjectKind::Blob, b"payload")
        .unwrap();
    index.add_or_replace(grit_lib::index::IndexEntry {
        ctime_sec: 0,
        ctime_nsec: 0,
        mtime_sec: 0,
        mtime_nsec: 0,
        dev: 0,
        ino: 0,
        mode: grit_lib::index::MODE_REGULAR,
        uid: 0,
        gid: 0,
        size: 7,
        oid: blob,
        flags: 4,
        flags_extended: None,
        path: b"file.txt".to_vec(),
        base_index_pos: 0,
    });

    let tree_oid =
        write_tree_update_index(&odb, &mut index, "", WriteTreeFlags::default()).unwrap();
    assert!(cache_tree_fully_valid(&odb, index.cache_tree.as_ref()));

    let git_dir = tmp.path().join("mini.git");
    std::fs::create_dir_all(git_dir.join("objects")).unwrap();
    std::fs::write(git_dir.join("HEAD"), "ref: refs/heads/main\n").unwrap();
    std::fs::create_dir_all(git_dir.join("refs/heads")).unwrap();
    std::fs::write(
        git_dir.join("refs/heads/main"),
        format!("{}\n", ObjectId::zero().to_hex()),
    )
    .unwrap();

    // Copy loose objects into a minimal git dir for cat-file.
    for entry in std::fs::read_dir(&objects).unwrap().flatten() {
        if !entry.file_type().map(|t| t.is_dir()).unwrap_or(false) {
            continue;
        }
        let shard = entry.file_name();
        let dst_shard = git_dir.join("objects").join(shard);
        std::fs::create_dir_all(&dst_shard).unwrap();
        for obj in std::fs::read_dir(entry.path()).unwrap().flatten() {
            std::fs::copy(obj.path(), dst_shard.join(obj.file_name())).unwrap();
        }
    }

    let kind = Command::new("git")
        .args([
            "--git-dir",
            git_dir.to_str().unwrap(),
            "cat-file",
            "-t",
            &tree_oid.to_hex(),
        ])
        .output()
        .expect("git cat-file");
    assert!(kind.status.success());
    assert_eq!(String::from_utf8_lossy(&kind.stdout).trim(), "tree");
}

//! System-git round-trip for concurrent disjoint ref updates (issue #2250).

use std::fs;
use std::process::Command;
use std::sync::Arc;

use grit_lib::ident::Signature;
use grit_lib::objects::{NewCommit, ObjectKind, TreeEntry};
use grit_lib::refs::store::RefName;
use grit_lib::repo::{InitOptions, Repository};
use grit_test_support::git_fsck;
use time::OffsetDateTime;

#[test]
fn files_disjoint_ref_updates_git_show_ref_and_fsck() {
    const THREADS: usize = 8;
    const ITERS: usize = 50;
    let tmp = tempfile::tempdir().expect("tempdir");
    let repo = Arc::new(Repository::init(tmp.path(), &InitOptions::default()).expect("init"));
    let sig = Signature::new(
        "A",
        "a@example.com",
        OffsetDateTime::from_unix_timestamp(1_700_000_000).expect("time"),
    )
    .expect("sig");
    let blob = repo
        .objects()
        .write(ObjectKind::Blob, b"x\n")
        .expect("blob");
    let tree = repo
        .objects()
        .write_tree([TreeEntry {
            mode: 0o100644,
            name: b"f".to_vec(),
            oid: blob,
        }])
        .expect("tree");
    let commit = repo
        .objects()
        .write_commit(&NewCommit::new(tree, sig.clone(), sig, "c"))
        .expect("commit");

    std::thread::scope(|s| {
        for t in 0..THREADS {
            let repo = Arc::clone(&repo);
            s.spawn(move || {
                let name = RefName::try_from(format!("refs/heads/t{t}").as_str()).expect("ref");
                for _ in 0..ITERS {
                    repo.references().update(&name, &commit).expect("update");
                }
            });
        }
    });

    let show = Command::new("git")
        .args([
            "-C",
            tmp.path().to_str().expect("utf8"),
            "show-ref",
            "--heads",
        ])
        .output()
        .expect("git show-ref");
    assert!(show.status.success(), "{show:?}");
    let stdout = String::from_utf8_lossy(&show.stdout);
    for t in 0..THREADS {
        assert!(
            stdout.contains(&format!("refs/heads/t{t}")),
            "missing refs/heads/t{t} in:\n{stdout}"
        );
    }
    let fsck = git_fsck(tmp.path(), true);
    assert!(fsck.ok, "{fsck:?}");
}

#[test]
fn git_creates_ref_over_stale_empty_directory_at_exact_ref_path() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let repo = Repository::init(tmp.path(), &InitOptions::default()).expect("init");
    let sig = Signature::new(
        "A",
        "a@example.com",
        OffsetDateTime::from_unix_timestamp(1_700_000_000).expect("time"),
    )
    .expect("sig");
    let blob = repo
        .objects()
        .write(ObjectKind::Blob, b"x\n")
        .expect("blob");
    let tree = repo
        .objects()
        .write_tree([TreeEntry {
            mode: 0o100644,
            name: b"f".to_vec(),
            oid: blob,
        }])
        .expect("tree");
    let commit = repo
        .objects()
        .write_commit(&NewCommit::new(tree, sig.clone(), sig, "c"))
        .expect("commit");

    let stale = repo.git_dir().join("refs/tags/empty");
    fs::create_dir_all(&stale).expect("stale empty directory at ref path");
    assert!(stale.is_dir());

    let update = Command::new("git")
        .args([
            "-C",
            tmp.path().to_str().expect("utf8"),
            "update-ref",
            "refs/tags/empty",
            &commit.to_hex(),
        ])
        .status()
        .expect("git update-ref");
    assert!(
        update.success(),
        "git must create a ref over an empty directory at the ref path"
    );
    assert!(
        stale.is_file(),
        "git replaces the empty directory with the ref file"
    );

    let show = Command::new("git")
        .args([
            "-C",
            tmp.path().to_str().expect("utf8"),
            "show-ref",
            "refs/tags/empty",
        ])
        .output()
        .expect("git show-ref");
    assert!(show.status.success(), "{show:?}");
    let stdout = String::from_utf8_lossy(&show.stdout);
    assert!(
        stdout.contains(&commit.to_hex()),
        "expected {commit} in show-ref output:\n{stdout}"
    );
}

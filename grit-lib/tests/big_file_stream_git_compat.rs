//! Files of at least `core.bigFileThreshold` are hashed and stored as a stream into a pack
//! instead of being read into memory (#1850), and the result round-trips with system `git`.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::fs;
use std::path::{Path, PathBuf};

use grit_lib::config::environment::RepositoryOptions;
use grit_lib::objects::{ObjectId, ObjectKind};
use grit_lib::porcelain::add::{stage, StageOptions};
use grit_lib::porcelain::commit::{commit_worktree, CommitRequest, HookPolicy};
use grit_lib::porcelain::status::{status, StatusOptions};
use grit_lib::progress::{NeverCancel, NullProgress};
use grit_lib::repo::Repository;
use grit_test_support::git;

/// Deterministic multi-line text of exactly `size` bytes.
fn text(size: usize, seed: u32) -> Vec<u8> {
    let mut out = Vec::with_capacity(size + 32);
    let mut n = seed;
    while out.len() < size {
        n = n.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
        out.extend_from_slice(format!("line {n:010}\n").as_bytes());
    }
    out.truncate(size);
    out
}

fn init(threshold: &str) -> tempfile::TempDir {
    let dir = tempfile::tempdir().expect("tempdir");
    git(dir.path(), &["init", "-q", "-b", "main"]);
    git(dir.path(), &["config", "core.bigFileThreshold", threshold]);
    git(dir.path(), &["config", "user.name", "Big File"]);
    git(dir.path(), &["config", "user.email", "big@example.com"]);
    dir
}

fn open(root: &Path) -> Repository {
    Repository::open(
        &root.join(".git"),
        &RepositoryOptions::empty().with_work_tree(root),
    )
    .expect("open repo")
}

fn index_oid(root: &Path, path: &str) -> ObjectId {
    ObjectId::from_hex(git(root, &["rev-parse", &format!(":{path}")]).trim()).expect("oid")
}

fn pack_files(root: &Path) -> Vec<PathBuf> {
    let mut packs: Vec<PathBuf> = fs::read_dir(root.join(".git/objects/pack"))
        .map(|rd| {
            rd.map(|e| e.expect("entry").path())
                .filter(|p| p.extension().is_some_and(|e| e == "pack"))
                .collect()
        })
        .unwrap_or_default();
    packs.sort();
    packs
}

fn loose_path(root: &Path, oid: &ObjectId) -> PathBuf {
    let hex = oid.to_hex();
    root.join(".git/objects").join(&hex[..2]).join(&hex[2..])
}

fn commit_req(message: &str) -> CommitRequest {
    let id = "Big File <big@example.com> 1700000000 +0000";
    {
        let mut o = CommitRequest::default();
        o.message = message.to_owned();
        o.author = Some(id.parse().expect("signature"));
        o.committer = Some(id.parse().expect("signature"));
        o.hooks = HookPolicy::SkipVerify;
        o
    }
}

#[test]
fn add_streams_big_file_into_a_pack_that_git_reads() {
    let dir = init("64k");
    let root = dir.path();
    let big = text(2 * 1024 * 1024 + 3, 1);
    fs::write(root.join("big.txt"), &big).unwrap();
    fs::write(root.join("small.txt"), b"small\n").unwrap();

    stage(
        &open(root),
        &StageOptions::default(),
        &mut NullProgress,
        &NeverCancel,
    )
    .expect("stage");

    let big_oid = index_oid(root, "big.txt");
    assert_eq!(
        git(root, &["hash-object", "big.txt"]).trim(),
        big_oid.to_hex()
    );
    assert!(
        !loose_path(root, &big_oid).exists(),
        "big blob must be streamed into a pack, not written loose"
    );
    assert_eq!(pack_files(root).len(), 1);
    assert!(loose_path(root, &index_oid(root, "small.txt")).is_file());

    assert_eq!(git(root, &["cat-file", "-p", ":big.txt"]).as_bytes(), big);
    git(root, &["fsck", "--strict"]);
    assert_eq!(
        git(root, &["status", "--porcelain"]),
        "A  big.txt\nA  small.txt\n"
    );
}

#[test]
fn commit_auto_stage_streams_big_files_and_restages_changes() {
    let dir = init("32k");
    let root = dir.path();
    fs::write(root.join("a.txt"), text(500_000, 2)).unwrap();
    fs::write(root.join("b.txt"), text(500_000, 2)).unwrap();

    let repo = open(root);
    commit_worktree(
        &repo,
        &commit_req("add big"),
        &mut NullProgress,
        &NeverCancel,
    )
    .expect("commit");
    assert_eq!(
        pack_files(root).len(),
        1,
        "identical content under two paths is stored once"
    );
    git(root, &["fsck", "--strict"]);
    assert_eq!(git(root, &["status", "--porcelain"]), "");
    assert_eq!(
        git(root, &["cat-file", "-s", "HEAD:a.txt"]).trim(),
        "500000"
    );

    let changed = text(700_000, 3);
    fs::write(root.join("a.txt"), &changed).unwrap();
    let repo = open(root);
    commit_worktree(
        &repo,
        &commit_req("change big"),
        &mut NullProgress,
        &NeverCancel,
    )
    .expect("commit");
    assert_eq!(pack_files(root).len(), 2);
    assert_eq!(
        git(root, &["cat-file", "-p", "HEAD:a.txt"]).as_bytes(),
        changed
    );
    git(root, &["fsck", "--strict"]);
    assert_eq!(git(root, &["status", "--porcelain"]), "");
}

#[test]
fn big_file_with_a_clean_conversion_is_converted_like_git() {
    let dir = init("1k");
    let root = dir.path();
    fs::write(root.join(".gitattributes"), "*.txt text\n").unwrap();
    let crlf: Vec<u8> = text(200_000, 4)
        .split(|&b| b == b'\n')
        .flat_map(|line| line.iter().copied().chain(*b"\r\n"))
        .collect();
    fs::write(root.join("crlf.txt"), &crlf).unwrap();

    stage(
        &open(root),
        &StageOptions::default(),
        &mut NullProgress,
        &NeverCancel,
    )
    .expect("stage");

    let oid = index_oid(root, "crlf.txt");
    assert_eq!(
        git(root, &["hash-object", "--path", "crlf.txt", "crlf.txt"]).trim(),
        oid.to_hex()
    );
    let stored = git(root, &["cat-file", "-p", ":crlf.txt"]);
    assert!(!stored.contains('\r'), "line endings are normalized");
    assert!(
        pack_files(root).is_empty(),
        "converted content takes the buffered loose path"
    );
    git(root, &["fsck", "--strict"]);
}

#[test]
fn big_file_git_streamed_reads_back_clean_in_grit() {
    let dir = init("16k");
    let root = dir.path();
    let big = text(400_000, 5);
    fs::write(root.join("data.txt"), &big).unwrap();
    git(root, &["add", "data.txt"]);
    git(root, &["commit", "-q", "-m", "git big file"]);
    assert_eq!(
        pack_files(root).len(),
        1,
        "git streams big files into a pack"
    );

    let repo = open(root);
    let oid = index_oid(root, "data.txt");
    let obj = repo.odb().read(&oid).expect("read git's packed blob");
    assert_eq!(obj.kind, ObjectKind::Blob);
    assert_eq!(obj.data, big);

    // Rewriting the same bytes changes the stat data, so status re-hashes the content
    // through the stream path.
    std::thread::sleep(std::time::Duration::from_millis(10));
    fs::write(root.join("data.txt"), &big).unwrap();
    let model = status(
        &repo,
        &StatusOptions::default(),
        &mut NullProgress,
        &NeverCancel,
    )
    .expect("status");
    assert!(model.unstaged.is_empty(), "{:?}", model.unstaged);
    assert!(model.staged.is_empty(), "{:?}", model.staged);
}

#[test]
fn dry_run_hashes_big_file_without_storing_it() {
    let dir = init("8k");
    let root = dir.path();
    fs::write(root.join("big.txt"), text(100_000, 6)).unwrap();
    let opts = StageOptions {
        dry_run: true,
        ..StageOptions::default()
    };
    let outcome = stage(&open(root), &opts, &mut NullProgress, &NeverCancel).expect("dry run");
    assert_eq!(outcome.added, vec!["big.txt".to_owned()]);
    assert!(pack_files(root).is_empty());
    assert_eq!(
        git(root, &["count-objects"]).trim(),
        "0 objects, 0 kilobytes"
    );
}

//! Integration tests for the in-process maintenance primitives in
//! `grit_lib::refs::gc` — the remaining non-transport "replaced paths" from the jj
//! spike (PR #9632):
//!
//! * [`prune_loose_unreachable`] (the `jj util gc` core)
//! * [`remote_default_branch_local`] (the `git remote show` default-branch lookup)
//! * [`update_refs`] (the CAS, all-or-nothing batch ref transaction)
//!
//! Repos are built with the system `git`; results are cross-checked with
//! `git fsck` / `git rev-parse` and direct object existence.

use std::path::Path;
use std::process::Command;

use grit_lib::objects::odb::Odb;
use grit_lib::objects::ObjectId;
use grit_lib::refs::gc::{
    prune_loose_unreachable, remote_default_branch_local, update_refs, RefTransactionItem,
};

fn git(dir: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .current_dir(dir)
        .args(args)
        .env("GIT_AUTHOR_NAME", "T")
        .env("GIT_AUTHOR_EMAIL", "t@example.com")
        .env("GIT_AUTHOR_DATE", "2005-04-07T22:13:13 +0200")
        .env("GIT_COMMITTER_NAME", "T")
        .env("GIT_COMMITTER_EMAIL", "t@example.com")
        .env("GIT_COMMITTER_DATE", "2005-04-07T22:13:13 +0200")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .output()
        .expect("run git");
    assert!(
        out.status.success(),
        "git {:?} failed: {}",
        args,
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8(out.stdout).expect("utf8 git output")
}

fn git_try(dir: &Path, args: &[&str]) -> (bool, String) {
    let out = Command::new("git")
        .current_dir(dir)
        .args(args)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .output()
        .expect("run git");
    let mut combined = String::from_utf8_lossy(&out.stdout).into_owned();
    combined.push_str(&String::from_utf8_lossy(&out.stderr));
    (out.status.success(), combined)
}

fn rev_parse(dir: &Path, rev: &str) -> ObjectId {
    ObjectId::from_hex(git(dir, &["rev-parse", rev]).trim()).expect("valid oid")
}

fn odb_for(git_dir: &Path) -> Odb {
    Odb::new(&git_dir.join("objects")).with_config_git_dir(git_dir.to_path_buf())
}

/// Reachability closure used by prune: the commit, its tree, and the tree's blobs
/// must all be retained; an unreachable dangling object must be removed.
#[test]
fn prune_removes_unreachable_keeps_reachable() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let repo = tmp.path().join("repo");
    std::fs::create_dir_all(&repo).unwrap();
    git(&repo, &["init", "-q", "-b", "main", "."]);
    let git_dir = repo.join(".git");

    // One commit on main: introduces a tree + a blob, all reachable from the tip.
    std::fs::write(repo.join("a.txt"), "hello\n").unwrap();
    git(&repo, &["add", "a.txt"]);
    git(&repo, &["commit", "-q", "-m", "c1"]);

    let tip = rev_parse(&repo, "refs/heads/main");
    let tree = rev_parse(&repo, "refs/heads/main^{tree}");
    let blob = rev_parse(&repo, "HEAD:a.txt");

    // A dangling/unreachable loose object: hash-object -w writes a blob that no
    // ref or commit references.
    let dangling = write_dangling(&repo, b"unreferenced junk\n");

    let odb = odb_for(&git_dir);

    // Sanity: all four objects are present loose before prune.
    for oid in [&tip, &tree, &blob, &dangling] {
        assert!(odb.exists(oid), "object {oid} should exist before prune");
    }

    // Prune with the live ref tip as the only root, no grace window.
    let stats = prune_loose_unreachable(&odb, &[tip], None).expect("prune");

    // The dangling blob must be gone; the reachable objects must remain.
    assert!(
        !git_dir
            .join("objects")
            .join(dangling.loose_prefix())
            .join(dangling.loose_suffix())
            .exists(),
        "dangling object {dangling} should have been pruned"
    );
    assert!(odb.exists(&tip), "tip kept");
    assert!(odb.exists(&tree), "tree kept");
    assert!(odb.exists(&blob), "blob kept");

    assert_eq!(stats.pruned, 1, "exactly one object pruned");
    assert!(stats.kept >= 3, "at least commit+tree+blob kept");

    // git fsck must stay clean after the prune.
    let (ok, out) = git_try(&git_dir, &["fsck", "--strict"]);
    assert!(ok, "git fsck not clean after prune: {out}");
}

/// `keep_newer_than` must protect a recently-written unreachable object.
#[test]
fn prune_respects_grace_window() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let repo = tmp.path().join("repo");
    std::fs::create_dir_all(&repo).unwrap();
    git(&repo, &["init", "-q", "-b", "main", "."]);
    let git_dir = repo.join(".git");

    std::fs::write(repo.join("a.txt"), "hello\n").unwrap();
    git(&repo, &["add", "a.txt"]);
    git(&repo, &["commit", "-q", "-m", "c1"]);
    let tip = rev_parse(&repo, "refs/heads/main");

    // A fresh dangling object (just written, mtime ~= now).
    let dangling = write_dangling(&repo, b"recent junk\n");
    let odb = odb_for(&git_dir);
    assert!(odb.exists(&dangling));

    // Cutoff in the past: the recent object is newer than the cutoff, so it is
    // kept even though unreachable.
    let cutoff = std::time::SystemTime::now() - std::time::Duration::from_secs(3600);
    let stats = prune_loose_unreachable(&odb, &[tip], Some(cutoff)).expect("prune");
    assert!(
        odb.exists(&dangling),
        "recent unreachable object must be kept by the grace window"
    );
    assert_eq!(stats.pruned, 0, "nothing pruned within the grace window");

    // Cutoff in the future: now the object is older than the cutoff and pruned.
    let cutoff = std::time::SystemTime::now() + std::time::Duration::from_secs(3600);
    let stats = prune_loose_unreachable(&odb, &[tip], Some(cutoff)).expect("prune");
    assert!(
        !odb.exists(&dangling),
        "object older than the (future) cutoff must be pruned"
    );
    assert_eq!(stats.pruned, 1);
}

#[test]
fn default_branch_is_main_for_normal_repo() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let repo = tmp.path().join("repo");
    std::fs::create_dir_all(&repo).unwrap();
    git(&repo, &["init", "-q", "-b", "main", "."]);
    let git_dir = repo.join(".git");
    std::fs::write(repo.join("a.txt"), "hi\n").unwrap();
    git(&repo, &["add", "a.txt"]);
    git(&repo, &["commit", "-q", "-m", "c1"]);

    let branch = remote_default_branch_local(&git_dir).expect("default branch");
    assert_eq!(branch.as_deref(), Some("main"));

    // A bare clone preserves HEAD -> main too.
    let bare = tmp.path().join("bare.git");
    git(
        tmp.path(),
        &[
            "clone",
            "-q",
            "--bare",
            repo.to_str().unwrap(),
            bare.to_str().unwrap(),
        ],
    );
    let branch = remote_default_branch_local(&bare).expect("default branch bare");
    assert_eq!(branch.as_deref(), Some("main"));
}

#[test]
fn default_branch_for_empty_bare_repo() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let bare = tmp.path().join("empty.git");
    git(
        tmp.path(),
        &["init", "-q", "--bare", "-b", "main", bare.to_str().unwrap()],
    );
    let branch = remote_default_branch_local(&bare).expect("default branch empty bare");
    assert_eq!(branch.as_deref(), Some("main"));
}

/// CAS semantics + all-or-nothing application in one batch.
#[test]
fn update_refs_cas_and_atomicity() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let repo = tmp.path().join("repo");
    std::fs::create_dir_all(&repo).unwrap();
    git(&repo, &["init", "-q", "-b", "main", "."]);
    let git_dir = repo.join(".git");

    std::fs::write(repo.join("a.txt"), "one\n").unwrap();
    git(&repo, &["add", "a.txt"]);
    git(&repo, &["commit", "-q", "-m", "c1"]);
    let c1 = rev_parse(&repo, "refs/heads/main");

    std::fs::write(repo.join("a.txt"), "two\n").unwrap();
    git(&repo, &["add", "a.txt"]);
    git(&repo, &["commit", "-q", "-m", "c2"]);
    let c2 = rev_parse(&repo, "refs/heads/main");

    // Seed a ref to update + a ref to delete.
    git(&repo, &["update-ref", "refs/heads/to-update", &c1.to_hex()]);
    git(&repo, &["update-ref", "refs/heads/to-delete", &c1.to_hex()]);

    // 1. A batch with a wrong expected_old must apply NOTHING (all-or-nothing).
    let bad = vec![
        // Valid: create a brand-new ref (no CAS).
        RefTransactionItem {
            name: "refs/heads/created".to_owned(),
            new_oid: Some(c2),
            expected_old: None,
        },
        // Valid CAS update.
        RefTransactionItem {
            name: "refs/heads/to-update".to_owned(),
            new_oid: Some(c2),
            expected_old: Some(c1),
        },
        // INVALID CAS: expects c2 but ref currently holds c1.
        RefTransactionItem {
            name: "refs/heads/to-delete".to_owned(),
            new_oid: None,
            expected_old: Some(c2),
        },
    ];
    let err = update_refs(&git_dir, &bad).expect_err("batch with bad CAS must fail");
    let _ = err;

    // Nothing applied: created absent, to-update still c1, to-delete still present.
    assert!(
        remote_ref(&git_dir, "refs/heads/created").is_none(),
        "failed batch must not create refs"
    );
    assert_eq!(
        remote_ref(&git_dir, "refs/heads/to-update"),
        Some(c1),
        "failed batch must not update refs"
    );
    assert_eq!(
        remote_ref(&git_dir, "refs/heads/to-delete"),
        Some(c1),
        "failed batch must not delete refs"
    );

    // 2. The same batch with the CORRECT expected_old applies everything.
    let good = vec![
        RefTransactionItem {
            name: "refs/heads/created".to_owned(),
            new_oid: Some(c2),
            expected_old: None,
        },
        RefTransactionItem {
            name: "refs/heads/to-update".to_owned(),
            new_oid: Some(c2),
            expected_old: Some(c1),
        },
        RefTransactionItem {
            name: "refs/heads/to-delete".to_owned(),
            new_oid: None,
            expected_old: Some(c1),
        },
    ];
    update_refs(&git_dir, &good).expect("good batch applies");

    assert_eq!(remote_ref(&git_dir, "refs/heads/created"), Some(c2));
    assert_eq!(remote_ref(&git_dir, "refs/heads/to-update"), Some(c2));
    assert!(
        remote_ref(&git_dir, "refs/heads/to-delete").is_none(),
        "to-delete must be gone"
    );

    // 3. CAS against an absent ref: expected_old set but ref does not exist → fail.
    let absent_cas = vec![RefTransactionItem {
        name: "refs/heads/never".to_owned(),
        new_oid: Some(c2),
        expected_old: Some(c1),
    }];
    update_refs(&git_dir, &absent_cas).expect_err("CAS on an absent ref must fail");
    assert!(remote_ref(&git_dir, "refs/heads/never").is_none());

    let (ok, out) = git_try(&git_dir, &["fsck", "--strict"]);
    assert!(ok, "git fsck not clean after ref transaction: {out}");
}

/// Current value of `refname` via the system git, or `None` if absent.
fn remote_ref(git_dir: &Path, refname: &str) -> Option<ObjectId> {
    let (ok, out) = git_try(git_dir, &["rev-parse", "--verify", "-q", refname]);
    if ok {
        ObjectId::from_hex(out.trim()).ok()
    } else {
        None
    }
}

/// Write `content` as a dangling loose blob via `git hash-object -w --stdin`.
fn write_dangling(repo: &Path, content: &[u8]) -> ObjectId {
    let child = Command::new("git")
        .current_dir(repo)
        .args(["hash-object", "-w", "--stdin"])
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .spawn()
        .expect("spawn hash-object");
    dangling_write(&child, content);
    let res = child.wait_with_output().expect("hash-object output");
    ObjectId::from_hex(String::from_utf8_lossy(&res.stdout).trim()).expect("dangling oid")
}

fn dangling_write(child: &std::process::Child, content: &[u8]) {
    use std::io::Write as _;
    child
        .stdin
        .as_ref()
        .expect("stdin")
        .write_all(content)
        .expect("write stdin");
}

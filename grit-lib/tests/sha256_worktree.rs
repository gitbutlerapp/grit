//! SHA-256 worktree hashing, patch-ids, and prune-packed compatibility with system git.

use std::fs;
use std::path::Path;
use std::process::Command;

use grit_lib::objects::HashAlgo;
use grit_lib::patch_ids::compute_patch_id;
use grit_lib::porcelain::add::{stage, StageOptions};
use grit_lib::porcelain::status::{status, StatusOptions};
use grit_lib::progress::NullProgress;
use grit_lib::prune_packed::{prune_packed_objects, PrunePackedOptions};
use grit_lib::repo::Repository;
use grit_lib::rerere::rerere_conflict_id;
use grit_test_support::git;

fn git_sha256_supported() -> bool {
    let Ok(probe_dir) = tempfile::TempDir::new() else {
        return false;
    };
    Command::new("git")
        .args(["init", "--object-format=sha256", "--bare", "."])
        .current_dir(probe_dir.path())
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

fn init_sha256_repo(root: &Path) {
    git(
        root,
        &[
            "init",
            "-q",
            "--object-format=sha256",
            "--initial-branch=main",
            ".",
        ],
    );
    git(root, &["config", "user.email", "t@example.com"]);
    git(root, &["config", "user.name", "Test"]);
}

fn touch_all_tracked(repo: &Path) {
    let out = git(repo, &["ls-files", "-z"]);
    for path in out.split('\0').filter(|p| !p.is_empty()) {
        let abs = repo.join(path);
        filetime::set_file_mtime(&abs, filetime::FileTime::now()).expect("touch");
    }
}

fn git_patch_id_stable(repo: &Path, commit: &str) -> String {
    let diff = git(repo, &["show", "-p", "--pretty=format:", commit]);
    let output = Command::new("git")
        .args(["patch-id", "--stable"])
        .current_dir(repo)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .spawn()
        .expect("spawn git patch-id");
    use std::io::Write;
    {
        let mut stdin = output.stdin.as_ref().expect("stdin");
        stdin.write_all(diff.as_bytes()).expect("write diff");
    }
    let out = output.wait_with_output().expect("patch-id output");
    assert!(out.status.success(), "git patch-id failed");
    std::str::from_utf8(&out.stdout)
        .expect("utf-8 patch-id output")
        .split_whitespace()
        .next()
        .expect("patch-id token")
        .to_string()
}

#[test]
fn sha256_stat_refresh_staging_patch_id_and_prune_packed() {
    if !git_sha256_supported() {
        eprintln!("SKIP: git lacks --object-format=sha256");
        return;
    }

    let tmp = tempfile::tempdir().expect("tempdir");
    let repo_root = tmp.path();
    init_sha256_repo(repo_root);

    fs::write(repo_root.join("tracked.txt"), "initial\n").expect("write");
    git(repo_root, &["add", "tracked.txt"]);
    git(repo_root, &["commit", "-qm", "initial"]);

    touch_all_tracked(repo_root);

    let grit_repo =
        Repository::open(&repo_root.join(".git"), Some(repo_root)).expect("open grit repo");
    assert_eq!(grit_repo.odb.hash_algo(), HashAlgo::Sha256);

    let model = status(&grit_repo, &StatusOptions::default(), &mut NullProgress)
        .expect("status after touch");
    assert!(
        model.unstaged.is_empty() && model.staged.is_empty(),
        "grit status must be clean after stat refresh, got {model:?}"
    );

    let porcelain = git(repo_root, &["status", "--porcelain"]);
    assert!(
        porcelain.trim().is_empty(),
        "git status --porcelain must be empty, got {porcelain:?}"
    );

    fs::write(repo_root.join("tracked.txt"), "modified body\n").expect("modify");
    let outcome = stage(&grit_repo, &StageOptions::default(), &mut NullProgress).expect("stage");
    assert!(
        outcome.modified >= 1,
        "expected at least one staged modification"
    );

    let git_oid = git(repo_root, &["hash-object", "-w", "tracked.txt"])
        .trim()
        .to_string();
    let index = grit_repo.load_index().expect("index");
    let entry = index
        .entries
        .iter()
        .find(|e| e.path == b"tracked.txt" && e.stage() == 0)
        .expect("index entry");
    assert_eq!(
        entry.oid.to_hex(),
        git_oid,
        "staged blob oid must match git hash-object"
    );

    fs::write(repo_root.join("second.txt"), "another\n").expect("write second");
    git(repo_root, &["add", "second.txt"]);
    git(repo_root, &["commit", "-qm", "second"]);
    let commit2 = git(repo_root, &["rev-parse", "HEAD"]).trim().to_string();

    let commit_oid: grit_lib::objects::ObjectId = commit2.parse().expect("parse commit oid");
    let grit_patch = compute_patch_id(HashAlgo::Sha256, &grit_repo.odb, &commit_oid)
        .expect("patch-id")
        .expect("non-merge patch-id")
        .to_hex();
    let git_patch = git_patch_id_stable(repo_root, &commit2);
    assert_eq!(
        grit_patch, git_patch,
        "patch-id must match git patch-id --stable"
    );

    git(repo_root, &["fsck"]);

    // Repack without -d so redundant loose objects remain, then prune via grit.
    git(repo_root, &["repack", "-a"]);
    let objects_dir = repo_root.join(".git/objects");
    let loose_before: Vec<_> = fs::read_dir(objects_dir.join(&git_oid[..2]))
        .expect("loose prefix dir")
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .collect();
    assert!(
        !loose_before.is_empty(),
        "expected redundant loose objects after repack -a"
    );

    let removed = prune_packed_objects(
        &objects_dir,
        PrunePackedOptions {
            dry_run: false,
            quiet: true,
        },
    )
    .expect("prune");
    assert!(
        !removed.is_empty(),
        "prune-packed should remove packed loose SHA-256 objects"
    );

    git(repo_root, &["fsck"]);
}

#[test]
fn sha256_rerere_conflict_id_uses_configured_hash() {
    if !git_sha256_supported() {
        eprintln!("SKIP: git lacks --object-format=sha256");
        return;
    }

    let content = "<<<<<<< HEAD\nours\n=======\ntheirs\n>>>>>>> branch\n";
    let id = rerere_conflict_id(HashAlgo::Sha256, content, 7).expect("parse conflict");
    assert_eq!(id.algo(), HashAlgo::Sha256);
    assert_eq!(id.as_bytes().len(), 32);

    let sha1_id = rerere_conflict_id(HashAlgo::Sha1, content, 7).expect("sha1 conflict");
    assert_eq!(sha1_id.as_bytes().len(), 20);
    assert_ne!(id, sha1_id);
}

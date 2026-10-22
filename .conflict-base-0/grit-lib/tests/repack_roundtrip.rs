//! Repack/prune maintenance API round-trips against system `git`.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::SystemTime;

use filetime::FileTime;
use grit_lib::config::environment::RepositoryOptions;
use grit_lib::error::Error;
use grit_lib::index::Index;
use grit_lib::maintenance::{repack, repack_all, RepackOptions};
use grit_lib::objects::odb::store::MemoryStore;
use grit_lib::objects::odb::OdbBuilder;
use grit_lib::objects::{ObjectId, ObjectKind};
use grit_lib::porcelain::add::stage_for_commit;
use grit_lib::porcelain::commit::{create_commit, CommitRequest, HookPolicy};
use grit_lib::progress::NullProgress;
use grit_lib::repo::{InitOptions, Repository, RepositoryBuilder};
use grit_test_support::git_fsck;
use std::sync::Arc;
use tempfile::TempDir;
use time::OffsetDateTime;

fn git(dir: &Path, args: &[&str]) {
    let out = Command::new("git")
        .current_dir(dir)
        .args(args)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .env("GIT_AUTHOR_NAME", "t")
        .env("GIT_AUTHOR_EMAIL", "t@example.com")
        .env("GIT_COMMITTER_NAME", "t")
        .env("GIT_COMMITTER_EMAIL", "t@example.com")
        .output()
        .expect("git");
    assert!(
        out.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

fn git_rev_list_objects_all(dir: &Path) -> String {
    let out = Command::new("git")
        .current_dir(dir)
        .args(["rev-list", "--objects", "--all"])
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .output()
        .expect("rev-list");
    assert!(out.status.success());
    String::from_utf8_lossy(&out.stdout).into_owned()
}

fn many_commits(repo: &Repository, n: usize) {
    let wt = repo.work_tree().expect("worktree");
    let ident = "Test <t@example.com> 1 +0000".to_string();
    for i in 0..n {
        std::fs::write(wt.join(format!("f{i}.txt")), format!("body-{i}\n")).expect("write");
        stage_for_commit(repo, &mut NullProgress).expect("stage");
        create_commit(
            repo,
            &CommitRequest {
                message: format!("c{i}"),
                author: ident.clone(),
                committer: ident.clone(),
                allow_empty: false,
                sign_override: None,
                amend: false,
                assume_worktree_synced: false,
                skip_pre_commit_tree_match: false,
                staged_change_count: None,
                ..Default::default()
            },
            &mut NullProgress,
        )
        .expect("commit");
    }
}

#[test]
fn repack_all_into_one_git_fsck_strict_clean() {
    let dir = TempDir::new().expect("tempdir");
    let repo = Repository::init(dir.path(), &InitOptions::default()).expect("init");
    many_commits(&repo, 25);
    repack_all(&repo, &RepackOptions::default()).expect("repack");
    let packs = std::fs::read_dir(dir.path().join(".git/objects/pack"))
        .expect("pack dir")
        .filter_map(|e| e.ok())
        .filter(|e| e.path().extension().is_some_and(|x| x == "pack"))
        .count();
    assert_eq!(packs, 1);
    let outcome = git_fsck(dir.path(), true);
    assert!(outcome.msg_ids.is_empty(), "{:?}", outcome.msg_ids);
}

#[test]
fn repack_preserves_rev_list_objects_all() {
    let dir = TempDir::new().expect("tempdir");
    let repo = Repository::init(dir.path(), &InitOptions::default()).expect("init");
    many_commits(&repo, 15);
    let before = git_rev_list_objects_all(dir.path());
    repack_all(&repo, &RepackOptions::default()).expect("repack");
    let after = git_rev_list_objects_all(dir.path());
    assert_eq!(before, after);
}

#[test]
fn repack_keeps_reflog_index_and_worktree_head_objects() {
    let dir = TempDir::new().expect("tempdir");
    git(dir.path(), &["init", "-b", "main"]);
    git(dir.path(), &["commit", "--allow-empty", "-m", "one"]);
    git(dir.path(), &["commit", "--allow-empty", "-m", "two"]);
    git(dir.path(), &["reset", "--hard", "HEAD~1"]);

    let repo = Repository::discover(Some(dir.path()), &RepositoryOptions::empty()).expect("open");
    let reflog_only: ObjectId = {
        let out = Command::new("git")
            .current_dir(dir.path())
            .args(["rev-parse", "HEAD@{1}"])
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_SYSTEM", "/dev/null")
            .output()
            .expect("rev-parse");
        ObjectId::from_hex(String::from_utf8_lossy(&out.stdout).trim()).expect("oid")
    };

    let wt = repo.work_tree().expect("wt");
    std::fs::write(wt.join("staged-only.txt"), b"staged\n").expect("write");
    stage_for_commit(&repo, &mut NullProgress).expect("stage");
    let index = Index::load(&repo.git_dir().join("index")).expect("index");
    let staged_oid = index
        .entries()
        .iter()
        .find(|e| e.path.ends_with(b"staged-only.txt"))
        .expect("entry")
        .oid;

    repack_all(&repo, &RepackOptions::default()).expect("repack");
    assert!(repo.odb().read(&reflog_only).is_ok());
    assert!(repo.odb().read(&staged_oid).is_ok());
}

#[test]
fn keep_packs_untouched() {
    let dir = TempDir::new().expect("tempdir");
    let repo = Repository::init(dir.path(), &InitOptions::default()).expect("init");
    many_commits(&repo, 3);
    git(dir.path(), &["repack", "-a", "-d"]);
    let pack_dir = dir.path().join(".git/objects/pack");
    let kept = std::fs::read_dir(&pack_dir)
        .expect("pack dir")
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .find(|p| p.extension().is_some_and(|x| x == "pack"))
        .expect("pack");
    std::fs::write(kept.with_extension("keep"), b"hold").expect("keep");

    repack_all(&repo, &RepackOptions::default()).expect("repack");
    assert!(kept.is_file(), "pack with .keep must remain");
}

#[test]
fn bitmap_written_and_git_rev_list_use_bitmap_index_count_matches() {
    let dir = TempDir::new().expect("tempdir");
    let repo = Repository::init(dir.path(), &InitOptions::default()).expect("init");
    many_commits(&repo, 40);
    let mut opts = RepackOptions::default();
    opts.write_bitmap = true;
    let report = repack(&repo, &opts).expect("repack bitmap");
    assert_eq!(report.packs_written, 1);
    let pack_dir = dir.path().join(".git/objects/pack");
    let has_bitmap = std::fs::read_dir(&pack_dir)
        .expect("pack dir")
        .filter_map(|e| e.ok())
        .any(|e| e.path().extension().is_some_and(|x| x == "bitmap"));
    assert!(has_bitmap, "expected .bitmap sidecar");

    let count_plain = Command::new("git")
        .current_dir(dir.path())
        .args(["rev-list", "--count", "HEAD"])
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .output()
        .expect("count");
    let count_bitmap = Command::new("git")
        .current_dir(dir.path())
        .args(["rev-list", "--use-bitmap-index", "--count", "HEAD"])
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .output()
        .expect("count bitmap");
    assert!(count_plain.status.success() && count_bitmap.status.success());
    assert_eq!(count_plain.stdout, count_bitmap.stdout);
}

#[test]
fn git_verify_pack_accepts_output() {
    let dir = TempDir::new().expect("tempdir");
    let repo = Repository::init(dir.path(), &InitOptions::default()).expect("init");
    many_commits(&repo, 10);
    repack_all(&repo, &RepackOptions::default()).expect("repack");
    let pack = std::fs::read_dir(dir.path().join(".git/objects/pack"))
        .expect("pack dir")
        .filter_map(|e| e.ok())
        .find(|e| e.path().extension().is_some_and(|x| x == "pack"))
        .expect("pack")
        .path();
    git(dir.path(), &["verify-pack", "-v", &pack.to_string_lossy()]);
}

#[test]
fn repack_git_written_repo() {
    let dir = TempDir::new().expect("tempdir");
    git(dir.path(), &["init", "-b", "main"]);
    for i in 0..8 {
        std::fs::write(dir.path().join(format!("g{i}.txt")), format!("x{i}\n")).expect("write");
        git(dir.path(), &["add", &format!("g{i}.txt")]);
        git(dir.path(), &["commit", "-m", &format!("g{i}")]);
    }
    git(dir.path(), &["repack", "-d"]);
    git(dir.path(), &["multi-pack-index", "write"]);

    let repo = Repository::discover(Some(dir.path()), &RepositoryOptions::empty()).expect("open");
    repack_all(&repo, &RepackOptions::default()).expect("repack");
    let packs = std::fs::read_dir(dir.path().join(".git/objects/pack"))
        .expect("pack dir")
        .filter_map(|e| e.ok())
        .filter(|e| e.path().extension().is_some_and(|x| x == "pack"))
        .count();
    assert_eq!(packs, 1);
    git(dir.path(), &["fsck", "--strict"]);
}

#[test]
fn rootless_repo_prunes_unreachable_loose_when_pruning_enabled() {
    let dir = TempDir::new().expect("tempdir");
    let repo = Repository::init(dir.path(), &InitOptions::default()).expect("init");

    let orphan = repo
        .odb()
        .write_loose_materialize(ObjectKind::Blob, b"rootless orphan\n")
        .expect("orphan");

    let mut opts = RepackOptions::default();
    opts.keep_unreachable = false;
    let report = repack(&repo, &opts).expect("repack");
    assert_eq!(report.packs_written, 0);
    assert!(
        report.loose_removed >= 1,
        "expected unreachable loose prune, got {report:?}"
    );
    assert!(
        repo.odb().read(&orphan).is_err(),
        "unreachable loose blob must be removed when keep_unreachable is false"
    );
}

#[test]
fn keep_unreachable_preserves_packed_unreachable_like_git_repack_ad() {
    let dir = TempDir::new().expect("tempdir");
    git(dir.path(), &["init", "-b", "main"]);
    git(dir.path(), &["commit", "--allow-empty", "-m", "main"]);
    git(dir.path(), &["checkout", "-b", "side"]);
    std::fs::write(dir.path().join("side.txt"), b"side-only\n").expect("write");
    git(dir.path(), &["add", "side.txt"]);
    git(dir.path(), &["commit", "-m", "side commit"]);
    let out = Command::new("git")
        .current_dir(dir.path())
        .args(["rev-parse", "HEAD"])
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .output()
        .expect("rev-parse");
    assert!(out.status.success());
    let orphan_hex = String::from_utf8_lossy(&out.stdout).trim().to_string();
    let orphan = ObjectId::from_hex(&orphan_hex).expect("oid");

    git(dir.path(), &["checkout", "main"]);
    git(dir.path(), &["repack", "-ad"]);

    git(dir.path(), &["branch", "-D", "side"]);
    git(dir.path(), &["reflog", "expire", "--expire=now", "--all"]);

    let repo = Repository::discover(Some(dir.path()), &RepositoryOptions::empty()).expect("open");
    assert!(
        repo.odb().read(&orphan).is_ok(),
        "orphan commit must still exist in pack before repack"
    );

    let git_only = TempDir::new().expect("git-only tempdir");
    copy_repo_tree(dir.path(), git_only.path()).expect("copy repo");

    repack_all(&repo, &RepackOptions::default()).expect("grit repack");
    git(git_only.path(), &["repack", "-ad", "--keep-unreachable"]);

    fn cat_file_exists(repo_path: &Path, hex: &str) -> bool {
        Command::new("git")
            .current_dir(repo_path)
            .args(["cat-file", "-e", hex])
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_SYSTEM", "/dev/null")
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false)
    }

    assert!(
        cat_file_exists(dir.path(), &orphan_hex),
        "default grit repack must preserve unreachable objects already in a pack"
    );
    assert!(
        cat_file_exists(git_only.path(), &orphan_hex),
        "system git repack -ad --keep-unreachable must preserve packed unreachable commit"
    );
}

fn copy_repo_tree(src: &Path, dst: &Path) -> std::io::Result<()> {
    copy_dir_recursive(src, dst)
}

fn copy_dir_recursive(src: &Path, dst: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(dst)?;
    for entry in std::fs::read_dir(src)? {
        let entry = entry?;
        let from = entry.path();
        let to = dst.join(entry.file_name());
        if from.is_dir() {
            copy_dir_recursive(&from, &to)?;
        } else {
            std::fs::copy(&from, &to)?;
        }
    }
    Ok(())
}

#[test]
fn default_repack_preserves_unreachable_loose_like_git_repack_ad() {
    let dir = TempDir::new().expect("tempdir");
    git(dir.path(), &["init", "-b", "main"]);
    git(dir.path(), &["commit", "--allow-empty", "-m", "seed"]);

    let out = Command::new("git")
        .current_dir(dir.path())
        .args(["hash-object", "-w", "--stdin"])
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .spawn()
        .expect("spawn hash-object");
    use std::io::Write;
    let mut child = out;
    child
        .stdin
        .take()
        .expect("stdin")
        .write_all(b"orphan payload\n")
        .expect("write stdin");
    let out = child.wait_with_output().expect("hash-object");
    assert!(out.status.success());
    let orphan_hex = String::from_utf8_lossy(&out.stdout).trim().to_string();
    let orphan = ObjectId::from_hex(&orphan_hex).expect("oid");

    let git_only = TempDir::new().expect("git-only tempdir");
    copy_repo_tree(dir.path(), git_only.path()).expect("copy repo");

    let repo = Repository::discover(Some(dir.path()), &RepositoryOptions::empty()).expect("open");
    repack_all(&repo, &RepackOptions::default()).expect("grit repack");
    git(git_only.path(), &["repack", "-ad"]);

    fn cat_file_exists(repo_path: &Path, hex: &str) -> bool {
        Command::new("git")
            .current_dir(repo_path)
            .args(["cat-file", "-e", hex])
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_SYSTEM", "/dev/null")
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false)
    }

    assert!(
        cat_file_exists(dir.path(), &orphan_hex),
        "default grit repack must preserve unreachable loose objects"
    );
    assert!(
        cat_file_exists(git_only.path(), &orphan_hex),
        "system git repack -ad must preserve unreachable loose object"
    );
    assert!(
        repo.odb().read(&orphan).is_ok(),
        "grit odb must still read preserved orphan"
    );
}

#[test]
fn prune_respects_cutoff() {
    let dir = TempDir::new().expect("tempdir");
    let repo = Repository::init(dir.path(), &InitOptions::default()).expect("init");
    many_commits(&repo, 1);

    let orphan = repo
        .odb()
        .write_loose_materialize(ObjectKind::Blob, b"old orphan\n")
        .expect("orphan");
    let orphan_path: PathBuf = repo
        .odb()
        .enumerate_local_loose_objects()
        .expect("loose")
        .into_iter()
        .find(|(oid, _)| *oid == orphan)
        .map(|(_, p)| p)
        .expect("path");
    FileTime::from_unix_time(1_000_000_000, 0);
    filetime::set_file_mtime(&orphan_path, FileTime::from_unix_time(1_000_000_000, 0))
        .expect("mtime");

    let fresh = repo
        .odb()
        .write_loose_materialize(ObjectKind::Blob, b"fresh orphan\n")
        .expect("fresh");
    let fresh_path = repo
        .odb()
        .enumerate_local_loose_objects()
        .expect("loose")
        .into_iter()
        .find(|(oid, _)| *oid == fresh)
        .map(|(_, p)| p)
        .expect("fresh path");
    filetime::set_file_mtime(&fresh_path, FileTime::from_system_time(SystemTime::now()))
        .expect("mtime fresh");

    let cutoff = OffsetDateTime::from_unix_timestamp(1_500_000_000).expect("cutoff");
    let mut opts = RepackOptions::default();
    opts.prune_older_than = Some(cutoff);
    opts.keep_unreachable = false;
    repack(&repo, &opts).expect("repack prune");

    assert!(
        !orphan_path.exists(),
        "orphan older than cutoff should be pruned"
    );
    assert!(
        repo.odb().read(&fresh).is_ok(),
        "recent unreachable loose object must be kept"
    );
}

#[test]
fn custom_backend_unsupported_error() {
    let dir = TempDir::new().expect("tempdir");
    Repository::init(dir.path(), &InitOptions::default()).expect("init");
    let git_dir = dir.path().join(".git");
    let store = Arc::new(MemoryStore::new(grit_lib::objects::HashAlgo::Sha1));
    let custom = RepositoryBuilder::new(RepositoryOptions::empty())
        .odb_builder(
            OdbBuilder::files(git_dir.join("objects"))
                .primary(store)
                .alternates(false),
        )
        .open(&git_dir, Some(dir.path()))
        .expect("open");
    let err = repack(&custom, &RepackOptions::default()).expect_err("custom backend");
    assert!(matches!(
        err,
        Error::UnsupportedObjectStore {
            operation: "repack"
        }
    ));
}

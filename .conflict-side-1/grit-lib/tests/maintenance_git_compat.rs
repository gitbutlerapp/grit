//! Repack/gc compatibility: grit maintenance vs system `git fsck`.

use std::path::{Path, PathBuf};
use std::process::Command;

use filetime::FileTime;
use grit_lib::config::environment::RepositoryOptions;
use grit_lib::error::Error;
use grit_lib::index::Index;
use grit_lib::maintenance::{
    fsck as repo_fsck, gc, prune_loose_objects, repack_all, FsckOptions, GcOptions, PruneOptions,
    RepackOptions,
};
use grit_lib::objects::pack::{clear_pack_cache, test_pack_cache_guard};
use grit_lib::objects::{ObjectId, ObjectKind};
use grit_lib::porcelain::add::stage_for_commit;
use grit_lib::porcelain::commit::{create_commit, CommitRequest};
use grit_lib::progress::NullProgress;
use grit_lib::repo::{InitOptions, Repository};
use grit_test_support::git_fsck;
use tempfile::TempDir;
use time::OffsetDateTime;

fn duplicate_repo_tree(src: &Path, dst: &Path) {
    std::fs::create_dir_all(dst).expect("create dst");
    let status = Command::new("cp")
        .args([
            "-a",
            &format!("{}/.", src.display()),
            &dst.to_string_lossy(),
        ])
        .status()
        .expect("spawn cp");
    assert!(status.success(), "cp -a failed");
}

fn git_commit_dated(dir: &Path, when: &str, message: &str) {
    let out = Command::new("git")
        .current_dir(dir)
        .args(["commit", "--allow-empty", "-m", message])
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .env("GIT_AUTHOR_NAME", "t")
        .env("GIT_AUTHOR_EMAIL", "t@example.com")
        .env("GIT_COMMITTER_NAME", "t")
        .env("GIT_COMMITTER_EMAIL", "t@example.com")
        .env("GIT_AUTHOR_DATE", when)
        .env("GIT_COMMITTER_DATE", when)
        .output()
        .expect("git commit");
    assert!(
        out.status.success(),
        "git commit: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

/// Repack/gc and system `git gc` invalidate on-disk packs; grit fsck reads via the process pack cache.
fn clear_pack_cache_after_disk_maintenance() {
    clear_pack_cache();
}

fn git(dir: &std::path::Path, args: &[&str]) {
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

fn many_grit_commits(repo: &Repository, n: usize) {
    let wt = repo.work_tree().expect("worktree");
    let ident = "Test <t@example.com> 1 +0000".to_string();
    for i in 0..n {
        let path = wt.join(format!("k_{}.txt", i % 9));
        std::fs::write(&path, format!("payload-{i}\n")).expect("write");
        stage_for_commit(repo, &mut NullProgress).expect("stage");
        create_commit(
            repo,
            &CommitRequest {
                message: format!("m {i}"),
                author: ident.clone(),
                committer: ident.clone(),
                allow_empty: false,
                sign_override: None,
                amend: false,
                assume_worktree_synced: false,
            },
            &mut NullProgress,
        )
        .expect("commit");
    }
}

#[test]
fn repack_after_many_commits_single_pack_and_git_fsck_clean() {
    let _pack_guard = test_pack_cache_guard();
    let dir = TempDir::new().expect("tempdir");
    let repo = Repository::init(dir.path(), &InitOptions::default()).expect("init");
    many_grit_commits(&repo, 30);

    let git_dir = dir.path().join(".git");
    let loose_before = repo
        .odb()
        .enumerate_local_loose_objects()
        .expect("loose")
        .len();
    assert!(
        loose_before > 10,
        "expected many loose objects, got {loose_before}"
    );

    let stats = repack_all(&repo, &RepackOptions::default()).expect("repack");
    assert!(stats.pack_path.is_some());
    assert!(stats.objects_packed.unwrap_or(0) > 0);
    clear_pack_cache_after_disk_maintenance();

    let packs = std::fs::read_dir(git_dir.join("objects/pack"))
        .expect("pack dir")
        .filter_map(|e| e.ok())
        .filter(|e| e.path().extension().is_some_and(|x| x == "pack"))
        .count();
    assert_eq!(packs, 1, "expected one pack after repack -a");

    let git_outcome = git_fsck(dir.path(), true);
    assert!(
        git_outcome.msg_ids.is_empty(),
        "git fsck --strict msg-ids: {:?}",
        git_outcome.msg_ids
    );

    let report = repo_fsck(&repo, &FsckOptions::full_repository(true)).expect("grit fsck");
    assert!(report.ok, "{:?}", report.issues);
}

#[test]
fn gc_matches_repack_plus_prune_and_git_fsck() {
    let _pack_guard = test_pack_cache_guard();
    let dir = TempDir::new().expect("tempdir");
    let repo = Repository::init(dir.path(), &InitOptions::default()).expect("init");
    many_grit_commits(&repo, 20);

    let stats = gc(&repo, &GcOptions::with_reflog_policy(1_700_000_000, false)).expect("gc");
    assert!(stats.repack.pack_path.is_some());
    clear_pack_cache_after_disk_maintenance();

    git(dir.path(), &["fsck", "--strict"]);
}

fn count_loose_head_ref_files(git_dir: &Path) -> usize {
    let heads = git_dir.join("refs/heads");
    if !heads.is_dir() {
        return 0;
    }
    std::fs::read_dir(heads)
        .expect("refs/heads")
        .filter_map(|e| e.ok())
        .filter(|e| e.file_type().is_ok_and(|t| t.is_file()))
        .count()
}

#[test]
fn gc_pack_refs_packs_branch_refs() {
    let dir = TempDir::new().expect("tempdir");
    let repo = Repository::init(dir.path(), &InitOptions::default()).expect("init");
    many_grit_commits(&repo, 5);
    git(dir.path(), &["branch", "feature"]);

    let git_dir = dir.path().join(".git");
    let loose_before = count_loose_head_ref_files(&git_dir);
    assert!(
        loose_before >= 2,
        "expected at least main and feature as loose refs/heads, got {loose_before}"
    );

    let stats = gc(&repo, &GcOptions::with_reflog_policy(1_700_000_000, false)).expect("gc");
    let pack = stats.pack_refs.as_ref().expect("gc should run pack-refs");
    assert!(
        pack.refs_packed >= 2,
        "expected branch refs packed, got {}",
        pack.refs_packed
    );
    assert_eq!(
        count_loose_head_ref_files(&git_dir),
        0,
        "expected no loose refs/heads after gc pack-refs --all"
    );

    git(dir.path(), &["fsck", "--strict"]);
}

fn one_commit(repo: &Repository) {
    many_grit_commits(repo, 1);
}

#[test]
fn gc_preserves_staged_only_blob() {
    let _pack_guard = test_pack_cache_guard();
    let dir = TempDir::new().expect("tempdir");
    let repo = Repository::init(dir.path(), &InitOptions::default()).expect("init");
    one_commit(&repo);
    let wt = repo.work_tree().expect("wt");
    std::fs::write(wt.join("staged-only.txt"), b"staged payload\n").expect("write");
    stage_for_commit(&repo, &mut NullProgress).expect("stage");
    let index = Index::load(&repo.git_dir().join("index")).expect("index");
    let staged_oid = index
        .entries()
        .iter()
        .find(|e| e.path.ends_with(b"staged-only.txt"))
        .expect("staged entry")
        .oid;

    gc(&repo, &GcOptions::with_reflog_policy(1_700_000_000, false)).expect("gc");
    clear_pack_cache_after_disk_maintenance();

    assert!(
        repo.odb().read(&staged_oid).is_ok(),
        "gc must not delete index-only blob"
    );
    git_fsck(dir.path(), true);
    let report = repo_fsck(&repo, &FsckOptions::full_repository(true)).expect("fsck");
    assert!(report.ok, "{:?}", report.issues);
}

#[test]
fn fsck_skips_object_checks_when_check_objects_disabled() {
    let dir = TempDir::new().expect("tempdir");
    let repo = Repository::init(dir.path(), &InitOptions::default()).expect("init");
    one_commit(&repo);
    let wt = repo.work_tree().expect("wt");
    std::fs::write(wt.join("orphan-index.txt"), b"x").expect("write");
    stage_for_commit(&repo, &mut NullProgress).expect("stage");
    let index = Index::load(&repo.git_dir().join("index")).expect("index");
    let staged_oid = index
        .entries()
        .iter()
        .find(|e| e.path.ends_with(b"orphan-index.txt"))
        .expect("entry")
        .oid;
    let loose = repo
        .odb()
        .enumerate_local_loose_objects()
        .expect("loose")
        .into_iter()
        .find(|(oid, _)| *oid == staged_oid)
        .expect("loose path");
    std::fs::remove_file(loose.1).expect("remove blob");

    let mut opts = FsckOptions::default();
    opts.strict = false;
    opts.check_objects = false;
    opts.check_refs = false;
    opts.check_midx = false;
    let report = repo_fsck(&repo, &opts).expect("fsck");
    assert!(report.ok, "{:?}", report.issues);
    assert!(
        !report.issues.iter().any(|i| i.msg_id == "missingObject"),
        "{:?}",
        report.issues
    );
}

#[test]
fn fsck_reports_missing_index_blob_after_corruption() {
    let dir = TempDir::new().expect("tempdir");
    let repo = Repository::init(dir.path(), &InitOptions::default()).expect("init");
    one_commit(&repo);
    let wt = repo.work_tree().expect("wt");
    std::fs::write(wt.join("orphan-index.txt"), b"x").expect("write");
    stage_for_commit(&repo, &mut NullProgress).expect("stage");
    let index = Index::load(&repo.git_dir().join("index")).expect("index");
    let staged_oid = index
        .entries()
        .iter()
        .find(|e| e.path.ends_with(b"orphan-index.txt"))
        .expect("entry")
        .oid;
    let loose = repo
        .odb()
        .enumerate_local_loose_objects()
        .expect("loose")
        .into_iter()
        .find(|(oid, _)| *oid == staged_oid)
        .expect("loose path");
    std::fs::remove_file(loose.1).expect("remove blob");

    let report = repo_fsck(&repo, &FsckOptions::objects_and_refs(true)).expect("fsck");
    assert!(
        !report.ok,
        "fsck must not report clean when index references a missing blob"
    );
    assert!(
        report.issues.iter().any(|i| i.msg_id == "missingObject"),
        "{:?}",
        report.issues
    );
}

#[test]
fn repack_keeps_reflog_only_commit_and_kept_pack() {
    let _pack_guard = test_pack_cache_guard();
    let dir = TempDir::new().expect("tempdir");
    let repo = Repository::init(dir.path(), &InitOptions::default()).expect("init");
    many_grit_commits(&repo, 2);
    git(dir.path(), &["reset", "--hard", "HEAD~1"]);

    let reflog_only: ObjectId = {
        let out = Command::new("git")
            .current_dir(dir.path())
            .args(["rev-parse", "HEAD@{1}"])
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_SYSTEM", "/dev/null")
            .output()
            .expect("rev-parse");
        assert!(out.status.success());
        let hex = String::from_utf8_lossy(&out.stdout);
        ObjectId::from_hex(hex.trim()).expect("oid")
    };

    git(dir.path(), &["repack", "-a", "-d"]);
    let git_dir = dir.path().join(".git");
    let pack_dir = git_dir.join("objects/pack");
    let old_pack = std::fs::read_dir(&pack_dir)
        .expect("pack dir")
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .find(|p| p.extension().is_some_and(|x| x == "pack"))
        .expect("pack");
    std::fs::write(old_pack.with_extension("keep"), b"keep").expect("keep");

    repack_all(&repo, &RepackOptions::default()).expect("repack");
    clear_pack_cache_after_disk_maintenance();

    assert!(
        repo.odb().read(&reflog_only).is_ok(),
        "reflog-only commit must remain readable after repack"
    );
    assert!(old_pack.is_file(), "pack with .keep must not be deleted");
    git(dir.path(), &["fsck", "--strict"]);
}

#[test]
fn gc_default_options_uses_wall_clock_for_reflog_expiry() {
    let dir = TempDir::new().expect("tempdir");
    git(dir.path(), &["init", "-b", "main"]);
    git(dir.path(), &["config", "gc.auto", "0"]);
    git_commit_dated(dir.path(), "2000-01-01T00:00:00 +0000", "old");
    git_commit_dated(dir.path(), "2026-01-01T12:00:00 +0000", "new");
    git(dir.path(), &["reset", "--hard", "HEAD~1"]);

    let grit_copy = TempDir::new().expect("grit copy");
    duplicate_repo_tree(dir.path(), grit_copy.path());
    let explicit_copy = TempDir::new().expect("explicit copy");
    duplicate_repo_tree(dir.path(), explicit_copy.path());
    let git_copy = TempDir::new().expect("git copy");
    duplicate_repo_tree(dir.path(), git_copy.path());

    let repo_default =
        Repository::discover(Some(grit_copy.path()), &RepositoryOptions::empty()).expect("open");
    let repo_explicit =
        Repository::discover(Some(explicit_copy.path()), &RepositoryOptions::empty())
            .expect("open");

    let wall = OffsetDateTime::now_utc().unix_timestamp();
    let default_stats = gc(&repo_default, &GcOptions::default()).expect("default gc");
    let explicit_stats =
        gc(&repo_explicit, &GcOptions::with_reflog_policy(wall, true)).expect("explicit-now gc");

    assert_eq!(
        default_stats.reflog_entries_pruned, explicit_stats.reflog_entries_pruned,
        "GcOptions::default() must normalize now_unix before reflog expiry"
    );
    assert!(
        default_stats.reflog_entries_pruned > 0,
        "expected aged reflog entries to be pruned"
    );

    let grit_reflog = grit_copy.path().join(".git/logs/HEAD");
    let grit_body = std::fs::read_to_string(&grit_reflog).expect("grit HEAD reflog");
    assert!(
        !grit_body.contains("old"),
        "grit gc must expire aged reflog entries with default options"
    );

    let git_gc = Command::new("git")
        .current_dir(git_copy.path())
        .args(["gc", "--quiet"])
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .output()
        .expect("git gc");
    assert!(
        git_gc.status.success(),
        "git gc: {}",
        String::from_utf8_lossy(&git_gc.stderr)
    );
    let git_reflog = git_copy.path().join(".git/logs/HEAD");
    let git_body = std::fs::read_to_string(&git_reflog).expect("git HEAD reflog");
    assert!(
        !git_body.contains("old"),
        "system git gc must expire the same aged reflog entries"
    );
    assert!(
        default_stats.reflog_entries_pruned >= 1,
        "grit must report pruned reflog entries when git gc also expires them"
    );
}

#[test]
fn gc_invalid_prune_expire_rejects_before_deleting_loose() {
    let dir = TempDir::new().expect("tempdir");
    git(dir.path(), &["init", "-b", "main"]);
    git(dir.path(), &["config", "gc.auto", "0"]);
    git_commit_dated(dir.path(), "2000-01-01T00:00:00 +0000", "old");
    git_commit_dated(dir.path(), "2026-01-01T12:00:00 +0000", "new");
    git(dir.path(), &["reset", "--hard", "HEAD~1"]);

    let repo =
        Repository::discover(Some(dir.path()), &RepositoryOptions::empty()).expect("open repo");

    let orphan = repo
        .odb()
        .write_loose_materialize(ObjectKind::Blob, b"unreachable orphan payload\n")
        .expect("write loose");
    let loose_path: PathBuf = repo
        .odb()
        .enumerate_local_loose_objects()
        .expect("loose")
        .into_iter()
        .find(|(oid, _)| *oid == orphan)
        .map(|(_, path)| path)
        .expect("orphan loose path");
    filetime::set_file_mtime(&loose_path, FileTime::from_unix_time(978_307_200, 0))
        .expect("mtime 2001");

    git(dir.path(), &["config", "gc.pruneExpire", "bogus-value"]);

    let head_reflog = dir.path().join(".git/logs/HEAD");
    let reflog_before = std::fs::read(&head_reflog).expect("read HEAD reflog");
    assert_eq!(
        std::str::from_utf8(&reflog_before)
            .expect("utf8 reflog")
            .lines()
            .filter(|l| !l.is_empty())
            .count(),
        3,
        "fixture must start with a three-entry HEAD reflog"
    );

    let err = gc(&repo, &GcOptions::default())
        .expect_err("invalid gc.pruneExpire must fail before mutating");
    assert!(
        matches!(err, Error::Config(_)),
        "expected config error, got {err:?}"
    );
    let reflog_after = std::fs::read(&head_reflog).expect("read HEAD reflog after failed gc");
    assert_eq!(
        reflog_before, reflog_after,
        "failed grit gc must not mutate HEAD reflog"
    );
    assert!(
        repo.odb().read(&orphan).is_ok(),
        "invalid config must not delete unreachable loose objects"
    );

    let git_reflog_before = reflog_before.clone();
    let git_gc = Command::new("git")
        .current_dir(dir.path())
        .args(["gc", "--quiet"])
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .output()
        .expect("git gc");
    assert!(
        !git_gc.status.success(),
        "system git gc must reject bogus gc.pruneExpire"
    );
    assert_eq!(git_gc.status.code(), Some(128));
    let git_reflog_after = std::fs::read(&head_reflog).expect("read HEAD reflog after git gc");
    assert_eq!(
        git_reflog_before, git_reflog_after,
        "system git gc must not mutate HEAD reflog on invalid gc.pruneExpire"
    );
    let out = Command::new("git")
        .current_dir(dir.path())
        .args(["cat-file", "-e", &orphan.to_hex()])
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .output()
        .expect("cat-file");
    assert!(
        out.status.success(),
        "system git gc must preserve orphan when config is invalid"
    );
}

fn pack_stems(git_dir: &Path) -> Vec<String> {
    std::fs::read_dir(git_dir.join("objects/pack"))
        .expect("pack dir")
        .filter_map(|e| e.ok())
        .filter(|e| e.path().extension().is_some_and(|x| x == "pack"))
        .filter_map(|e| {
            e.path()
                .file_stem()
                .and_then(|s| s.to_str())
                .map(str::to_owned)
        })
        .collect()
}

fn cat_file_exists(repo_root: &Path, oid_hex: &str) -> bool {
    Command::new("git")
        .current_dir(repo_root)
        .args(["cat-file", "-e", oid_hex])
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .status()
        .expect("cat-file")
        .success()
}

struct GraceProtectedPackedFixture {
    _dir: TempDir,
    side_hex: String,
    side_oid: ObjectId,
    old_pack_stem: String,
}

fn build_grace_protected_packed_unreachable_fixture(
    stale_loose_duplicate: bool,
) -> GraceProtectedPackedFixture {
    let dir = TempDir::new().expect("tempdir");
    git(dir.path(), &["init", "-b", "main"]);
    git(dir.path(), &["config", "gc.auto", "0"]);
    git_commit_dated(dir.path(), "2020-01-01T00:00:00 +0000", "main seed");
    git(dir.path(), &["checkout", "-b", "side"]);
    git_commit_dated(dir.path(), "2026-10-10T12:00:00 +0000", "side tip");
    let side_tip = Command::new("git")
        .current_dir(dir.path())
        .args(["rev-parse", "HEAD"])
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .output()
        .expect("rev-parse");
    assert!(side_tip.status.success());
    let side_hex = String::from_utf8_lossy(&side_tip.stdout).trim().to_string();
    let side_oid = ObjectId::from_hex(&side_hex).expect("oid");
    git(dir.path(), &["checkout", "main"]);
    git(dir.path(), &["repack", "-ad"]);

    if stale_loose_duplicate {
        let repo =
            Repository::discover(Some(dir.path()), &RepositoryOptions::empty()).expect("open repo");
        let obj = repo.odb().read(&side_oid).expect("read side commit");
        repo.odb()
            .write_loose_materialize(obj.kind, &obj.data)
            .expect("loose dup");
        let loose_path = repo.odb().object_path(&side_oid);
        filetime::set_file_mtime(&loose_path, FileTime::from_unix_time(946_684_800, 0))
            .expect("stale loose mtime");
    }

    git(dir.path(), &["branch", "-D", "side"]);
    git(
        dir.path(),
        &[
            "reflog",
            "expire",
            "--expire=now",
            "--expire-unreachable=now",
            "--all",
        ],
    );

    let git_dir = dir.path().join(".git");
    let pre_gc_packs = pack_stems(&git_dir);
    assert_eq!(pre_gc_packs.len(), 1, "fixture must start with one pack");

    GraceProtectedPackedFixture {
        _dir: dir,
        side_hex,
        side_oid,
        old_pack_stem: pre_gc_packs[0].clone(),
    }
}

fn assert_grace_protected_packed_unreachable_gc_matches_git(fixture: &GraceProtectedPackedFixture) {
    let grit_fixture = TempDir::new().expect("grit fixture");
    let git_fixture = TempDir::new().expect("git fixture");
    duplicate_repo_tree(fixture._dir.path(), grit_fixture.path());
    duplicate_repo_tree(fixture._dir.path(), git_fixture.path());

    let grit_repo = Repository::discover(Some(grit_fixture.path()), &RepositoryOptions::empty())
        .expect("open grit repo");
    let now_unix = 1_791_979_200_i64;
    let stats = gc(&grit_repo, &GcOptions::with_reflog_policy(now_unix, false)).expect("grit gc");
    assert!(
        stats.repack.removed_pack_files >= 1,
        "grit gc repack must remove the superseded pack"
    );
    assert!(
        cat_file_exists(grit_fixture.path(), &fixture.side_hex),
        "grit gc must retain recent unreachable commit under default gc.pruneExpire"
    );
    let grit_packs = pack_stems(&grit_fixture.path().join(".git"));
    assert!(
        !grit_packs.iter().any(|s| s == &fixture.old_pack_stem),
        "superseded pack must be removed after grit gc"
    );
    assert!(
        grit_repo.odb().object_path(&fixture.side_oid).is_file(),
        "retained oid must be materialized as loose after grit gc, not only in removed pack"
    );

    git(
        git_fixture.path(),
        &["gc", "--prune=2.weeks.ago", "--quiet"],
    );
    assert!(
        cat_file_exists(git_fixture.path(), &fixture.side_hex),
        "system git gc --prune=2.weeks.ago must retain recent unreachable packed commit"
    );
    let git_packs = pack_stems(&git_fixture.path().join(".git"));
    assert!(
        !git_packs.iter().any(|s| s == &fixture.old_pack_stem),
        "superseded pack must be removed after system git gc"
    );
}

#[test]
fn gc_preserves_grace_protected_packed_unreachable_like_git() {
    let fixture = build_grace_protected_packed_unreachable_fixture(false);
    assert_grace_protected_packed_unreachable_gc_matches_git(&fixture);
}

#[test]
fn gc_preserves_grace_protected_packed_unreachable_with_stale_loose_dup_like_git() {
    let fixture = build_grace_protected_packed_unreachable_fixture(true);
    assert_grace_protected_packed_unreachable_gc_matches_git(&fixture);
}

#[test]
#[cfg(unix)]
fn gc_aborts_before_pack_removal_when_stale_loose_mtime_update_fails() {
    let fixture = build_grace_protected_packed_unreachable_fixture(true);
    let grit_fixture = TempDir::new().expect("grit fixture");
    duplicate_repo_tree(fixture._dir.path(), grit_fixture.path());

    let grit_repo = Repository::discover(Some(grit_fixture.path()), &RepositoryOptions::empty())
        .expect("open grit repo");
    let loose_path = grit_repo.odb().object_path(&fixture.side_oid);
    assert!(loose_path.is_file(), "stale loose duplicate must exist");

    let chattr_plus = Command::new("sudo")
        .args(["chattr", "+i", loose_path.to_str().expect("utf8 path")])
        .status()
        .expect("sudo chattr");
    assert!(
        chattr_plus.success(),
        "sudo chattr +i must succeed so mtime update fails on immutable loose object"
    );

    let packs_before = pack_stems(&grit_fixture.path().join(".git"));
    assert_eq!(packs_before.len(), 1);

    let err = gc(
        &grit_repo,
        &GcOptions::with_reflog_policy(1_791_979_200, false),
    )
    .expect_err("gc must fail when grace mtime cannot be applied to stale loose copy");
    assert!(
        matches!(err, Error::Io(_)),
        "expected I/O error from mtime update, got {err:?}"
    );

    let packs_after = pack_stems(&grit_fixture.path().join(".git"));
    assert!(
        packs_after.iter().any(|s| s == &fixture.old_pack_stem),
        "superseded pack must remain when loosen aborts (packs after: {packs_after:?})"
    );
    assert!(
        cat_file_exists(grit_fixture.path(), &fixture.side_hex),
        "unreachable commit must remain readable from the original pack"
    );

    let chattr_minus = Command::new("sudo")
        .args(["chattr", "-i", loose_path.to_str().expect("utf8 path")])
        .status()
        .expect("sudo chattr cleanup");
    assert!(chattr_minus.success(), "sudo chattr -i cleanup");
}

#[test]
fn gc_reports_kept_unreachable_loose_within_prune_grace() {
    let dir = TempDir::new().expect("tempdir");
    let repo = Repository::init(dir.path(), &InitOptions::default()).expect("init");
    one_commit(&repo);

    repo.odb()
        .write_loose_materialize(ObjectKind::Blob, b"grace-protected orphan\n")
        .expect("write unreachable loose blob");

    let stats = gc(&repo, &GcOptions::with_reflog_policy(1_700_000_000, false)).expect("grit gc");
    assert_eq!(
        stats.prune.pruned, 0,
        "recent orphan must survive grace window"
    );
    assert_eq!(
        stats.prune.kept, 1,
        "full gc must report kept count from unreachable loose prune"
    );
    assert!(stats.prune_ran);

    let prune_only = prune_loose_objects(&repo, &PruneOptions::default()).expect("prune");
    assert_eq!(prune_only.kept, stats.prune.kept);
}

#[test]
fn gc_prune_expire_never_preserves_unreachable_loose_blob() {
    let _pack_guard = test_pack_cache_guard();
    let dir = TempDir::new().expect("tempdir");
    let repo = Repository::init(dir.path(), &InitOptions::default()).expect("init");
    one_commit(&repo);

    let orphan = repo
        .odb()
        .write_loose_materialize(ObjectKind::Blob, b"unreachable orphan payload\n")
        .expect("write loose");

    git(dir.path(), &["config", "gc.pruneExpire", "never"]);

    let stats = gc(&repo, &GcOptions::with_reflog_policy(1_700_000_000, false)).expect("grit gc");
    assert_eq!(
        stats.prune.pruned, 0,
        "gc.pruneExpire=never must disable loose prune"
    );
    assert!(
        repo.odb().read(&orphan).is_ok(),
        "unreachable loose blob must remain after grit gc"
    );

    git(dir.path(), &["gc"]);
    let out = Command::new("git")
        .current_dir(dir.path())
        .args(["cat-file", "-e", &orphan.to_hex()])
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .output()
        .expect("cat-file");
    assert!(
        out.status.success(),
        "system git gc with pruneExpire=never must preserve orphan blob"
    );
}

fn git_loose_object_count(dir: &Path) -> usize {
    let out = Command::new("git")
        .current_dir(dir)
        .args(["count-objects"])
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .output()
        .expect("count-objects");
    assert!(out.status.success());
    let text = String::from_utf8_lossy(&out.stdout);
    text.lines()
        .next()
        .and_then(|l| l.split_whitespace().next())
        .and_then(|n| n.parse().ok())
        .unwrap_or(0)
}

fn git_pack_count(git_dir: &Path) -> usize {
    std::fs::read_dir(git_dir.join("objects/pack"))
        .ok()
        .into_iter()
        .flatten()
        .filter_map(|e| e.ok())
        .filter(|e| e.path().extension().is_some_and(|x| x == "pack"))
        .count()
}

fn repo_with_one_pack_and_loose_commits(loose_commits: usize) -> TempDir {
    let dir = TempDir::new().expect("tempdir");
    git(dir.path(), &["init", "-b", "main"]);
    std::fs::write(dir.path().join("f"), b"x").expect("file");
    git(dir.path(), &["add", "f"]);
    git(dir.path(), &["commit", "-m", "base"]);
    git(dir.path(), &["repack", "-a", "-d"]);
    for i in 0..loose_commits {
        git(
            dir.path(),
            &["commit", "--allow-empty", "-m", &format!("loose-{i}")],
        );
    }
    dir
}

#[test]
fn gc_packs_loose_objects_like_git() {
    let seed = repo_with_one_pack_and_loose_commits(3);
    assert_eq!(
        git_loose_object_count(seed.path()),
        3,
        "fixture must start with three loose objects"
    );

    let grit_dir = TempDir::new().expect("grit copy");
    duplicate_repo_tree(seed.path(), grit_dir.path());
    let git_dir = TempDir::new().expect("git copy");
    duplicate_repo_tree(seed.path(), git_dir.path());

    let repo = Repository::discover(Some(grit_dir.path()), &RepositoryOptions::empty())
        .expect("open grit copy");
    gc(&repo, &GcOptions::with_reflog_policy(1_700_000_000, false)).expect("grit gc");

    git(git_dir.path(), &["gc", "--quiet"]);

    assert_eq!(
        git_loose_object_count(grit_dir.path()),
        git_loose_object_count(git_dir.path()),
        "grit gc must pack loose objects like git gc"
    );
    assert_eq!(git_loose_object_count(grit_dir.path()), 0);
    git_fsck(grit_dir.path(), true);
}

#[test]
fn repack_consolidates_multiple_packs_like_git_repack_ad() {
    let dir = TempDir::new().expect("tempdir");
    git(dir.path(), &["init", "-b", "main"]);
    git(dir.path(), &["commit", "--allow-empty", "-m", "c1"]);
    git(dir.path(), &["repack", "-a", "-d"]);
    git(dir.path(), &["commit", "--allow-empty", "-m", "c2"]);
    git(dir.path(), &["repack", "-d"]);
    let git_dir = dir.path().join(".git");
    assert_eq!(
        git_pack_count(&git_dir),
        2,
        "fixture must have two pack files before consolidation"
    );

    let grit_copy = TempDir::new().expect("grit");
    duplicate_repo_tree(dir.path(), grit_copy.path());
    let git_copy = TempDir::new().expect("git");
    duplicate_repo_tree(dir.path(), git_copy.path());

    let repo =
        Repository::discover(Some(grit_copy.path()), &RepositoryOptions::empty()).expect("open");
    repack_all(&repo, &RepackOptions::default()).expect("grit repack");
    git(git_copy.path(), &["repack", "-a", "-d"]);

    assert_eq!(
        git_pack_count(&grit_copy.path().join(".git")),
        git_pack_count(&git_copy.path().join(".git")),
        "grit repack must consolidate packs like git repack -ad"
    );
    assert_eq!(git_pack_count(&grit_copy.path().join(".git")), 1);
}

#[test]
fn gc_autopacklimit_consolidates_packs_like_git() {
    let dir = TempDir::new().expect("tempdir");
    git(dir.path(), &["init", "-b", "main"]);
    git(dir.path(), &["config", "gc.autoPackLimit", "1"]);
    git(dir.path(), &["commit", "--allow-empty", "-m", "c1"]);
    git(dir.path(), &["repack", "-a", "-d"]);
    git(dir.path(), &["commit", "--allow-empty", "-m", "c2"]);
    git(dir.path(), &["repack", "-d"]);
    let git_dir = dir.path().join(".git");
    assert_eq!(git_pack_count(&git_dir), 2);

    let grit_copy = TempDir::new().expect("grit");
    duplicate_repo_tree(dir.path(), grit_copy.path());
    let git_copy = TempDir::new().expect("git");
    duplicate_repo_tree(dir.path(), git_copy.path());

    let repo =
        Repository::discover(Some(grit_copy.path()), &RepositoryOptions::empty()).expect("open");
    gc(&repo, &GcOptions::with_reflog_policy(1_700_000_000, false)).expect("grit gc");
    git(git_copy.path(), &["gc", "--quiet"]);

    assert_eq!(
        git_pack_count(&grit_copy.path().join(".git")),
        git_pack_count(&git_copy.path().join(".git")),
        "grit gc must consolidate packs when autopacklimit exceeded, like git gc"
    );
    assert_eq!(git_pack_count(&grit_copy.path().join(".git")), 1);
}

#[test]
fn index_gitlink_absent_commit_does_not_break_repack_or_fsck() {
    let _pack_guard = test_pack_cache_guard();
    let dir = TempDir::new().expect("tempdir");
    let repo = Repository::init(dir.path(), &InitOptions::default()).expect("init");
    one_commit(&repo);

    let absent = "aabbccddeeff00112233445566778899aabbccdd";
    git(
        dir.path(),
        &[
            "update-index",
            "--add",
            "--cacheinfo",
            &format!("160000,{absent},submodule"),
        ],
    );

    repack_all(&repo, &RepackOptions::default()).expect("grit repack");
    git(dir.path(), &["gc"]);
    clear_pack_cache_after_disk_maintenance();
    git(dir.path(), &["fsck", "--strict"]);

    let report = repo_fsck(&repo, &FsckOptions::objects_and_refs(true)).expect("grit fsck");
    assert!(
        report.ok,
        "gitlink to absent submodule commit must not fail fsck: {:?}",
        report.issues
    );
}

#[test]
fn incremental_repack_with_bitmap_midx_readable_by_git() {
    let dir = TempDir::new().expect("tempdir");
    let repo = Repository::init(dir.path(), &InitOptions::default()).expect("init");
    let wt = repo.work_tree().expect("worktree");
    std::fs::write(wt.join("base"), b"v1").expect("write base");
    stage_for_commit(&repo, &mut NullProgress).expect("stage");
    let ident = "Test <t@example.com> 1 +0000".to_string();
    create_commit(
        &repo,
        &CommitRequest {
            message: "base".into(),
            author: ident.clone(),
            committer: ident.clone(),
            allow_empty: false,
            sign_override: None,
            amend: false,
            assume_worktree_synced: false,
        },
        &mut NullProgress,
    )
    .expect("commit");
    let mut base_repack = RepackOptions::default();
    base_repack.keep_unreachable = false;
    repack_all(&repo, &base_repack).expect("repack base");

    std::fs::write(wt.join("child"), b"v2").expect("write child");
    stage_for_commit(&repo, &mut NullProgress).expect("stage child");
    create_commit(
        &repo,
        &CommitRequest {
            message: "child".into(),
            author: ident.clone(),
            committer: ident.clone(),
            allow_empty: false,
            sign_override: None,
            amend: false,
            assume_worktree_synced: false,
        },
        &mut NullProgress,
    )
    .expect("child commit");

    let mut incr_repack = RepackOptions::default();
    incr_repack.incremental = true;
    incr_repack.write_bitmap = true;
    repack_all(&repo, &incr_repack).expect("incremental repack with bitmap");

    let git_dir = repo.git_dir();
    git(git_dir, &["multi-pack-index", "verify"]);
    git_fsck(dir.path(), true);

    let rev_list = Command::new("git")
        .current_dir(dir.path())
        .args(["rev-list", "--all", "--objects"])
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .output()
        .expect("git rev-list");
    assert!(
        rev_list.status.success(),
        "git rev-list with MIDX bitmap: {}",
        String::from_utf8_lossy(&rev_list.stderr)
    );
    let listed = String::from_utf8_lossy(&rev_list.stdout);
    assert!(
        listed.lines().count() >= 3,
        "expected commits and tree/blob objects via bitmap-backed MIDX"
    );
}

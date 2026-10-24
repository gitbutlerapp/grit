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
use grit_lib::porcelain::commit::{create_commit, CommitRequest, HookPolicy};
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
    many_grit_commits_from(repo, 0, n);
}

fn many_grit_commits_from(repo: &Repository, start: usize, n: usize) {
    let wt = repo.work_tree().expect("worktree");
    let ident = "Test <t@example.com> 1 +0000".to_string();
    for i in 0..n {
        let idx = start + i;
        let path = wt.join(format!("k_{}.txt", idx % 9));
        std::fs::write(&path, format!("payload-{idx}\n")).expect("write");
        stage_for_commit(repo, &mut NullProgress).expect("stage");
        create_commit(
            repo,
            &CommitRequest {
                message: format!("m {idx}"),
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
    gc(&grit_repo, &GcOptions::with_reflog_policy(now_unix, false)).expect("grit gc");
    clear_pack_cache_after_disk_maintenance();
    assert!(
        cat_file_exists(grit_fixture.path(), &fixture.side_hex),
        "grit gc must retain recent unreachable commit under default gc.pruneExpire"
    );

    git(git_fixture.path(), &["gc", "--quiet"]);
    assert!(
        cat_file_exists(git_fixture.path(), &fixture.side_hex),
        "system git gc must retain recent unreachable packed commit"
    );
    assert_eq!(
        cat_file_exists(grit_fixture.path(), &fixture.side_hex),
        cat_file_exists(git_fixture.path(), &fixture.side_hex),
        "grit gc must preserve grace-protected unreachable objects like git gc"
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

fn git_size_pack_kib(dir: &Path) -> u64 {
    let out = Command::new("git")
        .current_dir(dir)
        .args(["count-objects", "-v"])
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .output()
        .expect("count-objects -v");
    assert!(out.status.success());
    let text = String::from_utf8_lossy(&out.stdout);
    for line in text.lines() {
        if let Some(kib) = line.strip_prefix("size-pack: ") {
            return kib.trim().parse().expect("size-pack");
        }
    }
    0
}

fn git_verify_pack_delta_chain_count(dir: &Path) -> usize {
    let pack_dir = dir.join(".git/objects/pack");
    let pack = std::fs::read_dir(&pack_dir)
        .expect("pack dir")
        .filter_map(|e| e.ok())
        .find(|e| e.path().extension().is_some_and(|ext| ext == "pack"))
        .expect("pack file")
        .path();
    let out = Command::new("git")
        .args(["verify-pack", "-v"])
        .arg(&pack)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .output()
        .expect("verify-pack");
    assert!(
        out.status.success(),
        "verify-pack: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter(|line| line.contains("chain length"))
        .count()
}

fn repo_with_loose_only_edited_blob_history() -> TempDir {
    let dir = TempDir::new().expect("tempdir");
    git(dir.path(), &["init", "-b", "main"]);
    let f = dir.path().join("f.txt");
    let mut lines: Vec<String> = (1..=3000)
        .map(|n| {
            format!("{n} some moderately long line of text to make blob bigger abcdefghijklmnop")
        })
        .collect();
    std::fs::write(&f, format!("{}\n", lines.join("\n"))).expect("write f.txt");
    git(dir.path(), &["add", "f.txt"]);
    git(dir.path(), &["commit", "-m", "c0"]);
    for i in 1..=10 {
        lines[i * 100 - 1] = format!("edit {i}");
        std::fs::write(&f, format!("{}\n", lines.join("\n"))).expect("rewrite f.txt");
        git(dir.path(), &["add", "f.txt"]);
        git(dir.path(), &["commit", "-m", &format!("c{i}")]);
    }
    assert_eq!(
        git_pack_count(&dir.path().join(".git")),
        0,
        "fixture must stay all-loose until maintenance"
    );
    dir
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
        "grit gc must match git gc loose-object counts after repack"
    );
    assert_eq!(git_loose_object_count(grit_dir.path()), 0);
    git_fsck(grit_dir.path(), true);
}

/// Objects stored as deltas in the single pack under `dir`, per `git verify-pack -v`.
fn git_verify_pack_delta_count(dir: &Path) -> usize {
    let pack = std::fs::read_dir(dir.join(".git/objects/pack"))
        .expect("pack dir")
        .filter_map(|e| e.ok())
        .find(|e| e.path().extension().is_some_and(|ext| ext == "pack"))
        .expect("pack file")
        .path();
    let out = Command::new("git")
        .args(["verify-pack", "-v"])
        .arg(&pack)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .output()
        .expect("verify-pack");
    assert!(out.status.success(), "verify-pack failed");
    // Delta rows add `<depth> <base>` to `<oid> <type> <size> <size-in-pack> <offset>`.
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter(|line| line.split_whitespace().count() == 7)
        .count()
}

#[test]
fn gc_over_existing_pack_keeps_its_deltas_and_deltifies_new_blobs() {
    let _pack_guard = test_pack_cache_guard();
    let seed = repo_with_loose_only_edited_blob_history();
    git(seed.path(), &["repack", "-a", "-d"]);
    let packed_deltas = git_verify_pack_delta_count(seed.path());
    assert!(packed_deltas >= 8, "git should deltify the edit history");

    let f = seed.path().join("f.txt");
    for i in 0..3 {
        let mut text = std::fs::read_to_string(&f).expect("read f.txt");
        text.push_str(&format!("appended {i}\n"));
        std::fs::write(&f, text).expect("write f.txt");
        git(seed.path(), &["add", "f.txt"]);
        git(seed.path(), &["commit", "-m", &format!("loose {i}")]);
    }

    let git_copy = TempDir::new().expect("git copy");
    duplicate_repo_tree(seed.path(), git_copy.path());
    git(git_copy.path(), &["gc", "--quiet"]);

    let repo =
        Repository::discover(Some(seed.path()), &RepositoryOptions::empty()).expect("open repo");
    gc(&repo, &GcOptions::with_reflog_policy(1_700_000_000, false)).expect("grit gc");
    clear_pack_cache_after_disk_maintenance();

    assert_eq!(git_pack_count(&seed.path().join(".git")), 1);
    assert_eq!(git_loose_object_count(seed.path()), 0);
    let fsck = git_fsck(seed.path(), true);
    assert!(fsck.ok, "git fsck --strict: {:?}", fsck.msg_ids);
    let deltas = git_verify_pack_delta_count(seed.path());
    assert!(
        deltas >= packed_deltas,
        "grit gc must retain existing pack deltas when repacking with reuse: {deltas} after, {packed_deltas} before"
    );
    let (grit_kib, git_kib) = (
        git_size_pack_kib(seed.path()),
        git_size_pack_kib(git_copy.path()),
    );
    assert!(
        grit_kib <= git_kib.saturating_mul(4).max(git_kib + 24),
        "grit gc pack {grit_kib} KiB vs git gc {git_kib} KiB (rewrite/reuse may store new blobs less aggressively than git gc)"
    );
}

#[test]
fn gc_repacks_loose_after_existing_pack_issue_1320() {
    let _pack_guard = test_pack_cache_guard();
    let dir = TempDir::new().expect("tempdir");
    let repo = Repository::init(dir.path(), &InitOptions::default()).expect("init");
    many_grit_commits(&repo, 5);

    let opts = GcOptions::with_reflog_policy(1_700_000_000, false);
    let first = gc(&repo, &opts).expect("first gc");
    assert!(
        first.repack.objects_packed.unwrap_or(0) > 0,
        "first gc should pack initial loose objects"
    );
    clear_pack_cache_after_disk_maintenance();
    assert_eq!(git_loose_object_count(dir.path()), 0);

    many_grit_commits_from(&repo, 100, 5);
    let loose_before_second = repo
        .odb()
        .enumerate_local_loose_objects()
        .expect("loose")
        .len();
    assert!(
        loose_before_second > 0,
        "second commit batch must leave loose objects before gc"
    );
    assert_eq!(git_pack_count(&dir.path().join(".git")), 1);

    let second = gc(&repo, &opts).expect("second gc");
    assert!(
        second.repack.objects_packed.unwrap_or(0) > 0,
        "second gc must repack loose objects even when a pack already exists (issue #1320)"
    );
    clear_pack_cache_after_disk_maintenance();
    assert_eq!(
        git_loose_object_count(dir.path()),
        0,
        "second gc must remove loose objects like git gc"
    );
    git_fsck(dir.path(), true);
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
fn gc_twice_on_single_pack_keeps_loose_count_zero() {
    let dir = TempDir::new().expect("tempdir");
    git(dir.path(), &["init", "-b", "main"]);
    git(dir.path(), &["commit", "--allow-empty", "-m", "c1"]);
    git(dir.path(), &["repack", "-a", "-d"]);
    let grit_copy = TempDir::new().expect("grit");
    duplicate_repo_tree(dir.path(), grit_copy.path());
    let repo =
        Repository::discover(Some(grit_copy.path()), &RepositoryOptions::empty()).expect("open");
    let opts = GcOptions::with_reflog_policy(1_700_000_000, false);
    gc(&repo, &opts).expect("grit gc first");
    let second = gc(&repo, &opts).expect("grit gc second");
    assert_eq!(
        second.repack.objects_packed, None,
        "second gc on a single-pack repo must skip repack like git gc"
    );
    assert_eq!(git_pack_count(&grit_copy.path().join(".git")), 1);
    assert_eq!(git_loose_object_count(grit_copy.path()), 0);
    git_fsck(grit_copy.path(), true);
}

#[test]
fn explicit_repack_rewrites_single_pack_with_no_loose() {
    let dir = TempDir::new().expect("tempdir");
    git(dir.path(), &["init", "-b", "main"]);
    git(dir.path(), &["commit", "--allow-empty", "-m", "c1"]);
    git(dir.path(), &["repack", "-a", "-d"]);
    let grit_copy = TempDir::new().expect("grit");
    duplicate_repo_tree(dir.path(), grit_copy.path());
    let repo =
        Repository::discover(Some(grit_copy.path()), &RepositoryOptions::empty()).expect("open");
    let stats = repack_all(&repo, &RepackOptions::default()).expect("repack");
    assert!(
        stats.objects_packed.is_some_and(|n| n > 0),
        "explicit repack must rewrite into a new pack even with one pack and no loose objects"
    );
    assert_eq!(git_pack_count(&grit_copy.path().join(".git")), 1);
    git_fsck(grit_copy.path(), true);
}

#[test]
fn gc_drops_expired_unreachable_packed_commit_like_git() {
    let dir = TempDir::new().expect("tempdir");
    git(dir.path(), &["init", "-b", "main"]);
    git(dir.path(), &["config", "gc.auto", "0"]);
    git(dir.path(), &["config", "gc.pruneExpire", "now"]);
    git_commit_dated(dir.path(), "2020-01-01T00:00:00 +0000", "main seed");
    git(dir.path(), &["checkout", "-b", "side"]);
    git_commit_dated(dir.path(), "2020-01-02T00:00:00 +0000", "side tip");
    let side_tip = Command::new("git")
        .current_dir(dir.path())
        .args(["rev-parse", "HEAD"])
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .output()
        .expect("rev-parse");
    assert!(side_tip.status.success());
    let side_hex = String::from_utf8_lossy(&side_tip.stdout).trim().to_string();
    git(dir.path(), &["checkout", "main"]);
    git(dir.path(), &["repack", "-a", "-d"]);
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

    let pack_path = std::fs::read_dir(dir.path().join(".git/objects/pack"))
        .expect("pack dir")
        .filter_map(|e| e.ok())
        .find(|e| e.path().extension().is_some_and(|x| x == "pack"))
        .expect("pack")
        .path();
    filetime::set_file_mtime(&pack_path, FileTime::from_unix_time(946_684_800, 0))
        .expect("age pack");

    let grit_copy = TempDir::new().expect("grit");
    duplicate_repo_tree(dir.path(), grit_copy.path());
    let git_copy = TempDir::new().expect("git");
    duplicate_repo_tree(dir.path(), git_copy.path());

    let repo =
        Repository::discover(Some(grit_copy.path()), &RepositoryOptions::empty()).expect("open");
    gc(&repo, &GcOptions::with_reflog_policy(1_761_000_000, false)).expect("grit gc");
    clear_pack_cache_after_disk_maintenance();
    git(git_copy.path(), &["gc", "--quiet"]);

    assert!(
        !cat_file_exists(grit_copy.path(), &side_hex),
        "grit gc must drop expired unreachable packed commit (issue #1038 review)"
    );
    assert!(
        !cat_file_exists(git_copy.path(), &side_hex),
        "git gc must drop expired unreachable packed commit"
    );
}

#[test]
fn gc_skips_repack_when_single_pack_and_no_loose() {
    let dir = TempDir::new().expect("tempdir");
    git(dir.path(), &["init", "-b", "main"]);
    git(dir.path(), &["commit", "--allow-empty", "-m", "c1"]);
    git(dir.path(), &["repack", "-a", "-d"]);
    let pack_before = std::fs::read_dir(dir.path().join(".git/objects/pack"))
        .expect("pack dir")
        .filter_map(|e| e.ok())
        .find(|e| e.path().extension().is_some_and(|x| x == "pack"))
        .expect("pack")
        .path();

    let repo = Repository::discover(Some(dir.path()), &RepositoryOptions::empty()).expect("open");
    let stats = gc(&repo, &GcOptions::with_reflog_policy(1_700_000_000, false)).expect("grit gc");
    assert!(
        stats.repack.objects_packed.is_none() || stats.repack.objects_packed == Some(0),
        "expected repack skip on single pack with no loose objects"
    );
    let pack_after = std::fs::read_dir(dir.path().join(".git/objects/pack"))
        .expect("pack dir")
        .filter_map(|e| e.ok())
        .find(|e| e.path().extension().is_some_and(|x| x == "pack"))
        .expect("pack")
        .path();
    assert_eq!(pack_before, pack_after, "gc must not rewrite the lone pack");
    git_fsck(dir.path(), true);
}

#[test]
fn gc_with_write_bitmaps_on_single_pack_writes_bitmap_like_git() {
    fn pack_has_bitmap(git_dir: &Path) -> bool {
        std::fs::read_dir(git_dir.join("objects/pack"))
            .expect("pack dir")
            .filter_map(|e| e.ok())
            .any(|e| e.path().extension().is_some_and(|x| x == "bitmap"))
    }
    fn pack_mtime(git_dir: &Path) -> FileTime {
        let pack = std::fs::read_dir(git_dir.join("objects/pack"))
            .expect("pack dir")
            .filter_map(|e| e.ok())
            .find(|e| e.path().extension().is_some_and(|x| x == "pack"))
            .expect("pack file")
            .path();
        FileTime::from_last_modification_time(&pack.metadata().expect("pack meta"))
    }

    let dir = TempDir::new().expect("tempdir");
    git(dir.path(), &["init", "-b", "main"]);
    git(dir.path(), &["commit", "--allow-empty", "-m", "c1"]);
    git(dir.path(), &["repack", "-a", "-d"]);
    git(dir.path(), &["config", "repack.writeBitmaps", "true"]);
    let git_dir = dir.path().join(".git");
    assert_eq!(git_pack_count(&git_dir), 1);
    assert!(
        !pack_has_bitmap(&git_dir),
        "fixture must not already have a bitmap before gc"
    );

    let grit_copy = TempDir::new().expect("grit");
    duplicate_repo_tree(dir.path(), grit_copy.path());
    let git_copy = TempDir::new().expect("git");
    duplicate_repo_tree(dir.path(), git_copy.path());
    let grit_git_dir = grit_copy.path().join(".git");
    let pack_before = pack_mtime(&grit_git_dir);

    let repo =
        Repository::discover(Some(grit_copy.path()), &RepositoryOptions::empty()).expect("open");
    let stats = gc(&repo, &GcOptions::with_reflog_policy(1_700_000_000, false)).expect("grit gc");
    git(git_copy.path(), &["gc", "--quiet"]);

    assert!(
        stats.repack.objects_packed.is_some_and(|n| n > 0),
        "grit gc must repack (not skip) when repack.writeBitmaps is enabled"
    );
    assert!(
        pack_mtime(&grit_git_dir) > pack_before,
        "grit gc must rewrite the pack when generating a bitmap"
    );
    assert!(
        pack_has_bitmap(&grit_git_dir),
        "grit gc with repack.writeBitmaps must write a bitmap even on a single-pack repo"
    );
    assert!(
        pack_has_bitmap(&git_copy.path().join(".git")),
        "git gc fixture must write bitmap"
    );
    git_fsck(grit_copy.path(), true);
}

#[test]
fn full_gc_auto_skips_repack_but_explicit_repack_runs_on_single_pack() {
    let dir = TempDir::new().expect("tempdir");
    git(dir.path(), &["init", "-b", "main"]);
    git(dir.path(), &["commit", "--allow-empty", "-m", "c1"]);
    git(dir.path(), &["repack", "-a", "-d"]);

    let grit_gc = TempDir::new().expect("gc copy");
    duplicate_repo_tree(dir.path(), grit_gc.path());
    let repo_gc =
        Repository::discover(Some(grit_gc.path()), &RepositoryOptions::empty()).expect("open");
    let gc_stats = gc(&repo_gc, &GcOptions::full_run(false, 1_700_000_000)).expect("gc");
    assert!(
        gc_stats.repack.objects_packed.is_none(),
        "full grit gc must auto-skip repack on a single pack with no loose objects"
    );

    let grit_repack = TempDir::new().expect("repack copy");
    duplicate_repo_tree(dir.path(), grit_repack.path());
    let repo_repack =
        Repository::discover(Some(grit_repack.path()), &RepositoryOptions::empty()).expect("open");
    let repack_stats = repack_all(&repo_repack, &RepackOptions::default()).expect("repack");
    assert!(
        repack_stats.objects_packed.is_some_and(|n| n > 0),
        "explicit repack must run even when full gc would skip"
    );
}

#[test]
fn repack_with_existing_pack_and_loose_uses_rewrite_not_full_delta_search() {
    let _pack_guard = test_pack_cache_guard();
    let dir = TempDir::new().expect("tempdir");
    git(dir.path(), &["init", "-b", "main"]);
    git(dir.path(), &["config", "gc.auto", "1"]);
    std::fs::write(dir.path().join("blob"), b"v1\n").expect("write");
    git(dir.path(), &["add", "blob"]);
    git(dir.path(), &["commit", "-m", "c1"]);
    git(dir.path(), &["repack", "-a", "-d"]);
    std::fs::write(dir.path().join("blob"), b"v1\nextra\n").expect("write");
    git(dir.path(), &["add", "blob"]);
    git(dir.path(), &["commit", "-m", "c2"]);
    assert!(
        git_loose_object_count(dir.path()) > 0,
        "fixture must have loose objects after second commit"
    );
    assert_eq!(git_pack_count(&dir.path().join(".git")), 1);

    let grit_copy = TempDir::new().expect("grit");
    duplicate_repo_tree(dir.path(), grit_copy.path());
    let repo =
        Repository::discover(Some(grit_copy.path()), &RepositoryOptions::empty()).expect("open");
    let start = std::time::Instant::now();
    let stats = repack_all(&repo, &RepackOptions::default()).expect("repack");
    let elapsed = start.elapsed();
    assert!(
        stats.objects_packed.is_some_and(|n| n > 0),
        "repack must consolidate loose objects into the pack"
    );
    assert_eq!(git_pack_count(&grit_copy.path().join(".git")), 1);
    assert_eq!(git_loose_object_count(grit_copy.path()), 0);
    assert!(
        elapsed.as_secs() < 30,
        "repack with existing pack + loose must use rewrite/reuse (took {:.2}s)",
        elapsed.as_secs_f64()
    );
    git_fsck(grit_copy.path(), true);
}

/// Reachable packed delta whose base is not in the repack output set must not be copied verbatim.
#[test]
fn repack_materializes_delta_when_base_unreachable() {
    let dir = TempDir::new().expect("tempdir");
    git(dir.path(), &["init", "-b", "main"]);
    let mut payload: Vec<u8> = (0..64 * 1024).map(|i| (i % 251) as u8).collect();
    std::fs::write(dir.path().join("big.bin"), &payload).expect("write");
    git(dir.path(), &["add", "big.bin"]);
    git(dir.path(), &["commit", "-m", "v1"]);
    payload.iter_mut().for_each(|b| *b = b.wrapping_add(1));
    std::fs::write(dir.path().join("big.bin"), &payload).expect("write v2");
    git(dir.path(), &["add", "big.bin"]);
    git(dir.path(), &["commit", "-m", "v2"]);
    git(dir.path(), &["repack", "-a", "-d"]);

    let tree_out = Command::new("git")
        .current_dir(dir.path())
        .args(["rev-parse", "HEAD^{tree}"])
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .output()
        .expect("tree");
    assert!(tree_out.status.success());
    let tree = String::from_utf8_lossy(&tree_out.stdout).trim().to_string();
    let commit_out = Command::new("git")
        .current_dir(dir.path())
        .args(["commit-tree", &tree, "-m", "orphan tip"])
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .output()
        .expect("commit-tree");
    assert!(commit_out.status.success());
    let orphan = String::from_utf8_lossy(&commit_out.stdout)
        .trim()
        .to_string();
    git(dir.path(), &["update-ref", "refs/heads/main", &orphan]);
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
    git(dir.path(), &["repack", "-a", "-d"]);

    let grit_copy = TempDir::new().expect("grit");
    duplicate_repo_tree(dir.path(), grit_copy.path());
    let repo =
        Repository::discover(Some(grit_copy.path()), &RepositoryOptions::empty()).expect("open");
    let mut opts = RepackOptions::default();
    opts.keep_unreachable = false;
    repack_all(&repo, &opts).expect("grit repack");
    clear_pack_cache_after_disk_maintenance();
    git_fsck(grit_copy.path(), true);
}

#[test]
fn gc_full_run_skips_repack_on_single_pack_with_no_loose() {
    let dir = TempDir::new().expect("tempdir");
    git(dir.path(), &["init", "-b", "main"]);
    git(dir.path(), &["commit", "--allow-empty", "-m", "c1"]);
    git(dir.path(), &["repack", "-a", "-d"]);
    let grit_copy = TempDir::new().expect("grit");
    duplicate_repo_tree(dir.path(), grit_copy.path());
    let pack_dir = grit_copy.path().join(".git/objects/pack");
    let pack_mtime_before = std::fs::read_dir(&pack_dir)
        .expect("pack dir")
        .filter_map(|e| e.ok())
        .find(|e| e.path().extension().is_some_and(|x| x == "pack"))
        .and_then(|e| e.metadata().ok().map(|m| m.modified().ok()))
        .flatten()
        .expect("pack mtime");
    let repo =
        Repository::discover(Some(grit_copy.path()), &RepositoryOptions::empty()).expect("open");
    let stats = gc(&repo, &GcOptions::full_run(false, 1_700_000_000)).expect("grit gc");
    assert!(
        stats.repack.objects_packed.is_none(),
        "single-pack gc with auto_skip must not rewrite the pack"
    );
    let pack_mtime_after = std::fs::read_dir(&pack_dir)
        .expect("pack dir")
        .filter_map(|e| e.ok())
        .find(|e| e.path().extension().is_some_and(|x| x == "pack"))
        .and_then(|e| e.metadata().ok().map(|m| m.modified().ok()))
        .flatten()
        .expect("pack mtime");
    assert_eq!(pack_mtime_before, pack_mtime_after);
    git_fsck(grit_copy.path(), true);
}

#[test]
fn gc_on_single_pack_with_no_loose_matches_git_gc_loose_count() {
    let dir = TempDir::new().expect("tempdir");
    git(dir.path(), &["init", "-b", "main"]);
    git(dir.path(), &["commit", "--allow-empty", "-m", "c1"]);
    git(dir.path(), &["repack", "-a", "-d"]);
    let git_dir = dir.path().join(".git");
    assert_eq!(git_pack_count(&git_dir), 1);

    let grit_copy = TempDir::new().expect("grit");
    duplicate_repo_tree(dir.path(), grit_copy.path());
    let git_copy = TempDir::new().expect("git");
    duplicate_repo_tree(dir.path(), git_copy.path());

    let repo =
        Repository::discover(Some(grit_copy.path()), &RepositoryOptions::empty()).expect("open");
    let stats = gc(&repo, &GcOptions::with_reflog_policy(1_700_000_000, false)).expect("grit gc");
    git(git_copy.path(), &["gc", "--quiet"]);

    assert_eq!(
        stats.repack.objects_packed, None,
        "grit gc must skip repack on a single-pack repo with no loose objects"
    );
    assert_eq!(
        git_loose_object_count(grit_copy.path()),
        git_loose_object_count(git_copy.path()),
        "grit gc must match git gc loose-object count on a single-pack repo"
    );
    assert_eq!(git_pack_count(&grit_copy.path().join(".git")), 1);
    git_fsck(grit_copy.path(), true);
}

#[test]
fn gc_autopacklimit_zero_keeps_two_packs_like_git_gc_auto() {
    let dir = TempDir::new().expect("tempdir");
    git(dir.path(), &["init", "-b", "main"]);
    git(dir.path(), &["config", "gc.autoPackLimit", "0"]);
    git(dir.path(), &["commit", "--allow-empty", "-m", "c1"]);
    git(dir.path(), &["repack", "-a", "-d"]);
    git(dir.path(), &["commit", "--allow-empty", "-m", "c2"]);
    git(dir.path(), &["repack", "-d"]);
    let git_dir = dir.path().join(".git");
    assert_eq!(
        git_pack_count(&git_dir),
        2,
        "fixture must have two pack files"
    );

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
        "grit gc must consolidate packs like full git gc regardless of gc.autoPackLimit"
    );
    assert_eq!(git_pack_count(&grit_copy.path().join(".git")), 1);
}

#[test]
fn gc_autopacklimit_ignores_kept_packs_like_git_gc_auto() {
    let dir = TempDir::new().expect("tempdir");
    git(dir.path(), &["init", "-b", "main"]);
    git(dir.path(), &["config", "gc.autoPackLimit", "1"]);
    git(dir.path(), &["commit", "--allow-empty", "-m", "c1"]);
    git(dir.path(), &["repack", "-a", "-d"]);
    git(dir.path(), &["commit", "--allow-empty", "-m", "c2"]);
    git(dir.path(), &["repack", "-d"]);
    let git_dir = dir.path().join(".git");
    let pack_dir = git_dir.join("objects/pack");
    assert_eq!(git_pack_count(&git_dir), 2);

    let kept = std::fs::read_dir(&pack_dir)
        .expect("pack dir")
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .find(|p| p.extension().is_some_and(|x| x == "pack"))
        .expect("pack");
    std::fs::write(kept.with_extension("keep"), b"keep").expect("keep");

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
        "grit gc must respect .keep packs like full git gc"
    );
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
fn gc_from_loose_objects_builds_delta_compressed_pack_like_repack() {
    let _pack_guard = test_pack_cache_guard();
    let seed = repo_with_loose_only_edited_blob_history();

    let grit_gc = TempDir::new().expect("gc copy");
    duplicate_repo_tree(seed.path(), grit_gc.path());
    let grit_repack = TempDir::new().expect("repack copy");
    duplicate_repo_tree(seed.path(), grit_repack.path());

    let repo_gc = Repository::discover(Some(grit_gc.path()), &RepositoryOptions::empty())
        .expect("open gc copy");
    let gc_stats = gc(
        &repo_gc,
        &GcOptions::with_reflog_policy(1_700_000_000, false),
    )
    .expect("grit gc");
    clear_pack_cache_after_disk_maintenance();

    let repo_repack = Repository::discover(Some(grit_repack.path()), &RepositoryOptions::empty())
        .expect("open repack copy");
    let repack_stats = repack_all(&repo_repack, &RepackOptions::default()).expect("grit repack");
    clear_pack_cache_after_disk_maintenance();

    let gc_bytes = gc_stats.repack.pack_bytes.expect("gc pack bytes");
    let repack_bytes = repack_stats.pack_bytes.expect("repack pack bytes");
    assert!(
        gc_bytes <= repack_bytes.saturating_mul(3) / 2,
        "gc pack {gc_bytes} bytes should match repack {repack_bytes} bytes (within 50%)"
    );
    assert!(
        git_verify_pack_delta_chain_count(grit_gc.path()) > 0,
        "gc pack must contain delta chains when built from loose objects"
    );
    assert!(
        git_size_pack_kib(grit_gc.path()) <= git_size_pack_kib(grit_repack.path()) * 2,
        "gc size-pack should stay near repack for the same history"
    );
    git_fsck(grit_gc.path(), true);
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
            skip_pre_commit_tree_match: false,
            staged_change_count: None,
            ..Default::default()
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
            skip_pre_commit_tree_match: false,
            staged_change_count: None,
            ..Default::default()
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

#[test]
fn gc_replaces_readonly_commit_graph_like_git_on_windows() {
    let _pack_guard = test_pack_cache_guard();
    let dir = TempDir::new().expect("tempdir");
    git(dir.path(), &["init", "-b", "main"]);
    git(dir.path(), &["commit", "--allow-empty", "-m", "one"]);
    git(
        dir.path(),
        &["commit-graph", "write", "--reachable", "--changed-paths"],
    );

    let graph_path = dir.path().join(".git/objects/info/commit-graph");
    assert!(graph_path.is_file(), "git must write commit-graph");
    let mut perms = std::fs::metadata(&graph_path).expect("meta").permissions();
    perms.set_readonly(true);
    std::fs::set_permissions(&graph_path, perms).expect("readonly");

    let repo = Repository::discover(Some(dir.path()), &RepositoryOptions::empty()).expect("open");
    gc(&repo, &GcOptions::with_reflog_policy(1_700_000_000, false))
        .expect("grit gc must replace read-only commit-graph");
    git_fsck(dir.path(), true);
}

fn missing_oid() -> ObjectId {
    ObjectId::from_hex(&"1".repeat(40)).expect("oid")
}

#[test]
fn prune_rejects_missing_root_with_empty_loose_store() {
    let dir = TempDir::new().expect("tempdir");
    let repo = Repository::init(dir.path(), &InitOptions::default()).expect("init");
    std::fs::write(
        repo.git_dir().join("refs/heads/main"),
        format!("{}\n", missing_oid().to_hex()),
    )
    .expect("write ref");
    assert!(repo
        .odb()
        .enumerate_local_loose_objects()
        .expect("loose")
        .is_empty());

    let err = prune_loose_objects(
        &repo,
        &PruneOptions::with_older_than(Some(std::time::SystemTime::now())),
    )
    .expect_err("prune must not accept a missing root");
    assert!(matches!(err, Error::ObjectNotFound(_)), "{err:?}");

    let err = grit_lib::refs::gc::prune_loose_unreachable(repo.odb(), &[missing_oid()], None)
        .expect_err("missing root");
    assert!(matches!(err, Error::ObjectNotFound(_)), "{err:?}");

    gc(&repo, &GcOptions::with_reflog_policy(1_700_000_000, false))
        .expect_err("full gc must not accept a missing root");
}

#[test]
fn prune_rejects_missing_reachable_blob() {
    let dir = TempDir::new().expect("tempdir");
    git(dir.path(), &["init", "-b", "main"]);
    std::fs::write(dir.path().join("f.txt"), b"blob that goes missing\n").expect("write");
    git(dir.path(), &["add", "f.txt"]);
    git(dir.path(), &["commit", "-m", "c"]);
    let repo =
        Repository::discover(Some(dir.path()), &RepositoryOptions::empty()).expect("open repo");
    let blob = grit_lib::objects::hash::hash_object(
        grit_lib::objects::HashAlgo::Sha1,
        ObjectKind::Blob,
        b"blob that goes missing\n",
    )
    .expect("blob id");
    let hex = blob.to_hex();
    std::fs::remove_file(repo.odb().objects_dir().join(&hex[..2]).join(&hex[2..]))
        .expect("drop blob");

    let roots = grit_lib::refs::gc::collect_referenced_object_roots(repo.git_dir()).expect("roots");
    let err = grit_lib::refs::gc::prune_loose_unreachable(repo.odb(), &roots, None)
        .expect_err("missing blob");
    assert!(
        matches!(err, Error::ObjectNotFound(ref hex) if *hex == blob.to_hex()),
        "{err:?}"
    );
}

/// Old trees may record directories with non-canonical modes such as `40755`; prune must
/// still walk them (Git classifies entries by their file type bits).
#[test]
fn prune_walks_trees_with_non_canonical_directory_modes() {
    let dir = TempDir::new().expect("tempdir");
    let repo = Repository::init(dir.path(), &InitOptions::default()).expect("init");
    let odb = repo.odb();
    let blob = odb.write(ObjectKind::Blob, b"kept\n").expect("blob");
    let mut sub = b"100644 kept.txt\0".to_vec();
    sub.extend_from_slice(blob.as_bytes());
    let sub = odb.write(ObjectKind::Tree, &sub).expect("subtree");
    let mut root = b"40755 dir\0".to_vec();
    root.extend_from_slice(sub.as_bytes());
    let root = odb.write(ObjectKind::Tree, &root).expect("root tree");
    let commit = format!(
        "tree {root}\nauthor A <a@example.com> 1 +0000\ncommitter A <a@example.com> 1 +0000\n\nm\n"
    );
    let commit = odb
        .write(ObjectKind::Commit, commit.as_bytes())
        .expect("commit");

    let stats = grit_lib::refs::gc::prune_loose_unreachable(odb, &[commit], None).expect("prune");
    assert_eq!(stats.pruned, 0, "{stats:?}");
    assert!(
        odb.exists(&blob),
        "blob under a 40755 directory must survive prune"
    );
}

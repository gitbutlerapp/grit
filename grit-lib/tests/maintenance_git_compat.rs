//! Repack/gc compatibility: grit maintenance vs system `git fsck`.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::process::Command;

use filetime::FileTime;
use grit_lib::config::environment::RepositoryOptions;
use grit_lib::error::Error;
use grit_lib::index::Index;
use grit_lib::maintenance::repository_fsck::{self, issue_msg_id, FsckOptions, FsckReport};
use grit_lib::maintenance::{
    auto_gc_needed, gc, gc_auto, pack_refs, prune_loose_objects, repack_all, AutoGcOutcome,
    AutoGcReason, GcOptions, PackRefsOptions, PruneOptions, RepackOptions,
};
use grit_lib::objects::pack::{clear_pack_cache, test_pack_cache_guard};
use grit_lib::objects::{ObjectId, ObjectKind};
use grit_lib::porcelain::add::stage_for_commit;
use grit_lib::porcelain::commit::{create_commit, CommitRequest};
use grit_lib::progress::{NeverCancel, NullProgress};
use grit_lib::repo::{InitOptions, Repository};
use grit_test_support::git_fsck;
use tempfile::TempDir;
use time::OffsetDateTime;

/// Run grit fsck and report whether it passed under `opts`.
fn repo_fsck(repo: &Repository, opts: &FsckOptions) -> (FsckReport, bool) {
    let report =
        repository_fsck::fsck(repo, opts, &mut NullProgress, &NeverCancel).expect("grit fsck");
    let ok = report.ok(&repo.config().expect("config"), opts);
    (report, ok)
}

/// Object store, connectivity and refs, without the dangling-object scan.
fn objects_and_refs(strict: bool) -> FsckOptions {
    FsckOptions::full_repository(strict).with_check_dangling(false)
}

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
        .envs(NO_AUTO_MAINTENANCE_ENV.iter().copied())
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

/// Keeps system `git` from starting detached `maintenance run --auto` after commits:
/// newer Git repacks geometrically and writes a multi-pack-index in the background,
/// changing the pack set while a test counts or reads it. `gc --auto` and explicit
/// `maintenance run` are unaffected.
const NO_AUTO_MAINTENANCE_ENV: &[(&str, &str)] = &[
    ("GIT_CONFIG_COUNT", "1"),
    ("GIT_CONFIG_KEY_0", "maintenance.auto"),
    ("GIT_CONFIG_VALUE_0", "false"),
];

fn git(dir: &std::path::Path, args: &[&str]) {
    let out = Command::new("git")
        .current_dir(dir)
        .args(args)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .envs(NO_AUTO_MAINTENANCE_ENV.iter().copied())
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
        stage_for_commit(repo, &mut NullProgress, &NeverCancel).expect("stage");
        create_commit(
            repo,
            &CommitRequest {
                message: format!("m {idx}"),
                author: Some(ident.parse().expect("signature")),
                committer: Some(ident.parse().expect("signature")),
                allow_empty: false,
                sign_override: None,
                amend: false,
                assume_worktree_synced: false,
                skip_pre_commit_tree_match: false,
                staged_change_count: None,
                ..Default::default()
            },
            &mut NullProgress,
            &NeverCancel,
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

    let (report, ok) = repo_fsck(&repo, &objects_and_refs(true));
    assert!(ok, "{:?}", report.issues);
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
    assert!(
        git_dir.join("refs/heads").is_dir(),
        "refs/heads/ namespace kept after packing all branches"
    );

    git(dir.path(), &["fsck", "--strict"]);
}

/// One commit on `main`, a loose `refs/stash`, and the empty `refs/tags` that init creates:
/// packing every ref empties the whole `refs/` tree.
fn repo_with_loose_stash_ref() -> (TempDir, Repository) {
    let dir = TempDir::new().expect("tempdir");
    let repo = Repository::init(dir.path(), &InitOptions::default()).expect("init");
    one_commit(&repo);
    let head = grit_lib::refs::resolve_ref(repo.git_dir(), "refs/heads/main").expect("main");
    grit_lib::refs::write_ref(repo.git_dir(), "refs/stash", &head).expect("refs/stash");
    let refs = repo.git_dir().join("refs");
    assert!(refs.join("stash").is_file());
    assert!(refs.join("tags").is_dir());
    (dir, repo)
}

fn git_stdout(dir: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .current_dir(dir)
        .args(args)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .output()
        .expect("git");
    assert!(
        out.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

/// `git` only recognizes a `.git` directory that has a `refs/` directory.
fn assert_packed_stash_repo_readable_by_git(dir: &Path, repo: &Repository) {
    let git_dir = repo.git_dir();
    assert!(
        git_dir.join("refs").is_dir(),
        "packing refs must keep .git/refs (issue #2191)"
    );
    assert!(!git_dir.join("refs/stash").exists());
    let packed = std::fs::read_to_string(git_dir.join("packed-refs")).expect("packed-refs");
    assert!(packed.contains(" refs/heads/main\n"), "{packed}");
    assert!(packed.contains(" refs/stash\n"), "{packed}");

    assert_eq!(git_stdout(dir, &["rev-parse", "--git-dir"]), ".git");
    let head = grit_lib::refs::resolve_ref(git_dir, "refs/heads/main").expect("main");
    assert_eq!(git_stdout(dir, &["rev-parse", "refs/stash"]), head.to_hex());
    git(dir, &["fsck", "--strict"]);
}

#[test]
fn pack_refs_keeps_refs_dir_after_packing_stash_issue_2191() {
    let (dir, repo) = repo_with_loose_stash_ref();
    let report = pack_refs(&repo, &PackRefsOptions::with_all(true)).expect("pack-refs");
    assert_eq!(report.refs_pruned, 2, "{report:?}");
    assert_packed_stash_repo_readable_by_git(dir.path(), &repo);
}

#[test]
fn gc_keeps_refs_dir_after_packing_stash_issue_2191() {
    let (dir, repo) = repo_with_loose_stash_ref();
    gc(&repo, &GcOptions::with_reflog_policy(1_700_000_000, false)).expect("gc");
    assert_packed_stash_repo_readable_by_git(dir.path(), &repo);
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
    stage_for_commit(&repo, &mut NullProgress, &NeverCancel).expect("stage");
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
    let (report, ok) = repo_fsck(&repo, &objects_and_refs(true));
    assert!(ok, "{:?}", report.issues);
}

#[test]
fn fsck_skips_object_checks_when_check_objects_disabled() {
    let dir = TempDir::new().expect("tempdir");
    let repo = Repository::init(dir.path(), &InitOptions::default()).expect("init");
    one_commit(&repo);
    let wt = repo.work_tree().expect("wt");
    std::fs::write(wt.join("orphan-index.txt"), b"x").expect("write");
    stage_for_commit(&repo, &mut NullProgress, &NeverCancel).expect("stage");
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

    let mut opts = FsckOptions::connectivity_and_refs(false);
    opts.check_connectivity = false;
    let (report, ok) = repo_fsck(&repo, &opts);
    assert!(ok, "{:?}", report.issues);
    assert!(
        !report
            .issues
            .iter()
            .any(|i| issue_msg_id(i) == "missingObject"),
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
    stage_for_commit(&repo, &mut NullProgress, &NeverCancel).expect("stage");
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

    let (report, ok) = repo_fsck(&repo, &objects_and_refs(true));
    assert!(
        !ok,
        "fsck must not report clean when index references a missing blob"
    );
    assert!(
        report
            .issues
            .iter()
            .any(|i| issue_msg_id(i) == "missingObject"),
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
        .current_dir(dir)
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
        .current_dir(dir)
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
        deltas > packed_deltas,
        "pack deltas must be reused and new blobs deltified: {deltas} after, {packed_deltas} before"
    );
    let (grit_kib, git_kib) = (
        git_size_pack_kib(seed.path()),
        git_size_pack_kib(git_copy.path()),
    );
    assert!(
        grit_kib <= git_kib * 2,
        "grit gc pack {grit_kib} KiB vs git gc {git_kib} KiB"
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
    assert_eq!(
        stats.repack.objects_packed, None,
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
    let dir = repo_with_loose_only_edited_blob_history();
    git(dir.path(), &["repack", "-a", "-d"]);

    let pack_path = std::fs::read_dir(dir.path().join(".git/objects/pack"))
        .expect("pack dir")
        .filter_map(|e| e.ok())
        .find(|e| e.path().extension().is_some_and(|x| x == "pack"))
        .expect("pack")
        .path();
    let verify_before = Command::new("git")
        .current_dir(dir.path())
        .args(["verify-pack", "-v"])
        .arg(&pack_path)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .output()
        .expect("verify-pack");
    assert!(verify_before.status.success());
    let verify_text = String::from_utf8_lossy(&verify_before.stdout);

    let mut blob_oid = None;
    for line in verify_text.lines() {
        let fields: Vec<_> = line.split_whitespace().collect();
        if fields.len() != 7 || fields[1] != "blob" {
            continue;
        }
        let target = fields[0];
        let base = fields[6];
        let commits = Command::new("git")
            .current_dir(dir.path())
            .args(["rev-list", "--all"])
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_SYSTEM", "/dev/null")
            .output()
            .expect("rev-list all");
        if !commits.status.success() {
            continue;
        }
        let mut tree = String::new();
        for commit in String::from_utf8_lossy(&commits.stdout).lines() {
            let commit = commit.trim();
            if commit.is_empty() {
                continue;
            }
            let ls = Command::new("git")
                .current_dir(dir.path())
                .args(["ls-tree", "-r", commit])
                .env("GIT_CONFIG_GLOBAL", "/dev/null")
                .env("GIT_CONFIG_SYSTEM", "/dev/null")
                .output()
                .expect("ls-tree");
            if !ls.status.success() {
                continue;
            }
            if String::from_utf8_lossy(&ls.stdout).contains(target) {
                let tree_out = Command::new("git")
                    .current_dir(dir.path())
                    .args(["rev-parse", &format!("{commit}^{{tree}}")])
                    .env("GIT_CONFIG_GLOBAL", "/dev/null")
                    .env("GIT_CONFIG_SYSTEM", "/dev/null")
                    .output()
                    .expect("tree");
                if tree_out.status.success() {
                    tree = String::from_utf8_lossy(&tree_out.stdout).trim().to_string();
                    break;
                }
            }
        }
        if tree.is_empty() {
            continue;
        }
        let orphan_tip = Command::new("git")
            .current_dir(dir.path())
            .args(["commit-tree", &tree, "-m", "orphan delta tip"])
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_SYSTEM", "/dev/null")
            .output()
            .expect("commit-tree");
        assert!(orphan_tip.status.success());
        let orphan_ref = String::from_utf8_lossy(&orphan_tip.stdout)
            .trim()
            .to_string();
        git(dir.path(), &["update-ref", "refs/heads/main", &orphan_ref]);
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
        let reachable = Command::new("git")
            .current_dir(dir.path())
            .args(["rev-list", "--objects", "HEAD"])
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_SYSTEM", "/dev/null")
            .output()
            .expect("rev-list");
        assert!(reachable.status.success());
        let reachable_now: HashSet<String> = String::from_utf8_lossy(&reachable.stdout)
            .lines()
            .filter_map(|line| line.split_whitespace().next())
            .map(str::to_string)
            .collect();
        if reachable_now.contains(target) && !reachable_now.contains(base) {
            blob_oid = Some(target.to_string());
            break;
        }
    }
    let blob_oid = blob_oid.unwrap_or_else(|| {
        panic!("no reachable delta with unreachable base in pack:\n{verify_text}")
    });

    let grit_copy = TempDir::new().expect("grit");
    duplicate_repo_tree(dir.path(), grit_copy.path());
    let expected_payload = Command::new("git")
        .current_dir(grit_copy.path())
        .args(["cat-file", "blob", &blob_oid])
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .output()
        .expect("cat-file before")
        .stdout;
    let repo =
        Repository::discover(Some(grit_copy.path()), &RepositoryOptions::empty()).expect("open");
    let mut opts = RepackOptions::default();
    opts.keep_unreachable = false;
    repack_all(&repo, &opts).expect("grit repack");
    clear_pack_cache_after_disk_maintenance();
    let cat = Command::new("git")
        .current_dir(grit_copy.path())
        .args(["cat-file", "blob", &blob_oid])
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .output()
        .expect("cat-file");
    assert!(
        cat.status.success(),
        "blob must remain readable after repack"
    );
    assert_eq!(cat.stdout, expected_payload, "blob payload must round-trip");
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

    let (report, ok) = repo_fsck(&repo, &objects_and_refs(true));
    assert!(
        ok,
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
    stage_for_commit(&repo, &mut NullProgress, &NeverCancel).expect("stage");
    let ident = "Test <t@example.com> 1 +0000".to_string();
    create_commit(
        &repo,
        &CommitRequest {
            message: "base".into(),
            author: Some(ident.parse().expect("signature")),
            committer: Some(ident.parse().expect("signature")),
            allow_empty: false,
            sign_override: None,
            amend: false,
            assume_worktree_synced: false,
            skip_pre_commit_tree_match: false,
            staged_change_count: None,
            ..Default::default()
        },
        &mut NullProgress,
        &NeverCancel,
    )
    .expect("commit");
    let mut base_repack = RepackOptions::default();
    base_repack.keep_unreachable = false;
    repack_all(&repo, &base_repack).expect("repack base");

    std::fs::write(wt.join("child"), b"v2").expect("write child");
    stage_for_commit(&repo, &mut NullProgress, &NeverCancel).expect("stage child");
    create_commit(
        &repo,
        &CommitRequest {
            message: "child".into(),
            author: Some(ident.parse().expect("signature")),
            committer: Some(ident.parse().expect("signature")),
            allow_empty: false,
            sign_override: None,
            amend: false,
            assume_worktree_synced: false,
            skip_pre_commit_tree_match: false,
            staged_change_count: None,
            ..Default::default()
        },
        &mut NullProgress,
        &NeverCancel,
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

/// `refs/heads/main` names a commit whose tree does not exist, stored in a pack with no
/// loose objects left. Any reachability walk fails on the missing tree, so an operation
/// that succeeds here did not walk the object graph.
fn packed_repo_whose_tip_tree_is_missing() -> (TempDir, Repository, ObjectId) {
    let dir = TempDir::new().expect("tempdir");
    git(dir.path(), &["init", "-b", "main"]);
    let repo = Repository::discover(Some(dir.path()), &RepositoryOptions::empty()).expect("open");
    let commit = format!(
        "tree {}\nauthor A <a@example.com> 1 +0000\ncommitter A <a@example.com> 1 +0000\n\nm\n",
        missing_oid().to_hex()
    );
    let commit = repo
        .odb()
        .write(ObjectKind::Commit, commit.as_bytes())
        .expect("commit");
    std::fs::write(
        repo.git_dir().join("refs/heads/main"),
        format!("{}\n", commit.to_hex()),
    )
    .expect("ref");

    let mut pack_objects = Command::new("git")
        .current_dir(dir.path())
        .args(["pack-objects", "-q", ".git/objects/pack/pack"])
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::null())
        .spawn()
        .expect("spawn git pack-objects");
    {
        use std::io::Write as _;
        let mut stdin = pack_objects.stdin.take().expect("stdin");
        writeln!(stdin, "{}", commit.to_hex()).expect("write oid");
    }
    assert!(pack_objects.wait().expect("pack-objects").success());
    git(dir.path(), &["prune-packed"]);
    clear_pack_cache_after_disk_maintenance();
    assert!(repo
        .odb()
        .enumerate_local_loose_objects()
        .expect("loose")
        .is_empty());
    (dir, repo, commit)
}

/// Like `git prune`, nothing is walked when there is no loose object to consider.
#[test]
fn prune_with_no_loose_objects_skips_reachability_walk() {
    let _pack_guard = test_pack_cache_guard();
    let (_dir, repo, commit) = packed_repo_whose_tip_tree_is_missing();

    let stats = prune_loose_objects(
        &repo,
        &PruneOptions::with_older_than(Some(std::time::SystemTime::now())),
    )
    .expect("prune with nothing loose must not walk the graph");
    assert_eq!(stats, grit_lib::refs::gc::PruneStats::default());

    let stats = grit_lib::refs::gc::prune_loose_unreachable(repo.odb(), &[commit], None)
        .expect("prune with nothing loose must not walk the graph");
    assert_eq!(stats, grit_lib::refs::gc::PruneStats::default());
}

/// Loose objects inside the grace window are kept whether reachable or not, so they do
/// not need the walk either.
#[test]
fn prune_skips_reachability_walk_when_all_loose_objects_are_recent() {
    let _pack_guard = test_pack_cache_guard();
    let (_dir, repo, commit) = packed_repo_whose_tip_tree_is_missing();
    repo.odb()
        .write(ObjectKind::Blob, b"recent unreachable\n")
        .expect("blob");
    let cutoff = std::time::SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(1);

    let stats = grit_lib::refs::gc::prune_loose_unreachable(repo.odb(), &[commit], Some(cutoff))
        .expect("recent loose objects must not trigger the walk");
    assert_eq!(stats.pruned, 0, "{stats:?}");
    assert_eq!(stats.kept, 1, "{stats:?}");
}

/// gc on an already packed repository with a fresh pack and nothing loose skips the repack
/// and the prune walk, and still reports the prune as done.
#[test]
fn gc_on_single_fresh_pack_skips_reachability_walk() {
    let _pack_guard = test_pack_cache_guard();
    let (_dir, repo, _commit) = packed_repo_whose_tip_tree_is_missing();
    let now = OffsetDateTime::now_utc().unix_timestamp();

    let stats = gc(&repo, &GcOptions::with_reflog_policy(now, false))
        .expect("gc of a packed repository must not walk the graph");
    assert_eq!(stats.repack.objects_packed, None, "{:?}", stats.repack);
    assert!(stats.prune_ran, "{stats:?}");
    assert_eq!(stats.prune, grit_lib::refs::gc::PruneStats::default());
}

fn top_commit_graph_has_bloom_filters(repo: &Repository) -> bool {
    grit_lib::revwalk::commit_graph::CommitGraphChain::try_load(repo.odb().objects_dir())
        .expect("load commit-graph")
        .expect("commit-graph written")
        .top_layer_bloom_settings()
        .is_some()
}

/// Like `git gc`, the commit-graph keeps changed-path Bloom filters when the existing graph
/// has them and does not compute them from scratch otherwise.
#[test]
fn gc_commit_graph_keeps_existing_changed_path_choice_like_git() {
    let _pack_guard = test_pack_cache_guard();
    let dir = TempDir::new().expect("tempdir");
    git(dir.path(), &["init", "-b", "main"]);
    std::fs::write(dir.path().join("a.txt"), b"one\n").expect("write");
    git(dir.path(), &["add", "a.txt"]);
    git(dir.path(), &["commit", "-m", "one"]);
    let repo = Repository::discover(Some(dir.path()), &RepositoryOptions::empty()).expect("open");

    gc(&repo, &GcOptions::with_reflog_policy(1_700_000_000, false)).expect("first gc");
    assert!(
        !top_commit_graph_has_bloom_filters(&repo),
        "gc must not add Bloom filters to a repository without them"
    );
    git(dir.path(), &["commit-graph", "verify"]);

    git(
        dir.path(),
        &["commit-graph", "write", "--reachable", "--changed-paths"],
    );
    std::fs::write(dir.path().join("a.txt"), b"two\n").expect("write");
    git(dir.path(), &["commit", "-am", "two"]);
    gc(&repo, &GcOptions::with_reflog_policy(1_700_000_000, false)).expect("second gc");
    assert!(
        top_commit_graph_has_bloom_filters(&repo),
        "gc must keep the Bloom filters of an existing commit-graph"
    );
    git(dir.path(), &["commit-graph", "verify"]);
    git_fsck(dir.path(), true);
}

fn loose_in_fanout(git_dir: &Path, fanout: &str) -> usize {
    std::fs::read_dir(git_dir.join("objects").join(fanout))
        .map(|entries| entries.filter_map(|e| e.ok()).count())
        .unwrap_or(0)
}

fn now_unix() -> i64 {
    OffsetDateTime::now_utc().unix_timestamp()
}

#[test]
fn gc_auto_packs_loose_objects_past_gc_auto_and_git_reads_the_result() {
    let _pack_guard = test_pack_cache_guard();
    let dir = TempDir::new().expect("tempdir");
    git(dir.path(), &["init", "-b", "main"]);
    std::fs::write(dir.path().join("base.txt"), b"base\n").expect("write");
    git(dir.path(), &["add", "base.txt"]);
    git(dir.path(), &["commit", "-m", "base"]);
    git(dir.path(), &["repack", "-a", "-d"]);
    // ceil(20 / 256) = 1, so two loose objects in objects/17 trip the threshold.
    git(dir.path(), &["config", "gc.auto", "20"]);
    let git_dir = dir.path().join(".git");
    let repo = Repository::discover(Some(dir.path()), &RepositoryOptions::empty()).expect("open");
    let opts = GcOptions::full_run(false, now_unix());

    let mut commits = 0;
    while loose_in_fanout(&git_dir, "17") < 2 {
        assert_eq!(
            gc_auto(&repo, &opts).expect("gc_auto below threshold"),
            AutoGcOutcome::NotNeeded,
            "a sample at or below the threshold must not run maintenance"
        );
        many_grit_commits_from(&repo, commits, 1);
        commits += 1;
        assert!(commits < 5000, "objects/17 never filled");
    }
    let loose_before = git_loose_object_count(dir.path());
    assert!(loose_before > 20);

    let outcome = gc_auto(&repo, &opts).expect("gc_auto");
    let AutoGcOutcome::Ran { reason, stats } = outcome else {
        panic!("expected maintenance to run, got {outcome:?}");
    };
    assert!(matches!(
        reason,
        AutoGcReason::TooManyLoose { limit: 20, .. }
    ));
    assert_eq!(stats.repack.removed_pack_files, 0);
    clear_pack_cache_after_disk_maintenance();

    assert_eq!(
        git_loose_object_count(dir.path()),
        0,
        "every loose object must be packed"
    );
    assert_eq!(
        git_pack_count(&git_dir),
        2,
        "loose objects go into a new pack next to the existing one"
    );
    assert!(!git_dir.join("gc.pid").exists(), "gc.pid must be released");
    git_fsck(dir.path(), true);
    git(dir.path(), &["log", "--oneline", "-n", "3"]);
    let (_, ok) = repo_fsck(&repo, &objects_and_refs(true));
    assert!(ok, "grit fsck after gc_auto");

    assert_eq!(auto_gc_needed(&repo).expect("recheck"), None);
    assert_eq!(
        gc_auto(&repo, &opts).expect("second gc_auto"),
        AutoGcOutcome::NotNeeded
    );
}

/// Deterministic pseudo-random bytes (xorshift64), incompressible like a binary asset.
fn pseudo_random_bytes(state: &mut u64, len: usize) -> Vec<u8> {
    (0..len)
        .map(|_| {
            *state ^= *state << 13;
            *state ^= *state >> 7;
            *state ^= *state << 17;
            (*state >> 24) as u8
        })
        .collect()
}

#[test]
fn gc_auto_delta_compresses_loose_versions_of_a_large_blob_issue_2158() {
    const ASSET_LEN: usize = 256 * 1024;
    const VERSIONS: usize = 8;
    let _pack_guard = test_pack_cache_guard();
    let dir = TempDir::new().expect("tempdir");
    git(dir.path(), &["init", "-b", "main"]);
    std::fs::write(dir.path().join("base.txt"), b"base\n").expect("write");
    git(dir.path(), &["add", "base.txt"]);
    git(dir.path(), &["commit", "-m", "base"]);
    git(dir.path(), &["repack", "-a", "-d"]);
    let git_dir = dir.path().join(".git");
    let pack_dir = git_dir.join("objects/pack");
    let base_packs = pack_stems(&git_dir);
    assert_eq!(base_packs.len(), 1);

    let mut rng = 0x2158_u64;
    let mut asset = pseudo_random_bytes(&mut rng, ASSET_LEN);
    for v in 0..VERSIONS {
        for _ in 0..20 {
            let at = (rng as usize) % (ASSET_LEN - 100);
            let patch = pseudo_random_bytes(&mut rng, 100);
            asset[at..at + 100].copy_from_slice(&patch);
        }
        std::fs::write(dir.path().join("asset.bin"), &asset).expect("write asset");
        git(dir.path(), &["add", "asset.bin"]);
        git(dir.path(), &["commit", "-m", &format!("asset v{v}")]);
    }
    let repo = Repository::discover(Some(dir.path()), &RepositoryOptions::empty()).expect("open");
    let mut commits = 0;
    while loose_in_fanout(&git_dir, "17") < 2 {
        many_grit_commits_from(&repo, commits, 1);
        commits += 1;
        assert!(commits < 5000, "objects/17 never filled");
    }
    git(dir.path(), &["config", "gc.auto", "1"]);

    let repack_copy = TempDir::new().expect("repack copy");
    duplicate_repo_tree(dir.path(), repack_copy.path());

    let repo = Repository::discover(Some(dir.path()), &RepositoryOptions::empty()).expect("open");
    let outcome = gc_auto(&repo, &GcOptions::full_run(false, now_unix())).expect("gc_auto");
    let AutoGcOutcome::Ran { reason, stats } = outcome else {
        panic!("expected maintenance to run, got {outcome:?}");
    };
    assert!(matches!(reason, AutoGcReason::TooManyLoose { .. }));
    assert_eq!(stats.repack.removed_pack_files, 0);
    clear_pack_cache_after_disk_maintenance();

    let stems = pack_stems(&git_dir);
    assert_eq!(stems.len(), 2, "the loose objects go into a second pack");
    assert!(
        stems.contains(&base_packs[0]),
        "the existing pack must be kept"
    );
    assert_eq!(git_loose_object_count(dir.path()), 0);
    let new_pack = pack_dir.join(format!(
        "{}.pack",
        stems
            .iter()
            .find(|s| **s != base_packs[0])
            .expect("new pack")
    ));
    let auto_bytes = std::fs::metadata(&new_pack).expect("new pack").len();
    assert!(
        auto_bytes < (3 * ASSET_LEN) as u64,
        "auto-gc pack is {auto_bytes} bytes for {VERSIONS} versions of a {ASSET_LEN}-byte blob"
    );

    let repack_repo = Repository::discover(Some(repack_copy.path()), &RepositoryOptions::empty())
        .expect("open repack copy");
    let repack_stats = repack_all(&repack_repo, &RepackOptions::default()).expect("repack");
    clear_pack_cache_after_disk_maintenance();
    let repack_bytes = repack_stats.pack_bytes.expect("repack pack bytes");
    assert!(
        auto_bytes <= repack_bytes.saturating_mul(3) / 2,
        "auto-gc pack {auto_bytes} bytes vs repack {repack_bytes} bytes"
    );

    let verify = grit_test_support::git_verify_pack(dir.path(), &new_pack);
    assert!(verify.ok, "verify-pack: {}", verify.output);
    let deltas = verify
        .output
        .lines()
        .filter(|line| line.split_whitespace().count() == 7)
        .count();
    assert!(
        deltas >= VERSIONS - 1,
        "every later asset version should be a delta, got {deltas} deltas"
    );

    let check_dir = TempDir::new().expect("index-pack out");
    let index_pack = Command::new("git")
        .current_dir(dir.path())
        .arg("index-pack")
        .arg("--strict")
        .arg("-o")
        .arg(check_dir.path().join("check.idx"))
        .arg(&new_pack)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .output()
        .expect("index-pack");
    assert!(
        index_pack.status.success(),
        "git index-pack --strict: {}",
        String::from_utf8_lossy(&index_pack.stderr)
    );
    let fsck = git_fsck(dir.path(), true);
    assert!(fsck.ok, "git fsck --strict: {:?}", fsck.msg_ids);
}

#[test]
fn gc_auto_consolidates_packs_past_gc_auto_pack_limit_like_git() {
    let _pack_guard = test_pack_cache_guard();
    let dir = TempDir::new().expect("tempdir");
    git(dir.path(), &["init", "-b", "main"]);
    for i in 0..3 {
        std::fs::write(dir.path().join("f.txt"), format!("v{i}\n")).expect("write");
        git(dir.path(), &["add", "f.txt"]);
        git(dir.path(), &["commit", "-m", &format!("c{i}")]);
        git(dir.path(), &["repack", "-d"]);
    }
    let git_dir = dir.path().join(".git");
    assert_eq!(git_pack_count(&git_dir), 3);
    git(dir.path(), &["config", "gc.autoPackLimit", "2"]);

    let git_copy = TempDir::new().expect("git copy");
    duplicate_repo_tree(dir.path(), git_copy.path());

    let repo = Repository::discover(Some(dir.path()), &RepositoryOptions::empty()).expect("open");
    assert_eq!(
        auto_gc_needed(&repo).expect("check"),
        Some(AutoGcReason::TooManyPacks { count: 3, limit: 2 })
    );
    let outcome = gc_auto(&repo, &GcOptions::full_run(false, now_unix())).expect("gc_auto");
    assert!(
        matches!(outcome, AutoGcOutcome::Ran { .. }),
        "expected maintenance to run, got {outcome:?}"
    );
    clear_pack_cache_after_disk_maintenance();
    git(
        git_copy.path(),
        &["-c", "gc.autoDetach=false", "gc", "--auto", "--quiet"],
    );

    assert_eq!(git_pack_count(&git_dir), 1);
    assert_eq!(
        git_pack_count(&git_dir),
        git_pack_count(&git_copy.path().join(".git")),
        "grit must consolidate packs when git gc --auto does"
    );
    git_fsck(dir.path(), true);
    assert_eq!(auto_gc_needed(&repo).expect("recheck"), None);
}

#[test]
fn gc_auto_is_a_no_op_when_gc_auto_is_zero() {
    let dir = TempDir::new().expect("tempdir");
    git(dir.path(), &["init", "-b", "main"]);
    git(dir.path(), &["config", "gc.auto", "0"]);
    let repo = Repository::discover(Some(dir.path()), &RepositoryOptions::empty()).expect("open");
    many_grit_commits(&repo, 600);
    let loose_before = git_loose_object_count(dir.path());
    assert!(loose_before >= 1800);
    assert_eq!(
        gc_auto(&repo, &GcOptions::full_run(false, now_unix())).expect("gc_auto"),
        AutoGcOutcome::NotNeeded
    );
    assert_eq!(git_loose_object_count(dir.path()), loose_before);
    assert_eq!(git_pack_count(&dir.path().join(".git")), 0);
}

fn git_count_objects_field(dir: &Path, field: &str) -> usize {
    let out = Command::new("git")
        .current_dir(dir)
        .args(["count-objects", "-v"])
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .output()
        .expect("count-objects -v");
    assert!(out.status.success());
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .find_map(|l| l.strip_prefix(&format!("{field}: ")))
        .and_then(|n| n.trim().parse().ok())
        .unwrap_or_else(|| panic!("count-objects -v has no {field}"))
}

/// One empty commit (commit + empty tree) plus unreachable loose blobs, with at least two
/// loose objects in `objects/17` so `gc.auto = 1` fires.
///
/// With `pack_reachable`, the commit and tree are packed first, so every loose object is
/// unreachable.
fn repo_with_unreachable_loose_blobs(prune_expire: &str, pack_reachable: bool) -> TempDir {
    let dir = TempDir::new().expect("tempdir");
    git(dir.path(), &["init", "-b", "main"]);
    git(dir.path(), &["commit", "--allow-empty", "-m", "c1"]);
    if pack_reachable {
        git(dir.path(), &["repack", "-a", "-d"]);
    }
    let git_dir = dir.path().join(".git");
    let mut n = 0;
    while loose_in_fanout(&git_dir, "17") < 2 {
        let blob = dir.path().join("blob.tmp");
        std::fs::write(&blob, format!("unreachable {n}\n")).expect("write");
        git(dir.path(), &["hash-object", "-w", "blob.tmp"]);
        n += 1;
        assert!(n < 5000, "objects/17 never filled");
    }
    std::fs::remove_file(dir.path().join("blob.tmp")).expect("rm");
    git(dir.path(), &["config", "gc.auto", "1"]);
    git(dir.path(), &["config", "gc.autoDetach", "false"]);
    git(dir.path(), &["config", "gc.pruneExpire", prune_expire]);
    dir
}

/// Runs grit `gc_auto` and system `git gc --auto` on copies of `seed`; returns the two copies.
fn grit_and_git_gc_auto(seed: &Path) -> (TempDir, TempDir) {
    let grit_copy = TempDir::new().expect("grit copy");
    duplicate_repo_tree(seed, grit_copy.path());
    let git_copy = TempDir::new().expect("git copy");
    duplicate_repo_tree(seed, git_copy.path());
    let repo =
        Repository::discover(Some(grit_copy.path()), &RepositoryOptions::empty()).expect("open");
    let outcome = gc_auto(&repo, &GcOptions::full_run(false, now_unix())).expect("gc_auto");
    assert!(
        matches!(
            outcome,
            AutoGcOutcome::Ran {
                reason: AutoGcReason::TooManyLoose { .. },
                ..
            }
        ),
        "{outcome:?}"
    );
    clear_pack_cache_after_disk_maintenance();
    git(git_copy.path(), &["gc", "--auto", "--quiet"]);
    (grit_copy, git_copy)
}

#[test]
fn gc_auto_prunes_expired_unreachable_loose_objects_like_git() {
    let _pack_guard = test_pack_cache_guard();
    let seed = repo_with_unreachable_loose_blobs("now", false);
    let (grit_copy, git_copy) = grit_and_git_gc_auto(seed.path());

    for field in ["count", "in-pack", "packs"] {
        assert_eq!(
            git_count_objects_field(grit_copy.path(), field),
            git_count_objects_field(git_copy.path(), field),
            "count-objects {field} must match git gc --auto"
        );
    }
    assert_eq!(
        git_count_objects_field(grit_copy.path(), "in-pack"),
        2,
        "only the reachable commit and tree are packed"
    );
    assert_eq!(git_count_objects_field(grit_copy.path(), "count"), 0);
    git_fsck(grit_copy.path(), true);
}

#[test]
fn gc_auto_leaves_recent_unreachable_loose_objects_loose_like_git() {
    let _pack_guard = test_pack_cache_guard();
    let seed = repo_with_unreachable_loose_blobs("2.weeks.ago", false);
    let unreachable = git_count_objects_field(seed.path(), "count") - 2;
    let (grit_copy, git_copy) = grit_and_git_gc_auto(seed.path());

    for field in ["count", "in-pack", "packs"] {
        assert_eq!(
            git_count_objects_field(grit_copy.path(), field),
            git_count_objects_field(git_copy.path(), field),
            "count-objects {field} must match git gc --auto"
        );
    }
    assert_eq!(
        git_count_objects_field(grit_copy.path(), "count"),
        unreachable,
        "unreachable objects inside the grace period stay loose for a later prune"
    );
    git_fsck(grit_copy.path(), true);
}

#[test]
fn gc_auto_prunes_when_every_loose_object_is_unreachable_like_git() {
    let _pack_guard = test_pack_cache_guard();
    let seed = repo_with_unreachable_loose_blobs("now", true);
    let (grit_copy, git_copy) = grit_and_git_gc_auto(seed.path());

    for field in ["count", "in-pack", "packs"] {
        assert_eq!(
            git_count_objects_field(grit_copy.path(), field),
            git_count_objects_field(git_copy.path(), field),
            "count-objects {field} must match git gc --auto"
        );
    }
    assert_eq!(git_count_objects_field(grit_copy.path(), "count"), 0);
    assert_eq!(git_count_objects_field(grit_copy.path(), "in-pack"), 2);
    git_fsck(grit_copy.path(), true);
}

fn shared_clone_from(source: &Path, dest: &Path) {
    let status = Command::new("git")
        .args([
            "clone",
            "--shared",
            &source.to_string_lossy(),
            &dest.to_string_lossy(),
        ])
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .status()
        .expect("git clone --shared");
    assert!(status.success(), "git clone --shared failed");
    git(dest, &["config", "gc.auto", "0"]);
}

fn history_source_repo(n_commits: usize) -> TempDir {
    let dir = TempDir::new().expect("tempdir");
    git(dir.path(), &["init", "-b", "main"]);
    git(dir.path(), &["config", "gc.auto", "0"]);
    for i in 0..n_commits {
        let path = dir.path().join(format!("file_{i}.txt"));
        std::fs::write(&path, format!("payload {i}\n")).expect("write");
        git(dir.path(), &["add", &format!("file_{i}.txt")]);
        git(dir.path(), &["commit", "-m", &format!("commit {i}")]);
    }
    dir
}

fn grit_commit_all(repo: &Repository, path: &Path, message: &str) {
    std::fs::write(path, format!("{message}\n")).expect("write");
    stage_for_commit(repo, &mut NullProgress, &NeverCancel).expect("stage");
    let ident = "Test <t@example.com> 1 +0000".to_string();
    create_commit(
        repo,
        &CommitRequest {
            message: message.to_string(),
            author: Some(ident.parse().expect("signature")),
            committer: Some(ident.parse().expect("signature")),
            allow_empty: false,
            sign_override: None,
            amend: false,
            assume_worktree_synced: false,
            skip_pre_commit_tree_match: false,
            staged_change_count: None,
            ..Default::default()
        },
        &mut NullProgress,
        &NeverCancel,
    )
    .expect("commit");
}

#[test]
fn gc_on_shared_clone_packs_only_local_objects_like_git() {
    let _pack_guard = test_pack_cache_guard();
    let source = history_source_repo(10);
    let grit_clone_dir = TempDir::new().expect("grit clone dir");
    let git_clone_dir = TempDir::new().expect("git clone dir");
    shared_clone_from(source.path(), grit_clone_dir.path());
    shared_clone_from(source.path(), git_clone_dir.path());

    let grit_repo = Repository::discover(Some(grit_clone_dir.path()), &RepositoryOptions::empty())
        .expect("open grit clone");
    grit_commit_all(
        &grit_repo,
        &grit_clone_dir.path().join("local-only.txt"),
        "grit local",
    );

    std::fs::write(git_clone_dir.path().join("local-only.txt"), b"git local\n").expect("write");
    git(git_clone_dir.path(), &["add", "local-only.txt"]);
    git(git_clone_dir.path(), &["commit", "-m", "git local"]);

    assert_eq!(
        git_count_objects_field(grit_clone_dir.path(), "in-pack"),
        0,
        "fresh shared clone should borrow the alternate"
    );

    let alternates_before =
        std::fs::read_to_string(grit_clone_dir.path().join(".git/objects/info/alternates"))
            .expect("alternates file");

    gc(
        &grit_repo,
        &GcOptions::with_reflog_policy(1_700_000_000, false),
    )
    .expect("grit gc");
    clear_pack_cache_after_disk_maintenance();

    git(git_clone_dir.path(), &["gc"]);

    let grit_in_pack = git_count_objects_field(grit_clone_dir.path(), "in-pack");
    let git_in_pack = git_count_objects_field(git_clone_dir.path(), "in-pack");
    assert_eq!(
        grit_in_pack, git_in_pack,
        "grit gc should pack the same local object count as git gc"
    );
    assert!(
        git_size_pack_kib(grit_clone_dir.path()) <= git_size_pack_kib(git_clone_dir.path()) + 1,
        "shared clone pack should stay small"
    );
    assert_eq!(
        std::fs::read_to_string(grit_clone_dir.path().join(".git/objects/info/alternates"))
            .expect("alternates after gc"),
        alternates_before
    );
    git_fsck(grit_clone_dir.path(), true);
}

#[test]
fn gc_shared_clone_dry_run_counts_local_objects_only() {
    let _pack_guard = test_pack_cache_guard();
    let source = history_source_repo(8);
    let clone_dir = TempDir::new().expect("clone");
    shared_clone_from(source.path(), clone_dir.path());
    let repo =
        Repository::discover(Some(clone_dir.path()), &RepositoryOptions::empty()).expect("open");
    grit_commit_all(&repo, &clone_dir.path().join("dry.txt"), "dry run local");

    let mut repack_opts = RepackOptions::default();
    repack_opts.dry_run = true;
    let detail = repack_all(&repo, &repack_opts).expect("repack dry-run");
    git(clone_dir.path(), &["gc"]);
    let git_in_pack = git_count_objects_field(clone_dir.path(), "in-pack");
    assert_eq!(
        detail.objects_packed,
        Some(git_in_pack),
        "dry-run object count should match git gc local pack size"
    );
}

#[test]
fn gc_shared_clone_recovers_from_duplicated_alternate_pack() {
    let _pack_guard = test_pack_cache_guard();
    let source = history_source_repo(12);
    let clone_dir = TempDir::new().expect("clone");
    shared_clone_from(source.path(), clone_dir.path());
    let repo =
        Repository::discover(Some(clone_dir.path()), &RepositoryOptions::empty()).expect("open");
    grit_commit_all(&repo, &clone_dir.path().join("recover.txt"), "local tip");

    git(clone_dir.path(), &["repack", "-a", "-d"]);
    clear_pack_cache_after_disk_maintenance();
    let inflated = git_count_objects_field(clone_dir.path(), "in-pack");
    assert!(
        inflated > git_count_objects_field(clone_dir.path(), "count") + 5,
        "repack -a without --local should duplicate the alternate into the clone"
    );

    repack_all(&repo, &RepackOptions::default()).expect("grit repack");
    clear_pack_cache_after_disk_maintenance();

    let after = git_count_objects_field(clone_dir.path(), "in-pack");
    assert!(
        after < inflated / 2,
        "repack after a duplicated alternate pack should keep only local objects"
    );
    git(clone_dir.path(), &["rev-parse", "HEAD"]);
    git_fsck(clone_dir.path(), true);
}

fn git_rev_parse(dir: &Path, spec: &str) -> String {
    let out = Command::new("git")
        .current_dir(dir)
        .args(["rev-parse", spec])
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .output()
        .expect("rev-parse");
    assert!(out.status.success(), "rev-parse {spec}");
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

fn local_loose_object_path(git_dir: &Path, oid: &str) -> PathBuf {
    git_dir.join("objects").join(&oid[..2]).join(&oid[2..])
}

/// Copy a loose object from the alternate into the clone's own `objects/` tree.
fn duplicate_alternate_loose_object_in_clone(clone: &Path, oid: &str) {
    let git_dir = clone.join(".git");
    let objects = git_dir.join("objects");
    let alt_file = objects.join("info/alternates");
    let alt_raw = std::fs::read_to_string(&alt_file).expect("alternates");
    let alt_line = alt_raw.lines().next().expect("alternates line").trim();
    let alt_objects = PathBuf::from(alt_line);
    let alt_objects = if alt_objects.is_absolute() {
        alt_objects
    } else {
        alt_file
            .parent()
            .expect("info")
            .join(alt_objects)
            .canonicalize()
            .expect("canonical alternates path")
    };
    let src = alt_objects.join(&oid[..2]).join(&oid[2..]);
    assert!(
        src.is_file(),
        "alternate must hold {oid} as loose for this fixture"
    );
    let dst = local_loose_object_path(&git_dir, oid);
    if let Some(parent) = dst.parent() {
        std::fs::create_dir_all(parent).expect("fan-out dir");
    }
    std::fs::copy(&src, &dst).expect("copy loose duplicate");
    assert!(dst.is_file(), "duplicate must exist as local loose");
}

/// Materialize a local loose copy when the object is available via alternates (e.g. packed).
fn write_loose_materialize_duplicate(repo: &Repository, oid: &str) {
    let oid: ObjectId = oid.parse().expect("oid");
    let obj = repo.odb().read(&oid).expect("read via alternates");
    repo.odb()
        .write_loose_materialize(obj.kind, &obj.data)
        .expect("local loose materialize");
    assert!(
        local_loose_object_path(repo.git_dir(), oid.to_hex().as_str()).is_file(),
        "local loose copy must exist"
    );
}

#[test]
fn gc_preserves_loose_duplicate_when_alternate_holds_object_loose_only() {
    let _pack_guard = test_pack_cache_guard();
    let source = history_source_repo(3);
    assert_eq!(
        git_pack_count(&source.path().join(".git")),
        0,
        "source must keep history loose-only in the alternate"
    );

    let grit_clone = TempDir::new().expect("grit clone");
    let git_clone = TempDir::new().expect("git clone");
    shared_clone_from(source.path(), grit_clone.path());
    shared_clone_from(source.path(), git_clone.path());

    let grit_repo =
        Repository::discover(Some(grit_clone.path()), &RepositoryOptions::empty()).expect("open");
    grit_commit_all(
        &grit_repo,
        &grit_clone.path().join("local.txt"),
        "grit local",
    );
    std::fs::write(git_clone.path().join("local.txt"), b"x\n").expect("write");
    git(git_clone.path(), &["add", "local.txt"]);
    git(git_clone.path(), &["commit", "-m", "git local"]);

    let parent = git_rev_parse(grit_clone.path(), "HEAD~1");
    duplicate_alternate_loose_object_in_clone(grit_clone.path(), &parent);
    duplicate_alternate_loose_object_in_clone(git_clone.path(), &parent);

    git(git_clone.path(), &["gc", "--quiet"]);
    gc(
        &grit_repo,
        &GcOptions::with_reflog_policy(1_700_000_000, false),
    )
    .expect("grit gc");
    clear_pack_cache_after_disk_maintenance();

    assert!(
        local_loose_object_path(&grit_clone.path().join(".git"), &parent).is_file(),
        "grit gc must keep a local loose duplicate when the alternate copy is loose-only"
    );
    assert!(
        local_loose_object_path(&git_clone.path().join(".git"), &parent).is_file(),
        "git gc keeps the local loose duplicate in this fixture"
    );
    git_fsck(grit_clone.path(), true);
}

#[test]
fn gc_prunes_loose_duplicate_when_alternate_holds_object_in_pack() {
    let _pack_guard = test_pack_cache_guard();
    let source = history_source_repo(5);
    git(source.path(), &["repack", "-a", "-d"]);
    assert!(
        git_pack_count(&source.path().join(".git")) >= 1,
        "alternate must serve objects from packs"
    );

    let grit_clone = TempDir::new().expect("grit clone");
    let git_clone = TempDir::new().expect("git clone");
    shared_clone_from(source.path(), grit_clone.path());
    shared_clone_from(source.path(), git_clone.path());

    let grit_repo =
        Repository::discover(Some(grit_clone.path()), &RepositoryOptions::empty()).expect("open");
    grit_commit_all(
        &grit_repo,
        &grit_clone.path().join("packed-alt.txt"),
        "grit local",
    );
    std::fs::write(git_clone.path().join("packed-alt.txt"), b"y\n").expect("write");
    git(git_clone.path(), &["add", "packed-alt.txt"]);
    git(git_clone.path(), &["commit", "-m", "git local"]);

    let parent = git_rev_parse(grit_clone.path(), "HEAD~1");
    write_loose_materialize_duplicate(&grit_repo, &parent);
    let git_repo = Repository::discover(Some(git_clone.path()), &RepositoryOptions::empty())
        .expect("open git clone");
    write_loose_materialize_duplicate(&git_repo, &parent);

    git(git_clone.path(), &["gc", "--quiet"]);
    gc(
        &grit_repo,
        &GcOptions::with_reflog_policy(1_700_000_000, false),
    )
    .expect("grit gc");
    clear_pack_cache_after_disk_maintenance();

    assert!(
        !local_loose_object_path(&grit_clone.path().join(".git"), &parent).exists(),
        "grit gc must prune a local loose duplicate when the alternate has a pack copy"
    );
    assert!(
        !local_loose_object_path(&git_clone.path().join(".git"), &parent).exists(),
        "git gc prunes the local loose duplicate in this fixture"
    );
    git_fsck(grit_clone.path(), true);
}

#[test]
fn gc_honors_write_commit_graph_false_like_git() {
    let dir = TempDir::new().expect("tempdir");
    git(dir.path(), &["init", "-b", "main"]);
    git(dir.path(), &["commit", "--allow-empty", "-m", "c1"]);
    git(dir.path(), &["gc", "--quiet"]);
    let git_dir = dir.path().join(".git");
    assert!(git_dir.join("objects/info/commit-graph").is_file());
    git(dir.path(), &["config", "gc.writeCommitGraph", "false"]);
    git(dir.path(), &["branch", "t1"]);

    let grit_copy = TempDir::new().expect("grit");
    duplicate_repo_tree(dir.path(), grit_copy.path());
    let git_copy = TempDir::new().expect("git");
    duplicate_repo_tree(dir.path(), git_copy.path());

    let repo =
        Repository::discover(Some(grit_copy.path()), &RepositoryOptions::empty()).expect("open");
    let stats = gc(&repo, &GcOptions::with_reflog_policy(1_700_000_000, false)).expect("gc");
    git(git_copy.path(), &["gc", "--quiet"]);

    assert!(
        stats.commit_graph.is_none(),
        "gc.writeCommitGraph=false must skip commit-graph during gc"
    );
    let grit_git = grit_copy.path().join(".git");
    assert!(
        grit_git.join("objects/info/commit-graph").is_file(),
        "existing commit-graph must be kept"
    );
    assert!(
        git_copy
            .path()
            .join(".git/objects/info/commit-graph")
            .is_file(),
        "git fixture keeps commit-graph"
    );
    assert_eq!(
        count_loose_head_ref_files(&grit_git),
        0,
        "pack-refs still runs when only writeCommitGraph is false"
    );
    assert!(
        grit_git.join("refs/heads").is_dir(),
        "refs/heads/ kept after pack-refs"
    );
    git_fsck(grit_copy.path(), true);
}

#[test]
fn gc_honors_pack_refs_false_and_notbare_like_git() {
    let dir = TempDir::new().expect("tempdir");
    git(dir.path(), &["init", "-b", "main"]);
    git(dir.path(), &["commit", "--allow-empty", "-m", "c1"]);
    git(dir.path(), &["config", "gc.packRefs", "false"]);
    git(dir.path(), &["branch", "t1"]);

    let grit_copy = TempDir::new().expect("grit");
    duplicate_repo_tree(dir.path(), grit_copy.path());
    let repo =
        Repository::discover(Some(grit_copy.path()), &RepositoryOptions::empty()).expect("open");
    let stats = gc(&repo, &GcOptions::with_reflog_policy(1_700_000_000, false)).expect("gc");
    assert!(stats.pack_refs.is_none());
    let git_dir = grit_copy.path().join(".git");
    assert!(
        count_loose_head_ref_files(&git_dir) >= 2,
        "gc.packRefs=false must leave loose branch refs"
    );

    git(dir.path(), &["config", "--unset", "gc.packRefs"]);
    git(dir.path(), &["config", "gc.packRefs", "notbare"]);
    let notbare_copy = TempDir::new().expect("notbare");
    duplicate_repo_tree(dir.path(), notbare_copy.path());
    let notbare_repo =
        Repository::discover(Some(notbare_copy.path()), &RepositoryOptions::empty()).expect("open");
    let notbare_stats = gc(
        &notbare_repo,
        &GcOptions::with_reflog_policy(1_700_000_000, false),
    )
    .expect("gc");
    assert!(notbare_stats.pack_refs.is_some());
    assert_eq!(
        count_loose_head_ref_files(&notbare_copy.path().join(".git")),
        0
    );

    let bare = TempDir::new().expect("bare");
    git(
        dir.path(),
        &["clone", "--bare", ".", bare.path().to_str().expect("bare")],
    );
    git(bare.path(), &["config", "gc.packRefs", "notbare"]);
    git(
        bare.path(),
        &[
            "update-ref",
            "refs/heads/t2",
            &git_rev_parse(bare.path(), "HEAD"),
        ],
    );
    let bare_repo =
        Repository::discover(Some(bare.path()), &RepositoryOptions::empty()).expect("open");
    let bare_stats = gc(
        &bare_repo,
        &GcOptions::with_reflog_policy(1_700_000_000, false),
    )
    .expect("gc");
    assert!(
        bare_stats.pack_refs.is_none(),
        "gc.packRefs=notbare must not pack refs in a bare repo"
    );
}

#[test]
fn gc_rejects_invalid_gc_pack_refs_config() {
    let dir = TempDir::new().expect("tempdir");
    git(dir.path(), &["init", "-b", "main"]);
    git(dir.path(), &["commit", "--allow-empty", "-m", "c1"]);
    git(dir.path(), &["config", "gc.packRefs", "maybe"]);
    let repo = Repository::discover(Some(dir.path()), &RepositoryOptions::empty()).expect("open");
    let err = gc(&repo, &GcOptions::with_reflog_policy(1_700_000_000, false)).unwrap_err();
    assert!(matches!(err, Error::Config(_)), "unexpected error: {err:?}");
}

#[test]
fn gc_honors_pack_write_bitmaps_synonym_and_scope_order() {
    fn pack_has_bitmap(git_dir: &Path) -> bool {
        std::fs::read_dir(git_dir.join("objects/pack"))
            .expect("pack dir")
            .filter_map(|e| e.ok())
            .any(|e| e.path().extension().is_some_and(|x| x == "bitmap"))
    }

    let dir = TempDir::new().expect("tempdir");
    git(dir.path(), &["init", "-b", "main"]);
    git(dir.path(), &["commit", "--allow-empty", "-m", "c1"]);
    git(dir.path(), &["repack", "-a", "-d"]);
    git(dir.path(), &["config", "pack.writeBitmaps", "true"]);

    let grit_copy = TempDir::new().expect("grit");
    duplicate_repo_tree(dir.path(), grit_copy.path());
    let repo =
        Repository::discover(Some(grit_copy.path()), &RepositoryOptions::empty()).expect("open");
    gc(&repo, &GcOptions::with_reflog_policy(1_700_000_000, false)).expect("gc");
    assert!(
        pack_has_bitmap(&grit_copy.path().join(".git")),
        "pack.writeBitmaps alone must enable gc repack bitmaps"
    );

    let order_base = TempDir::new().expect("order base");
    git(order_base.path(), &["init", "-b", "main"]);
    git(order_base.path(), &["commit", "--allow-empty", "-m", "c1"]);
    git(order_base.path(), &["repack", "-a", "-d"]);
    let order_dir = TempDir::new().expect("order");
    duplicate_repo_tree(order_base.path(), order_dir.path());
    git(
        order_dir.path(),
        &["config", "repack.writeBitmaps", "false"],
    );
    git(order_dir.path(), &["config", "pack.writeBitmaps", "true"]);
    let order_repo =
        Repository::discover(Some(order_dir.path()), &RepositoryOptions::empty()).expect("open");
    gc(
        &order_repo,
        &GcOptions::with_reflog_policy(1_700_000_000, false),
    )
    .expect("gc");
    assert!(
        pack_has_bitmap(&order_dir.path().join(".git")),
        "later pack.writeBitmaps must override earlier repack.writeBitmaps=false"
    );

    let reverse_base = TempDir::new().expect("reverse base");
    git(reverse_base.path(), &["init", "-b", "main"]);
    git(
        reverse_base.path(),
        &["commit", "--allow-empty", "-m", "c1"],
    );
    git(reverse_base.path(), &["repack", "-a", "-d"]);
    let reverse_dir = TempDir::new().expect("reverse");
    duplicate_repo_tree(reverse_base.path(), reverse_dir.path());
    git(reverse_dir.path(), &["config", "pack.writeBitmaps", "true"]);
    git(
        reverse_dir.path(),
        &["config", "repack.writeBitmaps", "false"],
    );
    let reverse_repo =
        Repository::discover(Some(reverse_dir.path()), &RepositoryOptions::empty()).expect("open");
    gc(
        &reverse_repo,
        &GcOptions::with_reflog_policy(1_700_000_000, false),
    )
    .expect("gc");
    assert!(
        !pack_has_bitmap(&reverse_dir.path().join(".git")),
        "later repack.writeBitmaps=false must override earlier pack.writeBitmaps=true"
    );
}

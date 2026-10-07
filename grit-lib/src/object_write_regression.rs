//! Regression tests locking in #912 / #913 hot-path behaviour (object write, freshen, hash counts).

#![cfg(test)]

use std::fs;
use std::path::Path;
use std::process::Command;

use filetime::{set_file_mtime, FileTime};
use tempfile::TempDir;

use crate::index::{entry_from_stat, Index, IndexEntry, MODE_REGULAR};
use crate::midx;
use crate::objects::{parse_commit, ObjectId, ObjectKind};
use crate::odb::Odb;
use crate::pack;
use crate::porcelain::add::{stage, StageOptions};
use crate::porcelain::commit::{create_commit, CommitRequest};
use crate::progress::NullProgress;
use crate::repo::{init_repository, Repository};
use crate::write_tree::{cache_tree_update, write_tree_update_index, WriteTreeFlags};

const INDEX_MTIME: (u32, u32) = (1_700_000_000, 123_456_789);

struct PackStampCountingGuard {
    _lock: std::sync::MutexGuard<'static, ()>,
}

impl PackStampCountingGuard {
    fn acquire() -> Self {
        static LOCK: std::sync::OnceLock<std::sync::Mutex<()>> = std::sync::OnceLock::new();
        let _lock = LOCK
            .get_or_init(|| std::sync::Mutex::new(()))
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        Self { _lock }
    }
}

impl Drop for PackStampCountingGuard {
    fn drop(&mut self) {
        pack::test_set_pack_signature_stat_counting(false);
        midx::test_set_midx_stamp_counting(false);
        crate::odb::test_counters::set_freshen_counting(false);
    }
}

fn bench_oid(seed: u32) -> ObjectId {
    let mut bytes = [0x42_u8; 20];
    bytes[16..20].copy_from_slice(&seed.to_be_bytes());
    ObjectId::from_bytes(&bytes).expect("bench oid")
}

fn make_index_entry(path: &str, seed: u32) -> IndexEntry {
    let path_bytes = path.as_bytes().to_vec();
    IndexEntry {
        ctime_sec: INDEX_MTIME.0,
        ctime_nsec: INDEX_MTIME.1,
        mtime_sec: INDEX_MTIME.0,
        mtime_nsec: INDEX_MTIME.1,
        dev: 1,
        ino: seed,
        mode: MODE_REGULAR,
        uid: 1000,
        gid: 1000,
        size: 64,
        oid: bench_oid(seed),
        flags: path_bytes.len().min(0xFFF) as u16,
        flags_extended: None,
        path: path_bytes,
        base_index_pos: 0,
    }
}

fn build_flat_index(entry_count: usize, version: u32) -> Index {
    let mut idx = Index::empty(version);
    for i in 0..entry_count {
        let path = format!("d{:04}/f{:05}.txt", i / 100, i % 100);
        idx.add_or_replace(make_index_entry(&path, i as u32));
    }
    idx
}

fn pin_mtime(path: &Path, sec: u32, nsec: u32) {
    set_file_mtime(path, FileTime::from_unix_time(i64::from(sec), nsec)).unwrap();
}

fn git_in(dir: &Path, args: &[&str]) {
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
}

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

fn materialize_index_worktree(repo: &Repository, index: &Index, content_prefix: &str) {
    let wt = repo.work_tree.as_ref().unwrap();
    for entry in &index.entries {
        let rel = std::str::from_utf8(&entry.path).unwrap();
        let abs = wt.join(rel);
        if let Some(parent) = abs.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        fs::write(&abs, format!("{content_prefix}{rel}")).unwrap();
        pin_mtime(&abs, INDEX_MTIME.0, INDEX_MTIME.1);
    }
}

fn finalize_index_stat_trust(repo: &Repository) {
    let wt = repo.work_tree.as_ref().unwrap();
    let mut index = repo.load_index().unwrap();
    for entry in &mut index.entries {
        if entry.stage() != 0 {
            continue;
        }
        let abs = wt.join(std::str::from_utf8(&entry.path).unwrap());
        if let Ok(fresh) = entry_from_stat(&abs, &entry.path, entry.oid, entry.mode) {
            *entry = fresh;
        }
    }
    repo.write_index(&mut index).unwrap();
    // Index mtime must be strictly after entry mtimes so paths are not racy-clean.
    pin_mtime(
        &repo.index_path(),
        INDEX_MTIME.0.saturating_add(60),
        INDEX_MTIME.1,
    );
}

fn commit_ident() -> String {
    "Test User <t@example.com> 1 +0000".to_owned()
}

#[test]
fn write_tree_unchanged_index_writes_and_freshens_nothing() {
    let tmp = TempDir::new().unwrap();
    let odb = Odb::new(tmp.path());
    let mut index = build_flat_index(10_000, 2);
    for i in 0..index.entries.len() {
        let data = format!("blob-{i}");
        let oid = odb.write(ObjectKind::Blob, data.as_bytes()).unwrap();
        index.entries[i].oid = oid;
    }
    cache_tree_update(&odb, &mut index, WriteTreeFlags::default()).unwrap();

    crate::write_tree::test_reset_tree_write_count();
    crate::odb::test_counters::reset_freshen_calls();
    crate::odb::test_counters::set_freshen_counting(true);
    write_tree_update_index(&odb, &mut index, "", WriteTreeFlags::default()).unwrap();
    crate::odb::test_counters::set_freshen_counting(false);

    assert_eq!(
        crate::write_tree::test_tree_write_count(),
        0,
        "valid cache-tree must not rewrite tree objects"
    );
    assert_eq!(
        crate::odb::test_counters::freshen_calls(),
        0,
        "unchanged write-tree must not freshen objects"
    );
}

#[test]
fn create_commit_one_change_writes_only_changed_path_trees() {
    let tmp = TempDir::new().unwrap();
    let repo = init_repository(tmp.path(), false, "main", None, "files").unwrap();
    let mut index = Index::new();
    let paths = [
        "top/mid/unchanged.txt",
        "top/mid/changed.txt",
        "other/leaf.txt",
    ];
    for (i, rel) in paths.iter().enumerate() {
        let data = format!("seed-{i}");
        let oid = repo.odb.write(ObjectKind::Blob, data.as_bytes()).unwrap();
        let abs = repo.work_tree.as_ref().unwrap().join(rel);
        fs::create_dir_all(abs.parent().unwrap()).unwrap();
        fs::write(&abs, &data).unwrap();
        pin_mtime(&abs, INDEX_MTIME.0, INDEX_MTIME.1);
        let entry = entry_from_stat(&abs, rel.as_bytes(), oid, MODE_REGULAR).unwrap();
        index.add_or_replace(entry);
    }
    repo.write_index(&mut index).unwrap();
    pin_mtime(&repo.index_path(), INDEX_MTIME.0, INDEX_MTIME.1);

    let ident = commit_ident();
    create_commit(
        &repo,
        &CommitRequest {
            message: "initial".into(),
            author: ident.clone(),
            committer: ident.clone(),
            allow_empty: false,
        },
        &mut NullProgress,
    )
    .unwrap();

    let changed_path = repo.work_tree.as_ref().unwrap().join("top/mid/changed.txt");
    fs::write(&changed_path, b"updated-content").unwrap();
    pin_mtime(&changed_path, INDEX_MTIME.0 + 1, INDEX_MTIME.1);
    let new_oid = repo
        .odb
        .write(ObjectKind::Blob, b"updated-content")
        .unwrap();
    let mut index = repo.load_index().unwrap();
    let entry =
        entry_from_stat(&changed_path, b"top/mid/changed.txt", new_oid, MODE_REGULAR).unwrap();
    index.add_or_replace(entry);
    repo.write_index(&mut index).unwrap();

    crate::write_tree::test_reset_tree_write_count();
    crate::odb::test_counters::reset_freshen_calls();
    crate::odb::test_counters::set_freshen_counting(true);
    let outcome = create_commit(
        &repo,
        &CommitRequest {
            message: "second".into(),
            author: ident.clone(),
            committer: ident,
            allow_empty: false,
        },
        &mut NullProgress,
    )
    .unwrap();
    crate::odb::test_counters::set_freshen_counting(false);

    assert!(
        crate::write_tree::test_tree_write_count() <= 3,
        "one depth-2 path change should rewrite at most root + top + mid trees, got {}",
        crate::write_tree::test_tree_write_count()
    );
    assert!(
        crate::odb::test_counters::freshen_calls() <= 3,
        "freshen calls must scale with changed trees, not index size (got {})",
        crate::odb::test_counters::freshen_calls()
    );

    let wt = repo.work_tree.as_ref().unwrap();
    git_in(wt, &["fsck", "--strict"]);
    let git_tree = git_out(wt, &["rev-parse", "HEAD^{tree}"]);
    let commit_obj = repo.odb.read(&outcome.oid).unwrap();
    let grit_tree = parse_commit(&commit_obj.data).unwrap().tree.to_hex();
    assert_eq!(
        git_tree, grit_tree,
        "git and grit must agree on commit tree"
    );
}

#[test]
fn stage_one_modified_file_hashes_one_blob() {
    let tmp = TempDir::new().unwrap();
    let repo = init_repository(tmp.path(), false, "main", None, "files").unwrap();
    let mut index = build_flat_index(10_000, 2);
    materialize_index_worktree(&repo, &index, "v1:");
    for entry in index.entries.iter_mut() {
        let rel = std::str::from_utf8(&entry.path).unwrap();
        let data = format!("v1:{rel}");
        entry.oid = repo.odb.write(ObjectKind::Blob, data.as_bytes()).unwrap();
        let abs = repo.work_tree.as_ref().unwrap().join(rel);
        if let Ok(fresh) = entry_from_stat(&abs, &entry.path, entry.oid, entry.mode) {
            *entry = fresh;
        }
    }
    repo.write_index(&mut index).unwrap();
    finalize_index_stat_trust(&repo);

    let target = "d0050/f00050.txt";
    let target_abs = repo.work_tree.as_ref().unwrap().join(target);
    fs::write(&target_abs, b"v2:changed").unwrap();
    pin_mtime(&target_abs, INDEX_MTIME.0 + 1, INDEX_MTIME.1);

    crate::odb::test_counters::reset_blob_content_reads();
    let staged = stage(&repo, &StageOptions::default(), &mut NullProgress).unwrap();
    assert_eq!(staged.modified, 1, "expected exactly one staged path");
    assert_eq!(
        crate::odb::test_counters::blob_content_reads(),
        1,
        "staging must hash/read only the modified worktree file"
    );
}

#[test]
fn pack_signature_not_restatted_per_object() {
    let _guard = PackStampCountingGuard::acquire();
    let tmp = TempDir::new().unwrap();
    let repo = init_repository(tmp.path(), false, "main", None, "files").unwrap();
    let wt = repo.work_tree.as_ref().unwrap();
    let mut index = build_flat_index(10_000, 2);
    materialize_index_worktree(&repo, &index, "pack:");
    for entry in index.entries.iter_mut() {
        let rel = std::str::from_utf8(&entry.path).unwrap();
        let data = format!("pack:{rel}");
        entry.oid = repo.odb.write(ObjectKind::Blob, data.as_bytes()).unwrap();
        let abs = wt.join(rel);
        if let Ok(fresh) = entry_from_stat(&abs, &entry.path, entry.oid, entry.mode) {
            *entry = fresh;
        }
    }
    repo.write_index(&mut index).unwrap();
    finalize_index_stat_trust(&repo);

    let ident = commit_ident();
    create_commit(
        &repo,
        &CommitRequest {
            message: "initial".into(),
            author: ident.clone(),
            committer: ident.clone(),
            allow_empty: false,
        },
        &mut NullProgress,
    )
    .unwrap();

    git_in(wt, &["repack", "-adf"]);
    git_in(wt, &["multi-pack-index", "write"]);
    pack::clear_pack_cache();

    let sample = index.entries[0].oid;
    let _ = repo.odb.read(&sample).expect("warm pack cache");

    let mut index = repo.load_index().unwrap();
    crate::odb::test_counters::reset_freshen_calls();
    pack::test_reset_pack_signature_stat_calls();
    midx::test_reset_midx_stamp_calls();
    pack::test_set_pack_signature_stat_counting(true);
    midx::test_set_midx_stamp_counting(true);
    crate::odb::test_counters::set_freshen_counting(true);
    for entry in &index.entries {
        let obj = repo.odb.read(&entry.oid).unwrap();
        let _ = repo
            .odb
            .write_with_options(obj.kind, &obj.data, Default::default())
            .unwrap();
    }
    pack::test_set_pack_signature_stat_counting(false);
    midx::test_set_midx_stamp_counting(false);
    crate::odb::test_counters::set_freshen_counting(false);

    let stamp_calls =
        pack::test_pack_signature_stat_call_count() + midx::test_midx_stamp_call_count();
    assert!(
        stamp_calls <= 10,
        "pack/midx signature stats must not scale with object count (got {stamp_calls})"
    );
    assert!(
        crate::odb::test_counters::freshen_calls() <= 16,
        "freshen touches must not scale with packed object count (got {})",
        crate::odb::test_counters::freshen_calls()
    );
    assert!(
        stamp_calls < 500,
        "sanity: stamp calls would be ~10k if regression returned"
    );

    let target = wt.join("d0099/f00099.txt");
    fs::write(&target, b"pack:changed").unwrap();
    pin_mtime(&target, INDEX_MTIME.0 + 2, INDEX_MTIME.1);
    let new_oid = repo.odb.write(ObjectKind::Blob, b"pack:changed").unwrap();
    let entry = entry_from_stat(&target, b"d0099/f00099.txt", new_oid, MODE_REGULAR).unwrap();
    index.add_or_replace(entry);
    repo.write_index(&mut index).unwrap();

    pack::test_reset_pack_signature_stat_calls();
    midx::test_reset_midx_stamp_calls();
    pack::test_set_pack_signature_stat_counting(true);
    midx::test_set_midx_stamp_counting(true);
    let _ = create_commit(
        &repo,
        &CommitRequest {
            message: "packed follow-up".into(),
            author: ident.clone(),
            committer: ident,
            allow_empty: false,
        },
        &mut NullProgress,
    )
    .unwrap();
    pack::test_set_pack_signature_stat_counting(false);
    midx::test_set_midx_stamp_counting(false);
    let stamp_after_commit =
        pack::test_pack_signature_stat_call_count() + midx::test_midx_stamp_call_count();
    assert!(
        stamp_after_commit <= 12,
        "commit at 10k must not restat pack/midx per object (got {stamp_after_commit})"
    );

    git_in(wt, &["fsck", "--strict"]);
}

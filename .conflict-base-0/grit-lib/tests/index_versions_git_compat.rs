//! Index format versions, bogus inputs, unmerged stages, and resolve-undo (t1600/t1601/t1015/t2030/t2107).

mod support;

use std::fs;
use std::process::Command;

use grit_lib::config::environment::RepositoryOptions;
use grit_lib::error::Error;
use grit_lib::index::resolve_undo::{
    parse_resolve_undo_payload, write_resolve_undo_payload, ResolveUndoRecord,
};
use grit_lib::index::{Index, IndexEntry, MODE_REGULAR};
use grit_lib::objects::{ObjectId, ObjectKind};
use grit_lib::repo::Repository;

use support::{
    assert_git_fsck_strict, assert_grit_index_matches_git_ls_files,
    assert_porcelain_v2_matches_git, git, git_ls_files_stage, seed_multidir_repo, HistoryRepo,
};

fn open_grit(wt: &std::path::Path) -> Repository {
    Repository::discover(Some(wt), &RepositoryOptions::empty()).expect("open grit repo")
}

#[test]
fn t1600_index_versions_roundtrip_with_git() {
    for version in [2u32, 3, 4] {
        let repo = HistoryRepo::init();
        seed_multidir_repo(&repo);
        let wt = repo.worktree();

        let grit = open_grit(wt);
        let mut index = grit.index_handle().load().expect("load");
        index.version = version;
        if version == 3 {
            if let Some(e) = index.entries_mut().first_mut() {
                e.set_skip_worktree(true);
            }
        }
        grit.index_handle()
            .write(&mut index)
            .expect("grit write index");
        assert_git_fsck_strict(wt);
        assert_grit_index_matches_git_ls_files(wt, &index);

        // Git reads grit bytes.
        let git_debug = git(wt, &["ls-files", "-s"]);
        assert!(!git_debug.is_empty());

        // Git writes the same logical index at this version; grit reloads.
        match version {
            2 => {
                Command::new("git")
                    .current_dir(wt)
                    .args(["add", "-A"])
                    .env("GIT_INDEX_VERSION", "2")
                    .status()
                    .expect("git add");
            }
            3 => {
                git(wt, &["update-index", "--skip-worktree", "a/f0.txt"]);
            }
            4 => {
                git(wt, &["config", "index.version", "4"]);
                Command::new("git")
                    .current_dir(wt)
                    .args(["add", "-A"])
                    .env("GIT_INDEX_VERSION", "4")
                    .status()
                    .expect("git add");
            }
            _ => unreachable!(),
        }
        let bytes = fs::read(wt.join(".git/index")).expect("read git index");
        let parsed =
            Index::parse_with_algo(&bytes, grit.odb().hash_algo()).expect("parse git index");
        let on_disk_version = u32::from_be_bytes(bytes[4..8].try_into().expect("header"));
        assert_eq!(parsed.version, on_disk_version);
        match version {
            2 => assert_eq!(on_disk_version, 2),
            3 => assert!(on_disk_version >= 3, "skip-worktree forces index v3+"),
            4 => assert_eq!(on_disk_version, 4),
            _ => {}
        }
        assert_grit_index_matches_git_ls_files(wt, &parsed);

        assert_porcelain_v2_matches_git(wt, &grit);
    }
}

#[test]
fn t1601_bogus_index_typed_errors() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let path = tmp.path().join("index");

    fs::write(&path, b"XXXX").expect("write");
    let err = Index::load(&path).unwrap_err();
    assert!(matches!(err, Error::IndexError(_)));

    let mut good = Index::new();
    good.add_or_replace(IndexEntry {
        ctime_sec: 0,
        ctime_nsec: 0,
        mtime_sec: 0,
        mtime_nsec: 0,
        dev: 0,
        ino: 0,
        mode: MODE_REGULAR,
        uid: 0,
        gid: 0,
        size: 1,
        oid: ObjectId::zero(),
        flags: 1,
        flags_extended: None,
        path: b"f".to_vec(),
        base_index_pos: 0,
    });
    good.write(&path).expect("write index");
    let loaded_null = Index::load(&path).expect("load index with null oid entry");
    assert_eq!(loaded_null.entries()[0].oid, ObjectId::zero());

    let mut bytes = fs::read(&path).expect("read");
    // Corrupt entry count in header.
    bytes[8..12].copy_from_slice(&9999u32.to_be_bytes());
    let err = Index::parse_with_algo(&bytes, grit_lib::objects::HashAlgo::Sha1).unwrap_err();
    assert!(matches!(err, Error::IndexError(_)));

    let bad_ru = b"path\0not-octal\0\0\0";
    let err = parse_resolve_undo_payload(bad_ru).unwrap_err();
    assert!(matches!(err, Error::IndexError(_)));
}

#[test]
fn t1015_read_index_unmerged_stages_roundtrip() {
    let repo = HistoryRepo::init();
    seed_multidir_repo(&repo);
    let wt = repo.worktree();
    let grit = open_grit(wt);

    let blob = grit.odb().write(ObjectKind::Blob, b"base\n").expect("blob");
    let mut index = grit.index_handle().load().expect("load");
    for stage in 1u8..=3 {
        let mut e = IndexEntry {
            ctime_sec: 1,
            ctime_nsec: 0,
            mtime_sec: 1,
            mtime_nsec: 0,
            dev: 0,
            ino: 0,
            mode: MODE_REGULAR,
            uid: 0,
            gid: 0,
            size: 5,
            oid: blob,
            flags: 4,
            flags_extended: None,
            path: b"conflict.txt".to_vec(),
            base_index_pos: 0,
        };
        e.flags = (e.flags & 0x0FFF) | ((stage as u16) << 12);
        index.add_or_replace(e);
    }
    grit.index_handle().write(&mut index).expect("write");
    assert_git_fsck_strict(wt);

    let git_rows = git_ls_files_stage(wt);
    assert_eq!(git_rows.iter().filter(|p| p.0 == "conflict.txt").count(), 3);

    let reloaded = Index::load(&grit.index_handle().path()).expect("reload");
    assert!(reloaded.has_unmerged_entries());
}

#[test]
fn t2030_resolve_undo_roundtrip() {
    let repo = HistoryRepo::init();
    seed_multidir_repo(&repo);
    let wt = repo.worktree();
    let grit = open_grit(wt);
    let oid1 = grit.odb().write(ObjectKind::Blob, b"ours\n").expect("blob");
    let oid2 = grit
        .odb()
        .write(ObjectKind::Blob, b"theirs\n")
        .expect("blob");
    let mut map = std::collections::BTreeMap::new();
    map.insert(
        b"path.txt".to_vec(),
        ResolveUndoRecord {
            modes: [0o100644, 0o100644, 0],
            oids: [oid1, oid2, ObjectId::zero()],
        },
    );
    let payload = write_resolve_undo_payload(&map);
    let parsed = parse_resolve_undo_payload(&payload).expect("parse REUC");
    assert_eq!(parsed, map);

    let mut index = grit.index_handle().load().expect("load");
    index.resolve_undo = Some(parsed);
    index.version = 3;
    grit.index_handle().write(&mut index).expect("write");
    assert_git_fsck_strict(wt);

    let bytes = fs::read(grit.index_handle().path()).expect("read index");
    assert!(bytes.windows(4).any(|w| w == b"REUC"));
    let loaded = Index::load(&grit.index_handle().path()).expect("reload");
    assert_eq!(loaded.resolve_undo, index.resolve_undo);
}

#[test]
fn t2107_intent_to_add_forces_index_version_three() {
    let repo = HistoryRepo::init();
    let wt = repo.worktree();
    fs::write(wt.join("new.txt"), "n\n").expect("write");
    let grit = open_grit(wt);
    let mut index = grit.index_handle().load().expect("load");
    assert_eq!(index.version, 2);
    let blob = grit.odb().write(ObjectKind::Blob, b"n\n").expect("blob");
    let mut e = IndexEntry {
        ctime_sec: 1,
        ctime_nsec: 0,
        mtime_sec: 1,
        mtime_nsec: 0,
        dev: 0,
        ino: 0,
        mode: MODE_REGULAR,
        uid: 0,
        gid: 0,
        size: 2,
        oid: blob,
        flags: 7,
        flags_extended: None,
        path: b"new.txt".to_vec(),
        base_index_pos: 0,
    };
    e.set_intent_to_add(true);
    index.add_or_replace(e);
    assert!(index.entries().iter().any(|e| e.intent_to_add()));
    assert_eq!(
        index.version, 2,
        "intent-to-add must not bump version until write"
    );
    grit.index_handle().write(&mut index).expect("write");
    assert_git_fsck_strict(wt);
    let header = fs::read(grit.index_handle().path()).expect("read")[4..8].to_vec();
    assert_eq!(header, 3u32.to_be_bytes());
}

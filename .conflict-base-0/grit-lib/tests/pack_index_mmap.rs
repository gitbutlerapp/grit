//! Memory-mapped pack index open path and lazy validation behavior.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use grit_lib::config::environment::RepositoryOptions;
use grit_lib::error::Error;
use grit_lib::maintenance::{repack_all, RepackOptions};
use grit_lib::objects::odb::Odb;
use grit_lib::objects::pack::{
    clear_pack_cache, parse_pack_index_bytes, read_object_from_pack, read_pack_index,
    read_pack_index_cached, read_pack_index_no_verify, test_pack_cache_guard,
    verify_pack_and_collect, write_v2_pack_index_with_trailer,
};
use grit_lib::objects::ObjectId;
use grit_lib::repo::Repository;
use grit_test_support::objects::{
    write_pack_and_index, IndexPackOptions, IndexVersion, ObjectKind as FixtureKind, PackBuilder,
    RepoFixture,
};
use std::path::Path;

fn index_v2(repo: &RepoFixture, stem: &str, pack_bytes: &[u8]) -> std::path::PathBuf {
    write_pack_and_index(
        &repo.objects_dir(),
        stem,
        pack_bytes,
        grit_test_support::objects::HashAlgo::Sha1,
        &IndexPackOptions {
            index_version: Some(IndexVersion::V2),
            ..IndexPackOptions::default()
        },
    )
    .idx_path
    .expect("idx")
}

#[test]
fn git_written_v1_and_v2_mmap_match_parsed_offsets() {
    let repo = RepoFixture::init(grit_test_support::objects::HashAlgo::Sha1).expect("init");
    let mut builder = PackBuilder::new(grit_test_support::objects::HashAlgo::Sha1);
    builder.add_full(FixtureKind::Blob, b"one");
    builder.add_full(FixtureKind::Blob, b"two");
    let built = builder.build();
    let idx_v2 = index_v2(&repo, "mmap-v2", &built.bytes);
    let idx_v1 = write_pack_and_index(
        &repo.objects_dir(),
        "mmap-v1",
        &built.bytes,
        grit_test_support::objects::HashAlgo::Sha1,
        &IndexPackOptions {
            index_version: Some(IndexVersion::V1),
            ..IndexPackOptions::default()
        },
    )
    .idx_path
    .expect("idx");
    for idx_path in [idx_v2, idx_v1] {
        clear_pack_cache();
        let mapped = read_pack_index_no_verify(&idx_path).expect("mmap open");
        let from_bytes =
            parse_pack_index_bytes(&idx_path, std::fs::read(&idx_path).expect("read"), false)
                .expect("parse bytes");
        assert_eq!(mapped.len(), from_bytes.len());
        for (i, entry) in mapped.iter().enumerate() {
            assert_eq!(entry.oid(), from_bytes.oid_at(i));
            assert_eq!(
                entry.offset().expect("off"),
                from_bytes.offset_at(i).expect("off")
            );
            assert_eq!(entry.crc32(), from_bytes.crc32_at(i));
        }
    }
}

#[test]
fn git_large_offset_table_opens_via_mmap() {
    let repo = RepoFixture::init(grit_test_support::objects::HashAlgo::Sha1).expect("init");
    let mut builder = PackBuilder::new(grit_test_support::objects::HashAlgo::Sha1);
    builder.add_full(FixtureKind::Blob, b"large-off");
    let built = builder.build();
    let out = write_pack_and_index(
        &repo.objects_dir(),
        "large-off",
        &built.bytes,
        grit_test_support::objects::HashAlgo::Sha1,
        &IndexPackOptions {
            index_version: Some(IndexVersion::V2LargeOffsetAt(0x40)),
            ..IndexPackOptions::default()
        },
    );
    let idx_path = out.idx_path.expect("idx");
    let mapped = read_pack_index_no_verify(&idx_path).expect("mmap");
    let oid = ObjectId::from_bytes(mapped.oid_at(0)).expect("oid");
    assert!(mapped.find_offset(&oid).expect("lookup").is_some());
}

#[test]
fn lazy_v2_out_of_range_large_offset_slot_returns_corruption_not_panic() {
    let oid = ObjectId::from_bytes(&[0x01; 20]).expect("oid");
    let mut body = Vec::new();
    body.extend_from_slice(b"\xfftOc");
    body.extend_from_slice(&2u32.to_be_bytes());
    let fanout = grit_lib::objects::pack::compute_fanout_from_oid_slices(&[oid.as_bytes()]);
    for f in fanout {
        body.extend_from_slice(&f.to_be_bytes());
    }
    body.extend_from_slice(oid.as_bytes());
    body.extend_from_slice(&0u32.to_be_bytes());
    body.extend_from_slice(&0xffff_ffffu32.to_be_bytes());
    body.extend_from_slice(&[0u8; 20]);
    body.extend_from_slice(&[0u8; 20]);
    let idx = parse_pack_index_bytes(Path::new("bad-large-slot.idx"), body, false).expect("lazy");
    let err = idx.find_offset(&oid).expect_err("corrupt slot");
    assert!(matches!(err, Error::CorruptObject(_)));
}

#[test]
fn lazy_v2_inconsistent_large_offset_slot_returns_corruption() {
    let oid = ObjectId::from_bytes(&[0x01; 20]).expect("oid");
    let mut body = Vec::new();
    body.extend_from_slice(b"\xfftOc");
    body.extend_from_slice(&2u32.to_be_bytes());
    let fanout = grit_lib::objects::pack::compute_fanout_from_oid_slices(&[oid.as_bytes()]);
    for f in fanout {
        body.extend_from_slice(&f.to_be_bytes());
    }
    body.extend_from_slice(oid.as_bytes());
    body.extend_from_slice(&0u32.to_be_bytes());
    body.extend_from_slice(&0x8000_0001u32.to_be_bytes());
    body.extend_from_slice(&1000u64.to_be_bytes());
    body.extend_from_slice(&[0u8; 20]);
    body.extend_from_slice(&[0u8; 20]);
    let idx = parse_pack_index_bytes(Path::new("bad-assign.idx"), body, false).expect("lazy");
    let err = idx.find_offset(&oid).expect_err("inconsistent slot");
    assert!(matches!(err, Error::CorruptObject(_)));
}

#[test]
fn corrupt_large_offset_read_object_from_pack_propagates_error() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let idx_path = tmp.path().join("c.idx");
    let pack_path = tmp.path().join("c.pack");
    std::fs::write(&pack_path, b"PACK\x00\x00\x00\x02\x00\x00\x00\x00").expect("pack");
    let oid = ObjectId::from_bytes(&[0x01; 20]).expect("oid");
    write_v2_pack_index_with_trailer(&idx_path, &[(oid, 0x8000_0000u64, 0)], &[0u8; 20], 20, None)
        .expect("write");
    let mut raw = std::fs::read(&idx_path).expect("read");
    let hb = 20;
    let off32 = 8 + 256 * 4 + hb + 4;
    raw[off32..off32 + 4].copy_from_slice(&0xffff_ffffu32.to_be_bytes());
    std::fs::write(&idx_path, &raw).expect("write corrupt");
    let idx = read_pack_index_no_verify(&idx_path).expect("open");
    let mut idx = idx;
    idx.pack_path = pack_path;
    let err = read_object_from_pack(&idx, &oid).expect_err("typed corruption");
    assert!(matches!(err, Error::CorruptObject(_)));
}

#[test]
fn misordered_oid_lazy_open_verify_and_fsck_reject() {
    let repo = RepoFixture::init(grit_test_support::objects::HashAlgo::Sha1).expect("init");
    let mut builder = PackBuilder::new(grit_test_support::objects::HashAlgo::Sha1);
    builder.add_full(FixtureKind::Blob, b"a");
    builder.add_full(FixtureKind::Blob, b"b");
    let built = builder.build();
    let out = write_pack_and_index(
        &repo.objects_dir(),
        "order",
        &built.bytes,
        grit_test_support::objects::HashAlgo::Sha1,
        &IndexPackOptions {
            index_version: Some(IndexVersion::V2),
            ..IndexPackOptions::default()
        },
    );
    let idx_path = out.idx_path.expect("idx");
    let pack_path = out.pack_path;
    let mut raw = std::fs::read(&idx_path).expect("read idx");
    let idx = read_pack_index(&idx_path).expect("valid idx");
    let hb = idx.hash_bytes();
    let oid_base = 8 + 256 * 4;
    let (left, right) = raw.split_at_mut(oid_base + hb);
    left[oid_base..].swap_with_slice(&mut right[..hb]);
    let corrupt_path = pack_path.with_file_name("swapped-oids.idx");
    std::fs::write(&corrupt_path, &raw).expect("write corrupt");
    std::fs::hard_link(&pack_path, corrupt_path.with_extension("pack")).expect("link pack");

    read_pack_index_no_verify(&corrupt_path).expect("lazy open accepts swapped oids");
    assert!(matches!(
        read_pack_index(&corrupt_path),
        Err(Error::CorruptObject(msg)) if msg.contains("out of order")
    ));
    assert!(matches!(
        verify_pack_and_collect(&corrupt_path),
        Err(Error::CorruptObject(_))
    ));

    let lazy = read_pack_index_no_verify(&corrupt_path).expect("lazy idx");
    let oid_a = ObjectId::from_bytes(idx.oid_at(0)).expect("a");
    let oid_b = ObjectId::from_bytes(idx.oid_at(1)).expect("b");
    for oid in [oid_a, oid_b] {
        if let Some(pos) = lazy.find_position(&oid) {
            assert_eq!(lazy.oid_at(pos), oid.as_bytes());
        }
    }
}

#[test]
fn repack_with_cached_mmap_index_succeeds() {
    let _guard = test_pack_cache_guard();
    let repo = RepoFixture::init(grit_test_support::objects::HashAlgo::Sha1).expect("init");
    let mut builder = PackBuilder::new(grit_test_support::objects::HashAlgo::Sha1);
    for i in 0..32u8 {
        builder.add_full(FixtureKind::Blob, format!("blob-{i}").as_bytes());
    }
    let built = builder.build();
    let out = write_pack_and_index(
        &repo.objects_dir(),
        "repack-src",
        &built.bytes,
        grit_test_support::objects::HashAlgo::Sha1,
        &IndexPackOptions {
            index_version: Some(IndexVersion::V2),
            ..IndexPackOptions::default()
        },
    );
    clear_pack_cache();
    let idx_path = out.idx_path.expect("idx");
    let odb = Odb::new(repo.path());
    odb.with_pack_read_context(|| {
        read_pack_index_cached(&idx_path).expect("prime pack_cache index");
    });
    let grit_repo = Repository::open(&repo.path().join(".git"), &RepositoryOptions::default())
        .expect("open repo");
    repack_all(&grit_repo, &RepackOptions::default()).expect("repack");
    clear_pack_cache();
    git_fsck(repo.path());
}

fn git_fsck(repo: &Path) {
    let status = std::process::Command::new("git")
        .args(["-C", repo.to_str().expect("utf8"), "fsck"])
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .output()
        .expect("git fsck");
    assert!(
        status.status.success(),
        "git fsck after repack: {}",
        String::from_utf8_lossy(&status.stderr)
    );
}

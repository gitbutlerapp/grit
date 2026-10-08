//! Unit tests for pack fixtures and system-git cross-checks.

use super::{
    git_fsck, git_supports_sha256, git_verify_pack, hash_loose_object, write_loose_object,
    write_pack_and_index, DeltaOps, HashAlgo, IndexPackOptions, IndexVersion, ObjectKind,
    PackBuilder, PackOffsetLabel, RepoFixture,
};

fn run_algo(algo: HashAlgo, f: impl FnOnce(HashAlgo)) {
    if matches!(algo, HashAlgo::Sha256) && !git_supports_sha256() {
        eprintln!("SKIP: system git lacks sha256 object format");
        return;
    }
    f(algo);
}

#[test]
fn pack_builder_ofs_only_index_pack() {
    run_algo(HashAlgo::Sha1, |algo| {
        let base = b"ofs-only-base";
        let mut d = DeltaOps::new();
        d.header(base.len(), base.len() + 1);
        d.copy(0, base.len());
        d.insert(b"!");
        let delta = d.finish();
        let mut builder = PackBuilder::new(algo);
        let i = builder.add_full(ObjectKind::Blob, base);
        builder.add_ofs_delta(i, &delta, delta.len());
        let built = builder.build();
        let repo = RepoFixture::init(algo).expect("init");
        let outcome = write_pack_and_index(
            &repo.objects_dir(),
            "ofs-only",
            &built.bytes,
            algo,
            &IndexPackOptions::default(),
        );
        assert!(outcome.index_ok, "{}", outcome.index_stderr);
    });
}

#[test]
fn pack_builder_full_ofs_ref_deltas_index_and_verify() {
    run_algo(HashAlgo::Sha1, |algo| {
        pack_builder_full_ofs_ref_deltas_index_and_verify_inner(algo);
    });
    run_algo(HashAlgo::Sha256, |algo| {
        pack_builder_full_ofs_ref_deltas_index_and_verify_inner(algo);
    });
}

fn pack_builder_full_ofs_ref_deltas_index_and_verify_inner(algo: HashAlgo) {
    let base = b"pack-fixture-delta-base";
    let intermediate = b"pack-fixture-delta-base-ofs";
    let final_blob = b"pack-fixture-delta-base-ofs-ref";

    let mut ofs_delta = DeltaOps::new();
    ofs_delta.header(base.len(), intermediate.len());
    ofs_delta.copy(0, base.len());
    ofs_delta.insert(b"-ofs");
    let ofs_bytes = ofs_delta.finish();

    let mut ref_delta = DeltaOps::new();
    ref_delta.header(intermediate.len(), final_blob.len());
    ref_delta.copy(0, intermediate.len());
    ref_delta.insert(b"-ref");
    let ref_bytes = ref_delta.finish();

    let mut builder = PackBuilder::new(algo);
    let base_idx = builder.add_full(ObjectKind::Blob, base);
    builder.add_ofs_delta(base_idx, &ofs_bytes, ofs_bytes.len());

    let inter_oid = hash_loose_object(algo, "blob", intermediate);
    let inter_bytes = hex::decode(&inter_oid).expect("hex oid");
    builder.add_ref_delta(&inter_bytes, &ref_bytes, ref_bytes.len());

    let built = builder.build();
    let repo = RepoFixture::init(algo).expect("init repo");
    let outcome = write_pack_and_index(
        &repo.objects_dir(),
        "fixture",
        &built.bytes,
        algo,
        &IndexPackOptions::default(),
    );
    assert!(
        outcome.index_ok,
        "index-pack failed: {}",
        outcome.index_stderr
    );
    let pack_path = outcome.pack_path;
    let verify = git_verify_pack(repo.path(), &pack_path);
    assert!(verify.ok, "verify-pack failed: {}", verify.output);
}

#[test]
fn corrupted_trailer_rejected_by_index_pack() {
    run_algo(HashAlgo::Sha1, |algo| {
        let mut builder = PackBuilder::new(algo);
        builder.add_full(ObjectKind::Blob, b"trailer corruption probe");
        builder.set_corrupt_trailer();
        let built = builder.build();
        let repo = RepoFixture::init(algo).expect("init");
        let outcome = write_pack_and_index(
            &repo.objects_dir(),
            "bad-trailer",
            &built.bytes,
            algo,
            &IndexPackOptions::default(),
        );
        assert!(
            !outcome.index_ok,
            "expected index-pack to reject corrupt trailer"
        );
    });
}

#[test]
fn forced_large_offset_index_version() {
    run_algo(HashAlgo::Sha1, |algo| {
        let mut builder = PackBuilder::new(algo);
        builder.add_full(ObjectKind::Blob, b"x");
        let built = builder.build();
        let repo = RepoFixture::init(algo).expect("init");
        let opts = IndexPackOptions {
            rev_index: false,
            index_version: Some(IndexVersion::V2LargeOffsetAt(1)),
        };
        let outcome =
            write_pack_and_index(&repo.objects_dir(), "large-off", &built.bytes, algo, &opts);
        assert!(outcome.index_ok, "{}", outcome.index_stderr);
        let idx = std::fs::read(outcome.idx_path.expect("idx path")).expect("read idx");
        assert_eq!(&idx[0..4], b"\xfftOc");
        let version = u32::from_be_bytes([idx[4], idx[5], idx[6], idx[7]]);
        assert_eq!(version, 2);
        // v2 with low threshold uses 8-byte offset table entries (4-byte oid fanout + 4-byte crc + 8*n offsets minimum layout).
        let n = u32::from_be_bytes([
            built.bytes[8],
            built.bytes[9],
            built.bytes[10],
            built.bytes[11],
        ]);
        assert_eq!(n, 1);
        let oid_len = algo.oid_len();
        let header_len = 8 + 256 * 4 + oid_len + 4 * 1;
        assert!(
            idx.len() >= header_len + 8,
            "expected 64-bit offset extension in idx (len={})",
            idx.len()
        );
    });
}

#[test]
fn git_fsck_reports_bad_tree_msg_id() {
    run_algo(HashAlgo::Sha1, |algo| {
        git_fsck_reports_bad_tree_msg_id_inner(algo);
    });
    run_algo(HashAlgo::Sha256, |algo| {
        git_fsck_reports_bad_tree_msg_id_inner(algo);
    });
}

fn git_fsck_reports_bad_tree_msg_id_inner(algo: HashAlgo) {
    let repo = RepoFixture::init(algo).expect("init");
    let bad_tree = b"40000 not-a-valid-mode\x00name\x00";
    let _oid =
        write_loose_object(&repo.objects_dir(), algo, "tree", bad_tree).expect("write loose tree");

    let fsck = git_fsck(repo.path(), true);
    assert!(
        fsck.msg_ids.iter().any(|id| id == "badTree"),
        "expected badTree msg-id, got {:?} (ok={})",
        fsck.msg_ids,
        fsck.ok
    );
}

#[test]
fn pack_offset_labels_resolve() {
    let mut builder = PackBuilder::new(HashAlgo::Sha1);
    builder.add_full(ObjectKind::Blob, b"a");
    let built = builder.build();
    assert_eq!(
        built.resolve_offset(PackOffsetLabel::HeaderObjectCount),
        Some(8)
    );
    assert_eq!(
        built.resolve_offset(PackOffsetLabel::EntryHeader(0)),
        Some(12)
    );
    assert!(built.resolve_offset(PackOffsetLabel::Trailer).is_some());
}

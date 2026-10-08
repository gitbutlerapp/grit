//! Pack delta resolution and apply_delta scenarios (upstream t5303/t5309/t5314/t5316).

mod pack_delta_scenarios;

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use grit_lib::delta_encode::{encode_lcp_delta, encode_prefix_extension_delta};
use grit_lib::error::Error;
use grit_lib::objects::{ObjectId, ObjectKind};
use grit_lib::odb::Odb;
use grit_lib::pack::read_packed_delta_dependency;
use grit_lib::pack::{
    apply_delta_depth_limit, clear_pack_cache, max_verify_pack_delta_depth, packed_delta_base_oid,
    read_object_from_pack, read_object_from_packs, read_pack_index, verify_pack_and_collect,
    PackedDeltaDependency,
};
use grit_lib::unpack_objects::apply_delta;
use grit_test_support::objects::{
    hash_loose_object, write_pack_and_index, DeltaOps, HashAlgo, IndexPackOptions,
    ObjectKind as PackObjectKind, PackBuilder, RepoFixture,
};

use pack_delta_scenarios::{
    assert_delta_verdict, expect_corrupt_object, expect_delta_chain_limit, finish_synthetic_pack,
    install_hand_pack, lcp_delta, odb_at, run_algo, run_with_timeout, t5303_deltas,
    write_loose_blob,
};

// --- t5303 apply_delta accept/reject (bottom of script) ---

#[test]
fn t5303_apply_delta_verdicts_match_git_index_pack() {
    run_algo(HashAlgo::Sha1, |_| {
        assert_delta_verdict(b"", &t5303_deltas::minimal_good(), "minimal good delta");
        assert_delta_verdict(
            b"",
            &t5303_deltas::too_many_literal(),
            "too many literal bytes",
        );
        assert_delta_verdict(
            b"base",
            &t5303_deltas::too_many_copied(4),
            "too many copied bytes",
        );
        assert_delta_verdict(
            b"",
            &t5303_deltas::too_few_literal(),
            "too few literal bytes",
        );
        assert_delta_verdict(
            b"",
            &t5303_deltas::too_few_base_bytes(),
            "too few bytes in base",
        );
        assert_delta_verdict(
            b"base",
            &t5303_deltas::truncated_copy(),
            "truncated copy parameters",
        );
        assert_delta_verdict(
            b"",
            &t5303_deltas::trailing_garbage_literal(),
            "trailing garbage after literal",
        );
        assert_delta_verdict(
            b"base",
            &t5303_deltas::trailing_garbage_copy(),
            "trailing garbage after copy",
        );
        assert_delta_verdict(
            b"",
            &t5303_deltas::trailing_garbage_opcode(),
            "trailing garbage opcode",
        );
    });
}

#[test]
fn t5303_apply_delta_source_size_mismatch_is_rejected() {
    run_algo(HashAlgo::Sha1, |_| {
        let delta = t5303_deltas::source_size_mismatch();
        assert_delta_verdict(b"abc", &delta, "source size mismatch");
    });
}

// --- delta_encode round-trip via git index-pack ---

#[test]
fn delta_encode_lcp_and_prefix_extension_index_pack_with_git() {
    run_algo(HashAlgo::Sha1, |algo| {
        let base = b"shared prefix content";
        let via_lcp = b"shared prefix content extended";
        let via_ext = b"shared prefix content!!!";

        for (label, delta) in [
            ("lcp", encode_lcp_delta(base, via_lcp).expect("encode lcp")),
            (
                "prefix_extension",
                encode_prefix_extension_delta(base, via_ext).expect("encode ext"),
            ),
        ] {
            assert_eq!(
                apply_delta(base, &delta).expect("grit apply"),
                match label {
                    "lcp" => via_lcp.to_vec(),
                    _ => via_ext.to_vec(),
                }
            );
            let mut builder = PackBuilder::new(algo);
            let i = builder.add_full(PackObjectKind::Blob, base);
            let _ = i;
            let base_oid = hex::decode(hash_loose_object(algo, "blob", base)).unwrap();
            builder.add_ref_delta(&base_oid, &delta, delta.len());
            let built = builder.build();
            let repo = RepoFixture::init(algo).expect("init");
            let outcome = write_pack_and_index(
                &repo.objects_dir(),
                label,
                &built.bytes,
                algo,
                &IndexPackOptions::default(),
            );
            assert!(outcome.index_ok, "{}: {}", label, outcome.index_stderr);
        }
    });
}

// --- A<->B single ref deltas (t5309-style resolution) ---

#[test]
fn ref_delta_a_to_b_and_b_to_a_single_deltas() {
    run_algo(HashAlgo::Sha1, |algo| {
        clear_pack_cache();
        let a = b"blob-payload-aaa";
        let b = b"blob-payload-bbb";
        let oid_a = ObjectId::from_hex(&hash_loose_object(algo, "blob", a)).unwrap();
        let oid_b = ObjectId::from_hex(&hash_loose_object(algo, "blob", b)).unwrap();
        let delta_a = encode_lcp_delta(b, a).unwrap();
        let delta_b = encode_lcp_delta(a, b).unwrap();

        let repo = RepoFixture::init(algo).expect("init");
        let objects = repo.objects_dir();

        let mut to_a = PackBuilder::new(algo);
        to_a.add_full(PackObjectKind::Blob, b);
        to_a.add_ref_delta(oid_b.as_bytes(), &delta_a, delta_a.len());
        let built_a = to_a.build();
        finish_synthetic_pack(
            &objects,
            "a-from-b",
            algo,
            &built_a,
            &[(oid_b, 0), (oid_a, 1)],
        );

        let mut to_b = PackBuilder::new(algo);
        to_b.add_full(PackObjectKind::Blob, a);
        to_b.add_ref_delta(oid_a.as_bytes(), &delta_b, delta_b.len());
        let built_b = to_b.build();
        finish_synthetic_pack(
            &objects,
            "b-from-a",
            algo,
            &built_b,
            &[(oid_a, 0), (oid_b, 1)],
        );

        let odb = odb_at(&objects);
        assert_eq!(odb.read(&oid_a).expect("read A").data, a);
        assert_eq!(odb.read(&oid_b).expect("read B").data, b);
    });
}

// --- missing REF base ---

#[test]
fn ref_delta_missing_base_returns_object_not_found() {
    run_algo(HashAlgo::Sha1, |algo| {
        clear_pack_cache();
        let base = b"only-in-delta-header";
        let target = b"only-in-delta-header!";
        let missing = ObjectId::from_hex("0101010101010101010101010101010101010101").unwrap();
        let delta = encode_lcp_delta(base, target).unwrap();
        let oid_tip = ObjectId::from_hex(&hash_loose_object(algo, "blob", target)).unwrap();

        let repo = RepoFixture::init(algo).expect("init");
        let objects = repo.objects_dir();
        let mut builder = PackBuilder::new(algo);
        builder.add_ref_delta(missing.as_bytes(), &delta, delta.len());
        let built = builder.build();
        finish_synthetic_pack(&objects, "thin", algo, &built, &[(oid_tip, 0)]);

        let err = odb_at(&objects)
            .read(&oid_tip)
            .expect_err("missing ref base");
        expect_corrupt_object(err);
    });
}

// --- REF cycle and self-reference (t5309/t5314) with timeout guard ---

#[test]
fn ref_delta_self_reference_errors_without_hanging() {
    run_algo(HashAlgo::Sha1, |algo| {
        clear_pack_cache();
        let content = b"self-ref";
        let oid = ObjectId::from_hex(&hash_loose_object(algo, "blob", content)).unwrap();
        let delta = encode_lcp_delta(content, b"self-ref!").unwrap();

        let repo = RepoFixture::init(algo).expect("init");
        let objects = repo.objects_dir();
        let mut builder = PackBuilder::new(algo);
        builder.add_ref_delta(oid.as_bytes(), &delta, delta.len());
        let built = builder.build();
        let pack_path = finish_synthetic_pack(&objects, "self", algo, &built, &[(oid, 0)]);
        let idx = read_pack_index(&pack_path.with_extension("idx")).expect("idx");

        let err = run_with_timeout("self-ref read", move || {
            read_object_from_pack(&idx, &oid).expect_err("self ref")
        });
        expect_delta_chain_limit(err);
    });
}

#[test]
fn ref_delta_two_object_cycle_errors_without_hanging() {
    run_algo(HashAlgo::Sha1, |algo| {
        clear_pack_cache();
        let a = b"aaa";
        let b = b"bbb";
        let oid_a = ObjectId::from_hex(&hash_loose_object(algo, "blob", a)).unwrap();
        let oid_b = ObjectId::from_hex(&hash_loose_object(algo, "blob", b)).unwrap();
        let delta_a = encode_lcp_delta(b, a).unwrap();
        let delta_b = encode_lcp_delta(a, b).unwrap();

        let repo = RepoFixture::init(algo).expect("init");
        let objects = repo.objects_dir();
        let mut builder = PackBuilder::new(algo);
        builder.add_ref_delta(oid_b.as_bytes(), &delta_a, delta_a.len());
        builder.add_ref_delta(oid_a.as_bytes(), &delta_b, delta_b.len());
        let built = builder.build();
        let pack_path =
            finish_synthetic_pack(&objects, "cycle", algo, &built, &[(oid_a, 0), (oid_b, 1)]);
        let idx = read_pack_index(&pack_path.with_extension("idx")).expect("idx");

        let err = run_with_timeout("in-pack cycle", move || {
            read_object_from_pack(&idx, &oid_a).expect_err("cycle")
        });
        expect_delta_chain_limit(err);
    });
}

#[test]
fn cross_pack_ref_delta_cycle_errors_without_hanging() {
    run_algo(HashAlgo::Sha1, |algo| {
        clear_pack_cache();
        let a = b"aaa";
        let b = b"bbb";
        let oid_a = ObjectId::from_hex(&hash_loose_object(algo, "blob", a)).unwrap();
        let oid_b = ObjectId::from_hex(&hash_loose_object(algo, "blob", b)).unwrap();
        let delta_a = encode_lcp_delta(b, a).unwrap();
        let delta_b = encode_lcp_delta(a, b).unwrap();

        let repo = RepoFixture::init(algo).expect("init");
        let objects = repo.objects_dir();
        let mut ba = PackBuilder::new(algo);
        ba.add_ref_delta(oid_b.as_bytes(), &delta_a, delta_a.len());
        let built_a = ba.build();
        finish_synthetic_pack(&objects, "cycle-a", algo, &built_a, &[(oid_a, 0)]);

        let mut ab = PackBuilder::new(algo);
        ab.add_ref_delta(oid_a.as_bytes(), &delta_b, delta_b.len());
        let built_b = ab.build();
        finish_synthetic_pack(&objects, "cycle-b", algo, &built_b, &[(oid_b, 0)]);
        let idx_a = read_pack_index(&objects.join("pack").join("cycle-a.idx")).expect("idx a");
        let err = run_with_timeout("cross-pack cycle", move || {
            read_object_from_pack(&idx_a, &oid_a).expect_err("cross-pack cycle")
        });
        expect_delta_chain_limit(err);
    });
}

// --- cross-pack base and duplicate entry in same pack ---

#[test]
fn ref_delta_base_in_other_pack() {
    run_algo(HashAlgo::Sha1, |algo| {
        clear_pack_cache();
        let base = b"cross-pack-base";
        let tip = b"cross-pack-base!";
        let oid_base = ObjectId::from_hex(&hash_loose_object(algo, "blob", base)).unwrap();
        let oid_tip = ObjectId::from_hex(&hash_loose_object(algo, "blob", tip)).unwrap();
        let delta = encode_lcp_delta(base, tip).unwrap();

        let repo = RepoFixture::init(algo).expect("init");
        let objects = repo.objects_dir();
        install_hand_pack(&objects, "base-pack", algo, |builder| {
            builder.add_full(PackObjectKind::Blob, base);
        });
        let mut tip_b = PackBuilder::new(algo);
        tip_b.add_ref_delta(oid_base.as_bytes(), &delta, delta.len());
        let built_tip = tip_b.build();
        finish_synthetic_pack(&objects, "tip-pack", algo, &built_tip, &[(oid_tip, 0)]);

        let got = read_object_from_packs(&objects, &oid_tip).expect("cross-pack read");
        assert_eq!(got.data, tip);
        assert_eq!(
            packed_delta_base_oid(&objects, &oid_tip)
                .expect("base oid lookup")
                .expect("some base"),
            oid_base
        );
    });
}

/// Same pack holds a full base object and a ref-delta tip; the index maps the base OID to the
/// full copy so resolution stays in-pack (t5309/t5314 fallback) rather than needing another pack.
#[test]
fn ref_delta_full_base_duplicate_in_same_pack_enables_read() {
    run_algo(HashAlgo::Sha1, |algo| {
        clear_pack_cache();
        let a = b"aaa";
        let b = b"bbb";
        let oid_a = ObjectId::from_hex(&hash_loose_object(algo, "blob", a)).unwrap();
        let oid_b = ObjectId::from_hex(&hash_loose_object(algo, "blob", b)).unwrap();
        let delta_a = encode_lcp_delta(b, a).unwrap();
        let delta_b = encode_lcp_delta(a, b).unwrap();

        let repo = RepoFixture::init(algo).expect("init");
        let objects = repo.objects_dir();

        let mut with_full_base = PackBuilder::new(algo);
        let full_b = with_full_base.add_full(PackObjectKind::Blob, b);
        with_full_base.add_ref_delta(oid_b.as_bytes(), &delta_a, delta_a.len());
        let built = with_full_base.build();
        finish_synthetic_pack(
            &objects,
            "full-plus-delta",
            algo,
            &built,
            &[(oid_a, 1), (oid_b, full_b)],
        );

        let got_a = read_object_from_packs(&objects, &oid_a).expect("read A via in-pack full B");
        assert_eq!(got_a.data, a);
        let got_b = read_object_from_packs(&objects, &oid_b).expect("read in-pack full B");
        assert_eq!(got_b.data, b);

        // Thin pack alone (separate repo): only the ref-delta slot — must fail without a base pack.
        clear_pack_cache();
        let thin_repo = RepoFixture::init(algo).expect("thin repo");
        let thin_objects = thin_repo.objects_dir();
        let mut thin = PackBuilder::new(algo);
        thin.add_ref_delta(oid_b.as_bytes(), &delta_a, delta_a.len());
        let thin_built = thin.build();
        finish_synthetic_pack(&thin_objects, "thin-only", algo, &thin_built, &[(oid_a, 0)]);
        assert!(
            read_object_from_packs(&thin_objects, &oid_a).is_err(),
            "ref-delta without in-pack or cross-pack base must fail"
        );

        // Indexed two-object ref-delta cycle (no full duplicate): must hit depth limit.
        clear_pack_cache();
        let cycle_repo = RepoFixture::init(algo).expect("cycle repo");
        let cycle_objects = cycle_repo.objects_dir();
        let mut cyclic = PackBuilder::new(algo);
        cyclic.add_ref_delta(oid_b.as_bytes(), &delta_a, delta_a.len());
        cyclic.add_ref_delta(oid_a.as_bytes(), &delta_b, delta_b.len());
        let cyclic_built = cyclic.build();
        finish_synthetic_pack(
            &cycle_objects,
            "pure-cycle",
            algo,
            &cyclic_built,
            &[(oid_a, 0), (oid_b, 1)],
        );
        let idx = read_pack_index(&cycle_objects.join("pack").join("pure-cycle.idx")).expect("idx");
        let err = read_object_from_pack(&idx, &oid_a).expect_err("pure cycle must not resolve");
        expect_delta_chain_limit(err);
    });
}

// --- thin pack A->B->C with B on disk ---

#[test]
fn thin_chain_a_to_b_to_c_with_intermediate_on_disk() {
    run_algo(HashAlgo::Sha1, |algo| {
        clear_pack_cache();
        let a = b"chain-base-aaa";
        let b = b"chain-base-bbb";
        let c = b"chain-base-ccc";
        let oid_a = ObjectId::from_hex(&hash_loose_object(algo, "blob", a)).unwrap();
        let oid_b = ObjectId::from_hex(&hash_loose_object(algo, "blob", b)).unwrap();
        let oid_c = ObjectId::from_hex(&hash_loose_object(algo, "blob", c)).unwrap();
        let delta_b = encode_lcp_delta(a, b).unwrap();
        let delta_c = encode_lcp_delta(b, c).unwrap();

        let repo = RepoFixture::init(algo).expect("init");
        let objects = repo.objects_dir();
        write_loose_blob(&objects, algo, a);
        write_loose_blob(&objects, algo, b);

        let mut thin = PackBuilder::new(algo);
        thin.add_ref_delta(oid_a.as_bytes(), &delta_b, delta_b.len());
        thin.add_ref_delta(oid_b.as_bytes(), &delta_c, delta_c.len());
        let built = thin.build();
        finish_synthetic_pack(&objects, "thin", algo, &built, &[(oid_b, 0), (oid_c, 1)]);

        let odb = odb_at(&objects);
        assert_eq!(odb.read(&oid_c).expect("read C").data, c);
    });
}

// --- deep OFS chain depth 50+ (t5316) ---

fn build_ofs_chain_pack(depth: usize) -> Option<(tempfile::TempDir, PathBuf, ObjectId, Vec<u8>)> {
    if depth == 0 {
        return None;
    }
    let tmp = tempfile::tempdir().ok()?;
    let objects = tmp.path().join("objects");
    std::fs::create_dir_all(objects.join("pack")).ok()?;
    let odb = Odb::new(&objects);
    let mut builder = PackBuilder::new(HashAlgo::Sha1);
    let mut content = b"ofs-chain-base\n".to_vec();
    let mut base_idx = builder.add_full(PackObjectKind::Blob, &content);
    for _ in 0..depth {
        let mut next = content.clone();
        next.push(b'!');
        let delta = lcp_delta(&content, &next);
        base_idx = builder.add_ofs_delta(base_idx, &delta, delta.len());
        content = next;
    }
    let built = builder.build();
    let tip = odb.hash(ObjectKind::Blob, &content);
    let outcome = write_pack_and_index(
        &objects,
        "chain",
        &built.bytes,
        HashAlgo::Sha1,
        &IndexPackOptions::default(),
    );
    if !outcome.index_ok {
        return None;
    }
    Some((tmp, outcome.pack_path, tip, content))
}

#[test]
fn t5316_deep_ofs_chain_depth_55_matches_verify_pack_and_read() {
    run_algo(HashAlgo::Sha1, |_| {
        clear_pack_cache();
        let Some((_tmp, pack_path, tip, expected)) = build_ofs_chain_pack(55) else {
            panic!("failed to build depth-55 pack");
        };
        let idx_path = pack_path.with_extension("idx");
        let records = verify_pack_and_collect(&idx_path).expect("verify-pack");
        let max_depth = max_verify_pack_delta_depth(&records);
        assert!(
            max_depth >= 55,
            "expected depth >= 55, verify-pack max {max_depth}"
        );
        let idx = read_pack_index(&idx_path).expect("read idx");
        let got = read_object_from_pack(&idx, &tip).expect("read tip");
        assert_eq!(got.data, expected);

        let mut map: HashMap<ObjectId, ObjectId> = HashMap::new();
        for rec in &records {
            if let Some(base) = &rec.base_oid {
                if base.len() == 20 {
                    let t = ObjectId::from_bytes(rec.oid.as_slice()).unwrap();
                    let b = ObjectId::from_bytes(base.as_slice()).unwrap();
                    map.insert(t, b);
                }
            }
        }
        apply_delta_depth_limit(&mut map, 50);
        let max_edges = max_delta_dependency_chain_edges(&map);
        assert!(
            max_edges <= 50,
            "after depth limit, longest dependency chain must be <= 50 edges (got {max_edges})"
        );
    });
}

fn max_delta_dependency_chain_edges(map: &HashMap<ObjectId, ObjectId>) -> usize {
    let value_set: std::collections::HashSet<ObjectId> = map.values().copied().collect();
    let tips: Vec<ObjectId> = map
        .keys()
        .copied()
        .filter(|k| !value_set.contains(k))
        .collect();
    let mut max_edges = 0usize;
    for tip in tips {
        let mut edges = 0usize;
        let mut cur = tip;
        let mut seen = std::collections::HashSet::new();
        while seen.insert(cur) {
            let Some(&base) = map.get(&cur) else {
                break;
            };
            edges += 1;
            cur = base;
        }
        max_edges = max_edges.max(edges);
    }
    max_edges
}

#[test]
fn read_packed_delta_dependency_reports_ofs_and_ref() {
    run_algo(HashAlgo::Sha1, |algo| {
        clear_pack_cache();
        let base = b"dep-base";
        let tip = b"dep-base-x";
        let delta = encode_lcp_delta(base, tip).unwrap();
        let oid_base = ObjectId::from_hex(&hash_loose_object(algo, "blob", base)).unwrap();

        let repo = RepoFixture::init(algo).expect("init");
        let objects = repo.objects_dir();
        let pack_path = install_hand_pack(&objects, "dep", algo, |builder| {
            let i = builder.add_full(PackObjectKind::Blob, base);
            builder.add_ofs_delta(i, &delta, delta.len());
        });
        let pack_bytes = std::fs::read(&pack_path).expect("read pack");
        let idx = read_pack_index(&pack_path.with_extension("idx")).expect("idx");
        let tip_entry = idx.iter().last().expect("tip entry");
        let dep = read_packed_delta_dependency(&pack_bytes, tip_entry.offset())
            .expect("parse dep")
            .expect("delta dep");
        assert!(matches!(dep, PackedDeltaDependency::OfsBase { .. }));

        let pack_path2 = install_hand_pack(&objects, "dep-ref", algo, |builder| {
            builder.add_full(PackObjectKind::Blob, base);
            builder.add_ref_delta(oid_base.as_bytes(), &delta, delta.len());
        });
        let pack_bytes2 = std::fs::read(&pack_path2).expect("read pack2");
        let idx2 = read_pack_index(&pack_path2.with_extension("idx")).expect("idx2");
        let tip2 = idx2.iter().last().expect("tip2");
        let dep2 = read_packed_delta_dependency(&pack_bytes2, tip2.offset())
            .expect("parse ref dep")
            .expect("ref dep");
        assert!(matches!(dep2, PackedDeltaDependency::RefBase { .. }));
    });
}

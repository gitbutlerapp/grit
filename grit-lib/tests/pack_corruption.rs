//! Pack corruption resilience (upstream t5303).

mod pack_delta_scenarios;

use grit_lib::objects::{ObjectId, ObjectKind};
use grit_lib::odb::Odb;
use grit_lib::pack::{
    clear_pack_cache, packed_delta_base_oid, packed_full_object_slice, read_object_from_packs,
    read_packed_delta_dependency, PackedDeltaDependency,
};
use grit_test_support::objects::{
    flip_byte_at, hash_loose_object, overwrite_range, DeltaOps, HashAlgo,
    ObjectKind as PackObjectKind, PackBuilder, RepoFixture,
};

use pack_delta_scenarios::{
    finish_synthetic_pack, install_hand_pack, odb_at, run_algo, write_loose_blob,
};

/// Three-blob chain: blob1, blob2 delta on blob1, blob3 delta on blob2 (depth 2).
struct T5303Pack {
    repo: RepoFixture,
    blob1: ObjectId,
    blob2: ObjectId,
    blob3: ObjectId,
    pack_path: std::path::PathBuf,
    file1: Vec<u8>,
    file2: Vec<u8>,
    file3: Vec<u8>,
    ref_deltas: bool,
}

impl T5303Pack {
    fn build_ofs(algo: HashAlgo) -> Self {
        Self::build_inner(algo, false)
    }

    fn build_ref(algo: HashAlgo) -> Self {
        Self::build_inner(algo, true)
    }

    fn build_inner(algo: HashAlgo, ref_deltas: bool) -> Self {
        let mut file1 = vec![0u8; 2000];
        let mut file2 = vec![0u8; 1800];
        let mut file3 = vec![0u8; 1800];
        for (i, b) in file1.iter_mut().enumerate() {
            *b = (i % 251) as u8;
        }
        file2.copy_from_slice(&file1[..1800]);
        file3.copy_from_slice(&file1[..1800]);
        file1.extend_from_slice(b" base ");
        file2.extend_from_slice(b" delta1 ");
        file3.extend_from_slice(b" delta delta2 ");
        file2.extend_from_slice(&[0u8; 150]);
        file3.extend_from_slice(&[0u8; 100]);

        let blob1 = ObjectId::from_hex(&hash_loose_object(algo, "blob", &file1)).unwrap();
        let blob2 = ObjectId::from_hex(&hash_loose_object(algo, "blob", &file2)).unwrap();
        let blob3 = ObjectId::from_hex(&hash_loose_object(algo, "blob", &file3)).unwrap();

        let delta2 = delta_between(&file1, &file2);
        let delta3 = delta_between(&file2, &file3);

        let repo = RepoFixture::init(algo).expect("init");
        let objects = repo.objects_dir();
        let stem = if ref_deltas { "t5303-ref" } else { "t5303-ofs" };
        let pack_path = install_hand_pack(&objects, stem, algo, |builder| {
            builder.add_full(PackObjectKind::Blob, &file1);
            if ref_deltas {
                builder.add_ref_delta(blob1.as_bytes(), &delta2, delta2.len());
                builder.add_ref_delta(blob2.as_bytes(), &delta3, delta3.len());
            } else {
                builder.add_ofs_delta(0, &delta2, delta2.len());
                builder.add_ofs_delta(1, &delta3, delta3.len());
            }
        });

        Self {
            repo,
            blob1,
            blob2,
            blob3,
            pack_path,
            file1,
            file2,
            file3,
            ref_deltas,
        }
    }

    fn objects(&self) -> std::path::PathBuf {
        self.repo.objects_dir()
    }
}

fn after_object_header(pack: &[u8], offset: u64) -> usize {
    let mut p = offset as usize;
    let first = *pack.get(p).expect("header byte");
    let type_code = (first >> 4) & 0x7;
    let mut c = first;
    p += 1;
    while c & 0x80 != 0 {
        c = *pack.get(p).expect("size cont");
        p += 1;
    }
    if type_code == 6 {
        while pack[p] & 0x80 != 0 {
            p += 1;
        }
        p += 1;
    } else if type_code == 7 {
        p += 20;
    }
    p
}

fn ref_delta_base_oid_offset(pack: &[u8], offset: u64) -> u64 {
    let mut p = offset as usize;
    let first = *pack.get(p).expect("header byte");
    let type_code = (first >> 4) & 0x7;
    assert_eq!(type_code, 7, "expected REF_DELTA");
    let mut c = first;
    p += 1;
    while c & 0x80 != 0 {
        c = *pack.get(p).expect("size cont");
        p += 1;
    }
    u64::try_from(p).expect("ref base oid offset")
}

fn delta_zlib_payload_offset(pack: &[u8], offset: u64) -> u64 {
    u64::try_from(after_object_header(pack, offset)).expect("zlib start")
}

fn ofs_delta_distance_offset(pack: &[u8], offset: u64) -> u64 {
    let mut p = offset as usize;
    let first = *pack.get(p).expect("header byte");
    let mut c = first;
    p += 1;
    while c & 0x80 != 0 {
        c = *pack.get(p).expect("size cont");
        p += 1;
    }
    u64::try_from(p).expect("ofs distance offset")
}

fn install_redundant_t5303_chain(
    objects: &std::path::Path,
    algo: HashAlgo,
    fx: &T5303Pack,
    stem: &str,
) {
    let mut redundant = PackBuilder::new(algo);
    redundant.add_full(PackObjectKind::Blob, &fx.file1);
    let d2 = delta_between(&fx.file1, &fx.file2);
    if fx.ref_deltas {
        redundant.add_ref_delta(fx.blob1.as_bytes(), &d2, d2.len());
        let d3 = delta_between(&fx.file2, &fx.file3);
        redundant.add_ref_delta(fx.blob2.as_bytes(), &d3, d3.len());
    } else {
        redundant.add_ofs_delta(0, &d2, d2.len());
        let d3 = delta_between(&fx.file2, &fx.file3);
        redundant.add_ofs_delta(1, &d3, d3.len());
    }
    let built = redundant.build();
    finish_synthetic_pack(
        objects,
        stem,
        algo,
        &built,
        &[(fx.blob1, 0), (fx.blob2, 1), (fx.blob3, 2)],
    );
}

fn delta_between(base: &[u8], target: &[u8]) -> Vec<u8> {
    let mut d = DeltaOps::new();
    d.header(base.len(), target.len());
    let lcp = base
        .iter()
        .zip(target.iter())
        .take_while(|(a, b)| a == b)
        .count();
    d.copy(0, lcp);
    if target.len() > lcp {
        d.insert(&target[lcp..]);
    }
    d.finish()
}

fn assert_read_fails(odb: &Odb, oid: &ObjectId) {
    assert!(
        odb.read(oid).is_err(),
        "expected read failure for {}",
        oid.to_hex()
    );
}

fn assert_read_ok(odb: &Odb, oid: &ObjectId, expected: &[u8]) {
    let obj = odb.read(oid).expect("read");
    assert_eq!(obj.kind, ObjectKind::Blob);
    assert_eq!(obj.data, expected);
}

fn assert_t5303_chain_metadata(fx: &T5303Pack) {
    let odb = odb_at(&fx.objects());
    assert_read_ok(&odb, &fx.blob1, &fx.file1);
    assert_read_ok(&odb, &fx.blob2, &fx.file2);
    assert_read_ok(&odb, &fx.blob3, &fx.file3);

    let pack_bytes = std::fs::read(&fx.pack_path).expect("read pack");
    let idx = grit_lib::pack::read_pack_index(&fx.pack_path.with_extension("idx")).expect("idx");
    for entry in idx.iter() {
        let oid = ObjectId::from_bytes(entry.oid()).expect("oid");
        if oid == fx.blob1 {
            continue;
        }
        let off = entry.offset();
        let dep = read_packed_delta_dependency(&pack_bytes, off)
            .expect("dep parse")
            .expect("delta");
        if fx.ref_deltas {
            assert!(
                matches!(dep, PackedDeltaDependency::RefBase { .. }),
                "REF pack delta must report RefBase"
            );
        } else {
            assert!(
                matches!(dep, PackedDeltaDependency::OfsBase { .. }),
                "OFS pack delta must report OfsBase"
            );
        }
    }
    assert!(packed_full_object_slice(&fx.objects(), &fx.blob1)
        .expect("slice lookup")
        .is_some());
}

#[test]
fn t5303_initial_chain_reads_and_dependency_metadata_ofs_and_ref() {
    run_algo(HashAlgo::Sha1, |algo| {
        for ref_deltas in [false, true] {
            clear_pack_cache();
            let fx = if ref_deltas {
                T5303Pack::build_ref(algo)
            } else {
                T5303Pack::build_ofs(algo)
            };
            assert_t5303_chain_metadata(&fx);
        }
    });
}

#[test]
fn t5303_corrupt_first_object_header_then_recover_via_loose() {
    run_algo(HashAlgo::Sha1, |algo| {
        clear_pack_cache();
        let fx = T5303Pack::build_ofs(algo);
        flip_byte_at(&fx.pack_path, 12).expect("corrupt header");
        clear_pack_cache();
        let odb = odb_at(&fx.objects());
        assert_read_fails(&odb, &fx.blob1);
        assert_read_fails(&odb, &fx.blob2);
        assert_read_fails(&odb, &fx.blob3);

        write_loose_blob(&fx.objects(), algo, &fx.file1);
        clear_pack_cache();
        assert_read_ok(&odb, &fx.blob1, &fx.file1);
        assert_read_ok(&odb, &fx.blob2, &fx.file2);
        assert_read_ok(&odb, &fx.blob3, &fx.file3);
    });
}

#[test]
fn t5303_corrupt_first_object_data_then_recover_via_loose() {
    run_algo(HashAlgo::Sha1, |algo| {
        clear_pack_cache();
        let fx = T5303Pack::build_ofs(algo);
        let idx =
            grit_lib::pack::read_pack_index(&fx.pack_path.with_extension("idx")).expect("idx");
        let off1 = idx.find_offset(&fx.blob1).expect("blob1 off");
        flip_byte_at(&fx.pack_path, off1 + 4).expect("corrupt blob1 zlib payload");
        clear_pack_cache();
        let odb = odb_at(&fx.objects());
        assert_read_fails(&odb, &fx.blob1);

        write_loose_blob(&fx.objects(), algo, &fx.file1);
        clear_pack_cache();
        assert_read_ok(&odb, &fx.blob1, &fx.file1);
        assert_read_ok(&odb, &fx.blob2, &fx.file2);
    });
}

#[test]
fn t5303_corrupt_first_delta_zlib_then_recover_via_loose() {
    run_algo(HashAlgo::Sha1, |algo| {
        for ref_deltas in [false, true] {
            clear_pack_cache();
            let fx = if ref_deltas {
                T5303Pack::build_ref(algo)
            } else {
                T5303Pack::build_ofs(algo)
            };
            let idx =
                grit_lib::pack::read_pack_index(&fx.pack_path.with_extension("idx")).expect("idx");
            let pack = std::fs::read(&fx.pack_path).expect("read pack");
            let off2 = idx.find_offset(&fx.blob2).expect("blob2 off");
            let zlib = delta_zlib_payload_offset(&pack, off2);
            flip_byte_at(&fx.pack_path, zlib + 2).expect("corrupt delta zlib");
            clear_pack_cache();
            let odb = odb_at(&fx.objects());
            assert_read_fails(&odb, &fx.blob2);

            write_loose_blob(&fx.objects(), algo, &fx.file2);
            install_redundant_t5303_chain(&fx.objects(), algo, &fx, "redundant-zlib");
            clear_pack_cache();
            let got2 = read_object_from_packs(&fx.objects(), &fx.blob2).expect("recover blob2");
            assert_eq!(got2.data, fx.file2);
            let got3 = read_object_from_packs(&fx.objects(), &fx.blob3).expect("recover blob3");
            assert_eq!(got3.data, fx.file3);
        }
    });
}

#[test]
fn t5303_corrupt_ref_delta_base_oid_then_recover() {
    run_algo(HashAlgo::Sha1, |algo| {
        clear_pack_cache();
        let fx = T5303Pack::build_ref(algo);
        let idx =
            grit_lib::pack::read_pack_index(&fx.pack_path.with_extension("idx")).expect("idx");
        let pack = std::fs::read(&fx.pack_path).expect("read pack");
        let off3 = idx.find_offset(&fx.blob3).expect("blob3 off");
        let base_oid_off = ref_delta_base_oid_offset(&pack, off3);
        flip_byte_at(&fx.pack_path, base_oid_off).expect("flip ref base oid byte");
        flip_byte_at(&fx.pack_path, base_oid_off + 1).expect("flip ref base oid byte 2");
        clear_pack_cache();
        let odb = odb_at(&fx.objects());
        assert_read_fails(&odb, &fx.blob3);

        write_loose_blob(&fx.objects(), algo, &fx.file3);
        install_redundant_t5303_chain(&fx.objects(), algo, &fx, "redundant-ref-base");
        clear_pack_cache();
        let got = read_object_from_packs(&fx.objects(), &fx.blob3).expect("recover blob3");
        assert_eq!(got.data, fx.file3);
        let _ = odb;
    });
}

#[test]
fn t5303_corrupt_first_delta_header_then_recover_partial_via_loose_blob2() {
    run_algo(HashAlgo::Sha1, |algo| {
        clear_pack_cache();
        let fx = T5303Pack::build_ofs(algo);
        let idx =
            grit_lib::pack::read_pack_index(&fx.pack_path.with_extension("idx")).expect("idx");
        let off2 = idx.find_offset(&fx.blob2).expect("blob2 off");
        let end3 = idx
            .find_offset(&fx.blob3)
            .map(|o| o as usize)
            .unwrap_or_else(|| std::fs::metadata(&fx.pack_path).unwrap().len() as usize - 20);
        for pos in (off2 as usize)..end3.min((off2 as usize) + 64) {
            flip_byte_at(&fx.pack_path, pos as u64).ok();
        }
        clear_pack_cache();
        let odb = odb_at(&fx.objects());
        assert_read_fails(&odb, &fx.blob2);

        write_loose_blob(&fx.objects(), algo, &fx.file2);
        clear_pack_cache();
        assert_read_ok(&odb, &fx.blob2, &fx.file2);
        assert_read_ok(&odb, &fx.blob3, &fx.file3);
    });
}

#[test]
fn t5303_corrupt_ofs_delta_distance_variants_then_recover() {
    run_algo(HashAlgo::Sha1, |algo| {
        for (label, byte) in [("zero", 0u8), ("nonzero", 0x01u8)] {
            clear_pack_cache();
            let fx = T5303Pack::build_ofs(algo);
            let idx =
                grit_lib::pack::read_pack_index(&fx.pack_path.with_extension("idx")).expect("idx");
            let pack = std::fs::read(&fx.pack_path).expect("read pack");
            let off2 = idx.find_offset(&fx.blob2).expect("blob2 off");
            let dist = ofs_delta_distance_offset(&pack, off2);
            if byte == 0 {
                flip_byte_at(&fx.pack_path, dist).expect("corrupt ofs distance");
            } else {
                overwrite_range(&fx.pack_path, dist, &[byte]).expect("corrupt ofs distance");
            }
            clear_pack_cache();
            let odb = odb_at(&fx.objects());
            assert_read_fails(&odb, &fx.blob2);

            let mut redundant = PackBuilder::new(algo);
            redundant.add_full(PackObjectKind::Blob, &fx.file1);
            let d2 = delta_between(&fx.file1, &fx.file2);
            redundant.add_ofs_delta(0, &d2, d2.len());
            let d3 = delta_between(&fx.file2, &fx.file3);
            redundant.add_ofs_delta(1, &d3, d3.len());
            let built = redundant.build();
            finish_synthetic_pack(
                &fx.objects(),
                "redundant-ofs",
                algo,
                &built,
                &[(fx.blob1, 0), (fx.blob2, 1), (fx.blob3, 2)],
            );
            clear_pack_cache();
            let got2 = read_object_from_packs(&fx.objects(), &fx.blob2).expect("recover blob2");
            assert_eq!(got2.data, fx.file2);
            let got3 = read_object_from_packs(&fx.objects(), &fx.blob3).expect("recover blob3");
            assert_eq!(got3.data, fx.file3);
            let _ = label;
        }
    });
}

#[test]
fn t5303_corrupt_delta_base_to_wrong_object_then_recover_via_loose() {
    run_algo(HashAlgo::Sha1, |algo| {
        clear_pack_cache();
        let fx = T5303Pack::build_ofs(algo);
        let idx =
            grit_lib::pack::read_pack_index(&fx.pack_path.with_extension("idx")).expect("idx");
        let pack = std::fs::read(&fx.pack_path).expect("read pack");
        let off3 = idx.find_offset(&fx.blob3).expect("blob3 off");
        let dist = ofs_delta_distance_offset(&pack, off3);
        overwrite_range(&fx.pack_path, dist, &[0x20, 0x33]).expect("wrong base ofs");
        clear_pack_cache();
        let odb = odb_at(&fx.objects());
        assert_read_fails(&odb, &fx.blob3);

        let mut redundant = PackBuilder::new(algo);
        redundant.add_full(PackObjectKind::Blob, &fx.file1);
        let d2 = delta_between(&fx.file1, &fx.file2);
        redundant.add_ofs_delta(0, &d2, d2.len());
        let d3 = delta_between(&fx.file2, &fx.file3);
        redundant.add_ofs_delta(1, &d3, d3.len());
        let built = redundant.build();
        finish_synthetic_pack(
            &fx.objects(),
            "redundant-b3",
            algo,
            &built,
            &[(fx.blob1, 0), (fx.blob2, 1), (fx.blob3, 2)],
        );
        clear_pack_cache();
        let got = read_object_from_packs(&fx.objects(), &fx.blob3).expect("recover blob3");
        assert_eq!(got.data, fx.file3);
        let _ = odb;
    });
}

#[test]
fn t5303_redundant_pack_recovers_when_primary_corrupt() {
    run_algo(HashAlgo::Sha1, |algo| {
        clear_pack_cache();
        let fx = T5303Pack::build_ofs(algo);
        let primary = fx.pack_path.clone();
        let objects = fx.objects();

        // Good redundant pack with blob1 and blob2 only.
        install_hand_pack(&objects, "redundant", algo, |builder| {
            builder.add_full(PackObjectKind::Blob, &fx.file1);
            let d2 = delta_between(&fx.file1, &fx.file2);
            builder.add_ofs_delta(0, &d2, d2.len());
        });

        let idx = grit_lib::pack::read_pack_index(&primary.with_extension("idx")).expect("idx");
        let off2 = idx.find_offset(&fx.blob2).expect("off2");
        flip_byte_at(&primary, off2).expect("break primary delta");
        clear_pack_cache();

        let odb = odb_at(&objects);
        assert_read_ok(&odb, &fx.blob1, &fx.file1);
        assert_read_ok(&odb, &fx.blob2, &fx.file2);
        assert_read_ok(&odb, &fx.blob3, &fx.file3);

        assert_eq!(
            packed_delta_base_oid(&objects, &fx.blob2)
                .expect("base")
                .map(|o| o.to_hex()),
            Some(fx.blob1.to_hex())
        );
    });
}

#[test]
fn t5303_packed_full_object_slice_skips_crc_mismatch_and_reads_redundant() {
    run_algo(HashAlgo::Sha1, |algo| {
        clear_pack_cache();
        let fx = T5303Pack::build_ofs(algo);
        let objects = fx.objects();
        let good_pack = std::fs::read(&fx.pack_path).expect("read good");
        install_hand_pack(&objects, "good-copy", algo, |builder| {
            builder.add_full(PackObjectKind::Blob, &fx.file1);
            let d2 = delta_between(&fx.file1, &fx.file2);
            builder.add_ofs_delta(0, &d2, d2.len());
            let d3 = delta_between(&fx.file2, &fx.file3);
            builder.add_ofs_delta(1, &d3, d3.len());
        });

        flip_byte_at(&fx.pack_path, 12 + 50).expect("flip primary blob bytes");
        clear_pack_cache();
        assert!(
            packed_full_object_slice(&objects, &fx.blob1)
                .expect("lookup")
                .is_some(),
            "redundant full slice should remain available"
        );
        let got = read_object_from_packs(&objects, &fx.blob1).expect("read blob1");
        assert_eq!(got.data, fx.file1);
        let _ = good_pack;
    });
}

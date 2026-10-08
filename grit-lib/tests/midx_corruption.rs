//! Corrupt multi-pack-index images: `verify_midx` diagnostics and pack fallback reads (t5319).

#![allow(clippy::unwrap_used, clippy::expect_used)]

pub mod midx_support;

use std::fs;

use grit_lib::midx::{try_read_object_via_midx, verify_midx, WriteMultiPackIndexOptions};
use grit_lib::objects::ObjectId;
use grit_lib::pack::clear_pack_cache;
use grit_test_support::objects::HashAlgo;

use grit_test_support::objects::RepoFixture;
use midx_support::{
    find_midx_chunk, git_write_midx, head_oid, multi_pack_repo, odb_with_midx_config,
    patch_midx_file, remove_loose_object,
};

const CHUNK_PACKNAMES: u32 = 0x504e_414d;
const CHUNK_OIDFANOUT: u32 = 0x4f49_4446;
const CHUNK_OIDLOOKUP: u32 = 0x4f49_444c;
const CHUNK_OBJECTOFFSETS: u32 = 0x4f4f_4646;

fn corrupt_fixture() -> Option<(RepoFixture, std::path::PathBuf, ObjectId)> {
    let (repo, objects, _) = multi_pack_repo(HashAlgo::Sha1, 3)?;
    git_write_midx(&repo);
    let oid = head_oid(&repo);
    Some((repo, objects, oid))
}

fn assert_verify_reports(objects: &std::path::Path, needle: &str) {
    let errs = verify_midx(objects).expect_err("expected verify failure");
    assert!(
        errs.iter().any(|e| e.contains(needle)),
        "expected verify message containing {needle:?}, got {errs:?}"
    );
}

fn assert_read_fallback_via_odb(repo: &RepoFixture, objects: &std::path::Path, oid: &ObjectId) {
    let git_dir = repo.path().join(".git");
    remove_loose_object(objects, oid);
    clear_pack_cache();
    grit_lib::midx::evict_midx_read_cache_for_pack_dir(&objects.join("pack"));
    let odb = odb_with_midx_config(objects, &git_dir, true);
    let via_midx_on = odb
        .read(oid)
        .expect("Odb must read via pack indexes with core.multiPackIndex=true");
    assert_eq!(via_midx_on.kind, grit_lib::objects::ObjectKind::Commit);
    let odb_off = odb_with_midx_config(objects, &git_dir, false);
    let via_packs = odb_off
        .read(oid)
        .expect("Odb must read via pack indexes with MIDX off");
    assert_eq!(via_midx_on.data, via_packs.data);
}

fn assert_midx_read_skips_or_none(objects: &std::path::Path, oid: &ObjectId) {
    clear_pack_cache();
    grit_lib::midx::evict_midx_read_cache_for_pack_dir(&objects.join("pack"));
    match try_read_object_via_midx(objects, oid) {
        Ok(None) => {}
        Ok(Some(_)) => panic!("expected MIDX read to skip corrupt index"),
        Err(err) => panic!("expected MIDX skip, got error: {err:?}"),
    }
}

#[test]
fn verify_reports_no_objects_in_midx() {
    let Some((repo, objects, oid)) = corrupt_fixture() else {
        eprintln!("SKIP: fixture");
        return;
    };
    let pack_dir = objects.join("pack");
    patch_midx_file(&pack_dir, |data| {
        if let Some((fanout_off, _)) = find_chunk(data, CHUNK_OIDFANOUT) {
            for i in 0..256 {
                data[fanout_off + i * 4..fanout_off + i * 4 + 4]
                    .copy_from_slice(&0u32.to_be_bytes());
            }
        }
        let num_chunks = data[6] as usize;
        let toc_off = 12usize;
        let mut oidl_off_bytes = None;
        let mut ooff_entry = None;
        for i in 0..num_chunks {
            let entry = toc_off + i * 12;
            let chunk_id = u32::from_be_bytes([
                data[entry],
                data[entry + 1],
                data[entry + 2],
                data[entry + 3],
            ]);
            if chunk_id == CHUNK_OIDLOOKUP {
                oidl_off_bytes = Some(data[entry + 4..entry + 12].to_vec());
            }
            if chunk_id == CHUNK_OBJECTOFFSETS {
                ooff_entry = Some(entry);
            }
        }
        if let (Some(oidl_off), Some(ooff_entry)) = (oidl_off_bytes, ooff_entry) {
            data[ooff_entry + 4..ooff_entry + 12].copy_from_slice(&oidl_off);
            let next_entry = ooff_entry + 12;
            if next_entry + 12 <= data.len() {
                data[next_entry + 4..next_entry + 12].copy_from_slice(&oidl_off);
            }
        }
    });
    let errs = verify_midx(&objects).expect_err("verify should fail");
    assert!(
        errs.iter().any(|e| e.contains("no oid")),
        "expected no oid error, got {errs:?}"
    );
    assert_read_fallback_via_odb(&repo, &objects, &oid);
}

#[test]
fn verify_bad_signature() {
    let Some((repo, objects, oid)) = corrupt_fixture() else {
        eprintln!("SKIP: fixture");
        return;
    };
    let pack_dir = objects.join("pack");
    patch_midx_file(&pack_dir, |data| {
        data[0..4].copy_from_slice(&[0, 0, 0, 0]);
    });
    assert_verify_reports(&objects, "signature");
    assert_read_fallback_via_odb(&repo, &objects, &oid);
}

#[test]
fn verify_bad_version() {
    let Some((repo, objects, oid)) = corrupt_fixture() else {
        eprintln!("SKIP: fixture");
        return;
    };
    let pack_dir = objects.join("pack");
    patch_midx_file(&pack_dir, |data| {
        data[4] = 99;
    });
    assert_verify_reports(&objects, "version");
    assert_read_fallback_via_odb(&repo, &objects, &oid);
}

#[test]
fn verify_hash_version_mismatch() {
    let Some((repo, objects, oid)) = corrupt_fixture() else {
        eprintln!("SKIP: fixture");
        return;
    };
    let pack_dir = objects.join("pack");
    patch_midx_file(&pack_dir, |data| {
        data[5] = 2;
    });
    assert_verify_reports(&objects, "hash version");
    match try_read_object_via_midx(&objects, &oid) {
        Err(grit_lib::error::Error::Midx(
            grit_lib::midx_error::MidxError::HashVersionMismatch { .. },
        )) => {}
        other => panic!("expected hash-version MIDX error, got {other:?}"),
    }
    assert_read_fallback_via_odb(&repo, &objects, &oid);
}

#[test]
fn verify_truncated_chunk_table() {
    let Some((repo, objects, oid)) = corrupt_fixture() else {
        eprintln!("SKIP: fixture");
        return;
    };
    let pack_dir = objects.join("pack");
    patch_midx_file(&pack_dir, |data| {
        data.truncate(data.len().saturating_sub(40));
    });
    assert!(verify_midx(&objects).is_err());
    assert_read_fallback_via_odb(&repo, &objects, &oid);
}

#[test]
fn verify_duplicate_chunk_id() {
    let Some((repo, objects, oid)) = corrupt_fixture() else {
        eprintln!("SKIP: fixture");
        return;
    };
    let pack_dir = objects.join("pack");
    patch_midx_file(&pack_dir, |data| {
        if data.len() > 28 {
            let dup = data[12..16].to_vec();
            data[24..28].copy_from_slice(&dup);
        }
    });
    let errs = verify_midx(&objects).expect_err("duplicate chunk id");
    assert!(
        errs.iter().any(|e| e.contains("duplicate chunk ID")),
        "expected duplicate chunk diagnostic, got {errs:?}"
    );
    assert_read_fallback_via_odb(&repo, &objects, &oid);
}

#[test]
fn verify_missing_required_chunk() {
    let Some((repo, objects, oid)) = corrupt_fixture() else {
        eprintln!("SKIP: fixture");
        return;
    };
    let pack_dir = objects.join("pack");
    patch_midx_file(&pack_dir, |data| {
        data[6] = 0;
        data.truncate(64);
    });
    let errs = verify_midx(&objects).expect_err("missing chunks");
    assert!(
        errs.iter()
            .any(|e| { e.contains("pack-name") || e.contains("pack name") || e.contains("chunk") }),
        "expected missing pack-name chunk, got {errs:?}"
    );
    assert_read_fallback_via_odb(&repo, &objects, &oid);
}

#[test]
fn verify_oid_fanout_out_of_order() {
    let Some((repo, objects, oid)) = corrupt_fixture() else {
        eprintln!("SKIP: fixture");
        return;
    };
    let pack_dir = objects.join("pack");
    patch_midx_file(&pack_dir, |data| {
        if let Some((fanout_off, _)) = find_chunk(data, CHUNK_OIDFANOUT) {
            data[fanout_off..fanout_off + 4].copy_from_slice(&100u32.to_be_bytes());
            data[fanout_off + 4..fanout_off + 8].copy_from_slice(&1u32.to_be_bytes());
        }
    });
    assert_verify_reports(&objects, "fanout");
    assert_read_fallback_via_odb(&repo, &objects, &oid);
}

#[test]
fn verify_oid_lookup_out_of_order() {
    let Some((repo, objects, oid)) = corrupt_fixture() else {
        eprintln!("SKIP: fixture");
        return;
    };
    let pack_dir = objects.join("pack");
    patch_midx_file(&pack_dir, |data| {
        if let Some((oidl_off, oidl_len)) = find_chunk(data, CHUNK_OIDLOOKUP) {
            let hash_len = if data[5] == 2 { 32 } else { 20 };
            if oidl_len >= 2 * hash_len {
                let a = data[oidl_off..oidl_off + hash_len].to_vec();
                let b = data[oidl_off + hash_len..oidl_off + 2 * hash_len].to_vec();
                data[oidl_off..oidl_off + hash_len].copy_from_slice(&b);
                data[oidl_off + hash_len..oidl_off + 2 * hash_len].copy_from_slice(&a);
            }
        }
    });
    assert_verify_reports(&objects, "oid lookup out of order");
    assert_read_fallback_via_odb(&repo, &objects, &oid);
}

#[test]
fn verify_bad_object_offset() {
    let Some((repo, objects, oid)) = corrupt_fixture() else {
        eprintln!("SKIP: fixture");
        return;
    };
    let pack_dir = objects.join("pack");
    patch_midx_file(&pack_dir, |data| {
        if let Some((ooff_off, _)) = find_chunk(data, CHUNK_OBJECTOFFSETS) {
            if let Some((oidl_off, oidl_len)) = find_chunk(data, CHUNK_OIDLOOKUP) {
                let hash_len = if data[5] == 2 { 32 } else { 20 };
                let num = oidl_len / hash_len;
                for i in 0..num {
                    let start = oidl_off + i * hash_len;
                    if data[start..start + hash_len] == *oid.as_bytes() {
                        let ob = ooff_off + i * 8 + 4;
                        data[ob..ob + 4].copy_from_slice(&0x0000_1234u32.to_be_bytes());
                        break;
                    }
                }
            }
        }
    });
    assert_verify_reports(&objects, "incorrect object offset");
    assert_read_fallback_via_odb(&repo, &objects, &oid);
}

#[test]
fn verify_bad_trailer_checksum() {
    let Some((repo, objects, oid)) = corrupt_fixture() else {
        eprintln!("SKIP: fixture");
        return;
    };
    let pack_dir = objects.join("pack");
    patch_midx_file(&pack_dir, |data| {
        if !data.is_empty() {
            let last = data.len() - 1;
            data[last] ^= 0xff;
        }
    });
    assert_verify_reports(&objects, "incorrect checksum");
    assert_midx_read_skips_or_none(&objects, &oid);
    assert_read_fallback_via_odb(&repo, &objects, &oid);
}

#[test]
fn verify_v1_pack_names_out_of_order() {
    let Some((repo, objects, oid)) = corrupt_fixture() else {
        eprintln!("SKIP: fixture");
        return;
    };
    let pack_dir = objects.join("pack");
    grit_lib::midx::write_multi_pack_index_with_options(
        &pack_dir,
        &WriteMultiPackIndexOptions {
            version: Some(1),
            ..Default::default()
        },
    )
    .expect("rewrite v1");
    grit_lib::midx::evict_midx_read_cache_for_pack_dir(&pack_dir);
    patch_midx_file(&pack_dir, |data| {
        if let Some((pn_off, pn_len)) = find_chunk(data, CHUNK_PACKNAMES) {
            let end = (pn_off + pn_len).min(data.len());
            let slice = &mut data[pn_off..end];
            for i in (0..slice.len().saturating_sub(1)).step_by(2) {
                slice.swap(i, i + 1);
            }
        }
    });
    let errs = verify_midx(&objects).expect_err("v1 pack name order");
    assert!(
        errs.iter().any(|e| e.contains("pack names out of order")),
        "expected pack name order error, got {errs:?}"
    );
    assert_read_fallback_via_odb(&repo, &objects, &oid);
}

#[test]
fn verify_inflated_num_chunks_in_header() {
    let Some((repo, objects, oid)) = corrupt_fixture() else {
        eprintln!("SKIP: fixture");
        return;
    };
    let pack_dir = objects.join("pack");
    patch_midx_file(&pack_dir, |data| {
        data[6] = 40;
    });
    assert!(verify_midx(&objects).is_err());
    let _ = (repo, oid);
}

#[test]
fn verify_misaligned_chunk_offset() {
    let Some((repo, objects, oid)) = corrupt_fixture() else {
        eprintln!("SKIP: fixture");
        return;
    };
    let pack_dir = objects.join("pack");
    patch_midx_file(&pack_dir, |data| {
        let entry = 12 + 4;
        if data.len() > entry + 8 {
            data[entry..entry + 8].copy_from_slice(&1u64.to_be_bytes());
        }
    });
    assert!(verify_midx(&objects).is_err());
    let _ = (repo, oid);
}

#[test]
fn verify_pack_names_chunk_non_utf8() {
    let Some((repo, objects, oid)) = corrupt_fixture() else {
        eprintln!("SKIP: fixture");
        return;
    };
    let pack_dir = objects.join("pack");
    patch_midx_file(&pack_dir, |data| {
        if let Some((pn_off, pn_len)) = find_chunk(data, CHUNK_PACKNAMES) {
            let end = (pn_off + pn_len).min(data.len());
            if end > pn_off {
                data[pn_off] = 0xff;
            }
        }
    });
    assert!(verify_midx(&objects).is_err());
    let _ = (repo, oid);
}

#[test]
fn verify_improper_chunk_offset_pair() {
    let Some((repo, objects, oid)) = corrupt_fixture() else {
        eprintln!("SKIP: fixture");
        return;
    };
    let pack_dir = objects.join("pack");
    patch_midx_file(&pack_dir, |data| {
        let next_entry = 12 + 12;
        if data.len() > next_entry + 8 {
            data[next_entry + 4..next_entry + 12].copy_from_slice(&8u64.to_be_bytes());
        }
    });
    assert!(verify_midx(&objects).is_err());
    let _ = (repo, oid);
}

#[test]
fn read_with_corrupt_pack_index_still_resolves_other_objects() {
    let Some((repo, objects, oid)) = corrupt_fixture() else {
        eprintln!("SKIP: fixture");
        return;
    };
    let pack_dir = objects.join("pack");
    let names = grit_lib::midx::read_midx_pack_idx_names(&objects).expect("names");
    if names.len() < 2 {
        eprintln!("SKIP: need multiple packs");
        return;
    }
    let victim = pack_dir.join(&names[0]);
    let mut perms = fs::metadata(&victim).expect("idx meta").permissions();
    perms.set_readonly(false);
    fs::set_permissions(&victim, perms).expect("chmod idx");
    let mut idx_bytes = fs::read(&victim).expect("idx");
    if !idx_bytes.is_empty() {
        let last = idx_bytes.len() - 1;
        idx_bytes[last] ^= 0x7f;
        fs::write(&victim, &idx_bytes).expect("write corrupt idx");
    }
    grit_lib::midx::evict_midx_read_cache_for_pack_dir(&pack_dir);
    grit_lib::pack::clear_pack_cache();
    let _ = try_read_object_via_midx(&objects, &oid);
    assert_read_fallback_via_odb(&repo, &objects, &oid);
}

#[test]
fn verify_file_too_small() {
    let Some((repo, objects, oid)) = corrupt_fixture() else {
        eprintln!("SKIP: fixture");
        return;
    };
    let pack_dir = objects.join("pack");
    patch_midx_file(&pack_dir, |data| {
        data.truncate(8);
    });
    assert_verify_reports(&objects, "too small");
    let _ = (repo, oid);
}

#[test]
fn verify_wrong_oid_lookup_chunk_size() {
    let Some((repo, objects, oid)) = corrupt_fixture() else {
        eprintln!("SKIP: fixture");
        return;
    };
    let pack_dir = objects.join("pack");
    patch_midx_file(&pack_dir, |data| {
        if let Some((fanout_off, _)) = find_chunk(data, CHUNK_OIDFANOUT) {
            data[fanout_off + 255 * 4..fanout_off + 256 * 4].copy_from_slice(&50u32.to_be_bytes());
        }
    });
    assert_verify_reports(&objects, "OID lookup chunk is the wrong size");
    assert_read_fallback_via_odb(&repo, &objects, &oid);
}

#[test]
fn verify_wrong_object_offsets_chunk_size() {
    let Some((repo, objects, oid)) = corrupt_fixture() else {
        eprintln!("SKIP: fixture");
        return;
    };
    let pack_dir = objects.join("pack");
    patch_midx_file(&pack_dir, |data| {
        let num = if let Some((fanout_off, _)) = find_chunk(data, CHUNK_OIDFANOUT) {
            u32::from_be_bytes([
                data[fanout_off + 255 * 4],
                data[fanout_off + 255 * 4 + 1],
                data[fanout_off + 255 * 4 + 2],
                data[fanout_off + 255 * 4 + 3],
            ]) as usize
        } else {
            return;
        };
        if let Some((ooff_off, ooff_len)) = find_chunk(data, CHUNK_OBJECTOFFSETS) {
            let want = num * 8;
            if ooff_len > 8 && want <= ooff_len {
                let shrink = ooff_off + want - 8;
                data[shrink..ooff_off + ooff_len].fill(0);
            }
        }
    });
    assert!(verify_midx(&objects).is_err());
    assert_read_fallback_via_odb(&repo, &objects, &oid);
}

#[test]
fn verify_bad_pack_int_id() {
    let Some((repo, objects, oid)) = corrupt_fixture() else {
        eprintln!("SKIP: fixture");
        return;
    };
    let pack_dir = objects.join("pack");
    patch_midx_file(&pack_dir, |data| {
        if let Some((ooff_off, _)) = find_chunk(data, CHUNK_OBJECTOFFSETS) {
            data[ooff_off..ooff_off + 4].copy_from_slice(&99u32.to_be_bytes());
        }
    });
    let errs = verify_midx(&objects).expect_err("verify");
    assert!(
        errs.iter().any(|e| e.contains("bad pack-int-id")),
        "{errs:?}"
    );
    assert_read_fallback_via_odb(&repo, &objects, &oid);
}

#[test]
fn verify_large_offset_out_of_bounds() {
    let Some((repo, objects, oid)) = corrupt_fixture() else {
        eprintln!("SKIP: fixture");
        return;
    };
    let pack_dir = objects.join("pack");
    patch_midx_file(&pack_dir, |data| {
        if let Some((ooff_off, _)) = find_chunk(data, CHUNK_OBJECTOFFSETS) {
            data[ooff_off + 4..ooff_off + 8].copy_from_slice(&0x8000_00ffu32.to_be_bytes());
        }
    });
    assert_verify_reports(&objects, "large offset out of bounds");
    assert_read_fallback_via_odb(&repo, &objects, &oid);
}

#[test]
fn verify_failed_to_load_pack_file() {
    let Some((repo, objects, oid)) = corrupt_fixture() else {
        eprintln!("SKIP: fixture");
        return;
    };
    let pack_dir = objects.join("pack");
    let names = grit_lib::midx::read_midx_pack_idx_names(&objects).expect("names");
    let victim = names.first().expect("pack name");
    let stem = victim.strip_suffix(".idx").unwrap();
    std::fs::remove_file(pack_dir.join(victim)).ok();
    std::fs::remove_file(pack_dir.join(format!("{stem}.pack"))).ok();
    grit_lib::midx::evict_midx_read_cache_for_pack_dir(&pack_dir);
    let errs = verify_midx(&objects).expect_err("verify");
    assert!(
        errs.iter().any(|e| e.contains("failed to load pack")),
        "{errs:?}"
    );
    let _ = oid;
    let _ = repo;
}

#[test]
fn load_reuse_tables_rejects_bad_ridx_entry() {
    let Some((repo, objects, _)) = corrupt_fixture() else {
        eprintln!("SKIP: fixture");
        return;
    };
    let pack_dir = objects.join("pack");
    grit_lib::midx::write_multi_pack_index_with_options(
        &pack_dir,
        &WriteMultiPackIndexOptions {
            write_bitmap_placeholders: true,
            write_rev_placeholder: false,
            version: Some(1),
            ..Default::default()
        },
    )
    .expect("write with embedded ridx");
    patch_midx_file(&pack_dir, |data| {
        if let Some((ridx_off, ridx_len)) = find_chunk(data, 0x52494458) {
            if ridx_len >= 4 {
                data[ridx_off..ridx_off + 4].copy_from_slice(&9999u32.to_be_bytes());
            }
        }
    });
    grit_lib::midx::evict_midx_read_cache_for_pack_dir(&pack_dir);
    assert!(grit_lib::midx::load_midx_reuse_tables(&objects).is_err());
    let _ = repo;
}

fn find_chunk(data: &[u8], id: u32) -> Option<(usize, usize)> {
    find_midx_chunk(data, id)
}

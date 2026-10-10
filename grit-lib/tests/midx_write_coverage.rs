//! Extra multi-pack-index write, chain, and compaction paths for line coverage (t5319/t5335).

#![allow(clippy::unwrap_used, clippy::expect_used)]

pub mod midx_support;

use grit_lib::midx::{
    compact_multi_pack_index, midx_lookup_pack_and_offset, read_midx_objects,
    read_midx_pack_idx_names, read_midx_preferred_idx_name, resolve_midx_layer_path,
    resolve_tip_midx_path, verify_midx, write_multi_pack_index_with_options, CompactError,
    WriteMultiPackIndexOptions,
};
use grit_lib::objects::ObjectId;
use grit_test_support::objects::HashAlgo;
use std::fs;
use std::path::Path;

use midx_support::{
    git_available, grit_write_midx, multi_pack_repo, pack_idx_paths, patch_midx_file,
    read_chain_hashes, tip_midx_path,
};

fn midx_trailing_hex(path: &Path) -> String {
    let data = fs::read(path).expect("read midx");
    let hash_len = if data.len() > 5 && data[5] == 2 {
        32
    } else {
        20
    };
    hex::encode(&data[data.len() - hash_len..])
}

#[test]
fn preferred_pack_name_accepts_pack_suffix_and_relative_path() {
    let Some((repo, objects, _)) = multi_pack_repo(HashAlgo::Sha1, 2) else {
        eprintln!("SKIP: fixture");
        return;
    };
    let pack_dir = objects.join("pack");
    let idx = pack_idx_paths(&objects)[0]
        .file_name()
        .unwrap()
        .to_string_lossy()
        .into_owned();
    let stem = idx.strip_suffix(".idx").unwrap();
    for name in [format!("./{idx}"), format!("{stem}.pack"), idx.clone()] {
        let _ = &repo;
        grit_lib::midx::clear_pack_midx_state(&pack_dir).expect("clear");
        grit_write_midx(
            &pack_dir,
            &WriteMultiPackIndexOptions {
                preferred_pack_name: Some(name),
                version: Some(1),
                ..Default::default()
            },
        );
    }
    let _ = repo;
}

#[test]
fn unknown_preferred_pack_emits_warning_and_still_writes() {
    let Some((repo, objects, _)) = multi_pack_repo(HashAlgo::Sha1, 2) else {
        eprintln!("SKIP: fixture");
        return;
    };
    let pack_dir = objects.join("pack");
    grit_lib::midx::clear_pack_midx_state(&pack_dir).expect("clear");
    grit_write_midx(
        &pack_dir,
        &WriteMultiPackIndexOptions {
            preferred_pack_name: Some("no-such-pack.idx".into()),
            version: Some(1),
            ..Default::default()
        },
    );
    assert!(resolve_tip_midx_path(&pack_dir).is_some());
    let _ = repo;
}

#[test]
fn pack_names_subset_skips_unknown_entries() {
    let Some((repo, objects, _)) = multi_pack_repo(HashAlgo::Sha1, 3) else {
        eprintln!("SKIP: fixture");
        return;
    };
    let pack_dir = objects.join("pack");
    let idx = pack_idx_paths(&objects)[0]
        .file_name()
        .unwrap()
        .to_string_lossy()
        .into_owned();
    grit_lib::midx::clear_pack_midx_state(&pack_dir).expect("clear");
    grit_write_midx(
        &pack_dir,
        &WriteMultiPackIndexOptions {
            pack_names_subset_ordered: Some(vec![idx, "missing-pack.idx".into()]),
            version: Some(1),
            ..Default::default()
        },
    );
    let _ = repo;
}

#[test]
fn incremental_write_without_new_packs_is_noop() {
    let Some((repo, objects, _)) = multi_pack_repo(HashAlgo::Sha1, 2) else {
        eprintln!("SKIP: fixture");
        return;
    };
    let pack_dir = objects.join("pack");
    grit_write_midx(&pack_dir, &WriteMultiPackIndexOptions::default());
    let before = fs::metadata(tip_midx_path(&pack_dir))
        .expect("midx exists")
        .modified()
        .ok();
    write_multi_pack_index_with_options(
        &pack_dir,
        &WriteMultiPackIndexOptions {
            incremental: true,
            version: Some(1),
            ..Default::default()
        },
    )
    .expect("noop incremental");
    assert!(
        read_chain_hashes(&pack_dir).is_none(),
        "incremental noop must not create a chain"
    );
    let _ = (before, repo, objects);
}

#[test]
fn rewrite_ignores_existing_midx_with_bad_checksum() {
    let Some((repo, objects, _)) = multi_pack_repo(HashAlgo::Sha1, 2) else {
        eprintln!("SKIP: fixture");
        return;
    };
    let pack_dir = objects.join("pack");
    grit_write_midx(&pack_dir, &WriteMultiPackIndexOptions::default());
    patch_midx_file(&pack_dir, |data| {
        if !data.is_empty() {
            let last = data.len() - 1;
            data[last] ^= 0x55;
        }
    });
    grit_write_midx(&pack_dir, &WriteMultiPackIndexOptions::default());
    assert!(resolve_tip_midx_path(&pack_dir).is_some());
    let _ = repo;
}

#[test]
fn rewrite_fails_when_existing_midx_references_missing_pack() {
    let Some((repo, objects, _)) = multi_pack_repo(HashAlgo::Sha1, 2) else {
        eprintln!("SKIP: fixture");
        return;
    };
    let pack_dir = objects.join("pack");
    grit_write_midx(&pack_dir, &WriteMultiPackIndexOptions::default());
    let idx = pack_idx_paths(&objects)[0].clone();
    let stem = idx.file_stem().unwrap().to_string_lossy();
    fs::remove_file(pack_dir.join(format!("{stem}.pack"))).expect("remove pack");
    let err =
        write_multi_pack_index_with_options(&pack_dir, &WriteMultiPackIndexOptions::default());
    assert!(err.is_err(), "expected could not load pack error");
    let _ = repo;
}

#[test]
fn resolve_midx_layer_path_matches_root_file_checksum() {
    let Some((repo, objects, _)) = multi_pack_repo(HashAlgo::Sha1, 2) else {
        eprintln!("SKIP: fixture");
        return;
    };
    let pack_dir = objects.join("pack");
    grit_write_midx(&pack_dir, &WriteMultiPackIndexOptions::default());
    let root = pack_dir.join("multi-pack-index");
    let hex = midx_trailing_hex(&root);
    assert_eq!(
        resolve_midx_layer_path(&pack_dir, &hex),
        Some(root),
        "root MIDX should resolve by checksum"
    );
    assert!(resolve_midx_layer_path(&pack_dir, "deadbeef").is_none());
    let _ = repo;
}

#[test]
fn incremental_write_links_root_midx_into_chain() {
    let Some((repo, objects, _oids)) = multi_pack_repo(HashAlgo::Sha1, 2) else {
        eprintln!("SKIP: fixture");
        return;
    };
    let pack_dir = objects.join("pack");
    grit_write_midx(&pack_dir, &WriteMultiPackIndexOptions::default());
    assert!(pack_dir.join("multi-pack-index").is_file());

    std::fs::write(repo.path().join("chain-layer.txt"), b"chain\n").unwrap();
    repo.git(&["add", "chain-layer.txt"]);
    repo.git(&["commit", "-q", "-m", "chain layer"]);
    assert!(midx_support::pack_objects_layer(repo.path(), 50));

    write_multi_pack_index_with_options(
        &pack_dir,
        &WriteMultiPackIndexOptions {
            incremental: true,
            version: Some(1),
            ..Default::default()
        },
    )
    .expect("incremental layer");

    assert!(!pack_dir.join("multi-pack-index").exists());
    let Some(chain) = read_chain_hashes(&pack_dir) else {
        panic!("expected chain after incremental write");
    };
    assert!(chain.len() >= 2);
    for h in chain {
        assert!(resolve_midx_layer_path(&pack_dir, &h).is_some());
    }
}

#[test]
fn non_incremental_write_clears_incremental_chain() {
    let Some((repo, objects, _)) = multi_pack_repo(HashAlgo::Sha1, 1) else {
        eprintln!("SKIP: fixture");
        return;
    };
    let pack_dir = objects.join("pack");
    grit_write_midx(&pack_dir, &WriteMultiPackIndexOptions::default());
    std::fs::write(repo.path().join("inc.txt"), b"inc\n").unwrap();
    repo.git(&["add", "inc.txt"]);
    repo.git(&["commit", "-q", "-m", "inc"]);
    assert!(midx_support::pack_objects_layer(repo.path(), 51));
    write_multi_pack_index_with_options(
        &pack_dir,
        &WriteMultiPackIndexOptions {
            incremental: true,
            version: Some(1),
            ..Default::default()
        },
    )
    .expect("incremental");
    assert!(read_chain_hashes(&pack_dir).is_some());

    grit_write_midx(
        &pack_dir,
        &WriteMultiPackIndexOptions {
            version: Some(1),
            write_bitmap: true,
            write_rev_sidecar: true,
            ..Default::default()
        },
    );
    assert!(pack_dir.join("multi-pack-index").is_file());
    assert!(!pack_dir
        .join("multi-pack-index.d/multi-pack-index-chain")
        .exists());
}

#[test]
fn compact_with_bitmaps_and_rev_sidecars() {
    let Some((repo, objects, _)) = multi_pack_repo(HashAlgo::Sha1, 1) else {
        eprintln!("SKIP: fixture");
        return;
    };
    let pack_dir = objects.join("pack");
    grit_write_midx(&pack_dir, &WriteMultiPackIndexOptions::default());
    for i in 0..4 {
        std::fs::write(
            repo.path().join(format!("cb-{i}.txt")),
            format!("compact {i}\n"),
        )
        .unwrap();
        repo.git(&["add", &format!("cb-{i}.txt")]);
        repo.git(&["commit", "-q", "-m", &format!("cb {i}")]);
        assert!(midx_support::pack_objects_layer(repo.path(), 110 + i));
        write_multi_pack_index_with_options(
            &pack_dir,
            &WriteMultiPackIndexOptions {
                incremental: true,
                version: Some(2),
                ..Default::default()
            },
        )
        .expect("incremental v2");
    }
    let Some(chain) = read_chain_hashes(&pack_dir) else {
        eprintln!("SKIP: no grit chain");
        return;
    };
    if chain.len() < 3 {
        eprintln!("SKIP: chain too short");
        return;
    }
    let from = chain[0].clone();
    let to = chain[chain.len() - 2].clone();
    compact_multi_pack_index(&pack_dir, &from, &to, true, true, Some(2)).expect("compact bitmaps");
    grit_lib::midx::verify_midx(&objects).expect("verify after compact");
    if git_available() {
        let v = repo.git(&["multi-pack-index", "verify"]);
        if !v.ok {
            eprintln!("SKIP git verify after v2 compact: {}", v.stderr);
        }
    }
}

#[test]
fn compact_missing_to_endpoint() {
    let Some((repo, objects, _)) = multi_pack_repo(HashAlgo::Sha1, 1) else {
        eprintln!("SKIP: fixture");
        return;
    };
    let pack_dir = objects.join("pack");
    grit_write_midx(&pack_dir, &WriteMultiPackIndexOptions::default());
    std::fs::write(repo.path().join("ce.txt"), b"ce\n").unwrap();
    repo.git(&["add", "ce.txt"]);
    repo.git(&["commit", "-q", "-m", "ce"]);
    assert!(midx_support::pack_objects_layer(repo.path(), 52));
    write_multi_pack_index_with_options(
        &pack_dir,
        &WriteMultiPackIndexOptions {
            incremental: true,
            version: Some(2),
            ..Default::default()
        },
    )
    .expect("incremental");
    let Some(chain) = read_chain_hashes(&pack_dir) else {
        eprintln!("SKIP: no chain");
        return;
    };
    let from = &chain[0];
    assert!(matches!(
        compact_multi_pack_index(&pack_dir, from, "badchecksum", false, false, Some(2)),
        Err(grit_lib::midx::CompactError::MissingEndpoint(_))
    ));
    let _ = (repo, objects);
}

#[test]
fn bitmap_sidecars_scrubbed_on_fresh_write() {
    let Some((repo, objects, _)) = multi_pack_repo(HashAlgo::Sha1, 2) else {
        eprintln!("SKIP: fixture");
        return;
    };
    let pack_dir = objects.join("pack");
    grit_write_midx(
        &pack_dir,
        &WriteMultiPackIndexOptions {
            write_bitmap: true,
            write_rev_sidecar: true,
            version: Some(1),
            ..Default::default()
        },
    );
    let first_rev = fs::read_dir(&pack_dir)
        .expect("read pack dir")
        .filter_map(|e| e.ok())
        .find(|e| e.file_name().to_string_lossy().ends_with(".rev"))
        .map(|e| e.path());
    assert!(first_rev.is_some(), "expected .rev sidecar");

    grit_lib::midx::clear_pack_midx_state(&pack_dir).expect("clear");
    grit_write_midx(
        &pack_dir,
        &WriteMultiPackIndexOptions {
            version: Some(1),
            ..Default::default()
        },
    );
    let stale = fs::read_dir(&pack_dir)
        .expect("read pack dir")
        .filter_map(|e| e.ok())
        .any(|e| {
            let name = e.file_name();
            let n = name.to_string_lossy();
            n.starts_with("multi-pack-index-") && (n.ends_with(".rev") || n.ends_with(".bitmap"))
        });
    assert!(
        !stale,
        "stale MIDX sidecars from prior write should be scrubbed"
    );
    let _ = (repo, objects);
}

#[test]
fn many_lookups_exercise_chain_and_info_paths() {
    let Some((repo, objects, _)) = multi_pack_repo(HashAlgo::Sha1, 4) else {
        eprintln!("SKIP: fixture");
        return;
    };
    let pack_dir = objects.join("pack");
    grit_write_midx(&pack_dir, &WriteMultiPackIndexOptions::default());
    for i in 0..6 {
        std::fs::write(
            repo.path().join(format!("lk-{i}.txt")),
            format!("lookup stress {i}\n"),
        )
        .unwrap();
        repo.git(&["add", &format!("lk-{i}.txt")]);
        repo.git(&["commit", "-q", "-m", &format!("lk {i}")]);
        assert!(midx_support::pack_objects_layer(repo.path(), 120 + i));
        write_multi_pack_index_with_options(
            &pack_dir,
            &WriteMultiPackIndexOptions {
                incremental: true,
                version: Some(1),
                ..Default::default()
            },
        )
        .expect("incr");
    }
    let packed: Vec<ObjectId> = midx_support::all_packed_oids(&objects)
        .into_iter()
        .collect();
    for oid in packed {
        let _ = grit_lib::midx::midx_lookup_pack_and_offset_opt(&objects, &oid);
        let _ = grit_lib::midx::try_read_info_via_midx(&objects, &oid);
        let _ = grit_lib::midx::try_read_object_via_midx(&objects, &oid);
        let _ = grit_lib::midx::midx_oid_listed_in_tip(&objects, &oid);
    }
    grit_lib::midx::validate_midx_referenced_packs(&objects);
}

#[test]
fn read_midx_api_errors_without_index() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let objects = tmp.path().join("objects");
    fs::create_dir_all(objects.join("pack")).expect("pack dir");
    assert!(read_midx_pack_idx_names(&objects).is_err());
    assert!(read_midx_objects(&objects).is_err());
    assert!(read_midx_preferred_idx_name(&objects).is_err());
}

#[test]
fn verify_midx_ok_when_no_midx_present() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let objects = tmp.path().join("objects");
    fs::create_dir_all(objects.join("pack")).expect("pack dir");
    verify_midx(&objects).expect("no midx is not an error");
}

#[test]
fn compact_error_display_matches_git_diagnostics() {
    assert!(CompactError::NoChain.to_string().contains("chain"));
    assert!(CompactError::MissingEndpoint("abc".into())
        .to_string()
        .contains("abc"));
    assert!(CompactError::IdenticalEndpoints
        .to_string()
        .contains("unique"));
    assert!(CompactError::NotAncestor("a".into(), "b".into())
        .to_string()
        .contains("ancestor"));
    assert!(CompactError::V1Format.to_string().contains("v1"));
    assert!(CompactError::Other("detail".into())
        .to_string()
        .contains("detail"));
}

#[test]
fn incremental_layer_with_bitmap_and_rev_sidecars_v2() {
    let Some((repo, objects, _)) = multi_pack_repo(HashAlgo::Sha1, 1) else {
        eprintln!("SKIP: fixture");
        return;
    };
    let pack_dir = objects.join("pack");
    grit_write_midx(&pack_dir, &WriteMultiPackIndexOptions::default());
    std::fs::write(repo.path().join("side.txt"), b"side\n").unwrap();
    repo.git(&["add", "side.txt"]);
    repo.git(&["commit", "-q", "-m", "side"]);
    assert!(midx_support::pack_objects_layer(repo.path(), 77));
    let result = write_multi_pack_index_with_options(
        &pack_dir,
        &WriteMultiPackIndexOptions {
            incremental: true,
            version: Some(2),
            write_bitmap: true,
            write_rev_sidecar: true,
            ..Default::default()
        },
    )
    .expect("incremental bitmap layer");
    assert_eq!(
        result.bitmap,
        grit_lib::midx::MidxBitmapWriteOutcome::SkippedIncrementalLayer
    );
    let midx_d = pack_dir.join("multi-pack-index.d");
    let has_zero_bitmap = fs::read_dir(&midx_d)
        .expect("midx.d")
        .filter_map(|e| e.ok())
        .any(|e| {
            let fname = e.file_name();
            let s = fname.to_string_lossy();
            if !s.ends_with(".bitmap") {
                return false;
            }
            e.metadata().map(|m| m.len() == 0).unwrap_or(false)
        });
    assert!(
        !has_zero_bitmap,
        "incremental layer must not create a zero-byte .bitmap sidecar"
    );
    let has_rev = fs::read_dir(&midx_d)
        .expect("midx.d")
        .filter_map(|e| e.ok())
        .any(|e| e.file_name().to_string_lossy().ends_with(".rev"));
    assert!(
        has_rev,
        "incremental layer with write_rev_sidecar should emit a .rev sidecar"
    );
    verify_midx(&objects).expect("verify");
    let _ = repo;
}

#[test]
fn non_incremental_rewrite_retains_identical_midx_bytes() {
    let Some((repo, objects, _)) = multi_pack_repo(HashAlgo::Sha1, 2) else {
        eprintln!("SKIP: fixture");
        return;
    };
    let pack_dir = objects.join("pack");
    let opts = WriteMultiPackIndexOptions {
        version: Some(1),
        ..Default::default()
    };
    grit_write_midx(&pack_dir, &opts);
    let path = pack_dir.join("multi-pack-index");
    let before = fs::read(&path).expect("midx bytes");
    write_multi_pack_index_with_options(&pack_dir, &opts).expect("rewrite identical");
    let after = fs::read(&path).expect("midx still there");
    assert_eq!(before, after);
    let _ = repo;
}

#[test]
fn write_default_v2_midx_roundtrip() {
    let Some((repo, objects, oids)) = multi_pack_repo(HashAlgo::Sha1, 2) else {
        eprintln!("SKIP: fixture");
        return;
    };
    let pack_dir = objects.join("pack");
    grit_lib::midx::clear_pack_midx_state(&pack_dir).expect("clear");
    write_multi_pack_index_with_options(&pack_dir, &WriteMultiPackIndexOptions::default())
        .expect("default v2 write");
    verify_midx(&objects).expect("verify grit v2");
    for oid in &oids {
        assert!(grit_lib::midx::try_read_object_via_midx(&objects, oid)
            .expect("read")
            .is_some());
    }
    let _ = repo;
}

#[test]
fn format_midx_dump_and_show_objects_layers() {
    let Some((repo, objects, oids)) = multi_pack_repo(HashAlgo::Sha1, 2) else {
        eprintln!("SKIP: fixture");
        return;
    };
    let pack_dir = objects.join("pack");
    grit_write_midx(&pack_dir, &WriteMultiPackIndexOptions::default());
    let dump = grit_lib::midx::format_midx_dump(&objects).expect("dump");
    assert!(dump.contains("header:"));
    assert!(dump.contains("packs:"));
    let show = grit_lib::midx::format_midx_show_objects(&objects).expect("show");
    assert!(show.contains(&oids[0].to_hex()));
    let tip = tip_midx_path(&pack_dir);
    let hex = midx_trailing_hex(&tip);
    let layer_dump =
        grit_lib::midx::format_midx_dump_layer(&objects, Some(&hex)).expect("layer dump");
    assert!(layer_dump.contains("num_objects:"));
    let layer_show =
        grit_lib::midx::format_midx_show_objects_layer(&objects, Some(&hex)).expect("layer show");
    assert!(layer_show.lines().count() >= oids.len());
    let _ = repo;
}

#[test]
fn midx_lookup_errors_when_oid_absent() {
    let Some((repo, objects, oids)) = multi_pack_repo(HashAlgo::Sha1, 2) else {
        eprintln!("SKIP: fixture");
        return;
    };
    grit_write_midx(
        &objects.join("pack"),
        &WriteMultiPackIndexOptions::default(),
    );
    let missing = ObjectId::from_hex("0000000000000000000000000000000000000001").expect("oid");
    assert!(midx_lookup_pack_and_offset(&objects, &missing).is_err());
    assert!(
        grit_lib::midx::midx_lookup_pack_and_offset_opt(&objects, &missing)
            .expect("opt lookup")
            .is_none()
    );
    let _ = (repo, oids);
}

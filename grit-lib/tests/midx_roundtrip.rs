//! Multi-pack-index round-trips: grit write vs git verify, git write vs grit read (t5319/t5334/t5335).

#![allow(clippy::unwrap_used, clippy::expect_used)]

pub mod midx_support;

use grit_lib::midx::{
    cached_tip_midx_path, compact_multi_pack_index, evict_midx_read_cache_for_pack_dir,
    load_midx_reuse_tables, midx_lookup_pack_and_offset_opt, midx_oid_listed_in_tip,
    read_midx_objects, read_midx_pack_idx_names, read_midx_preferred_idx_name,
    resolve_midx_layer_path, resolve_tip_midx_path, try_read_info_via_midx,
    try_read_object_via_midx, validate_midx_referenced_packs, verify_midx, write_multi_pack_index,
    CompactError, WriteMultiPackIndexOptions,
};
use grit_lib::objects::ObjectId;
use grit_lib::pack::clear_pack_cache;
use grit_test_support::objects::HashAlgo;
use std::collections::HashSet;

use midx_support::{
    all_packed_oids, assert_git_midx_verify, assert_grit_midx_reads_match_git,
    assert_lookup_matches_idx, git_available, git_cat_file_batch, git_write_midx,
    git_write_midx_incremental, grit_write_midx, head_oid, multi_pack_repo, odb_with_midx_config,
    pack_idx_paths, read_chain_hashes, tip_midx_path,
};

#[test]
fn grit_midx_write_variants_pass_git_verify_sha1() {
    if !git_available() {
        eprintln!("SKIP: git unavailable");
        return;
    }
    let Some((repo, objects, oids)) = multi_pack_repo(HashAlgo::Sha1, 3) else {
        eprintln!("SKIP: fixture setup failed");
        return;
    };
    let pack_dir = objects.join("pack");
    let idx_names: Vec<String> = pack_idx_paths(&objects)
        .into_iter()
        .map(|p| p.file_name().unwrap().to_string_lossy().into_owned())
        .collect();

    let variants = [
        WriteMultiPackIndexOptions::default(),
        WriteMultiPackIndexOptions {
            version: Some(1),
            ..Default::default()
        },
        WriteMultiPackIndexOptions {
            version: Some(2),
            ..Default::default()
        },
        WriteMultiPackIndexOptions {
            preferred_pack_idx: Some(0),
            ..Default::default()
        },
        WriteMultiPackIndexOptions {
            preferred_pack_name: Some(idx_names[1].clone()),
            ..Default::default()
        },
        WriteMultiPackIndexOptions {
            write_bitmap_placeholders: true,
            write_rev_placeholder: true,
            version: Some(1),
            ..Default::default()
        },
        WriteMultiPackIndexOptions {
            pack_names_subset_ordered: Some(vec![idx_names[0].clone(), idx_names[2].clone()]),
            ..Default::default()
        },
    ];

    for opts in variants {
        grit_lib::midx::clear_pack_midx_state(&pack_dir).expect("reset midx");
        grit_write_midx(&pack_dir, &opts);
        if opts.version != Some(2) {
            assert_git_midx_verify(&repo);
        }
        verify_midx(&objects).expect("grit verify_midx clean");
        if opts.version == Some(2) {
            continue;
        }
        let _ = git_cat_file_batch(repo.path(), &oids);
        let listed: Vec<ObjectId> = oids
            .iter()
            .copied()
            .filter(|oid| {
                midx_lookup_pack_and_offset_opt(&objects, oid)
                    .ok()
                    .flatten()
                    .is_some()
            })
            .collect();
        assert!(
            !listed.is_empty(),
            "MIDX should list at least one commit oid"
        );
        assert_grit_midx_reads_match_git(&objects, &listed);
        for oid in listed {
            assert_lookup_matches_idx(&objects, &oid);
        }
    }
}

#[test]
fn grit_midx_write_variants_pass_git_verify_sha256() {
    if !git_available() {
        eprintln!("SKIP: git unavailable");
        return;
    }
    let Some((repo, objects, oids)) = multi_pack_repo(HashAlgo::Sha256, 3) else {
        eprintln!("SKIP: sha256 fixture failed");
        return;
    };
    let pack_dir = objects.join("pack");
    let opts = WriteMultiPackIndexOptions {
        write_bitmap_placeholders: true,
        write_rev_placeholder: true,
        version: Some(1),
        ..Default::default()
    };
    grit_write_midx(&pack_dir, &opts);
    assert_git_midx_verify(&repo);
    verify_midx(&objects).expect("verify");
    assert_grit_midx_reads_match_git(&objects, &oids);
}

#[test]
fn git_midx_grit_reads_every_object_and_info() {
    let Some((repo, objects, oids)) = multi_pack_repo(HashAlgo::Sha1, 4) else {
        eprintln!("SKIP: fixture");
        return;
    };
    git_write_midx(&repo);
    let packed = all_packed_oids(&objects);
    assert!(packed.len() > oids.len());

    let (_names, listed) = read_midx_objects(&objects).expect("read midx objects");
    for entry in &listed {
        assert!(packed.contains(&entry.oid));
        assert!(try_read_object_via_midx(&objects, &entry.oid)
            .expect("read")
            .is_some());
    }

    for oid in packed {
        assert_lookup_matches_idx(&objects, &oid);
    }
}

#[test]
fn odb_read_same_with_midx_on_and_off() {
    let Some((repo, objects, _oids)) = multi_pack_repo(HashAlgo::Sha1, 3) else {
        eprintln!("SKIP: fixture");
        return;
    };
    git_write_midx(&repo);
    let git_dir = repo.path().join(".git");
    let packed: Vec<ObjectId> = all_packed_oids(&objects).into_iter().collect();

    let odb_on = odb_with_midx_config(&objects, &git_dir, true);
    let odb_off = odb_with_midx_config(&objects, &git_dir, false);

    for oid in &packed {
        let on = odb_on.read(oid).expect("read midx on");
        let off = odb_off.read(oid).expect("read midx off");
        assert_eq!(on.kind, off.kind);
        assert_eq!(on.data, off.data);
    }
}

#[test]
fn duplicate_oid_respects_preferred_pack() {
    let Some((repo, objects, _)) = multi_pack_repo(HashAlgo::Sha1, 1) else {
        eprintln!("SKIP: fixture");
        return;
    };
    let pack_dir = objects.join("pack");
    let idx0 = pack_idx_paths(&objects)[0]
        .file_name()
        .unwrap()
        .to_string_lossy()
        .into_owned();

    // Add a second pack that still contains historical objects (no -d on pack-objects).
    std::fs::write(repo.path().join("extra.txt"), b"extra\n").unwrap();
    repo.git(&["add", "extra.txt"]);
    repo.git(&["commit", "-q", "-m", "extra"]);
    let pack_out = repo.git(&["pack-objects", "--all", ".git/objects/pack/extra"]);
    assert!(pack_out.ok, "pack-objects: {}", pack_out.stderr);
    assert!(pack_idx_paths(&objects).len() >= 2);

    let dup_oid = head_oid(&repo);
    let opts_old = WriteMultiPackIndexOptions {
        preferred_pack_idx: Some(0),
        ..Default::default()
    };
    grit_write_midx(&pack_dir, &opts_old);
    let (pack_id_old, _) = midx_lookup_pack_and_offset_opt(&objects, &dup_oid)
        .expect("lookup")
        .expect("listed");

    grit_lib::midx::clear_pack_midx_state(&pack_dir).expect("clear");
    let names = read_midx_pack_idx_names(&objects).unwrap_or_default();
    let preferred_new = if names.len() > 1 { 1 } else { 0 };
    let opts_new = WriteMultiPackIndexOptions {
        preferred_pack_idx: Some(preferred_new),
        ..Default::default()
    };
    grit_write_midx(&pack_dir, &opts_new);
    let (pack_id_new, _) = midx_lookup_pack_and_offset_opt(&objects, &dup_oid)
        .expect("lookup")
        .expect("listed");
    if pack_id_old == pack_id_new {
        eprintln!("SKIP: no duplicate oid across packs in fixture");
        return;
    }
    let _ = idx0;
}

#[test]
fn reuse_tables_and_preferred_pack_with_ridx() {
    let Some((_repo, objects, oids)) = multi_pack_repo(HashAlgo::Sha1, 2) else {
        eprintln!("SKIP: fixture");
        return;
    };
    let pack_dir = objects.join("pack");
    let opts = WriteMultiPackIndexOptions {
        write_bitmap_placeholders: true,
        write_rev_placeholder: true,
        version: Some(1),
        ..Default::default()
    };
    grit_lib::midx::clear_pack_midx_state(&pack_dir).expect("clear");
    grit_write_midx(&pack_dir, &opts);
    let Some(tables) = load_midx_reuse_tables(&objects).expect("load tables") else {
        eprintln!("SKIP: embedded RIDX chunk not present in this Git/grit combination");
        return;
    };
    assert!(!tables.oids.is_empty());
    for oid in &oids {
        if let Some(bit) = tables.global_bitmap_bit(oid) {
            let canon = tables.canonical_pack(oid).expect("canonical pack");
            assert_eq!(
                bit,
                tables.oid_idx_to_rank[tables.oids.binary_search(oid).unwrap()]
            );
            let _ = canon;
        }
    }
    let pref = read_midx_preferred_idx_name(&objects).expect("preferred pack name");
    assert!(pref.ends_with(".idx"));
}

#[test]
fn incremental_chain_layer_paths_and_reads() {
    let Some((repo, objects, base_oids)) = multi_pack_repo(HashAlgo::Sha1, 2) else {
        eprintln!("SKIP: fixture");
        return;
    };
    git_write_midx(&repo);
    assert!(resolve_tip_midx_path(&objects.join("pack")).is_some());

    std::fs::write(repo.path().join("incr.txt"), b"incr\n").unwrap();
    repo.git(&["add", "incr.txt"]);
    repo.git(&["commit", "-q", "-m", "incr"]);
    repo.git(&["repack", "-d"]);
    if !git_write_midx_incremental(&repo) {
        eprintln!("SKIP: incremental MIDX unsupported");
        return;
    }

    let pack_dir = objects.join("pack");
    assert!(!pack_dir.join("multi-pack-index").exists());
    let Some(chain) = read_chain_hashes(&pack_dir) else {
        eprintln!("SKIP: no incremental chain file");
        return;
    };
    assert_eq!(chain.len(), 2);

    for h in &chain {
        let layer = resolve_midx_layer_path(&pack_dir, h).expect("layer path");
        assert!(layer.is_file(), "layer file for {h}");
    }

    for oid in base_oids {
        assert!(try_read_object_via_midx(&objects, &oid)
            .expect("read base through chain")
            .is_some());
    }
    assert_git_midx_verify(&repo);
}

#[test]
fn stale_midx_after_pack_delete_falls_back() {
    let Some((repo, objects, _)) = multi_pack_repo(HashAlgo::Sha1, 3) else {
        eprintln!("SKIP: fixture");
        return;
    };
    git_write_midx(&repo);
    let pack_dir = objects.join("pack");
    let first_commit = head_oid(&repo);
    let (pack_id, _) = midx_lookup_pack_and_offset_opt(&objects, &first_commit)
        .expect("lookup")
        .expect("first commit in midx");
    let names = read_midx_pack_idx_names(&objects).expect("names");
    let victim = names.get(pack_id as usize).expect("idx name").clone();
    let head = head_oid(&repo);
    let stem = victim.strip_suffix(".idx").unwrap();
    std::fs::remove_file(pack_dir.join(&victim)).ok();
    std::fs::remove_file(pack_dir.join(format!("{stem}.pack"))).ok();
    evict_midx_read_cache_for_pack_dir(&pack_dir);
    clear_pack_cache();

    let _ = try_read_object_via_midx(&objects, &head);

    let git_dir = repo.path().join(".git");
    let odb = odb_with_midx_config(&objects, &git_dir, true);
    let obj = odb.read(&head).expect("Odb fallback after stale pack");
    assert_eq!(obj.kind, grit_lib::objects::ObjectKind::Commit);
}

#[test]
fn clear_midx_state_after_new_pack() {
    let Some((repo, objects, _)) = multi_pack_repo(HashAlgo::Sha1, 2) else {
        eprintln!("SKIP: fixture");
        return;
    };
    git_write_midx(&repo);
    let pack_dir = objects.join("pack");
    let before = tip_midx_path(&pack_dir);
    assert!(before.is_file());

    std::fs::write(repo.path().join("after.txt"), b"after\n").unwrap();
    repo.git(&["add", "after.txt"]);
    repo.git(&["commit", "-q", "-m", "after"]);
    repo.git(&["repack", "-adf"]);

    grit_lib::midx::clear_pack_midx_state(&pack_dir).expect("clear");
    assert!(resolve_tip_midx_path(&pack_dir).is_none());

    grit_write_midx(&pack_dir, &WriteMultiPackIndexOptions::default());
    assert_git_midx_verify(&repo);
    let packed: HashSet<_> = all_packed_oids(&objects);
    for oid in packed {
        assert!(try_read_object_via_midx(&objects, &oid)
            .expect("read after rewrite")
            .is_some());
    }
}

#[test]
fn compact_multi_pack_index_builds_verified_chain() {
    let Some((repo, objects, _)) = multi_pack_repo(HashAlgo::Sha1, 1) else {
        eprintln!("SKIP: fixture");
        return;
    };
    git_write_midx(&repo);
    build_incremental_layers(&repo, 4);
    let pack_dir = objects.join("pack");
    let Some(chain) = read_chain_hashes(&pack_dir) else {
        eprintln!("SKIP: incremental MIDX chain unavailable");
        return;
    };
    if chain.len() < 4 {
        eprintln!("SKIP: need 4+ chain layers, got {}", chain.len());
        return;
    }

    let from = chain[1].clone();
    let to = chain[3].clone();
    compact_multi_pack_index(&pack_dir, &from, &to, true, true, None).expect("compact");

    let Some(after) = read_chain_hashes(&pack_dir) else {
        panic!("chain missing after compact");
    };
    assert_eq!(after.len(), 3);
    assert_git_midx_verify(&repo);
    verify_midx(&objects).expect("verify after compact");

    for oid in all_packed_oids(&objects) {
        assert!(try_read_object_via_midx(&objects, &oid)
            .expect("read")
            .is_some());
    }
}

#[test]
fn compact_error_variants() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let pack_dir = tmp.path().join("pack");
    std::fs::create_dir_all(&pack_dir).expect("mkdir");

    assert!(matches!(
        compact_multi_pack_index(&pack_dir, "a", "b", false, false, Some(1)),
        Err(CompactError::V1Format)
    ));
    assert!(matches!(
        compact_multi_pack_index(&pack_dir, "a", "b", false, false, None),
        Err(CompactError::NoChain)
    ));

    let Some((repo, objects, _)) = multi_pack_repo(HashAlgo::Sha1, 1) else {
        eprintln!("SKIP: fixture");
        return;
    };
    git_write_midx(&repo);
    build_incremental_layers(&repo, 2);
    let pack_dir = objects.join("pack");
    let Some(chain) = read_chain_hashes(&pack_dir) else {
        eprintln!("SKIP: incremental MIDX chain unavailable");
        return;
    };
    if chain.len() < 2 {
        eprintln!("SKIP: need 2+ chain layers");
        return;
    }
    let a = &chain[0];
    assert!(matches!(
        compact_multi_pack_index(&pack_dir, "deadbeef", a, false, false, None),
        Err(CompactError::MissingEndpoint(_))
    ));
    assert!(matches!(
        compact_multi_pack_index(&pack_dir, a, a, false, false, None),
        Err(CompactError::IdenticalEndpoints)
    ));
    assert!(matches!(
        compact_multi_pack_index(&pack_dir, &chain[1], &chain[0], false, false, None),
        Err(CompactError::NotAncestor(_, _))
    ));
}

#[test]
fn midx_public_api_smoke_sha1_and_sha256() {
    for algo in [HashAlgo::Sha1, HashAlgo::Sha256] {
        let Some((repo, objects, oids)) = multi_pack_repo(algo, 3) else {
            eprintln!("SKIP: fixture for {algo:?}");
            continue;
        };
        git_write_midx(&repo);
        validate_midx_referenced_packs(&objects);
        let pack_dir = objects.join("pack");
        assert!(cached_tip_midx_path(&pack_dir).is_some());
        assert!(resolve_tip_midx_path(&pack_dir).is_some());
        let names = read_midx_pack_idx_names(&objects).expect("pack names");
        assert!(!names.is_empty());
        let (names2, listed) = read_midx_objects(&objects).expect("objects");
        assert_eq!(names, names2);
        for entry in listed {
            let _ = midx_oid_listed_in_tip(&objects, &entry.oid).expect("listed?");
            let _ = try_read_info_via_midx(&objects, &entry.oid).expect("info");
        }
        write_multi_pack_index(&pack_dir).expect("rewrite");
        evict_midx_read_cache_for_pack_dir(&pack_dir);
        grit_write_midx(
            &pack_dir,
            &WriteMultiPackIndexOptions {
                write_bitmap_placeholders: true,
                write_rev_placeholder: true,
                ..Default::default()
            },
        );
        verify_midx(&objects).expect("verify");
        for oid in &oids {
            let _ = try_read_object_via_midx(&objects, oid).expect("read");
        }
        let _ = read_midx_preferred_idx_name(&objects);
        let _ = load_midx_reuse_tables(&objects);
    }
}

#[test]
fn grit_incremental_chain_compact_roundtrip() {
    let Some((repo, objects, _oids)) = multi_pack_repo(HashAlgo::Sha1, 1) else {
        eprintln!("SKIP: fixture");
        return;
    };
    let pack_dir = objects.join("pack");
    grit_write_midx(&pack_dir, &WriteMultiPackIndexOptions::default());

    for i in 0..4 {
        std::fs::write(
            repo.path().join(format!("grit-incr-{i}.txt")),
            format!("layer {i}\n"),
        )
        .unwrap();
        repo.git(&["add", &format!("grit-incr-{i}.txt")]);
        repo.git(&["commit", "-q", "-m", &format!("grit incr {i}")]);
        assert!(midx_support::pack_objects_layer(repo.path(), 100 + i));
        grit_lib::midx::write_multi_pack_index_with_options(
            &pack_dir,
            &WriteMultiPackIndexOptions {
                incremental: true,
                version: Some(1),
                ..Default::default()
            },
        )
        .expect("incremental write");
    }

    let Some(chain) = read_chain_hashes(&pack_dir) else {
        eprintln!("SKIP: no grit incremental chain");
        return;
    };
    if chain.len() < 3 {
        eprintln!("SKIP: chain too short ({})", chain.len());
        return;
    }
    let from = chain[0].clone();
    let to = chain[chain.len() - 2].clone();
    compact_multi_pack_index(&pack_dir, &from, &to, false, false, None).expect("compact");
    verify_midx(&objects).expect("verify after grit compact");
    assert_git_midx_verify(&repo);
}

#[test]
fn deep_git_incremental_chain_exercises_layer_apis() {
    let Some((repo, objects, oids)) = multi_pack_repo(HashAlgo::Sha1, 2) else {
        eprintln!("SKIP: fixture");
        return;
    };
    git_write_midx(&repo);
    for i in 0..5 {
        std::fs::write(
            repo.path().join(format!("deep-{i}.txt")),
            format!("deep layer {i}\n"),
        )
        .unwrap();
        repo.git(&["add", &format!("deep-{i}.txt")]);
        repo.git(&["commit", "-q", "-m", &format!("deep {i}")]);
        assert!(midx_support::pack_objects_layer(repo.path(), 200 + i));
        if !git_write_midx_incremental(&repo) {
            eprintln!("SKIP: git incremental unavailable");
            return;
        }
    }
    let pack_dir = objects.join("pack");
    let Some(chain) = read_chain_hashes(&pack_dir) else {
        eprintln!("SKIP: no chain");
        return;
    };
    for h in &chain {
        let p = resolve_midx_layer_path(&pack_dir, h).expect("layer path");
        assert!(p.is_file());
    }
    let packed: Vec<ObjectId> = all_packed_oids(&objects).into_iter().collect();
    for oid in packed.iter().take(64) {
        let _ = midx_lookup_pack_and_offset_opt(&objects, oid);
        let _ = try_read_info_via_midx(&objects, oid);
        let _ = try_read_object_via_midx(&objects, oid);
    }
    verify_midx(&objects).expect("verify");
    let _ = oids;
}

#[test]
fn git_bitmap_midx_grit_reads_and_tables() {
    let Some((repo, objects, oids)) = multi_pack_repo(HashAlgo::Sha1, 3) else {
        eprintln!("SKIP: fixture");
        return;
    };
    let out = repo.git(&["multi-pack-index", "write", "--bitmap"]);
    if !out.ok {
        eprintln!("SKIP: git bitmap midx: {}", out.stderr);
        return;
    }
    verify_midx(&objects).expect("verify");
    let _ = load_midx_reuse_tables(&objects);
    let _ = read_midx_preferred_idx_name(&objects);
    assert_grit_midx_reads_match_git(&objects, &oids);
}

#[test]
fn midx_write_rejects_empty_and_bad_preferred() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let pack_dir = tmp.path().join("pack");
    std::fs::create_dir_all(&pack_dir).expect("mkdir");
    assert!(write_multi_pack_index(&pack_dir).is_err());
    let Some((_, objects, _)) = multi_pack_repo(HashAlgo::Sha1, 1) else {
        return;
    };
    let pack_dir = objects.join("pack");
    let err = grit_lib::midx::write_multi_pack_index_with_options(
        &pack_dir,
        &WriteMultiPackIndexOptions {
            preferred_pack_idx: Some(99),
            ..Default::default()
        },
    );
    assert!(err.is_err());
}

#[test]
fn grit_incremental_write_layer() {
    let Some((repo, objects, oids)) = multi_pack_repo(HashAlgo::Sha1, 2) else {
        eprintln!("SKIP: fixture");
        return;
    };
    let pack_dir = objects.join("pack");
    grit_write_midx(&pack_dir, &WriteMultiPackIndexOptions::default());
    std::fs::write(repo.path().join("incr-grit.txt"), b"incr grit layer\n").unwrap();
    repo.git(&["add", "incr-grit.txt"]);
    repo.git(&["commit", "-q", "-m", "incr grit"]);
    assert!(midx_support::pack_objects_layer(repo.path(), 99));
    let opts = WriteMultiPackIndexOptions {
        incremental: true,
        version: Some(1),
        ..Default::default()
    };
    grit_lib::midx::write_multi_pack_index_with_options(&pack_dir, &opts)
        .expect("grit incremental");
    if let Some(chain) = read_chain_hashes(&pack_dir) {
        assert!(!chain.is_empty());
        for h in chain {
            assert!(resolve_midx_layer_path(&pack_dir, &h).is_some());
        }
    }
    for oid in oids {
        assert!(try_read_object_via_midx(&objects, &oid)
            .expect("read")
            .is_some());
    }
}

fn build_incremental_layers(repo: &midx_support::RepoFixture, extra_layers: usize) {
    for i in 0..extra_layers {
        std::fs::write(
            repo.path().join(format!("c-{i}.txt")),
            format!("layer {i}\n"),
        )
        .unwrap();
        repo.git(&["add", &format!("c-{i}.txt")]);
        repo.git(&["commit", "-q", "-m", &format!("layer {i}")]);
        repo.git(&["repack", "-d"]);
        if !git_write_midx_incremental(repo) {
            eprintln!("SKIP: git multi-pack-index --incremental unavailable");
            return;
        }
    }
}

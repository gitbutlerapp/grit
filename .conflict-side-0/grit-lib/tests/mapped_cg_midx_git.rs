//! Git-written commit-graph and MIDX: mapped file backing matches owned-byte parsing.
//!
//! Fixtures are sized above [`PackData`]'s owned-read threshold (8 KiB) so the hot path
//! uses memory-mapped bytes, not in-memory copies.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Command;

use grit_lib::objects::commit_graph::{
    commit_graph_chain_from_layers_for_test, CommitGraphChain, CommitGraphLayer,
};
use grit_lib::objects::midx::{
    load_midx_reuse_tables_from_path, midx_lookup_pack_and_offset_opt, read_midx_objects,
    read_midx_pack_idx_names, try_read_object_via_midx, write_multi_pack_index_with_options,
    WriteMultiPackIndexOptions,
};
use grit_lib::objects::pack::{
    clear_pack_cache, read_object_from_pack_at_offset, read_pack_index_cached,
};
/// One byte above PackData's 8 KiB owned-read threshold (8192).
const MMAP_FILE_MIN_LEN: u64 = 8193;

fn git_ok() -> bool {
    Command::new("git")
        .arg("--version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

fn git_out(dir: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .current_dir(dir)
        .args(args)
        .env("GIT_AUTHOR_NAME", "T")
        .env("GIT_AUTHOR_EMAIL", "t@example.com")
        .env("GIT_COMMITTER_NAME", "T")
        .env("GIT_COMMITTER_EMAIL", "t@example.com")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .env("GIT_CONFIG_COUNT", "2")
        .env("GIT_CONFIG_KEY_0", "gc.auto")
        .env("GIT_CONFIG_VALUE_0", "0")
        .env("GIT_CONFIG_KEY_1", "maintenance.auto")
        .env("GIT_CONFIG_VALUE_1", "false")
        .output()
        .expect("git");
    assert!(out.status.success(), "git {args:?}");
    String::from_utf8(out.stdout).expect("utf8")
}

fn git(dir: &Path, args: &[&str]) {
    git_out(dir, args);
}

fn assert_file_forces_mmap(path: &Path) {
    let len = std::fs::metadata(path)
        .unwrap_or_else(|e| panic!("stat {}: {e}", path.display()))
        .len();
    assert!(
        len >= MMAP_FILE_MIN_LEN,
        "fixture {} is {len} bytes; need at least {MMAP_FILE_MIN_LEN} to force mmap",
        path.display()
    );
}

/// Enough reachable commits that Git's commit-graph file exceeds the mmap threshold.
fn write_large_reachable_commit_graph(dir: &Path) -> PathBuf {
    git(dir, &["init", "-q", "-b", "main"]);
    git(dir, &["commit", "--allow-empty", "-q", "-m", "root"]);
    for i in 0..350 {
        git(
            dir,
            &["commit", "--allow-empty", "-q", "-m", &format!("c{i}")],
        );
    }
    git(dir, &["commit-graph", "write", "--reachable"]);
    let graph = dir.join(".git/objects/info/commit-graph");
    assert_file_forces_mmap(&graph);
    graph
}

/// Many unique blobs in one pack plus a Git-written MIDX large enough to mmap.
fn write_large_single_pack_midx(dir: &Path) -> (PathBuf, PathBuf) {
    git(dir, &["init", "-q", "-b", "main"]);
    for i in 0..450 {
        std::fs::write(
            dir.join(format!("blob-{i}.txt")),
            format!("payload-{i}-{}", "x".repeat(48)),
        )
        .expect("write blob file");
        git(dir, &["add", &format!("blob-{i}.txt")]);
        git(dir, &["commit", "-q", "-m", &format!("commit-{i}")]);
    }
    git(dir, &["repack", "-adf"]);
    let pack_dir = dir.join(".git/objects/pack");
    let preferred_idx = std::fs::read_dir(&pack_dir)
        .expect("pack dir")
        .filter_map(Result::ok)
        .map(|e| e.file_name())
        .find(|n| {
            let s = n.to_string_lossy();
            s.starts_with("pack-") && s.ends_with(".idx")
        })
        .expect("pack index");
    write_multi_pack_index_with_options(
        &pack_dir,
        &WriteMultiPackIndexOptions {
            preferred_pack_name: Some(preferred_idx.to_string_lossy().into_owned()),
            ..WriteMultiPackIndexOptions::default()
        },
    )
    .expect("grit midx with embedded RIDX");
    let objects = dir.join(".git/objects");
    let midx = objects.join("pack/multi-pack-index");
    assert_file_forces_mmap(&midx);
    (objects, midx)
}

#[test]
fn git_commit_graph_chain_mapped_matches_owned_layers() {
    if !git_ok() {
        eprintln!("SKIP: git unavailable");
        return;
    }
    let dir = tempfile::tempdir().expect("tempdir");
    write_large_reachable_commit_graph(dir.path());

    let objects = dir.path().join(".git/objects");
    let mapped = CommitGraphChain::try_load(&objects)
        .expect("load")
        .expect("chain");
    let mut owned_layers = Vec::new();
    for path in mapped.layer_paths_oldest_first() {
        let raw = std::fs::read(&path).expect("read layer");
        owned_layers.push(CommitGraphLayer::try_parse(path, raw).expect("owned layer"));
    }
    owned_layers.reverse();
    let owned = commit_graph_chain_from_layers_for_test(owned_layers);
    assert_eq!(mapped.num_layers(), owned.num_layers());
    assert_eq!(mapped.total_commits(), owned.total_commits());
    for oid in mapped.all_oids_in_order() {
        assert_eq!(mapped.graph_commit(&oid), owned.graph_commit(&oid));
        assert_eq!(
            mapped.commit_generation_for_walk(&oid),
            owned.commit_generation_for_walk(&oid)
        );
    }
}

#[test]
fn git_midx_mapped_read_matches_owned_byte_lookup() {
    if !git_ok() {
        eprintln!("SKIP: git unavailable");
        return;
    }
    let dir = tempfile::tempdir().expect("tempdir");
    let (objects, midx_path) = write_large_single_pack_midx(dir.path());
    let pack_dir = objects.join("pack");

    let (_pack_names, refs) = read_midx_objects(&objects).expect("midx object list");
    assert!(!refs.is_empty(), "expected MIDX entries");

    let mut mapped_lookups = HashMap::new();
    let mut mapped_objects = HashMap::new();
    for mref in &refs {
        let oid = mref.oid;
        mapped_lookups.insert(
            oid,
            midx_lookup_pack_and_offset_opt(&objects, &oid).expect("mapped lookup"),
        );
        let obj = try_read_object_via_midx(&objects, &oid)
            .expect("mapped read")
            .expect("object in midx");
        mapped_objects.insert(oid, obj);
    }

    grit_lib::objects::midx::evict_midx_read_cache_for_pack_dir(&pack_dir);
    clear_pack_cache();

    let tables = load_midx_reuse_tables_from_path(&midx_path)
        .expect("owned tables")
        .expect("non-empty midx");
    let idx_names = read_midx_pack_idx_names(&objects).expect("pack names");
    assert_eq!(tables.oids.len(), refs.len());

    for (idx, oid) in tables.oids.iter().enumerate() {
        let owned_loc = tables.pack_and_offset[idx];
        assert_eq!(
            mapped_lookups.get(oid).copied().flatten(),
            Some(owned_loc),
            "lookup mismatch for {}",
            oid.to_hex()
        );
        let pack_name = &idx_names[owned_loc.0 as usize];
        let idx_path = pack_dir.join(pack_name);
        let pack_idx = read_pack_index_cached(&idx_path).expect("pack index");
        let owned_obj =
            read_object_from_pack_at_offset(&pack_idx, owned_loc.1).expect("owned pack read");
        let mapped_obj = mapped_objects.get(oid).expect("mapped object");
        assert_eq!(mapped_obj.data, owned_obj.data, "{}", oid.to_hex());
        assert_eq!(mapped_obj.kind, owned_obj.kind);
    }
}

//! Deterministic object-database fixtures for Criterion micro-benchmarks.

#![allow(clippy::expect_used, clippy::unwrap_used)]
//!
//! Builds repositories with `grit-lib` and uses the system `git` binary only for
//! `index-pack` / `repack`, which is allowed in benchmark harness code.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::OnceLock;

use flate2::write::ZlibEncoder;
use flate2::Compression;
use grit_lib::delta_encode::encode_lcp_delta;
use grit_lib::objects::{ObjectId, ObjectKind};
use grit_lib::odb::Odb;
use grit_lib::pack::{clear_pack_cache, read_pack_index, PackIndex};
use grit_lib::transfer::{build_pack, PackBuildOptions};
use tempfile::TempDir;

const GIT_ENV: &[(&str, &str)] = &[
    ("GIT_CONFIG_GLOBAL", "/dev/null"),
    ("GIT_CONFIG_SYSTEM", "/dev/null"),
    ("GIT_CONFIG_NOSYSTEM", "1"),
    ("GIT_AUTHOR_NAME", "Bench"),
    ("GIT_AUTHOR_EMAIL", "bench@grit-scm.test"),
    ("GIT_COMMITTER_NAME", "Bench"),
    ("GIT_COMMITTER_EMAIL", "bench@grit-scm.test"),
    ("GIT_AUTHOR_DATE", "1700000000 +0000"),
    ("GIT_COMMITTER_DATE", "1700000000 +0000"),
];

fn git_ok(dir: &Path, args: &[&str]) -> bool {
    let mut cmd = Command::new("git");
    cmd.current_dir(dir).args(args);
    for (k, v) in GIT_ENV {
        cmd.env(k, v);
    }
    cmd.status().ok().is_some_and(|s| s.success())
}

fn git_out(dir: &Path, args: &[&str]) -> Option<String> {
    let mut cmd = Command::new("git");
    cmd.current_dir(dir).args(args);
    for (k, v) in GIT_ENV {
        cmd.env(k, v);
    }
    let out = cmd.output().ok()?;
    if !out.status.success() {
        return None;
    }
    Some(String::from_utf8_lossy(&out.stdout).trim().to_owned())
}

fn store_bytes(kind: ObjectKind, data: &[u8]) -> Vec<u8> {
    let header = format!("{} {}\0", kind, data.len());
    let mut out = Vec::with_capacity(header.len() + data.len());
    out.extend_from_slice(header.as_bytes());
    out.extend_from_slice(data);
    out
}

fn deflate_store_bytes(store: &[u8]) -> Vec<u8> {
    let mut enc = ZlibEncoder::new(Vec::new(), Compression::default());
    std::io::Write::write_all(&mut enc, store).expect("zlib deflate");
    enc.finish().expect("zlib finish")
}

fn index_pack_in_dir(dir: &Path, pack_bytes: &[u8], stem: &str) -> PackIndex {
    let pack_path = dir.join(format!("{stem}.pack"));
    std::fs::write(&pack_path, pack_bytes).expect("write pack bytes");
    let mut cmd = Command::new("git");
    cmd.current_dir(dir).args([
        "index-pack",
        pack_path.file_name().unwrap().to_str().unwrap(),
    ]);
    for (k, v) in GIT_ENV {
        cmd.env(k, v);
    }
    let out = cmd.output().expect("spawn git index-pack");
    assert!(
        out.status.success(),
        "git index-pack failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let idx_path = dir.join(format!("{stem}.idx"));
    read_pack_index(&idx_path).expect("read generated idx")
}

fn install_indexed_pack(objects_dir: &Path, stem: &str, pack_bytes: &[u8]) -> PackIndex {
    let pack_dir = objects_dir.join("pack");
    std::fs::create_dir_all(&pack_dir).expect("pack dir");
    let scratch = tempfile::tempdir().expect("index-pack scratch");
    let idx = index_pack_in_dir(scratch.path(), pack_bytes, stem);
    let dest_pack = pack_dir.join(format!("{stem}.pack"));
    let dest_idx = pack_dir.join(format!("{stem}.idx"));
    std::fs::copy(&idx.pack_path, &dest_pack).expect("copy pack");
    std::fs::copy(&idx.idx_path, &dest_idx).expect("copy idx");
    clear_pack_cache();
    read_pack_index(&dest_idx).expect("read installed idx")
}

fn typical_tree_bytes() -> Vec<u8> {
    let blob_body = b"typical blob payload for zlib bench\n";
    let odb = Odb::new(tempfile::tempdir().expect("temp").path());
    let blob_oid = odb.hash(ObjectKind::Blob, blob_body);
    let mut tree = Vec::new();
    tree.extend_from_slice(b"100644 README.md\0");
    tree.extend_from_slice(blob_oid.as_bytes());
    tree.extend_from_slice(b"040000 src\0");
    let sub_blob = odb.hash(ObjectKind::Blob, b"mod rs\n");
    let mut sub = Vec::new();
    sub.extend_from_slice(b"100644 lib.rs\0");
    sub.extend_from_slice(sub_blob.as_bytes());
    let sub_tree = odb.hash(ObjectKind::Tree, &sub);
    tree.extend_from_slice(sub_tree.as_bytes());
    tree
}

fn build_grit_loose_repo(blob_body: &[u8]) -> (TempDir, Odb, ObjectId) {
    let dir = tempfile::tempdir().expect("tempdir");
    let objects = dir.path().join("objects");
    std::fs::create_dir_all(&objects).expect("objects dir");
    let odb = Odb::new(&objects);
    let oid = odb.write(ObjectKind::Blob, blob_body).expect("write loose");
    (dir, odb, oid)
}

fn build_whole_blob_pack(body: &[u8]) -> (Vec<u8>, ObjectId) {
    let (_dir, odb, oid) = build_grit_loose_repo(body);
    let pack = build_pack(
        &odb,
        &[oid],
        &[],
        &PackBuildOptions {
            delta: false,
            ..PackBuildOptions::default()
        },
    )
    .expect("whole-object pack");
    (pack, oid)
}

fn build_delta_chain_pack(depth: usize) -> Option<(Vec<u8>, ObjectId)> {
    let tmp = tempfile::tempdir().ok()?;
    let dir = tmp.path();
    if !git_ok(dir, &["init", "-q", "-b", "main", "."]) {
        return None;
    }
    let mut body = String::new();
    for i in 0..128 {
        body.push_str(&format!("seed {i:04}\n"));
    }
    for rev in 0..depth {
        body.push_str(&format!("layer-{rev}\n"));
        std::fs::write(dir.join("chain.txt"), body.as_bytes()).ok()?;
        git_ok(dir, &["add", "chain.txt"]);
        git_ok(dir, &["commit", "-q", "-m", &format!("c{rev}")]);
    }
    let tip_hex = git_out(dir, &["rev-parse", "HEAD"])?;
    let tip = ObjectId::from_hex(&tip_hex).ok()?;
    let git_dir = dir.join(".git");
    let odb = Odb::new(&git_dir.join("objects")).with_config_git_dir(git_dir);
    let pack = build_pack(
        &odb,
        &[tip],
        &[],
        &PackBuildOptions {
            delta: true,
            max_depth: depth,
            use_ofs_delta: true,
            ..PackBuildOptions::default()
        },
    )
    .ok()?;
    Some((pack, tip))
}

fn build_large_pack_index(
    objects_dir: &Path,
    object_count: usize,
) -> Option<(PackIndex, ObjectId)> {
    let tmp = tempfile::tempdir().ok()?;
    let dir = tmp.path();
    if !git_ok(dir, &["init", "-q", "-b", "main", "."]) {
        return None;
    }
    for i in 0..object_count {
        let path = dir.join(format!("file-{i:05}.txt"));
        std::fs::write(&path, format!("deterministic payload {i}\n")).ok()?;
        git_ok(dir, &["add", path.file_name().unwrap().to_str().unwrap()]);
        git_ok(dir, &["commit", "-q", "-m", &format!("o{i}")]);
    }
    git_ok(dir, &["repack", "-a", "-d"]);
    let tip_hex = git_out(dir, &["rev-parse", "HEAD"])?;
    let tip = ObjectId::from_hex(&tip_hex).ok()?;
    let git_dir = dir.join(".git");
    let pack_dir = git_dir.join("objects/pack");
    let src_idx = std::fs::read_dir(&pack_dir)
        .ok()?
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .find(|p| p.extension().is_some_and(|x| x == "idx"))?;
    let src_pack = src_idx.with_extension("pack");
    let dest_pack = objects_dir.join("pack/large.pack");
    let dest_idx = objects_dir.join("pack/large.idx");
    std::fs::create_dir_all(objects_dir.join("pack")).ok()?;
    std::fs::copy(&src_pack, &dest_pack).ok()?;
    std::fs::copy(&src_idx, &dest_idx).ok()?;
    clear_pack_cache();
    let idx = read_pack_index(&dest_idx).ok()?;
    Some((idx, tip))
}

/// Shared, lazily-built fixtures for object micro-benchmarks.
pub struct ObjectBenchFixtures {
    pub _root: TempDir,
    pub _objects_dir: PathBuf,
    pub odb: Odb,
    pub sha1_buffers: [(usize, Vec<u8>); 3],
    pub blob_store: Vec<u8>,
    pub tree_body: Vec<u8>,
    pub tree_store: Vec<u8>,
    pub blob_zlib: Vec<u8>,
    pub tree_zlib: Vec<u8>,
    pub loose_blob_oid: ObjectId,
    pub packed_whole_oid: ObjectId,
    pub packed_whole_idx: PackIndex,
    pub packed_delta_oid: ObjectId,
    pub packed_delta_idx: PackIndex,
    pub small_idx: PackIndex,
    pub small_hit: ObjectId,
    pub small_miss: ObjectId,
    pub large_idx: PackIndex,
    pub large_hit: ObjectId,
    pub large_miss: ObjectId,
    pub delta_base: Vec<u8>,
    pub delta_bytes: Vec<u8>,
}

impl ObjectBenchFixtures {
    fn build() -> Self {
        const BLOB_BODY: &[u8] = b"A typical text blob used for object micro-benchmarks.\n\
            It is long enough to exercise zlib and SHA-1 at a realistic size.\n\
            Line three adds a little more entropy for delta encoding.\n";
        let blob_body = BLOB_BODY;
        let tree_body = typical_tree_bytes();
        let blob_store = store_bytes(ObjectKind::Blob, blob_body);
        let tree_store = store_bytes(ObjectKind::Tree, &tree_body);
        let blob_zlib = deflate_store_bytes(&blob_store);
        let tree_zlib = deflate_store_bytes(&tree_store);

        let sha1_buffers = [
            (1024, vec![0x5Au8; 1024]),
            (64 * 1024, vec![0xA5u8; 64 * 1024]),
            (16 * 1024 * 1024, vec![0x3Cu8; 16 * 1024 * 1024]),
        ];

        let (root, odb, loose_blob_oid) = build_grit_loose_repo(blob_body);
        let objects_dir = root.path().join("objects");

        let (whole_pack, packed_whole_oid) = build_whole_blob_pack(blob_body);
        let packed_whole_idx = install_indexed_pack(&objects_dir, "whole", &whole_pack);

        let (delta_pack, packed_delta_oid) =
            build_delta_chain_pack(80).unwrap_or_else(|| build_whole_blob_pack(blob_body));
        let packed_delta_idx = install_indexed_pack(&objects_dir, "delta", &delta_pack);

        let small_idx = packed_whole_idx.clone();
        let small_hit = packed_whole_oid;
        let small_miss =
            ObjectId::from_hex("deadbeefdeadbeefdeadbeefdeadbeefdeadbeef").expect("miss oid");

        let (large_idx, large_hit) = build_large_pack_index(&objects_dir, 800)
            .unwrap_or_else(|| (packed_whole_idx.clone(), packed_whole_oid));
        let large_miss =
            ObjectId::from_hex("cafebabecafebabecafebabecafebabecafebabe").expect("miss oid");

        let delta_base = blob_body.to_vec();
        let delta_target = {
            let mut t = delta_base.clone();
            t.extend_from_slice(b"\nextra delta suffix\n");
            t
        };
        let delta_bytes = encode_lcp_delta(&delta_base, &delta_target).expect("encode delta");

        Self {
            _root: root,
            _objects_dir: objects_dir,
            odb,
            sha1_buffers,
            blob_store,
            tree_body: tree_body.to_vec(),
            tree_store,
            blob_zlib,
            tree_zlib,
            loose_blob_oid,
            packed_whole_oid,
            packed_whole_idx,
            packed_delta_oid,
            packed_delta_idx,
            small_idx,
            small_hit,
            small_miss,
            large_idx,
            large_hit,
            large_miss,
            delta_base,
            delta_bytes,
        }
    }

    /// Process-wide singleton; building packs is expensive.
    #[must_use]
    pub fn global() -> &'static Self {
        static FIX: OnceLock<ObjectBenchFixtures> = OnceLock::new();
        FIX.get_or_init(ObjectBenchFixtures::build)
    }
}

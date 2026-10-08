//! Deterministic object-database fixtures for Criterion micro-benchmarks.
//!
//! Builds repositories with `grit-lib` and uses the system `git` binary only for
//! `index-pack` / `repack`, which is allowed in benchmark harness code.

#![allow(clippy::expect_used, clippy::unwrap_used)]

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::OnceLock;

use flate2::write::ZlibEncoder;
use flate2::Compression;
use grit_lib::delta_encode::encode_lcp_delta;
use grit_lib::error::Result;
use grit_lib::objects::{serialize_commit, CommitData, ObjectId, ObjectKind};
use grit_lib::odb::Odb;
use grit_lib::pack::{
    clear_pack_cache, max_verify_pack_delta_depth, read_pack_index, verify_pack_and_collect,
    PackIndex, PackedType,
};
use grit_lib::porcelain::add::{stage, StageOptions};
use grit_lib::progress::NullProgress;
use grit_lib::refs;
use grit_lib::repo::{init_repository, Repository};
use grit_lib::transfer::{build_pack, PackBuildOptions};
use grit_lib::write_tree::{write_tree_update_index, WriteTreeFlags};
use tempfile::TempDir;

const GIT_ENV: &[(&str, &str)] = &[
    ("GIT_CONFIG_GLOBAL", "/dev/null"),
    ("GIT_CONFIG_SYSTEM", "/dev/null"),
    ("GIT_CONFIG_NOSYSTEM", "1"),
];

const BENCH_IDENT: &str = "Bench <bench@grit-scm.test> 1700000000 +0000";
const MIN_DEEP_DELTA_DEPTH: u64 = 50;

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

fn git_index_pack(dir: &Path, pack_path: &Path) -> Result<()> {
    let mut cmd = Command::new("git");
    cmd.current_dir(dir).args([
        "index-pack",
        pack_path
            .file_name()
            .and_then(|s| s.to_str())
            .expect("pack file name"),
    ]);
    for (k, v) in GIT_ENV {
        cmd.env(k, v);
    }
    let out = cmd.output().map_err(grit_lib::error::Error::Io)?;
    if !out.status.success() {
        return Err(grit_lib::error::Error::Message(format!(
            "git index-pack failed: {}",
            String::from_utf8_lossy(&out.stderr)
        )));
    }
    Ok(())
}

fn git_repack(repo_root: &Path) -> Result<()> {
    let mut cmd = Command::new("git");
    cmd.current_dir(repo_root).args(["repack", "-a", "-d"]);
    for (k, v) in GIT_ENV {
        cmd.env(k, v);
    }
    let out = cmd.output().map_err(grit_lib::error::Error::Io)?;
    if !out.status.success() {
        return Err(grit_lib::error::Error::Message(format!(
            "git repack failed: {}",
            String::from_utf8_lossy(&out.stderr)
        )));
    }
    Ok(())
}

fn index_pack_in_dir(dir: &Path, pack_bytes: &[u8], stem: &str) -> PackIndex {
    let pack_path = dir.join(format!("{stem}.pack"));
    std::fs::write(&pack_path, pack_bytes).expect("write pack bytes");
    git_index_pack(dir, &pack_path).expect("index-pack");
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

fn grit_record_commit(
    repo: &Repository,
    parent: Option<ObjectId>,
    message: &str,
) -> Result<ObjectId> {
    let mut index = repo.load_index()?;
    let tree = write_tree_update_index(&repo.odb, &mut index, "", WriteTreeFlags::silent())?;
    repo.write_index(&mut index)?;
    let commit_data = CommitData {
        tree,
        parents: parent.into_iter().collect(),
        author: BENCH_IDENT.to_owned(),
        committer: BENCH_IDENT.to_owned(),
        author_raw: Vec::new(),
        committer_raw: Vec::new(),
        encoding: None,
        message: format!("{message}\n"),
        raw_message: None,
    };
    let bytes = serialize_commit(&commit_data);
    let oid = repo.odb.write(ObjectKind::Commit, &bytes)?;
    refs::write_ref(&repo.git_dir, "refs/heads/main", &oid)?;
    Ok(oid)
}

fn grit_commit_file(
    repo: &Repository,
    parent: Option<ObjectId>,
    rel_path: &str,
    contents: &[u8],
    message: &str,
) -> Result<ObjectId> {
    let work_tree = repo
        .work_tree
        .as_deref()
        .expect("bench repo must have a work tree");
    let path = work_tree.join(rel_path);
    if let Some(parent_dir) = path.parent() {
        std::fs::create_dir_all(parent_dir).map_err(grit_lib::error::Error::Io)?;
    }
    std::fs::write(&path, contents).map_err(grit_lib::error::Error::Io)?;
    let mut progress = NullProgress;
    stage(
        repo,
        &StageOptions {
            pathspecs: vec![rel_path.to_owned()],
            ..StageOptions::default()
        },
        &mut progress,
    )?;
    grit_record_commit(repo, parent, message)
}

fn deepest_deltified_oid(pack_bytes: &[u8], stem: &str) -> (ObjectId, u64) {
    let scratch = tempfile::tempdir().expect("verify scratch");
    let idx = index_pack_in_dir(scratch.path(), pack_bytes, stem);
    let records = verify_pack_and_collect(&idx.idx_path).expect("verify-pack metadata");
    let max_depth = max_verify_pack_delta_depth(&records);
    assert!(
        max_depth >= MIN_DEEP_DELTA_DEPTH,
        "expected delta chain depth >= {MIN_DEEP_DELTA_DEPTH}, got {max_depth}"
    );
    let best = records
        .iter()
        .filter(|r| matches!(r.packed_type, PackedType::RefDelta | PackedType::OfsDelta))
        .max_by_key(|r| r.depth.unwrap_or(0))
        .expect("pack must contain at least one deltified object");
    let depth = best.depth.expect("delta record must have depth");
    assert!(
        depth >= MIN_DEEP_DELTA_DEPTH,
        "deepest delta object depth {depth} below minimum {MIN_DEEP_DELTA_DEPTH}"
    );
    let oid = ObjectId::from_bytes(&best.oid).expect("idx oid bytes");
    (oid, depth)
}

fn build_delta_chain_pack(depth: usize) -> (Vec<u8>, ObjectId, u64) {
    let tmp = tempfile::tempdir().expect("delta chain tempdir");
    let repo = init_repository(tmp.path(), false, "main", None, "files")
        .expect("init bench repo for delta chain");
    let mut parent = None;
    let mut body = String::new();
    for i in 0..128 {
        body.push_str(&format!("seed {i:04}\n"));
    }
    for rev in 0..depth {
        body.push_str(&format!("layer-{rev}\n"));
        parent = Some(
            grit_commit_file(
                &repo,
                parent,
                "chain.txt",
                body.as_bytes(),
                &format!("c{rev}"),
            )
            .expect("grit commit in delta chain"),
        );
    }
    let tip = parent.expect("delta chain must have at least one commit");
    let odb = repo.odb.with_config_git_dir(repo.git_dir.clone());
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
    .expect("build delta pack");
    let (delta_oid, chain_depth) = deepest_deltified_oid(&pack, "verify-delta");
    (pack, delta_oid, chain_depth)
}

pub fn build_packed_exists_local_fixture(object_count: usize) -> (TempDir, Odb, Vec<ObjectId>) {
    let root = tempfile::tempdir().expect("exists_local fixture tempdir");
    let objects = root.path().join("objects");
    let (idx, _tip) = build_large_pack_index(&objects, object_count);
    let odb = Odb::new(&objects);
    let mut oids = Vec::with_capacity(object_count.min(idx.entries.len()));
    for e in &idx.entries {
        if e.oid.len() == 20 {
            if let Ok(oid) = ObjectId::from_bytes(&e.oid) {
                oids.push(oid);
                if oids.len() >= object_count {
                    break;
                }
            }
        }
    }
    assert!(
        oids.len() >= object_count.min(1000),
        "expected at least {} packed oids, got {}",
        object_count.min(1000),
        oids.len()
    );
    (root, odb, oids)
}

fn build_large_pack_index(objects_dir: &Path, object_count: usize) -> (PackIndex, ObjectId) {
    let tmp = tempfile::tempdir().expect("large pack tempdir");
    let repo = init_repository(tmp.path(), false, "main", None, "files").expect("init repo");
    let mut parent = None;
    for i in 0..object_count {
        let rel = format!("file-{i:05}.txt");
        let body = format!("deterministic payload {i}\n");
        parent = Some(
            grit_commit_file(&repo, parent, &rel, body.as_bytes(), &format!("o{i}"))
                .expect("grit commit"),
        );
    }
    let tip = parent.expect("at least one commit");
    git_repack(tmp.path()).expect("git repack");
    let pack_dir = repo.git_dir.join("objects/pack");
    let _keep_repo = (&repo, &tmp);
    let src_idx = std::fs::read_dir(&pack_dir)
        .expect("read pack dir")
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .find(|p| p.extension().is_some_and(|x| x == "idx"))
        .expect("repack must produce a .idx");
    assert!(
        src_idx.metadata().expect("idx metadata").len() > 4096,
        "large index fixture should be non-trivial"
    );
    let src_pack = src_idx.with_extension("pack");
    let dest_pack = objects_dir.join("pack/large.pack");
    let dest_idx = objects_dir.join("pack/large.idx");
    std::fs::create_dir_all(objects_dir.join("pack")).expect("pack dir");
    std::fs::copy(&src_pack, &dest_pack).expect("copy pack");
    std::fs::copy(&src_idx, &dest_idx).expect("copy idx");
    clear_pack_cache();
    let idx = read_pack_index(&dest_idx).expect("read large idx");
    assert!(
        idx.entries.len() >= object_count,
        "large idx should list many objects, got {}",
        idx.entries.len()
    );
    (idx, tip)
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
    pub packed_delta_chain_depth: u64,
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

        let (delta_pack, packed_delta_oid, packed_delta_chain_depth) = build_delta_chain_pack(80);
        let packed_delta_idx = install_indexed_pack(&objects_dir, "delta", &delta_pack);

        let small_idx = packed_whole_idx.clone();
        let small_hit = packed_whole_oid;
        let small_miss =
            ObjectId::from_hex("deadbeefdeadbeefdeadbeefdeadbeefdeadbeef").expect("miss oid");

        let (large_idx, large_hit) = build_large_pack_index(&objects_dir, 800);
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
            packed_delta_chain_depth,
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

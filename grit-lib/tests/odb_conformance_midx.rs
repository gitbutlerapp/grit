//! [`MidxObjects`] conformance and Git MIDX round-trips (8-pack repos).

#![allow(clippy::unwrap_used, clippy::expect_used)]

pub mod midx_support;

use std::collections::{BTreeMap, HashSet};
use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::ops::ControlFlow;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Arc;

use flate2::write::ZlibEncoder;
use flate2::Compression;
use grit_lib::hash;
use grit_lib::midx::{write_multi_pack_index_with_options, WriteMultiPackIndexOptions};
use grit_lib::objects::{HashAlgo, Object, ObjectId, ObjectKind};
use grit_lib::odb::store::{
    MidxObjects, MidxObjectsStatus, ObjectStore, ObjectStream, PackedObjects,
};
use grit_lib::pack::{clear_pack_cache, write_v2_pack_index};
use grit_lib::pack_store::PackStore;
use grit_test_support::git_supports_sha256;
use grit_test_support::objects::HashAlgo as FixtureHashAlgo;
use grit_test_support::odb_conformance::run_read_suite;
use tempfile::TempDir;

use midx_support::{
    all_packed_oids, git_available, git_write_midx, head_oid, multi_pack_repo, pack_objects_layer,
    read_chain_hashes, tip_midx_path,
};

struct MidxBackedStore {
    _root: TempDir,
    inner: MidxObjects,
}

impl std::fmt::Debug for MidxBackedStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.inner.fmt(f)
    }
}

impl ObjectStore for MidxBackedStore {
    fn hash_algo(&self) -> HashAlgo {
        self.inner.hash_algo()
    }

    fn read(&self, oid: &ObjectId) -> grit_lib::error::Result<Option<Object>> {
        self.inner.read(oid)
    }

    fn read_info(
        &self,
        oid: &ObjectId,
    ) -> grit_lib::error::Result<Option<grit_lib::objects::ObjectInfo>> {
        self.inner.read_info(oid)
    }

    fn for_each_object(
        &self,
        f: &mut dyn FnMut(&ObjectId) -> ControlFlow<()>,
    ) -> grit_lib::error::Result<()> {
        self.inner.for_each_object(f)
    }

    fn lookup_prefix(
        &self,
        prefix: &str,
        limit: usize,
        out: &mut Vec<ObjectId>,
    ) -> grit_lib::error::Result<()> {
        self.inner.lookup_prefix(prefix, limit, out)
    }

    fn open_stream(&self, oid: &ObjectId) -> grit_lib::error::Result<Option<ObjectStream<'_>>> {
        self.inner.open_stream(oid)
    }

    fn refresh(&self) -> grit_lib::error::Result<bool> {
        self.inner.refresh()
    }
}

fn zlib_pack(data: &[u8]) -> Vec<u8> {
    let mut enc = ZlibEncoder::new(Vec::new(), Compression::default());
    enc.write_all(data).unwrap();
    enc.finish().unwrap()
}

fn pack_type_code(kind: ObjectKind) -> u8 {
    match kind {
        ObjectKind::Commit => 1,
        ObjectKind::Tree => 2,
        ObjectKind::Blob => 3,
        ObjectKind::Tag => 4,
    }
}

fn append_pack_object_header(buf: &mut Vec<u8>, type_bits: u8, size: usize) {
    let mut n = size;
    let mut first = (type_bits << 4) | (n & 0x0f) as u8;
    n >>= 4;
    while n > 0 {
        buf.push(first | 0x80);
        first = (n & 0x7f) as u8;
        n >>= 7;
    }
    buf.push(first);
}

fn append_whole_object(buf: &mut Vec<u8>, kind: ObjectKind, data: &[u8]) -> u64 {
    let off = buf.len() as u64;
    let compressed = zlib_pack(data);
    append_pack_object_header(buf, pack_type_code(kind), data.len());
    buf.extend_from_slice(&compressed);
    off
}

fn install_specs_only_pack(objects: &Path, specs: &[(ObjectKind, Vec<u8>)], algo: HashAlgo) {
    let hash_bytes = algo.len();
    let mut pack = Vec::new();
    pack.extend_from_slice(b"PACK");
    pack.extend_from_slice(&2u32.to_be_bytes());
    pack.extend_from_slice(&(specs.len() as u32).to_be_bytes());
    let mut entries = Vec::new();
    for (kind, data) in specs {
        let oid = hash::hash_object(algo, *kind, data);
        let off = append_whole_object(&mut pack, *kind, data);
        entries.push((oid, off, 0u32));
    }
    let digest = algo.digest(&pack);
    pack.extend_from_slice(digest.as_bytes());

    let pack_dir = objects.join("pack");
    fs::create_dir_all(&pack_dir).expect("pack dir");
    let pack_path = pack_dir.join("pack-conformance.pack");
    let idx_path = pack_dir.join("pack-conformance.idx");
    fs::write(&pack_path, &pack).expect("write pack");
    write_v2_pack_index(&idx_path, &pack_path, &entries, hash_bytes).expect("write idx");
    clear_pack_cache();
    write_multi_pack_index_with_options(&pack_dir, &WriteMultiPackIndexOptions::default())
        .expect("write midx");
    clear_pack_cache();
}

fn seed_midx(algo: HashAlgo) -> impl Fn(&[(ObjectKind, Vec<u8>)]) -> MidxBackedStore {
    move |specs: &[(ObjectKind, Vec<u8>)]| {
        let root = TempDir::new().expect("tempdir");
        let git_dir = root.path().join(".git");
        let objects = git_dir.join("objects");
        fs::create_dir_all(&objects).expect("objects dir");
        if matches!(algo, HashAlgo::Sha256) {
            fs::write(
                git_dir.join("config"),
                "[extensions]\n\tobjectformat = sha256\n",
            )
            .expect("sha256 config");
        }
        install_specs_only_pack(&objects, specs, algo);
        let store = Arc::new(PackStore::new(objects));
        let inner = MidxObjects::new(store, true).expect("midx store");
        assert_eq!(inner.status(), MidxObjectsStatus::Active);
        MidxBackedStore { _root: root, inner }
    }
}

#[test]
fn midx_objects_sha1_read_suite() {
    run_read_suite(seed_midx(HashAlgo::Sha1));
}

#[test]
fn midx_objects_sha256_read_suite() {
    if !git_supports_sha256() {
        return;
    }
    run_read_suite(seed_midx(HashAlgo::Sha256));
}

fn git_cat_file(repo: &Path, oid: &ObjectId) -> Object {
    let hex = oid.to_hex();
    let kind_line = Command::new("git")
        .current_dir(repo)
        .args(["cat-file", "-t", &hex])
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .output()
        .expect("cat-file -t");
    assert!(kind_line.status.success());
    let kind_str = String::from_utf8_lossy(&kind_line.stdout)
        .trim()
        .to_string();
    let kind = match kind_str.as_str() {
        "blob" => ObjectKind::Blob,
        "tree" => ObjectKind::Tree,
        "commit" => ObjectKind::Commit,
        "tag" => ObjectKind::Tag,
        other => panic!("unexpected git type {other}"),
    };
    let out = Command::new("git")
        .current_dir(repo)
        .args(["cat-file", kind_str.as_str(), &hex])
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .output()
        .expect("cat-file");
    assert!(out.status.success());
    Object {
        kind,
        data: out.stdout,
    }
}

fn git_batch_all_objects(repo: &Path) -> BTreeMap<ObjectId, (ObjectKind, u64)> {
    let mut child = Command::new("git")
        .current_dir(repo)
        .args(["cat-file", "--batch-all-objects", "--batch-check"])
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .spawn()
        .expect("batch-all");
    let reader = BufReader::new(child.stdout.take().expect("stdout"));
    let mut map = BTreeMap::new();
    for line in reader.lines() {
        let line = line.expect("line");
        if line.is_empty() {
            continue;
        }
        let mut parts = line.split_whitespace();
        let hex = parts.next().expect("hex");
        let kind = parts.next().expect("kind");
        let size: u64 = parts.next().expect("size").parse().expect("size");
        let oid = ObjectId::from_hex(hex).expect("oid");
        let kind = match kind {
            "blob" => ObjectKind::Blob,
            "tree" => ObjectKind::Tree,
            "commit" => ObjectKind::Commit,
            "tag" => ObjectKind::Tag,
            other => panic!("unexpected kind {other}"),
        };
        map.insert(oid, (kind, size));
    }
    assert!(child.wait().expect("wait").success());
    map
}

fn assert_midx_git_roundtrip(repo: &Path, objects: &Path) {
    let pack_store = Arc::new(PackStore::new(objects.to_path_buf()));
    let midx = MidxObjects::new(Arc::clone(&pack_store), true).expect("open midx store");
    assert_eq!(midx.status(), MidxObjectsStatus::Active);
    let covered = midx.covered_pack_basenames().expect("covered packs");
    assert!(
        !covered.is_empty(),
        "MIDX should list at least one pack index"
    );
    let packs = PackedObjects::new(pack_store);
    for name in &covered {
        assert!(packs
            .for_each_object(&mut |_| ControlFlow::Continue(()))
            .is_ok());
        let _ = name;
    }

    let git_map = git_batch_all_objects(repo);
    let packed: HashSet<_> = all_packed_oids(objects);
    for oid in &packed {
        let git_obj = git_cat_file(repo, oid);
        let grit_obj = midx
            .read(oid)
            .expect("read")
            .unwrap_or_else(|| panic!("MIDX store missing packed oid {oid}"));
        assert_eq!(grit_obj.kind, git_obj.kind);
        assert_eq!(grit_obj.data, git_obj.data);
    }

    let mut grit_ids = Vec::new();
    midx.for_each_object(&mut |oid| {
        grit_ids.push(*oid);
        ControlFlow::Continue(())
    })
    .expect("for_each");
    grit_ids.sort_by_key(|o| o.to_hex());
    grit_ids.dedup();

    let mut git_ids: Vec<ObjectId> = git_map.keys().copied().collect();
    git_ids.sort_by_key(|o| o.to_hex());
    git_ids.retain(|oid| packed.contains(oid));
    git_ids.sort_by_key(|o| o.to_hex());
    git_ids.dedup();

    assert_eq!(
        grit_ids, git_ids,
        "MIDX for_each_object should match git batch-all-objects for packed oids"
    );
}

#[test]
fn git_midx_eight_pack_roundtrip() {
    if !git_available() {
        eprintln!("SKIP: git unavailable");
        return;
    }
    let Some((repo, objects, _)) = multi_pack_repo(FixtureHashAlgo::Sha1, 8) else {
        eprintln!("SKIP: eight-pack fixture failed");
        return;
    };
    git_write_midx(&repo);
    clear_pack_cache();
    assert_midx_git_roundtrip(repo.path(), &objects);
}

/// Base MIDX plus one grit incremental layer (two chain entries).
fn grit_two_layer_incremental_fixture(
) -> Option<(midx_support::RepoFixture, PathBuf, ObjectId, ObjectId)> {
    let (repo, objects, base_oids) = multi_pack_repo(FixtureHashAlgo::Sha1, 2)?;
    let pack_dir = objects.join("pack");
    write_multi_pack_index_with_options(
        &pack_dir,
        &WriteMultiPackIndexOptions {
            version: Some(1),
            ..Default::default()
        },
    )
    .ok()?;
    let old_only = *base_oids.first()?;
    std::fs::write(repo.path().join("incr-only.txt"), b"incr layer\n").ok()?;
    repo.git(&["add", "incr-only.txt"]);
    repo.git(&["commit", "-q", "-m", "incr layer"]);
    let new_only = head_oid(&repo);
    if !pack_objects_layer(repo.path(), 50) {
        return None;
    }
    write_multi_pack_index_with_options(
        &pack_dir,
        &WriteMultiPackIndexOptions {
            incremental: true,
            version: Some(1),
            ..Default::default()
        },
    )
    .ok()?;
    let chain = read_chain_hashes(&pack_dir)?;
    if chain.len() != 2 {
        return None;
    }
    Some((repo, objects, old_only, new_only))
}

#[test]
fn grit_incremental_two_layer_old_and_new_reads() {
    let Some((repo, objects, old_only, new_only)) = grit_two_layer_incremental_fixture() else {
        panic!("grit two-layer incremental fixture setup failed");
    };
    clear_pack_cache();
    let store = Arc::new(PackStore::new(objects.clone()));
    let midx = MidxObjects::new(Arc::clone(&store), true).expect("open midx store");
    assert_eq!(midx.status(), MidxObjectsStatus::Active);
    assert_eq!(
        read_chain_hashes(&objects.join("pack"))
            .expect("chain file")
            .len(),
        2
    );

    assert!(
        midx.read(&old_only).expect("old read").is_some(),
        "object listed only in the base layer must read via chain offset"
    );
    assert!(
        midx.read(&new_only).expect("new read").is_some(),
        "object in the incremental layer must read"
    );
    let _ = repo;
}

#[test]
fn grit_incremental_two_layer_for_each_stops_on_break() {
    let Some((_repo, objects, _, _)) = grit_two_layer_incremental_fixture() else {
        panic!("grit two-layer incremental fixture setup failed");
    };
    clear_pack_cache();
    let midx = MidxObjects::new(Arc::new(PackStore::new(objects)), true).expect("open");
    let mut break_callbacks = 0usize;
    midx.for_each_object(&mut |_| {
        break_callbacks += 1;
        ControlFlow::Break(())
    })
    .expect("for_each");
    assert_eq!(
        break_callbacks, 1,
        "for_each_object must stop after the first Break across chain layers"
    );
}

#[test]
fn grit_incremental_corrupt_tip_makes_store_unusable() {
    let Some((repo, objects, old_only, _)) = grit_two_layer_incremental_fixture() else {
        panic!("grit two-layer incremental fixture setup failed");
    };
    let pack_dir = objects.join("pack");
    let tip = tip_midx_path(&pack_dir);
    let mut data = std::fs::read(&tip).expect("read tip layer");
    if data.len() > 24 {
        let corrupt_offset = data.len() - 21;
        data[corrupt_offset] ^= 0xff;
    }
    std::fs::write(&tip, &data).expect("write corrupt tip");
    clear_pack_cache();
    grit_lib::midx::evict_midx_read_cache_for_pack_dir(&pack_dir);

    let store = Arc::new(PackStore::new(objects.clone()));
    let midx = MidxObjects::new(Arc::clone(&store), true).expect("open");
    assert_eq!(
        midx.status(),
        MidxObjectsStatus::Unusable,
        "any corrupt referenced chain layer must disable the MIDX backend"
    );
    assert!(midx.read(&old_only).expect("read").is_none());

    let packs = PackedObjects::new(store);
    assert!(packs.read(&old_only).expect("pack fallback").is_some());
    let _ = repo;
}

#[test]
fn corrupt_midx_reports_unusable_and_packs_still_read() {
    if !git_available() {
        eprintln!("SKIP: git unavailable");
        return;
    }
    let Some((repo, objects, oids)) = multi_pack_repo(FixtureHashAlgo::Sha1, 3) else {
        eprintln!("SKIP: fixture");
        return;
    };
    git_write_midx(&repo);
    let pack_dir = objects.join("pack");
    midx_support::patch_midx_file(&pack_dir, |data| {
        if data.len() > 24 {
            let corrupt_offset = data.len() - 21;
            data[corrupt_offset] ^= 0xff;
        }
    });
    clear_pack_cache();
    grit_lib::midx::evict_midx_read_cache_for_pack_dir(&pack_dir);

    let store = Arc::new(PackStore::new(objects.clone()));
    let midx = MidxObjects::new(Arc::clone(&store), true).expect("open");
    assert_eq!(midx.status(), MidxObjectsStatus::Unusable);
    assert!(midx.covered_pack_basenames().expect("names").is_empty());

    let oid = oids.last().copied().expect("commit oid");
    assert!(midx.read(&oid).expect("read").is_none());

    let packs = PackedObjects::new(store);
    let via_packs = packs.read(&oid).expect("pack read");
    assert!(via_packs.is_some());
}

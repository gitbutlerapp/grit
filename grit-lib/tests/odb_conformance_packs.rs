//! [`PackedObjects`] conformance via the shared [`grit_test_support::odb_conformance`] suite.

use std::fs;
use std::io::Write;
use std::ops::ControlFlow;
use std::path::Path;
use std::sync::Arc;

use grit_lib::hash;
use grit_lib::objects::{HashAlgo, ObjectKind};
use grit_lib::odb::store::{ObjectStore, ObjectStream, PackedObjects};
use grit_lib::pack::{clear_pack_cache, write_v2_pack_index};
use grit_lib::pack_store::PackStore;
use grit_test_support::git_supports_sha256;
use grit_test_support::odb_conformance::run_read_suite;
use tempfile::TempDir;

struct PackBackedStore {
    _root: TempDir,
    inner: PackedObjects,
}

impl std::fmt::Debug for PackBackedStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.inner.fmt(f)
    }
}

impl ObjectStore for PackBackedStore {
    fn hash_algo(&self) -> HashAlgo {
        self.inner.hash_algo()
    }

    fn read(
        &self,
        oid: &grit_lib::objects::ObjectId,
    ) -> grit_lib::error::Result<Option<grit_lib::objects::Object>> {
        self.inner.read(oid)
    }

    fn read_info(
        &self,
        oid: &grit_lib::objects::ObjectId,
    ) -> grit_lib::error::Result<Option<grit_lib::objects::ObjectInfo>> {
        self.inner.read_info(oid)
    }

    fn for_each_object(
        &self,
        f: &mut dyn FnMut(&grit_lib::objects::ObjectId) -> ControlFlow<()>,
    ) -> grit_lib::error::Result<()> {
        self.inner.for_each_object(f)
    }

    fn lookup_prefix(
        &self,
        prefix: &str,
        limit: usize,
        out: &mut Vec<grit_lib::objects::ObjectId>,
    ) -> grit_lib::error::Result<()> {
        self.inner.lookup_prefix(prefix, limit, out)
    }

    fn open_stream(
        &self,
        oid: &grit_lib::objects::ObjectId,
    ) -> grit_lib::error::Result<Option<ObjectStream<'_>>> {
        self.inner.open_stream(oid)
    }

    fn refresh(&self) -> grit_lib::error::Result<bool> {
        self.inner.refresh()
    }
}

fn zlib_pack(data: &[u8]) -> Vec<u8> {
    use flate2::write::ZlibEncoder;
    use flate2::Compression;
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
}

fn seed_packed(algo: HashAlgo) -> impl Fn(&[(ObjectKind, Vec<u8>)]) -> PackBackedStore {
    move |specs: &[(ObjectKind, Vec<u8>)]| {
        let root = TempDir::new().expect("tempdir");
        let objects = root.path().join("objects");
        fs::create_dir_all(&objects).expect("objects dir");
        install_specs_only_pack(&objects, specs, algo);
        let store = Arc::new(PackStore::new(objects));
        PackBackedStore {
            _root: root,
            inner: PackedObjects::new(store),
        }
    }
}

#[test]
fn packed_objects_sha1_read_suite() {
    run_read_suite(seed_packed(HashAlgo::Sha1));
}

#[test]
fn packed_objects_sha256_read_suite() {
    if !git_supports_sha256() {
        return;
    }
    run_read_suite(seed_packed(HashAlgo::Sha256));
}

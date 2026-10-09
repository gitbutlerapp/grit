//! Reusable conformance checks for [`ObjectStore`] and [`WritableObjectStore`] backends.
//!
//! External crates (or integration tests in this workspace) can validate a custom backend by
//! wiring factory functions into [`run_read_suite`] and [`run_write_suite`]:
//!
//! ```no_run
//! use grit_lib::objects::{HashAlgo, ObjectKind};
//! use grit_lib::odb::store::{MemoryStore, ObjectStore, WritableObjectStore};
//! use grit_lib::odb::WriteOptions;
//! use grit_test_support::odb_conformance::{run_read_suite, run_write_suite};
//!
//! fn seed(specs: &[(ObjectKind, Vec<u8>)]) -> MemoryStore {
//!     let store = MemoryStore::new(HashAlgo::Sha1);
//!     for (kind, data) in specs {
//!         WritableObjectStore::write(&store, *kind, data, WriteOptions::default()).unwrap();
//!     }
//!     store
//! }
//!
//! run_read_suite(seed);
//! run_write_suite(|| MemoryStore::new(HashAlgo::Sha1));
//! ```
//!
//! Each case is also exposed as its own function so test harnesses can name failures precisely.

#![allow(clippy::expect_used)]

use std::collections::HashSet;
use std::io::{Cursor, Read};
use std::ops::ControlFlow;
use std::sync::Arc;
use std::thread;

use grit_lib::error::Result;
use grit_lib::hash;
use grit_lib::objects::{HashAlgo, Object, ObjectId, ObjectInfo, ObjectKind};
use grit_lib::odb::store::{ObjectStore, WritableObjectStore};
use grit_lib::odb::WriteOptions;

const LARGE_BLOB_LEN: usize = 8 * 1024 * 1024;

/// Run every read-side conformance case against `seed`.
///
/// `seed` must populate a store with exactly the objects described in `specs` and return it.
pub fn run_read_suite<S: ObjectStore + 'static>(seed: impl Fn(&[(ObjectKind, Vec<u8>)]) -> S) {
    read_all_four_object_kinds(&seed);
    read_empty_blob_and_empty_tree(&seed);
    read_binary_data_with_nuls(&seed);
    read_large_blob_stream_matches_read(&seed);
    read_info_matches_read(&seed);
    read_contains_agreement_on_misses(&seed);
    read_for_each_object_yields_each_id_once(&seed);
    read_lookup_prefix_unique_ambiguous_and_exact(&seed);
    read_concurrent_reads_from_eight_threads(&seed);
    read_sha256_variants(&seed);
}

/// Run every write-side conformance case against `make`.
///
/// `make` must return a fresh, empty writable store for each case that needs one.
pub fn run_write_suite<S: WritableObjectStore>(make: impl Fn() -> S) {
    write_returns_git_hashed_id(&make);
    write_idempotence(&make);
    write_stream_equal_to_write(&make);
    write_freshen_present_vs_absent(&make);
    write_sha256_variants(&make);
}

fn seed_algo<S: ObjectStore>(seed: &impl Fn(&[(ObjectKind, Vec<u8>)]) -> S) -> HashAlgo {
    seed(&[]).hash_algo()
}

/// All four Git object kinds round-trip through [`ObjectStore::read`].
pub fn read_all_four_object_kinds<S: ObjectStore>(seed: &impl Fn(&[(ObjectKind, Vec<u8>)]) -> S) {
    let algo = seed_algo(seed);
    let specs = standard_object_specs(algo);
    let store = seed(&specs);
    for (kind, data) in &specs {
        let oid = hash::hash_object(algo, *kind, data);
        let obj = store
            .read(&oid)
            .expect("read_all_four_object_kinds: read")
            .expect("read_all_four_object_kinds: hit");
        assert_eq!(obj.kind, *kind, "read_all_four_object_kinds: kind");
        assert_eq!(obj.data, *data, "read_all_four_object_kinds: payload");
    }
}

/// Empty blob and empty tree payloads are stored and loaded correctly.
pub fn read_empty_blob_and_empty_tree<S: ObjectStore>(
    seed: &impl Fn(&[(ObjectKind, Vec<u8>)]) -> S,
) {
    let algo = seed_algo(seed);
    let specs = vec![
        (ObjectKind::Blob, Vec::new()),
        (ObjectKind::Tree, Vec::new()),
    ];
    let store = seed(&specs);
    for (kind, data) in &specs {
        let oid = hash::hash_object(algo, *kind, data);
        let obj = store
            .read(&oid)
            .expect("read_empty_blob_and_empty_tree: read")
            .expect("read_empty_blob_and_empty_tree: hit");
        assert_eq!(obj.kind, *kind);
        assert_eq!(obj.data, *data);
    }
}

/// Binary blob payloads may contain NUL bytes.
pub fn read_binary_data_with_nuls<S: ObjectStore>(seed: &impl Fn(&[(ObjectKind, Vec<u8>)]) -> S) {
    let payload = vec![0u8, 1, 0, 255, 0, 42];
    let specs = vec![(ObjectKind::Blob, payload.clone())];
    let store = seed(&specs);
    let algo = store.hash_algo();
    let oid = hash::hash_object(algo, ObjectKind::Blob, &payload);
    let obj = store
        .read(&oid)
        .expect("read_binary_data_with_nuls: read")
        .expect("read_binary_data_with_nuls: hit");
    assert_eq!(obj.data, payload);
}

/// A large blob (≥ 8 MiB) matches whether read wholly or via [`ObjectStore::open_stream`].
pub fn read_large_blob_stream_matches_read<S: ObjectStore>(
    seed: &impl Fn(&[(ObjectKind, Vec<u8>)]) -> S,
) {
    let payload = vec![0xCDu8; LARGE_BLOB_LEN];
    let specs = vec![(ObjectKind::Blob, payload.clone())];
    let store = seed(&specs);
    let algo = store.hash_algo();
    let oid = hash::hash_object(algo, ObjectKind::Blob, &payload);
    let whole = store
        .read(&oid)
        .expect("read_large_blob_stream_matches_read: read")
        .expect("read_large_blob_stream_matches_read: hit");
    let mut stream = store
        .open_stream(&oid)
        .expect("read_large_blob_stream_matches_read: open_stream")
        .expect("read_large_blob_stream_matches_read: stream hit");
    assert_eq!(stream.kind, ObjectKind::Blob);
    assert_eq!(stream.size, payload.len() as u64);
    let mut streamed = Vec::with_capacity(payload.len());
    stream
        .reader
        .read_to_end(&mut streamed)
        .expect("read_large_blob_stream_matches_read: read stream");
    assert_eq!(whole.data, payload);
    assert_eq!(streamed, payload);
}

/// [`ObjectStore::read_info`] agrees with [`ObjectStore::read`] on kind and size.
pub fn read_info_matches_read<S: ObjectStore>(seed: &impl Fn(&[(ObjectKind, Vec<u8>)]) -> S) {
    let algo = seed_algo(seed);
    let specs = standard_object_specs(algo);
    let store = seed(&specs);
    for (kind, data) in &specs {
        let oid = hash::hash_object(algo, *kind, data);
        let obj = store
            .read(&oid)
            .expect("read_info_matches_read: read")
            .expect("read_info_matches_read: hit");
        let info = store
            .read_info(&oid)
            .expect("read_info_matches_read: read_info")
            .expect("read_info_matches_read: info hit");
        assert_eq!(info.kind, obj.kind, "read_info_matches_read: kind");
        assert_eq!(
            info.size,
            u64::try_from(obj.data.len()).expect("read_info_matches_read: size"),
            "read_info_matches_read: size"
        );
    }
}

/// Missing objects report `None` from read and `false` from contains consistently.
pub fn read_contains_agreement_on_misses<S: ObjectStore>(
    seed: &impl Fn(&[(ObjectKind, Vec<u8>)]) -> S,
) {
    let store = seed(&[]);
    let algo = store.hash_algo();
    let misses = [
        ObjectId::zero(),
        hash::hash_object(algo, ObjectKind::Blob, b"never-written"),
    ];
    for oid in misses {
        assert!(
            store
                .read(&oid)
                .expect("read_contains_agreement_on_misses: read")
                .is_none(),
            "read_contains_agreement_on_misses: read miss {oid}"
        );
        assert!(
            !store
                .contains(&oid)
                .expect("read_contains_agreement_on_misses: contains"),
            "read_contains_agreement_on_misses: contains miss {oid}"
        );
        assert!(
            store
                .read_info(&oid)
                .expect("read_contains_agreement_on_misses: read_info")
                .is_none(),
            "read_contains_agreement_on_misses: read_info miss {oid}"
        );
    }
}

/// [`ObjectStore::for_each_object`] visits every stored id exactly once.
pub fn read_for_each_object_yields_each_id_once<S: ObjectStore>(
    seed: &impl Fn(&[(ObjectKind, Vec<u8>)]) -> S,
) {
    let algo = seed_algo(seed);
    let specs = standard_object_specs(algo);
    let store = seed(&specs);
    let expected: HashSet<ObjectId> = specs
        .iter()
        .map(|(kind, data)| hash::hash_object(algo, *kind, data))
        .collect();
    let mut seen = HashSet::new();
    store
        .for_each_object(&mut |oid| {
            assert!(
                seen.insert(*oid),
                "read_for_each_object_yields_each_id_once: duplicate {oid}"
            );
            ControlFlow::Continue(())
        })
        .expect("read_for_each_object_yields_each_id_once: for_each");
    assert_eq!(
        seen, expected,
        "read_for_each_object_yields_each_id_once: set"
    );
}

/// Prefix lookup: short unique match, ambiguity, and full-id exact match.
pub fn read_lookup_prefix_unique_ambiguous_and_exact<S: ObjectStore>(
    seed: &impl Fn(&[(ObjectKind, Vec<u8>)]) -> S,
) {
    let a_data = b"prefix-a";
    let b_data = b"prefix-b";
    let specs = vec![
        (ObjectKind::Blob, a_data.to_vec()),
        (ObjectKind::Blob, b_data.to_vec()),
    ];
    let store = seed(&specs);
    let algo = store.hash_algo();
    let a = hash::hash_object(algo, ObjectKind::Blob, a_data);
    let b = hash::hash_object(algo, ObjectKind::Blob, b_data);
    let hex_a = a.to_hex();
    let four_char = &hex_a[..4];
    let mut unique = Vec::new();
    store
        .lookup_prefix(four_char, 0, &mut unique)
        .expect("read_lookup_prefix: unique prefix");
    assert_eq!(unique, vec![a], "read_lookup_prefix: unique");

    let mut ambiguous = Vec::new();
    store
        .lookup_prefix("", 0, &mut ambiguous)
        .expect("read_lookup_prefix: ambiguous");
    assert_eq!(ambiguous.len(), 2);
    assert!(ambiguous.contains(&a));
    assert!(ambiguous.contains(&b));

    let mut exact = Vec::new();
    store
        .lookup_prefix(&hex_a, 0, &mut exact)
        .expect("read_lookup_prefix: exact");
    assert_eq!(exact, vec![a], "read_lookup_prefix: exact full id");

    let mut none = Vec::new();
    store
        .lookup_prefix("ffffffffffffffffffffffffffffffffffffffff", 0, &mut none)
        .expect("read_lookup_prefix: none");
    assert!(none.is_empty(), "read_lookup_prefix: no false positives");
}

/// Eight threads can read the same objects concurrently without error.
pub fn read_concurrent_reads_from_eight_threads<S: ObjectStore + 'static>(
    seed: &impl Fn(&[(ObjectKind, Vec<u8>)]) -> S,
) {
    let algo = seed_algo(seed);
    let specs = standard_object_specs(algo);
    let store = Arc::new(seed(&specs));
    let oids: Arc<Vec<ObjectId>> = Arc::new(
        specs
            .iter()
            .map(|(kind, data)| hash::hash_object(algo, *kind, data))
            .collect(),
    );
    let mut handles = Vec::with_capacity(8);
    for _ in 0..8 {
        let store = Arc::clone(&store);
        let oids = Arc::clone(&oids);
        handles.push(thread::spawn(move || {
            for oid in oids.iter() {
                store
                    .read(oid)
                    .expect("read_concurrent_reads_from_eight_threads: read")
                    .expect("read_concurrent_reads_from_eight_threads: hit");
            }
        }));
    }
    for handle in handles {
        handle
            .join()
            .expect("read_concurrent_reads_from_eight_threads: join");
    }
}

/// When the store uses SHA-256, object ids and prefix rules follow that width.
pub fn read_sha256_variants<S: ObjectStore>(seed: &impl Fn(&[(ObjectKind, Vec<u8>)]) -> S) {
    let store = seed(&[(ObjectKind::Blob, b"sha256-probe".to_vec())]);
    if store.hash_algo() != HashAlgo::Sha256 {
        return;
    }
    let oid = hash::hash_object(HashAlgo::Sha256, ObjectKind::Blob, b"sha256-probe");
    assert_eq!(oid.to_hex().len(), HashAlgo::Sha256.hex_len());
    let mut out = Vec::new();
    store
        .lookup_prefix(&oid.to_hex(), 0, &mut out)
        .expect("read_sha256_variants: exact lookup");
    assert_eq!(out, vec![oid]);
}

/// [`WritableObjectStore::write`] returns the Git object id for canonical bytes.
pub fn write_returns_git_hashed_id<S: WritableObjectStore>(make: &impl Fn() -> S) {
    let store = make();
    let data = b"hash-me";
    let oid = store
        .write(ObjectKind::Blob, data, WriteOptions::default())
        .expect("write_returns_git_hashed_id: write");
    let expected = hash::hash_object(store.hash_algo(), ObjectKind::Blob, data);
    assert_eq!(oid, expected, "write_returns_git_hashed_id: oid");
}

/// Writing the same bytes twice is idempotent and keeps one stored copy.
pub fn write_idempotence<S: WritableObjectStore>(make: &impl Fn() -> S) {
    let store = make();
    let data = b"same-bytes";
    let oid1 = store
        .write(ObjectKind::Blob, data, WriteOptions::default())
        .expect("write_idempotence: first");
    let oid2 = store
        .write(ObjectKind::Blob, data, WriteOptions::default())
        .expect("write_idempotence: second");
    assert_eq!(oid1, oid2, "write_idempotence: oid");
    let mut ids = Vec::new();
    store
        .for_each_object(&mut |oid| {
            ids.push(*oid);
            ControlFlow::Continue(())
        })
        .expect("write_idempotence: for_each");
    assert_eq!(ids.len(), 1, "write_idempotence: single object");
}

/// [`WritableObjectStore::write_stream`] stores the same bytes as [`WritableObjectStore::write`].
pub fn write_stream_equal_to_write<S: WritableObjectStore>(make: &impl Fn() -> S) {
    let store = make();
    let payload = b"stream-bytes";
    let via_write = store
        .write(ObjectKind::Blob, payload, WriteOptions::default())
        .expect("write_stream_equal_to_write: write");
    let mut cursor = Cursor::new(*payload);
    let via_stream = store
        .write_stream(
            ObjectKind::Blob,
            payload.len() as u64,
            &mut cursor,
            WriteOptions::default(),
        )
        .expect("write_stream_equal_to_write: write_stream");
    assert_eq!(via_write, via_stream, "write_stream_equal_to_write: oid");
    let obj = store
        .read(&via_stream)
        .expect("write_stream_equal_to_write: read")
        .expect("write_stream_equal_to_write: hit");
    assert_eq!(obj.data, payload.as_slice());
}

/// [`WritableObjectStore::freshen`] returns true for stored objects and false for misses.
pub fn write_freshen_present_vs_absent<S: WritableObjectStore>(make: &impl Fn() -> S) {
    let store = make();
    let oid = store
        .write(ObjectKind::Blob, b"touch", WriteOptions::default())
        .expect("write_freshen_present_vs_absent: write");
    assert!(
        store
            .freshen(&oid)
            .expect("write_freshen_present_vs_absent: freshen hit"),
        "write_freshen_present_vs_absent: present"
    );
    assert!(
        !store
            .freshen(&ObjectId::zero())
            .expect("write_freshen_present_vs_absent: freshen miss"),
        "write_freshen_present_vs_absent: absent"
    );
}

/// SHA-256 writable stores hash with the 32-byte object id width.
pub fn write_sha256_variants<S: WritableObjectStore>(make: &impl Fn() -> S) {
    let store = make();
    if store.hash_algo() != HashAlgo::Sha256 {
        return;
    }
    let oid = store
        .write(ObjectKind::Blob, b"x", WriteOptions::default())
        .expect("write_sha256_variants: write");
    assert_eq!(oid.to_hex().len(), HashAlgo::Sha256.hex_len());
}

fn standard_object_specs(algo: HashAlgo) -> Vec<(ObjectKind, Vec<u8>)> {
    let blob = b"blob-body".to_vec();
    let blob_oid = hash::hash_object(algo, ObjectKind::Blob, &blob);
    let empty_tree = hash::hash_object(algo, ObjectKind::Tree, b"");
    let tree = tree_with_blob_entry(&blob_oid);
    let commit = minimal_commit_body(algo, &empty_tree);
    let commit_oid = hash::hash_object(algo, ObjectKind::Commit, &commit);
    let tag = minimal_tag_body(algo, &commit_oid);
    vec![
        (ObjectKind::Blob, blob),
        (ObjectKind::Tree, tree),
        (ObjectKind::Commit, commit),
        (ObjectKind::Tag, tag),
    ]
}

fn tree_with_blob_entry(blob_oid: &ObjectId) -> Vec<u8> {
    let mut body = b"100644 file\0".to_vec();
    body.extend_from_slice(blob_oid.as_bytes());
    body
}

fn minimal_commit_body(algo: HashAlgo, tree_oid: &ObjectId) -> Vec<u8> {
    let _ = algo;
    format!(
        "tree {}\nauthor T <t@example.com> 1 +0000\ncommitter T <t@example.com> 1 +0000\n\n",
        tree_oid.to_hex()
    )
    .into_bytes()
}

fn minimal_tag_body(algo: HashAlgo, target: &ObjectId) -> Vec<u8> {
    let _ = algo;
    format!(
        "object {}\ntype commit\ntag t\ntagger T <t@example.com> 1 +0000\n\n",
        target.to_hex()
    )
    .into_bytes()
}

/// Wrappers that violate invariants for meta-tests proving the suite catches bugs.
pub mod broken_store {
    use super::*;

    /// Wraps `inner` but reports the wrong kind from [`ObjectStore::read_info`].
    #[derive(Debug)]
    pub struct WrongReadInfo<S> {
        inner: S,
    }

    impl<S> WrongReadInfo<S> {
        /// Wrap an existing store.
        #[must_use]
        pub fn new(inner: S) -> Self {
            Self { inner }
        }
    }

    impl<S: ObjectStore> ObjectStore for WrongReadInfo<S> {
        fn hash_algo(&self) -> HashAlgo {
            self.inner.hash_algo()
        }

        fn read(&self, oid: &ObjectId) -> Result<Option<Object>> {
            self.inner.read(oid)
        }

        fn read_info(&self, oid: &ObjectId) -> Result<Option<ObjectInfo>> {
            Ok(self.inner.read_info(oid)?.map(|info| ObjectInfo {
                kind: ObjectKind::Tag,
                size: info.size,
            }))
        }

        fn for_each_object(&self, f: &mut dyn FnMut(&ObjectId) -> ControlFlow<()>) -> Result<()> {
            self.inner.for_each_object(f)
        }
    }
}

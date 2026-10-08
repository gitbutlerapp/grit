//! Shared helpers for pack delta and corruption integration tests (t5303/t5309/t5314/t5316).

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::mpsc;
use std::time::Duration;

use grit_lib::error::Error;
use grit_lib::objects::{ObjectId, ObjectKind};
use grit_lib::odb::Odb;
use grit_lib::pack::clear_pack_cache;
use grit_lib::unpack_objects::{apply_delta, apply_delta_into};
use grit_test_support::objects::{
    flip_byte_at, hash_loose_object, write_loose_object, write_pack_and_index, DeltaOps, HashAlgo,
    IndexPackOptions, ObjectKind as PackObjectKind, PackBuilder, PackBuilt, RepoFixture,
};

pub const TEST_TIMEOUT: Duration = Duration::from_secs(30);

/// Run `f` on a worker thread; panic if it does not finish within [`TEST_TIMEOUT`].
pub fn run_with_timeout<T, F>(label: &str, f: F) -> T
where
    F: FnOnce() -> T + Send + 'static,
    T: Send + 'static,
{
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(f());
    });
    rx.recv_timeout(TEST_TIMEOUT)
        .unwrap_or_else(|_| panic!("{label} timed out after {TEST_TIMEOUT:?}"))
}

pub fn run_algo(algo: HashAlgo, f: impl FnOnce(HashAlgo)) {
    if matches!(algo, HashAlgo::Sha256) && !grit_test_support::objects::git_supports_sha256() {
        eprintln!("SKIP: system git lacks sha256 object format");
        return;
    }
    f(algo);
}

pub fn grit_apply_accepts(base: &[u8], delta: &[u8]) -> bool {
    apply_delta(base, delta).is_ok()
}

pub fn grit_apply_into_accepts(base: &[u8], delta: &[u8]) -> bool {
    let mut out = Vec::new();
    apply_delta_into(&mut out, base, delta).is_ok()
}

/// Build a one-blob + one-ref-delta pack and run `git index-pack`.
pub fn git_index_pack_accepts_delta(algo: HashAlgo, base: &[u8], delta: &[u8]) -> bool {
    let mut builder = PackBuilder::new(algo);
    let base_idx = builder.add_full(PackObjectKind::Blob, base);
    let base_oid = hex::decode(hash_loose_object(algo, "blob", base)).expect("oid hex");
    let _ = base_idx;
    builder.add_ref_delta(&base_oid, delta, delta.len());
    let built = builder.build();
    let repo = match RepoFixture::init(algo) {
        Ok(r) => r,
        Err(_) => return false,
    };
    let outcome = write_pack_and_index(
        &repo.objects_dir(),
        "delta-probe",
        &built.bytes,
        algo,
        &IndexPackOptions::default(),
    );
    outcome.index_ok
}

/// Grit and Git must agree on whether a hand-made delta is acceptable.
pub fn assert_delta_verdict(base: &[u8], delta: &[u8], case: &str) {
    let grit = grit_apply_accepts(base, delta);
    let git = git_index_pack_accepts_delta(HashAlgo::Sha1, base, delta);
    assert_eq!(
        grit, git,
        "apply_delta vs git index-pack mismatch for {case}: grit={grit} git={git}"
    );
    assert_eq!(
        grit,
        grit_apply_into_accepts(base, delta),
        "apply_delta vs apply_delta_into mismatch for {case}"
    );
}

pub fn install_hand_pack(
    objects: &Path,
    stem: &str,
    algo: HashAlgo,
    build: impl FnOnce(&mut PackBuilder) -> (),
) -> PathBuf {
    clear_pack_cache();
    let mut builder = PackBuilder::new(algo);
    build(&mut builder);
    let built = builder.build();
    let outcome = write_pack_and_index(
        objects,
        stem,
        &built.bytes,
        algo,
        &IndexPackOptions::default(),
    );
    assert!(
        outcome.index_ok,
        "index-pack failed for {stem}: {}",
        outcome.index_stderr
    );
    outcome.pack_path
}

/// Install pack bytes with a synthetic v2 `.idx` (when `git index-pack` cannot complete thin packs).
/// Index selected pack entries with a synthetic v2 `.idx` (no `git index-pack`).
pub fn finish_synthetic_pack(
    objects: &Path,
    stem: &str,
    algo: HashAlgo,
    built: &PackBuilt,
    indexed: &[(ObjectId, usize)],
) -> PathBuf {
    let entries: Vec<(ObjectId, u64)> = indexed
        .iter()
        .map(|(oid, idx)| {
            (
                *oid,
                u64::try_from(built.entry_offsets[*idx]).expect("entry offset"),
            )
        })
        .collect();
    install_synthetic_idx_pack(objects, stem, algo, &built.bytes, &entries)
}

pub fn install_synthetic_idx_pack(
    objects: &Path,
    stem: &str,
    algo: HashAlgo,
    pack: &[u8],
    entries: &[(ObjectId, u64)],
) -> PathBuf {
    clear_pack_cache();
    let pack_dir = objects.join("pack");
    std::fs::create_dir_all(&pack_dir).expect("pack dir");
    let pack_path = pack_dir.join(format!("{stem}.pack"));
    let idx_path = pack_dir.join(format!("{stem}.idx"));
    std::fs::write(&pack_path, pack).expect("write pack");
    write_v2_idx(&idx_path, &pack_path, entries, algo);
    pack_path
}

fn write_v2_idx(idx_path: &Path, pack_path: &Path, entries: &[(ObjectId, u64)], algo: HashAlgo) {
    let mut sorted = entries.to_vec();
    sorted.sort_by(|a, b| a.0.as_bytes().cmp(b.0.as_bytes()));
    let n = sorted.len();
    let oid_len = algo.oid_len();
    let mut fanout = [0u32; 256];
    for byte in 0u32..256 {
        let count = sorted
            .iter()
            .filter(|(oid, _)| u32::from(oid.as_bytes()[0]) <= byte)
            .count();
        fanout[byte as usize] = u32::try_from(count).unwrap_or(u32::MAX);
    }
    let mut buf = Vec::new();
    buf.extend_from_slice(b"\xfftOc");
    buf.extend_from_slice(&2u32.to_be_bytes());
    for f in fanout {
        buf.extend_from_slice(&f.to_be_bytes());
    }
    for (oid, _) in &sorted {
        buf.extend_from_slice(oid.as_bytes());
    }
    for _ in 0..n {
        buf.extend_from_slice(&0u32.to_be_bytes());
    }
    for (_, off) in &sorted {
        let v = u32::try_from(*off).unwrap_or(0x8000_0000);
        buf.extend_from_slice(&v.to_be_bytes());
    }
    let pack_bytes = std::fs::read(pack_path).expect("read pack");
    buf.extend_from_slice(&pack_bytes[pack_bytes.len() - oid_len..]);
    let lib_algo = match algo {
        HashAlgo::Sha1 => grit_lib::objects::HashAlgo::Sha1,
        HashAlgo::Sha256 => grit_lib::objects::HashAlgo::Sha256,
    };
    let digest = lib_algo.digest(&buf);
    buf.extend_from_slice(digest.as_bytes());
    std::fs::write(idx_path, buf).expect("write idx");
}

pub fn odb_at(objects: &Path) -> Odb {
    Odb::new(objects)
}

pub fn oid_from_hex(algo: HashAlgo, hex: &str) -> ObjectId {
    ObjectId::from_hex(hex).expect("valid test oid")
}

pub fn read_blob(odb: &Odb, hex: &str) -> Result<grit_lib::objects::Object, Error> {
    odb.read(&oid_from_hex(HashAlgo::Sha1, hex))
}

pub fn expect_corrupt_object(err: Error) {
    assert!(
        matches!(err, Error::CorruptObject(_)),
        "expected CorruptObject, got {err:?}"
    );
}

pub fn expect_delta_chain_limit(err: Error) {
    assert!(
        matches!(err, Error::DeltaChainTooDeep { .. }),
        "expected DeltaChainTooDeep, got {err:?}"
    );
}

pub fn expect_object_not_found(err: Error) {
    assert!(
        matches!(err, Error::ObjectNotFound(_)),
        "expected ObjectNotFound, got {err:?}"
    );
}

/// Flip one byte in a pack file at `offset` (relative to pack start).
pub fn corrupt_pack_byte(pack_path: &Path, offset: u64) {
    flip_byte_at(pack_path, offset).expect("flip pack byte");
    clear_pack_cache();
}

pub fn write_loose_blob(objects: &Path, algo: HashAlgo, body: &[u8]) -> String {
    write_loose_object(objects, algo, "blob", body).expect("write loose blob")
}

pub fn lcp_delta(base: &[u8], target: &[u8]) -> Vec<u8> {
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

/// Hand-made delta bodies from upstream t5303 (Git `printf` vectors).
pub mod t5303_deltas {
    pub fn minimal_good() -> Vec<u8> {
        vec![0, 1, 1, b'X']
    }
    pub fn too_many_literal() -> Vec<u8> {
        vec![0, 1, 2, b'X', b'X']
    }
    pub fn too_many_copied(base_len: u8) -> Vec<u8> {
        vec![base_len, 1, 0x91, 0, 2]
    }
    pub fn too_few_literal() -> Vec<u8> {
        vec![0, 2, 2, b'X']
    }
    pub fn too_few_base_bytes() -> Vec<u8> {
        vec![0, 1, 0x91, 0, 1]
    }
    pub fn truncated_copy() -> Vec<u8> {
        vec![4, 2, 1, b'X', 0x91]
    }
    pub fn trailing_garbage_literal() -> Vec<u8> {
        vec![0, 1, 1, b'X', 1]
    }
    pub fn trailing_garbage_copy() -> Vec<u8> {
        vec![4, 1, 1, b'X', 0x91, 0, 1]
    }
    pub fn trailing_garbage_opcode() -> Vec<u8> {
        vec![0, 1, 1, b'X', 0]
    }
    pub fn source_size_mismatch() -> Vec<u8> {
        vec![5, 1, 1, b'X']
    }
}

pub fn git_cat_file_blob(repo: &Path, oid_hex: &str) -> bool {
    Command::new("git")
        .current_dir(repo)
        .args(["cat-file", "blob", oid_hex])
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

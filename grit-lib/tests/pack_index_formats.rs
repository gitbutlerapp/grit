//! Pack index v1/v2 layouts, fanout edge cases, and bounds checks (t5302/t5313/t5308).

#![allow(clippy::unwrap_used, clippy::expect_used)]

use flate2::write::ZlibEncoder;
use flate2::Compression;
use grit_lib::error::Error;
use grit_lib::objects::{HashAlgo, ObjectId, ObjectKind};
use grit_lib::odb::Odb;
use grit_lib::pack::{
    clear_pack_cache, collect_local_pack_info, pack_index_entry_matches_sha1_oid,
    parse_pack_index_bytes, read_idx_object_ids, read_local_pack_indexes,
    read_local_pack_indexes_cached, read_object_from_pack, read_object_from_packs,
    read_pack_bytes_cached, read_pack_index, read_pack_index_no_verify, skip_one_pack_object,
    verify_pack_and_collect, write_v2_pack_index, PackIndex, PackIndexEntry,
};
use grit_test_support::objects::{
    git_cat_file_batch_check, git_supports_sha256, write_pack_and_index, HashAlgo as FixtureAlgo,
    IndexPackOptions, IndexVersion, ObjectKind as FixtureKind, PackBuilder, RepoFixture,
};
use std::collections::HashSet;
use std::fs::OpenOptions;
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

fn fixture_algo(algo: HashAlgo) -> FixtureAlgo {
    match algo {
        HashAlgo::Sha1 => FixtureAlgo::Sha1,
        HashAlgo::Sha256 => FixtureAlgo::Sha256,
    }
}

fn run_both_algos(f: impl Fn(HashAlgo)) {
    f(HashAlgo::Sha1);
    if git_supports_sha256() {
        f(HashAlgo::Sha256);
    } else {
        eprintln!("SKIP: system git lacks sha256 object format");
    }
}

fn git_show_index_lines(idx_path: &Path, sha256: bool) -> Vec<(String, u64, Option<u32>)> {
    let bytes = std::fs::read(idx_path).expect("read idx");
    let mut child = Command::new("git")
        .args([
            "show-index",
            if sha256 {
                "--object-format=sha256"
            } else {
                "--object-format=sha1"
            },
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .spawn()
        .expect("spawn show-index");
    child
        .stdin
        .take()
        .expect("stdin")
        .write_all(&bytes)
        .expect("write idx");
    let out = child.wait_with_output().expect("show-index wait");
    assert!(
        out.status.success(),
        "git show-index: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let mut rows = Vec::new();
    for line in String::from_utf8_lossy(&out.stdout).lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let mut parts = line.split_whitespace();
        let off = parts.next().expect("offset").parse::<u64>().expect("off");
        let oid = parts.next().expect("oid").to_string();
        let crc = parts
            .next()
            .and_then(|s| s.strip_prefix('(').and_then(|t| t.strip_suffix(')')))
            .and_then(|hex| u32::from_str_radix(hex, 16).ok());
        rows.push((oid, off, crc));
    }
    rows
}

fn git_cat_file(repo: &Path, oid_hex: &str) -> (ObjectKind, Vec<u8>) {
    let out = Command::new("git")
        .current_dir(repo)
        .args(["cat-file", "-p", oid_hex])
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .output()
        .expect("cat-file");
    assert!(
        out.status.success(),
        "git cat-file -p {oid_hex}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = out.stdout;
    if stdout.starts_with(b"tree ") || stdout.starts_with(b"commit ") {
        // For trees/commits git prints headers; batch-check is enough for blobs in our fixtures.
    }
    (ObjectKind::Blob, stdout)
}

fn pack_end_bytes(pack_path: &Path, hash_bytes: usize) -> u64 {
    let len = std::fs::metadata(pack_path).expect("stat pack").len();
    len.saturating_sub(hash_bytes as u64)
}

fn oracle_index_and_reads(repo: &RepoFixture, idx_path: &Path, idx: &PackIndex, sha256: bool) {
    let git_rows = git_show_index_lines(idx_path, sha256);
    assert_eq!(idx.len(), git_rows.len());

    let pack_end = pack_end_bytes(&idx.pack_path, idx.hash_bytes());
    let mut iter_oids = HashSet::new();
    for entry in idx.iter() {
        iter_oids.insert(ObjectId::from_bytes(entry.oid()).expect("oid"));
        let _owned: PackIndexEntry = entry.into();
        if !sha256 {
            let oid = ObjectId::from_bytes(entry.oid()).expect("oid");
            assert!(pack_index_entry_matches_sha1_oid(&entry, &oid));
        }
    }
    assert_eq!(iter_oids.len(), git_rows.len());

    for (hex, off, crc) in &git_rows {
        let oid = ObjectId::from_hex(hex).expect("hex oid");
        assert!(idx.contains(&oid));
        let pos = idx.find_position(&oid).expect("find_position");
        assert_eq!(idx.find_offset(&oid), Some(*off));
        assert_eq!(idx.offset_at(pos), *off);
        assert_eq!(idx.crc32_at(pos), *crc);
        assert_eq!(idx.crc32_for_pack_offset(*off), *crc);
        assert_eq!(
            idx.find_position_by_offset_sorted(*off),
            Some(pos),
            "offset lookup for {hex}"
        );
        let next = idx.next_pack_offset_after(*off, pack_end);
        assert!(next > *off);
        assert!(next <= pack_end);

        let obj = read_object_from_pack(idx, &oid).expect("read_object_from_pack");
        let (_kind, git_data) = git_cat_file(repo.path(), hex);
        if obj.kind == ObjectKind::Blob {
            assert_eq!(obj.data, git_data);
        }
        let from_packs =
            read_object_from_packs(&repo.objects_dir(), &oid).expect("read_object_from_packs");
        assert_eq!(from_packs.data, obj.data);
    }

    let ids = read_idx_object_ids(idx_path).expect("read_idx_object_ids");
    assert_eq!(ids.len(), git_rows.len());
    let hex_ids: Vec<String> = ids.iter().map(|o| o.to_hex()).collect();
    let hex_refs: Vec<&str> = hex_ids.iter().map(String::as_str).collect();
    let batch = git_cat_file_batch_check(repo.path(), &hex_refs);
    assert!(batch.ok, "batch-check: {:?}", batch.lines);
}

fn rich_pack_fixture(algo: HashAlgo) -> (RepoFixture, Vec<u8>, Vec<ObjectId>) {
    let repo = RepoFixture::init(fixture_algo(algo)).expect("init");
    for i in 0..8u8 {
        let name = format!("file-{i}.txt");
        std::fs::write(repo.path().join(&name), format!("payload {i}\n")).expect("write");
        repo.git(&["add", &name]);
    }
    assert!(repo.git(&["commit", "-qm", "seed"]).ok);
    repo.git(&["repack", "-a", "-d"]);
    let objects = repo.objects_dir();
    let pack_path = std::fs::read_dir(objects.join("pack"))
        .expect("pack dir")
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .find(|p| p.extension().is_some_and(|x| x == "pack"))
        .expect("pack");
    let pack_bytes = std::fs::read(&pack_path).expect("read pack");
    let idx_path = pack_path.with_extension("idx");
    let idx = read_pack_index(&idx_path).expect("idx");
    let oids: Vec<ObjectId> = read_idx_object_ids(&idx_path).expect("oids");
    assert!(oids.len() >= 3);
    let _ = idx;
    (repo, pack_bytes, oids)
}

fn zlib_pack(data: &[u8]) -> Vec<u8> {
    let mut enc = ZlibEncoder::new(Vec::new(), Compression::default());
    enc.write_all(data).expect("zlib");
    enc.finish().expect("zlib finish")
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

fn append_whole_blob(buf: &mut Vec<u8>, data: &[u8]) -> u64 {
    let off = buf.len() as u64;
    let compressed = zlib_pack(data);
    append_pack_object_header(buf, 3, data.len());
    buf.extend_from_slice(&compressed);
    off
}

fn sha1_trailer_for_file(file: &mut std::fs::File) -> ObjectId {
    let len = file.metadata().expect("meta").len();
    file.seek(SeekFrom::Start(0)).expect("seek");
    let mut hasher = HashAlgo::Sha1.hasher();
    let mut buf = [0u8; 64 * 1024];
    let mut left = len;
    while left > 0 {
        let chunk = left.min(buf.len() as u64) as usize;
        let n = file.read(&mut buf[..chunk]).expect("read");
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
        left -= n as u64;
    }
    ObjectId::from_bytes(hasher.finalize().as_bytes()).expect("trailer oid")
}

/// On-disk pack longer than the mmap owned-buffer threshold without huge compressed payloads.
fn write_sparse_blob_pack(
    pack_path: &Path,
    repo_root: &Path,
    data: &[u8],
    object_offset: u64,
) -> ObjectId {
    let odb = Odb::new(repo_root);
    let oid = odb.hash(ObjectKind::Blob, data);
    let mut file = OpenOptions::new()
        .create(true)
        .write(true)
        .read(true)
        .truncate(true)
        .open(pack_path)
        .expect("create sparse pack");
    file.write_all(b"PACK").expect("sig");
    file.write_all(&2u32.to_be_bytes()).expect("ver");
    file.write_all(&1u32.to_be_bytes()).expect("count");
    file.seek(SeekFrom::Start(object_offset)).expect("seek");
    let mut tail = Vec::new();
    append_whole_blob(&mut tail, data);
    file.write_all(&tail).expect("object");
    let trailer = sha1_trailer_for_file(&mut file);
    file.write_all(trailer.as_bytes()).expect("trailer");
    file.flush().expect("flush");
    let file_len = file.metadata().expect("stat").len();
    assert!(
        file_len > 8192,
        "sparse pack len {file_len} must exceed mmap owned-buffer threshold"
    );
    oid
}

fn index_pack_with_version(
    repo: &RepoFixture,
    stem: &str,
    pack: &[u8],
    version: IndexVersion,
) -> PathBuf {
    let opts = IndexPackOptions {
        index_version: Some(version),
        ..IndexPackOptions::default()
    };
    let out = write_pack_and_index(&repo.objects_dir(), stem, pack, repo.algo(), &opts);
    assert!(out.index_ok, "index-pack: {}", out.index_stderr);
    out.idx_path.expect("idx path")
}

#[test]
fn git_indexed_v1_v2_and_large_offset_table_oracle() {
    run_both_algos(|algo| {
        let (repo, pack, _oids) = rich_pack_fixture(algo);
        let sha256 = matches!(algo, HashAlgo::Sha256);
        let mut versions: Vec<(&str, IndexVersion)> = vec![
            ("v2", IndexVersion::V2),
            ("v2-64", IndexVersion::V2LargeOffsetAt(0x40)),
        ];
        if matches!(algo, HashAlgo::Sha1) {
            versions.insert(0, ("v1", IndexVersion::V1));
        }
        for (stem, version) in versions {
            let idx_path = index_pack_with_version(&repo, stem, &pack, version);
            let idx = read_pack_index(&idx_path).expect("parse idx");
            oracle_index_and_reads(&repo, &idx_path, &idx, sha256);
        }
    });
}

#[test]
fn fanout_all_objects_share_first_byte() {
    let mut rows = Vec::new();
    for i in 0..32u8 {
        let mut oid = vec![0xab_u8; 20];
        oid[19] = i;
        rows.push((oid, u64::from(i) * 12 + 100));
    }
    let mut body = Vec::new();
    let mut sorted_oids: Vec<&[u8]> = rows.iter().map(|(o, _)| o.as_slice()).collect();
    sorted_oids.sort();
    let mut fanout = [0u32; 256];
    let mut idx = 0usize;
    for byte in 0..256usize {
        while idx < sorted_oids.len()
            && sorted_oids[idx].first().copied().unwrap_or(0) <= byte as u8
        {
            idx += 1;
        }
        fanout[byte] = idx as u32;
    }
    for slot in fanout {
        body.extend_from_slice(&slot.to_be_bytes());
    }
    rows.sort_by(|a, b| a.0.cmp(&b.0));
    for (oid, off) in &rows {
        body.extend_from_slice(&(*off as u32).to_be_bytes());
        body.extend_from_slice(oid);
    }
    body.extend_from_slice(&HashAlgo::Sha1.digest(&body).as_bytes().to_vec());
    let idx = parse_pack_index_bytes(Path::new("one-bucket.idx"), body, false).expect("v1 idx");
    assert_eq!(idx.fanout[0xab], 32);
    assert_eq!(idx.fanout[0xac], 32);
    for (oid, off) in rows {
        let id = ObjectId::from_bytes(&oid).expect("oid");
        assert_eq!(idx.find_offset(&id), Some(off));
    }
}

#[test]
fn fanout_first_and_last_bucket_hits() {
    let (repo, _pack, oids) = rich_pack_fixture(HashAlgo::Sha1);
    let idx_path = std::fs::read_dir(repo.objects_dir().join("pack"))
        .unwrap()
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .find(|p| p.extension().is_some_and(|x| x == "idx"))
        .expect("idx");
    let idx = read_pack_index(&idx_path).expect("idx");
    let mut sorted = oids;
    sorted.sort_by_key(|o| o.to_hex());
    let first = sorted.first().expect("first");
    let last = sorted.last().expect("last");
    let fb = first.as_bytes()[0] as usize;
    let lb = last.as_bytes()[0] as usize;
    assert!(idx.find_position(first).is_some());
    assert!(idx.find_position(last).is_some());
    assert!(idx.fanout[fb] >= 1);
    assert_eq!(idx.fanout[lb], idx.len() as u32);
}

#[test]
fn empty_pack_index_from_git() {
    run_both_algos(|algo| {
        let repo = RepoFixture::init(fixture_algo(algo)).expect("init");
        let builder = PackBuilder::new(fixture_algo(algo));
        let built = builder.build();
        assert_eq!(built.entry_offsets.len(), 0);
        let idx_path = index_pack_with_version(&repo, "empty", &built.bytes, IndexVersion::V2);
        let idx = read_pack_index(&idx_path).expect("empty idx");
        assert!(idx.is_empty());
        assert_eq!(read_idx_object_ids(&idx_path).expect("ids").len(), 0);
    });
}

fn assert_corrupt(result: Result<(), Error>) {
    match result {
        Err(Error::CorruptObject(_)) => {}
        other => panic!("expected CorruptObject, got {other:?}"),
    }
}

#[test]
fn truncated_idx_bad_magic_version_and_trailer() {
    let scratch = tempfile::tempdir().expect("scratch");
    let repo = RepoFixture::init(FixtureAlgo::Sha1).expect("init");
    let mut builder = PackBuilder::new(FixtureAlgo::Sha1);
    builder.add_full(FixtureKind::Blob, b"idx-trailer-probe");
    let built = builder.build();
    let idx_path = index_pack_with_version(&repo, "base", &built.bytes, IndexVersion::V2);
    let bytes = std::fs::read(&idx_path).expect("read");
    let trunc_path = scratch.path().join("trunc.idx");
    let trunc_len = 64.min(bytes.len().saturating_sub(1));
    std::fs::write(&trunc_path, &bytes[..trunc_len]).expect("write truncated idx");
    assert_corrupt(read_pack_index(&trunc_path).map(|_| ()));

    let mut bad_magic = bytes.clone();
    bad_magic[0] ^= 0xff;
    assert_corrupt(parse_pack_index_bytes(&trunc_path, bad_magic.clone(), true).map(|_| ()));

    let mut bad_ver = bytes;
    bad_ver[7] = 3;
    assert_corrupt(parse_pack_index_bytes(Path::new("bad-ver.idx"), bad_ver, false).map(|_| ()));

    let bad_trailer = scratch.path().join("bad-trailer.idx");
    let mut trailer_bytes = std::fs::read(&idx_path).expect("read for trailer");
    let last = trailer_bytes.len() - 1;
    trailer_bytes[last] ^= 0xff;
    std::fs::write(&bad_trailer, &trailer_bytes).expect("write bad trailer idx");
    assert_corrupt(read_pack_index(&bad_trailer).map(|_| ()));
    read_pack_index_no_verify(&bad_trailer).expect("no-verify loads bad trailer");
}

#[test]
fn pack_index_object_count_mismatch_errors() {
    let scratch = tempfile::tempdir().expect("scratch");
    let repo = RepoFixture::init(FixtureAlgo::Sha1).expect("init");
    let mut builder = PackBuilder::new(FixtureAlgo::Sha1);
    builder.add_full(FixtureKind::Blob, b"a");
    builder.add_full(FixtureKind::Blob, b"b");
    let built = builder.build();
    let idx_path = index_pack_with_version(&repo, "count", &built.bytes, IndexVersion::V2);
    let idx = read_pack_index(&idx_path).expect("idx");
    let oid = read_idx_object_ids(&idx_path).expect("ids")[0];
    let bad_pack = scratch.path().join("bad-count.pack");
    let mut pack_bytes = std::fs::read(&idx.pack_path).expect("pack");
    pack_bytes[11] = pack_bytes[11].wrapping_add(1);
    std::fs::write(&bad_pack, &pack_bytes).expect("bump pack count");
    let mut idx_bad = idx.clone();
    idx_bad.pack_path = bad_pack;
    let err = read_object_from_pack(&idx_bad, &oid).unwrap_err();
    assert!(
        matches!(err, Error::CorruptObject(_) | Error::Io(_)),
        "unexpected error: {err:?}"
    );

    let mut builder2 = PackBuilder::new(FixtureAlgo::Sha1);
    builder2.add_full(FixtureKind::Blob, b"a");
    builder2.add_full(FixtureKind::Blob, b"b");
    builder2.set_header_object_count(99);
    let bogus_count = builder2.build();
    let out = write_pack_and_index(
        &repo.objects_dir(),
        "bogus-header",
        &bogus_count.bytes,
        FixtureAlgo::Sha1,
        &IndexPackOptions {
            index_version: Some(IndexVersion::V2),
            ..IndexPackOptions::default()
        },
    );
    assert!(
        !out.index_ok,
        "git must reject pack header/index count skew"
    );
}

#[test]
fn bogus_offsets_and_ofs_delta_rejected_without_panic() {
    let scratch = tempfile::tempdir().expect("scratch");
    let repo = RepoFixture::init(FixtureAlgo::Sha1).expect("init");
    let mut builder = PackBuilder::new(FixtureAlgo::Sha1);
    builder.add_full(FixtureKind::Blob, b"first");
    builder.add_full(FixtureKind::Blob, b"second");
    let built = builder.build();
    let idx_path = index_pack_with_version(&repo, "offsets", &built.bytes, IndexVersion::V2);
    let pack_path = idx_path.with_extension("pack");
    let pack_len = std::fs::metadata(&pack_path).expect("stat").len();
    let ref_idx = read_pack_index(&idx_path).expect("ref idx");
    let oid_bytes = ref_idx.oid_at(0).to_vec();
    let bogus: u32 = (pack_len + 4096) as u32;
    let mut body = Vec::new();
    let mut fanout = [0u32; 256];
    for slot in 0..256usize {
        fanout[slot] = if slot >= oid_bytes[0] as usize { 1 } else { 0 };
    }
    for slot in fanout {
        body.extend_from_slice(&slot.to_be_bytes());
    }
    body.extend_from_slice(&bogus.to_be_bytes());
    body.extend_from_slice(&oid_bytes);
    body.extend_from_slice(HashAlgo::Sha1.digest(&body).as_bytes());
    let corrupt_v1 = scratch.path().join("corrupt-v1.idx");
    std::fs::write(&corrupt_v1, &body).expect("write synthetic v1");
    let idx = read_pack_index_no_verify(&corrupt_v1).expect("parse corrupt v1");
    let oid = ObjectId::from_bytes(&oid_bytes).expect("oid");
    let mut idx = idx;
    idx.pack_path = pack_path.clone();
    assert!(
        read_object_from_pack(&idx, &oid).is_err(),
        "bogus v1 offset must not read successfully"
    );

    // Corrupt 32-bit offset / large table on a low-threshold v2 index.
    let idx_path = index_pack_with_version(
        &repo,
        "offsets-64",
        &built.bytes,
        IndexVersion::V2LargeOffsetAt(0x40),
    );
    let idx = read_pack_index(&idx_path).expect("v2 idx");
    let oid0 = ObjectId::from_bytes(idx.oid_at(0)).expect("oid0");

    let mut raw = std::fs::read(&idx_path).expect("raw idx");
    let hb = idx.hash_bytes();
    let count = idx.len();
    let off32_start = 8 + 256 * 4 + count * hb + count * 4;
    // Bogus v2 offset without MSB set but value beyond pack (simulates t5313).
    let second_off = idx.offset_at(1) as u32;
    let bad32: u32 = second_off.saturating_add(8);
    raw[off32_start..off32_start + 4].copy_from_slice(&bad32.to_be_bytes());
    let bad32_path = scratch.path().join("bad32.idx");
    std::fs::write(&bad32_path, &raw).expect("write bad32");
    std::fs::copy(&pack_path, scratch.path().join("bad32.pack")).expect("pair pack");
    assert!(
        verify_pack_and_collect(&bad32_path).is_err(),
        "bogus v2 32-bit offset must fail verify_pack_and_collect"
    );
    let _ = oid0;

    let tiny_built = {
        let mut b = PackBuilder::new(FixtureAlgo::Sha1);
        b.add_full(FixtureKind::Blob, b"x");
        b.build()
    };
    let ext_idx_path = index_pack_with_version(
        &repo,
        "ext64",
        &tiny_built.bytes,
        IndexVersion::V2LargeOffsetAt(1),
    );
    let ext_idx = read_pack_index(&ext_idx_path).expect("64-bit idx");
    let mut ext_raw = std::fs::read(&ext_idx_path).expect("raw ext");
    let ext_count = ext_idx.len();
    let ext_off32 = 8 + 256 * 4 + ext_count * hb + ext_count * 4;
    let large_base = ext_off32 + ext_count * 4;
    let ext_pack_len = std::fs::metadata(ext_idx_path.with_extension("pack"))
        .expect("stat ext pack")
        .len();
    let bogus64: u64 = ext_pack_len + 500;
    ext_raw[large_base..large_base + 8].copy_from_slice(&bogus64.to_be_bytes());
    let ext_bad = scratch.path().join("ext64-bad.idx");
    std::fs::write(&ext_bad, &ext_raw).expect("write ext bad");
    std::fs::copy(
        ext_idx_path.with_extension("pack"),
        scratch.path().join("ext64-bad.pack"),
    )
    .expect("pair ext pack");
    assert!(
        verify_pack_and_collect(&ext_bad).is_err(),
        "bogus 64-bit extension offset must fail verify_pack_and_collect"
    );

    // Bogus OFS_DELTA distance in pack stream (t5313).
    let base_data = b"base-for-delta";
    let mut delta_builder = PackBuilder::new(FixtureAlgo::Sha1);
    let base_i = delta_builder.add_full(FixtureKind::Blob, base_data);
    let mut delta_ops = grit_test_support::objects::DeltaOps::new();
    delta_ops.header(base_data.len(), base_data.len());
    delta_ops.copy(0, base_data.len());
    let delta = delta_ops.finish();
    delta_builder.add_ofs_delta(base_i, &delta, delta.len());
    let mut delta_built = delta_builder.build();
    let start = delta_built.entry_offsets[1] as u64;
    delta_built.bytes[start as usize + 2] ^= 0xff;
    let mut pos = start as usize;
    assert!(
        skip_one_pack_object(&delta_built.bytes, &mut pos, start, 20).is_err(),
        "bogus ofs-delta varint must fail skip_one_pack_object"
    );
}

#[test]
fn duplicate_index_entries_remain_readable() {
    run_both_algos(|algo| {
        let repo = RepoFixture::init(fixture_algo(algo)).expect("init");
        std::fs::write(repo.path().join("dup.txt"), b"dup-me\n").expect("write");
        assert!(repo.git(&["add", "dup.txt"]).ok);
        assert!(repo.git(&["commit", "-qm", "c"]).ok);
        let oid_hex = repo
            .git(&["hash-object", "dup.txt"])
            .stdout
            .trim()
            .to_string();
        let stdin = format!("{oid_hex}\n{oid_hex}\n");
        let mut child = Command::new("git")
            .current_dir(repo.path())
            .args(["pack-objects", "dup-pack"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_SYSTEM", "/dev/null")
            .spawn()
            .expect("pack-objects");
        child
            .stdin
            .as_mut()
            .unwrap()
            .write_all(stdin.as_bytes())
            .unwrap();
        let pack_out = child.wait_with_output().unwrap();
        assert!(
            pack_out.status.success(),
            "pack-objects: {}",
            String::from_utf8_lossy(&pack_out.stderr)
        );
        let stem = String::from_utf8_lossy(&pack_out.stdout).trim().to_string();
        let pack_path = repo.path().join(format!("dup-pack-{stem}.pack"));
        let idx_path = pack_path.with_extension("idx");
        assert!(
            repo.git(&["index-pack", "-v", pack_path.to_str().unwrap()])
                .ok
        );
        let idx = read_pack_index(&idx_path).expect("dup idx");
        let oid = ObjectId::from_hex(&oid_hex).expect("oid");
        assert!(idx.find_position(&oid).is_some());
        read_object_from_pack(&idx, &oid).expect("read duplicate slot");
    });
}

#[test]
fn large_pack_uses_mmap_backing_for_reads() {
    clear_pack_cache();
    let repo = RepoFixture::init(FixtureAlgo::Sha1).expect("init");
    std::fs::create_dir_all(repo.objects_dir().join("pack")).expect("pack dir");
    let pack_path = repo.objects_dir().join("pack/mmap-threshold.pack");
    let object_offset = 9000_u64;
    let oid = write_sparse_blob_pack(
        &pack_path,
        repo.path(),
        b"mmap-backed blob payload",
        object_offset,
    );
    let idx_path = pack_path.with_extension("idx");
    let pack_bytes = std::fs::read(&pack_path).expect("read sparse pack");
    let mut end = object_offset as usize;
    skip_one_pack_object(&pack_bytes, &mut end, object_offset, 20).expect("walk object");
    let crc = crc32fast::hash(&pack_bytes[object_offset as usize..end]);
    write_v2_pack_index(&idx_path, &pack_path, &[(oid, object_offset, crc)], 20)
        .expect("write idx");
    let idx = read_pack_index(&idx_path).expect("idx");
    let pack_len = std::fs::metadata(&pack_path).expect("stat").len();
    let cached = read_pack_bytes_cached(&idx.pack_path).expect("mmap pack bytes");
    assert_eq!(cached.len(), pack_len as usize);
    read_object_from_pack(&idx, &oid).expect("read via mmap-backed pack");
}

#[test]
fn collect_local_pack_info_and_cache_invalidation() {
    clear_pack_cache();
    run_both_algos(|algo| {
        let (repo, _pack, oids) = rich_pack_fixture(algo);
        let objects = repo.objects_dir();
        let info = collect_local_pack_info(&objects).expect("collect");
        assert!(info.pack_count >= 1);
        assert!(info.object_count >= oids.len());
        assert!(info.object_ids.len() >= oids.len());

        let listed = read_local_pack_indexes(&objects).expect("list");
        assert_eq!(listed.len(), info.pack_count);

        read_local_pack_indexes_cached(&objects).expect("warm cache");
        let pack_path = objects
            .join("pack")
            .read_dir()
            .unwrap()
            .find_map(|e| {
                let p = e.ok()?.path();
                if p.extension().is_some_and(|x| x == "pack") {
                    Some(p)
                } else {
                    None
                }
            })
            .expect("pack");
        let idx_path = pack_path.with_extension("idx");
        std::fs::remove_file(&pack_path).expect("remove pack");
        std::fs::remove_file(&idx_path).expect("remove idx");
        clear_pack_cache();
        let info2 = collect_local_pack_info(&objects).expect("after remove");
        assert_eq!(info2.pack_count, 0);
        let cached = read_local_pack_indexes_cached(&objects).expect("rescan");
        assert!(cached.is_empty());
    });
}

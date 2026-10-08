//! [`Odb::read_info`] matches `git cat-file --batch-check` across storage layouts.

use grit_lib::delta_encode::encode_lcp_delta;
use grit_lib::error::Error;
use grit_lib::midx::{write_multi_pack_index_with_options, WriteMultiPackIndexOptions};
use grit_lib::objects::{HashAlgo, ObjectId, ObjectKind};
use grit_lib::odb::Odb;
use grit_lib::pack::{clear_pack_cache, read_idx_object_ids};
use grit_test_support::{git, git_cmd};
use std::collections::HashSet;
use std::io::Write;
use std::path::{Path, PathBuf};

fn zlib_pack(data: &[u8]) -> Vec<u8> {
    use flate2::write::ZlibEncoder;
    use flate2::Compression;
    let mut enc = ZlibEncoder::new(Vec::new(), Compression::default());
    enc.write_all(data).unwrap();
    enc.finish().unwrap()
}

fn append_pack_object_header(buf: &mut Vec<u8>, type_code: u8, payload_len: usize) {
    let mut size = payload_len;
    let first = ((type_code & 0x7) << 4) | (size & 0x0f) as u8;
    size >>= 4;
    if size > 0 {
        buf.push(first | 0x80);
        while size > 0 {
            let b = (size & 0x7f) as u8;
            size >>= 7;
            buf.push(if size > 0 { b | 0x80 } else { b });
        }
    } else {
        buf.push(first);
    }
}

fn encode_ofs_delta_distance(buf: &mut Vec<u8>, mut ofs: u64) {
    let mut dheader = [0u8; 32];
    let mut pos = dheader.len() - 1;
    dheader[pos] = (ofs & 0x7f) as u8;
    while {
        ofs >>= 7;
        ofs != 0
    } {
        pos -= 1;
        ofs -= 1;
        dheader[pos] = 0x80 | ((ofs & 0x7f) as u8);
    }
    buf.extend_from_slice(&dheader[pos..]);
}

fn append_whole_object(buf: &mut Vec<u8>, type_bits: u8, payload: &[u8]) -> u64 {
    let off = buf.len() as u64;
    append_pack_object_header(buf, type_bits, payload.len());
    buf.extend_from_slice(&zlib_pack(payload));
    off
}

fn append_ofs_delta(buf: &mut Vec<u8>, base_offset: u64, delta: &[u8]) -> u64 {
    let object_start = buf.len() as u64;
    append_pack_object_header(buf, 6, delta.len());
    let dist = object_start - base_offset;
    encode_ofs_delta_distance(buf, dist);
    buf.extend_from_slice(&zlib_pack(delta));
    object_start
}

fn append_ref_delta(buf: &mut Vec<u8>, base_oid: &ObjectId, delta: &[u8]) {
    append_pack_object_header(buf, 7, delta.len());
    buf.extend_from_slice(base_oid.as_bytes());
    buf.extend_from_slice(&zlib_pack(delta));
}

fn append_sha1_pack_trailer(buf: &mut Vec<u8>) {
    buf.extend_from_slice(HashAlgo::Sha1.digest(buf).as_bytes());
}

fn write_v2_idx(idx_path: &Path, pack_path: &Path, entries: &[(ObjectId, u64)]) {
    let mut sorted = entries.to_vec();
    sorted.sort_by(|a, b| a.0.as_bytes().cmp(b.0.as_bytes()));
    let n = sorted.len();
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
    buf.extend_from_slice(&pack_bytes[pack_bytes.len() - 20..]);
    let digest = HashAlgo::Sha1.digest(&buf);
    buf.extend_from_slice(digest.as_bytes());
    std::fs::write(idx_path, buf).expect("write idx");
}

fn install_pack_manual(objects: &Path, stem: &str, pack: &[u8], entries: &[(ObjectId, u64)]) {
    let pack_dir = objects.join("pack");
    std::fs::create_dir_all(&pack_dir).unwrap();
    let pack_path = pack_dir.join(format!("{stem}.pack"));
    let idx_path = pack_dir.join(format!("{stem}.idx"));
    std::fs::write(&pack_path, pack).unwrap();
    write_v2_idx(&idx_path, &pack_path, entries);
    clear_pack_cache();
}

fn collect_object_ids(objects: &Path) -> HashSet<ObjectId> {
    let mut out = HashSet::new();
    if let Ok(rd) = std::fs::read_dir(objects) {
        for entry in rd.filter_map(Result::ok) {
            let name = entry.file_name();
            let name = name.to_string_lossy();
            if name.len() == 2 && name.chars().all(|c| c.is_ascii_hexdigit()) {
                let prefix_dir = entry.path();
                if let Ok(sub) = std::fs::read_dir(prefix_dir) {
                    for f in sub.filter_map(Result::ok) {
                        let hex = f.file_name().to_string_lossy().to_string();
                        if hex.len() == 38 {
                            if let Ok(oid) = ObjectId::from_hex(&format!("{name}{hex}")) {
                                out.insert(oid);
                            }
                        }
                    }
                }
            }
        }
    }
    let pack_dir = objects.join("pack");
    if let Ok(rd) = std::fs::read_dir(&pack_dir) {
        for entry in rd.filter_map(Result::ok) {
            let path = entry.path();
            if path.extension().is_some_and(|e| e == "idx") {
                if let Ok(ids) = read_idx_object_ids(&path) {
                    out.extend(ids);
                }
            }
        }
    }
    out
}

struct ReadInfoFixture {
    loose_oid: ObjectId,
    alt_oid: ObjectId,
    whole_pack_oid: ObjectId,
    ofs_delta_oid: ObjectId,
    ref_delta_oid: ObjectId,
}

fn install_pack_git_fix_thin(git_dir: &Path, pack: &[u8]) {
    git_cmd(&["index-pack", "-v", "--fix-thin", "--stdin"])
        .in_dir(git_dir)
        .stdin(pack.to_vec())
        .suc();
    clear_pack_cache();
}

fn build_fixture() -> (
    tempfile::TempDir,
    PathBuf,
    Odb,
    HashSet<ObjectId>,
    ReadInfoFixture,
) {
    clear_pack_cache();
    let root = tempfile::tempdir().expect("tempdir");
    let git_dir = root.path().join("repo.git");
    git_cmd(&["init", "--bare", "repo.git"])
        .in_dir(root.path())
        .suc();
    let objects = git_dir.join("objects");
    std::fs::create_dir_all(&objects).expect("objects dir");

    let setup_odb = Odb::new(&objects);
    let loose_body = b"loose object payload for read_info\n";
    let loose_oid = setup_odb
        .write(ObjectKind::Blob, loose_body)
        .expect("loose write");

    let alt_objects = root.path().join("alt-objects");
    let alt_odb = Odb::new(&alt_objects);
    let alt_body = b"alternate-store blob\n";
    let alt_oid = alt_odb
        .write(ObjectKind::Blob, alt_body)
        .expect("alt write");

    let base = b"common prefix base for read_info packs";
    let ref_target = b"common prefix target for ref delta";
    let base_oid = HashAlgo::Sha1.hash_object(ObjectKind::Blob, base);
    let ref_delta = encode_lcp_delta(base, ref_target).expect("ref delta");
    let ref_oid = HashAlgo::Sha1.hash_object(ObjectKind::Blob, ref_target);

    let mut pack_base = Vec::new();
    pack_base.extend_from_slice(b"PACK");
    pack_base.extend_from_slice(&2u32.to_be_bytes());
    pack_base.extend_from_slice(&1u32.to_be_bytes());
    let off_base = append_whole_object(&mut pack_base, 3, base);
    append_sha1_pack_trailer(&mut pack_base);
    install_pack_manual(&objects, "base", &pack_base, &[(base_oid, off_base)]);

    let mut pack_tip = Vec::new();
    pack_tip.extend_from_slice(b"PACK");
    pack_tip.extend_from_slice(&2u32.to_be_bytes());
    pack_tip.extend_from_slice(&1u32.to_be_bytes());
    append_ref_delta(&mut pack_tip, &base_oid, &ref_delta);
    append_sha1_pack_trailer(&mut pack_tip);
    install_pack_git_fix_thin(&git_dir, &pack_tip);

    std::fs::create_dir_all(objects.join("info")).expect("info dir");
    let alt_canonical = std::fs::canonicalize(&alt_objects).expect("canonical alt");
    std::fs::write(
        objects.join("info/alternates"),
        format!("{}\n", alt_canonical.display()),
    )
    .expect("alternates");

    let mut ofs_target = base.to_vec();
    ofs_target.extend_from_slice(b"\nofs suffix\n");
    let ofs_delta = encode_lcp_delta(base, &ofs_target).expect("ofs delta");
    let ofs_oid = HashAlgo::Sha1.hash_object(ObjectKind::Blob, &ofs_target);
    let mut pack_ofs = Vec::new();
    pack_ofs.extend_from_slice(b"PACK");
    pack_ofs.extend_from_slice(&2u32.to_be_bytes());
    pack_ofs.extend_from_slice(&2u32.to_be_bytes());
    let ofs_base_off = append_whole_object(&mut pack_ofs, 3, base);
    let ofs_off = append_ofs_delta(&mut pack_ofs, ofs_base_off, &ofs_delta);
    append_sha1_pack_trailer(&mut pack_ofs);
    install_pack_manual(
        &objects,
        "ofs",
        &pack_ofs,
        &[(base_oid, ofs_base_off), (ofs_oid, ofs_off)],
    );

    let mut ids = collect_object_ids(&objects);
    ids.insert(loose_oid);
    ids.insert(alt_oid);

    let fixture = ReadInfoFixture {
        loose_oid,
        alt_oid,
        whole_pack_oid: base_oid,
        ofs_delta_oid: ofs_oid,
        ref_delta_oid: ref_oid,
    };

    let odb = Odb::new(&objects).with_config_git_dir(git_dir.clone());

    (root, git_dir, odb, ids, fixture)
}

fn assert_read_info_matches_git(odb: &Odb, git_dir: &Path, oid: &ObjectId, label: &str) {
    let read = odb.read(oid);
    assert!(read.is_ok(), "{label}: grit read: {read:?}");
    let hex = oid.to_hex();
    assert!(
        git_cmd(&["cat-file", "-e", &hex])
            .in_dir(git_dir)
            .exec()
            .ok(),
        "{label}: git cat-file -e"
    );
    let info = odb.read_info(oid).expect("read_info");
    let git_kind = git(git_dir, &["cat-file", "-t", &hex]).trim().to_string();
    let git_size: u64 = git(git_dir, &["cat-file", "-s", &hex])
        .trim()
        .parse()
        .expect("size");
    assert_eq!(info.kind.as_str(), git_kind, "kind for {label}");
    assert_eq!(info.size, git_size, "size for {label}");
}

#[test]
fn read_info_matches_git_batch_check() {
    let (_root, git_dir, odb, _ids, fx) = build_fixture();
    assert_read_info_matches_git(&odb, &git_dir, &fx.loose_oid, "loose");
    assert_read_info_matches_git(&odb, &git_dir, &fx.alt_oid, "alternate");
    assert_read_info_matches_git(&odb, &git_dir, &fx.whole_pack_oid, "whole pack");
    assert_read_info_matches_git(&odb, &git_dir, &fx.ofs_delta_oid, "ofs delta");
    assert_read_info_matches_git(&odb, &git_dir, &fx.ref_delta_oid, "ref delta");
}

#[test]
fn read_info_midx_matches_full_read() {
    clear_pack_cache();
    let (_root, git_dir, odb, ids, _fx) = build_fixture();
    let pack_dir = git_dir.join("objects/pack");
    write_multi_pack_index_with_options(&pack_dir, &WriteMultiPackIndexOptions::default())
        .expect("write midx");
    std::fs::write(
        git_dir.join("config"),
        "[core]\n\trepositoryformatversion = 0\n\tmultiPackIndex = true\n",
    )
    .expect("config");
    let odb = odb.with_config_git_dir(git_dir);
    for oid in ids.iter() {
        let Ok(obj) = odb.read(oid) else {
            continue;
        };
        let info = odb.read_info(oid).expect("read_info");
        assert_eq!(info.kind, obj.kind, "kind for {}", oid.to_hex());
        assert_eq!(
            info.size,
            u64::try_from(obj.data.len()).expect("size"),
            "size for {}",
            oid.to_hex()
        );
    }
}

#[test]
fn read_info_missing_object_is_not_found() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let objects = tmp.path().join("objects");
    std::fs::create_dir_all(&objects).expect("objects");
    let odb = Odb::new(&objects);
    let missing = ObjectId::from_hex("0123456789abcdef0123456789abcdef01234567").expect("oid");
    match odb.read_info(&missing) {
        Err(Error::ObjectNotFound(_)) => {}
        other => panic!("expected ObjectNotFound, got {other:?}"),
    }
}

#[test]
fn read_info_corrupt_loose_header_is_typed_error() {
    use flate2::write::ZlibEncoder;
    use flate2::Compression;

    let tmp = tempfile::tempdir().expect("tempdir");
    let objects = tmp.path().join("objects");
    let oid = ObjectId::from_hex("deadbeefdeadbeefdeadbeefdeadbeefdeadbeef").expect("oid");
    let path = objects.join(oid.loose_prefix()).join(oid.loose_suffix());
    std::fs::create_dir_all(path.parent().expect("parent")).expect("mkdir");
    let bad = b"not-a-valid-header";
    let mut enc = ZlibEncoder::new(Vec::new(), Compression::default());
    enc.write_all(bad).expect("zlib");
    let compressed = enc.finish().expect("finish");
    std::fs::write(&path, compressed).expect("write loose");

    let odb = Odb::new(&objects);
    match odb.read_info(&oid) {
        Err(Error::CorruptObject(_)) | Err(Error::ObjectHeaderTooLong { .. }) => {}
        other => panic!("expected corrupt header error, got {other:?}"),
    }
}

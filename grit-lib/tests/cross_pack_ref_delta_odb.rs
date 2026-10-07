//! `Odb::read` resolves REF_DELTA when the base object lives in another pack.

use grit_lib::delta_encode::encode_lcp_delta;
use grit_lib::objects::HashAlgo;
use grit_lib::objects::{ObjectId, ObjectKind};
use grit_lib::odb::Odb;
use grit_lib::pack::{clear_pack_cache, read_object_from_packs};
use std::io::Write;
use std::path::Path;

fn zlib_pack(data: &[u8]) -> Vec<u8> {
    use flate2::write::ZlibEncoder;
    use flate2::Compression;
    let mut enc = ZlibEncoder::new(Vec::new(), Compression::default());
    enc.write_all(data).unwrap();
    enc.finish().unwrap()
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

fn append_ref_delta(buf: &mut Vec<u8>, base_oid: &ObjectId, delta: &[u8]) {
    let compressed = zlib_pack(delta);
    append_pack_object_header(buf, 7, delta.len());
    buf.extend_from_slice(base_oid.as_bytes());
    buf.extend_from_slice(&compressed);
}

fn append_sha1_pack_trailer(buf: &mut Vec<u8>) {
    let digest = HashAlgo::Sha1.digest(buf);
    buf.extend_from_slice(digest.as_bytes());
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

fn install_pack(objects: &Path, stem: &str, pack: &[u8], entries: &[(ObjectId, u64)]) {
    let pack_dir = objects.join("pack");
    std::fs::create_dir_all(&pack_dir).unwrap();
    let pack_path = pack_dir.join(format!("{stem}.pack"));
    let idx_path = pack_dir.join(format!("{stem}.idx"));
    std::fs::write(&pack_path, pack).unwrap();
    write_v2_idx(&idx_path, &pack_path, entries);
    clear_pack_cache();
}

#[test]
fn odb_read_cross_pack_ref_delta_applies_delta_frames() {
    clear_pack_cache();
    let tmp = tempfile::tempdir().unwrap();
    let objects = tmp.path().join("objects");
    let odb = Odb::new(&objects);
    let base = b"common prefix base";
    let target = b"common prefix target";
    let oid_base = odb.hash(ObjectKind::Blob, base);
    let delta = encode_lcp_delta(base, target).unwrap();
    let oid_target = odb.hash(ObjectKind::Blob, target);

    let mut pack_base = Vec::new();
    pack_base.extend_from_slice(b"PACK");
    pack_base.extend_from_slice(&2u32.to_be_bytes());
    pack_base.extend_from_slice(&1u32.to_be_bytes());
    let off_base = append_whole_blob(&mut pack_base, base);
    append_sha1_pack_trailer(&mut pack_base);

    let mut pack_tip = Vec::new();
    pack_tip.extend_from_slice(b"PACK");
    pack_tip.extend_from_slice(&2u32.to_be_bytes());
    pack_tip.extend_from_slice(&1u32.to_be_bytes());
    let off_tip = pack_tip.len() as u64;
    append_ref_delta(&mut pack_tip, &oid_base, &delta);
    append_sha1_pack_trailer(&mut pack_tip);

    install_pack(&objects, "base", &pack_base, &[(oid_base, off_base)]);
    install_pack(&objects, "tip", &pack_tip, &[(oid_target, off_tip)]);

    read_object_from_packs(&objects, &oid_base).unwrap();
    let got = odb.read(&oid_target).unwrap();
    assert_eq!(got.kind, ObjectKind::Blob);
    assert_eq!(got.data.as_slice(), target);
    assert_eq!(odb.hash(got.kind, &got.data), oid_target);
}

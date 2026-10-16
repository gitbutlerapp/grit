//! Write pack index files and install pack streams into an object directory.
//!
//! Used by embedders and tooling that build pack bytes in memory (for example
//! [`crate::transfer::build_pack`]) and need a Git-compatible `.pack`/`.idx` pair
//! on disk without shelling out to `git index-pack`.
//!
//! ```rust
//! use grit_lib::pack_write::InstallPackOptions;
//! # fn main() {
//! let _ = InstallPackOptions::default();
//! # }
//! ```

use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};

use flate2::read::ZlibDecoder;
use sha1::{Digest, Sha1};
use sha2::Sha256;

use crate::error::{Error, Result};
use crate::objects::{HashAlgo, ObjectId, ObjectKind};
use crate::pack::{
    clear_pack_cache, hash_object_bytes, parse_ofs_delta_base, parse_pack_object_header,
    skip_one_pack_object, PackedType,
};

/// One `(object id, pack offset)` entry used when writing a v2 index.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PackIndexWriteEntry {
    /// Canonical object id for the stored object.
    pub oid: ObjectId,
    /// Byte offset of the object header within the `.pack` file.
    pub offset: u64,
}

/// Options controlling [`install_pack_stream`].
#[derive(Debug, Clone)]
pub struct InstallPackOptions {
    /// When set, use this stem for `pack/<stem>.pack` instead of deriving one from the pack checksum.
    pub stem: Option<String>,
    /// Hash width for this repository (`20` for SHA-1, `32` for SHA-256).
    pub hash_algo: HashAlgo,
}

impl Default for InstallPackOptions {
    fn default() -> Self {
        Self {
            stem: None,
            hash_algo: HashAlgo::Sha1,
        }
    }
}

/// Validate a PACK header and return `(body_end, object_count)` for `hash_algo`.
fn validated_pack_body(pack: &[u8], hash_algo: HashAlgo) -> Result<(usize, usize)> {
    let hash_bytes = hash_algo.len();
    if pack.len() < 12 + hash_bytes {
        return Err(Error::CorruptObject("pack file too small".into()));
    }
    if &pack[0..4] != b"PACK" {
        return Err(Error::CorruptObject("invalid PACK signature".into()));
    }
    let version = u32::from_be_bytes(pack[4..8].try_into().unwrap_or([0; 4]));
    if version != 2 && version != 3 {
        return Err(Error::CorruptObject(format!(
            "unsupported pack version {version}"
        )));
    }
    let count = u32::from_be_bytes(pack[8..12].try_into().unwrap_or([0; 4])) as usize;
    let body_end = pack.len() - hash_bytes;
    Ok((body_end, count))
}

/// Scan a pack containing only whole (non-delta) objects and return sorted index entries.
///
/// # Errors
///
/// Returns [`Error::CorruptObject`] when the stream is malformed or contains delta objects.
pub fn enumerate_whole_pack_entries(
    pack: &[u8],
    hash_algo: HashAlgo,
) -> Result<Vec<PackIndexWriteEntry>> {
    let hash_bytes = hash_algo.len();
    let (body_end, count) = validated_pack_body(pack, hash_algo)?;
    let mut pos = 12usize;
    let mut out = Vec::with_capacity(count);
    while pos < body_end {
        let offset =
            u64::try_from(pos).map_err(|_| Error::CorruptObject("pack offset overflow".into()))?;
        let (packed_type, size) = parse_pack_object_header(pack, &mut pos)?;
        match packed_type {
            PackedType::Commit | PackedType::Tree | PackedType::Blob | PackedType::Tag => {
                let mut dec = ZlibDecoder::new(&pack[pos..]);
                let mut data = Vec::with_capacity(size as usize);
                dec.read_to_end(&mut data)
                    .map_err(|e| Error::Zlib(e.to_string()))?;
                pos += dec.total_in() as usize;
                let kind = packed_type_to_kind(packed_type)?;
                let oid_bytes = hash_object_bytes(kind, &data, hash_bytes)?;
                let oid = ObjectId::from_bytes(&oid_bytes)?;
                out.push(PackIndexWriteEntry { oid, offset });
            }
            PackedType::RefDelta | PackedType::OfsDelta => {
                return Err(Error::CorruptObject(
                    "enumerate_whole_pack_entries requires a non-delta pack".into(),
                ));
            }
        }
    }
    if out.len() != count {
        return Err(Error::CorruptObject(format!(
            "pack header object count {count} != scanned {}",
            out.len()
        )));
    }
    out.sort_by(|a, b| a.oid.as_bytes().cmp(b.oid.as_bytes()));
    Ok(out)
}

/// Write a version-2 pack index for `pack_path` using precomputed `entries`.
///
/// `entries` must be sorted by OID (as produced by [`enumerate_whole_pack_entries`]).
///
/// # Errors
///
/// Returns [`Error::Io`] or [`Error::CorruptObject`] on failure.
pub fn write_pack_index_v2(
    idx_path: &Path,
    pack_path: &Path,
    entries: &[PackIndexWriteEntry],
    hash_bytes: usize,
) -> Result<()> {
    let pack_bytes = fs::read(pack_path).map_err(Error::Io)?;
    if pack_bytes.len() < hash_bytes {
        return Err(Error::CorruptObject(
            "pack too small for idx trailer".into(),
        ));
    }
    let mut fanout = [0u32; 256];
    for byte in 0u32..256 {
        let count = entries
            .iter()
            .filter(|e| u32::from(e.oid.as_bytes()[0]) <= byte)
            .count();
        fanout[byte as usize] = u32::try_from(count).unwrap_or(u32::MAX);
    }
    let mut buf = Vec::new();
    buf.extend_from_slice(b"\xfftOc");
    buf.extend_from_slice(&2u32.to_be_bytes());
    for f in fanout {
        buf.extend_from_slice(&f.to_be_bytes());
    }
    for e in entries {
        buf.extend_from_slice(e.oid.as_bytes());
    }
    for e in entries {
        let start = e.offset as usize;
        let mut end = start;
        skip_one_pack_object(&pack_bytes, &mut end, e.offset, hash_bytes)
            .map_err(|err| Error::CorruptObject(format!("idx crc walk: {err}")))?;
        let crc = crc32fast::hash(&pack_bytes[start..end]);
        buf.extend_from_slice(&crc.to_be_bytes());
    }
    let (offsets32, large_offsets) = pack_index_v2_offset_tables(entries);
    for v in offsets32 {
        buf.extend_from_slice(&v.to_be_bytes());
    }
    for lo in large_offsets {
        buf.extend_from_slice(&lo.to_be_bytes());
    }
    buf.extend_from_slice(&pack_bytes[pack_bytes.len() - hash_bytes..]);
    let idx_hash = match hash_bytes {
        20 => {
            let mut hasher = Sha1::new();
            Digest::update(&mut hasher, &buf);
            hasher.finalize().to_vec()
        }
        32 => {
            let mut hasher = Sha256::new();
            hasher.update(&buf);
            hasher.finalize().to_vec()
        }
        _ => {
            return Err(Error::CorruptObject(format!(
                "unsupported hash width {hash_bytes} for pack index"
            )));
        }
    };
    buf.extend_from_slice(&idx_hash);
    fs::write(idx_path, buf).map_err(Error::Io)?;
    Ok(())
}

/// Install `pack` under `objects_dir/pack/` and write the matching v2 `.idx`.
///
/// Returns paths to the `.pack` and `.idx` files. Clears the in-process pack cache so
/// subsequent reads observe the new pack.
///
/// # Errors
///
/// Returns [`Error::CorruptObject`] when `pack` cannot be indexed, or [`Error::Io`] on failure.
pub fn install_pack_stream(
    objects_dir: &Path,
    pack: &[u8],
    opts: InstallPackOptions,
) -> Result<(PathBuf, PathBuf)> {
    let hash_bytes = opts.hash_algo.len();
    let entries = enumerate_whole_pack_entries(pack, opts.hash_algo)?;
    let pack_dir = objects_dir.join("pack");
    fs::create_dir_all(&pack_dir).map_err(Error::Io)?;
    let stem = opts.stem.unwrap_or_else(|| {
        let trailer = &pack[pack.len() - hash_bytes..];
        hex::encode(&trailer[..hash_bytes.min(12)])
    });
    let pack_path = pack_dir.join(format!("pack-{stem}.pack"));
    let idx_path = pack_dir.join(format!("pack-{stem}.idx"));
    fs::write(&pack_path, pack).map_err(Error::Io)?;
    write_pack_index_v2(&idx_path, &pack_path, &entries, hash_bytes)?;
    clear_pack_cache();
    Ok((pack_path, idx_path))
}

/// Uncompressed payload size of all whole objects in `pack`.
///
/// # Errors
///
/// Returns [`Error::CorruptObject`] when `pack` is malformed.
#[must_use = "callers use this for size reporting"]
pub fn whole_pack_uncompressed_size(pack: &[u8], hash_algo: HashAlgo) -> Result<u64> {
    let hash_bytes = hash_algo.len();
    let (body_end, header_count) = validated_pack_body(pack, hash_algo)?;
    let mut pos = 12usize;
    let mut total = 0u64;
    let mut seen = 0usize;
    while pos < body_end {
        let (packed_type, size) = parse_pack_object_header(pack, &mut pos)?;
        match packed_type {
            PackedType::Commit | PackedType::Tree | PackedType::Blob | PackedType::Tag => {
                total = total.saturating_add(size);
                let mut dec = ZlibDecoder::new(&pack[pos..]);
                let mut tmp = Vec::with_capacity(size as usize);
                dec.read_to_end(&mut tmp)
                    .map_err(|e| Error::Zlib(e.to_string()))?;
                pos += dec.total_in() as usize;
                seen += 1;
            }
            PackedType::RefDelta => {
                pos += hash_bytes;
                let mut dec = ZlibDecoder::new(&pack[pos..]);
                let mut tmp = Vec::with_capacity(size as usize);
                dec.read_to_end(&mut tmp)
                    .map_err(|e| Error::Zlib(e.to_string()))?;
                pos += dec.total_in() as usize;
                seen += 1;
            }
            PackedType::OfsDelta => {
                let obj_start = pos;
                let _base = parse_ofs_delta_base(pack, &mut pos, obj_start as u64)?;
                let mut dec = ZlibDecoder::new(&pack[pos..]);
                let mut tmp = Vec::with_capacity(size as usize);
                dec.read_to_end(&mut tmp)
                    .map_err(|e| Error::Zlib(e.to_string()))?;
                pos += dec.total_in() as usize;
                seen += 1;
            }
        }
    }
    if seen != header_count {
        return Err(Error::CorruptObject(format!(
            "pack header object count {header_count} != scanned {seen}"
        )));
    }
    Ok(total)
}

/// Build the v2 index 32-bit offset column and optional 64-bit large-offset table.
///
/// Offsets below `0x8000_0000` are stored literally. Larger offsets (or any offset
/// that would set the high bit) are stored as `0x8000_0000 | index` into `large`,
/// followed by the real offset in the 64-bit table (Git pack index v2).
#[must_use]
pub fn pack_index_v2_offset_tables(entries: &[PackIndexWriteEntry]) -> (Vec<u32>, Vec<u64>) {
    let mut offsets32 = Vec::with_capacity(entries.len());
    let mut large = Vec::new();
    for e in entries {
        if e.offset < 0x8000_0000 {
            offsets32.push(e.offset as u32);
        } else {
            let idx = u32::try_from(large.len()).unwrap_or(u32::MAX);
            offsets32.push(0x8000_0000 | idx);
            large.push(e.offset);
        }
    }
    (offsets32, large)
}

fn packed_type_to_kind(pt: PackedType) -> Result<ObjectKind> {
    match pt {
        PackedType::Commit => Ok(ObjectKind::Commit),
        PackedType::Tree => Ok(ObjectKind::Tree),
        PackedType::Blob => Ok(ObjectKind::Blob),
        PackedType::Tag => Ok(ObjectKind::Tag),
        PackedType::RefDelta | PackedType::OfsDelta => Err(Error::CorruptObject(
            "delta object in whole-object enumeration".into(),
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::objects::ObjectKind;
    use crate::odb::Odb;
    use crate::pack::read_pack_index;
    use crate::transfer::{build_pack, PackBuildOptions};

    #[test]
    fn install_pack_passes_git_verify_pack() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let odb = Odb::new(tmp.path());
        let blob = odb
            .write(ObjectKind::Blob, b"synth pack-write test\n")
            .expect("write");
        let pack = build_pack(
            &odb,
            &[blob],
            &[],
            &PackBuildOptions {
                delta: false,
                ..PackBuildOptions::default()
            },
        )
        .expect("build");
        let (pack_path, idx_path) = install_pack_stream(
            tmp.path(),
            &pack,
            InstallPackOptions {
                hash_algo: HashAlgo::Sha1,
                ..InstallPackOptions::default()
            },
        )
        .expect("install");
        let idx = read_pack_index(&idx_path).expect("read idx");
        assert_eq!(idx.entries.len(), 1);
        let pack_str = pack_path.to_str().expect("pack path utf-8");
        let out = grit_test_support::git_cmd(&["verify-pack", "-s", pack_str])
            .in_dir(tmp.path())
            .exec();
        assert!(out.ok(), "{}", out.dump("git verify-pack"));
        assert_eq!(
            idx.find_offset(&blob),
            Some(idx.entries[0].offset),
            "idx offset matches blob"
        );
    }

    #[test]
    fn pack_index_v2_large_offset_table_round_trips() {
        let entries = vec![
            PackIndexWriteEntry {
                oid: ObjectId::from_hex("0000000000000000000000000000000000000001").unwrap(),
                offset: 100,
            },
            PackIndexWriteEntry {
                oid: ObjectId::from_hex("0000000000000000000000000000000000000002").unwrap(),
                offset: 0x8000_0000 + 512,
            },
            PackIndexWriteEntry {
                oid: ObjectId::from_hex("0000000000000000000000000000000000000003").unwrap(),
                offset: (1u64 << 33) + 99,
            },
        ];
        let (offsets32, large) = pack_index_v2_offset_tables(&entries);
        assert_eq!(offsets32[0], 100);
        assert_eq!(offsets32[1], 0x8000_0000);
        assert_eq!(offsets32[2], 0x8000_0001);
        assert_eq!(large, vec![0x8000_0000 + 512, (1u64 << 33) + 99]);
        let mut decoded_large = 0usize;
        for (i, raw) in offsets32.iter().enumerate() {
            let off = if (*raw & 0x8000_0000) == 0 {
                u64::from(*raw)
            } else {
                let slot = decoded_large;
                decoded_large += 1;
                large[slot]
            };
            assert_eq!(off, entries[i].offset, "entry {i}");
        }
    }

    #[test]
    fn whole_pack_uncompressed_size_rejects_malformed_input() {
        use crate::error::Error;
        let err = whole_pack_uncompressed_size(&[], HashAlgo::Sha1).unwrap_err();
        assert!(matches!(err, Error::CorruptObject(_)));
        let truncated = b"PACK";
        let err = whole_pack_uncompressed_size(truncated, HashAlgo::Sha1).unwrap_err();
        assert!(matches!(err, Error::CorruptObject(_)));
        let bad_sig = b"NOPE\x00\x00\x00\x02\x00\x00\x00\x00";
        let err = whole_pack_uncompressed_size(bad_sig, HashAlgo::Sha1).unwrap_err();
        assert!(matches!(err, Error::CorruptObject(_)));
    }

    #[test]
    fn whole_pack_uncompressed_size_empty_pack_is_zero() {
        let mut pack = Vec::from(b"PACK" as &[u8]);
        pack.extend_from_slice(&2u32.to_be_bytes());
        pack.extend_from_slice(&0u32.to_be_bytes());
        pack.extend_from_slice(&[0u8; 20]);
        assert_eq!(
            whole_pack_uncompressed_size(&pack, HashAlgo::Sha1).unwrap(),
            0
        );
    }

    #[test]
    fn enumerate_whole_pack_matches_build_pack() {
        let odb = Odb::new(tempfile::tempdir().expect("tempdir").path());
        let a = odb.write(ObjectKind::Blob, b"aaa").unwrap();
        let b = odb.write(ObjectKind::Blob, b"bbb").unwrap();
        let pack = build_pack(
            &odb,
            &[a, b],
            &[],
            &PackBuildOptions {
                delta: false,
                ..Default::default()
            },
        )
        .unwrap();
        let entries = enumerate_whole_pack_entries(&pack, HashAlgo::Sha1).unwrap();
        assert_eq!(entries.len(), 2);
        assert!(entries
            .windows(2)
            .all(|w| w[0].oid.as_bytes() <= w[1].oid.as_bytes()));
    }
}

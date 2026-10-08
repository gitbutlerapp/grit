//! On-disk pack reverse index (`.rev`) — RIDX format matching Git's `pack-write.c`.
//!
//! Maps pack-file order (sorted by object offset) to index positions in the `.idx` OID table.

use crate::hash::verify_trailer;
use crate::objects::HashAlgo;
use crate::pack::PackIndex;
use std::path::Path;

/// Magic `RIDX` in big-endian form (same as Git's `RIDX_SIGNATURE`).
pub const RIDX_SIGNATURE: u32 = 0x5249_4458;
/// On-disk format version (Git `RIDX_VERSION`).
pub const RIDX_VERSION: u32 = 1;
/// Hash id field for SHA-1 packs (`oid_version` in Git).
pub const RIDX_HASH_ID_SHA1: u32 = 1;
/// Hash id field for SHA-256 packs (`oid_version` in Git).
pub const RIDX_HASH_ID_SHA256: u32 = 2;

const HEADER_LEN: usize = 12;
const SHA1_TRAILER: usize = 20;
const SHA256_TRAILER: usize = 32;

/// The RIDX hash-id field for a given trailing-hash width.
const fn ridx_hash_id(hash_len: usize) -> u32 {
    if hash_len == SHA256_TRAILER {
        RIDX_HASH_ID_SHA256
    } else {
        RIDX_HASH_ID_SHA1
    }
}

/// Append the hashfile body checksum (`hash_len`-wide) over `out` so far.
///
/// Used for pack `.rev` files and MIDX `.rev` sidecars (RIDX format).
pub fn append_hashfile_checksum(out: &mut Vec<u8>, hash_len: usize) {
    let algo = HashAlgo::from_len(hash_len).unwrap_or(HashAlgo::Sha1);
    out.extend_from_slice(algo.digest(&*out).as_bytes());
}

/// True if `data` is a valid hashfile of trailing width `hash_len`: the last
/// `hash_len` bytes equal the hash (SHA-1 or SHA-256) of the preceding bytes.
#[must_use]
pub fn hashfile_checksum_valid(data: &[u8], hash_len: usize) -> bool {
    if data.len() < hash_len {
        return false;
    }
    HashAlgo::from_len(hash_len).is_some_and(|algo| verify_trailer(algo, data).is_ok())
}

/// Build `.rev` file bytes for a pack index (RIDX body + trailing SHA-1 of the body).
#[must_use]
pub fn build_pack_rev_bytes(index: &PackIndex) -> Vec<u8> {
    let offsets: Vec<u64> = index.iter().map(|e| e.offset()).collect();
    build_pack_rev_bytes_from_index_order_offsets(&offsets)
}

/// Build `.rev` bytes from object offsets in **pack index order** (OID-sorted, same row order as `.idx`).
#[must_use]
pub fn build_pack_rev_bytes_from_index_order_offsets(offsets: &[u64]) -> Vec<u8> {
    build_pack_rev_bytes_from_index_order_offsets_and_checksum(offsets, &[0u8; SHA1_TRAILER])
}

/// Build `.rev` bytes from index-order offsets and the corresponding pack checksum.
#[must_use]
pub fn build_pack_rev_bytes_from_index_order_offsets_and_checksum(
    offsets: &[u64],
    pack_checksum: &[u8],
) -> Vec<u8> {
    // The trailing-hash width is implied by the pack checksum (20 for SHA-1,
    // 32 for SHA-256); fall back to SHA-1 width for an unexpected length.
    let hash_len = if pack_checksum.len() == SHA256_TRAILER {
        SHA256_TRAILER
    } else {
        SHA1_TRAILER
    };
    let n = offsets.len();
    let mut order: Vec<u32> = (0..n as u32).collect();
    order.sort_by_key(|&i| offsets[i as usize]);

    let body_len = HEADER_LEN + n * 4 + hash_len;
    let total_len = body_len + hash_len;
    let mut out = Vec::with_capacity(total_len);

    out.extend_from_slice(&RIDX_SIGNATURE.to_be_bytes());
    out.extend_from_slice(&RIDX_VERSION.to_be_bytes());
    out.extend_from_slice(&ridx_hash_id(hash_len).to_be_bytes());
    for idx_pos in order {
        out.extend_from_slice(&idx_pos.to_be_bytes());
    }
    if pack_checksum.len() >= hash_len {
        out.extend_from_slice(&pack_checksum[..hash_len]);
    } else {
        out.extend_from_slice(&vec![0u8; hash_len]);
    }

    debug_assert_eq!(out.len(), body_len);
    // Git's hashfile appends the hash of the body; it is unrelated to the pack checksum.
    append_hashfile_checksum(&mut out, hash_len);
    debug_assert_eq!(out.len(), total_len);

    out
}

/// Path for the reverse index alongside a `.idx` file (`other.idx` → `other.rev`).
#[must_use]
pub fn rev_path_for_index(idx_path: &Path) -> std::path::PathBuf {
    let mut p = idx_path.to_path_buf();
    p.set_extension("rev");
    p
}

/// True if `data` is a valid SHA-1 hashfile: last 20 bytes equal SHA-1 of preceding bytes.
#[must_use]
pub fn hashfile_checksum_valid_sha1(data: &[u8]) -> bool {
    if data.len() < SHA1_TRAILER {
        return false;
    }
    verify_trailer(HashAlgo::Sha1, data).is_ok()
}

fn read_u32_be(buf: &[u8], pos: &mut usize) -> Option<u32> {
    if *pos + 4 > buf.len() {
        return None;
    }
    let v = u32::from_be_bytes(buf[*pos..*pos + 4].try_into().ok()?);
    *pos += 4;
    Some(v)
}

/// Verify a `.rev` file for `index-pack --rev-index --verify` (checksum first, like Git's
/// `verify_pack_revindex`).
pub fn verify_pack_rev_file_contents(
    data: &[u8],
    index: &PackIndex,
    path_for_errors: &str,
) -> std::result::Result<(), String> {
    let hash_len = index.hash_bytes();
    if !hashfile_checksum_valid(data, hash_len) {
        return Err(format!("sha1 file '{path_for_errors}': validation error"));
    }
    let n = index.len();
    let expected_len = HEADER_LEN + n * 4 + hash_len + hash_len;
    if data.len() != expected_len {
        return Err(format!("reverse-index file {path_for_errors} is corrupt"));
    }
    let mut pos = 0usize;
    let sig = read_u32_be(data, &mut pos).ok_or_else(|| "truncated rev-index header".to_owned())?;
    if sig != RIDX_SIGNATURE {
        return Err(format!(
            "reverse-index file {path_for_errors} has unknown signature"
        ));
    }
    let ver = read_u32_be(data, &mut pos).ok_or_else(|| "truncated rev-index header".to_owned())?;
    if ver != RIDX_VERSION {
        return Err(format!(
            "reverse-index file {path_for_errors} has unsupported version {ver}"
        ));
    }
    let hash_id =
        read_u32_be(data, &mut pos).ok_or_else(|| "truncated rev-index header".to_owned())?;
    if hash_id != RIDX_HASH_ID_SHA1 && hash_id != 2 {
        return Err(format!(
            "reverse-index file {path_for_errors} has unsupported hash id {hash_id}"
        ));
    }
    let mut order: Vec<u32> = (0..n as u32).collect();
    order.sort_by_key(|&i| index.offset_at(i as usize));
    for (i, &expected) in order.iter().enumerate() {
        let got =
            read_u32_be(data, &mut pos).ok_or_else(|| "truncated rev-index data".to_owned())?;
        if got != expected {
            return Err(format!(
                "invalid rev-index position at {i}: {got} != {expected}"
            ));
        }
    }
    Ok(())
}

/// Verify on-disk `.rev` against the given pack index.
///
/// Returns `Ok(())` when the file is missing (optional sidecar). On parse or mismatch errors,
/// returns a message suitable for `error: ...` output.
pub fn verify_pack_rev_file(rev_path: &Path, index: &PackIndex) -> std::result::Result<(), String> {
    let data = match std::fs::read(rev_path) {
        Ok(d) => d,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(format!("failed to read {}: {e}", rev_path.display())),
    };
    let p = rev_path.display().to_string();
    verify_pack_rev_file_contents(&data, index, &p)
}

/// If `data` is a well-formed RIDX for `num_objects`, returns index positions in **pack order**
/// (object at pack offset rank `i` → OID at index row `result[i]`). Otherwise `None`.
#[must_use]
pub fn try_rev_positions_in_pack_order(data: &[u8], num_objects: usize) -> Option<Vec<u32>> {
    // The trailing-hash width (SHA-1 vs SHA-256) is implied by the file length.
    let hash_len = [SHA1_TRAILER, SHA256_TRAILER]
        .into_iter()
        .find(|&hl| data.len() == HEADER_LEN + num_objects * 4 + hl + hl)?;
    if !hashfile_checksum_valid(data, hash_len) {
        return None;
    }
    let mut pos = 0usize;
    let sig = read_u32_be(data, &mut pos)?;
    if sig != RIDX_SIGNATURE {
        return None;
    }
    let ver = read_u32_be(data, &mut pos)?;
    if ver != RIDX_VERSION {
        return None;
    }
    let hash_id = read_u32_be(data, &mut pos)?;
    if hash_id != RIDX_HASH_ID_SHA1 && hash_id != RIDX_HASH_ID_SHA256 {
        return None;
    }
    let mut pack_order_idx = vec![0u32; num_objects];
    for slot in &mut pack_order_idx {
        *slot = read_u32_be(data, &mut pos)?;
    }
    if pos != HEADER_LEN + num_objects * 4 {
        return None;
    }
    pos += hash_len;
    if pos != data.len() - hash_len {
        return None;
    }
    if pack_order_idx.len() != num_objects {
        return None;
    }
    let mut seen = vec![false; num_objects];
    for &p in &pack_order_idx {
        let pi = p as usize;
        if pi >= num_objects || seen[pi] {
            return None;
        }
        seen[pi] = true;
    }
    Some(pack_order_idx)
}

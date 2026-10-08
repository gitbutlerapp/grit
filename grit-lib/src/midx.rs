//! Multi-pack-index (MIDX) file writing and minimal reading.
//!
//! Writes a Git-compatible `multi-pack-index` file (version 1, SHA-1) covering
//! selected `pack-*.idx` files. Objects that appear in multiple packs keep the
//! preferred pack's copy when `preferred_pack_idx` is set (matching Git's
//! geometric repack tests).
//!
//! Incremental writes follow Git's split layout: layers live under
//! `pack/multi-pack-index.d/multi-pack-index-<sha1>.midx` with ordering in
//! `multi-pack-index-chain` (oldest hash first, newest last).

use std::collections::{HashMap, HashSet};
use std::fs;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::hash::verify_trailer;

use crate::error::{Error, Result};
use crate::objects::{HashAlgo, ObjectId, ObjectInfo};
use crate::pack::{read_pack_index_no_verify, PackIndex};
use crate::pack_rev::append_hashfile_checksum;

const MIDX_SIGNATURE: u32 = 0x4d49_4458;
const MIDX_VERSION_V1: u8 = 1;
const MIDX_VERSION_V2: u8 = 2;
const HASH_VERSION_SHA1: u8 = 1;
const HASH_VERSION_SHA256: u8 = 2;
const MIDX_HEADER_SIZE: usize = 12;
const CHUNK_TOC_ENTRY_SIZE: usize = 12;
const MIDX_CHUNKID_PACKNAMES: u32 = 0x504e_414d;
const MIDX_CHUNKID_OIDFANOUT: u32 = 0x4f49_4446;
const MIDX_CHUNKID_OIDLOOKUP: u32 = 0x4f49_444c;
const MIDX_CHUNKID_OBJECTOFFSETS: u32 = 0x4f4f_4646;
const MIDX_CHUNKID_LARGEOFFSETS: u32 = 0x4c4f_4646;
const MIDX_CHUNKID_REVINDEX: u32 = 0x5249_4458;
const MIDX_CHUNKID_BITMAPPED_PACKS: u32 = 0x4254_4d50;

// Git `pack-revindex.h` / `pack-write.c` (standalone `.rev` next to MIDX).
const RIDX_SIGNATURE: u32 = 0x5249_4458;
const RIDX_VERSION: u32 = 1;
const RIDX_HEADER_SIZE: usize = 12;
const MIDX_CHUNK_ALIGNMENT: usize = 4;

// `git midx.h` (MIDX_LARGE_OFFSET_NEEDED).
const MIDX_LARGE_OFFSET_NEEDED: u32 = 0x8000_0000;

struct MidxEntry {
    oid: ObjectId,
    pack_id: u32,
    offset: u64,
    pack_mtime: std::time::SystemTime,
}

/// Options for writing a multi-pack index (extension of the simple writer).
#[derive(Debug, Clone, Default)]
pub struct WriteMultiPackIndexOptions {
    /// When set, objects also present in other packs are taken from this pack
    /// (`pack_names` index in the sorted name list).
    pub preferred_pack_idx: Option<u32>,
    /// Basename of the preferred pack (e.g. `pack-abc.idx` or `pack-abc.pack`); resolved against
    /// the working pack name list after optional subset filtering.
    pub preferred_pack_name: Option<String>,
    /// If set, only these `pack-*.idx` basenames are included, in this order (Git `--stdin-packs`).
    pub pack_names_subset_ordered: Option<Vec<String>>,
    /// When true, append RIDX + empty BTMP chunks so `test-tool read-midx --bitmap` succeeds.
    pub write_bitmap_placeholders: bool,
    /// When true, write a new layer in `multi-pack-index.d/` and extend the chain file
    /// instead of replacing `pack/multi-pack-index`.
    pub incremental: bool,
    /// When true with [`Self::write_bitmap_placeholders`], also create an empty `.rev`
    /// sidecar (Git `GIT_TEST_MIDX_WRITE_REV` compatibility).
    pub write_rev_placeholder: bool,
    /// On-disk MIDX format version to write (`1` or `2`). `None` writes the default (v2).
    /// Set from `midx.version`.
    pub version: Option<u8>,
}

fn normalize_pack_idx_basename(raw: &str) -> Result<String> {
    let t = raw.trim();
    let t = std::path::Path::new(t)
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or(t);
    let t = t.strip_prefix("./").unwrap_or(t);
    if t.ends_with(".idx") {
        Ok(t.to_string())
    } else if t.ends_with(".pack") {
        Ok(format!("{}.idx", t.strip_suffix(".pack").unwrap_or(t)))
    } else {
        Ok(format!("{t}.idx"))
    }
}

/// Read a big-endian `u32` from `data` at byte offset `off`.
///
/// Returns [`Error::CorruptObject`] if `data` does not contain 4 bytes at `off`,
/// replacing the previous fixed-width-slice `.try_into().unwrap()` with real
/// bounds handling (the success-path value is unchanged).
fn read_be_u32(data: &[u8], off: usize) -> Result<u32> {
    let end = off.checked_add(4).filter(|&e| e <= data.len());
    let Some(end) = end else {
        return Err(Error::CorruptObject(
            "truncated MIDX data reading u32".to_owned(),
        ));
    };
    let bytes: [u8; 4] = data[off..end]
        .try_into()
        .map_err(|_| Error::CorruptObject("truncated MIDX data reading u32".to_owned()))?;
    Ok(u32::from_be_bytes(bytes))
}

/// Read a big-endian `u64` from `data` at byte offset `off`.
///
/// Returns [`Error::CorruptObject`] if `data` does not contain 8 bytes at `off`,
/// replacing the previous fixed-width-slice `.try_into().unwrap()` with real
/// bounds handling (the success-path value is unchanged).
fn read_be_u64(data: &[u8], off: usize) -> Result<u64> {
    let end = off.checked_add(8).filter(|&e| e <= data.len());
    let Some(end) = end else {
        return Err(Error::CorruptObject(
            "truncated MIDX data reading u64".to_owned(),
        ));
    };
    let bytes: [u8; 8] = data[off..end]
        .try_into()
        .map_err(|_| Error::CorruptObject("truncated MIDX data reading u64".to_owned()))?;
    Ok(u64::from_be_bytes(bytes))
}

struct MidxFileHeader {
    num_chunks: u8,
}

fn parse_midx_header(data: &[u8]) -> Result<(MidxFileHeader, usize, u8)> {
    if data.len() < MIDX_HEADER_SIZE + 20 {
        return Err(Error::CorruptObject("midx file too small".to_owned()));
    }
    let sig = read_be_u32(data, 0)?;
    if sig != MIDX_SIGNATURE {
        return Err(Error::CorruptObject("bad MIDX signature".to_owned()));
    }
    let version = data[4];
    if version != MIDX_VERSION_V1 && version != MIDX_VERSION_V2 {
        return Err(Error::CorruptObject(format!(
            "multi-pack-index version {version} not recognized"
        )));
    }
    let object_hash_bytes = data[5];
    let num_chunks = data[6];
    let _num_packs = read_be_u32(data, 8)?;
    Ok((
        MidxFileHeader { num_chunks },
        MIDX_HEADER_SIZE,
        object_hash_bytes,
    ))
}

fn parse_pack_names_blob(pn: &[u8]) -> Result<Vec<String>> {
    let mut names = Vec::new();
    let mut start = 0usize;
    for (i, &b) in pn.iter().enumerate() {
        if b == 0 && i >= start {
            if i > start {
                let s = std::str::from_utf8(&pn[start..i])
                    .map_err(|_| Error::CorruptObject("non-utf8 pack name in MIDX".to_owned()))?;
                names.push(s.to_string());
            }
            start = i + 1;
        }
    }
    Ok(names)
}

/// Compare a pack basename that may use `.pack` or `.idx` with an MIDX pack name (`.idx`).
fn cmp_idx_or_pack_name(idx_or_pack_name: &str, idx_name: &str) -> std::cmp::Ordering {
    let a = idx_or_pack_name.as_bytes();
    let b = idx_name.as_bytes();
    let mut i = 0usize;
    let min = a.len().min(b.len());
    while i < min && a[i] == b[i] {
        i += 1;
    }
    let suf_a = &a[i..];
    let suf_b = &b[i..];
    if suf_b == b"idx" && suf_a == b"pack" {
        return std::cmp::Ordering::Equal;
    }
    suf_a.cmp(suf_b)
}

fn preferred_pack_index_by_mtime(pack_dir: &Path, names: &[String]) -> Result<Option<usize>> {
    let mut best: Option<(usize, std::time::SystemTime)> = None;
    for (i, n) in names.iter().enumerate() {
        let meta = fs::metadata(pack_dir.join(n)).map_err(Error::Io)?;
        let mtime = meta.modified().map_err(Error::Io)?;
        match best {
            None => best = Some((i, mtime)),
            Some((_, t)) if mtime < t => best = Some((i, mtime)),
            _ => {}
        }
    }
    Ok(best.map(|(i, _)| i))
}

fn midx_d_dir(pack_dir: &Path) -> std::path::PathBuf {
    pack_dir.join("multi-pack-index.d")
}

fn chain_file_path(pack_dir: &Path) -> std::path::PathBuf {
    midx_d_dir(pack_dir).join("multi-pack-index-chain")
}

fn midx_chain_hash_hex_len(pack_dir: &Path) -> usize {
    let Some(objects_dir) = pack_dir.parent() else {
        return HashAlgo::Sha1.hex_len();
    };
    if repo_midx_hash_version_for_objects_dir(objects_dir) == HASH_VERSION_SHA256 {
        HashAlgo::Sha256.hex_len()
    } else {
        HashAlgo::Sha1.hex_len()
    }
}

fn is_valid_midx_chain_hash_line(t: &str, hex_len: usize) -> bool {
    t.len() == hex_len && t.chars().all(|c| c.is_ascii_hexdigit())
}

fn read_chain_layer_hashes(pack_dir: &Path) -> Result<Vec<String>> {
    let hex_len = midx_chain_hash_hex_len(pack_dir);
    let path = chain_file_path(pack_dir);
    let f = fs::File::open(&path).map_err(Error::Io)?;
    let mut out = Vec::new();
    for line in BufReader::new(f).lines() {
        let line = line.map_err(Error::Io)?;
        let t = line.trim();
        if t.is_empty() {
            continue;
        }
        if !is_valid_midx_chain_hash_line(t, hex_len) {
            return Err(Error::CorruptObject(format!(
                "invalid multi-pack-index chain line: {t}"
            )));
        }
        out.push(t.to_ascii_lowercase());
    }
    Ok(out)
}

/// Resolve the path to the newest MIDX layer (root `multi-pack-index` or last chain entry).
/// Return the MIDX hash-version byte expected for the repository owning `pack_dir`,
/// mirroring git's `oid_version(r->hash_algo)` (SHA-1 → 1, SHA-256 → 2).
///
/// `pack_dir` is `<gitdir>/objects/pack`; the object format lives in the gitdir's
/// `config` under `extensions.objectformat`. When the config cannot be read or the
/// extension is absent, the default SHA-1 version (1) is returned.
fn repo_midx_hash_version(pack_dir: &Path) -> u8 {
    // pack_dir = <gitdir>/objects/pack -> gitdir = pack_dir/../..
    let Some(objects_dir) = pack_dir.parent() else {
        return HASH_VERSION_SHA1;
    };
    repo_midx_hash_version_for_objects_dir(objects_dir)
}

// Repository-scoped MIDX read cache: one [`PreparedMidxChain`] per `objects/pack` directory,
// built once (no stat/read/parse on lookup hits). In-process writers call [`evict_pack_dir`].
pub(crate) mod midx_cache {
    use super::{
        midx_chain_layer_paths_newest_first, midx_load_for_read, midx_warn_once,
        repo_midx_hash_version_for_objects_dir, MidxLoadResult, MidxReadView,
        MIDX_LARGE_OFFSET_NEEDED,
    };
    use crate::error::{Error, Result};
    use crate::objects::{ObjectId, ObjectInfo};
    use crate::pack::{read_object_at, read_pack_index_cached, PackIndex};
    use std::collections::HashMap;
    use std::fs;
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{Arc, Mutex, OnceLock};

    /// Parsed MIDX layer: fanout + binary search; pack indexes opened on first read per pack id.
    pub struct PreparedMidxLayer {
        bytes: Arc<[u8]>,
        fanout: [u32; 256],
        oidl_off: usize,
        num_objects: usize,
        hash_len: usize,
        /// Per sorted OID position: `(pack_id, in-pack offset)` decoded at prepare time.
        pack_offsets: Vec<(u32, u64)>,
        pack_dir: PathBuf,
        pack_names: Vec<String>,
        pack_indexes: Vec<Arc<OnceLock<Option<Arc<PackIndex>>>>>,
        packs_validated: AtomicBool,
    }

    impl PreparedMidxLayer {
        fn prepare(bytes: Arc<[u8]>, view: MidxReadView, pack_dir: &Path) -> Self {
            let hash_len = if bytes.get(5) == Some(&2) { 32 } else { 20 };
            let mut fanout = [0u32; 256];
            for (i, slot) in fanout.iter_mut().enumerate() {
                *slot = read_be_u32(&bytes, view.oidf_off + i * 4).unwrap_or(0);
            }
            let mut pack_offsets = Vec::with_capacity(view.num_objects);
            for i in 0..view.num_objects {
                let ob = view.ooff_off + i * 8;
                let pack_id = read_be_u32(&bytes, ob).unwrap_or(0);
                let raw_off = read_be_u32(&bytes, ob + 4).unwrap_or(0);
                let offset = decode_midx_object_offset(&bytes, raw_off, view.loff).unwrap_or(0);
                pack_offsets.push((pack_id, offset));
            }
            Self {
                fanout,
                oidl_off: view.oidl_off,
                num_objects: view.num_objects,
                hash_len,
                pack_offsets,
                pack_dir: pack_dir.to_path_buf(),
                pack_names: view.pack_names.clone(),
                pack_indexes: view
                    .pack_names
                    .iter()
                    .map(|_| Arc::new(OnceLock::new()))
                    .collect(),
                packs_validated: AtomicBool::new(false),
                bytes,
            }
        }

        fn open_pack_index(&self, pack_id: u32) -> Option<Arc<PackIndex>> {
            let i = pack_id as usize;
            let slot = self.pack_indexes.get(i)?;
            slot.get_or_init(|| {
                let idx_path = self.pack_dir.join(self.pack_names.get(i)?);
                if !idx_path.is_file() {
                    return None;
                }
                read_pack_index_cached(&idx_path).ok()
            })
            .clone()
        }

        fn oid_at(&self, pos: usize) -> &[u8] {
            let base = self.oidl_off + pos * self.hash_len;
            &self.bytes[base..base + self.hash_len]
        }

        #[inline(always)]
        fn find_position(&self, oid: &ObjectId) -> Option<usize> {
            let needle = oid.as_bytes();
            if needle.len() != self.hash_len {
                return None;
            }
            let first = needle[0] as usize;
            let lo = if first == 0 {
                0
            } else {
                self.fanout[first - 1] as usize
            };
            let hi = self.fanout[first] as usize;
            if lo >= hi || hi > self.num_objects {
                return None;
            }
            if hi - lo >= 64 {
                if let Some(pos) = self.interpolation_search(lo, hi, needle) {
                    return Some(pos);
                }
            }
            self.binary_search(lo, hi, needle)
        }

        fn binary_search(&self, mut lo: usize, mut hi: usize, needle: &[u8]) -> Option<usize> {
            while lo < hi {
                let mid = lo + (hi - lo) / 2;
                match self.oid_at(mid).cmp(needle) {
                    std::cmp::Ordering::Less => lo = mid + 1,
                    std::cmp::Ordering::Greater => hi = mid,
                    std::cmp::Ordering::Equal => return Some(mid),
                }
            }
            None
        }

        fn interpolation_search(&self, lo: usize, hi: usize, needle: &[u8]) -> Option<usize> {
            use crate::pack_index::oid_interp_prefix_u64;
            let mut lo = lo;
            let mut hi = hi;
            while lo < hi && needle >= self.oid_at(lo) && needle <= self.oid_at(hi - 1) {
                let lo_oid = self.oid_at(lo);
                let hi_oid = self.oid_at(hi - 1);
                if lo_oid == hi_oid {
                    return if lo_oid == needle { Some(lo) } else { None };
                }
                let span = hi - lo - 1;
                let num = oid_interp_prefix_u64(needle);
                let lo_val = oid_interp_prefix_u64(lo_oid);
                let hi_val = oid_interp_prefix_u64(hi_oid);
                let guess = if hi_val <= lo_val {
                    lo + span / 2
                } else {
                    let num = u128::from(num.saturating_sub(lo_val));
                    let den = u128::from(hi_val - lo_val);
                    lo.saturating_add(((num * u128::from(span as u64)) / den) as usize)
                };
                let pos = guess.min(hi - 1);
                match self.oid_at(pos).cmp(needle) {
                    std::cmp::Ordering::Equal => return Some(pos),
                    std::cmp::Ordering::Less => lo = pos + 1,
                    std::cmp::Ordering::Greater => hi = pos,
                }
            }
            None
        }

        pub fn ensure_packs_validated(&self, pack_dir: &Path, pack_names: &[String]) {
            if self.packs_validated.swap(true, Ordering::Relaxed) {
                return;
            }
            for idx_name in pack_names {
                let idx_path = pack_dir.join(idx_name);
                if !idx_path.is_file() {
                    continue;
                }
                if crate::pack::read_pack_index_no_verify(&idx_path).is_err() {
                    let mut pack_path = idx_path.clone();
                    pack_path.set_extension("pack");
                    midx_warn_once(&format!(
                        "error: packfile {} index unavailable",
                        pack_path.display()
                    ));
                }
            }
        }

        /// Fanout bucket + binary search over OID lookup rows; returns in-pack offset.
        #[inline(always)]
        pub fn lookup_pack_and_offset(&self, oid: &ObjectId) -> Option<(u32, u64)> {
            let pos = self.find_position(oid)?;
            Some(self.pack_offsets[pos])
        }

        #[inline]
        pub fn read_at(&self, pack_id: u32, offset: u64) -> Result<Option<crate::objects::Object>> {
            let Some(idx) = self.open_pack_index(pack_id) else {
                return Ok(None);
            };
            match read_object_at(idx.as_ref(), offset) {
                Ok(obj) => Ok(Some(obj)),
                Err(Error::Io(err)) if err.kind() == std::io::ErrorKind::NotFound => Ok(None),
                Err(err) => Err(err),
            }
        }
    }

    /// All incremental MIDX layers for one `objects/pack` directory (newest first).
    pub struct PreparedMidxChain {
        pack_dir: PathBuf,
        pack_names_by_layer: Vec<Vec<String>>,
        layers: Vec<PreparedMidxLayer>,
        packs_validated: AtomicBool,
    }

    impl PreparedMidxChain {
        #[inline(always)]
        pub fn lookup_pack_and_offset(&self, oid: &ObjectId) -> Option<(u32, u64)> {
            for layer in &self.layers {
                if let Some(found) = layer.lookup_pack_and_offset(oid) {
                    return Some(found);
                }
            }
            None
        }

        pub fn oid_listed_in_tip(&self, oid: &ObjectId) -> bool {
            self.lookup_pack_and_offset(oid).is_some()
        }

        pub fn try_read_object(&self, oid: &ObjectId) -> Result<Option<crate::objects::Object>> {
            for (layer, names) in self.layers.iter().zip(self.pack_names_by_layer.iter()) {
                let Some((pack_id, offset)) = layer.lookup_pack_and_offset(oid) else {
                    continue;
                };
                self.ensure_packs_validated();
                match layer.read_at(pack_id, offset)? {
                    Some(obj) => return Ok(Some(obj)),
                    None => {
                        layer.ensure_packs_validated(&self.pack_dir, names);
                        return Ok(None);
                    }
                }
            }
            Ok(None)
        }

        pub fn try_read_info(&self, oid: &ObjectId) -> Result<Option<ObjectInfo>> {
            for (layer, names) in self.layers.iter().zip(self.pack_names_by_layer.iter()) {
                let Some((pack_id, _offset)) = layer.lookup_pack_and_offset(oid) else {
                    continue;
                };
                self.ensure_packs_validated();
                let Some(idx) = layer.open_pack_index(pack_id) else {
                    layer.ensure_packs_validated(&self.pack_dir, names);
                    return Ok(None);
                };
                return crate::pack::read_object_info_from_pack(idx.as_ref(), oid).map(Some);
            }
            Ok(None)
        }

        pub fn ensure_packs_validated(&self) {
            if self.packs_validated.swap(true, Ordering::Relaxed) {
                return;
            }
            for (layer, names) in self.layers.iter().zip(self.pack_names_by_layer.iter()) {
                layer.ensure_packs_validated(&self.pack_dir, names);
            }
        }
    }

    fn read_be_u32(data: &[u8], off: usize) -> Result<u32> {
        if off + 4 > data.len() {
            return Err(Error::CorruptObject("truncated MIDX u32".to_owned()));
        }
        Ok(u32::from_be_bytes([
            data[off],
            data[off + 1],
            data[off + 2],
            data[off + 3],
        ]))
    }

    fn read_be_u64(data: &[u8], off: usize) -> Result<u64> {
        if off + 8 > data.len() {
            return Err(Error::CorruptObject("truncated MIDX u64".to_owned()));
        }
        Ok(u64::from_be_bytes([
            data[off],
            data[off + 1],
            data[off + 2],
            data[off + 3],
            data[off + 4],
            data[off + 5],
            data[off + 6],
            data[off + 7],
        ]))
    }

    fn decode_midx_object_offset(
        data: &[u8],
        raw_off: u32,
        loff: Option<(usize, usize)>,
    ) -> Option<u64> {
        if loff.is_none() {
            return Some(u64::from(raw_off));
        }
        if (raw_off & MIDX_LARGE_OFFSET_NEEDED) != 0 {
            let idx = (raw_off & !MIDX_LARGE_OFFSET_NEEDED) as usize;
            let need = (idx + 1) * 8;
            match loff {
                Some((loff_off, loff_len)) if loff_len >= need => {
                    read_be_u64(data, loff_off + idx * 8).ok()
                }
                _ => super::midx_die(&["multi-pack-index large offset out of bounds"]),
            }
        } else {
            Some(u64::from(raw_off))
        }
    }

    #[cfg(test)]
    pub(super) fn decode_midx_object_offset_for_test(
        data: &[u8],
        raw_off: u32,
        loff: Option<(usize, usize)>,
    ) -> Option<u64> {
        decode_midx_object_offset(data, raw_off, loff)
    }

    #[derive(Default)]
    pub(crate) struct State {
        chains: HashMap<PathBuf, Arc<PreparedMidxChain>>,
        hot_chain: Option<(PathBuf, Arc<PreparedMidxChain>)>,
    }

    fn build_chain(objects_dir: &Path) -> Result<Option<Arc<PreparedMidxChain>>> {
        let pack_dir = objects_dir.join("pack");
        let paths = midx_chain_layer_paths_newest_first(&pack_dir);
        if paths.is_empty() {
            return Ok(None);
        }
        let hash_version = repo_midx_hash_version_for_objects_dir(objects_dir);
        let mut layers = Vec::with_capacity(paths.len());
        let mut pack_names_by_layer = Vec::with_capacity(paths.len());
        for path in paths {
            let bytes: Arc<[u8]> = match fs::read(&path) {
                Ok(b) => Arc::from(b),
                Err(err) if err.kind() == std::io::ErrorKind::NotFound => continue,
                Err(err) => return Err(Error::Io(err)),
            };
            let view = match midx_load_for_read(&bytes, hash_version) {
                MidxLoadResult::Ok(v) => v,
                MidxLoadResult::Skip => continue,
            };
            pack_names_by_layer.push(view.pack_names.clone());
            layers.push(PreparedMidxLayer::prepare(bytes, view, &pack_dir));
        }
        if layers.is_empty() {
            return Ok(None);
        }
        Ok(Some(Arc::new(PreparedMidxChain {
            pack_dir,
            pack_names_by_layer,
            layers,
            packs_validated: AtomicBool::new(false),
        })))
    }

    /// Cached prepared chain for `objects_dir` (thread-local + read-lock on first use per thread).
    pub fn prepared_chain(objects_dir: &Path) -> Result<Option<Arc<PreparedMidxChain>>> {
        if let Some(chain) = crate::pack_store::PackStore::with_current_or_implicit_midx(|state| {
            state
                .hot_chain
                .as_ref()
                .and_then(|(path, chain)| (path == objects_dir).then(|| Arc::clone(chain)))
                .or_else(|| state.chains.get(&objects_dir.join("pack")).map(Arc::clone))
        }) {
            crate::pack_store::PackStore::with_current_or_implicit_midx(|state| {
                state.hot_chain = Some((objects_dir.to_path_buf(), Arc::clone(&chain)));
            });
            return Ok(Some(chain));
        }
        let pack_dir = objects_dir.join("pack");
        let built = build_chain(objects_dir)?;
        let Some(chain) = built else {
            return Ok(None);
        };
        let stored = crate::pack_store::PackStore::with_current_or_implicit_midx(|state| {
            let chain = Arc::clone(
                state
                    .chains
                    .entry(pack_dir)
                    .or_insert_with(|| Arc::clone(&chain)),
            );
            state.hot_chain = Some((objects_dir.to_path_buf(), Arc::clone(&chain)));
            chain
        });
        Ok(Some(stored))
    }

    pub fn evict_pack_dir(pack_dir: &Path) {
        crate::pack_store::PackStore::with_current_or_implicit_midx(|state| {
            evict_pack_dir_in_state(state, pack_dir);
        });
    }

    pub(crate) fn evict_pack_dir_on(state: &Mutex<State>, pack_dir: &Path) {
        let mut state = state.lock().unwrap_or_else(|poison| poison.into_inner());
        evict_pack_dir_in_state(&mut state, pack_dir);
    }

    fn evict_pack_dir_in_state(state: &mut State, pack_dir: &Path) {
        if state.hot_chain.as_ref().is_some_and(|(objects_dir, _)| {
            objects_dir.join("pack") == pack_dir || pack_dir.starts_with(objects_dir)
        }) {
            state.hot_chain = None;
        }
        state.chains.retain(|path, _| !path.starts_with(pack_dir));
    }
}

/// Cached MIDX chain for object reads (see [`midx_cache::prepared_chain`]).
pub(crate) use midx_cache::PreparedMidxChain;

pub(crate) fn prepared_midx_chain(objects_dir: &Path) -> Result<Option<Arc<PreparedMidxChain>>> {
    midx_cache::prepared_chain(objects_dir)
}

/// MIDX layer files to search on object reads, newest chain layer first.
fn midx_chain_layer_paths_newest_first(pack_dir: &Path) -> Vec<PathBuf> {
    let root = pack_dir.join("multi-pack-index");
    if root.is_file() {
        return vec![root];
    }
    let Ok(hashes) = read_chain_layer_hashes(pack_dir) else {
        return Vec::new();
    };
    if hashes.is_empty() {
        return Vec::new();
    }
    let midx_d = midx_d_dir(pack_dir);
    hashes
        .iter()
        .rev()
        .map(|h| midx_d.join(format!("multi-pack-index-{h}.midx")))
        .collect()
}

/// Like [`repo_midx_hash_version`] but starting from the `objects` directory.
fn repo_midx_hash_version_for_objects_dir(objects_dir: &Path) -> u8 {
    let Some(gitdir) = objects_dir.parent() else {
        return HASH_VERSION_SHA1;
    };
    sniff_objectformat_hash_version(&gitdir.join("config"))
}

/// Uncached `[extensions] objectformat` scan of one config file.
fn sniff_objectformat_hash_version(config_path: &Path) -> u8 {
    let Ok(text) = fs::read_to_string(config_path) else {
        return HASH_VERSION_SHA1;
    };
    // Minimal scan for `[extensions]` ... `objectformat = sha256`. Section and key
    // names are case-insensitive in git config; values are case-sensitive but git
    // only accepts the literals "sha1"/"sha256".
    let mut in_extensions = false;
    for raw in text.lines() {
        let line = raw.trim();
        if line.starts_with('[') {
            let section = line.trim_start_matches('[').trim_end_matches(']');
            let name = section.split_whitespace().next().unwrap_or("");
            in_extensions = name.eq_ignore_ascii_case("extensions");
            continue;
        }
        if !in_extensions {
            continue;
        }
        if let Some((key, value)) = line.split_once('=') {
            if key.trim().eq_ignore_ascii_case("objectformat")
                && value.trim().eq_ignore_ascii_case("sha256")
            {
                return HASH_VERSION_SHA256;
            }
        }
    }
    HASH_VERSION_SHA1
}

/// Whether a tip MIDX file exists under `pack_dir` (live filesystem probe).
#[must_use]
pub fn cached_tip_midx_path(pack_dir: &Path) -> Option<std::path::PathBuf> {
    resolve_tip_midx_path(pack_dir)
}

/// Drop cached MIDX bytes and tip resolution after pack-directory changes.
pub fn evict_midx_read_cache_for_pack_dir(pack_dir: &Path) {
    midx_cache::evict_pack_dir(pack_dir);
}

pub fn resolve_tip_midx_path(pack_dir: &Path) -> Option<std::path::PathBuf> {
    let root = pack_dir.join("multi-pack-index");
    if root.exists() {
        return Some(root);
    }
    let hashes = read_chain_layer_hashes(pack_dir).ok()?;
    let last = hashes.last()?;
    Some(midx_d_dir(pack_dir).join(format!("multi-pack-index-{last}.midx")))
}

/// Resolve a specific MIDX layer file by its lowercase hex checksum. Searches the
/// incremental chain (`multi-pack-index.d/multi-pack-index-<hash>.midx`) and the
/// single-file root MIDX. Returns `None` when no layer matches that checksum.
pub fn resolve_midx_layer_path(pack_dir: &Path, checksum: &str) -> Option<std::path::PathBuf> {
    let checksum = checksum.to_ascii_lowercase();
    if let Ok(hashes) = read_chain_layer_hashes(pack_dir) {
        if hashes.contains(&checksum) {
            return Some(midx_d_dir(pack_dir).join(format!("multi-pack-index-{checksum}.midx")));
        }
    }
    let root = pack_dir.join("multi-pack-index");
    if root.exists() {
        if let Ok(hex) = midx_checksum_hex_from_path(&root) {
            if hex == checksum {
                return Some(root);
            }
        }
    }
    None
}

fn load_midx_file(path: &Path) -> Result<Vec<u8>> {
    let data = fs::read(path).map_err(Error::Io)?;
    let _ = parse_midx_header(&data)?;
    Ok(data)
}

/// OID width implied by a MIDX file's header hash-version byte (`data[5]`):
/// 2 → SHA-256 (32 bytes), anything else → SHA-1 (20 bytes).
fn midx_hash_len(data: &[u8]) -> usize {
    if data.len() > 5 && data[5] == 2 {
        32
    } else {
        20
    }
}

fn oids_and_packs_from_midx_data(data: &[u8]) -> Result<(HashSet<ObjectId>, Vec<String>)> {
    let hash_len = midx_hash_len(data);
    let (_, hdr_end, _) = parse_midx_header(data)?;
    let (pn_off, pn_len) = find_chunk(data, hdr_end, MIDX_CHUNKID_PACKNAMES)?;
    let pack_names = parse_pack_names_blob(&data[pn_off..pn_off + pn_len])?;
    let (_ooff_off, ooff_len) = find_chunk(data, hdr_end, MIDX_CHUNKID_OBJECTOFFSETS)?;
    let (oidl_off, oidl_len) = find_chunk(data, hdr_end, MIDX_CHUNKID_OIDLOOKUP)?;
    let num_objects = ooff_len / 8;
    if oidl_len != num_objects * hash_len {
        return Err(Error::CorruptObject(
            "MIDX oid-lookup size mismatch".to_owned(),
        ));
    }
    let mut oids = HashSet::with_capacity(num_objects);
    for i in 0..num_objects {
        let start = oidl_off + i * hash_len;
        let oid = ObjectId::from_bytes(&data[start..start + hash_len])?;
        oids.insert(oid);
    }
    Ok((oids, pack_names))
}

fn collect_incremental_base(pack_dir: &Path) -> Result<(HashSet<ObjectId>, HashSet<String>)> {
    let mut oids = HashSet::new();
    let mut packs = HashSet::new();
    let root = pack_dir.join("multi-pack-index");
    let chain_path = chain_file_path(pack_dir);
    if chain_path.exists() {
        for h in read_chain_layer_hashes(pack_dir)? {
            let p = midx_d_dir(pack_dir).join(format!("multi-pack-index-{h}.midx"));
            let data = load_midx_file(&p)?;
            let (layer_oids, names) = oids_and_packs_from_midx_data(&data)?;
            oids.extend(layer_oids);
            for n in names {
                packs.insert(n);
            }
        }
        return Ok((oids, packs));
    }
    if root.exists() {
        let data = load_midx_file(&root)?;
        let (o, names) = oids_and_packs_from_midx_data(&data)?;
        oids = o;
        for n in names {
            packs.insert(n);
        }
    }
    Ok((oids, packs))
}

fn midx_trailing_hash_len(data: &[u8]) -> Result<usize> {
    if data.len() < MIDX_HEADER_SIZE {
        return Err(Error::CorruptObject(
            "midx file too small for header".to_owned(),
        ));
    }
    HashAlgo::try_from(data[5]).map(|a| a.len()).map_err(|e| {
        Error::CorruptObject(format!(
            "multi-pack-index hash version {} not recognized",
            e.0
        ))
    })
}

fn midx_checksum_hex_from_path(path: &Path) -> Result<String> {
    let data = fs::read(path).map_err(Error::Io)?;
    let hash_len = midx_trailing_hash_len(&data)?;
    if data.len() < hash_len {
        return Err(Error::CorruptObject(
            "midx too small for checksum".to_owned(),
        ));
    }
    let hash = &data[data.len() - hash_len..];
    Ok(hex::encode(hash))
}

fn hard_link_or_copy(src: &Path, dst: &Path) -> Result<()> {
    let _ = fs::remove_file(dst);
    if fs::hard_link(src, dst).is_ok() {
        return Ok(());
    }
    fs::copy(src, dst).map_err(Error::Io)?;
    Ok(())
}

fn link_root_midx_into_chain(pack_dir: &Path, root_checksum_hex: &str) -> Result<()> {
    let midx_d = midx_d_dir(pack_dir);
    fs::create_dir_all(&midx_d).map_err(Error::Io)?;
    let dst_midx = midx_d.join(format!("multi-pack-index-{root_checksum_hex}.midx"));
    hard_link_or_copy(&pack_dir.join("multi-pack-index"), &dst_midx)?;
    let exts = ["bitmap", "rev"];
    for ext in exts {
        let src = pack_dir.join(format!("multi-pack-index-{root_checksum_hex}.{ext}"));
        if src.exists() {
            let dst = midx_d.join(format!("multi-pack-index-{root_checksum_hex}.{ext}"));
            hard_link_or_copy(&src, &dst)?;
        }
    }
    Ok(())
}

fn clear_stale_split_layers(pack_dir: &Path, keep: &[String]) -> Result<()> {
    let midx_d = midx_d_dir(pack_dir);
    if !midx_d.exists() {
        return Ok(());
    }
    let keep: HashSet<&str> = keep.iter().map(|s| s.as_str()).collect();
    for ent in fs::read_dir(&midx_d).map_err(Error::Io)? {
        let ent = ent.map_err(Error::Io)?;
        let name = ent.file_name().to_string_lossy().to_string();
        let Some(rest) = name.strip_prefix("multi-pack-index-") else {
            continue;
        };
        let Some((hash_part, _ext)) = rest.split_once('.') else {
            continue;
        };
        if hash_part.len() == 40 && !keep.contains(hash_part) {
            let _ = fs::remove_file(ent.path());
        }
    }
    Ok(())
}

/// Remove every incremental MIDX layer file (`multi-pack-index-<hash>.midx`,
/// `.bitmap`, `.rev`) from `multi-pack-index.d/` and unlink the chain file, but
/// leave the (now empty) directory in place.
///
/// This mirrors git's `clear_incremental_midx_files_ext` plus the chain unlink in
/// `clear_midx_files` for a non-incremental write: git iterates the directory and
/// `unlink`s the matching files individually and never `rmdir`s the directory, so
/// a single-file MIDX write leaves an empty `multi-pack-index.d/` behind rather
/// than removing it (see t5334 "convert incremental to non-incremental").
fn clear_incremental_midx_files(pack_dir: &Path) -> Result<()> {
    let midx_d = midx_d_dir(pack_dir);
    // Unlink the chain file regardless of whether other entries remain.
    let _ = fs::remove_file(chain_file_path(pack_dir));
    if !midx_d.exists() {
        return Ok(());
    }
    for ent in fs::read_dir(&midx_d).map_err(Error::Io)? {
        let ent = ent.map_err(Error::Io)?;
        let name = ent.file_name().to_string_lossy().to_string();
        if name.starts_with("multi-pack-index-")
            && (name.ends_with(".midx") || name.ends_with(".bitmap") || name.ends_with(".rev"))
        {
            let _ = fs::remove_file(ent.path());
        }
    }
    Ok(())
}

fn pack_mtime_for_midx(idx: &PackIndex) -> std::time::SystemTime {
    fs::metadata(&idx.pack_path)
        .and_then(|m| m.modified())
        .unwrap_or(std::time::SystemTime::UNIX_EPOCH)
}

fn midx_pick_better_entry(
    cur: &MidxEntry,
    cand_pack: u32,
    cand_offset: u64,
    cand_mtime: std::time::SystemTime,
    preferred_pack: Option<u32>,
) -> bool {
    let cur_pref = preferred_pack == Some(cur.pack_id);
    let new_pref = preferred_pack == Some(cand_pack);
    if new_pref && !cur_pref {
        return true;
    }
    if cur_pref && !new_pref {
        return false;
    }
    match cand_mtime.cmp(&cur.pack_mtime) {
        std::cmp::Ordering::Greater => true,
        std::cmp::Ordering::Less => false,
        std::cmp::Ordering::Equal => {
            if cand_pack != cur.pack_id {
                cand_pack < cur.pack_id
            } else {
                cand_offset < cur.offset
            }
        }
    }
}

/// Build a MIDX layer's bytes, omitting objects whose OID is present in
/// `exclude_oids` (the base chain for incremental layers and compaction, where
/// objects already provided by a lower layer must not be repeated). Pass `None`
/// for a full (non-incremental) MIDX.
#[allow(clippy::too_many_arguments)]
fn build_midx_bytes_filtered(
    idx_names: &[String],
    indexes: &[PackIndex],
    preferred_idx: Option<usize>,
    write_bitmap_placeholders: bool,
    omit_embedded_ridx_chunk: bool,
    version: u8,
    hash_version: u8,
    exclude_oids: Option<&HashSet<ObjectId>>,
) -> Result<(Vec<u8>, Option<Vec<u32>>)> {
    // OID width implied by the MIDX hash version (1 → SHA-1/20, 2 → SHA-256/32).
    let hash_len = if hash_version == 2 { 32 } else { 20 };
    let preferred_pack_idx = preferred_idx.map(|p| p as u32);
    let pack_mtimes: Vec<std::time::SystemTime> = indexes.iter().map(pack_mtime_for_midx).collect();

    let mut best: HashMap<ObjectId, MidxEntry> = HashMap::new();
    for (pack_id, idx) in indexes.iter().enumerate() {
        let pack_id = u32::try_from(pack_id).map_err(|_| {
            Error::CorruptObject("too many pack files for multi-pack-index".to_owned())
        })?;
        let mtime = pack_mtimes[pack_id as usize];
        for e in idx.iter() {
            if e.oid().len() != hash_len {
                continue;
            }
            let Ok(oid) = ObjectId::from_bytes(e.oid()) else {
                continue;
            };
            if let Some(ex) = exclude_oids {
                if ex.contains(&oid) {
                    continue;
                }
            }
            let cand = MidxEntry {
                oid,
                pack_id,
                offset: e.offset(),
                pack_mtime: mtime,
            };
            match best.get(&oid) {
                None => {
                    best.insert(oid, cand);
                }
                Some(cur) => {
                    if midx_pick_better_entry(cur, pack_id, e.offset(), mtime, preferred_pack_idx) {
                        best.insert(oid, cand);
                    }
                }
            }
        }
    }

    let mut entries: Vec<MidxEntry> = best.into_values().collect();
    entries.sort_by_key(|a| a.oid);

    // Decide how object offsets are encoded, mirroring git/midx-write.c.
    // `large_offsets_needed` becomes true only when some offset cannot fit in a
    // 32-bit field (> 0xffffffff); in that mode every offset that does not fit in
    // 31 bits (> 0x7fffffff) is stored in the 64-bit large-offset (LOFF) chunk and
    // its 32-bit slot is `MIDX_LARGE_OFFSET_NEEDED | slot`. When no offset exceeds
    // 32 bits, offsets in [2^31, 2^32) are written directly as raw 32-bit values
    // and no LOFF chunk is emitted.
    let large_offsets_needed = entries.iter().any(|e| e.offset > u64::from(u32::MAX));

    let num_packs = indexes.len() as u32;

    let mut pack_names_blob = Vec::new();
    for name in idx_names {
        pack_names_blob.extend_from_slice(name.as_bytes());
        pack_names_blob.push(0);
    }
    let pad = (MIDX_CHUNK_ALIGNMENT - (pack_names_blob.len() % MIDX_CHUNK_ALIGNMENT))
        % MIDX_CHUNK_ALIGNMENT;
    pack_names_blob.extend(std::iter::repeat_n(0u8, pad));
    let chunk_pnam = pack_names_blob;

    let mut chunk_oidf = vec![0u8; 256 * 4];
    let mut j = 0usize;
    for i in 0..256 {
        while j < entries.len() && entries[j].oid.as_bytes()[0] <= i as u8 {
            j += 1;
        }
        chunk_oidf[i * 4..(i + 1) * 4].copy_from_slice(&(j as u32).to_be_bytes());
    }

    let mut chunk_oidl = Vec::with_capacity(entries.len() * 20);
    for e in &entries {
        chunk_oidl.extend_from_slice(e.oid.as_bytes());
    }

    let mut large_offsets: Vec<u64> = Vec::new();
    let mut chunk_ooff = Vec::with_capacity(entries.len() * 8);
    for e in &entries {
        chunk_ooff.extend_from_slice(&e.pack_id.to_be_bytes());
        let encoded = if large_offsets_needed && e.offset >> 31 != 0 {
            let slot = u32::try_from(large_offsets.len()).map_err(|_| {
                Error::CorruptObject("too many large offsets in multi-pack-index".to_owned())
            })?;
            large_offsets.push(e.offset);
            MIDX_LARGE_OFFSET_NEEDED | slot
        } else {
            // When large offsets are not needed, an offset in [2^31, 2^32) is
            // written verbatim (truncation via `as u32` is exact here because the
            // value fits in 32 bits).
            e.offset as u32
        };
        chunk_ooff.extend_from_slice(&encoded.to_be_bytes());
    }

    let chunk_loff: Vec<u8> = if large_offsets.is_empty() {
        Vec::new()
    } else {
        let mut v = Vec::with_capacity(large_offsets.len() * 8);
        for off in &large_offsets {
            v.extend_from_slice(&off.to_be_bytes());
        }
        v
    };

    let pref = preferred_pack_idx;
    let mut order: Vec<u32> = (0..entries.len() as u32).collect();
    order.sort_by(|&ai, &bi| {
        let a = &entries[ai as usize];
        let b = &entries[bi as usize];
        let a_pref = pref == Some(a.pack_id);
        let b_pref = pref == Some(b.pack_id);
        b_pref
            .cmp(&a_pref)
            .then_with(|| a.pack_id.cmp(&b.pack_id))
            .then_with(|| a.offset.cmp(&b.offset))
            .then_with(|| ai.cmp(&bi))
    });

    let mut chunk_ridx = Vec::with_capacity(entries.len() * 4);
    for oid_idx in &order {
        chunk_ridx.extend_from_slice(&oid_idx.to_be_bytes());
    }

    // BTMP: per-pack (bitmap_pos, bitmap_nr) in the pseudo-bitmap namespace, matching Git's
    // `write_midx_bitmapped_packs` (cumulative start + object count per pack).
    let rev_sidecar_order = if omit_embedded_ridx_chunk && write_bitmap_placeholders {
        Some(order.clone())
    } else {
        None
    };
    let chunk_btmp: Vec<u8> = if write_bitmap_placeholders {
        // Per-pack `(bitmap_pos, bitmap_nr)`: position of the pack's first object in
        // the MIDX pack-order traversal and the number of (deduplicated) MIDX objects
        // selected from that pack — matching `write_midx_bitmapped_packs` in
        // git/midx-write.c (counts MIDX entries per pack, not raw idx entry counts).
        let num_packs_usize = indexes.len();
        let mut bitmap_pos = vec![u32::MAX; num_packs_usize];
        let mut bitmap_nr = vec![0u32; num_packs_usize];
        for (rank, &oid_idx) in order.iter().enumerate() {
            let pack = entries[oid_idx as usize].pack_id as usize;
            if let Some(p) = bitmap_pos.get_mut(pack) {
                if *p == u32::MAX {
                    *p = rank as u32;
                }
            }
            if let Some(n) = bitmap_nr.get_mut(pack) {
                *n += 1;
            }
        }
        let mut v = Vec::new();
        for pack in 0..num_packs_usize {
            let pos = if bitmap_pos[pack] == u32::MAX {
                0
            } else {
                bitmap_pos[pack]
            };
            v.extend_from_slice(&pos.to_be_bytes());
            v.extend_from_slice(&bitmap_nr[pack].to_be_bytes());
        }
        let pad = (MIDX_CHUNK_ALIGNMENT - (v.len() % MIDX_CHUNK_ALIGNMENT)) % MIDX_CHUNK_ALIGNMENT;
        v.extend(std::iter::repeat_n(0u8, pad));
        v
    } else {
        Vec::new()
    };

    let mut chunks: Vec<(u32, Vec<u8>)> = vec![
        (MIDX_CHUNKID_PACKNAMES, chunk_pnam),
        (MIDX_CHUNKID_OIDFANOUT, chunk_oidf),
        (MIDX_CHUNKID_OIDLOOKUP, chunk_oidl),
        (MIDX_CHUNKID_OBJECTOFFSETS, chunk_ooff),
    ];
    if !chunk_loff.is_empty() {
        chunks.push((MIDX_CHUNKID_LARGEOFFSETS, chunk_loff));
    }
    if (pref.is_some() || write_bitmap_placeholders) && !omit_embedded_ridx_chunk {
        chunks.push((MIDX_CHUNKID_REVINDEX, chunk_ridx));
    }
    if write_bitmap_placeholders {
        chunks.push((MIDX_CHUNKID_BITMAPPED_PACKS, chunk_btmp));
    }

    let num_chunks: u8 = chunks
        .len()
        .try_into()
        .map_err(|_| Error::CorruptObject("too many MIDX chunks".to_owned()))?;

    let mut body = Vec::new();
    let mut cur_offset =
        MIDX_HEADER_SIZE as u64 + ((chunks.len() + 1) * CHUNK_TOC_ENTRY_SIZE) as u64;

    for (id, data) in &chunks {
        body.extend_from_slice(&id.to_be_bytes());
        body.extend_from_slice(&cur_offset.to_be_bytes());
        cur_offset += data.len() as u64;
    }
    body.extend_from_slice(&0u32.to_be_bytes());
    body.extend_from_slice(&cur_offset.to_be_bytes());

    for (_, data) in &chunks {
        body.extend_from_slice(data);
    }

    let mut out = Vec::with_capacity(MIDX_HEADER_SIZE + body.len() + 20);
    out.extend_from_slice(&MIDX_SIGNATURE.to_be_bytes());
    out.push(if version == MIDX_VERSION_V1 {
        MIDX_VERSION_V1
    } else {
        MIDX_VERSION_V2
    });
    out.push(hash_version);
    out.push(num_chunks);
    out.push(0);
    out.extend_from_slice(&num_packs.to_be_bytes());
    out.extend_from_slice(&body);

    // Trailing checksum matches the MIDX hash version (SHA-1 for 1, SHA-256 for 2).
    let algo = if hash_version == 2 {
        HashAlgo::Sha256
    } else {
        HashAlgo::Sha1
    };
    out.extend_from_slice(algo.digest(&out).as_bytes());

    Ok((out, rev_sidecar_order))
}

/// Standalone MIDX `.rev` file (Git `write_rev_file_order` / `RIDX_SIGNATURE`).
///
/// `midx_file_hash` is the MIDX's own trailing checksum (20 bytes for SHA-1, 32
/// for SHA-256); its width selects the RIDX hash-id (1 or 2).
fn write_midx_rev_sidecar(
    path: &Path,
    pack_order: &[u32],
    midx_file_hash: &[u8],
    algo: HashAlgo,
) -> Result<()> {
    let hash_len = algo.len();
    let hash_id = u32::from(u8::from(algo));
    let body_len = RIDX_HEADER_SIZE + pack_order.len() * 4 + hash_len;
    let mut out = Vec::with_capacity(body_len + hash_len);
    out.extend_from_slice(&RIDX_SIGNATURE.to_be_bytes());
    out.extend_from_slice(&RIDX_VERSION.to_be_bytes());
    out.extend_from_slice(&hash_id.to_be_bytes());
    for idx in pack_order {
        out.extend_from_slice(&idx.to_be_bytes());
    }
    if midx_file_hash.len() >= hash_len {
        out.extend_from_slice(&midx_file_hash[..hash_len]);
    } else {
        return Err(Error::CorruptObject(format!(
            "MIDX checksum length {} does not match hash algorithm width {hash_len}",
            midx_file_hash.len()
        )));
    }
    append_hashfile_checksum(&mut out, hash_len);
    fs::write(path, out).map_err(Error::Io)
}

fn repo_hash_algo_for_pack_dir(pack_dir: &Path) -> HashAlgo {
    pack_dir
        .parent()
        .map(crate::odb::hash_algo_for_objects_dir)
        .unwrap_or(HashAlgo::Sha1)
}

fn find_chunk(data: &[u8], header_end: usize, chunk_id: u32) -> Result<(usize, usize)> {
    let (hdr, _, _) = parse_midx_header(data)?;
    let n = hdr.num_chunks as usize;
    let pos = header_end;
    let toc_end = pos + (n + 1) * CHUNK_TOC_ENTRY_SIZE;
    if data.len() < toc_end + 20 {
        return Err(Error::CorruptObject(
            "truncated MIDX chunk table".to_owned(),
        ));
    }
    for i in 0..n {
        let base = pos + i * CHUNK_TOC_ENTRY_SIZE;
        let id = read_be_u32(data, base)?;
        let off = read_be_u64(data, base + 4)? as usize;
        if id == chunk_id {
            let next_off = if i + 1 < n {
                let nb = pos + (i + 1) * CHUNK_TOC_ENTRY_SIZE;
                read_be_u64(data, nb + 4)? as usize
            } else {
                let term = pos + n * CHUNK_TOC_ENTRY_SIZE;
                read_be_u64(data, term + 4)? as usize
            };
            return Ok((off, next_off.saturating_sub(off)));
        }
    }
    Err(Error::CorruptObject(format!(
        "MIDX chunk {chunk_id:08x} not found"
    )))
}

/// A fatal MIDX parse failure (Git `die()` in `load_multi_pack_index`). The
/// contained message is the exact text Git prints, without the `error:`/`fatal:`
/// prefix.
#[derive(Debug, Clone)]
pub struct MidxLoadError(pub String);

impl std::fmt::Display for MidxLoadError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// Parsed table-of-contents entry: `(chunk_id, file_offset)`.
struct TocEntry {
    id: u32,
    offset: usize,
}

/// Walk the MIDX chunk table of contents, mirroring `read_table_of_contents`
/// in `git/chunk-format.c`. Returns the chunk list plus any reported errors,
/// or a fatal `MidxLoadError` for the conditions Git treats as `die()`-worthy.
fn parse_midx_toc(
    data: &[u8],
    hash_len: usize,
    errors: &mut Vec<String>,
) -> std::result::Result<Vec<TocEntry>, MidxLoadError> {
    if data.len() < MIDX_HEADER_SIZE + hash_len {
        return Err(MidxLoadError("multi-pack-index file too small".to_owned()));
    }
    let num_chunks = data[6] as usize;
    let toc_off = MIDX_HEADER_SIZE;
    let needed = toc_off + (num_chunks + 1) * CHUNK_TOC_ENTRY_SIZE;
    if data.len() < needed {
        return Err(MidxLoadError(
            "multi-pack-index chunk table is truncated".to_owned(),
        ));
    }
    let file_size = data.len();
    let mut chunks: Vec<TocEntry> = Vec::with_capacity(num_chunks);

    let read_be64 = |off: usize| -> u64 {
        let mut b = [0u8; 8];
        b.copy_from_slice(&data[off..off + 8]);
        u64::from_be_bytes(b)
    };
    let read_be32 = |off: usize| -> u32 {
        let mut b = [0u8; 4];
        b.copy_from_slice(&data[off..off + 4]);
        u32::from_be_bytes(b)
    };

    for i in 0..num_chunks {
        let entry = toc_off + i * CHUNK_TOC_ENTRY_SIZE;
        let chunk_id = read_be32(entry);
        let chunk_offset = read_be64(entry + 4);

        if chunk_id == 0 {
            errors.push("terminating chunk id appears earlier than expected".to_owned());
            return Err(MidxLoadError(
                "multi-pack-index required pack-name chunk missing or corrupted".to_owned(),
            ));
        }
        if !(chunk_offset as usize).is_multiple_of(MIDX_CHUNK_ALIGNMENT) {
            errors.push(format!(
                "chunk id {chunk_id:x} not {MIDX_CHUNK_ALIGNMENT}-byte aligned"
            ));
            return Err(MidxLoadError(
                "multi-pack-index required pack-name chunk missing or corrupted".to_owned(),
            ));
        }

        let next_entry = toc_off + (i + 1) * CHUNK_TOC_ENTRY_SIZE;
        let next_chunk_offset = read_be64(next_entry + 4);

        if next_chunk_offset < chunk_offset
            || next_chunk_offset > (file_size as u64).saturating_sub(hash_len as u64)
        {
            errors.push(format!(
                "improper chunk offset(s) {chunk_offset:x} and {next_chunk_offset:x}"
            ));
            return Err(MidxLoadError(
                "multi-pack-index required pack-name chunk missing or corrupted".to_owned(),
            ));
        }

        if chunks.iter().any(|c| c.id == chunk_id) {
            errors.push(format!("duplicate chunk ID {chunk_id:x} found"));
            return Err(MidxLoadError(
                "multi-pack-index required pack-name chunk missing or corrupted".to_owned(),
            ));
        }

        chunks.push(TocEntry {
            id: chunk_id,
            offset: chunk_offset as usize,
        });
    }

    // Terminating TOC entry must have chunk id 0.
    let term_entry = toc_off + num_chunks * CHUNK_TOC_ENTRY_SIZE;
    let final_id = read_be32(term_entry);
    if final_id != 0 {
        errors.push(format!("final chunk has non-zero id {final_id:x}"));
        return Err(MidxLoadError(
            "multi-pack-index required pack-name chunk missing or corrupted".to_owned(),
        ));
    }

    // Record the terminator offset as a sentinel (id 0) so the final real chunk's
    // length is taken from the table — not a hash-width-dependent file-size guess.
    let term_offset = read_be64(term_entry + 4) as usize;
    chunks.push(TocEntry {
        id: 0,
        offset: term_offset,
    });

    Ok(chunks)
}

/// Look up `(start, len)` of a chunk in a parsed TOC.
fn toc_chunk_range(chunks: &[TocEntry], data_len: usize, id: u32) -> Option<(usize, usize)> {
    for (i, c) in chunks.iter().enumerate() {
        if c.id == id {
            let next = if i + 1 < chunks.len() {
                chunks[i + 1].offset
            } else {
                data_len.saturating_sub(20)
            };
            return Some((c.offset, next.saturating_sub(c.offset)));
        }
    }
    None
}

/// Full multi-pack-index verification, mirroring `verify_midx_file` in `git/midx.c`
/// plus the `die()`/`error()` conditions in `load_multi_pack_index`. On any problem
/// returns the list of error lines (without `error:`/`fatal:` prefixes) in the order
/// Git emits them; an empty list means the MIDX is valid.
///
/// `objects_dir` is the object database (e.g. `.git/objects`).
pub fn verify_midx(objects_dir: &Path) -> std::result::Result<(), Vec<String>> {
    let pack_dir = objects_dir.join("pack");
    let path = match resolve_tip_midx_path(&pack_dir) {
        Some(p) => p,
        None => return Ok(()),
    };
    let data = match fs::read(&path) {
        Ok(d) => d,
        Err(_) => return Ok(()),
    };

    let mut fatal: Vec<String> = Vec::new();
    let mut errors: Vec<String> = Vec::new();

    // --- header checks (load_multi_pack_index) ---
    if data.len() < MIDX_HEADER_SIZE + 20 {
        return Err(vec!["multi-pack-index file is too small".to_owned()]);
    }
    let sig = u32::from_be_bytes([data[0], data[1], data[2], data[3]]);
    if sig != MIDX_SIGNATURE {
        return Err(vec![format!(
            "multi-pack-index signature 0x{sig:08x} does not match signature 0x{MIDX_SIGNATURE:08x}"
        )]);
    }
    let version = data[4];
    if version != MIDX_VERSION_V1 && version != MIDX_VERSION_V2 {
        return Err(vec![format!(
            "multi-pack-index version {version} not recognized"
        )]);
    }
    let hash_version = data[5];
    let expected_hash_version = repo_midx_hash_version_for_objects_dir(objects_dir);
    if hash_version != expected_hash_version {
        return Err(vec![format!(
            "multi-pack-index hash version {hash_version} does not match version {expected_hash_version}"
        )]);
    }
    let hash_len = if hash_version == 2 { 32usize } else { 20usize };
    let num_packs = u32::from_be_bytes([data[8], data[9], data[10], data[11]]) as usize;

    // --- table of contents ---
    let chunks = match parse_midx_toc(&data, hash_len, &mut errors) {
        Ok(c) => c,
        Err(e) => {
            errors.push(e.0);
            return Err(errors);
        }
    };

    // required pack-names chunk
    let Some((pn_off, pn_len)) = toc_chunk_range(&chunks, data.len(), MIDX_CHUNKID_PACKNAMES)
    else {
        errors.push("multi-pack-index required pack-name chunk missing or corrupted".to_owned());
        return Err(errors);
    };

    // oid-fanout chunk + ordering check
    let Some((fan_off, fan_len)) = toc_chunk_range(&chunks, data.len(), MIDX_CHUNKID_OIDFANOUT)
    else {
        errors.push("multi-pack-index required OID fanout chunk missing or corrupted".to_owned());
        return Err(errors);
    };
    if fan_len != 256 * 4 {
        errors.push("multi-pack-index OID fanout is of the wrong size".to_owned());
        errors.push("multi-pack-index required OID fanout chunk missing or corrupted".to_owned());
        return Err(errors);
    }
    let fanout = |i: usize| -> u32 {
        let b = fan_off + i * 4;
        u32::from_be_bytes([data[b], data[b + 1], data[b + 2], data[b + 3]])
    };
    for i in 0..255 {
        let f1 = fanout(i);
        let f2 = fanout(i + 1);
        if f1 > f2 {
            errors.push(format!(
                "oid fanout out of order: fanout[{i}] = {f1:x} > {f2:x} = fanout[{}]",
                i + 1
            ));
            errors
                .push("multi-pack-index required OID fanout chunk missing or corrupted".to_owned());
            return Err(errors);
        }
    }
    let num_objects = fanout(255) as usize;

    // oid-lookup chunk (size depends on num_objects)
    let Some((oidl_off, oidl_len)) = toc_chunk_range(&chunks, data.len(), MIDX_CHUNKID_OIDLOOKUP)
    else {
        errors.push("multi-pack-index required OID lookup chunk missing or corrupted".to_owned());
        return Err(errors);
    };
    if oidl_len != hash_len * num_objects {
        errors.push("multi-pack-index OID lookup chunk is the wrong size".to_owned());
        errors.push("multi-pack-index required OID lookup chunk missing or corrupted".to_owned());
        return Err(errors);
    }

    // object-offsets chunk
    let Some((ooff_off, ooff_len)) =
        toc_chunk_range(&chunks, data.len(), MIDX_CHUNKID_OBJECTOFFSETS)
    else {
        errors
            .push("multi-pack-index required object offsets chunk missing or corrupted".to_owned());
        return Err(errors);
    };
    if ooff_len != num_objects * 8 {
        errors.push("multi-pack-index object offset chunk is the wrong size".to_owned());
        errors
            .push("multi-pack-index required object offsets chunk missing or corrupted".to_owned());
        return Err(errors);
    }

    let large_off = toc_chunk_range(&chunks, data.len(), MIDX_CHUNKID_LARGEOFFSETS);

    // pack names: parse and (for V1) verify ordering.
    let names = match parse_pack_names_blob(&data[pn_off..pn_off + pn_len]) {
        Ok(n) => n,
        Err(_) => {
            errors.push("multi-pack-index pack-name chunk is too short".to_owned());
            return Err(errors);
        }
    };
    if version == MIDX_VERSION_V1 {
        for i in 1..names.len() {
            if names[i] <= names[i - 1] {
                fatal.push(format!(
                    "multi-pack-index pack names out of order: '{}' before '{}'",
                    names[i - 1],
                    names[i]
                ));
                // Git die()s here while loading; surface immediately.
                errors.extend(fatal);
                return Err(errors);
            }
        }
    }

    // --- checksum ---
    if !midx_checksum_is_valid(&data) {
        errors.push("incorrect checksum".to_owned());
    }

    // --- load each referenced pack (failed to load pack) ---
    let mut pack_indexes: Vec<Option<PackIndex>> = Vec::with_capacity(num_packs);
    for i in 0..num_packs {
        // Load the pack idx without verifying its trailing checksum: `git
        // multi-pack-index verify` uses `open_pack_index`, which only parses the
        // index header/tables. The 64-bit-offset tests deliberately corrupt a
        // pack `.idx` (invalidating its checksum) and still expect the MIDX
        // verify to read recorded offsets out of that idx for comparison.
        let loaded = match names.get(i) {
            Some(name) => read_pack_index_no_verify(&pack_dir.join(name)).ok(),
            None => None,
        };
        if loaded.is_none() {
            errors.push(format!("failed to load pack in position {i}"));
        }
        pack_indexes.push(loaded);
    }

    if num_objects == 0 {
        errors.push("the midx contains no oid".to_owned());
        if errors.is_empty() {
            return Ok(());
        }
        return Err(errors);
    }

    // --- OID lookup order ---
    let oid_at =
        |i: usize| -> &[u8] { &data[oidl_off + i * hash_len..oidl_off + (i + 1) * hash_len] };
    for i in 0..num_objects.saturating_sub(1) {
        let a = oid_at(i);
        let b = oid_at(i + 1);
        if a >= b {
            errors.push(format!(
                "oid lookup out of order: oid[{i}] = {} >= {} = oid[{}]",
                hex::encode(a),
                hex::encode(b),
                i + 1
            ));
        }
    }

    // --- object offsets vs pack index ---
    for i in 0..num_objects {
        let ob = ooff_off + i * 8;
        let pack_int_id = u32::from_be_bytes([data[ob], data[ob + 1], data[ob + 2], data[ob + 3]]);
        let off_raw = u32::from_be_bytes([data[ob + 4], data[ob + 5], data[ob + 6], data[ob + 7]]);
        let oid_hex = hex::encode(oid_at(i));

        if pack_int_id as usize >= num_packs {
            errors.push(format!(
                "bad pack-int-id: {pack_int_id} ({num_packs} total packs)"
            ));
            errors.push(format!(
                "failed to load pack entry for oid[{i}] = {oid_hex}"
            ));
            continue;
        }

        // resolve MIDX-recorded offset (handle large offsets)
        let m_offset: u64 = if off_raw & MIDX_LARGE_OFFSET_NEEDED != 0 {
            let slot = (off_raw & !MIDX_LARGE_OFFSET_NEEDED) as usize;
            match large_off {
                Some((lo_off, lo_len)) if (slot + 1) * 8 <= lo_len => {
                    let b = lo_off + slot * 8;
                    let mut arr = [0u8; 8];
                    arr.copy_from_slice(&data[b..b + 8]);
                    u64::from_be_bytes(arr)
                }
                _ => {
                    errors.push("multi-pack-index large offset out of bounds".to_owned());
                    continue;
                }
            }
        } else {
            u64::from(off_raw)
        };

        let Some(Some(idx)) = pack_indexes.get(pack_int_id as usize) else {
            errors.push(format!(
                "failed to load pack entry for oid[{i}] = {oid_hex}"
            ));
            continue;
        };
        let Ok(oid) = ObjectId::from_bytes(oid_at(i)) else {
            errors.push(format!(
                "failed to load pack entry for oid[{i}] = {oid_hex}"
            ));
            continue;
        };
        match idx.find_offset(&oid) {
            Some(p_offset) => {
                if m_offset != p_offset {
                    errors.push(format!(
                        "incorrect object offset for oid[{i}] = {oid_hex}: {m_offset:x} != {p_offset:x}"
                    ));
                }
            }
            None => {
                errors.push(format!(
                    "failed to load pack entry for oid[{i}] = {oid_hex}"
                ));
            }
        }
    }

    if errors.is_empty() {
        Ok(())
    } else {
        Err(errors)
    }
}

/// Validate the trailing checksum of an in-memory MIDX image, using the
/// algorithm implied by the header hash version (SHA-1 or SHA-256).
fn midx_checksum_is_valid(data: &[u8]) -> bool {
    let hash_len = midx_hash_len(data);
    if data.len() < hash_len {
        return false;
    }
    HashAlgo::from_len(hash_len).is_some_and(|algo| verify_trailer(algo, data).is_ok())
}

/// Return the `pack-*.idx` basename for the MIDX preferred pack (RIDX position 0).
///
/// `objects_dir` is the repository object database (e.g. `.git/objects`), not `objects/pack`.
///
/// Used by `test-tool read-midx --preferred-pack` compatibility.
/// Pack index basenames (`pack-*.idx`) stored in the MIDX pack-names chunk.
pub fn read_midx_pack_idx_names(objects_dir: &Path) -> Result<Vec<String>> {
    let pack_dir = objects_dir.join("pack");
    let path = resolve_tip_midx_path(&pack_dir)
        .ok_or_else(|| Error::CorruptObject("no multi-pack-index found".to_owned()))?;
    let data = fs::read(&path).map_err(Error::Io)?;
    let (_, hdr_end, _) = parse_midx_header(&data)?;
    let (pn_off, pn_len) = find_chunk(&data, hdr_end, MIDX_CHUNKID_PACKNAMES)?;
    parse_pack_names_blob(&data[pn_off..pn_off + pn_len])
}

/// A single MIDX-referenced object together with the pack it is attributed to.
pub struct MidxObjectRef {
    pub oid: ObjectId,
    /// Index into the pack-names list returned alongside this.
    pub pack_int_id: usize,
}

/// Read the tip MIDX and return `(pack_names, objects)`, where each object names
/// the pack it is attributed to (`pack_int_id`). Mirrors the per-object
/// `nth_midxed_pack_int_id` iteration in Git used by expire/repack.
pub fn read_midx_objects(objects_dir: &Path) -> Result<(Vec<String>, Vec<MidxObjectRef>)> {
    let pack_dir = objects_dir.join("pack");
    let path = resolve_tip_midx_path(&pack_dir)
        .ok_or_else(|| Error::CorruptObject("no multi-pack-index found".to_owned()))?;
    let data = fs::read(&path).map_err(Error::Io)?;
    let (_, hdr_end, _) = parse_midx_header(&data)?;
    let (pn_off, pn_len) = find_chunk(&data, hdr_end, MIDX_CHUNKID_PACKNAMES)?;
    let names = parse_pack_names_blob(&data[pn_off..pn_off + pn_len])?;
    let hash_len = midx_hash_len(&data);
    let (oidl_off, oidl_len) = find_chunk(&data, hdr_end, MIDX_CHUNKID_OIDLOOKUP)?;
    let (ooff_off, ooff_len) = find_chunk(&data, hdr_end, MIDX_CHUNKID_OBJECTOFFSETS)?;
    if oidl_len % hash_len != 0 || ooff_len % 8 != 0 {
        return Err(Error::CorruptObject(
            "bad MIDX oid-lookup / object-offsets size".to_owned(),
        ));
    }
    let num = oidl_len / hash_len;
    if num * 8 != ooff_len {
        return Err(Error::CorruptObject(
            "MIDX oid count does not match object-offsets".to_owned(),
        ));
    }
    let mut objects = Vec::with_capacity(num);
    for i in 0..num {
        let oid =
            ObjectId::from_bytes(&data[oidl_off + i * hash_len..oidl_off + (i + 1) * hash_len])
                .map_err(|e| Error::CorruptObject(e.to_string()))?;
        let base = ooff_off + i * 8;
        let pack_id = read_be_u32(&data, base)? as usize;
        objects.push(MidxObjectRef {
            oid,
            pack_int_id: pack_id,
        });
    }
    Ok((names, objects))
}

/// OID rows from the active multi-pack-index, plus reverse-index order for pack-reuse bitmap bits.
///
/// Git assigns each object a **global bitmap bit** equal to its position in the MIDX reverse index
/// (`RIDX` chunk) traversal order — not its position in the pack `.idx` file. Helpers on this struct
/// map [`ObjectId`] → global bit the same way as `midx-write.c` (`midx_pack_order`).
#[derive(Debug, Clone)]
pub struct MidxReuseTables {
    /// OIDs in MIDX lexicographic order (same order as the OID lookup chunk).
    pub oids: Vec<ObjectId>,
    /// `(pack_int_id, in-pack offset)` parallel to `oids`.
    pub pack_and_offset: Vec<(u32, u64)>,
    /// `rid_order[rank]` is the OID-table index of the object at global bitmap rank `rank`.
    pub rid_order: Vec<u32>,
    /// Inverse map: global bitmap rank for each OID-table index.
    pub oid_idx_to_rank: Vec<u32>,
}

/// Load OID / object-offset / reverse-index tables from the tip MIDX (root or chain tip).
///
/// Returns [`None`] when there is no MIDX or no `RIDX` chunk (no pseudo-bitmap ordering).
pub fn load_midx_reuse_tables(objects_dir: &Path) -> Result<Option<MidxReuseTables>> {
    let pack_dir = objects_dir.join("pack");
    let Some(path) = resolve_tip_midx_path(&pack_dir) else {
        return Ok(None);
    };
    let data = fs::read(&path).map_err(Error::Io)?;
    let hash_len = midx_hash_len(&data);
    let (_, hdr_end, _) = parse_midx_header(&data)?;
    let (oidl_off, oid_l_len) = find_chunk(&data, hdr_end, MIDX_CHUNKID_OIDLOOKUP)?;
    let (ooff_off, ooff_len) = find_chunk(&data, hdr_end, MIDX_CHUNKID_OBJECTOFFSETS)?;
    let Ok((ridx_off, ridx_len)) = find_chunk(&data, hdr_end, MIDX_CHUNKID_REVINDEX) else {
        return Ok(None);
    };
    if oid_l_len % hash_len != 0 || ooff_len != oid_l_len / hash_len * 8 {
        return Err(Error::CorruptObject(
            "MIDX OID / offset chunk size mismatch".to_owned(),
        ));
    }
    let num_objects = oid_l_len / hash_len;
    if ridx_len != num_objects.saturating_mul(4) {
        return Err(Error::CorruptObject(
            "MIDX reverse index length does not match object count".to_owned(),
        ));
    }
    if num_objects == 0 {
        return Ok(None);
    }

    let mut oids = Vec::with_capacity(num_objects);
    for i in 0..num_objects {
        let base = oidl_off + i * hash_len;
        oids.push(ObjectId::from_bytes(&data[base..base + hash_len])?);
    }

    let mut pack_and_offset = Vec::with_capacity(num_objects);
    for i in 0..num_objects {
        let ob = ooff_off + i * 8;
        let pack_id = read_be_u32(&data, ob)?;
        let off32 = read_be_u32(&data, ob + 4)?;
        pack_and_offset.push((pack_id, u64::from(off32)));
    }

    let mut rid_order = Vec::with_capacity(num_objects);
    for i in 0..num_objects {
        let base = ridx_off + i * 4;
        rid_order.push(read_be_u32(&data, base)?);
    }

    let mut oid_idx_to_rank = vec![0u32; num_objects];
    for (rank, &oid_idx) in rid_order.iter().enumerate() {
        let idx = usize::try_from(oid_idx)
            .map_err(|_| Error::CorruptObject("bad MIDX reverse index entry".to_owned()))?;
        if idx >= num_objects {
            return Err(Error::CorruptObject(
                "MIDX reverse index out of range".to_owned(),
            ));
        }
        oid_idx_to_rank[idx] = u32::try_from(rank)
            .map_err(|_| Error::CorruptObject("too many MIDX objects".to_owned()))?;
    }

    Ok(Some(MidxReuseTables {
        oids,
        pack_and_offset,
        rid_order,
        oid_idx_to_rank,
    }))
}

impl MidxReuseTables {
    /// Global pseudo-bitmap index for `oid`, or [`None`] if the object is not in this MIDX.
    #[must_use]
    pub fn global_bitmap_bit(&self, oid: &ObjectId) -> Option<u32> {
        let oid_idx = self.oids.binary_search(oid).ok()?;
        Some(self.oid_idx_to_rank[oid_idx])
    }

    /// MIDX-canonical pack id for `oid` (the single copy the MIDX selected after deduplication),
    /// or [`None`] if the object is not in this MIDX. Used to reject cross-pack delta reuse: a
    /// delta is only reusable verbatim when its base resolves to the *same* pack the delta lives
    /// in, mirroring Git's `midx_pair_to_pack_pos` check in `try_partial_reuse`.
    #[must_use]
    pub fn canonical_pack(&self, oid: &ObjectId) -> Option<u32> {
        let oid_idx = self.oids.binary_search(oid).ok()?;
        Some(self.pack_and_offset[oid_idx].0)
    }
}

/// Look up which pack and in-pack offset holds `oid` when listed in the active MIDX.
///
/// Returns [`None`] when no MIDX exists or `oid` is absent (no allocation on miss).
pub fn midx_lookup_pack_and_offset_opt(
    objects_dir: &Path,
    oid: &ObjectId,
) -> Result<Option<(u32, u64)>> {
    let Some(chain) = prepared_midx_chain(objects_dir)? else {
        return Ok(None);
    };
    Ok(chain.lookup_pack_and_offset(oid))
}

/// Look up which pack and in-pack offset holds `oid` according to the active MIDX.
pub fn midx_lookup_pack_and_offset(objects_dir: &Path, oid: &ObjectId) -> Result<(u32, u64)> {
    let Some(chain) = prepared_midx_chain(objects_dir)? else {
        return Err(Error::CorruptObject("no multi-pack-index found".to_owned()));
    };
    chain.lookup_pack_and_offset(oid).ok_or_else(|| {
        Error::CorruptObject(format!("object {} not in multi-pack-index", oid.to_hex()))
    })
}

/// Returns whether `oid` appears in the active MIDX OID table for `objects_dir`.
///
/// [`None`] means there is no MIDX at the pack tip. [`Some`] is the lookup result when a MIDX exists.
pub fn midx_oid_listed_in_tip(objects_dir: &Path, oid: &ObjectId) -> Result<Option<bool>> {
    let Some(chain) = prepared_midx_chain(objects_dir)? else {
        return Ok(None);
    };
    Ok(Some(chain.oid_listed_in_tip(oid)))
}

/// Chunk offsets and metadata of a successfully loaded MIDX, ready for object reads.
struct MidxReadView {
    oidf_off: usize,
    oidl_off: usize,
    ooff_off: usize,
    loff: Option<(usize, usize)>,
    num_objects: usize,
    pack_names: Vec<String>,
}

enum MidxLoadResult {
    Ok(MidxReadView),
    /// The MIDX is unusable but not fatal (Git returns NULL and falls back to packs);
    /// an `error:`/`warning:` line has already been printed.
    Skip,
}

/// Print a recoverable MIDX `error:`/`warning:` line at most once per process.
///
/// Git loads the MIDX once and caches it, so a recoverable corruption is reported a
/// single time. grit re-reads the MIDX per object lookup, so without deduping the same
/// line would repeat; this guard restores the single-report behavior the tests expect.
fn midx_warn_once(line: &str) {
    use std::sync::Mutex;
    use std::sync::OnceLock;
    static SEEN: OnceLock<Mutex<HashSet<String>>> = OnceLock::new();
    let seen = SEEN.get_or_init(|| Mutex::new(HashSet::new()));
    if let Ok(mut set) = seen.lock() {
        if set.insert(line.to_string()) {
            eprintln!("{line}");
        }
    } else {
        eprintln!("{line}");
    }
}

/// Print Git-style `error:`/`fatal:` lines and exit 128, mirroring `die()` after the
/// preceding `error()` calls. `lines` are printed as `error:` except the last as `fatal:`.
fn midx_die(lines: &[&str]) -> ! {
    use std::io::Write;
    let mut err = std::io::stderr().lock();
    let n = lines.len();
    for (i, l) in lines.iter().enumerate() {
        if i + 1 == n {
            let _ = writeln!(err, "{}", crate::diagnostics::fatal_line(l));
        } else {
            let _ = writeln!(err, "{}", crate::diagnostics::error_line(l));
        }
    }
    let _ = err.flush();
    std::process::exit(128);
}

/// Validate and load a MIDX image for object reads, mirroring `load_multi_pack_index`
/// in git/midx.c. Fatal corruptions print `error:`/`fatal:` and exit (Git `die()`);
/// recoverable corruptions print an `error:`/`warning:` and return [`MidxLoadResult::Skip`].
fn midx_load_for_read(data: &[u8], expected_hash_version: u8) -> MidxLoadResult {
    if data.len() < MIDX_HEADER_SIZE + 20 {
        return MidxLoadResult::Skip;
    }
    let sig = u32::from_be_bytes([data[0], data[1], data[2], data[3]]);
    if sig != MIDX_SIGNATURE {
        midx_die(&[&format!(
            "multi-pack-index signature 0x{sig:08x} does not match signature 0x{MIDX_SIGNATURE:08x}"
        )]);
    }
    let version = data[4];
    if version != MIDX_VERSION_V1 && version != MIDX_VERSION_V2 {
        midx_die(&[&format!(
            "multi-pack-index version {version} not recognized"
        )]);
    }
    let hash_version = data[5];
    if hash_version != expected_hash_version {
        // `load_multi_pack_index` error()s then `goto cleanup_fail` (returns NULL),
        // so this is recoverable, not fatal. The expected version is the repository's
        // own `oid_version(hash_algo)` (SHA-1 → 1, SHA-256 → 2).
        midx_warn_once(&crate::diagnostics::error_line(&format!(
            "multi-pack-index hash version {hash_version} does not match version {expected_hash_version}"
        )));
        return MidxLoadResult::Skip;
    }
    let hash_len = if hash_version == 2 { 32usize } else { 20usize };
    let num_packs = u32::from_be_bytes([data[8], data[9], data[10], data[11]]) as usize;

    // Table of contents (chunk-format.c read_table_of_contents). Recoverable failures
    // (unaligned / improper offset / duplicate / non-zero terminator) print error() and
    // return NULL.
    let mut toc_errors: Vec<String> = Vec::new();
    let chunks = match parse_midx_toc(data, hash_len, &mut toc_errors) {
        Ok(c) => c,
        Err(_) => {
            for e in &toc_errors {
                midx_warn_once(&crate::diagnostics::error_line(e));
            }
            return MidxLoadResult::Skip;
        }
    };

    // Required pack-names chunk.
    let Some((pn_off, pn_len)) = toc_chunk_range(&chunks, data.len(), MIDX_CHUNKID_PACKNAMES)
    else {
        midx_die(&["multi-pack-index required pack-name chunk missing or corrupted"]);
    };

    // Required oid-fanout chunk + size + ordering (midx_read_oid_fanout).
    let Some((oidf_off, oidf_len)) = toc_chunk_range(&chunks, data.len(), MIDX_CHUNKID_OIDFANOUT)
    else {
        midx_die(&["multi-pack-index required OID fanout chunk missing or corrupted"]);
    };
    if oidf_len != 256 * 4 {
        midx_die(&[
            "multi-pack-index OID fanout is of the wrong size",
            "multi-pack-index required OID fanout chunk missing or corrupted",
        ]);
    }
    let fanout = |i: usize| -> u32 {
        let b = oidf_off + i * 4;
        u32::from_be_bytes([data[b], data[b + 1], data[b + 2], data[b + 3]])
    };
    for i in 0..255 {
        let f1 = fanout(i);
        let f2 = fanout(i + 1);
        if f1 > f2 {
            midx_die(&[
                &format!(
                    "oid fanout out of order: fanout[{i}] = {f1:x} > {f2:x} = fanout[{}]",
                    i + 1
                ),
                "multi-pack-index required OID fanout chunk missing or corrupted",
            ]);
        }
    }
    let num_objects = fanout(255) as usize;

    // Required oid-lookup chunk + size (midx_read_oid_lookup).
    let Some((oidl_off, oidl_len)) = toc_chunk_range(&chunks, data.len(), MIDX_CHUNKID_OIDLOOKUP)
    else {
        midx_die(&["multi-pack-index required OID lookup chunk missing or corrupted"]);
    };
    if oidl_len != hash_len * num_objects {
        midx_die(&[
            "multi-pack-index OID lookup chunk is the wrong size",
            "multi-pack-index required OID lookup chunk missing or corrupted",
        ]);
    }

    // Required object-offsets chunk + size (midx_read_object_offsets).
    let Some((ooff_off, ooff_len)) =
        toc_chunk_range(&chunks, data.len(), MIDX_CHUNKID_OBJECTOFFSETS)
    else {
        midx_die(&["multi-pack-index required object offsets chunk missing or corrupted"]);
    };
    if ooff_len != num_objects * 8 {
        midx_die(&[
            "multi-pack-index object offset chunk is the wrong size",
            "multi-pack-index required object offsets chunk missing or corrupted",
        ]);
    }

    let loff = toc_chunk_range(&chunks, data.len(), MIDX_CHUNKID_LARGEOFFSETS);

    // Optional revindex chunk — wrong size warns but does not fail the load.
    if let Some((_, rlen)) = toc_chunk_range(&chunks, data.len(), MIDX_CHUNKID_REVINDEX) {
        if rlen != num_objects * 4 {
            midx_warn_once(&crate::diagnostics::error_line(
                "multi-pack-index reverse-index chunk is the wrong size",
            ));
            midx_warn_once(&crate::diagnostics::warning_line(
                "multi-pack bitmap is missing required reverse index",
            ));
        }
    }

    // Pack-name parsing (die if a name is unterminated).
    let mut pack_names: Vec<String> = Vec::with_capacity(num_packs);
    let blob = &data[pn_off..pn_off + pn_len];
    let mut start = 0usize;
    for _ in 0..num_packs {
        let Some(rel) = blob[start..].iter().position(|&b| b == 0) else {
            midx_die(&["multi-pack-index pack-name chunk is too short"]);
        };
        let name = match std::str::from_utf8(&blob[start..start + rel]) {
            Ok(s) => s.to_string(),
            Err(_) => midx_die(&["multi-pack-index pack-name chunk is too short"]),
        };
        if version == MIDX_VERSION_V1
            && !pack_names.is_empty()
            && name.as_str() <= pack_names.last().map(|s| s.as_str()).unwrap_or("")
        {
            midx_die(&[&format!(
                "multi-pack-index pack names out of order: '{}' before '{name}'",
                pack_names.last().cloned().unwrap_or_default()
            )]);
        }
        pack_names.push(name);
        start += rel + 1;
    }

    MidxLoadResult::Ok(MidxReadView {
        oidf_off,
        oidl_off,
        ooff_off,
        loff,
        num_objects,
        pack_names,
    })
}

/// Eagerly validate that every pack named by the active MIDX has a readable `.idx`.
///
/// Mirrors git/packfile.c `open_pack_index`: when `prepare_packed_git` registers the
/// packs the MIDX references, a pack whose `.idx` cannot be opened (truncated/corrupt)
/// triggers `error: packfile <pack> index unavailable`. Git reports this once because the
/// MIDX/pack store is prepared a single time; this routine reproduces that even when the
/// object that triggered the read is found loose (so it never reaches the per-object MIDX
/// lookup). Runs at most once per process per `objects_dir`.
/// Validate pack indexes named by the active MIDX once per prepared layer view.
pub fn validate_midx_referenced_packs(objects_dir: &Path) {
    if let Ok(Some(chain)) = prepared_midx_chain(objects_dir) {
        chain.ensure_packs_validated();
    }
}

/// When `core.multiPackIndex` is enabled, try to read `oid` from the active MIDX in `objects_dir`.
///
/// Returns [`None`] when no MIDX exists or `oid` is not listed. Returns `Some(Err(..))` when the
/// MIDX is present but malformed (callers surface Git-style `error:` / `fatal:` messages).
pub fn try_read_object_via_midx(
    objects_dir: &Path,
    oid: &ObjectId,
) -> Result<Option<crate::objects::Object>> {
    let Some(chain) = prepared_midx_chain(objects_dir)? else {
        return Ok(None);
    };
    chain.try_read_object(oid)
}

/// Like [`try_read_object_via_midx`], but returns [`ObjectInfo`] without inflating object bodies.
pub fn try_read_info_via_midx(objects_dir: &Path, oid: &ObjectId) -> Result<Option<ObjectInfo>> {
    let Some(chain) = prepared_midx_chain(objects_dir)? else {
        return Ok(None);
    };
    chain.try_read_info(oid)
}

pub fn read_midx_preferred_idx_name(objects_dir: &Path) -> Result<String> {
    let pack_dir = objects_dir.join("pack");
    let path = resolve_tip_midx_path(&pack_dir)
        .ok_or_else(|| Error::CorruptObject("no multi-pack-index found".to_owned()))?;
    let data = fs::read(&path).map_err(Error::Io)?;
    let (_, hdr_end, _) = parse_midx_header(&data)?;
    let (pn_off, pn_len) = find_chunk(&data, hdr_end, MIDX_CHUNKID_PACKNAMES)?;
    let names = parse_pack_names_blob(&data[pn_off..pn_off + pn_len])?;
    let (ooff_off, ooff_len) = find_chunk(&data, hdr_end, MIDX_CHUNKID_OBJECTOFFSETS)?;
    // The preferred pack is recorded in the MIDX reverse index, which is only
    // present when the MIDX has a bitmap. Without it, the preferred pack is
    // unknowable (git/midx.c `midx_preferred_pack` returns -1). Prefer the
    // embedded RIDX chunk; otherwise fall back to a `multi-pack-index*.rev`
    // sidecar, matching `load_midx_revindex`.
    let (ridx_off, ridx_len) = match find_chunk(&data, hdr_end, MIDX_CHUNKID_REVINDEX) {
        Ok(r) => r,
        Err(_) => {
            return Err(Error::CorruptObject(
                "could not determine MIDX preferred pack".to_owned(),
            ));
        }
    };

    if ridx_len < 4 || ooff_len < 8 {
        return Err(Error::CorruptObject("truncated MIDX RIDX/OOFF".to_owned()));
    }
    let first_oid_idx = read_be_u32(&data, ridx_off)? as usize;
    let entry_base = ooff_off + first_oid_idx * 8;
    if entry_base + 8 > data.len() || entry_base + 8 > ooff_off + ooff_len {
        return Err(Error::CorruptObject(
            "bad MIDX object-offsets index".to_owned(),
        ));
    }
    let pack_id = read_be_u32(&data, entry_base)?;
    let idx = usize::try_from(pack_id)
        .map_err(|_| Error::CorruptObject("pack id overflow in multi-pack-index".to_owned()))?;
    names
        .get(idx)
        .cloned()
        .ok_or_else(|| Error::CorruptObject("preferred pack id out of range".to_owned()))
}

/// Build `objects/pack/multi-pack-index` for all pack indexes in `pack_dir`.
///
/// Returns an error if there are no `.idx` files, if an object offset does not
/// fit in 31 bits (no `LOFF` chunk yet), or if I/O fails.
/// Remove every multi-pack-index file under `pack_dir` (root file, sidecars, and
/// `multi-pack-index.d/`). Used by full `repack -a` so stale incremental chains do not survive.
pub fn clear_pack_midx_state(pack_dir: &Path) -> Result<()> {
    let _ = fs::remove_file(pack_dir.join("multi-pack-index"));
    scrub_root_midx_sidecars_except(pack_dir, None)?;
    let midx_d = midx_d_dir(pack_dir);
    if midx_d.exists() {
        let _ = fs::remove_dir_all(&midx_d);
    }
    midx_cache::evict_pack_dir(pack_dir);
    Ok(())
}

pub fn write_multi_pack_index(pack_dir: &Path) -> Result<()> {
    write_multi_pack_index_with_options(pack_dir, &WriteMultiPackIndexOptions::default())
}

/// Write `multi-pack-index` with optional preferred pack, placeholders, and incremental chain.
pub fn write_multi_pack_index_with_options(
    pack_dir: &Path,
    opts: &WriteMultiPackIndexOptions,
) -> Result<()> {
    // Git warns and ignores an existing MIDX whose checksum does not validate when
    // writing a fresh (non-stdin-packs) MIDX (git/midx-write.c `write_midx_internal`).
    if opts.pack_names_subset_ordered.is_none() {
        if let Some(existing) = resolve_tip_midx_path(pack_dir) {
            if let Ok(bytes) = fs::read(&existing) {
                if midx_checksum_is_valid(&bytes) {
                    // A fresh write copies the existing MIDX's packs. Loading a pack
                    // it references whose `.pack` is gone fails with `could not load
                    // pack N` (git/midx-write.c `fill_pack_from_midx`).
                    if let Ok((_, existing_names)) = oids_and_packs_from_midx_data(&bytes) {
                        for (i, name) in existing_names.iter().enumerate() {
                            let stem = name.strip_suffix(".idx").unwrap_or(name);
                            if !pack_dir.join(format!("{stem}.pack")).exists() {
                                eprintln!(
                                    "{}",
                                    crate::diagnostics::error_line(&format!(
                                        "could not load pack {i}"
                                    ))
                                );
                                return Err(Error::CorruptObject(format!(
                                    "could not load pack {i}"
                                )));
                            }
                        }
                    }
                } else {
                    eprintln!(
                        "{}",
                        crate::diagnostics::warning_line(
                            "ignoring existing multi-pack-index; checksum mismatch"
                        )
                    );
                }
            }
        }
    }

    // Git's MIDX covers every pack index in the directory regardless of its
    // basename (the `.git/objects/pack/test-*.idx` packs created by t7900's
    // incremental-repack test, for instance), so include any `*.idx` whose
    // companion `.pack` exists.
    let mut idx_names: Vec<String> = fs::read_dir(pack_dir)
        .map(|rd| {
            rd.filter_map(|e| e.ok())
                .filter_map(|e| {
                    let name = e.file_name().to_string_lossy().to_string();
                    let stem = name.strip_suffix(".idx")?;
                    if pack_dir.join(format!("{stem}.pack")).exists() {
                        Some(name)
                    } else {
                        None
                    }
                })
                .collect()
        })
        .unwrap_or_default();
    idx_names.sort();

    let idx_names: Vec<String> = if let Some(sub) = &opts.pack_names_subset_ordered {
        let mut out = Vec::new();
        for line in sub {
            let want = normalize_pack_idx_basename(line)?;
            if let Some(found) = idx_names.iter().find(|n| **n == want).cloned() {
                if !out.contains(&found) {
                    out.push(found);
                }
            }
            // Unknown names on stdin are silently ignored (Git skips packs it
            // cannot find rather than failing the whole write).
        }
        out
    } else {
        idx_names
    };

    // Resolve / validate the preferred pack against the working pack set. Git emits a
    // (non-fatal) `warning: unknown preferred pack: '<name>'` when it cannot be matched.
    let mut preferred_warned = false;
    if let Some(raw) = opts.preferred_pack_name.as_deref() {
        if opts.preferred_pack_idx.is_none()
            && !idx_names
                .iter()
                .any(|n| cmp_idx_or_pack_name(raw, n).is_eq())
        {
            eprintln!(
                "{}",
                crate::diagnostics::warning_line(&format!("unknown preferred pack: '{raw}'"))
            );
            preferred_warned = true;
        }
    }

    if idx_names.is_empty() {
        // Git `write_midx_internal`: `error("no pack files to index.")` then fail.
        eprintln!(
            "{}",
            crate::diagnostics::error_line("no pack files to index.")
        );
        return Err(Error::CorruptObject("no pack files to index.".to_owned()));
    }

    let (base_oids, base_pack_names) = if opts.incremental {
        collect_incremental_base(pack_dir)?
    } else {
        (HashSet::new(), HashSet::new())
    };

    let layer_idx_names: Vec<String> = if opts.incremental {
        idx_names
            .iter()
            .filter(|n| {
                !base_pack_names
                    .iter()
                    .any(|bp| pack_names_match_layer(bp, n))
            })
            .cloned()
            .collect()
    } else {
        idx_names.clone()
    };

    if opts.incremental && layer_idx_names.is_empty() {
        return Ok(());
    }

    let work_names = if opts.incremental {
        &layer_idx_names[..]
    } else {
        &idx_names[..]
    };

    let mut preferred_idx = opts.preferred_pack_idx.map(|p| p as usize);
    if preferred_idx.is_none() && !preferred_warned {
        if let Some(raw) = opts.preferred_pack_name.as_deref() {
            // Already validated against `idx_names`; resolve against the working set.
            preferred_idx = work_names
                .iter()
                .position(|n| cmp_idx_or_pack_name(raw, n).is_eq());
        }
    }
    if preferred_idx.is_none() && opts.write_bitmap_placeholders && !work_names.is_empty() {
        preferred_idx = preferred_pack_index_by_mtime(pack_dir, work_names)?;
    }
    if let Some(p) = preferred_idx {
        if p >= work_names.len() {
            return Err(Error::CorruptObject(
                "preferred pack index out of range".to_owned(),
            ));
        }
    }

    let mut indexes: Vec<PackIndex> = Vec::with_capacity(work_names.len());
    for name in work_names {
        let path = pack_dir.join(name);
        // Do not re-verify the idx trailer here; Git reads the offset table
        // directly (t5319 forces a deliberately corrupt-but-valid 64-bit idx).
        indexes.push(crate::pack::read_pack_index_no_verify(&path)?);
    }

    // Git refuses an explicitly preferred pack that has no objects.
    if let Some(p) = preferred_idx {
        if indexes.get(p).map(PackIndex::len).unwrap_or(0) == 0 {
            let name = work_names.get(p).cloned().unwrap_or_default();
            let pack_name = name.strip_suffix(".idx").unwrap_or(&name);
            eprintln!(
                "{}",
                crate::diagnostics::error_line(&format!(
                    "cannot select preferred pack {pack_name}.pack with no objects"
                ))
            );
            return Err(Error::CorruptObject(
                "cannot select preferred pack with no objects".to_owned(),
            ));
        }
    }

    let pack_mtimes_layer: Vec<std::time::SystemTime> =
        indexes.iter().map(pack_mtime_for_midx).collect();
    let preferred_u32 = preferred_idx.map(|p| p as u32);
    let select_hash_len = if repo_midx_hash_version(pack_dir) == 2 {
        32
    } else {
        20
    };

    let mut best: HashMap<ObjectId, MidxEntry> = HashMap::new();
    for (pack_id, idx) in indexes.iter().enumerate() {
        let pack_id = u32::try_from(pack_id).map_err(|_| {
            Error::CorruptObject("too many pack files for multi-pack-index".to_owned())
        })?;
        let mtime = pack_mtimes_layer[pack_id as usize];
        for e in idx.iter() {
            if e.oid().len() != select_hash_len {
                continue;
            }
            let Ok(oid) = ObjectId::from_bytes(e.oid()) else {
                continue;
            };
            if opts.incremental && base_oids.contains(&oid) {
                continue;
            }
            let cand = MidxEntry {
                oid,
                pack_id,
                offset: e.offset(),
                pack_mtime: mtime,
            };
            match best.get(&oid) {
                None => {
                    best.insert(oid, cand);
                }
                Some(cur) => {
                    if midx_pick_better_entry(cur, pack_id, e.offset(), mtime, preferred_u32) {
                        best.insert(oid, cand);
                    }
                }
            }
        }
    }

    let bitmap_placeholders =
        opts.write_bitmap_placeholders && (!opts.incremental || !best.is_empty());

    let omit_embedded_ridx = opts.write_rev_placeholder;
    // An incremental layer must not repeat objects already provided by the base
    // chain even when the layer's own pack physically contains them (a fresh pack
    // built with `--revs` from a tag range, for instance). Filter by base OID.
    let exclude = if opts.incremental && !base_oids.is_empty() {
        Some(&base_oids)
    } else {
        None
    };
    let (out, rev_sidecar_order) = build_midx_bytes_filtered(
        work_names,
        &indexes,
        preferred_idx,
        bitmap_placeholders,
        omit_embedded_ridx,
        opts.version.unwrap_or(MIDX_VERSION_V2),
        repo_midx_hash_version(pack_dir),
        exclude,
    )?;

    let repo_algo = repo_hash_algo_for_pack_dir(pack_dir);
    let hash_len = repo_algo.len();
    let hash = &out[out.len() - hash_len..];
    let hash_hex = hex::encode(hash);
    let hash_arr: Vec<u8> = hash.to_vec();

    if opts.incremental {
        let root_midx = pack_dir.join("multi-pack-index");
        let chain_path = chain_file_path(pack_dir);
        let chain_existed = chain_path.exists();

        let mut chain = if root_midx.exists() && !chain_existed {
            let root_hex = midx_checksum_hex_from_path(&root_midx)?;
            link_root_midx_into_chain(pack_dir, &root_hex)?;
            vec![root_hex]
        } else {
            read_chain_layer_hashes(pack_dir).unwrap_or_default()
        };

        chain.push(hash_hex.clone());

        let midx_d = midx_d_dir(pack_dir);
        fs::create_dir_all(&midx_d).map_err(Error::Io)?;

        let layer_path = midx_d.join(format!("multi-pack-index-{hash_hex}.midx"));
        fs::write(&layer_path, &out).map_err(Error::Io)?;

        let mut chain_data = String::new();
        for h in &chain {
            chain_data.push_str(h);
            chain_data.push('\n');
        }
        fs::write(chain_file_path(pack_dir), chain_data.as_bytes()).map_err(Error::Io)?;

        clear_stale_split_layers(pack_dir, &chain)?;

        let _ = fs::remove_file(pack_dir.join("multi-pack-index"));
        scrub_root_midx_sidecars(pack_dir)?;
        if bitmap_placeholders {
            let full = hex::encode(hash);
            fs::write(midx_d.join(format!("multi-pack-index-{full}.bitmap")), [])
                .map_err(Error::Io)?;
            if opts.write_rev_placeholder {
                let rev_path = midx_d.join(format!("multi-pack-index-{full}.rev"));
                if let Some(order) = rev_sidecar_order.as_ref() {
                    write_midx_rev_sidecar(&rev_path, order, &hash_arr, repo_algo)?;
                } else {
                    fs::write(rev_path, []).map_err(Error::Io)?;
                }
            }
        }
    } else {
        // A non-incremental write replaces any prior split layout. Git removes the
        // individual incremental layer files inside `multi-pack-index.d/` and
        // unlinks the chain file, but never `rmdir`s the directory itself, so an
        // empty `multi-pack-index.d/` is left behind (t5334 expects
        // `test_dir_is_empty $midxdir` after the conversion).
        let dest = pack_dir.join("multi-pack-index");

        // Git's `midx_needs_update`: if the new MIDX is byte-identical to the one
        // already on disk and we are not (re)writing a bitmap, leave the file
        // untouched so its mtime is preserved (t5319 `test_midx_is_retained`).
        let bitmap_path = pack_dir.join(format!("multi-pack-index-{hash_hex}.bitmap"));
        let bitmap_ok = !opts.write_bitmap_placeholders || bitmap_path.exists();
        // Only short-circuit when there is no active incremental chain to collapse;
        // an empty leftover `multi-pack-index.d/` (from a prior conversion) must not
        // defeat the retention optimization, so key off the chain file, not the dir.
        if bitmap_ok && !chain_file_path(pack_dir).exists() {
            if let Ok(existing) = fs::read(&dest) {
                if existing == out {
                    return Ok(());
                }
            }
        }

        clear_incremental_midx_files(pack_dir)?;

        fs::write(&dest, &out).map_err(Error::Io)?;

        scrub_root_midx_sidecars_except(pack_dir, Some(&hash_hex))?;

        if opts.write_bitmap_placeholders {
            fs::write(
                pack_dir.join(format!("multi-pack-index-{hash_hex}.bitmap")),
                [],
            )
            .map_err(Error::Io)?;
            if opts.write_rev_placeholder {
                let rev_path = pack_dir.join(format!("multi-pack-index-{hash_hex}.rev"));
                if let Some(order) = rev_sidecar_order.as_ref() {
                    write_midx_rev_sidecar(&rev_path, order, &hash_arr, repo_algo)?;
                } else {
                    fs::write(rev_path, []).map_err(Error::Io)?;
                }
            }
        }
    }

    midx_cache::evict_pack_dir(pack_dir);
    Ok(())
}

fn pack_names_match_layer(base_name: &str, disk_idx: &str) -> bool {
    if base_name == disk_idx {
        return true;
    }
    cmp_idx_or_pack_name(disk_idx, base_name).is_eq()
}

/// Failure modes of [`compact_multi_pack_index`], each mapping to one of git's
/// user-facing diagnostics in `cmd_multi_pack_index_compact`.
#[derive(Debug)]
pub enum CompactError {
    /// `--incremental` was requested but no chain exists yet.
    NoChain,
    /// One of the endpoint checksums does not name a layer in the chain. Carries the
    /// raw argument text so the message matches `could not find MIDX: <arg>`.
    MissingEndpoint(String),
    /// Both endpoints resolve to the same layer.
    IdenticalEndpoints,
    /// `from` (`argv[0]`) is newer than `to` (`argv[1]`); git requires `from` to be an
    /// ancestor of `to`. Carries `(from, to)` arg text for the diagnostic.
    NotAncestor(String, String),
    /// Compaction was requested with the v1 on-disk MIDX format.
    V1Format,
    /// Any underlying I/O or parse failure.
    Other(String),
}

impl std::fmt::Display for CompactError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CompactError::NoChain => write!(f, "no multi-pack-index chain to compact"),
            CompactError::MissingEndpoint(s) => write!(f, "could not find MIDX: {s}"),
            CompactError::IdenticalEndpoints => {
                write!(f, "MIDX compaction endpoints must be unique")
            }
            CompactError::NotAncestor(from, to) => {
                write!(f, "MIDX {from} must be an ancestor of {to}")
            }
            CompactError::V1Format => write!(f, "cannot perform MIDX compaction with v1 format"),
            CompactError::Other(s) => write!(f, "{s}"),
        }
    }
}

impl From<Error> for CompactError {
    fn from(e: Error) -> Self {
        CompactError::Other(e.to_string())
    }
}

/// Collect every OID provided by the chain layers in `hashes` (each layer file is
/// self-contained: it lists only its own incremental objects).
fn collect_layer_oids(pack_dir: &Path, hashes: &[String]) -> Result<HashSet<ObjectId>> {
    let mut oids = HashSet::new();
    for h in hashes {
        let p = midx_d_dir(pack_dir).join(format!("multi-pack-index-{h}.midx"));
        let data = load_midx_file(&p)?;
        let (layer_oids, _) = oids_and_packs_from_midx_data(&data)?;
        oids.extend(layer_oids);
    }
    Ok(oids)
}

/// Pack idx basenames listed by a single chain layer, in the layer's stored order.
fn layer_pack_names(pack_dir: &Path, hash: &str) -> Result<Vec<String>> {
    let p = midx_d_dir(pack_dir).join(format!("multi-pack-index-{hash}.midx"));
    let data = load_midx_file(&p)?;
    let (_, hdr_end, _) = parse_midx_header(&data)?;
    let (pn_off, pn_len) = find_chunk(&data, hdr_end, MIDX_CHUNKID_PACKNAMES)?;
    parse_pack_names_blob(&data[pn_off..pn_off + pn_len])
}

/// `git multi-pack-index compact <from> <to>`: merge the inclusive chain range
/// `[from..to]` (oldest→newest, matching git's `from`=`argv[0]` / `to`=`argv[1]`) into a
/// single new incremental layer, preserving pack order, and rewrite the chain as
/// `[layers before from] + [compacted layer] + [layers after to]`.
///
/// Mirrors `write_midx_file_compact` (git/midx-write.c). Because grit's chain layers
/// are self-contained (each lists only its own packs/objects), layers outside the
/// compacted range keep their existing files and checksums untouched.
pub fn compact_multi_pack_index(
    pack_dir: &Path,
    from_arg: &str,
    to_arg: &str,
    write_bitmaps: bool,
    write_rev: bool,
    version: Option<u8>,
) -> std::result::Result<(), CompactError> {
    if version == Some(MIDX_VERSION_V1) {
        return Err(CompactError::V1Format);
    }

    let chain = read_chain_layer_hashes(pack_dir).map_err(|_| CompactError::NoChain)?;
    if chain.is_empty() {
        return Err(CompactError::NoChain);
    }

    let from_hex = from_arg.to_ascii_lowercase();
    let to_hex = to_arg.to_ascii_lowercase();

    let from_pos = chain.iter().position(|h| *h == from_hex);
    let to_pos = chain.iter().position(|h| *h == to_hex);

    // Match git: report `from` first, then `to`, when an endpoint is missing.
    let Some(from_pos) = from_pos else {
        return Err(CompactError::MissingEndpoint(from_arg.to_string()));
    };
    let Some(to_pos) = to_pos else {
        return Err(CompactError::MissingEndpoint(to_arg.to_string()));
    };

    if from_pos == to_pos {
        return Err(CompactError::IdenticalEndpoints);
    }
    // git walks `base_midx` from `from`; reaching `to` means `from` is an ancestor of
    // `to`, i.e. `from` is newer (higher chain index) than `to`. That is the reverse
    // of what compaction expects, so report the "must be an ancestor" error.
    if from_pos > to_pos {
        return Err(CompactError::NotAncestor(
            from_arg.to_string(),
            to_arg.to_string(),
        ));
    }

    // Layers strictly before `from` form the base; their objects are excluded from
    // the compacted layer.
    let base_hashes = &chain[..from_pos];
    let merged_hashes = &chain[from_pos..=to_pos];
    let upper_hashes = &chain[to_pos + 1..];

    let base_oids = collect_layer_oids(pack_dir, base_hashes)?;

    // Gather the merged layers' pack idx names in chain order (oldest layer first),
    // preserving each layer's internal order (git's `fill_packs_from_midx_range`).
    let mut ordered_idx_names: Vec<String> = Vec::new();
    for h in merged_hashes {
        for name in layer_pack_names(pack_dir, h)? {
            if !ordered_idx_names.contains(&name) {
                ordered_idx_names.push(name);
            }
        }
    }

    if ordered_idx_names.is_empty() {
        return Err(CompactError::Other(
            "no packs found in compaction range".to_owned(),
        ));
    }

    // Load the pack indexes in the resolved order.
    let mut indexes: Vec<PackIndex> = Vec::with_capacity(ordered_idx_names.len());
    for name in &ordered_idx_names {
        let path = pack_dir.join(name);
        indexes.push(crate::pack::read_pack_index_no_verify(&path)?);
    }

    // When writing a bitmap, git sets the preferred pack to the first (oldest) pack
    // of the compacted range so its objects win duplicate selection.
    let preferred_idx = if write_bitmaps { Some(0usize) } else { None };

    let exclude = if base_oids.is_empty() {
        None
    } else {
        Some(&base_oids)
    };

    let (out, rev_sidecar_order) = build_midx_bytes_filtered(
        &ordered_idx_names,
        &indexes,
        preferred_idx,
        write_bitmaps,
        write_rev,
        version.unwrap_or(MIDX_VERSION_V2),
        repo_midx_hash_version(pack_dir),
        exclude,
    )?;

    let repo_algo = repo_hash_algo_for_pack_dir(pack_dir);
    let hash_len = repo_algo.len();
    let hash = &out[out.len() - hash_len..];
    let hash_hex = hex::encode(hash);
    let hash_arr: Vec<u8> = hash.to_vec();

    let midx_d = midx_d_dir(pack_dir);
    fs::create_dir_all(&midx_d).map_err(Error::Io)?;

    let layer_path = midx_d.join(format!("multi-pack-index-{hash_hex}.midx"));
    fs::write(&layer_path, &out).map_err(Error::Io)?;

    // New chain: base layers, the compacted layer, then the untouched upper layers.
    let mut new_chain: Vec<String> = Vec::new();
    new_chain.extend(base_hashes.iter().cloned());
    new_chain.push(hash_hex.clone());
    new_chain.extend(upper_hashes.iter().cloned());

    let mut chain_data = String::new();
    for h in &new_chain {
        chain_data.push_str(h);
        chain_data.push('\n');
    }
    fs::write(chain_file_path(pack_dir), chain_data.as_bytes()).map_err(Error::Io)?;

    if write_bitmaps {
        fs::write(
            midx_d.join(format!("multi-pack-index-{hash_hex}.bitmap")),
            [],
        )
        .map_err(Error::Io)?;
        let rev_path = midx_d.join(format!("multi-pack-index-{hash_hex}.rev"));
        if write_rev {
            if let Some(order) = rev_sidecar_order.as_ref() {
                write_midx_rev_sidecar(&rev_path, order, &hash_arr, repo_algo)?;
            } else {
                fs::write(rev_path, []).map_err(Error::Io)?;
            }
        }
    }

    // Drop the now-removed range layers and their sidecars.
    clear_stale_split_layers(pack_dir, &new_chain)?;

    midx_cache::evict_pack_dir(pack_dir);
    Ok(())
}

fn scrub_root_midx_sidecars(pack_dir: &Path) -> Result<()> {
    scrub_root_midx_sidecars_except(pack_dir, None)
}

fn scrub_root_midx_sidecars_except(pack_dir: &Path, keep_hex: Option<&str>) -> Result<()> {
    let Ok(rd) = fs::read_dir(pack_dir) else {
        return Ok(());
    };
    for ent in rd {
        let ent = ent.map_err(Error::Io)?;
        let name = ent.file_name().to_string_lossy().to_string();
        let Some(rest) = name.strip_prefix("multi-pack-index-") else {
            continue;
        };
        if !(rest.ends_with(".bitmap") || rest.ends_with(".rev")) {
            continue;
        }
        let hash_part = rest
            .strip_suffix(".bitmap")
            .or_else(|| rest.strip_suffix(".rev"))
            .unwrap_or(rest);
        // Git's `clear_midx_files_ext` removes any `multi-pack-index-<hash>.<ext>`
        // sidecar that does not belong to the current MIDX, regardless of the
        // hash's textual length (t5319 plants a `multi-pack-index-abc.rev`).
        if keep_hex.is_some_and(|k| k == hash_part) {
            continue;
        }
        let _ = fs::remove_file(ent.path());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{
        clear_pack_midx_state, resolve_tip_midx_path, try_read_object_via_midx,
        write_multi_pack_index,
    };
    use crate::hot_path_test_metrics::HotPathMetricsScope;
    use crate::objects::ObjectId;
    use crate::odb::Odb;
    use crate::pack::{clear_pack_cache, read_object_from_packs};
    use std::path::Path;
    use std::process::Command;
    use std::sync::Arc;

    fn git(dir: &Path, args: &[&str]) {
        let out = Command::new("git")
            .current_dir(dir)
            .args(args)
            .env("GIT_AUTHOR_NAME", "T")
            .env("GIT_AUTHOR_EMAIL", "t@example.com")
            .env("GIT_COMMITTER_NAME", "T")
            .env("GIT_COMMITTER_EMAIL", "t@example.com")
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_SYSTEM", "/dev/null")
            .output()
            .expect("run git");
        assert!(
            out.status.success(),
            "git {:?} failed: {}",
            args,
            String::from_utf8_lossy(&out.stderr)
        );
    }

    fn git_available() -> bool {
        Command::new("git")
            .arg("--version")
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false)
    }

    /// Pack fixture: one commit, all objects in a single pack (no MIDX yet).
    fn midx_fixture(with_midx: bool) -> Option<(tempfile::TempDir, ObjectId)> {
        if !git_available() {
            return None;
        }
        let tmp = tempfile::tempdir().ok()?;
        let dir = tmp.path();
        git(dir, &["init", "-q", "-b", "main"]);
        std::fs::write(dir.join("f.txt"), b"hello midx cache").ok()?;
        git(dir, &["add", "f.txt"]);
        git(dir, &["commit", "-q", "-m", "c"]);
        git(dir, &["repack", "-adf"]);
        let pack_dir = dir.join(".git/objects/pack");
        if with_midx {
            write_multi_pack_index(&pack_dir).ok()?;
        }
        let hex = Command::new("git")
            .current_dir(dir)
            .args(["rev-parse", "HEAD"])
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_SYSTEM", "/dev/null")
            .output()
            .ok()?;
        if !hex.status.success() {
            return None;
        }
        let oid = ObjectId::from_hex(std::str::from_utf8(&hex.stdout).ok()?.trim()).ok()?;
        Some((tmp, oid))
    }

    #[test]
    fn vanished_cached_tip_falls_back() {
        let Some((tmp, oid)) = midx_fixture(true) else {
            eprintln!("SKIP: git unavailable for MIDX fixture");
            return;
        };
        let git_dir = tmp.path().join(".git");
        let objects = git_dir.join("objects");
        let pack_dir = objects.join("pack");
        let midx_path = pack_dir.join("multi-pack-index");
        assert!(midx_path.is_file(), "expected root MIDX");

        assert!(
            try_read_object_via_midx(&objects, &oid)
                .expect("prime midx read")
                .is_some(),
            "object should be reachable via MIDX"
        );

        std::fs::remove_file(&midx_path).expect("simulate external MIDX removal");

        // Prepared MIDX bytes stay in-process until evict; external unlink alone does not
        // invalidate the parsed layer (no stat on the hot path).
        assert!(
            try_read_object_via_midx(&objects, &oid)
                .expect("vanished tip must not fail reads")
                .is_some(),
            "cached prepared layer still serves until pack-dir evict"
        );

        let odb = Odb::new(&objects).with_config_git_dir(git_dir);
        let via_pack = odb.read(&oid).expect("pack fallback after vanished MIDX");
        assert_eq!(via_pack.kind, crate::objects::ObjectKind::Commit);
    }

    #[test]
    fn writer_evicts_tip_cache() {
        let Some((tmp, oid)) = midx_fixture(true) else {
            eprintln!("SKIP: git unavailable for MIDX fixture");
            return;
        };
        let objects = tmp.path().join(".git/objects");
        let pack_dir = objects.join("pack");

        assert!(try_read_object_via_midx(&objects, &oid)
            .expect("prime caches")
            .is_some());
        assert!(resolve_tip_midx_path(&pack_dir).is_some());

        clear_pack_midx_state(&pack_dir).expect("clear midx");
        assert!(resolve_tip_midx_path(&pack_dir).is_none());

        // Tip-path cache was evicted: listing must not resurrect the removed MIDX.
        assert!(try_read_object_via_midx(&objects, &oid)
            .expect("read after clear")
            .is_none());

        write_multi_pack_index(&pack_dir).expect("rewrite midx");
        assert!(
            try_read_object_via_midx(&objects, &oid)
                .expect("read after rewrite")
                .is_some(),
            "fresh MIDX must be visible after in-process rewrite"
        );
    }

    #[test]
    fn midx_hot_path_does_not_stat_after_cache_warm() {
        let Some((tmp, oid)) = midx_fixture(true) else {
            eprintln!("SKIP: git unavailable for MIDX fixture");
            return;
        };
        let git_dir = tmp.path().join(".git");
        let objects = git_dir.join("objects");
        let odb = Odb::new(&objects).with_config_git_dir(git_dir);
        let metrics = odb.hot_path_test_metrics_arc();
        let _scope = HotPathMetricsScope::install(Arc::clone(&metrics));
        metrics.set_stamp_counting(true);
        let _ = odb.read(&oid).expect("prime");
        metrics.reset_midx_stamp_stats();
        for _ in 0..200 {
            let _ = odb.read(&oid).expect("repeat");
        }
        metrics.set_stamp_counting(false);
        assert_eq!(metrics.midx_stamp_stat_calls(), 0);
    }

    #[test]
    fn decode_high_bit_offset_is_raw_without_loff_chunk() {
        use super::midx_cache::decode_midx_object_offset_for_test;
        assert_eq!(
            decode_midx_object_offset_for_test(&[], 0x8000_0010, None),
            Some(0x8000_0010)
        );
    }

    #[test]
    fn lookup_high_bit_raw_offset_written_without_loff() {
        use super::{
            clear_pack_midx_state, find_chunk, midx_lookup_pack_and_offset_opt, parse_midx_header,
            MIDX_CHUNKID_OBJECTOFFSETS, MIDX_CHUNKID_OIDLOOKUP,
        };
        let Some((tmp, oid)) = midx_fixture(true) else {
            eprintln!("SKIP: git unavailable for MIDX fixture");
            return;
        };
        let objects = tmp.path().join(".git/objects");
        let pack_dir = objects.join("pack");
        let midx_path = pack_dir.join("multi-pack-index");
        let mut data = std::fs::read(&midx_path).expect("midx bytes");
        let (_, hdr_end, _) = parse_midx_header(&data).expect("header");
        let (oidl_off, oidl_len) =
            find_chunk(&data, hdr_end, MIDX_CHUNKID_OIDLOOKUP).expect("oidl");
        let (ooff_off, _) = find_chunk(&data, hdr_end, MIDX_CHUNKID_OBJECTOFFSETS).expect("ooff");
        let hash_len = oid.as_bytes().len();
        let num_objects = oidl_len / hash_len;
        let needle = oid.as_bytes();
        let mut patched = false;
        for i in 0..num_objects {
            let start = oidl_off + i * hash_len;
            if data[start..start + hash_len] == *needle {
                let ob = ooff_off + i * 8 + 4;
                data[ob..ob + 4].copy_from_slice(&0x8000_0010u32.to_be_bytes());
                patched = true;
                break;
            }
        }
        assert!(patched, "oid row in MIDX");
        clear_pack_midx_state(&pack_dir).expect("evict midx cache");
        std::fs::write(&midx_path, &data).expect("write midx");
        crate::pack::clear_pack_cache();
        let (_pack_id, off) = midx_lookup_pack_and_offset_opt(&objects, &oid)
            .expect("lookup")
            .expect("listed");
        assert_eq!(off, 0x8000_0010);
    }

    #[test]
    fn read_after_in_process_pack_swap() {
        let Some((tmp, oid)) = midx_fixture(false) else {
            eprintln!("SKIP: git unavailable for MIDX fixture");
            return;
        };
        let objects = tmp.path().join(".git/objects");
        let before = read_object_from_packs(&objects, &oid).expect("read from pack");

        std::fs::write(tmp.path().join("f2.txt"), b"replacement pack").expect("write");
        git(tmp.path(), &["add", "f2.txt"]);
        git(tmp.path(), &["commit", "-q", "-m", "c2"]);
        git(tmp.path(), &["repack", "-adf"]);
        clear_pack_cache();

        let after = read_object_from_packs(&objects, &oid).expect("historical commit still packed");
        assert_eq!(before.kind, after.kind);
        assert_eq!(before.data, after.data);
    }
}

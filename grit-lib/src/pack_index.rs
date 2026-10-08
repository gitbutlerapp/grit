//! Zero-copy in-memory representation of Git pack index (`.idx`) files.

use crate::error::{Error, Result};
use crate::hash::verify_trailer;
use crate::objects::{HashAlgo, ObjectId};
use std::cmp::Ordering;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

type OffsetLookupCache = Arc<Mutex<Option<Vec<(u64, u32)>>>>;

/// Raw bytes backing a parsed pack index (mmap-friendly hook for later work).
#[derive(Debug, Clone)]
struct PackIndexStorage(Arc<[u8]>);

/// Version-1 (legacy) or version-2 pack index on-disk layout.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PackIndexVersion {
    V1,
    V2,
}

/// Byte offsets of tables inside [`PackIndexStorage`].
#[derive(Debug, Clone)]
struct PackIndexLayout {
    version: PackIndexVersion,
    hash_bytes: usize,
    count: usize,
    oid_base: usize,
    crc_base: usize,
    off32_base: usize,
    large_off_base: usize,
    #[allow(dead_code)]
    large_off_count: usize,
}

/// Parsed pack index with fanout-accelerated OID lookup and in-place table reads.
#[derive(Debug, Clone)]
pub struct PackIndex {
    /// Absolute path to the `.idx` file.
    pub idx_path: PathBuf,
    /// Absolute path to the `.pack` file.
    pub pack_path: PathBuf,
    storage: PackIndexStorage,
    layout: PackIndexLayout,
    /// 256-entry fanout: cumulative count of objects with first OID byte `<= b`.
    pub fanout: [u32; 256],
    /// Sibling `pack-*.promisor` marker was present when this index was prepared.
    pub is_promisor: bool,
    /// Sibling `pack-*.mtimes` marker was present when this index was prepared (cruft pack).
    pub is_cruft: bool,
    offset_lookup: OffsetLookupCache,
}

/// Borrowed view of one row in a pack index.
#[derive(Debug, Clone, Copy)]
pub struct PackIndexEntryRef<'a> {
    index: &'a PackIndex,
    position: usize,
}

impl<'a> PackIndexEntryRef<'a> {
    /// Object id bytes (`20` for SHA-1, `32` for SHA-256).
    #[must_use]
    pub fn oid(&self) -> &'a [u8] {
        self.index.oid_at(self.position)
    }

    /// Byte offset of the object in the corresponding `.pack`.
    #[must_use]
    pub fn offset(&self) -> u64 {
        self.index.offset_at(self.position)
    }

    /// CRC32 of packed entry bytes (v2 only).
    #[must_use]
    pub fn crc32(&self) -> Option<u32> {
        self.index.crc32_at(self.position)
    }
}

/// Owned pack index row (collect from [`PackIndex::iter`] when needed).
#[derive(Debug, Clone)]
pub struct PackIndexEntry {
    /// Raw object identifier (`20` bytes for SHA-1, `32` for SHA-256).
    pub oid: Vec<u8>,
    /// Byte offset of the object in the corresponding `.pack`.
    pub offset: u64,
    /// CRC32 of the raw packed entry bytes (header + payload), from the v2 index CRC table.
    pub crc32: Option<u32>,
}

impl From<PackIndexEntryRef<'_>> for PackIndexEntry {
    fn from(r: PackIndexEntryRef<'_>) -> Self {
        Self {
            oid: r.oid().to_vec(),
            offset: r.offset(),
            crc32: r.crc32(),
        }
    }
}

/// Iterator over index rows in OID sort order.
pub struct PackIndexIter<'a> {
    index: &'a PackIndex,
    next: usize,
}

impl<'a> Iterator for PackIndexIter<'a> {
    type Item = PackIndexEntryRef<'a>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.next >= self.index.len() {
            return None;
        }
        let pos = self.next;
        self.next += 1;
        Some(PackIndexEntryRef {
            index: self.index,
            position: pos,
        })
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        let rem = self.index.len().saturating_sub(self.next);
        (rem, Some(rem))
    }
}

impl ExactSizeIterator for PackIndexIter<'_> {}

impl PackIndex {
    /// Number of objects in this index.
    #[must_use]
    pub fn len(&self) -> usize {
        self.layout.count
    }

    /// Whether this index contains no objects.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.layout.count == 0
    }

    /// OID width in bytes (`20` for SHA-1, `32` for SHA-256).
    #[must_use]
    pub fn hash_bytes(&self) -> usize {
        self.layout.hash_bytes
    }

    /// OID bytes at sorted position `i`.
    #[must_use]
    pub fn oid_at(&self, i: usize) -> &[u8] {
        debug_assert!(i < self.layout.count);
        let start = match self.layout.version {
            PackIndexVersion::V1 => {
                let record = 4 + self.layout.hash_bytes;
                self.layout.oid_base + i * record + 4
            }
            PackIndexVersion::V2 => self.layout.oid_base + i * self.layout.hash_bytes,
        };
        let end = start + self.layout.hash_bytes;
        &self.storage.0[start..end]
    }

    /// Pack byte offset at sorted position `i`.
    #[must_use]
    pub fn offset_at(&self, i: usize) -> u64 {
        debug_assert!(i < self.layout.count);
        match self.layout.version {
            PackIndexVersion::V1 => {
                let start = self.layout.oid_base + i * (4 + self.layout.hash_bytes);
                read_u32_be_slice(&self.storage.0, start) as u64
            }
            PackIndexVersion::V2 => {
                let start = self.layout.off32_base + i * 4;
                let raw = read_u32_be_slice(&self.storage.0, start);
                if (raw & 0x8000_0000) == 0 {
                    u64::from(raw)
                } else {
                    let slot = (raw & 0x7fff_ffff) as usize;
                    let loff = self.layout.large_off_base + slot * 8;
                    read_u64_be_slice(&self.storage.0, loff)
                }
            }
        }
    }

    /// CRC32 at sorted position `i` (`None` for version-1 indexes).
    #[must_use]
    pub fn crc32_at(&self, i: usize) -> Option<u32> {
        debug_assert!(i < self.layout.count);
        if self.layout.version != PackIndexVersion::V2 {
            return None;
        }
        let start = self.layout.crc_base + i * 4;
        Some(read_u32_be_slice(&self.storage.0, start))
    }

    /// Sorted-table position for `oid`, or `None` when absent or hash width mismatches.
    #[must_use]
    pub fn find_position(&self, oid: &ObjectId) -> Option<usize> {
        let needle = oid.as_bytes();
        if needle.len() != self.layout.hash_bytes {
            return None;
        }
        let first = needle[0] as usize;
        let lo = if first == 0 {
            0
        } else {
            self.fanout[first - 1] as usize
        };
        let hi = self.fanout[first] as usize;
        if lo >= hi || hi > self.layout.count {
            return None;
        }
        self.find_position_in_bucket(lo, hi, needle)
    }

    /// Pack byte offset for `oid`, if present.
    #[must_use]
    pub fn find_offset(&self, oid: &ObjectId) -> Option<u64> {
        self.find_position(oid).map(|i| self.offset_at(i))
    }

    /// Whether this pack index contains the given OID.
    #[must_use]
    pub fn contains(&self, oid: &ObjectId) -> bool {
        self.find_offset(oid).is_some()
    }

    /// Iterate rows in OID sort order.
    pub fn iter(&self) -> PackIndexIter<'_> {
        PackIndexIter {
            index: self,
            next: 0,
        }
    }

    /// Smallest pack offset strictly greater than `entry_offset`, capped at `pack_end`.
    #[must_use]
    pub fn next_pack_offset_after(&self, entry_offset: u64, pack_end: u64) -> u64 {
        self.iter()
            .map(|e| e.offset())
            .filter(|&o| o > entry_offset && o <= pack_end)
            .min()
            .unwrap_or(pack_end)
    }

    /// CRC32 for the index row at `entry_offset`, if any.
    #[must_use]
    pub fn crc32_for_pack_offset(&self, entry_offset: u64) -> Option<u32> {
        self.find_position_by_offset_sorted(entry_offset)
            .and_then(|i| self.crc32_at(i))
    }

    /// Index row position for a pack byte offset via a sorted offset table.
    ///
    /// Builds and caches `(offset, position)` pairs on first use. Callers that
    /// have a `.rev` sidecar should try that first (see `pack::find_position_by_pack_offset`).
    #[must_use]
    /// Copy with promisor/cruft sidecar flags refreshed from disk markers.
    pub(crate) fn with_sidecar_flags(&self, is_promisor: bool, is_cruft: bool) -> Self {
        let mut next = self.clone();
        next.is_promisor = is_promisor;
        next.is_cruft = is_cruft;
        next
    }

    pub fn find_position_by_offset_sorted(&self, offset: u64) -> Option<usize> {
        let mut guard = self.offset_lookup.lock().unwrap_or_else(|e| e.into_inner());
        if guard.is_none() {
            let mut pairs: Vec<(u64, u32)> = (0..self.len())
                .map(|i| (self.offset_at(i), i as u32))
                .collect();
            pairs.sort_by_key(|p| p.0);
            *guard = Some(pairs);
        }
        let pairs = guard.as_ref()?;
        let idx = pairs.binary_search_by_key(&offset, |p| p.0).ok()?;
        Some(pairs[idx].1 as usize)
    }

    fn find_position_in_bucket(&self, lo: usize, hi: usize, needle: &[u8]) -> Option<usize> {
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
                Ordering::Less => lo = mid + 1,
                Ordering::Greater => hi = mid,
                Ordering::Equal => return Some(mid),
            }
        }
        None
    }

    fn interpolation_search(&self, lo: usize, hi: usize, needle: &[u8]) -> Option<usize> {
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
                Ordering::Equal => return Some(pos),
                Ordering::Less => lo = pos + 1,
                Ordering::Greater => hi = pos,
            }
        }
        None
    }
}

/// Leading OID bytes for interpolation probing (8 bytes — fits in `u64` without overflow).
const OID_INTERP_PREFIX_LEN: usize = 8;

pub(crate) fn oid_interp_prefix_u64(oid: &[u8]) -> u64 {
    let take = oid.len().min(OID_INTERP_PREFIX_LEN);
    let mut v = 0u64;
    for &b in &oid[..take] {
        v = (v << 8) | u64::from(b);
    }
    for _ in take..OID_INTERP_PREFIX_LEN {
        v <<= 8;
    }
    v
}

/// True when `entry` stores a SHA-1 OID matching `oid` (SHA-256 pack entries are ignored).
#[must_use]
pub fn pack_index_entry_matches_sha1_oid(entry: &PackIndexEntryRef<'_>, oid: &ObjectId) -> bool {
    entry.oid().len() == 20 && entry.oid() == oid.as_bytes()
}

/// Compute fanout from a sorted OID list (used when writing v1 indexes in tests).
#[must_use]
pub fn compute_fanout_from_oid_slices(entries: &[&[u8]]) -> [u32; 256] {
    let mut fanout = [0u32; 256];
    let mut idx = 0usize;
    for byte in 0u32..256 {
        let needle = byte as u8;
        while idx < entries.len() && entries[idx].first().copied().unwrap_or(0) <= needle {
            idx += 1;
        }
        fanout[byte as usize] = u32::try_from(idx).unwrap_or(u32::MAX);
    }
    fanout
}

/// Compute the 256-entry fanout from owned entries (legacy helper for tests).
#[must_use]
pub fn compute_fanout_from_entries(entries: &[PackIndexEntry]) -> [u32; 256] {
    let slices: Vec<&[u8]> = entries.iter().map(|e| e.oid.as_slice()).collect();
    compute_fanout_from_oid_slices(&slices)
}

/// Parse pack index bytes (v1 or v2). When `verify` is true, check the trailing checksum.
pub fn parse_pack_index_bytes(idx_path: &Path, bytes: Vec<u8>, verify: bool) -> Result<PackIndex> {
    if bytes.len() < 8 {
        return Err(Error::CorruptObject(format!(
            "index file {} is too small",
            idx_path.display()
        )));
    }
    let storage = PackIndexStorage(Arc::from(bytes));
    let magic = &storage.0[0..4];
    if magic == [0xff, b't', b'O', b'c'] {
        read_pack_index_v2(idx_path, &storage, verify)
    } else {
        read_pack_index_v1(idx_path, &storage, verify)
    }
}

fn read_pack_index_v1(
    idx_path: &Path,
    storage: &PackIndexStorage,
    verify: bool,
) -> Result<PackIndex> {
    let bytes = &storage.0;
    let mut pos = 0usize;
    if bytes.len() < 256 * 4 + 20 {
        return Err(Error::CorruptObject(format!(
            "index file {} is too small",
            idx_path.display()
        )));
    }
    let mut fanout = [0u32; 256];
    for slot in &mut fanout {
        *slot = read_u32_be(bytes, &mut pos)?;
    }
    check_fanout_monotonic(&fanout, idx_path)?;
    let object_count = fanout[255] as usize;
    let record_size = 4 + 20;
    let need = pos
        .saturating_add(object_count.saturating_mul(record_size))
        .saturating_add(20);
    if bytes.len() < need {
        return Err(Error::CorruptObject(format!(
            "truncated idx file {}",
            idx_path.display()
        )));
    }
    let oid_base = pos;
    for i in 0..object_count {
        let off_start = oid_base + i * record_size;
        let oid_start = off_start + 4;
        if i > 0 {
            let prev_start = oid_base + (i - 1) * record_size + 4;
            if bytes[prev_start..prev_start + 20] >= bytes[oid_start..oid_start + 20] {
                return Err(Error::CorruptObject(format!(
                    "oid lookup out of order in {}",
                    idx_path.display()
                )));
            }
        }
    }
    if verify {
        verify_idx_trailing_checksum(idx_path, bytes, 20)?;
    }
    let mut pack_path = idx_path.to_path_buf();
    pack_path.set_extension("pack");
    Ok(PackIndex {
        idx_path: idx_path.to_path_buf(),
        pack_path,
        storage: storage.clone(),
        layout: PackIndexLayout {
            version: PackIndexVersion::V1,
            hash_bytes: 20,
            count: object_count,
            oid_base,
            crc_base: 0,
            off32_base: 0,
            large_off_base: 0,
            large_off_count: 0,
        },
        fanout,
        is_promisor: false,
        is_cruft: false,
        offset_lookup: Arc::new(Mutex::new(None)),
    })
}

fn read_pack_index_v2(
    idx_path: &Path,
    storage: &PackIndexStorage,
    verify: bool,
) -> Result<PackIndex> {
    let bytes = &storage.0;
    if bytes.len() < 8 + 256 * 4 + 40 {
        return Err(Error::CorruptObject(format!(
            "index file {} is too small",
            idx_path.display()
        )));
    }
    let mut pos = 4usize;
    let version = read_u32_be(bytes, &mut pos)?;
    if version != 2 {
        return Err(Error::CorruptObject(format!(
            "unsupported idx version {version} in {}",
            idx_path.display()
        )));
    }
    let mut fanout = [0u32; 256];
    for slot in &mut fanout {
        *slot = read_u32_be(bytes, &mut pos)?;
    }
    check_fanout_monotonic(&fanout, idx_path)?;
    let object_count = fanout[255] as usize;
    let idx_file_len = bytes.len();
    let hash_bytes = detect_idx_hash_bytes_v2(idx_file_len, pos, object_count, idx_path)?;
    let oid_base = pos;
    let crc_base = oid_base + object_count * hash_bytes;
    let off32_base = crc_base + object_count * 4;
    let need = off32_base + object_count * 4;
    if bytes.len() < need {
        return Err(Error::CorruptObject(format!(
            "truncated idx file {}",
            idx_path.display()
        )));
    }
    pos = off32_base;
    let mut large_off_count = 0usize;
    let mut next_large = 0usize;
    for i in 0..object_count {
        let v = read_u32_be(bytes, &mut pos)?;
        if (v & 0x8000_0000) != 0 {
            let slot = (v & 0x7fff_ffff) as usize;
            if slot != next_large {
                return Err(Error::CorruptObject(format!(
                    "inconsistent 64b offset index at entry {i} in {}",
                    idx_path.display()
                )));
            }
            next_large += 1;
            large_off_count += 1;
        }
    }
    let large_off_base = off32_base + object_count * 4;
    if bytes.len() < large_off_base + large_off_count * 8 + 2 * hash_bytes {
        return Err(Error::CorruptObject(format!(
            "truncated large offset table in {}",
            idx_path.display()
        )));
    }
    for i in 0..object_count {
        let start = oid_base + i * hash_bytes;
        if i > 0 {
            let prev = oid_base + (i - 1) * hash_bytes;
            if bytes[prev..prev + hash_bytes] >= bytes[start..start + hash_bytes] {
                return Err(Error::CorruptObject(format!(
                    "oid lookup out of order in {}",
                    idx_path.display()
                )));
            }
        }
    }
    if verify {
        verify_idx_trailing_checksum(idx_path, bytes, hash_bytes)?;
    }
    let mut pack_path = idx_path.to_path_buf();
    pack_path.set_extension("pack");
    Ok(PackIndex {
        idx_path: idx_path.to_path_buf(),
        pack_path,
        storage: storage.clone(),
        layout: PackIndexLayout {
            version: PackIndexVersion::V2,
            hash_bytes,
            count: object_count,
            oid_base,
            crc_base,
            off32_base,
            large_off_base,
            large_off_count,
        },
        fanout,
        is_promisor: false,
        is_cruft: false,
        offset_lookup: Arc::new(Mutex::new(None)),
    })
}

fn verify_idx_trailing_checksum(idx_path: &Path, bytes: &[u8], hash_bytes: usize) -> Result<()> {
    let Some(algo) = HashAlgo::from_len(hash_bytes) else {
        return Err(Error::CorruptObject(format!(
            "unsupported index hash width {hash_bytes}"
        )));
    };
    verify_trailer(algo, bytes).map_err(|e| {
        Error::CorruptObject(format!(
            "index checksum mismatch for {}: {e}",
            idx_path.display()
        ))
    })
}

fn check_fanout_monotonic(fanout: &[u32; 256], idx_path: &Path) -> Result<()> {
    let mut prev = 0u32;
    for &n in fanout {
        if n < prev {
            return Err(Error::CorruptObject(format!(
                "non-monotonic index {}",
                idx_path.display()
            )));
        }
        prev = n;
    }
    Ok(())
}

fn detect_idx_hash_bytes_v2(
    idx_file_len: usize,
    fanout_end: usize,
    object_count: usize,
    idx_path: &Path,
) -> Result<usize> {
    if object_count == 0 {
        let sha1_len = fanout_end.saturating_add(2 * 20);
        let sha256_len = fanout_end.saturating_add(2 * 32);
        return match idx_file_len {
            n if n == sha1_len => Ok(20),
            n if n == sha256_len => Ok(32),
            _ => Err(Error::CorruptObject(format!(
                "wrong index v2 file size in {}",
                idx_path.display()
            ))),
        };
    }
    for &hb in &[20usize, 32] {
        let fixed = fanout_end
            .saturating_add(object_count.saturating_mul(hb + 4 + 4))
            .saturating_add(2 * hb);
        if idx_file_len < fixed {
            continue;
        }
        let extra = idx_file_len - fixed;
        if !extra.is_multiple_of(8) {
            continue;
        }
        if extra / 8 > object_count {
            continue;
        }
        return Ok(hb);
    }
    Err(Error::CorruptObject(format!(
        "wrong index v2 file size in {}",
        idx_path.display()
    )))
}

fn read_u32_be_slice(bytes: &[u8], start: usize) -> u32 {
    u32::from_be_bytes([
        bytes[start],
        bytes[start + 1],
        bytes[start + 2],
        bytes[start + 3],
    ])
}

fn read_u64_be_slice(bytes: &[u8], start: usize) -> u64 {
    u64::from_be_bytes([
        bytes[start],
        bytes[start + 1],
        bytes[start + 2],
        bytes[start + 3],
        bytes[start + 4],
        bytes[start + 5],
        bytes[start + 6],
        bytes[start + 7],
    ])
}

fn read_u32_be(bytes: &[u8], pos: &mut usize) -> Result<u32> {
    if bytes.len() < *pos + 4 {
        return Err(Error::CorruptObject(
            "unexpected end of idx while reading u32".to_owned(),
        ));
    }
    let v = u32::from_be_bytes(
        bytes[*pos..*pos + 4]
            .try_into()
            .map_err(|_| Error::CorruptObject("failed to parse u32".to_owned()))?,
    );
    *pos += 4;
    Ok(v)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::objects::ObjectId;

    #[test]
    fn pack_index_api_coverage_smoke() {
        let oid_a = vec![0x01u8; 20];
        let oid_b = vec![0x02u8; 20];
        let fanout = compute_fanout_from_oid_slices(&[&oid_a, &oid_b]);
        assert_eq!(fanout[1], 1);
        assert_eq!(fanout[2], 2);
    }

    #[test]
    fn find_position_empty_bucket() {
        let entries = vec![PackIndexEntry {
            oid: vec![0x05; 20],
            offset: 1,
            crc32: None,
        }];
        let fanout = compute_fanout_from_entries(&entries);
        let idx = PackIndex {
            idx_path: PathBuf::from("t.idx"),
            pack_path: PathBuf::from("t.pack"),
            storage: PackIndexStorage(Arc::from(Vec::<u8>::new())),
            layout: PackIndexLayout {
                version: PackIndexVersion::V1,
                hash_bytes: 20,
                count: 0,
                oid_base: 0,
                crc_base: 0,
                off32_base: 0,
                large_off_base: 0,
                large_off_count: 0,
            },
            fanout,
            is_promisor: false,
            is_cruft: false,
            offset_lookup: Arc::new(Mutex::new(None)),
        };
        let miss = ObjectId::from_bytes(&[0x06; 20]).unwrap();
        assert!(idx.find_position(&miss).is_none());
        let _ = idx.len();
        let _ = idx.is_empty();
        let _ = idx.hash_bytes();
        let _ = idx.iter().next();
    }

    fn build_v1_idx_bytes(entries: &[(Vec<u8>, u64)]) -> Vec<u8> {
        let mut sorted = entries.to_vec();
        sorted.sort_by(|a, b| a.0.cmp(&b.0));
        let slices: Vec<&[u8]> = sorted.iter().map(|(o, _)| o.as_slice()).collect();
        let fanout = compute_fanout_from_oid_slices(&slices);
        let mut body = Vec::new();
        for f in fanout {
            body.extend_from_slice(&f.to_be_bytes());
        }
        for (oid, off) in sorted {
            body.extend_from_slice(&(off as u32).to_be_bytes());
            body.extend_from_slice(&oid);
        }
        body.extend_from_slice(crate::objects::HashAlgo::Sha1.digest(&body).as_bytes());
        body
    }

    #[test]
    fn find_position_large_fanout_bucket_sha1_v1_no_panic() {
        let mut rows = Vec::new();
        for i in 0..64u8 {
            let mut oid = vec![0x05u8; 20];
            oid[19] = i;
            rows.push((oid, u64::from(i) * 12 + 100));
        }
        let body = build_v1_idx_bytes(&rows);
        let idx =
            parse_pack_index_bytes(Path::new("big-v1.idx"), body, false).expect("parse v1 idx");
        assert_eq!(idx.len(), 64);
        for (oid, _) in &rows {
            let id = ObjectId::from_bytes(oid).expect("oid");
            assert!(
                idx.find_position(&id).is_some(),
                "missing {}",
                hex::encode(oid)
            );
        }
        let miss = ObjectId::from_bytes(&[0x06; 20]).unwrap();
        assert!(idx.find_position(&miss).is_none());
    }

    #[test]
    fn detect_empty_v2_index_hash_width_from_file_size() {
        let fanout_end = 8 + 256 * 4;
        assert_eq!(
            detect_idx_hash_bytes_v2(1072, fanout_end, 0, Path::new("t.idx")).expect("sha1"),
            20
        );
        assert_eq!(
            detect_idx_hash_bytes_v2(1096, fanout_end, 0, Path::new("t.idx")).expect("sha256"),
            32
        );
        assert!(detect_idx_hash_bytes_v2(1073, fanout_end, 0, Path::new("t.idx")).is_err());
    }

    #[test]
    fn find_position_large_fanout_bucket_sha256_v2_no_panic() {
        use crate::pack::write_v2_pack_index_with_trailer;
        let mut rows: Vec<(ObjectId, u64, u32)> = Vec::new();
        for i in 0..64u8 {
            let mut bytes = [0u8; 32];
            bytes[0] = 0x07;
            bytes[31] = i;
            let oid = ObjectId::from_bytes(&bytes).expect("oid");
            rows.push((oid, u64::from(i) * 8 + 512, 0));
        }
        let tmp = tempfile::tempdir().expect("tempdir");
        let idx_path = tmp.path().join("big-v2.idx");
        write_v2_pack_index_with_trailer(&idx_path, &rows, &[0u8; 32], 32).expect("write v2");
        let bytes = std::fs::read(&idx_path).expect("read idx");
        let idx = parse_pack_index_bytes(&idx_path, bytes, false).expect("parse v2 idx");
        assert_eq!(idx.len(), 64);
        assert_eq!(idx.hash_bytes(), 32);
        for (oid, _, _) in &rows {
            assert!(idx.find_position(oid).is_some(), "missing {}", oid.to_hex());
        }
    }
}

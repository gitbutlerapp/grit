//! Read-only pack-file object backend ([`PackedObjects`]).
//!
//! All pack cache and index reads run under the owning [`PackStore`](crate::pack_store::PackStore)
//! context via [`PackStore::with_context`](crate::pack_store::PackStore::with_context).

use std::collections::HashSet;
use std::io::{self, Cursor, Read};
use std::ops::ControlFlow;
use std::path::Path;
use std::sync::Arc;

use flate2::read::ZlibDecoder;

use crate::error::{Error, Result};
use crate::objects::{HashAlgo, Object, ObjectId, ObjectInfo, ObjectKind};
use crate::pack::{
    self, read_local_pack_indexes_cached, reprepare_pack_directory_on_miss, PackedType,
};
use crate::pack_index::PackIndex;
use crate::pack_store::PackStore;

use super::{ObjectStore, ObjectStream};

/// Which local pack indexes participate in filtered membership checks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PackFilter {
    /// When false, skip indexes with a sibling `.promisor` marker.
    pub include_promisor: bool,
    /// When false, skip indexes with a sibling `.mtimes` (cruft) marker.
    pub include_cruft: bool,
}

impl Default for PackFilter {
    fn default() -> Self {
        Self {
            include_promisor: false,
            include_cruft: true,
        }
    }
}

/// Pack-index-backed read-only [`ObjectStore`] for one `objects/` directory.
pub struct PackedObjects {
    store: Arc<PackStore>,
}

impl std::fmt::Debug for PackedObjects {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PackedObjects")
            .field("objects_dir", &self.store.objects_dir())
            .finish()
    }
}

impl PackedObjects {
    /// Open pack reads for `store`'s `objects/` directory.
    #[must_use]
    pub fn new(store: Arc<PackStore>) -> Self {
        Self { store }
    }

    /// Shared pack cache handle (same as used for reads).
    #[must_use]
    pub fn pack_store(&self) -> &Arc<PackStore> {
        &self.store
    }

    /// Whether `oid` appears in any pack index matching `filter`.
    ///
    /// Used for local materialization checks (promisor/cruft exclusion); not part of
    /// [`ObjectStore::contains`], which ignores promisor packs.
    ///
    /// # Errors
    ///
    /// Propagates pack directory enumeration failures.
    pub fn contains_filtered(&self, oid: &ObjectId, filter: PackFilter) -> Result<bool> {
        self.with_pack(|objects_dir| {
            for idx in filtered_indexes(objects_dir, filter)? {
                if idx.contains(oid) {
                    return Ok(true);
                }
            }
            Ok(false)
        })
    }

    /// Whether a pack or MIDX reachability bitmap (`*.bitmap`) is present under `objects/pack/`.
    ///
    /// # Errors
    ///
    /// Propagates pack directory read failures other than a missing directory.
    pub fn reachability_bitmap_present(&self) -> Result<bool> {
        self.with_pack(pack_dir_has_bitmap_extension)
    }

    /// Invoke `f` for each object id stored in a promisor-marked pack index.
    ///
    /// # Errors
    ///
    /// Propagates pack index enumeration failures.
    pub fn for_each_promisor_object(
        &self,
        f: &mut dyn FnMut(&ObjectId) -> ControlFlow<()>,
    ) -> Result<()> {
        self.with_pack(|objects_dir| {
            let indexes = read_local_pack_indexes_cached(objects_dir)?;
            let mut seen = HashSet::new();
            for idx in indexes {
                if !idx.is_promisor {
                    continue;
                }
                for entry in idx.iter() {
                    let Ok(oid) = ObjectId::from_bytes(entry.oid()) else {
                        continue;
                    };
                    if !seen.insert(oid) {
                        continue;
                    }
                    if f(&oid).is_break() {
                        return Ok(());
                    }
                }
            }
            Ok(())
        })
    }

    /// Collect object ids from pack indexes that have a sibling `.keep` file.
    ///
    /// # Errors
    ///
    /// Propagates pack directory or index read failures.
    pub fn kept_pack_object_ids(&self) -> Result<HashSet<ObjectId>> {
        self.with_pack(collect_kept_pack_object_ids)
    }

    /// Touch the mtime of the pack file holding `oid` (Git pack freshen), when not cruft.
    ///
    /// # Errors
    ///
    /// Propagates I/O failures from pack listing or utime.
    pub fn freshen_pack_containing(&self, oid: &ObjectId) -> Result<bool> {
        self.with_pack(|objects_dir| {
            let indexes = read_local_pack_indexes_cached(objects_dir)?;
            for idx in &indexes {
                if !idx.contains(oid) {
                    continue;
                }
                if idx.is_cruft {
                    return Ok(false);
                }
                return Ok(touch_pack_mtime(&idx.pack_path));
            }
            Ok(false)
        })
    }

    fn with_pack<R>(&self, f: impl FnOnce(&std::path::Path) -> Result<R>) -> Result<R> {
        PackStore::with_context(Arc::clone(&self.store), || f(self.store.objects_dir()))
    }

    fn hash_algo_from_indexes(&self) -> HashAlgo {
        self.with_pack(|objects_dir| {
            let Ok(indexes) = read_local_pack_indexes_cached(objects_dir) else {
                return Ok(HashAlgo::Sha1);
            };
            Ok(indexes
                .first()
                .and_then(|idx| HashAlgo::from_len(idx.hash_bytes()))
                .unwrap_or(HashAlgo::Sha1))
        })
        .unwrap_or(HashAlgo::Sha1)
    }

    fn read_from_packs(&self, oid: &ObjectId) -> Result<Option<Object>> {
        self.with_pack(
            |objects_dir| match pack::read_object_from_packs(objects_dir, oid) {
                Ok(obj) => Ok(Some(obj)),
                Err(Error::ObjectNotFound(_)) => Ok(None),
                Err(err) => Err(err),
            },
        )
    }

    fn read_info_from_packs(&self, oid: &ObjectId) -> Result<Option<ObjectInfo>> {
        self.with_pack(
            |objects_dir| match pack::read_object_info_from_packs(objects_dir, oid) {
                Ok(info) => Ok(Some(info)),
                Err(Error::ObjectNotFound(_)) => Ok(None),
                Err(err) => Err(err),
            },
        )
    }
}

impl ObjectStore for PackedObjects {
    fn hash_algo(&self) -> HashAlgo {
        self.hash_algo_from_indexes()
    }

    fn read(&self, oid: &ObjectId) -> Result<Option<Object>> {
        self.read_from_packs(oid)
    }

    fn read_info(&self, oid: &ObjectId) -> Result<Option<ObjectInfo>> {
        self.read_info_from_packs(oid)
    }

    fn open_stream(&self, oid: &ObjectId) -> Result<Option<ObjectStream<'_>>> {
        self.with_pack(|dir| {
            let Some(entry) = pack::packed_full_object_slice(dir, oid)? else {
                return stream_from_buffered_read(self, oid);
            };
            let mut pos = 0usize;
            let (packed_type, size) = parse_pack_object_header(&entry, &mut pos)?;
            if matches!(packed_type, PackedType::OfsDelta | PackedType::RefDelta) {
                return stream_from_buffered_read(self, oid);
            }
            let kind = packed_type_to_kind(packed_type)?;
            let zlib = entry[pos..].to_vec();
            let decoder = ZlibDecoder::new(Cursor::new(zlib));
            Ok(Some(ObjectStream {
                kind,
                size,
                reader: Box::new(PackPayloadStream {
                    decoder,
                    remaining: size,
                }),
            }))
        })
    }

    fn for_each_object(&self, f: &mut dyn FnMut(&ObjectId) -> ControlFlow<()>) -> Result<()> {
        self.with_pack(|objects_dir| {
            let indexes = read_local_pack_indexes_cached(objects_dir)?;
            let mut seen = HashSet::new();
            for idx in &indexes {
                for entry in idx.iter() {
                    let Ok(oid) = ObjectId::from_bytes(entry.oid()) else {
                        continue;
                    };
                    if !seen.insert(oid) {
                        continue;
                    }
                    if f(&oid).is_break() {
                        return Ok(());
                    }
                }
            }
            Ok(())
        })
    }

    fn lookup_prefix(&self, prefix: &str, limit: usize, out: &mut Vec<ObjectId>) -> Result<()> {
        let prefix = super::normalize_oid_prefix(prefix, self.hash_algo())?;
        self.with_pack(|objects_dir| {
            let indexes = read_local_pack_indexes_cached(objects_dir)?;
            let mut seen = HashSet::new();
            let mut count = 0usize;
            for idx in &indexes {
                collect_prefix_from_index(idx, prefix.as_str(), limit, &mut count, out, &mut seen)?;
                if limit != 0 && count >= limit {
                    break;
                }
            }
            Ok(())
        })
    }

    fn refresh(&self) -> Result<bool> {
        self.with_pack(reprepare_pack_directory_on_miss)
    }
}

fn pack_dir_has_bitmap_extension(objects_dir: &Path) -> Result<bool> {
    let pack_dir = objects_dir.join("pack");
    let Ok(rd) = std::fs::read_dir(&pack_dir) else {
        return Ok(false);
    };
    Ok(rd.filter_map(|e| e.ok()).any(|e| {
        e.path()
            .extension()
            .and_then(|s| s.to_str())
            .is_some_and(|ext| ext == "bitmap")
    }))
}

fn collect_kept_pack_object_ids(objects_dir: &Path) -> Result<HashSet<ObjectId>> {
    let pack_dir = objects_dir.join("pack");
    let mut kept = HashSet::new();
    if !pack_dir.is_dir() {
        return Ok(kept);
    }
    for entry in std::fs::read_dir(&pack_dir).map_err(Error::Io)? {
        let entry = entry.map_err(Error::Io)?;
        let path = entry.path();
        if path.extension().is_some_and(|ext| ext == "keep") {
            let idx_path = path.with_extension("idx");
            if idx_path.is_file() {
                if let Ok(oids) = pack::read_idx_object_ids(&idx_path) {
                    kept.extend(oids);
                }
            }
        }
    }
    Ok(kept)
}

fn filtered_indexes(
    objects_dir: &std::path::Path,
    filter: PackFilter,
) -> Result<Vec<Arc<PackIndex>>> {
    let indexes = read_local_pack_indexes_cached(objects_dir)?;
    Ok(indexes
        .into_iter()
        .filter(|idx| {
            (filter.include_promisor || !idx.is_promisor) && (filter.include_cruft || !idx.is_cruft)
        })
        .collect())
}

fn collect_prefix_from_index(
    idx: &PackIndex,
    prefix: &str,
    limit: usize,
    count: &mut usize,
    out: &mut Vec<ObjectId>,
    seen: &mut HashSet<ObjectId>,
) -> Result<()> {
    if prefix.is_empty() {
        for entry in idx.iter() {
            let Ok(oid) = ObjectId::from_bytes(entry.oid()) else {
                continue;
            };
            if !seen.insert(oid) {
                continue;
            }
            out.push(oid);
            *count += 1;
            if limit != 0 && *count >= limit {
                return Ok(());
            }
        }
        return Ok(());
    }

    let hb = idx.hash_bytes();
    let (min_oid, max_oid) = oid_prefix_bounds(prefix, hb)?;
    let lo = lower_bound_oid(idx, min_oid.as_bytes());
    let hi = upper_bound_oid(idx, max_oid.as_bytes());
    for i in lo..hi {
        let Ok(oid) = ObjectId::from_bytes(idx.oid_at(i)) else {
            continue;
        };
        if !oid_hex_has_prefix(&oid, prefix) {
            continue;
        }
        if !seen.insert(oid) {
            continue;
        }
        out.push(oid);
        *count += 1;
        if limit != 0 && *count >= limit {
            break;
        }
    }
    Ok(())
}

fn oid_prefix_bounds(prefix: &str, hash_bytes: usize) -> Result<(ObjectId, ObjectId)> {
    let hex_len = hash_bytes * 2;
    if prefix.len() > hex_len {
        return Err(Error::InvalidObjectId(prefix.to_owned()));
    }
    let pad = hex_len - prefix.len();
    let min_hex = format!("{prefix}{:0>pad$}", "", pad = pad);
    let max_hex = format!("{prefix}{}", "f".repeat(pad));
    let min = ObjectId::from_hex(&min_hex)?;
    let max = ObjectId::from_hex(&max_hex)?;
    Ok((min, max))
}

fn lower_bound_oid(idx: &PackIndex, needle: &[u8]) -> usize {
    let mut lo = 0usize;
    let mut hi = idx.len();
    while lo < hi {
        let mid = lo + (hi - lo) / 2;
        if idx.oid_at(mid) < needle {
            lo = mid + 1;
        } else {
            hi = mid;
        }
    }
    lo
}

fn upper_bound_oid(idx: &PackIndex, needle: &[u8]) -> usize {
    let mut lo = 0usize;
    let mut hi = idx.len();
    while lo < hi {
        let mid = lo + (hi - lo) / 2;
        if idx.oid_at(mid) <= needle {
            lo = mid + 1;
        } else {
            hi = mid;
        }
    }
    lo
}

fn oid_hex_has_prefix(oid: &ObjectId, prefix: &str) -> bool {
    if prefix.is_empty() {
        return true;
    }
    oid.to_hex().starts_with(prefix)
}

fn parse_pack_object_header(bytes: &[u8], pos: &mut usize) -> Result<(PackedType, u64)> {
    let first = *bytes.get(*pos).ok_or_else(|| {
        Error::CorruptObject("unexpected end of pack header while decoding object".to_owned())
    })?;
    *pos += 1;

    let type_code = (first >> 4) & 0x7;
    let mut size = (first & 0x0f) as u64;
    let mut shift = 4u32;
    let mut c = first;
    while (c & 0x80) != 0 {
        c = *bytes.get(*pos).ok_or_else(|| {
            Error::CorruptObject("unexpected end of variable size header".to_owned())
        })?;
        *pos += 1;
        size |= ((c & 0x7f) as u64) << shift;
        shift += 7;
    }

    let packed_type = match type_code {
        1 => PackedType::Commit,
        2 => PackedType::Tree,
        3 => PackedType::Blob,
        4 => PackedType::Tag,
        6 => PackedType::OfsDelta,
        7 => PackedType::RefDelta,
        _ => {
            return Err(Error::CorruptObject(format!(
                "unsupported packed object type {type_code}"
            )));
        }
    };
    Ok((packed_type, size))
}

fn packed_type_to_kind(packed_type: PackedType) -> Result<ObjectKind> {
    match packed_type {
        PackedType::Commit => Ok(ObjectKind::Commit),
        PackedType::Tree => Ok(ObjectKind::Tree),
        PackedType::Blob => Ok(ObjectKind::Blob),
        PackedType::Tag => Ok(ObjectKind::Tag),
        PackedType::OfsDelta | PackedType::RefDelta => Err(Error::CorruptObject(
            "delta object passed to undeltified stream path".to_owned(),
        )),
    }
}

fn touch_pack_mtime(pack_path: &std::path::Path) -> bool {
    let now = filetime::FileTime::now();
    if filetime::set_file_times(pack_path, now, now).is_err() {
        return false;
    }
    let touched = std::time::UNIX_EPOCH
        + std::time::Duration::new(now.unix_seconds() as u64, now.nanoseconds());
    pack::refresh_pack_bytes_signature(pack_path, touched);
    true
}

struct PackPayloadStream {
    decoder: ZlibDecoder<Cursor<Vec<u8>>>,
    remaining: u64,
}

impl Read for PackPayloadStream {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if self.remaining == 0 {
            return Ok(0);
        }
        let max = buf.len().min(self.remaining as usize);
        let n = self.decoder.read(&mut buf[..max])?;
        self.remaining -= n as u64;
        Ok(n)
    }
}

fn stream_from_buffered_read(
    store: &PackedObjects,
    oid: &ObjectId,
) -> Result<Option<ObjectStream<'static>>> {
    let Some(obj) = store.read(oid)? else {
        return Ok(None);
    };
    let size = u64::try_from(obj.data.len())
        .map_err(|_| Error::CorruptObject(format!("object size overflow for {}", oid.to_hex())))?;
    Ok(Some(ObjectStream {
        kind: obj.kind,
        size,
        reader: Box::new(Cursor::new(obj.data)),
    }))
}

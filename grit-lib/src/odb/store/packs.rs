//! Read-only pack-file object backend ([`PackedObjects`]).
//!
//! All pack cache and index reads run under the owning [`PackStore`](crate::pack_store::PackStore)
//! context via [`PackStore::with_context`](crate::pack_store::PackStore::with_context).

use std::collections::HashSet;
use std::io::{self, Cursor, Read};
use std::ops::ControlFlow;
use std::path::Path;
use std::sync::Arc;

use flate2::{Decompress, FlushDecompress, Status};

use crate::error::{Error, Result};
use crate::objects::{HashAlgo, Object, ObjectId, ObjectIdPrefix, ObjectInfo, ObjectKind};
use crate::pack::{self, read_local_pack_indexes_cached, reprepare_pack_directory_on_miss};
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

    /// Object ids listed in a pack index file (for ingest paths outside the live ODB cache).
    ///
    /// # Errors
    ///
    /// Propagates pack index parse/verify failures from [`pack::read_pack_index`].
    pub fn object_ids_from_index_path(idx_path: &Path) -> Result<HashSet<ObjectId>> {
        let idx = pack::read_pack_index(idx_path)?;
        let mut out = HashSet::new();
        for entry in idx.iter() {
            if let Ok(oid) = ObjectId::from_bytes(entry.oid()) {
                out.insert(oid);
            }
        }
        Ok(out)
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
                if idx.contains(oid)? {
                    return Ok(true);
                }
            }
            Ok(false)
        })
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

    /// Invoke `f` for each object id in a local pack index without a sibling `.keep`
    /// or `.promisor` file.
    ///
    /// Used by repack when retaining unreachable storage from packs that will be removed.
    ///
    /// # Errors
    ///
    /// Propagates pack index enumeration failures.
    pub fn for_each_removable_pack_object(
        &self,
        f: &mut dyn FnMut(&ObjectId) -> ControlFlow<()>,
    ) -> Result<()> {
        self.with_pack(|objects_dir| for_each_removable_pack_object_ids(objects_dir, f))
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
                if !idx.contains(oid)? {
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
        if PackStore::read_context_matches_objects_dir(self.store.objects_dir()) {
            return f(self.store.objects_dir());
        }
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

    fn read_from_packs_into(
        &self,
        oid: &ObjectId,
        buf: &mut Vec<u8>,
    ) -> Result<Option<ObjectKind>> {
        self.with_pack(|objects_dir| {
            match pack::read_object_from_packs_into(objects_dir, oid, buf) {
                Ok(kind) => Ok(Some(kind)),
                Err(Error::ObjectNotFound(_)) => Ok(None),
                Err(err) => Err(err),
            }
        })
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

    fn read_into(&self, oid: &ObjectId, buf: &mut Vec<u8>) -> Result<Option<ObjectKind>> {
        self.read_from_packs_into(oid, buf)
    }

    fn read_info(&self, oid: &ObjectId) -> Result<Option<ObjectInfo>> {
        self.read_info_from_packs(oid)
    }

    fn open_stream(&self, oid: &ObjectId) -> Result<Option<ObjectStream<'_>>> {
        self.with_pack(|dir| {
            let mut entry = pack::locate_packed_whole_object(dir, oid, true)?;
            if matches!(entry, pack::PackedEntry::Absent) && reprepare_pack_directory_on_miss(dir)?
            {
                entry = pack::locate_packed_whole_object(dir, oid, true)?;
            }
            let pack::PackedEntry::Whole(whole) = entry else {
                return stream_from_buffered_read(self, oid);
            };
            Ok(Some(ObjectStream {
                kind: whole.kind,
                size: whole.size,
                reader: Box::new(PackPayloadStream::new(whole)),
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
        if prefix.len() > self.hash_algo().hex_len() {
            return Ok(());
        }
        let prefix = ObjectIdPrefix::from_hex(prefix, self.hash_algo())?;
        self.with_pack(|objects_dir| {
            let indexes = read_local_pack_indexes_cached(objects_dir)?;
            let mut seen = HashSet::new();
            let mut count = 0usize;
            for idx in &indexes {
                collect_prefix_from_index(idx, &prefix, limit, &mut count, out, &mut seen)?;
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

fn for_each_removable_pack_object_ids(
    objects_dir: &Path,
    f: &mut dyn FnMut(&ObjectId) -> ControlFlow<()>,
) -> Result<()> {
    let pack_dir = objects_dir.join("pack");
    let indexes = read_local_pack_indexes_cached(objects_dir)?;
    let mut seen = HashSet::new();
    for idx in indexes {
        if idx.is_promisor || pack_index_has_keep_marker(&pack_dir, &idx.pack_path) {
            continue;
        }
        let hash_bytes = idx.hash_bytes();
        for entry in idx.iter() {
            if entry.oid().len() != hash_bytes {
                continue;
            }
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
}

fn pack_index_has_keep_marker(pack_dir: &Path, pack_path: &Path) -> bool {
    let Some(stem) = pack_path.file_stem().and_then(|s| s.to_str()) else {
        return false;
    };
    pack_dir.join(format!("{stem}.keep")).is_file()
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
    prefix: &ObjectIdPrefix,
    limit: usize,
    count: &mut usize,
    out: &mut Vec<ObjectId>,
    seen: &mut HashSet<ObjectId>,
) -> Result<()> {
    let lo = lower_bound_oid(idx, prefix.lowest().as_bytes());
    let hi = upper_bound_oid(idx, prefix.highest().as_bytes());
    for i in lo..hi {
        let digest = idx.oid_at(i);
        if !prefix.matches_digest(digest) {
            continue;
        }
        let Ok(oid) = ObjectId::from_bytes(digest) else {
            continue;
        };
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

/// Freshen `pack_path` (like rewriting objects it already holds) so age-based pruning
/// treats its objects as new. Returns `false` when the times cannot be set.
pub(crate) fn touch_pack_mtime(pack_path: &std::path::Path) -> bool {
    let Some(touched) = crate::fs_readonly::set_file_times_now(pack_path) else {
        return false;
    };
    pack::refresh_pack_bytes_signature(pack_path, touched);
    true
}

/// Compressed bytes a [`PackPayloadStream`] reads from a mapped pack before it drops the
/// mapping's pages from the resident set, so streaming a large object keeps a bounded
/// window of the pack resident instead of the whole compressed entry.
const STREAM_RESIDENT_PACK_BYTES: usize = 4 * 1024 * 1024;

/// Inflates a whole (non-delta) pack entry straight out of the mapped pack.
///
/// Yields exactly the declared payload size, then requires the zlib stream to end there, so
/// truncated, oversized or checksum-corrupt entries surface as read errors.
struct PackPayloadStream {
    whole: pack::PackedWholeObject,
    dec: Decompress,
    pos: usize,
    remaining: u64,
    stream_ended: bool,
    /// Compressed bytes consumed since the mapping's pages were last released.
    resident: usize,
}

impl PackPayloadStream {
    fn new(whole: pack::PackedWholeObject) -> Self {
        Self {
            pos: whole.zlib_offset,
            remaining: whole.size,
            whole,
            dec: Decompress::new(true),
            stream_ended: false,
            resident: 0,
        }
    }

    /// Inflate into `out`, returning the bytes produced.
    fn inflate(&mut self, out: &mut [u8]) -> io::Result<usize> {
        let input = &self.whole.pack[self.pos..self.whole.data_end];
        let before_in = self.dec.total_in();
        let before_out = self.dec.total_out();
        let status = self
            .dec
            .decompress(input, out, FlushDecompress::None)
            .map_err(|e| corrupt_pack_stream(&e.to_string()))?;
        let consumed = (self.dec.total_in() - before_in) as usize;
        let produced = (self.dec.total_out() - before_out) as usize;
        self.pos += consumed;
        self.resident += consumed;
        if self.resident >= STREAM_RESIDENT_PACK_BYTES {
            // The bytes stay readable; later reads fault pages back in from the page cache.
            self.whole.pack.release_resident();
            self.resident = 0;
        }
        if status == Status::StreamEnd {
            self.stream_ended = true;
        } else if consumed == 0 && produced == 0 {
            return Err(corrupt_pack_stream("zlib stream truncated"));
        }
        Ok(produced)
    }
}

fn corrupt_pack_stream(detail: &str) -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidData,
        format!("corrupt packed object: {detail}"),
    )
}

impl Read for PackPayloadStream {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if self.remaining == 0 {
            let mut excess = [0u8; 1];
            while !self.stream_ended {
                if self.inflate(&mut excess)? > 0 {
                    return Err(corrupt_pack_stream("more data than declared size"));
                }
            }
            return Ok(0);
        }
        if buf.is_empty() {
            return Ok(0);
        }
        let max = usize::try_from(self.remaining).map_or(buf.len(), |r| r.min(buf.len()));
        loop {
            if self.stream_ended {
                return Err(corrupt_pack_stream("less data than declared size"));
            }
            let produced = self.inflate(&mut buf[..max])?;
            if produced > 0 {
                self.remaining -= produced as u64;
                return Ok(produced);
            }
        }
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

#[cfg(test)]
mod tests {
    use super::*;
    use flate2::write::ZlibEncoder;
    use flate2::Compression;
    use std::io::Write;

    use crate::pack_map::PackData;

    const TRAILER: usize = 20;

    fn zlib(data: &[u8]) -> Vec<u8> {
        let mut enc = ZlibEncoder::new(Vec::new(), Compression::default());
        enc.write_all(data).expect("deflate");
        enc.finish().expect("deflate")
    }

    /// A fake pack holding one whole entry: some leading bytes, the zlib stream, a trailer.
    fn whole(zlib_stream: &[u8], declared_size: u64) -> pack::PackedWholeObject {
        let mut bytes = b"PACK\0\0\0\x02\0\0\0\x01\xb0".to_vec();
        let zlib_offset = bytes.len();
        bytes.extend_from_slice(zlib_stream);
        let data_end = bytes.len();
        bytes.extend_from_slice(&[0u8; TRAILER]);
        pack::PackedWholeObject {
            pack: PackData::from_owned(bytes),
            zlib_offset,
            data_end,
            kind: ObjectKind::Blob,
            size: declared_size,
        }
    }

    fn read_all(whole: pack::PackedWholeObject) -> io::Result<Vec<u8>> {
        let mut stream = PackPayloadStream::new(whole);
        let mut out = Vec::new();
        let mut chunk = [0u8; 1000];
        loop {
            match stream.read(&mut chunk)? {
                0 => return Ok(out),
                n => out.extend_from_slice(&chunk[..n]),
            }
        }
    }

    fn data() -> Vec<u8> {
        (0..200_000u32)
            .flat_map(|i| (i % 251).to_le_bytes())
            .collect()
    }

    #[test]
    fn pack_payload_stream_yields_the_whole_payload() {
        let data = data();
        let out = read_all(whole(&zlib(&data), data.len() as u64)).expect("stream");
        assert!(out == data);
    }

    #[test]
    fn pack_payload_stream_rejects_truncated_entry() {
        let data = data();
        let stream = zlib(&data);
        let err =
            read_all(whole(&stream[..stream.len() / 2], data.len() as u64)).expect_err("truncated");
        assert_eq!(err.kind(), io::ErrorKind::InvalidData, "{err}");
    }

    #[test]
    fn pack_payload_stream_rejects_size_mismatch_both_ways() {
        let data = data();
        let stream = zlib(&data);
        let err = read_all(whole(&stream, data.len() as u64 + 1)).expect_err("short");
        assert!(err.to_string().contains("less data"), "{err}");
        let err = read_all(whole(&stream, data.len() as u64 - 1)).expect_err("long");
        assert!(err.to_string().contains("more data"), "{err}");
    }

    #[test]
    fn pack_payload_stream_rejects_bad_checksum() {
        let data = data();
        let mut stream = zlib(&data);
        let last = stream.len() - 1;
        stream[last] ^= 0xff;
        let err = read_all(whole(&stream, data.len() as u64)).expect_err("adler mismatch");
        assert_eq!(err.kind(), io::ErrorKind::InvalidData, "{err}");
    }
}

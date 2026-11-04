//! Loose-object storage under `objects/xx/<suffix>` (zlib-compressed Git objects).

use std::collections::{HashMap, HashSet};
use std::fs::{self, File};
use std::io::{self, Read, Write};
use std::ops::ControlFlow;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::SystemTime;

use flate2::write::ZlibEncoder;
use flate2::Compression;
use flate2::{Decompress, FlushDecompress, Status};

use super::{BoundedRead, ObjectStore, ObjectStream, WritableObjectStore};
use crate::error::{Error, Result};
use crate::git_dir::io_error::{map_objects_dir_io, GitDirPathOperation};
use crate::hash::{self, ObjectPayloadHasher};
use crate::objects::{HashAlgo, Object, ObjectId, ObjectIdPrefix, ObjectInfo, ObjectKind};
use crate::odb::WriteOptions;
use crate::zlib_inflate::{
    with_loose_scratch, LooseFileReadBudget, LOOSE_INFLATE_INPUT_CHUNK, LOOSE_INFLATE_OUTPUT_CHUNK,
};
/// Payload bytes read from a source stream per step when writing a loose object.
const STREAM_CHUNK: usize = 128 * 1024;

fn objects_dir_mtime(objects_dir: &Path) -> SystemTime {
    #[cfg(test)]
    crate::hot_path_test_metrics::record_loose_objects_root_metadata_for_active_scope();
    fs::metadata(objects_dir)
        .and_then(|m| m.modified())
        .unwrap_or(SystemTime::UNIX_EPOCH)
}

fn fanout_dir_mtime(objects_dir: &Path, prefix: &str) -> SystemTime {
    #[cfg(test)]
    crate::hot_path_test_metrics::record_loose_fanout_metadata_for_active_scope();
    fs::metadata(objects_dir.join(prefix))
        .and_then(|m| m.modified())
        .unwrap_or(SystemTime::UNIX_EPOCH)
}

fn loose_fanout_is_dir(objects_dir: &Path, prefix: &str) -> bool {
    #[cfg(test)]
    crate::hot_path_test_metrics::record_loose_path_exists_probe_for_active_scope();
    objects_dir.join(prefix).is_dir()
}

fn loose_fanout_prefix(objects_dir: &Path, path: &Path) -> Option<String> {
    path.strip_prefix(objects_dir)
        .ok()
        .and_then(|rel| rel.components().next())
        .and_then(|c| c.as_os_str().to_str())
        .filter(|s| s.len() == 2 && s.chars().all(|ch| ch.is_ascii_hexdigit()))
        .map(str::to_owned)
}

/// Per-`objects/` negative cache for loose read misses (owned by [`LooseStore`] / [`crate::odb::Odb`]).
#[derive(Debug)]
pub(crate) struct LooseNegativeCache {
    root_mtime: SystemTime,
    fanout_mtime: HashMap<String, SystemTime>,
    absent_paths: HashSet<PathBuf>,
    absent_fanout: HashSet<String>,
}

impl Default for LooseNegativeCache {
    fn default() -> Self {
        Self {
            root_mtime: SystemTime::UNIX_EPOCH,
            fanout_mtime: HashMap::new(),
            absent_paths: HashSet::new(),
            absent_fanout: HashSet::new(),
        }
    }
}

impl LooseNegativeCache {
    fn sync_root(&mut self, objects_dir: &Path) {
        let mt = objects_dir_mtime(objects_dir);
        if self.root_mtime != mt {
            *self = Self {
                root_mtime: mt,
                ..Self::default()
            };
        }
    }

    fn sync_fanout(&mut self, objects_dir: &Path, prefix: &str) {
        let mt = fanout_dir_mtime(objects_dir, prefix);
        if self.fanout_mtime.get(prefix) != Some(&mt) {
            self.fanout_mtime.insert(prefix.to_owned(), mt);
            let fanout_dir = objects_dir.join(prefix);
            self.absent_paths.retain(|p| !p.starts_with(&fanout_dir));
            self.absent_fanout.remove(prefix);
        }
    }

    /// Whether `path` is known absent from a prior miss with no filesystem probes.
    fn peek_known_absent(&self, objects_dir: &Path, path: &Path) -> bool {
        if self.absent_paths.contains(path) {
            return true;
        }
        if let Some(prefix) = loose_fanout_prefix(objects_dir, path) {
            if self.absent_fanout.contains(&prefix) {
                return true;
            }
        }
        false
    }

    fn may_skip_open(&mut self, objects_dir: &Path, path: &Path) -> bool {
        if self.peek_known_absent(objects_dir, path) {
            return true;
        }
        self.sync_root(objects_dir);
        if let Some(prefix) = loose_fanout_prefix(objects_dir, path) {
            self.sync_fanout(objects_dir, &prefix);
            if self.absent_fanout.contains(&prefix) {
                return true;
            }
        }
        self.absent_paths.contains(path)
    }

    fn record_absent(&mut self, objects_dir: &Path, path: &Path) {
        self.sync_root(objects_dir);
        if let Some(prefix) = loose_fanout_prefix(objects_dir, path) {
            self.sync_fanout(objects_dir, &prefix);
        }
        self.absent_paths.insert(path.to_path_buf());
        if let Some(prefix) = loose_fanout_prefix(objects_dir, path) {
            if !loose_fanout_is_dir(objects_dir, &prefix) {
                self.absent_fanout.insert(prefix);
            }
        }
    }
}

pub(crate) type SharedLooseNegativeCache = Arc<Mutex<LooseNegativeCache>>;

/// On-disk loose object store for one `objects/` directory.
#[derive(Debug, Clone)]
pub struct LooseStore {
    objects_dir: PathBuf,
    hash_algo: HashAlgo,
    compression: Compression,
    negative_cache: SharedLooseNegativeCache,
}

impl LooseStore {
    /// Open a loose store at `objects_dir` using `hash_algo` and zlib `compression`.
    #[must_use]
    pub fn new(objects_dir: PathBuf, hash_algo: HashAlgo, compression: Compression) -> Self {
        Self::new_with_negative_cache(
            objects_dir,
            hash_algo,
            compression,
            Arc::new(Mutex::new(LooseNegativeCache::default())),
        )
    }

    pub(crate) fn new_with_negative_cache(
        objects_dir: PathBuf,
        hash_algo: HashAlgo,
        compression: Compression,
        negative_cache: SharedLooseNegativeCache,
    ) -> Self {
        Self {
            objects_dir,
            hash_algo,
            compression,
            negative_cache,
        }
    }

    fn invalidate_negative_cache(&self) {
        if let Ok(mut guard) = self.negative_cache.lock() {
            *guard = LooseNegativeCache::default();
        }
    }

    fn open_may_be_absent(&self, path: &Path) -> bool {
        self.negative_cache
            .lock()
            .ok()
            .is_some_and(|mut guard| guard.may_skip_open(&self.objects_dir, path))
    }

    /// Memory-only negative-cache hit for `oid` (no filesystem metadata).
    pub(crate) fn cached_absent_loose_path(&self, oid: &ObjectId) -> bool {
        let path = self.object_path(oid);
        self.negative_cache
            .lock()
            .ok()
            .is_some_and(|guard| guard.peek_known_absent(&self.objects_dir, &path))
    }

    fn record_absent_path(&self, path: &Path) {
        if let Ok(mut guard) = self.negative_cache.lock() {
            guard.record_absent(&self.objects_dir, path);
        }
    }

    /// Path to the `objects/` root.
    #[must_use]
    pub fn objects_dir(&self) -> &Path {
        &self.objects_dir
    }

    /// Filesystem path for a loose object file.
    #[must_use]
    pub fn object_path(&self, oid: &ObjectId) -> PathBuf {
        oid.loose_path_in(&self.objects_dir)
    }

    /// Create all 256 loose-object prefix directories (`objects/xx/`).
    ///
    /// # Errors
    ///
    /// Returns [`Error::Io`] if a directory cannot be created.
    pub fn ensure_all_loose_prefix_dirs(&self) -> Result<()> {
        for i in 0u8..=255 {
            let prefix = self.objects_dir.join(format!("{i:02x}"));
            fs::create_dir_all(&prefix).map_err(|source| {
                map_objects_dir_io(
                    &self.objects_dir,
                    &prefix,
                    GitDirPathOperation::CreateDir,
                    source,
                )
            })?;
        }
        Ok(())
    }

    /// Zlib-compress canonical store bytes at this store's compression level.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Zlib`] when compression fails.
    pub fn zlib_compress_store_bytes(&self, store_bytes: &[u8]) -> Result<Vec<u8>> {
        zlib_compress_store_bytes(store_bytes, self.compression)
    }

    /// When a loose file already exists, freshen (unless silent) and return its id.
    pub(crate) fn try_finish_if_loose_present(
        &self,
        oid: &ObjectId,
        options: WriteOptions,
    ) -> Result<Option<ObjectId>> {
        let path = self.object_path(oid);
        if path.is_file() {
            if !options.silent {
                let _ = self.freshen(oid)?;
            }
            return Ok(Some(*oid));
        }
        Ok(None)
    }

    /// Write canonical store bytes for a precomputed `oid` (hashing skipped).
    ///
    /// # Errors
    ///
    /// Same as [`WritableObjectStore::write`].
    pub fn write_store_prehashed(
        &self,
        oid: &ObjectId,
        store_bytes: &[u8],
        options: WriteOptions,
    ) -> Result<ObjectId> {
        parse_object_bytes(store_bytes)?;
        if oid.algo() != self.hash_algo {
            return Err(Error::InvalidObjectId(oid.to_hex()));
        }
        if let Some(existing) = self.try_finish_if_loose_present(oid, options)? {
            return Ok(existing);
        }
        let path = self.object_path(oid);
        let prefix_dir = path
            .parent()
            .ok_or_else(|| Error::PathError("object path has no parent".to_owned()))?;
        let zlib = zlib_compress_store_bytes(store_bytes, self.compression)?;
        publish_loose_object(&self.objects_dir, &path, prefix_dir, false, |mut file| {
            file.write_all(&zlib).map_err(|source| {
                map_objects_dir_io(
                    &self.objects_dir,
                    &path,
                    GitDirPathOperation::WriteObject,
                    source,
                )
            })
        })?;
        self.invalidate_negative_cache();
        Ok(*oid)
    }

    /// Write zlib-compressed store bytes for a precomputed `oid`.
    ///
    /// # Errors
    ///
    /// Same as [`WritableObjectStore::write`].
    pub fn write_zlib_prehashed(
        &self,
        oid: &ObjectId,
        zlib_store: &[u8],
        options: WriteOptions,
    ) -> Result<ObjectId> {
        if oid.algo() != self.hash_algo {
            return Err(Error::InvalidObjectId(oid.to_hex()));
        }
        if let Some(existing) = self.try_finish_if_loose_present(oid, options)? {
            return Ok(existing);
        }
        let path = self.object_path(oid);
        let prefix_dir = path
            .parent()
            .ok_or_else(|| Error::PathError("object path has no parent".to_owned()))?;
        publish_loose_object(
            &self.objects_dir,
            &path,
            prefix_dir,
            options.loose_prefix_dirs_ready,
            |mut file| {
                file.write_all(zlib_store).map_err(|source| {
                    map_objects_dir_io(
                        &self.objects_dir,
                        &path,
                        GitDirPathOperation::WriteObject,
                        source,
                    )
                })
            },
        )?;
        self.invalidate_negative_cache();
        Ok(*oid)
    }

    /// Write `data` as object `oid` of `kind` (hash already computed by the caller),
    /// deflating straight into the temporary loose file so no compressed copy is held in
    /// memory.
    ///
    /// # Errors
    ///
    /// Same as [`WritableObjectStore::write`].
    pub(crate) fn write_prehashed(
        &self,
        oid: &ObjectId,
        kind: ObjectKind,
        data: &[u8],
        options: WriteOptions,
    ) -> Result<ObjectId> {
        if oid.algo() != self.hash_algo {
            return Err(Error::InvalidObjectId(oid.to_hex()));
        }
        if let Some(existing) = self.try_finish_if_loose_present(oid, options)? {
            return Ok(existing);
        }
        let path = self.object_path(oid);
        let prefix_dir = path
            .parent()
            .ok_or_else(|| Error::PathError("object path has no parent".to_owned()))?;
        publish_loose_object(
            &self.objects_dir,
            &path,
            prefix_dir,
            options.loose_prefix_dirs_ready,
            |file| deflate_object_into(file, kind, data, self.compression),
        )?;
        self.invalidate_negative_cache();
        Ok(*oid)
    }

    /// Read a loose object at `path`, verifying the payload hashes to `expected_oid`.
    ///
    /// # Errors
    ///
    /// - [`Error::Zlib`] — decompression failed.
    /// - [`Error::CorruptObject`] — header is malformed.
    /// - [`Error::LooseHashMismatch`] — payload OID does not match `expected_oid`.
    pub fn read_verify_oid(path: &Path, expected_oid: &ObjectId) -> Result<Object> {
        let file = fs::File::open(path).map_err(Error::Io)?;
        let raw = match read_zlib_loose_payload(file) {
            Ok(raw) => raw,
            Err(err) => {
                return Err(Error::wrap_loose_read_failure(
                    *expected_oid,
                    path.to_path_buf(),
                    err,
                ));
            }
        };
        let obj = match parse_object_bytes_with_oid(&raw, expected_oid) {
            Ok(obj) => obj,
            Err(err) => {
                return Err(Error::wrap_loose_read_failure(
                    *expected_oid,
                    path.to_path_buf(),
                    err,
                ));
            }
        };
        let computed = hash::hash_object(expected_oid.algo(), obj.kind, &obj.data)?;
        if computed != *expected_oid {
            return Err(Error::LooseHashMismatch {
                path: path.display().to_string(),
                real_oid: computed.to_hex(),
            });
        }
        Ok(obj)
    }

    /// Read `oid` unless its payload is larger than `limit` bytes, opening the file once.
    ///
    /// Inflation stops as soon as the header declares a payload larger than `limit`.
    /// `Ok(None)` when there is no loose file.
    ///
    /// # Errors
    ///
    /// Same as [`ObjectStore::read`]: [`Error::Io`], [`Error::Zlib`] or
    /// [`Error::CorruptObject`] for unreadable or malformed objects.
    pub(crate) fn read_unless_larger(
        &self,
        oid: &ObjectId,
        limit: u64,
    ) -> Result<Option<BoundedRead>> {
        let mut file = match fs::File::open(self.object_path(oid)) {
            Ok(f) => f,
            Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(err) => return Err(Error::Io(err)),
        };
        let mut buf = Vec::new();
        let complete =
            with_loose_scratch(|s| s.inflate_loose_file_unless_larger(&mut file, &mut buf, limit))?;
        if !complete {
            return Ok(Some(BoundedRead::Larger));
        }
        let kind = strip_decompressed_object_header(&mut buf, Some(oid))?;
        Ok(Some(BoundedRead::Object(Object::new(kind, buf))))
    }

    fn touch_mtime(path: &Path) -> bool {
        crate::fs_readonly::set_file_times_now(path).is_some()
    }
}

impl ObjectStore for LooseStore {
    fn hash_algo(&self) -> HashAlgo {
        self.hash_algo
    }

    fn read(&self, oid: &ObjectId) -> Result<Option<Object>> {
        let mut data = Vec::new();
        Ok(self
            .read_into(oid, &mut data)?
            .map(|kind| Object::new(kind, data)))
    }

    fn read_into(&self, oid: &ObjectId, buf: &mut Vec<u8>) -> Result<Option<ObjectKind>> {
        let path = self.object_path(oid);
        if self.open_may_be_absent(&path) {
            return Ok(None);
        }
        let file = match fs::File::open(&path) {
            Ok(f) => f,
            Err(err) if err.kind() == io::ErrorKind::NotFound => {
                self.record_absent_path(&path);
                return Ok(None);
            }
            Err(err) => return Err(Error::Io(err)),
        };
        if let Err(err) = read_zlib_loose_payload_into(file, buf) {
            return Err(Error::wrap_loose_read_failure(*oid, path, err));
        }
        let kind = match strip_decompressed_object_header(buf, Some(oid)) {
            Ok(kind) => kind,
            Err(err) => return Err(Error::wrap_loose_read_failure(*oid, path, err)),
        };
        Ok(Some(kind))
    }

    fn read_info(&self, oid: &ObjectId) -> Result<Option<ObjectInfo>> {
        let path = self.object_path(oid);
        if self.open_may_be_absent(&path) {
            return Ok(None);
        }
        let mut file = match fs::File::open(&path) {
            Ok(f) => f,
            Err(err) if err.kind() == io::ErrorKind::NotFound => {
                self.record_absent_path(&path);
                return Ok(None);
            }
            Err(err) => return Err(Error::Io(err)),
        };
        match loose_object_info_from_file(&mut file) {
            Err(Error::Io(err)) if err.kind() == io::ErrorKind::IsADirectory => Ok(None),
            Err(err) => Err(Error::wrap_loose_read_failure(*oid, path, err)),
            Ok(info) => Ok(Some(info)),
        }
    }

    fn open_stream(&self, oid: &ObjectId) -> Result<Option<ObjectStream<'_>>> {
        let path = self.object_path(oid);
        if self.open_may_be_absent(&path) {
            return Ok(None);
        }
        let file = match fs::File::open(&path) {
            Ok(f) => f,
            Err(err) if err.kind() == io::ErrorKind::NotFound => {
                self.record_absent_path(&path);
                return Ok(None);
            }
            Err(err) => return Err(Error::Io(err)),
        };
        let reader = match LoosePayloadReader::open(file) {
            Ok(reader) => reader,
            Err(err) => return Err(Error::wrap_loose_read_failure(*oid, path, err)),
        };
        Ok(Some(ObjectStream {
            kind: reader.kind,
            size: reader.payload_size,
            reader: Box::new(reader),
        }))
    }

    fn for_each_object(&self, f: &mut dyn FnMut(&ObjectId) -> ControlFlow<()>) -> Result<()> {
        for_each_loose_object_id(&self.objects_dir, self.hash_algo, f)
    }

    fn lookup_prefix(&self, prefix: &str, limit: usize, out: &mut Vec<ObjectId>) -> Result<()> {
        let prefix = ObjectIdPrefix::from_hex(prefix, self.hash_algo)?;
        let start_len = out.len();
        let mut collect = |oid: &ObjectId| {
            if prefix.matches(oid) {
                out.push(*oid);
                if limit != 0 && out.len() - start_len >= limit {
                    return ControlFlow::Break(());
                }
            }
            ControlFlow::Continue(())
        };
        if prefix.nibbles() < 2 {
            return for_each_loose_object_id(&self.objects_dir, self.hash_algo, &mut collect);
        }
        let fanout = format!("{:02x}", prefix.lowest().as_bytes()[0]);
        let fanout_dir = self.objects_dir.join(&fanout);
        for_each_object_id_in_fanout_dir(&fanout_dir, &fanout, self.hash_algo, &mut collect)
            .map(drop)
    }
}

impl WritableObjectStore for LooseStore {
    fn write(&self, kind: ObjectKind, data: &[u8], options: WriteOptions) -> Result<ObjectId> {
        let oid = hash::hash_object(self.hash_algo, kind, data)?;
        if let Some(existing) = self.try_finish_if_loose_present(&oid, options)? {
            return Ok(existing);
        }
        let path = self.object_path(&oid);
        let prefix_dir = path
            .parent()
            .ok_or_else(|| Error::PathError("object path has no parent".to_owned()))?;
        publish_loose_object(&self.objects_dir, &path, prefix_dir, false, |file| {
            deflate_object_into(file, kind, data, self.compression)
        })?;
        self.invalidate_negative_cache();
        Ok(oid)
    }

    fn write_stream(
        &self,
        kind: ObjectKind,
        size: u64,
        reader: &mut dyn Read,
        options: WriteOptions,
    ) -> Result<ObjectId> {
        self.write_payload_stream(kind, size, reader, options)
    }

    fn freshen(&self, oid: &ObjectId) -> Result<bool> {
        let path = self.object_path(oid);
        if path.is_file() {
            return Ok(LooseStore::touch_mtime(&path));
        }
        Ok(false)
    }
}

/// Enumerate loose `(oid, path)` pairs under `objects_dir`.
pub fn enumerate_loose_objects(
    objects_dir: &Path,
    hash_algo: HashAlgo,
) -> Result<Vec<(ObjectId, PathBuf)>> {
    let mut out = Vec::new();
    for_each_loose_object_id(objects_dir, hash_algo, &mut |oid| {
        let path = oid.loose_path_in(objects_dir);
        out.push((*oid, path));
        ControlFlow::Continue(())
    })?;
    Ok(out)
}

/// Invoke `f` for each loose object id under `objects_dir`.
pub fn for_each_loose_object_id(
    objects_dir: &Path,
    hash_algo: HashAlgo,
    f: &mut dyn FnMut(&ObjectId) -> ControlFlow<()>,
) -> Result<()> {
    let top = match fs::read_dir(objects_dir) {
        Ok(rd) => rd,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(Error::Io(e)),
    };

    for top_entry in top {
        let top_entry = top_entry.map_err(Error::Io)?;
        let name = top_entry.file_name();
        let Some(prefix) = name.to_str() else {
            continue;
        };
        if prefix.len() != 2 || !prefix.bytes().all(|b| b.is_ascii_hexdigit()) {
            continue;
        }
        if !top_entry.file_type().map_err(Error::Io)?.is_dir() {
            continue;
        }
        if for_each_object_id_in_fanout_dir(&top_entry.path(), prefix, hash_algo, f)?.is_break() {
            return Ok(());
        }
    }
    Ok(())
}

/// Visit the loose ids in one `objects/<fanout>` directory; a missing directory (or a
/// non-directory at that path) holds no objects.
pub(crate) fn for_each_object_id_in_fanout_dir(
    fanout_dir: &Path,
    fanout: &str,
    hash_algo: HashAlgo,
    f: &mut dyn FnMut(&ObjectId) -> ControlFlow<()>,
) -> Result<ControlFlow<()>> {
    let sub = match fs::read_dir(fanout_dir) {
        Ok(rd) => rd,
        Err(e)
            if matches!(
                e.kind(),
                io::ErrorKind::NotFound | io::ErrorKind::NotADirectory
            ) =>
        {
            return Ok(ControlFlow::Continue(()));
        }
        Err(e) => return Err(Error::Io(e)),
    };
    for sub_entry in sub {
        let sub_entry = sub_entry.map_err(Error::Io)?;
        if !sub_entry.file_type().map_err(Error::Io)?.is_file() {
            continue;
        }
        let suffix_name = sub_entry.file_name();
        let Some(suffix) = suffix_name.to_str() else {
            continue;
        };
        if !is_loose_suffix_for_algo(suffix, hash_algo) {
            continue;
        }
        let hex = format!("{fanout}{suffix}");
        let Ok(oid) = ObjectId::from_hex(&hex) else {
            continue;
        };
        if f(&oid).is_break() {
            return Ok(ControlFlow::Break(()));
        }
    }
    Ok(ControlFlow::Continue(()))
}

/// Decompress a zlib-wrapped loose object payload from an open file.
pub fn read_zlib_loose_payload(file: File) -> Result<Vec<u8>> {
    let mut out = Vec::new();
    read_zlib_loose_payload_into(file, &mut out)?;
    Ok(out)
}

/// Decompress a loose object into `buf`, reusing its capacity.
pub(crate) fn read_zlib_loose_payload_into(mut file: File, buf: &mut Vec<u8>) -> Result<()> {
    with_loose_scratch(|scratch| scratch.inflate_loose_file(&mut file, buf))
}

/// Remove the Git object header from decompressed bytes in `buf`, leaving only the payload.
pub(crate) fn strip_decompressed_object_header(
    buf: &mut Vec<u8>,
    oid_hint: Option<&ObjectId>,
) -> Result<ObjectKind> {
    let nul = buf
        .iter()
        .position(|&b| b == 0)
        .ok_or_else(|| Error::CorruptObject("missing NUL in object header".to_owned()))?;
    let (kind, size) = parse_object_header_prefix(buf)?;
    let payload_start = nul + 1;
    let size_usize = usize::try_from(size)
        .map_err(|_| Error::CorruptObject(format!("invalid object size: {size}")))?;
    if buf.len().saturating_sub(payload_start) != size_usize {
        let oid_str = oid_hint.map(ObjectId::to_hex).unwrap_or_else(|| {
            hash_algo_digest_hex(HashAlgo::Sha1, buf).unwrap_or_else(|_| "?".to_owned())
        });
        return Err(Error::CorruptObject(format!(
            "object {oid_str} size mismatch: header says {size}, got {}",
            buf.len().saturating_sub(payload_start)
        )));
    }
    if payload_start > 0 {
        buf.copy_within(payload_start.., 0);
    }
    buf.truncate(size_usize);
    Ok(kind)
}

/// Read kind and size from a loose object file without loading the full payload.
pub fn read_loose_object_info(path: &Path) -> Result<ObjectInfo> {
    let mut file = fs::File::open(path).map_err(Error::Io)?;
    match loose_object_info_from_file(&mut file) {
        Ok(info) => Ok(info),
        Err(err) => {
            if let Some(oid) = oid_from_loose_object_path(path) {
                Err(Error::wrap_loose_read_failure(oid, path.to_path_buf(), err))
            } else {
                Err(err)
            }
        }
    }
}

fn oid_from_loose_object_path(path: &Path) -> Option<ObjectId> {
    let file_name = path.file_name()?.to_str()?;
    if !ObjectId::is_loose_suffix_len(file_name.len()) {
        return None;
    }
    let parent = path.parent()?.file_name()?.to_str()?;
    if parent.len() != 2 {
        return None;
    }
    let hex = format!("{parent}{file_name}");
    ObjectId::from_hex(&hex).ok()
}

fn loose_object_info_from_file(file: &mut File) -> Result<ObjectInfo> {
    let prefix = with_loose_scratch(|scratch| scratch.inflate_loose_header(file))?;
    let (kind, size) = parse_object_header_prefix(&prefix)?;
    Ok(ObjectInfo { kind, size })
}

/// Parse decompressed object bytes (`"<type> <size>\0<data>"`) into an [`Object`].
pub(crate) fn parse_object_bytes(raw: &[u8]) -> Result<Object> {
    parse_object_bytes_inner(raw, None)
}

pub(crate) fn parse_object_bytes_with_oid(raw: &[u8], oid: &ObjectId) -> Result<Object> {
    parse_object_bytes_inner(raw, Some(oid))
}

pub(crate) fn zlib_compress_store_bytes(
    store_bytes: &[u8],
    compression: Compression,
) -> Result<Vec<u8>> {
    let mut out = Vec::new();
    let mut encoder = ZlibEncoder::new(&mut out, compression);
    encoder
        .write_all(store_bytes)
        .map_err(|e| Error::Zlib(e.to_string()))?;
    encoder.finish().map_err(|e| Error::Zlib(e.to_string()))?;
    Ok(out)
}

pub(crate) fn decompress_zlib_loose_bytes(zlib: &[u8]) -> Result<Vec<u8>> {
    let mut decoder = flate2::read::ZlibDecoder::new(zlib);
    let mut raw = Vec::new();
    decoder
        .read_to_end(&mut raw)
        .map_err(|e| Error::Zlib(e.to_string()))?;
    Ok(raw)
}

/// Parse `"<type> <size>\0"` from decompressed bytes that include at least the header prefix.
pub(crate) fn parse_object_header_prefix(raw: &[u8]) -> Result<(ObjectKind, u64)> {
    let nul = raw
        .iter()
        .position(|&b| b == 0)
        .ok_or_else(|| Error::CorruptObject("missing NUL in object header".to_owned()))?;

    let header = &raw[..nul];
    let sp = header
        .iter()
        .position(|&b| b == b' ')
        .ok_or_else(|| Error::CorruptObject("missing space in object header".to_owned()))?;

    if sp > 32 {
        return Err(Error::ObjectHeaderTooLong {
            oid: hash_algo_digest_hex(HashAlgo::Sha1, raw).unwrap_or_else(|_| "?".to_owned()),
        });
    }

    let kind = ObjectKind::from_bytes(&header[..sp])?;
    let size_str = std::str::from_utf8(&header[sp + 1..])
        .map_err(|_| Error::CorruptObject("non-UTF-8 object size".to_owned()))?;
    let size: u64 = size_str
        .parse()
        .map_err(|_| Error::CorruptObject(format!("invalid object size: {size_str}")))?;
    Ok((kind, size))
}

fn parse_object_bytes_inner(raw: &[u8], oid_hint: Option<&ObjectId>) -> Result<Object> {
    let nul = raw
        .iter()
        .position(|&b| b == 0)
        .ok_or_else(|| Error::CorruptObject("missing NUL in object header".to_owned()))?;

    let header = &raw[..nul];
    let data = raw[nul + 1..].to_vec();

    let sp = header
        .iter()
        .position(|&b| b == b' ')
        .ok_or_else(|| Error::CorruptObject("missing space in object header".to_owned()))?;

    if sp > 32 {
        let oid_str = oid_hint.map(|o| o.to_hex()).unwrap_or_else(|| {
            hash_algo_digest_hex(HashAlgo::Sha1, raw).unwrap_or_else(|_| "?".to_owned())
        });
        return Err(Error::ObjectHeaderTooLong { oid: oid_str });
    }

    let kind = ObjectKind::from_bytes(&header[..sp])?;

    let size_str = std::str::from_utf8(&header[sp + 1..])
        .map_err(|_| Error::CorruptObject("non-UTF-8 object size".to_owned()))?;
    let size: usize = size_str
        .parse()
        .map_err(|_| Error::CorruptObject(format!("invalid object size: {size_str}")))?;

    if data.len() != size {
        return Err(Error::CorruptObject(format!(
            "object size mismatch: header says {size} but got {}",
            data.len()
        )));
    }

    Ok(Object::new(kind, data))
}

fn hash_algo_digest_hex(algo: HashAlgo, data: &[u8]) -> Result<String> {
    Ok(algo.digest(data)?.to_hex())
}

/// Per-process sequence that, with the pid, makes every loose temp file name unique, so
/// concurrent writers of the same object (threads, handles or processes) never share one.
static LOOSE_TMP_SEQ: AtomicU64 = AtomicU64::new(0);

/// A fresh `tmp_obj_<pid>_<seq>` path in `prefix_dir`; the `tmp_` prefix keeps it out of
/// loose-object enumeration.
fn unique_loose_tmp_path(prefix_dir: &Path) -> PathBuf {
    let seq = LOOSE_TMP_SEQ.fetch_add(1, Ordering::Relaxed);
    prefix_dir.join(format!("tmp_obj_{}_{seq}", std::process::id()))
}

/// Deflate `"<kind> <len>\0"` and `data` into `file` without copying `data` behind the header.
fn deflate_object_into(
    file: File,
    kind: ObjectKind,
    data: &[u8],
    compression: Compression,
) -> Result<()> {
    let zlib_err = |e: io::Error| Error::Zlib(e.to_string());
    let mut encoder = ZlibEncoder::new(file, compression);
    write!(encoder, "{} {}\0", kind, data.len()).map_err(zlib_err)?;
    encoder.write_all(data).map_err(zlib_err)?;
    encoder.finish().map_err(zlib_err)?;
    Ok(())
}

fn read_payload_chunks(
    reader: &mut dyn Read,
    size: u64,
    mut sink: impl FnMut(&[u8]) -> Result<()>,
) -> Result<()> {
    let mut remaining = size;
    let mut buf = [0u8; STREAM_CHUNK];
    while remaining > 0 {
        let want = usize::try_from(remaining.min(STREAM_CHUNK as u64)).map_err(|_| {
            Error::CorruptObject(format!("object size {size} exceeds address space"))
        })?;
        let n = match reader.read(&mut buf[..want]) {
            Ok(0) => {
                return Err(Error::ObjectStreamTruncated {
                    expected: size,
                    actual: size - remaining,
                })
            }
            Ok(n) => n,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(Error::Io(e)),
        };
        sink(&buf[..n])?;
        remaining -= n as u64;
    }
    Ok(())
}

impl LooseStore {
    /// Store `size` uncompressed payload bytes of `kind` read from `reader` as a loose object.
    ///
    /// Hashes and deflates in fixed-size chunks so memory use does not grow with `size`.
    pub(crate) fn write_payload_stream(
        &self,
        kind: ObjectKind,
        size: u64,
        reader: &mut dyn Read,
        options: WriteOptions,
    ) -> Result<ObjectId> {
        let (first, tail) = crate::odb::stream::read_first_payload_chunk(reader, size)?;
        let compression =
            crate::odb::stream::compression_after_first_chunk(&first, self.compression);
        self.write_payload_stream_with(kind, size, options, compression, |chunk_sink| {
            chunk_sink(&first)?;
            if tail > 0 {
                read_payload_chunks(reader, tail, chunk_sink)
            } else {
                Ok(())
            }
        })
    }

    /// Store a payload supplied chunk-by-chunk through `supply` (used by pack unpack).
    pub(crate) fn write_payload_stream_with(
        &self,
        kind: ObjectKind,
        size: u64,
        options: WriteOptions,
        compression: Compression,
        mut supply: impl FnMut(&mut dyn FnMut(&[u8]) -> Result<()>) -> Result<()>,
    ) -> Result<ObjectId> {
        let prefix_dir = &self.objects_dir;
        let mut hasher = ObjectPayloadHasher::new(self.hash_algo, kind, size);
        let tmp_path = unique_loose_tmp_path(prefix_dir);
        fs::create_dir_all(prefix_dir).map_err(Error::Io)?;
        let file = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&tmp_path)
            .map_err(Error::Io)?;
        let zlib_err = |e: io::Error| Error::Zlib(e.to_string());
        let mut encoder = ZlibEncoder::new(file, compression);
        if let Err(e) = (|| {
            write!(encoder, "{} {}\0", kind, size).map_err(zlib_err)?;
            supply(&mut |chunk| {
                hasher.update(chunk);
                encoder.write_all(chunk).map_err(zlib_err)
            })?;
            encoder.finish().map_err(zlib_err)?;
            Ok::<(), Error>(())
        })() {
            let _ = fs::remove_file(&tmp_path);
            return Err(e);
        }
        let oid = match hasher.finish() {
            Ok(oid) => oid,
            Err(e) => {
                let _ = fs::remove_file(&tmp_path);
                return Err(e);
            }
        };
        if let Some(existing) = self.try_finish_if_loose_present(&oid, options)? {
            let _ = fs::remove_file(&tmp_path);
            return Ok(existing);
        }
        let path = self.object_path(&oid);
        let dest_prefix = path
            .parent()
            .ok_or_else(|| Error::PathError("object path has no parent".to_owned()))?;
        if let Err(e) = fs::create_dir_all(dest_prefix) {
            let _ = fs::remove_file(&tmp_path);
            return Err(Error::Io(e));
        }
        match fs::rename(&tmp_path, &path) {
            Ok(()) => {
                set_loose_object_mode(&path);
                Ok(oid)
            }
            Err(e) => {
                let _ = fs::remove_file(&tmp_path);
                if path.is_file() {
                    return Ok(oid);
                }
                Err(Error::Io(e))
            }
        }
    }
}

/// Publish a loose object at `path`: `fill` writes the zlib stream into a new, uniquely
/// named temp file in `prefix_dir`, which is then renamed into place.
///
/// Publishing is idempotent: when the rename fails but `path` exists, another writer
/// published the same object and this returns `Ok`. A missing `prefix_dir` (not yet
/// created, or pruned by a concurrent process) is recreated and the write retried; with
/// `prefix_dir_ready` the first attempt skips the `create_dir_all`.
///
/// # Errors
///
/// Returns [`Error::Io`] when the temp file cannot be created or renamed after the
/// retries, or whatever `fill` returns.
fn publish_loose_object(
    objects_dir: &Path,
    path: &Path,
    prefix_dir: &Path,
    prefix_dir_ready: bool,
    mut fill: impl FnMut(File) -> Result<()>,
) -> Result<()> {
    const MAX_ATTEMPTS: u32 = 4;
    let mut attempt = 0;
    loop {
        attempt += 1;
        let can_retry = attempt < MAX_ATTEMPTS;
        if !prefix_dir_ready || attempt > 1 {
            fs::create_dir_all(prefix_dir).map_err(|source| {
                map_objects_dir_io(
                    objects_dir,
                    prefix_dir,
                    GitDirPathOperation::CreateDir,
                    source,
                )
            })?;
        }
        let tmp_path = unique_loose_tmp_path(prefix_dir);
        let file = match fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&tmp_path)
        {
            Ok(file) => file,
            Err(e)
                if can_retry
                    && matches!(
                        e.kind(),
                        io::ErrorKind::NotFound | io::ErrorKind::AlreadyExists
                    ) =>
            {
                continue;
            }
            Err(e) => {
                return Err(map_objects_dir_io(
                    objects_dir,
                    &tmp_path,
                    GitDirPathOperation::WriteObject,
                    e,
                ));
            }
        };
        if let Err(e) = fill(file) {
            let _ = fs::remove_file(&tmp_path);
            return Err(e);
        }
        match fs::rename(&tmp_path, path) {
            Ok(()) => {
                set_loose_object_mode(path);
                return Ok(());
            }
            Err(e) => {
                let _ = fs::remove_file(&tmp_path);
                if path.is_file() {
                    return Ok(());
                }
                if can_retry && e.kind() == io::ErrorKind::NotFound {
                    continue;
                }
                return Err(map_objects_dir_io(
                    objects_dir,
                    path,
                    GitDirPathOperation::WriteObject,
                    e,
                ));
            }
        }
    }
}

fn set_loose_object_mode(path: &Path) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = fs::set_permissions(path, fs::Permissions::from_mode(0o444));
    }
    let _ = path;
}

fn is_loose_suffix_for_algo(suffix: &str, algo: HashAlgo) -> bool {
    suffix.len() == algo.hex_len() - 2 && suffix.bytes().all(|b| b.is_ascii_hexdigit())
}

/// Incremental zlib reader yielding only the object payload (after the Git header).
struct LoosePayloadReader {
    file: File,
    read_budget: LooseFileReadBudget,
    dec: Decompress,
    /// Compressed input read from `file`; bytes before `pending_pos` are already consumed.
    pending: Vec<u8>,
    pending_pos: usize,
    /// All compressed input from the file has been read.
    input_exhausted: bool,
    /// Inflater reached `Status::StreamEnd` (payload + zlib trailer validated).
    zlib_complete: bool,
    preset_dictionary: bool,
    kind: ObjectKind,
    payload_size: u64,
    payload_remaining: u64,
    phase: LooseReadPhase,
    header_buf: Vec<u8>,
    /// Payload bytes already inflated while reading the header (after the NUL).
    extra_payload: Vec<u8>,
}

enum LooseReadPhase {
    Header,
    Payload,
}

impl LoosePayloadReader {
    fn open(mut file: File) -> Result<Self> {
        let read_budget = LooseFileReadBudget::new(&file)?;
        let mut hdr = [0u8; 2];
        file.read_exact(&mut hdr).map_err(Error::Io)?;
        let cmf_flg = u16::from(hdr[0]) << 8 | u16::from(hdr[1]);
        let looks_like_zlib_header = cmf_flg != 0 && cmf_flg % 31 == 0;
        let preset_dictionary = looks_like_zlib_header && (hdr[1] & 0x20) != 0;
        let mut read_budget = read_budget;
        read_budget.note_consumed(2);
        let mut reader = Self {
            file,
            read_budget,
            dec: Decompress::new(true),
            pending: hdr.to_vec(),
            pending_pos: 0,
            input_exhausted: false,
            zlib_complete: false,
            preset_dictionary,
            kind: ObjectKind::Blob,
            payload_size: 0,
            payload_remaining: 0,
            phase: LooseReadPhase::Header,
            header_buf: Vec::with_capacity(64),
            extra_payload: Vec::new(),
        };
        reader.parse_header()?;
        Ok(reader)
    }

    fn parse_header(&mut self) -> Result<()> {
        let mut header_buf = std::mem::take(&mut self.header_buf);
        loop {
            self.pump_zlib(&mut |bytes| {
                header_buf.extend_from_slice(bytes);
            })?;
            if header_buf.contains(&0) || header_buf.len() >= 128 {
                break;
            }
            if self.input_exhausted && self.pending_pos == self.pending.len() {
                break;
            }
        }
        let nul = header_buf
            .iter()
            .position(|&b| b == 0)
            .ok_or_else(|| Error::CorruptObject("missing NUL in object header".to_owned()))?;
        let (kind, size) = parse_object_header_prefix(&header_buf[..=nul])?;
        self.extra_payload = header_buf[nul + 1..].to_vec();
        self.header_buf = header_buf;
        self.kind = kind;
        self.payload_size = size;
        self.payload_remaining = size;
        self.phase = LooseReadPhase::Payload;
        Ok(())
    }

    fn pump_zlib(&mut self, on_out: &mut dyn FnMut(&[u8])) -> Result<()> {
        let mut scratch = [0u8; LOOSE_INFLATE_OUTPUT_CHUNK];
        let produced = self.inflate_into(&mut scratch)?;
        if produced > 0 {
            on_out(&scratch[..produced]);
        }
        Ok(())
    }

    /// Inflate into `out`, reading more compressed input when the pending input is used up.
    ///
    /// Returns the bytes produced, which is zero once the zlib stream has ended.
    fn inflate_into(&mut self, out: &mut [u8]) -> Result<usize> {
        if self.zlib_complete {
            return Ok(0);
        }
        let cap = out.len().min(LOOSE_INFLATE_OUTPUT_CHUNK);
        let out = &mut out[..cap];
        if self.pending_pos == self.pending.len() && !self.input_exhausted {
            let mut buf = [0u8; LOOSE_INFLATE_INPUT_CHUNK];
            let n = self.read_budget.read(&mut self.file, &mut buf)?;
            self.pending.clear();
            self.pending.extend_from_slice(&buf[..n]);
            self.pending_pos = 0;
            self.input_exhausted = n == 0;
        }
        let flush = if self.input_exhausted && self.pending_pos == self.pending.len() {
            FlushDecompress::Finish
        } else {
            FlushDecompress::None
        };
        let before_in = self.dec.total_in();
        let before_out = self.dec.total_out();
        let status = match self
            .dec
            .decompress(&self.pending[self.pending_pos..], out, flush)
        {
            Ok(s) => s,
            Err(e) => {
                if self.preset_dictionary {
                    return Err(Error::Zlib("needs dictionary".to_owned()));
                }
                return Err(Error::Zlib(e.to_string()));
            }
        };
        // Both are bounded by the slices just passed to the inflater.
        self.pending_pos += (self.dec.total_in() - before_in) as usize;
        let produced = (self.dec.total_out() - before_out) as usize;
        if matches!(status, Status::StreamEnd) {
            self.zlib_complete = true;
        }
        Ok(produced)
    }

    /// After the declared payload is delivered, drain the zlib trailer and reject truncation.
    fn ensure_zlib_complete(&mut self) -> io::Result<()> {
        if self.zlib_complete {
            return Ok(());
        }
        if !self.extra_payload.is_empty() {
            return Err(io::Error::other(
                "corrupt object: inflated bytes remain after declared payload size",
            ));
        }
        loop {
            let mut excess = 0usize;
            self.pump_zlib(&mut |bytes| {
                excess += bytes.len();
            })
            .map_err(|e| io::Error::other(e.to_string()))?;
            if excess > 0 {
                return Err(io::Error::other(
                    "corrupt object: excess decompressed data after declared payload size",
                ));
            }
            if self.zlib_complete {
                return Ok(());
            }
            if self.input_exhausted && self.pending_pos == self.pending.len() {
                return Err(io::Error::other(
                    "corrupt object: zlib stream ended before trailer validation",
                ));
            }
        }
    }
}

impl Read for LoosePayloadReader {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if !matches!(self.phase, LooseReadPhase::Payload) {
            return Ok(0);
        }
        if self.payload_remaining == 0 {
            self.ensure_zlib_complete()?;
            return Ok(0);
        }
        if buf.is_empty() {
            return Ok(0);
        }
        let mut delivered = 0usize;
        while delivered < buf.len() && self.payload_remaining > 0 {
            if !self.extra_payload.is_empty() {
                let take = self
                    .extra_payload
                    .len()
                    .min(buf.len() - delivered)
                    .min(self.payload_remaining as usize);
                buf[delivered..delivered + take].copy_from_slice(&self.extra_payload[..take]);
                self.extra_payload.drain(..take);
                delivered += take;
                self.payload_remaining -= take as u64;
                continue;
            }
            let want = usize::try_from(self.payload_remaining)
                .map_or(buf.len() - delivered, |r| r.min(buf.len() - delivered));
            let produced = self
                .inflate_into(&mut buf[delivered..delivered + want])
                .map_err(|e| io::Error::other(e.to_string()))?;
            if produced == 0 {
                if self.zlib_complete
                    || (self.input_exhausted && self.pending_pos == self.pending.len())
                {
                    return Err(io::Error::other(format!(
                        "corrupt object: zlib stream ended with {} payload bytes remaining",
                        self.payload_remaining
                    )));
                }
                continue;
            }
            delivered += produced;
            self.payload_remaining -= produced as u64;
        }
        if self.payload_remaining == 0 {
            self.ensure_zlib_complete()?;
        }
        Ok(delivered)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Read;

    use super::super::{ObjectStore, WritableObjectStore};

    const READ_CHUNK: usize = LOOSE_INFLATE_INPUT_CHUNK;

    #[test]
    fn lookup_prefix_scans_single_fanout_dir() {
        let dir = tempfile::tempdir().unwrap();
        let store = LooseStore::new(
            dir.path().join("objects"),
            HashAlgo::Sha1,
            Compression::default(),
        );
        fs::create_dir_all(store.objects_dir()).unwrap();
        let oid =
            WritableObjectStore::write(&store, ObjectKind::Blob, b"x", WriteOptions::default())
                .unwrap();
        let mut out = Vec::new();
        ObjectStore::lookup_prefix(&store, &oid.to_hex()[..4], 0, &mut out).unwrap();
        assert_eq!(out, vec![oid]);
    }

    fn temp_store() -> (tempfile::TempDir, LooseStore) {
        let dir = tempfile::tempdir().unwrap();
        let store = LooseStore::new(
            dir.path().join("objects"),
            HashAlgo::Sha1,
            Compression::default(),
        );
        fs::create_dir_all(store.objects_dir()).unwrap();
        (dir, store)
    }

    fn tmp_entries(store: &LooseStore) -> Vec<PathBuf> {
        fs::read_dir(store.objects_dir())
            .unwrap()
            .flatten()
            .filter(|shard| shard.file_type().is_ok_and(|t| t.is_dir()))
            .flat_map(|shard| fs::read_dir(shard.path()).unwrap().flatten())
            .map(|entry| entry.path())
            .filter(|path| {
                path.file_name()
                    .is_some_and(|n| n.to_string_lossy().starts_with("tmp_"))
            })
            .collect()
    }

    #[test]
    fn identical_writes_through_separate_handles_all_succeed() {
        let (_dir, store) = temp_store();
        let threads = 8;
        let barrier = std::sync::Arc::new(std::sync::Barrier::new(threads));
        let payloads: Vec<Vec<u8>> = (0..200)
            .map(|j| format!("same {j}").into_bytes())
            .chain(std::iter::once(vec![0x5a; 1024 * 1024]))
            .collect();
        let payloads = std::sync::Arc::new(payloads);
        let handles: Vec<_> = (0..threads)
            .map(|_| {
                let store = store.clone();
                let barrier = std::sync::Arc::clone(&barrier);
                let payloads = std::sync::Arc::clone(&payloads);
                std::thread::spawn(move || {
                    barrier.wait();
                    payloads
                        .iter()
                        .map(|data| {
                            WritableObjectStore::write(
                                &store,
                                ObjectKind::Blob,
                                data,
                                WriteOptions::default(),
                            )
                        })
                        .collect::<Vec<_>>()
                })
            })
            .collect();
        for handle in handles {
            for (data, result) in payloads.iter().zip(handle.join().unwrap()) {
                let oid = result.expect("concurrent identical write succeeds");
                let obj = ObjectStore::read(&store, &oid).unwrap().expect("object");
                assert!(obj.data == *data);
            }
        }
        assert!(tmp_entries(&store).is_empty());
    }

    #[test]
    fn prehashed_write_recreates_pruned_fanout_dir_marked_ready() {
        let (_dir, store) = temp_store();
        let data = b"fanout pruned under a ready writer";
        let oid = hash::hash_object(HashAlgo::Sha1, ObjectKind::Blob, data).unwrap();
        let zlib = crate::odb::zlib_compress_blob_loose(data, Compression::default()).unwrap();
        let options = WriteOptions {
            loose_prefix_dirs_ready: true,
            ..WriteOptions::default()
        };
        assert!(!store.object_path(&oid).parent().unwrap().exists());
        let written = store.write_zlib_prehashed(&oid, &zlib, options).unwrap();
        assert_eq!(written, oid);
        assert_eq!(ObjectStore::read(&store, &oid).unwrap().unwrap().data, data);
        assert!(tmp_entries(&store).is_empty());
    }

    #[test]
    fn unique_tmp_paths_differ_and_are_not_loose_names() {
        let prefix = Path::new("objects/ab");
        let a = unique_loose_tmp_path(prefix);
        let b = unique_loose_tmp_path(prefix);
        assert_ne!(a, b);
        for path in [a, b] {
            let name = path.file_name().unwrap().to_str().unwrap();
            assert!(name.starts_with("tmp_obj_"), "{name}");
            assert!(!is_loose_suffix_for_algo(name, HashAlgo::Sha1));
            assert!(!is_loose_suffix_for_algo(name, HashAlgo::Sha256));
        }
    }

    #[test]
    fn read_info_and_read_agree_for_object_spanning_read_chunks() {
        let (_dir, store) = temp_store();
        let data: Vec<u8> = (0..3 * READ_CHUNK as u32)
            .map(|i| (i.wrapping_mul(2_654_435_761) >> 13) as u8)
            .collect();
        let oid =
            WritableObjectStore::write(&store, ObjectKind::Blob, &data, WriteOptions::default())
                .unwrap();
        let info = ObjectStore::read_info(&store, &oid).unwrap().expect("info");
        assert_eq!(info.kind, ObjectKind::Blob);
        assert_eq!(info.size, data.len() as u64);
        let obj = ObjectStore::read(&store, &oid).unwrap().expect("object");
        assert_eq!(obj.kind, ObjectKind::Blob);
        assert_eq!(obj.data, data);
        let mut buf = Vec::new();
        let kind = ObjectStore::read_into(&store, &oid, &mut buf).unwrap();
        assert_eq!(kind, Some(ObjectKind::Blob));
        assert_eq!(buf, data);
    }

    #[test]
    fn read_unless_larger_reads_small_and_skips_large_objects() {
        let (_dir, store) = temp_store();
        let data = vec![b'x'; 3 * READ_CHUNK];
        let oid =
            WritableObjectStore::write(&store, ObjectKind::Blob, &data, WriteOptions::default())
                .unwrap();
        match store.read_unless_larger(&oid, data.len() as u64).unwrap() {
            Some(BoundedRead::Object(obj)) => {
                assert_eq!(obj.kind, ObjectKind::Blob);
                assert_eq!(obj.data, data);
            }
            other => panic!("expected the object, got {other:?}"),
        }
        assert!(matches!(
            store
                .read_unless_larger(&oid, data.len() as u64 - 1)
                .unwrap(),
            Some(BoundedRead::Larger)
        ));
        let missing = ObjectId::from_hex("1234567890123456789012345678901234567890").unwrap();
        assert!(store.read_unless_larger(&missing, 0).unwrap().is_none());
    }

    #[test]
    fn stream_reader_delivers_payload_through_tiny_and_huge_buffers() {
        let (_dir, store) = temp_store();
        let data: Vec<u8> = (0..5 * READ_CHUNK as u32)
            .map(|i| (i.wrapping_mul(2_654_435_761) >> 11) as u8)
            .collect();
        let oid =
            WritableObjectStore::write(&store, ObjectKind::Blob, &data, WriteOptions::default())
                .unwrap();
        for chunk_len in [1usize, 7, 4096, 10 * READ_CHUNK] {
            let mut stream = ObjectStore::open_stream(&store, &oid)
                .unwrap()
                .expect("stream");
            let mut out = Vec::new();
            let mut chunk = vec![0u8; chunk_len];
            loop {
                let n = stream.reader.read(&mut chunk).unwrap();
                if n == 0 {
                    break;
                }
                out.extend_from_slice(&chunk[..n]);
            }
            assert!(out == data, "chunk length {chunk_len}");
        }
    }

    #[test]
    fn read_info_is_none_for_missing_object_or_directory_at_its_path() {
        let (_dir, store) = temp_store();
        let missing = ObjectId::from_hex("1234567890123456789012345678901234567890").unwrap();
        assert!(ObjectStore::read_info(&store, &missing).unwrap().is_none());
        fs::create_dir_all(store.object_path(&missing)).unwrap();
        assert!(ObjectStore::read_info(&store, &missing).unwrap().is_none());
    }

    #[test]
    fn lookup_prefix_one_hex_char_does_not_overflow() {
        let dir = tempfile::tempdir().unwrap();
        let store = LooseStore::new(
            dir.path().join("objects"),
            HashAlgo::Sha1,
            Compression::default(),
        );
        fs::create_dir_all(store.objects_dir()).unwrap();
        let oid =
            WritableObjectStore::write(&store, ObjectKind::Blob, b"x", WriteOptions::default())
                .unwrap();
        let mut out = Vec::new();
        ObjectStore::lookup_prefix(&store, &oid.to_hex()[..1], 0, &mut out).unwrap();
        assert!(out.contains(&oid));
    }

    #[test]
    fn truncated_zlib_stream_errors_on_read() {
        let dir = tempfile::tempdir().unwrap();
        let store = LooseStore::new(
            dir.path().join("objects"),
            HashAlgo::Sha1,
            Compression::default(),
        );
        fs::create_dir_all(store.objects_dir()).unwrap();
        let payload = vec![0u8; 100_000];
        let oid =
            WritableObjectStore::write(&store, ObjectKind::Blob, &payload, WriteOptions::default())
                .unwrap();
        let path = store.object_path(&oid);
        let mut bytes = fs::read(&path).unwrap();
        bytes.truncate(bytes.len() / 2);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();
        }
        fs::write(&path, &bytes).unwrap();
        let mut stream = ObjectStore::open_stream(&store, &oid)
            .unwrap()
            .expect("stream");
        let mut out = Vec::new();
        let err = stream.reader.read_to_end(&mut out).unwrap_err();
        assert!(
            err.to_string().contains("payload bytes remaining")
                || err.to_string().contains("corrupt object"),
            "unexpected error: {err}"
        );
        assert!(out.len() < payload.len());
    }

    #[test]
    fn truncated_zlib_checksum_only_errors_on_read() {
        let dir = tempfile::tempdir().unwrap();
        let store = LooseStore::new(
            dir.path().join("objects"),
            HashAlgo::Sha1,
            Compression::default(),
        );
        fs::create_dir_all(store.objects_dir()).unwrap();
        let payload = vec![0u8; 100_000];
        let oid =
            WritableObjectStore::write(&store, ObjectKind::Blob, &payload, WriteOptions::default())
                .unwrap();
        let path = store.object_path(&oid);
        let mut bytes = fs::read(&path).unwrap();
        assert!(bytes.len() > 1);
        bytes.pop();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();
        }
        fs::write(&path, &bytes).unwrap();
        match ObjectStore::read(&store, &oid) {
            Ok(None) => {}
            Ok(Some(_)) => panic!("full read must not succeed on checksum-truncated loose object"),
            Err(_) => {}
        }
        let mut stream = ObjectStore::open_stream(&store, &oid)
            .expect("open_stream")
            .expect("stream hit");
        let mut out = Vec::new();
        let err = stream.reader.read_to_end(&mut out).unwrap_err();
        assert!(
            err.to_string().contains("trailer validation")
                || err.to_string().contains("corrupt object"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn stream_sixteen_mib_peak_buffer_under_one_mib() {
        let dir = tempfile::tempdir().unwrap();
        let store = LooseStore::new(
            dir.path().join("objects"),
            HashAlgo::Sha1,
            Compression::default(),
        );
        fs::create_dir_all(store.objects_dir()).unwrap();
        let payload = vec![0xCDu8; 16 * 1024 * 1024];
        let oid =
            WritableObjectStore::write(&store, ObjectKind::Blob, &payload, WriteOptions::default())
                .unwrap();
        let mut stream = ObjectStore::open_stream(&store, &oid)
            .unwrap()
            .expect("stream");
        assert_eq!(stream.size, payload.len() as u64);
        let mut out = Vec::new();
        let mut chunk = [0u8; 64 * 1024];
        let mut peak = 0usize;
        loop {
            let n = stream.reader.read(&mut chunk).unwrap();
            if n == 0 {
                break;
            }
            peak = peak.max(n);
            out.extend_from_slice(&chunk[..n]);
        }
        assert_eq!(out, payload);
        assert!(
            peak < 1024 * 1024,
            "peak in-flight buffer {peak} must stay under 1 MiB"
        );
    }

    #[test]
    fn external_loose_write_in_fanout_visible_after_negative_cache() {
        use std::process::{Command, Stdio};

        use crate::odb::Odb;

        let dir = tempfile::tempdir().unwrap();
        let objects = dir.path().join("objects");
        fs::create_dir_all(&objects).unwrap();
        let odb = Odb::new(&objects);
        let payload = b"written externally into an existing fanout dir\n";
        let oid = odb.hash(ObjectKind::Blob, payload).unwrap();
        assert!(matches!(odb.read(&oid), Err(Error::ObjectNotFound(_))));

        let mut child = Command::new("git")
            .args(["hash-object", "-w", "--stdin"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .env("GIT_OBJECT_DIRECTORY", &objects)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_SYSTEM", "/dev/null")
            .spawn()
            .expect("git hash-object");
        if let Some(mut stdin) = child.stdin.take() {
            use std::io::Write;
            stdin.write_all(payload).unwrap();
        }
        let out = child.wait_with_output().expect("git wait");
        assert!(
            out.status.success(),
            "git hash-object: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        assert_eq!(String::from_utf8_lossy(&out.stdout).trim(), oid.to_hex());

        let obj = odb
            .read(&oid)
            .expect("external loose write must be visible");
        assert_eq!(obj.data, payload);
    }
}

//! Loose-object storage under `objects/xx/<suffix>` (zlib-compressed Git objects).

use std::fs::{self, File};
use std::io::{self, Read, Write};
use std::ops::ControlFlow;
use std::path::{Path, PathBuf};

use flate2::write::ZlibEncoder;
use flate2::Compression;
use flate2::{Decompress, FlushDecompress, Status};

use super::{BoundedRead, ObjectStore, ObjectStream, WritableObjectStore};
use crate::error::{Error, Result};
use crate::hash;
use crate::objects::{HashAlgo, Object, ObjectId, ObjectIdPrefix, ObjectInfo, ObjectKind};
use crate::odb::WriteOptions;
use crate::zlib_inflate::with_loose_scratch;

const READ_CHUNK: usize = 64 * 1024;

/// On-disk loose object store for one `objects/` directory.
#[derive(Debug, Clone)]
pub struct LooseStore {
    objects_dir: PathBuf,
    hash_algo: HashAlgo,
    compression: Compression,
}

impl LooseStore {
    /// Open a loose store at `objects_dir` using `hash_algo` and zlib `compression`.
    #[must_use]
    pub fn new(objects_dir: PathBuf, hash_algo: HashAlgo, compression: Compression) -> Self {
        Self {
            objects_dir,
            hash_algo,
            compression,
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
            fs::create_dir_all(prefix).map_err(Error::Io)?;
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
        publish_zlib_loose_object(&path, prefix_dir, oid, &zlib, false)?;
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
        publish_zlib_loose_object(
            &path,
            prefix_dir,
            oid,
            zlib_store,
            options.loose_prefix_dirs_ready,
        )?;
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
        let raw = read_zlib_loose_payload(file)?;
        let obj = parse_object_bytes_with_oid(&raw, expected_oid)?;
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
        let now = filetime::FileTime::now();
        filetime::set_file_times(path, now, now).is_ok()
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
        let file = match fs::File::open(&path) {
            Ok(f) => f,
            Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(err) => return Err(Error::Io(err)),
        };
        read_zlib_loose_payload_into(file, buf)?;
        let kind = strip_decompressed_object_header(buf, Some(oid))?;
        Ok(Some(kind))
    }

    fn read_info(&self, oid: &ObjectId) -> Result<Option<ObjectInfo>> {
        let path = self.object_path(oid);
        let mut file = match fs::File::open(&path) {
            Ok(f) => f,
            Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(err) => return Err(Error::Io(err)),
        };
        match loose_object_info_from_file(&mut file) {
            Err(Error::Io(err)) if err.kind() == io::ErrorKind::IsADirectory => Ok(None),
            other => other.map(Some),
        }
    }

    fn open_stream(&self, oid: &ObjectId) -> Result<Option<ObjectStream<'_>>> {
        let path = self.object_path(oid);
        let file = match fs::File::open(&path) {
            Ok(f) => f,
            Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(err) => return Err(Error::Io(err)),
        };
        let reader = LoosePayloadReader::open(file)?;
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
        let (store_bytes, oid) = build_store_bytes_and_hash(self.hash_algo, kind, data)?;
        if let Some(existing) = self.try_finish_if_loose_present(&oid, options)? {
            return Ok(existing);
        }
        let path = self.object_path(&oid);
        let prefix_dir = path
            .parent()
            .ok_or_else(|| Error::PathError("object path has no parent".to_owned()))?;
        fs::create_dir_all(prefix_dir).map_err(Error::Io)?;
        let tmp_path = oid.loose_tmp_path_in_prefix(prefix_dir);
        {
            let tmp_file = fs::File::create(&tmp_path).map_err(Error::Io)?;
            let mut encoder = ZlibEncoder::new(tmp_file, self.compression);
            encoder
                .write_all(&store_bytes)
                .map_err(|e| Error::Zlib(e.to_string()))?;
            encoder.finish().map_err(|e| Error::Zlib(e.to_string()))?;
        }
        fs::rename(&tmp_path, &path).map_err(|e| {
            let _ = fs::remove_file(&tmp_path);
            Error::Io(e)
        })?;
        set_loose_object_mode(&path);
        Ok(oid)
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
fn for_each_object_id_in_fanout_dir(
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
    loose_object_info_from_file(&mut file)
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

/// Build the canonical store byte sequence: `"<kind> <len>\0<data>"`.
pub(crate) fn build_store_bytes(kind: ObjectKind, data: &[u8]) -> Vec<u8> {
    let header = format!("{} {}\0", kind, data.len());
    let mut out = Vec::with_capacity(header.len() + data.len());
    out.extend_from_slice(header.as_bytes());
    out.extend_from_slice(data);
    out
}

/// Build canonical store bytes and the object id for a loose write.
pub(crate) fn build_store_bytes_and_hash(
    algo: HashAlgo,
    kind: ObjectKind,
    data: &[u8],
) -> Result<(Vec<u8>, ObjectId)> {
    let store_bytes = build_store_bytes(kind, data);
    let oid = hash::hash_object(algo, kind, data)?;
    Ok((store_bytes, oid))
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

fn publish_loose_tmp_path(prefix_dir: &Path, oid: &ObjectId, attempt: u32) -> PathBuf {
    let base = oid.loose_tmp_path_in_prefix(prefix_dir);
    let stem = base.file_name().and_then(|n| n.to_str()).unwrap_or("tmp");
    let thread = format!("{:?}", std::thread::current().id()).replace(['/', '\\', ' '], "_");
    prefix_dir.join(format!("{stem}.{}.{thread}.{attempt}", std::process::id()))
}

fn publish_zlib_loose_object(
    path: &Path,
    prefix_dir: &Path,
    oid: &ObjectId,
    zlib_store: &[u8],
    prefix_dir_ready: bool,
) -> Result<()> {
    if path.is_file() {
        set_loose_object_mode(path);
        return Ok(());
    }
    const MAX_ATTEMPTS: u32 = 4;
    for attempt in 0..MAX_ATTEMPTS {
        if !prefix_dir_ready {
            fs::create_dir_all(prefix_dir).map_err(Error::Io)?;
        }
        let tmp_path = publish_loose_tmp_path(prefix_dir, oid, attempt);
        match fs::write(&tmp_path, zlib_store) {
            Ok(()) => match fs::rename(&tmp_path, path) {
                Ok(()) => {
                    set_loose_object_mode(path);
                    return Ok(());
                }
                Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {
                    let _ = fs::remove_file(&tmp_path);
                    if path.is_file() {
                        set_loose_object_mode(path);
                        return Ok(());
                    }
                }
                Err(e) => {
                    let _ = fs::remove_file(&tmp_path);
                    if e.kind() == io::ErrorKind::NotFound && attempt + 1 < MAX_ATTEMPTS {
                        continue;
                    }
                    return Err(Error::Io(e));
                }
            },
            Err(e) => {
                let _ = fs::remove_file(&tmp_path);
                if e.kind() == io::ErrorKind::NotFound && attempt + 1 < MAX_ATTEMPTS {
                    continue;
                }
                return Err(Error::Io(e));
            }
        }
    }
    Err(Error::Io(io::Error::other(
        "exhausted loose object publish retries",
    )))
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
        let mut hdr = [0u8; 2];
        file.read_exact(&mut hdr).map_err(Error::Io)?;
        let cmf_flg = u16::from(hdr[0]) << 8 | u16::from(hdr[1]);
        let looks_like_zlib_header = cmf_flg != 0 && cmf_flg % 31 == 0;
        let preset_dictionary = looks_like_zlib_header && (hdr[1] & 0x20) != 0;
        let mut reader = Self {
            file,
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
        let mut out_chunk = [0u8; READ_CHUNK];
        let produced = self.inflate_into(&mut out_chunk)?;
        on_out(&out_chunk[..produced]);
        Ok(())
    }

    /// Inflate into `out`, reading more compressed input when the pending input is used up.
    ///
    /// Returns the bytes produced, which is zero once the zlib stream has ended.
    fn inflate_into(&mut self, out: &mut [u8]) -> Result<usize> {
        if self.zlib_complete {
            return Ok(0);
        }
        if self.pending_pos == self.pending.len() && !self.input_exhausted {
            self.pending.resize(READ_CHUNK, 0);
            let n = loop {
                match self.file.read(&mut self.pending) {
                    Ok(n) => break n,
                    Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                    Err(e) => return Err(Error::Io(e)),
                }
            };
            self.pending.truncate(n);
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
}

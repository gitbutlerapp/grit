//! Loose-object storage under `objects/xx/<suffix>` (zlib-compressed Git objects).

use std::fs::{self, File};
use std::io::{self, Read, Write};
use std::ops::ControlFlow;
use std::path::{Path, PathBuf};

use flate2::read::ZlibDecoder;
use flate2::write::ZlibEncoder;
use flate2::Compression;
use flate2::{Decompress, FlushDecompress, Status};

use super::{ObjectStore, ObjectStream, WritableObjectStore};
use crate::error::{Error, Result};
use crate::hash;
use crate::objects::{HashAlgo, Object, ObjectId, ObjectInfo, ObjectKind};
use crate::odb::WriteOptions;
use crate::zlib_inflate::ZlibInflateScratch;

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
        let path = self.object_path(oid);
        if path.is_file() {
            if !options.silent {
                let _ = self.freshen(oid)?;
            }
            return Ok(*oid);
        }
        let prefix_dir = path
            .parent()
            .ok_or_else(|| Error::PathError("object path has no parent".to_owned()))?;
        let zlib = zlib_compress_store_bytes(store_bytes, self.compression)?;
        publish_zlib_loose_object(&path, prefix_dir, oid, &zlib)?;
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
        let path = self.object_path(oid);
        if path.is_file() {
            if !options.silent {
                let _ = self.freshen(oid)?;
            }
            return Ok(*oid);
        }
        let prefix_dir = path
            .parent()
            .ok_or_else(|| Error::PathError("object path has no parent".to_owned()))?;
        publish_zlib_loose_object(&path, prefix_dir, oid, zlib_store)?;
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
        let computed = hash::hash_object(expected_oid.algo(), obj.kind, &obj.data);
        if computed != *expected_oid {
            return Err(Error::LooseHashMismatch {
                path: path.display().to_string(),
                real_oid: computed.to_hex(),
            });
        }
        Ok(obj)
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
        let path = self.object_path(oid);
        match fs::File::open(&path) {
            Ok(file) => {
                let raw = read_zlib_loose_payload(file)?;
                Ok(Some(parse_object_bytes(&raw)?))
            }
            Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(err) => Err(Error::Io(err)),
        }
    }

    fn read_info(&self, oid: &ObjectId) -> Result<Option<ObjectInfo>> {
        let path = self.object_path(oid);
        if !path.is_file() {
            return Ok(None);
        }
        Ok(Some(read_loose_object_info(&path)?))
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
        let prefix = normalize_oid_prefix(prefix, self.hash_algo)?;
        if prefix.len() < 2 {
            return lookup_prefix_scan_all(
                &self.objects_dir,
                self.hash_algo,
                prefix.as_str(),
                limit,
                out,
            );
        }
        let fanout = &prefix[..2];
        let fanout_dir = self.objects_dir.join(fanout);
        if !fanout_dir.is_dir() {
            return Ok(());
        }
        let start_len = out.len();
        let sub = match fs::read_dir(&fanout_dir) {
            Ok(rd) => rd,
            Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(()),
            Err(err) => return Err(Error::Io(err)),
        };
        for entry in sub {
            let entry = entry.map_err(Error::Io)?;
            if !entry.file_type().map_err(Error::Io)?.is_file() {
                continue;
            }
            let suffix_name = entry.file_name();
            let Some(suffix) = suffix_name.to_str() else {
                continue;
            };
            if !is_loose_suffix_for_algo(suffix, self.hash_algo) {
                continue;
            }
            let hex = format!("{fanout}{suffix}");
            let Ok(oid) = ObjectId::from_hex(&hex) else {
                continue;
            };
            if oid_hex_has_prefix(&oid, prefix.as_str()) {
                out.push(oid);
                if limit != 0 && out.len() - start_len >= limit {
                    break;
                }
            }
        }
        Ok(())
    }
}

impl WritableObjectStore for LooseStore {
    fn write(&self, kind: ObjectKind, data: &[u8], options: WriteOptions) -> Result<ObjectId> {
        let store_bytes = build_store_bytes(kind, data);
        let oid = hash::hash_object(self.hash_algo, kind, data);
        debug_assert_eq!(oid, hash::hash_object(self.hash_algo, kind, data));
        let path = self.object_path(&oid);
        if path.is_file() {
            if !options.silent {
                let _ = self.freshen(&oid)?;
            }
            return Ok(oid);
        }
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

        let sub = match fs::read_dir(top_entry.path()) {
            Ok(rd) => rd,
            Err(e) if e.kind() == io::ErrorKind::NotFound => continue,
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
            let hex = format!("{prefix}{suffix}");
            let Ok(oid) = ObjectId::from_hex(&hex) else {
                continue;
            };
            if f(&oid).is_break() {
                return Ok(());
            }
        }
    }
    Ok(())
}

/// Decompress a zlib-wrapped loose object payload from an open file.
pub fn read_zlib_loose_payload(file: File) -> Result<Vec<u8>> {
    let mut hdr = [0u8; 2];
    let mut read_file = file;
    read_file.read_exact(&mut hdr).map_err(Error::Io)?;
    let cmf_flg = u16::from(hdr[0]) << 8 | u16::from(hdr[1]);
    let looks_like_zlib_header = cmf_flg != 0 && cmf_flg % 31 == 0;
    let preset_dictionary = looks_like_zlib_header && (hdr[1] & 0x20) != 0;
    let mut scratch = ZlibInflateScratch::default();
    scratch.decompress_loose_payload(&hdr, read_file, preset_dictionary)
}

/// Read kind and size from a loose object file without loading the full payload.
pub fn read_loose_object_info(path: &Path) -> Result<ObjectInfo> {
    let file = fs::File::open(path).map_err(Error::Io)?;
    let mut decoder = ZlibDecoder::new(file);
    let mut prefix = Vec::with_capacity(64);
    let mut buf = [0u8; 256];
    loop {
        let n = decoder
            .read(&mut buf)
            .map_err(|e| Error::Zlib(e.to_string()))?;
        if n == 0 {
            break;
        }
        prefix.extend_from_slice(&buf[..n]);
        if prefix.contains(&0) || prefix.len() >= 128 {
            break;
        }
    }
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
            oid: hash_algo_digest_hex(HashAlgo::Sha1, raw),
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
        let oid_str = oid_hint
            .map(|o| o.to_hex())
            .unwrap_or_else(|| hash_algo_digest_hex(HashAlgo::Sha1, raw));
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

fn hash_algo_digest_hex(algo: HashAlgo, data: &[u8]) -> String {
    algo.digest(data).to_hex()
}

fn publish_zlib_loose_object(
    path: &Path,
    prefix_dir: &Path,
    oid: &ObjectId,
    zlib_store: &[u8],
) -> Result<()> {
    fs::create_dir_all(prefix_dir).map_err(Error::Io)?;
    let tmp_path = oid.loose_tmp_path_in_prefix(prefix_dir);
    if let Err(e) = fs::write(&tmp_path, zlib_store) {
        let _ = fs::remove_file(&tmp_path);
        return Err(Error::Io(e));
    }
    match fs::rename(&tmp_path, path) {
        Ok(()) => {}
        Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {
            let _ = fs::remove_file(&tmp_path);
        }
        Err(e) => {
            let _ = fs::remove_file(&tmp_path);
            return Err(Error::Io(e));
        }
    }
    set_loose_object_mode(path);
    Ok(())
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

fn normalize_oid_prefix(prefix: &str, algo: HashAlgo) -> Result<String> {
    if prefix.is_empty() {
        return Ok(String::new());
    }
    if !prefix.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err(Error::InvalidObjectId(prefix.to_owned()));
    }
    let max = algo.hex_len();
    if prefix.len() > max {
        return Err(Error::InvalidObjectId(prefix.to_owned()));
    }
    Ok(prefix.to_ascii_lowercase())
}

fn oid_hex_has_prefix(oid: &ObjectId, prefix: &str) -> bool {
    if prefix.is_empty() {
        return true;
    }
    oid.to_hex().starts_with(prefix)
}

fn lookup_prefix_scan_all(
    objects_dir: &Path,
    hash_algo: HashAlgo,
    prefix: &str,
    limit: usize,
    out: &mut Vec<ObjectId>,
) -> Result<()> {
    let start_len = out.len();
    for_each_loose_object_id(objects_dir, hash_algo, &mut |oid| {
        if oid_hex_has_prefix(oid, prefix) {
            out.push(*oid);
            if limit != 0 && out.len() - start_len >= limit {
                return ControlFlow::Break(());
            }
        }
        ControlFlow::Continue(())
    })
}

/// Incremental zlib reader yielding only the object payload (after the Git header).
struct LoosePayloadReader {
    file: File,
    dec: Decompress,
    pending: Vec<u8>,
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
            if self.input_exhausted && self.pending.is_empty() {
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
        if self.zlib_complete {
            return Ok(());
        }
        if self.pending.is_empty() && !self.input_exhausted {
            let mut buf = [0u8; READ_CHUNK];
            let n = self.file.read(&mut buf).map_err(Error::Io)?;
            if n == 0 {
                self.input_exhausted = true;
            } else {
                self.pending.extend_from_slice(&buf[..n]);
            }
        }
        let flush = if self.input_exhausted && self.pending.is_empty() {
            FlushDecompress::Finish
        } else {
            FlushDecompress::None
        };
        let before_in = self.dec.total_in();
        let before_out = self.dec.total_out();
        let mut out_chunk = [0u8; READ_CHUNK];
        let status = match self
            .dec
            .decompress(self.pending.as_slice(), &mut out_chunk, flush)
        {
            Ok(s) => s,
            Err(e) => {
                if self.preset_dictionary {
                    return Err(Error::Zlib("needs dictionary".to_owned()));
                }
                return Err(Error::Zlib(e.to_string()));
            }
        };
        let consumed = (self.dec.total_in() - before_in) as usize;
        if consumed > self.pending.len() {
            return Err(Error::CorruptObject(
                "zlib consumed more than pending buffer".to_owned(),
            ));
        }
        self.pending.drain(..consumed);
        let produced = (self.dec.total_out() - before_out) as usize;
        on_out(&out_chunk[..produced]);
        if matches!(status, Status::StreamEnd) {
            self.zlib_complete = true;
        }
        Ok(())
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
            if self.input_exhausted && self.pending.is_empty() {
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
            let mut out_chunk = Vec::new();
            self.pump_zlib(&mut |bytes| out_chunk.extend_from_slice(bytes))
                .map_err(|e| io::Error::other(e.to_string()))?;
            if out_chunk.is_empty() {
                if self.input_exhausted && !self.zlib_complete {
                    return Err(io::Error::other(format!(
                        "corrupt object: zlib stream ended with {} payload bytes remaining",
                        self.payload_remaining
                    )));
                }
                continue;
            }
            let take = out_chunk
                .len()
                .min(buf.len() - delivered)
                .min(self.payload_remaining as usize);
            buf[delivered..delivered + take].copy_from_slice(&out_chunk[..take]);
            if take < out_chunk.len() {
                self.extra_payload.extend_from_slice(&out_chunk[take..]);
            }
            delivered += take;
            self.payload_remaining -= take as u64;
        }
        if self.payload_remaining == 0 {
            self.ensure_zlib_complete()?;
        }
        Ok(delivered)
    }
}

pub(crate) fn loose_store_bytes_header_valid(raw: &[u8]) -> bool {
    let nul = match raw.iter().position(|&b| b == 0) {
        Some(i) => i,
        None => return false,
    };
    let header = &raw[..nul];
    let data = &raw[nul + 1..];
    let sp = match header.iter().position(|&b| b == b' ') {
        Some(i) => i,
        None => return false,
    };
    if sp == 0 || sp > 32 {
        return false;
    }
    let size_str = match std::str::from_utf8(&header[sp + 1..]) {
        Ok(s) => s,
        Err(_) => return false,
    };
    let size: usize = match size_str.parse() {
        Ok(s) => s,
        Err(_) => return false,
    };
    data.len() == size
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

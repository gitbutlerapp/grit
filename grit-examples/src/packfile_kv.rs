//! Append-only single-file object store for embedding examples.
//!
//! Each object is stored as a length-prefixed zlib blob of canonical Git store bytes.
//! An in-memory index mapping [`ObjectId`] to file offsets is rebuilt when the file is opened.

use grit_lib::objects::ObjectError;
use std::collections::HashMap;
use std::fs::{File, OpenOptions};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::ops::ControlFlow;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, RwLock};

use flate2::read::ZlibDecoder;
use flate2::write::ZlibEncoder;
use flate2::Compression;
use grit_lib::error::{Error, Result};
use grit_lib::objects::hash;
use grit_lib::objects::odb::store::{LooseStore, ObjectStore, ObjectStream, WritableObjectStore};
use grit_lib::objects::odb::WriteOptions;
use grit_lib::objects::{HashAlgo, Object, ObjectId, ObjectInfo, ObjectKind, ObjectResult};

const MAGIC: &[u8; 8] = b"GRITPKV1";
const HEADER_LEN: u64 = 16;

/// On-disk packfile key-value object store (one append-only file).
#[derive(Debug)]
pub struct PackfileKvStore {
    path: PathBuf,
    algo: HashAlgo,
    compression: Compression,
    index: RwLock<HashMap<ObjectId, u64>>,
    append: Mutex<()>,
}

impl PackfileKvStore {
    /// Create a new store file at `path`, truncating any existing file.
    ///
    /// # Errors
    ///
    /// Returns I/O errors from creating or writing the file header.
    pub fn create(path: impl AsRef<Path>, algo: HashAlgo) -> Result<Self> {
        let path = path.as_ref().to_path_buf();
        let mut file = OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .open(&path)
            .map_err(Error::Io)?;
        write_header(&mut file, algo)?;
        Ok(Self {
            path,
            algo,
            compression: Compression::default(),
            index: RwLock::new(HashMap::new()),
            append: Mutex::new(()),
        })
    }

    /// Open an existing store and rebuild the in-memory index from the file tail.
    ///
    /// # Errors
    ///
    /// Returns I/O, format, or corruption errors while scanning records.
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref().to_path_buf();
        let mut file = OpenOptions::new()
            .read(true)
            .write(true)
            .open(&path)
            .map_err(Error::Io)?;
        let algo = read_header(&mut file)?;
        let index = rebuild_index(&mut file, algo)?;
        Ok(Self {
            path,
            algo,
            compression: Compression::default(),
            index: RwLock::new(index),
            append: Mutex::new(()),
        })
    }

    /// Path to the backing file.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Copy every object into `loose` as zlib-compressed loose files.
    ///
    /// # Errors
    ///
    /// Propagates read or loose-write failures.
    pub fn export_to_loose(&self, loose: &LooseStore) -> Result<()> {
        loose.ensure_all_loose_prefix_dirs()?;
        let ids: Vec<ObjectId> = {
            let guard = self
                .index
                .read()
                .map_err(|e| Error::Io(io::Error::other(e.to_string())))?;
            guard.keys().copied().collect()
        };
        for oid in ids {
            let Some(obj) = self.read(&oid)? else {
                continue;
            };
            WritableObjectStore::write(loose, obj.kind, &obj.data, WriteOptions::default())?;
        }
        Ok(())
    }

    fn read_zlib_record(&self, offset: u64) -> Result<Vec<u8>> {
        let mut file = File::open(&self.path).map_err(Error::Io)?;
        file.seek(SeekFrom::Start(offset)).map_err(Error::Io)?;
        let len = read_u32_be(&mut file)? as usize;
        let mut zlib = vec![0u8; len];
        file.read_exact(&mut zlib).map_err(Error::Io)?;
        let mut decoder = ZlibDecoder::new(zlib.as_slice());
        let mut raw = Vec::new();
        decoder.read_to_end(&mut raw).map_err(|e| {
            Error::from(ObjectError::Zlib {
                detail: e.to_string(),
            })
        })?;
        Ok(raw)
    }

    fn append_record(&self, store_bytes: &[u8]) -> Result<u64> {
        let mut zlib = Vec::new();
        {
            let mut enc = ZlibEncoder::new(&mut zlib, self.compression);
            enc.write_all(store_bytes).map_err(Error::Io)?;
            enc.finish().map_err(|e| {
                Error::from(ObjectError::Zlib {
                    detail: e.to_string(),
                })
            })?;
        }
        let _guard = self
            .append
            .lock()
            .map_err(|e| Error::Io(io::Error::other(e.to_string())))?;
        let mut file = OpenOptions::new()
            .append(true)
            .open(&self.path)
            .map_err(Error::Io)?;
        file.seek(SeekFrom::End(0)).map_err(Error::Io)?;
        let offset = file.stream_position().map_err(Error::Io)?;
        write_u32_be(
            &mut file,
            u32::try_from(zlib.len()).map_err(|_| {
                Error::Object(ObjectError::corrupt_object(
                    None,
                    "zlib record length overflow".to_owned(),
                ))
            })?,
        )?;
        file.write_all(&zlib).map_err(Error::Io)?;
        file.sync_all().map_err(Error::Io)?;
        Ok(offset)
    }
}

impl ObjectStore for PackfileKvStore {
    fn hash_algo(&self) -> HashAlgo {
        self.algo
    }

    fn read(&self, oid: &ObjectId) -> ObjectResult<Option<Object>> {
        let offset = {
            let guard = self
                .index
                .read()
                .map_err(|e| ObjectError::from(Error::Io(io::Error::other(e.to_string()))))?;
            guard.get(oid).copied()
        };
        let Some(offset) = offset else {
            return Ok(None);
        };
        let raw = self.read_zlib_record(offset)?;
        let obj = parse_store_bytes(&raw)?;
        let computed = hash::hash_object(self.algo, obj.kind, &obj.data)?;
        if computed != *oid {
            return Err(ObjectError::corrupt_object(
                None,
                format!("packfile kv hash mismatch for {}", oid.to_hex()),
            ));
        }
        Ok(Some(obj))
    }

    fn read_info(&self, oid: &ObjectId) -> ObjectResult<Option<ObjectInfo>> {
        let offset = {
            let guard = self
                .index
                .read()
                .map_err(|e| ObjectError::from(Error::Io(io::Error::other(e.to_string()))))?;
            guard.get(oid).copied()
        };
        let Some(offset) = offset else {
            return Ok(None);
        };
        let raw = self.read_zlib_record(offset)?;
        Ok(Some(info_from_store_bytes(&raw)?))
    }

    fn open_stream(&self, oid: &ObjectId) -> ObjectResult<Option<ObjectStream<'_>>> {
        let Some(obj) = self.read(oid)? else {
            return Ok(None);
        };
        let size = u64::try_from(obj.data.len()).map_err(|_| {
            ObjectError::corrupt_object(None, format!("object size overflow for {}", oid.to_hex()))
        })?;
        Ok(Some(ObjectStream {
            kind: obj.kind,
            size,
            reader: Box::new(io::Cursor::new(obj.data)),
        }))
    }

    fn for_each_object(&self, f: &mut dyn FnMut(&ObjectId) -> ControlFlow<()>) -> ObjectResult<()> {
        let guard = self
            .index
            .read()
            .map_err(|e| ObjectError::from(Error::Io(io::Error::other(e.to_string()))))?;
        for oid in guard.keys() {
            if f(oid).is_break() {
                break;
            }
        }
        Ok(())
    }

    fn lookup_prefix(
        &self,
        prefix: &str,
        limit: usize,
        out: &mut Vec<ObjectId>,
    ) -> ObjectResult<()> {
        let guard = self
            .index
            .read()
            .map_err(|e| ObjectError::from(Error::Io(io::Error::other(e.to_string()))))?;
        let prefix = prefix.to_ascii_lowercase();
        if !prefix.is_empty() && !prefix.bytes().all(|b| b.is_ascii_hexdigit()) {
            return Err(ObjectError::InvalidObjectId { input: prefix });
        }
        if prefix.len() > self.algo.hex_len() {
            return Err(ObjectError::InvalidObjectId { input: prefix });
        }
        let mut count = 0usize;
        for oid in guard.keys() {
            if prefix.is_empty() || oid.to_hex().starts_with(&prefix) {
                out.push(*oid);
                count += 1;
                if limit != 0 && count >= limit {
                    break;
                }
            }
        }
        Ok(())
    }
}

impl WritableObjectStore for PackfileKvStore {
    fn write(
        &self,
        kind: ObjectKind,
        data: &[u8],
        _options: WriteOptions,
    ) -> ObjectResult<ObjectId> {
        let oid = hash::hash_object(self.algo, kind, data)?;
        {
            let guard = self
                .index
                .read()
                .map_err(|e| ObjectError::from(Error::Io(io::Error::other(e.to_string()))))?;
            if guard.contains_key(&oid) {
                return Ok(oid);
            }
        }
        let store_bytes = build_store_bytes(kind, data);
        let offset = self.append_record(&store_bytes)?;
        let mut guard = self
            .index
            .write()
            .map_err(|e| ObjectError::from(Error::Io(io::Error::other(e.to_string()))))?;
        guard.entry(oid).or_insert(offset);
        Ok(oid)
    }

    fn freshen(&self, oid: &ObjectId) -> ObjectResult<bool> {
        let guard = self
            .index
            .read()
            .map_err(|e| ObjectError::from(Error::Io(io::Error::other(e.to_string()))))?;
        Ok(guard.contains_key(oid))
    }
}

fn write_header(file: &mut File, algo: HashAlgo) -> ObjectResult<()> {
    file.write_all(MAGIC)
        .map_err(|e| ObjectError::from(Error::Io(e)))?;
    file.write_all(&[algo_byte(algo)])
        .map_err(|e| ObjectError::from(Error::Io(e)))?;
    file.write_all(&[0u8; 7])
        .map_err(|e| ObjectError::from(Error::Io(e)))?;
    Ok(())
}

fn read_header(file: &mut File) -> ObjectResult<HashAlgo> {
    let mut magic = [0u8; 8];
    file.read_exact(&mut magic)
        .map_err(|e| ObjectError::from(Error::Io(e)))?;
    if &magic != MAGIC {
        return Err(ObjectError::corrupt_object(
            None,
            "invalid packfile kv magic".to_owned(),
        ));
    }
    let mut algo_byte = [0u8; 1];
    file.read_exact(&mut algo_byte)
        .map_err(|e| ObjectError::from(Error::Io(e)))?;
    let mut reserved = [0u8; 7];
    file.read_exact(&mut reserved)
        .map_err(|e| ObjectError::from(Error::Io(e)))?;
    parse_algo_byte(algo_byte[0])
}

fn algo_byte(algo: HashAlgo) -> u8 {
    match algo {
        HashAlgo::Sha1 => 1,
        HashAlgo::Sha256 => 2,
    }
}

fn parse_algo_byte(byte: u8) -> ObjectResult<HashAlgo> {
    match byte {
        1 => Ok(HashAlgo::Sha1),
        2 => Ok(HashAlgo::Sha256),
        _ => Err(ObjectError::corrupt_object(
            None,
            format!("unknown packfile kv algo byte {byte}"),
        )),
    }
}

fn rebuild_index(file: &mut File, algo: HashAlgo) -> ObjectResult<HashMap<ObjectId, u64>> {
    file.seek(SeekFrom::Start(HEADER_LEN))
        .map_err(|e| ObjectError::from(Error::Io(e)))?;
    let file_len = file
        .metadata()
        .map_err(|e| ObjectError::from(Error::Io(e)))?
        .len();
    let mut index = HashMap::new();
    let mut valid_end = HEADER_LEN;
    loop {
        let offset = file
            .stream_position()
            .map_err(|e| ObjectError::from(Error::Io(e)))?;
        if offset >= file_len {
            break;
        }
        let len = match read_u32_be_or_eof(file) {
            Ok(None) => break,
            Ok(Some(len)) => len as usize,
            Err(ObjectError::CorruptObject { .. }) => break,
            Err(err) => return Err(err),
        };
        let payload_start = file
            .stream_position()
            .map_err(|e| ObjectError::from(Error::Io(e)))?;
        let remaining = usize::try_from(file_len.saturating_sub(payload_start)).map_err(|_| {
            ObjectError::corrupt_object(
                None,
                "packfile kv record extends past address space".to_owned(),
            )
        })?;
        if len > remaining {
            break;
        }
        let mut zlib = vec![0u8; len];
        if let Err(err) = file.read_exact(&mut zlib) {
            if err.kind() == io::ErrorKind::UnexpectedEof {
                break;
            }
            return Err(ObjectError::from(Error::Io(err)));
        }
        let mut decoder = ZlibDecoder::new(zlib.as_slice());
        let mut raw = Vec::new();
        if decoder.read_to_end(&mut raw).is_err() {
            break;
        }
        let obj = match parse_store_bytes(&raw) {
            Ok(obj) => obj,
            Err(_) => break,
        };
        let oid = hash::hash_object(algo, obj.kind, &obj.data)?;
        index.entry(oid).or_insert(offset);
        valid_end = file
            .stream_position()
            .map_err(|e| ObjectError::from(Error::Io(e)))?;
    }
    if valid_end < file_len {
        file.set_len(valid_end)
            .map_err(|e| ObjectError::from(Error::Io(e)))?;
    }
    Ok(index)
}

fn build_store_bytes(kind: ObjectKind, data: &[u8]) -> Vec<u8> {
    let header = format!("{kind} {}\0", data.len());
    let mut store_bytes = header.into_bytes();
    store_bytes.extend_from_slice(data);
    store_bytes
}

fn parse_store_bytes(raw: &[u8]) -> ObjectResult<Object> {
    let nul = raw.iter().position(|&b| b == 0).ok_or_else(|| {
        ObjectError::corrupt_object(None, "missing object header terminator".to_owned())
    })?;
    let header = std::str::from_utf8(&raw[..nul])
        .map_err(|_| ObjectError::corrupt_object(None, "invalid object header utf-8".to_owned()))?;
    let mut parts = header.splitn(2, ' ');
    let kind_str = parts
        .next()
        .ok_or_else(|| ObjectError::corrupt_object(None, "empty object header".to_owned()))?;
    let size_str = parts
        .next()
        .ok_or_else(|| ObjectError::corrupt_object(None, "missing object size".to_owned()))?;
    let kind: ObjectKind = kind_str.parse().map_err(|_| {
        ObjectError::corrupt_object(None, format!("unknown object kind {kind_str}"))
    })?;
    let size: usize = size_str.parse().map_err(|_| {
        ObjectError::corrupt_object(None, format!("invalid object size {size_str}"))
    })?;
    let payload = raw.get(nul + 1..).ok_or_else(|| {
        ObjectError::corrupt_object(None, "object payload shorter than header".to_owned())
    })?;
    if payload.len() != size {
        return Err(ObjectError::corrupt_object(
            None,
            format!(
                "object size {size} does not match payload {}",
                payload.len()
            ),
        ));
    }
    Ok(Object {
        kind,
        data: payload.to_vec(),
    })
}

fn info_from_store_bytes(raw: &[u8]) -> ObjectResult<ObjectInfo> {
    let obj = parse_store_bytes(raw)?;
    Ok(ObjectInfo {
        kind: obj.kind,
        size: u64::try_from(obj.data.len())
            .map_err(|_| ObjectError::corrupt_object(None, "object size overflow".to_owned()))?,
    })
}

fn read_u32_be(reader: &mut impl Read) -> ObjectResult<u32> {
    read_u32_be_or_eof(reader)?.ok_or_else(|| {
        ObjectError::from(Error::Io(io::Error::new(
            io::ErrorKind::UnexpectedEof,
            "missing record length",
        )))
    })
}

/// Read a big-endian record length, or `None` at a clean end-of-file (zero bytes read).
///
/// A partial 1–3 byte prefix is corruption and returns [`Error::CorruptObject`].
fn read_u32_be_or_eof(reader: &mut impl Read) -> ObjectResult<Option<u32>> {
    let mut buf = [0u8; 4];
    let mut filled = 0usize;
    while filled < 4 {
        match reader.read(&mut buf[filled..]) {
            Ok(0) => {
                if filled == 0 {
                    return Ok(None);
                }
                return Err(ObjectError::corrupt_object(
                    None,
                    "truncated packfile kv record length prefix",
                ));
            }
            Ok(n) => filled += n,
            Err(err) => return Err(ObjectError::from(Error::Io(err))),
        }
    }
    Ok(Some(u32::from_be_bytes(buf)))
}

fn write_u32_be(writer: &mut impl Write, value: u32) -> ObjectResult<()> {
    writer
        .write_all(&value.to_be_bytes())
        .map_err(|e| ObjectError::from(Error::Io(e)))
}

/// End-to-end demo used by the `custom-object-store` example and integration test.
///
/// # Errors
///
/// Propagates repository, object-store, or ref errors.
pub fn run_custom_object_store_demo(repo_root: &Path) -> ObjectResult<Vec<String>> {
    use std::sync::Arc;

    use grit_lib::config::environment::RepositoryOptions;
    use grit_lib::objects::odb::OdbBuilder;
    use grit_lib::objects::{serialize_tree, TreeEntry};
    use grit_lib::refs::store::RefName;
    use grit_lib::repo::{InitOptions, Repository, RepositoryBuilder};
    use grit_lib::revwalk::rev_list::{rev_list, RevListOptions};

    Repository::init(
        repo_root,
        &InitOptions::new()
            .with_initial_branch("main")
            .with_ref_storage(grit_lib::refs::ref_storage::RefStorageFormat::Files),
    )?;
    let git_dir = repo_root.join(".git");
    let objects_dir = git_dir.join("objects");
    let pack_path = git_dir.join("packfile-kv.store");

    let kv = Arc::new(PackfileKvStore::create(&pack_path, HashAlgo::Sha1)?);
    let repo = RepositoryBuilder::new(RepositoryOptions::empty())
        .odb_builder(
            OdbBuilder::files(&objects_dir)
                .primary(kv.clone())
                .alternates(false),
        )
        .open(&git_dir, Some(repo_root))?;

    let blob = repo.odb().write(ObjectKind::Blob, b"hello packfile kv\n")?;
    let tree_body = serialize_tree(&[TreeEntry {
        mode: 0o100644,
        name: b"hello.txt".to_vec(),
        oid: blob,
    }]);
    let tree = repo.odb().write(ObjectKind::Tree, &tree_body)?;
    let commit_body = format!(
        "tree {tree}\nauthor Demo <demo@example.com> 1 +0000\ncommitter Demo <demo@example.com> 1 +0000\n\npackfile kv demo\n"
    );
    let commit = repo
        .odb()
        .write(ObjectKind::Commit, commit_body.as_bytes())?;
    // The commit lives in the custom store, which only the repository's own ref store checks.
    repo.references().update(
        &RefName::try_from("refs/heads/main").map_err(|e| Error::InvalidRef(e.to_string()))?,
        &commit,
    )?;

    let tip = commit.to_hex();
    let listed = rev_list(
        &repo,
        std::slice::from_ref(&tip),
        &[],
        &RevListOptions::default(),
    )?;
    let grit_log: Vec<String> = listed.commits.iter().map(ObjectId::to_hex).collect();

    let loose = LooseStore::new(objects_dir.clone(), HashAlgo::Sha1, Compression::default());
    kv.export_to_loose(&loose)?;

    Ok(grit_log)
}

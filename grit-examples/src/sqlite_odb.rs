//! SQLite-backed object store for embedding examples.
//!
//! Each object is stored as a row keyed by [`ObjectId`] with zlib-compressed canonical
//! Git store bytes (the same payload written under `objects/xx/…` for loose objects).

use std::io::{self, Read, Write};
use std::ops::ControlFlow;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use flate2::read::ZlibDecoder;
use flate2::write::ZlibEncoder;
use flate2::Compression;
use grit_lib::error::{Error, Result};
use grit_lib::hash;
use grit_lib::objects::{HashAlgo, Object, ObjectId, ObjectInfo, ObjectKind};
use grit_lib::odb::store::{LooseStore, ObjectStore, ObjectStream, WritableObjectStore};
use grit_lib::odb::WriteOptions;
use rusqlite::{params, Connection};

const SCHEMA_VERSION: i64 = 1;

/// On-disk SQLite object database (one `.sqlite` file under `.git/`).
#[derive(Debug)]
pub struct SqliteOdbStore {
    path: PathBuf,
    algo: HashAlgo,
    compression: Compression,
    conn: Mutex<Connection>,
}

impl SqliteOdbStore {
    /// Create a new database at `path`, replacing any existing file.
    ///
    /// # Errors
    ///
    /// Returns I/O or SQLite errors while creating the schema.
    pub fn create(path: impl AsRef<Path>, algo: HashAlgo) -> Result<Self> {
        let path = path.as_ref().to_path_buf();
        if path.exists() {
            std::fs::remove_file(&path).map_err(Error::Io)?;
        }
        let conn = Connection::open(&path).map_err(sqlite_err)?;
        init_schema(&conn, algo)?;
        Ok(Self {
            path,
            algo,
            compression: Compression::default(),
            conn: Mutex::new(conn),
        })
    }

    /// Open an existing SQLite object database.
    ///
    /// # Errors
    ///
    /// Returns I/O, format, or SQLite errors.
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref().to_path_buf();
        let conn = Connection::open(&path).map_err(sqlite_err)?;
        let algo = read_algo(&conn)?;
        Ok(Self {
            path,
            algo,
            compression: Compression::default(),
            conn: Mutex::new(conn),
        })
    }

    /// Path to the backing SQLite file.
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
        let guard = self
            .conn
            .lock()
            .map_err(|e| Error::Io(io::Error::other(e.to_string())))?;
        let mut stmt = guard
            .prepare("SELECT oid, zlib FROM objects")
            .map_err(sqlite_err)?;
        let rows = stmt
            .query_map([], |row| {
                let oid_bytes: Vec<u8> = row.get(0)?;
                let zlib: Vec<u8> = row.get(1)?;
                Ok((oid_bytes, zlib))
            })
            .map_err(sqlite_err)?;
        for row in rows {
            let (oid_bytes, zlib) = row.map_err(sqlite_err)?;
            let oid = parse_oid_blob(&oid_bytes, self.algo)?;
            let raw = decompress_zlib(&zlib)?;
            let obj = parse_store_bytes(&raw)?;
            let computed = hash::hash_object(self.algo, obj.kind, &obj.data);
            if computed != oid {
                return Err(Error::CorruptObject(format!(
                    "sqlite odb hash mismatch for {}",
                    oid.to_hex()
                )));
            }
            WritableObjectStore::write(loose, obj.kind, &obj.data, WriteOptions::default())?;
        }
        Ok(())
    }

    fn read_zlib(&self, oid: &ObjectId) -> Result<Option<Vec<u8>>> {
        let guard = self
            .conn
            .lock()
            .map_err(|e| Error::Io(io::Error::other(e.to_string())))?;
        let mut stmt = guard
            .prepare("SELECT zlib FROM objects WHERE oid = ?1")
            .map_err(sqlite_err)?;
        let mut rows = stmt
            .query(params![oid.as_bytes().to_vec()])
            .map_err(sqlite_err)?;
        let Some(row) = rows.next().map_err(sqlite_err)? else {
            return Ok(None);
        };
        let zlib: Vec<u8> = row.get(0).map_err(sqlite_err)?;
        Ok(Some(zlib))
    }

    fn insert_zlib(&self, oid: &ObjectId, store_bytes: &[u8]) -> Result<()> {
        let mut zlib = Vec::new();
        {
            let mut enc = ZlibEncoder::new(&mut zlib, self.compression);
            enc.write_all(store_bytes).map_err(Error::Io)?;
            enc.finish().map_err(|e| Error::Zlib(e.to_string()))?;
        }
        let guard = self
            .conn
            .lock()
            .map_err(|e| Error::Io(io::Error::other(e.to_string())))?;
        guard
            .execute(
                "INSERT OR IGNORE INTO objects (oid, zlib) VALUES (?1, ?2)",
                params![oid.as_bytes().to_vec(), zlib],
            )
            .map_err(sqlite_err)?;
        Ok(())
    }
}

impl ObjectStore for SqliteOdbStore {
    fn hash_algo(&self) -> HashAlgo {
        self.algo
    }

    fn read(&self, oid: &ObjectId) -> Result<Option<Object>> {
        let Some(zlib) = self.read_zlib(oid)? else {
            return Ok(None);
        };
        let raw = decompress_zlib(&zlib)?;
        let obj = parse_store_bytes(&raw)?;
        let computed = hash::hash_object(self.algo, obj.kind, &obj.data);
        if computed != *oid {
            return Err(Error::CorruptObject(format!(
                "sqlite odb hash mismatch for {}",
                oid.to_hex()
            )));
        }
        Ok(Some(obj))
    }

    fn read_info(&self, oid: &ObjectId) -> Result<Option<ObjectInfo>> {
        let Some(zlib) = self.read_zlib(oid)? else {
            return Ok(None);
        };
        let raw = decompress_zlib(&zlib)?;
        Ok(Some(info_from_store_bytes(&raw)?))
    }

    fn contains(&self, oid: &ObjectId) -> Result<bool> {
        let guard = self
            .conn
            .lock()
            .map_err(|e| Error::Io(io::Error::other(e.to_string())))?;
        let count: i64 = guard
            .query_row(
                "SELECT COUNT(1) FROM objects WHERE oid = ?1",
                params![oid.as_bytes().to_vec()],
                |row| row.get(0),
            )
            .map_err(sqlite_err)?;
        Ok(count > 0)
    }

    fn open_stream(&self, oid: &ObjectId) -> Result<Option<ObjectStream<'_>>> {
        let Some(obj) = self.read(oid)? else {
            return Ok(None);
        };
        let size = u64::try_from(obj.data.len()).map_err(|_| {
            Error::CorruptObject(format!("object size overflow for {}", oid.to_hex()))
        })?;
        Ok(Some(ObjectStream {
            kind: obj.kind,
            size,
            reader: Box::new(io::Cursor::new(obj.data)),
        }))
    }

    fn for_each_object(&self, f: &mut dyn FnMut(&ObjectId) -> ControlFlow<()>) -> Result<()> {
        let guard = self
            .conn
            .lock()
            .map_err(|e| Error::Io(io::Error::other(e.to_string())))?;
        let mut stmt = guard
            .prepare("SELECT oid FROM objects")
            .map_err(sqlite_err)?;
        let rows = stmt
            .query_map([], |row| {
                let oid_bytes: Vec<u8> = row.get(0)?;
                Ok(oid_bytes)
            })
            .map_err(sqlite_err)?;
        for row in rows {
            let oid_bytes = row.map_err(sqlite_err)?;
            let oid = parse_oid_blob(&oid_bytes, self.algo)?;
            if f(&oid).is_break() {
                break;
            }
        }
        Ok(())
    }

    fn lookup_prefix(&self, prefix: &str, limit: usize, out: &mut Vec<ObjectId>) -> Result<()> {
        let prefix = prefix.to_ascii_lowercase();
        if !prefix.is_empty() && !prefix.bytes().all(|b| b.is_ascii_hexdigit()) {
            return Err(Error::InvalidObjectId(prefix));
        }
        if prefix.len() > self.algo.hex_len() {
            return Err(Error::InvalidObjectId(prefix));
        }
        let guard = self
            .conn
            .lock()
            .map_err(|e| Error::Io(io::Error::other(e.to_string())))?;
        let sql = if limit == 0 {
            "SELECT oid FROM objects WHERE hex(oid) LIKE ?1 || '%'".to_string()
        } else {
            format!("SELECT oid FROM objects WHERE hex(oid) LIKE ?1 || '%' LIMIT {limit}")
        };
        let mut stmt = guard.prepare(&sql).map_err(sqlite_err)?;
        let rows = stmt
            .query_map(params![prefix], |row| {
                let oid_bytes: Vec<u8> = row.get(0)?;
                Ok(oid_bytes)
            })
            .map_err(sqlite_err)?;
        for row in rows {
            let oid_bytes = row.map_err(sqlite_err)?;
            out.push(parse_oid_blob(&oid_bytes, self.algo)?);
        }
        Ok(())
    }
}

impl WritableObjectStore for SqliteOdbStore {
    fn write(&self, kind: ObjectKind, data: &[u8], _options: WriteOptions) -> Result<ObjectId> {
        let oid = hash::hash_object(self.algo, kind, data);
        if self.contains(&oid)? {
            return Ok(oid);
        }
        let store_bytes = build_store_bytes(kind, data);
        self.insert_zlib(&oid, &store_bytes)?;
        Ok(oid)
    }

    fn freshen(&self, oid: &ObjectId) -> Result<bool> {
        self.contains(oid)
    }
}

fn init_schema(conn: &Connection, algo: HashAlgo) -> Result<()> {
    conn.execute_batch(
        "CREATE TABLE meta (
            key TEXT PRIMARY KEY NOT NULL,
            value INTEGER NOT NULL
        );
        CREATE TABLE objects (
            oid BLOB PRIMARY KEY NOT NULL,
            zlib BLOB NOT NULL
        );
        CREATE INDEX objects_hex_oid ON objects (hex(oid));",
    )
    .map_err(sqlite_err)?;
    conn.execute(
        "INSERT INTO meta (key, value) VALUES ('schema_version', ?1), ('hash_algo', ?2)",
        params![SCHEMA_VERSION, algo_meta_value(algo)],
    )
    .map_err(sqlite_err)?;
    Ok(())
}

fn read_algo(conn: &Connection) -> Result<HashAlgo> {
    let version: i64 = conn
        .query_row(
            "SELECT value FROM meta WHERE key = 'schema_version'",
            [],
            |row| row.get(0),
        )
        .map_err(|_| Error::CorruptObject("sqlite odb missing schema_version".into()))?;
    if version != SCHEMA_VERSION {
        return Err(Error::CorruptObject(format!(
            "unsupported sqlite odb schema version {version}"
        )));
    }
    let algo_value: i64 = conn
        .query_row(
            "SELECT value FROM meta WHERE key = 'hash_algo'",
            [],
            |row| row.get(0),
        )
        .map_err(|_| Error::CorruptObject("sqlite odb missing hash_algo".into()))?;
    parse_algo_meta_value(algo_value)
}

fn algo_meta_value(algo: HashAlgo) -> i64 {
    match algo {
        HashAlgo::Sha1 => 1,
        HashAlgo::Sha256 => 2,
    }
}

fn parse_algo_meta_value(value: i64) -> Result<HashAlgo> {
    match value {
        1 => Ok(HashAlgo::Sha1),
        2 => Ok(HashAlgo::Sha256),
        _ => Err(Error::CorruptObject(format!(
            "unknown sqlite odb hash_algo value {value}"
        ))),
    }
}

fn parse_oid_blob(bytes: &[u8], algo: HashAlgo) -> Result<ObjectId> {
    let expected = algo.len();
    if bytes.len() != expected {
        return Err(Error::CorruptObject(format!(
            "sqlite odb oid length {} expected {expected}",
            bytes.len()
        )));
    }
    let oid = ObjectId::from_bytes(bytes)?;
    if oid.algo() != algo {
        return Err(Error::CorruptObject(format!(
            "sqlite odb oid algorithm mismatch for {}",
            oid.to_hex()
        )));
    }
    Ok(oid)
}

fn sqlite_err(err: rusqlite::Error) -> Error {
    Error::Io(io::Error::other(err.to_string()))
}

fn decompress_zlib(zlib: &[u8]) -> Result<Vec<u8>> {
    let mut decoder = ZlibDecoder::new(zlib);
    let mut raw = Vec::new();
    decoder
        .read_to_end(&mut raw)
        .map_err(|e| Error::Zlib(e.to_string()))?;
    Ok(raw)
}

fn build_store_bytes(kind: ObjectKind, data: &[u8]) -> Vec<u8> {
    let header = format!("{kind} {}\0", data.len());
    let mut store_bytes = header.into_bytes();
    store_bytes.extend_from_slice(data);
    store_bytes
}

fn parse_store_bytes(raw: &[u8]) -> Result<Object> {
    let nul = raw
        .iter()
        .position(|&b| b == 0)
        .ok_or_else(|| Error::CorruptObject("missing object header terminator".into()))?;
    let header = std::str::from_utf8(&raw[..nul])
        .map_err(|_| Error::CorruptObject("invalid object header utf-8".into()))?;
    let mut parts = header.splitn(2, ' ');
    let kind_str = parts
        .next()
        .ok_or_else(|| Error::CorruptObject("empty object header".into()))?;
    let size_str = parts
        .next()
        .ok_or_else(|| Error::CorruptObject("missing object size".into()))?;
    let kind: ObjectKind = kind_str
        .parse()
        .map_err(|_| Error::CorruptObject(format!("unknown object kind {kind_str}")))?;
    let size: usize = size_str
        .parse()
        .map_err(|_| Error::CorruptObject(format!("invalid object size {size_str}")))?;
    let payload = raw
        .get(nul + 1..)
        .ok_or_else(|| Error::CorruptObject("object payload shorter than header".into()))?;
    if payload.len() != size {
        return Err(Error::CorruptObject(format!(
            "object size {size} does not match payload {}",
            payload.len()
        )));
    }
    Ok(Object {
        kind,
        data: payload.to_vec(),
    })
}

fn info_from_store_bytes(raw: &[u8]) -> Result<ObjectInfo> {
    let obj = parse_store_bytes(raw)?;
    Ok(ObjectInfo {
        kind: obj.kind,
        size: u64::try_from(obj.data.len())
            .map_err(|_| Error::CorruptObject("object size overflow".into()))?,
    })
}

/// End-to-end demo used by the `sqlite-object-store` example and integration test.
///
/// # Errors
///
/// Propagates repository, object-store, or ref errors.
pub fn run_sqlite_object_store_demo(repo_root: &Path) -> Result<Vec<String>> {
    use std::sync::Arc;

    use grit_lib::environment::RepositoryOptions;
    use grit_lib::objects::{serialize_tree, TreeEntry};
    use grit_lib::odb::OdbBuilder;
    use grit_lib::refs::write_ref;
    use grit_lib::repo::{init_repository, Repository};
    use grit_lib::rev_list::{rev_list, RevListOptions};

    init_repository(
        repo_root,
        false,
        "main",
        None,
        grit_lib::RefStorageFormat::Files,
    )?;
    let git_dir = repo_root.join(".git");
    let objects_dir = git_dir.join("objects");
    let db_path = git_dir.join("objects.sqlite");

    let sqlite = Arc::new(SqliteOdbStore::create(&db_path, HashAlgo::Sha1)?);
    let repo = Repository::open_with_odb(
        &RepositoryOptions::empty(),
        &git_dir,
        Some(repo_root),
        OdbBuilder::files(&objects_dir)
            .primary(sqlite.clone())
            .alternates(false),
    )?;

    let blob = repo.odb.write(ObjectKind::Blob, b"hello sqlite odb\n")?;
    let tree_body = serialize_tree(&[TreeEntry {
        mode: 0o100644,
        name: b"hello.txt".to_vec(),
        oid: blob,
    }]);
    let tree = repo.odb.write(ObjectKind::Tree, &tree_body)?;
    let commit_body = format!(
        "tree {tree}\nauthor Demo <demo@example.com> 1 +0000\ncommitter Demo <demo@example.com> 1 +0000\n\nsqlite odb demo\n"
    );
    let commit = repo.odb.write(ObjectKind::Commit, commit_body.as_bytes())?;
    write_ref(&git_dir, "refs/heads/main", &commit)?;

    let tip = commit.to_hex();
    let listed = rev_list(
        &repo,
        std::slice::from_ref(&tip),
        &[],
        &RevListOptions::default(),
    )?;
    let grit_log: Vec<String> = listed.commits.iter().map(ObjectId::to_hex).collect();

    let loose = LooseStore::new(objects_dir.clone(), HashAlgo::Sha1, Compression::default());
    sqlite.export_to_loose(&loose)?;

    Ok(grit_log)
}

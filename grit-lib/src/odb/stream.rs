//! Bounded-memory object hashing and writing from a byte stream.
//!
//! Objects at or above `core.bigFileThreshold` are never held in memory: the payload is read
//! in fixed-size chunks that feed the object hasher and a zlib encoder writing a
//! single-object pack, which is how Git stores big files too.

use std::fs::{self, File};
use std::io::{self, BufWriter, Read, Write};
use std::path::{Path, PathBuf};

use flate2::write::ZlibEncoder;
use flate2::Compression;
use tempfile::NamedTempFile;

use crate::error::{Error, Result};
use crate::hash::{ChecksumHasher, ObjectPayloadHasher};
use crate::objects::{HashAlgo, ObjectId, ObjectKind};
use crate::pack_objects::{encode_pack_object_header, pack_type_code};

/// Bytes read from the source per step; the only payload-sized buffer a stream holds.
const CHUNK: usize = 128 * 1024;

/// Payloads at least this large are written from a stream without buffering the whole body.
pub(crate) const STREAMED_WRITE_MIN_SIZE: u64 = 1024 * 1024;

/// When the first chunk does not shrink at the configured level, store the payload at zlib level 0.
pub(crate) fn compression_after_first_chunk(first: &[u8], configured: Compression) -> Compression {
    if first.is_empty() || configured.level() == 0 {
        return configured;
    }
    let mut trial = Vec::new();
    let mut enc = ZlibEncoder::new(&mut trial, configured);
    if enc.write_all(first).is_err() || enc.finish().is_err() {
        return configured;
    }
    if trial.len() >= first.len() {
        Compression::none()
    } else {
        configured
    }
}

/// Read up to one [`CHUNK`] of the first `size` payload bytes from `reader`.
pub(crate) fn read_first_payload_chunk(reader: &mut dyn Read, size: u64) -> Result<(Vec<u8>, u64)> {
    if size == 0 {
        return Ok((Vec::new(), 0));
    }
    let take = usize::try_from(size.min(CHUNK as u64))
        .map_err(|_| Error::CorruptObject(format!("object size {size} exceeds address space")))?;
    let mut first = vec![0u8; take];
    let n = reader.read(&mut first)?;
    first.truncate(n);
    if n == 0 {
        return Err(Error::ObjectStreamTruncated {
            expected: size,
            actual: 0,
        });
    }
    Ok((first, size - n as u64))
}

/// Offset of the single object in a pack: right after the 12-byte `PACK` header.
const OBJECT_OFFSET: u64 = 12;

/// Deflate exactly `size` payload bytes from `reader` into `w` without holding the payload.
pub(crate) fn deflate_stream_to_writer(
    reader: &mut dyn Read,
    size: u64,
    w: &mut dyn Write,
    compression: Compression,
) -> Result<()> {
    let mut enc = ZlibEncoder::new(w, compression);
    for_each_chunk(reader, size, |chunk| {
        enc.write_all(chunk).map_err(Error::Io)
    })?;
    enc.finish().map_err(Error::Io)?;
    Ok(())
}

/// Read exactly `size` bytes from `reader` in [`CHUNK`]-sized steps, handing each to `sink`.
fn for_each_chunk(
    reader: &mut dyn Read,
    size: u64,
    mut sink: impl FnMut(&[u8]) -> Result<()>,
) -> Result<()> {
    let mut buf = vec![0u8; usize::try_from(size).map_or(CHUNK, |s| s.min(CHUNK))];
    let mut remaining = size;
    while remaining > 0 {
        let want = usize::try_from(remaining).map_or(buf.len(), |r| r.min(buf.len()));
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

/// Object id of `size` payload bytes of `kind` read from `reader`.
pub(crate) fn hash_stream(
    algo: HashAlgo,
    kind: ObjectKind,
    size: u64,
    reader: &mut dyn Read,
) -> Result<ObjectId> {
    let mut hasher = ObjectPayloadHasher::new(algo, kind, size);
    for_each_chunk(reader, size, |chunk| {
        hasher.update(chunk);
        Ok(())
    })?;
    hasher.finish()
}

/// Pack output: every byte goes to the file and the pack checksum, and also to the CRC32
/// that the `.idx` records for the object entry.
struct PackSink {
    out: BufWriter<File>,
    checksum: ChecksumHasher,
    crc: crc32fast::Hasher,
}

impl Write for PackSink {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let n = self.out.write(buf)?;
        self.checksum.update(&buf[..n]);
        self.crc.update(&buf[..n]);
        Ok(n)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.out.flush()
    }
}

/// A single-object pack in a temporary file under `objects/pack`, not yet visible to readers.
///
/// Dropping it without [`Self::install`] deletes the temporary file.
pub(crate) struct StagedPack {
    tmp: NamedTempFile,
    pack_dir: PathBuf,
    oid: ObjectId,
    crc: u32,
    trailer: Vec<u8>,
}

impl StagedPack {
    /// Deflate `size` payload bytes of `kind` from `reader` into a new temporary pack in
    /// `pack_dir`, hashing the object on the way.
    pub(crate) fn write(
        pack_dir: &Path,
        algo: HashAlgo,
        kind: ObjectKind,
        size: u64,
        reader: &mut dyn Read,
        compression: Compression,
    ) -> Result<Self> {
        let (first, tail) = read_first_payload_chunk(reader, size)?;
        let compression = compression_after_first_chunk(&first, compression);
        let payload_len = usize::try_from(size).map_err(|_| {
            Error::CorruptObject(format!("object size {size} exceeds address space"))
        })?;
        fs::create_dir_all(pack_dir)?;
        let tmp = tempfile::Builder::new()
            .prefix("tmp_pack_")
            .tempfile_in(pack_dir)?;

        let mut header = Vec::with_capacity(12);
        header.extend_from_slice(b"PACK");
        header.extend_from_slice(&2u32.to_be_bytes());
        header.extend_from_slice(&1u32.to_be_bytes());
        let mut out = BufWriter::with_capacity(CHUNK, tmp.reopen()?);
        out.write_all(&header)?;
        let mut checksum = algo.checksum_hasher();
        checksum.update(&header);
        let mut sink = PackSink {
            out,
            checksum,
            crc: crc32fast::Hasher::new(),
        };

        let mut entry_header = Vec::with_capacity(16);
        encode_pack_object_header(&mut entry_header, pack_type_code(kind), payload_len);
        sink.write_all(&entry_header)?;
        let mut hasher = ObjectPayloadHasher::new(algo, kind, size);
        let mut encoder = ZlibEncoder::new(&mut sink, compression);
        hasher.update(&first);
        encoder.write_all(&first).map_err(Error::Io)?;
        if tail > 0 {
            for_each_chunk(reader, tail, |chunk| {
                hasher.update(chunk);
                encoder.write_all(chunk).map_err(Error::Io)
            })?;
        }
        encoder.finish()?;
        let oid = hasher.finish()?;

        let PackSink {
            mut out,
            checksum,
            crc,
        } = sink;
        let trailer = checksum.finalize();
        out.write_all(&trailer)?;
        out.flush()?;
        Ok(Self {
            tmp,
            pack_dir: pack_dir.to_path_buf(),
            oid,
            crc: crc.finalize(),
            trailer,
        })
    }

    /// Id of the object in this pack.
    pub(crate) fn oid(&self) -> ObjectId {
        self.oid
    }

    /// Publish as `pack-<checksum>.pack` with its `.idx`.
    ///
    /// The `.idx` is moved into place last because readers discover packs through it. Both
    /// names derive from the pack checksum, so a file already at either name holds the same
    /// bytes (another writer of the same object, or an install interrupted between the two
    /// steps); it is kept instead of replaced, which Windows could not do anyway.
    pub(crate) fn install(self) -> Result<()> {
        let stem = format!("pack-{}", hex::encode(&self.trailer));
        let pack_path = self.pack_dir.join(format!("{stem}.pack"));
        let idx_path = self.pack_dir.join(format!("{stem}.idx"));
        publish_new(self.tmp, &pack_path)?;

        let idx_tmp = tempfile::Builder::new()
            .prefix("tmp_idx_")
            .tempfile_in(&self.pack_dir)?;
        crate::pack::write_v2_pack_index_sorting(
            idx_tmp.path(),
            &mut [(self.oid, OBJECT_OFFSET, self.crc)],
            &self.trailer,
            self.trailer.len(),
            None,
        )?;
        publish_new(idx_tmp, &idx_path)
    }
}

/// Move `tmp` to `path` unless a file is already there, in which case `tmp` is deleted.
fn publish_new(tmp: NamedTempFile, path: &Path) -> Result<()> {
    match tmp.persist_noclobber(path) {
        Ok(_) => {
            set_read_only(path);
            Ok(())
        }
        Err(e) if e.error.kind() == io::ErrorKind::AlreadyExists => Ok(()),
        Err(e) => Err(Error::Io(e.error)),
    }
}

fn set_read_only(path: &Path) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = fs::set_permissions(path, fs::Permissions::from_mode(0o444));
    }
    let _ = path;
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used, clippy::unwrap_used)]

    use super::*;
    use std::io::Cursor;

    #[test]
    fn hash_stream_matches_buffered_hash() {
        let data: Vec<u8> = (0..3 * CHUNK + 17).map(|i| (i * 31 % 251) as u8).collect();
        for algo in [HashAlgo::Sha1, HashAlgo::Sha256] {
            let streamed = hash_stream(
                algo,
                ObjectKind::Blob,
                data.len() as u64,
                &mut Cursor::new(&data),
            )
            .unwrap();
            assert_eq!(
                streamed,
                crate::hash::hash_object(algo, ObjectKind::Blob, &data).unwrap()
            );
        }
    }

    #[test]
    fn short_stream_reports_truncation() {
        let err = hash_stream(
            HashAlgo::Sha1,
            ObjectKind::Blob,
            10,
            &mut Cursor::new(b"abc".to_vec()),
        )
        .unwrap_err();
        assert!(matches!(
            err,
            Error::ObjectStreamTruncated {
                expected: 10,
                actual: 3
            }
        ));
    }

    #[test]
    fn stream_reads_only_the_declared_size() {
        let mut reader = Cursor::new(b"abcdef".to_vec());
        let oid = hash_stream(HashAlgo::Sha1, ObjectKind::Blob, 3, &mut reader).unwrap();
        assert_eq!(
            oid,
            crate::hash::hash_object(HashAlgo::Sha1, ObjectKind::Blob, b"abc").unwrap()
        );
        assert_eq!(reader.position(), 3);
    }

    #[test]
    fn dropped_staged_pack_leaves_no_files() {
        let dir = tempfile::tempdir().unwrap();
        let pack_dir = dir.path().join("pack");
        let staged = StagedPack::write(
            &pack_dir,
            HashAlgo::Sha1,
            ObjectKind::Blob,
            5,
            &mut Cursor::new(b"hello".to_vec()),
            Compression::default(),
        )
        .unwrap();
        assert_eq!(fs::read_dir(&pack_dir).unwrap().count(), 1);
        drop(staged);
        assert_eq!(fs::read_dir(&pack_dir).unwrap().count(), 0);
    }

    use crate::odb::{Odb, WriteOptions};
    use std::process::Command;

    /// Reader over generated bytes that records the largest read it was asked for.
    struct ProbeReader {
        remaining: u64,
        max_request: usize,
        state: u32,
    }

    impl ProbeReader {
        fn new(size: u64) -> Self {
            Self {
                remaining: size,
                max_request: 0,
                state: 1,
            }
        }
    }

    impl Read for ProbeReader {
        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            self.max_request = self.max_request.max(buf.len());
            let n = usize::try_from(self.remaining).map_or(buf.len(), |r| r.min(buf.len()));
            for b in &mut buf[..n] {
                self.state = self.state.wrapping_mul(1_103_515_245).wrapping_add(12_345);
                *b = b"abcdefgh\n"[(self.state >> 16) as usize % 9];
            }
            self.remaining -= n as u64;
            Ok(n)
        }
    }

    fn generated(size: u64) -> Vec<u8> {
        let mut data = Vec::new();
        ProbeReader::new(size).read_to_end(&mut data).unwrap();
        data
    }

    /// An `objects/` dir inside a git repository whose `core.bigFileThreshold` is `threshold`.
    fn repo_with_threshold(threshold: &str) -> (tempfile::TempDir, Odb) {
        let dir = tempfile::tempdir().unwrap();
        let status = Command::new("git")
            .args(["init", "-q"])
            .arg(dir.path())
            .status()
            .unwrap();
        assert!(status.success());
        let git_dir = dir.path().join(".git");
        let status = Command::new("git")
            .arg("--git-dir")
            .arg(&git_dir)
            .args(["config", "core.bigFileThreshold", threshold])
            .status()
            .unwrap();
        assert!(status.success());
        let odb = Odb::new(&git_dir.join("objects"));
        (dir, odb)
    }

    fn git_out(dir: &Path, args: &[&str]) -> Vec<u8> {
        let out = Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(args)
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        out.stdout
    }

    fn pack_files(odb: &Odb) -> Vec<String> {
        let mut names: Vec<String> = fs::read_dir(odb.objects_dir().join("pack"))
            .map(|rd| {
                rd.map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
                    .collect()
            })
            .unwrap_or_default();
        names.sort();
        names
    }

    #[cfg_attr(
        not(feature = "full-tests"),
        ignore = "starts system git or is heavy; run with --features full-tests"
    )]
    #[test]
    fn odb_reads_big_file_threshold_from_config() {
        let (_dir, odb) = repo_with_threshold("3k");
        assert_eq!(odb.big_file_threshold().unwrap(), 3 * 1024);
        let bare = tempfile::tempdir().unwrap();
        assert_eq!(
            Odb::new(&bare.path().join("objects"))
                .big_file_threshold()
                .unwrap(),
            crate::config::ConfigSet::BIG_FILE_THRESHOLD_DEFAULT
        );
    }

    #[cfg_attr(
        not(feature = "full-tests"),
        ignore = "starts system git or is heavy; run with --features full-tests"
    )]
    #[test]
    fn odb_hash_stream_matches_hash() {
        let (_dir, odb) = repo_with_threshold("1k");
        let data = generated(300_000);
        let oid = odb
            .hash_stream(ObjectKind::Blob, data.len() as u64, &mut Cursor::new(&data))
            .unwrap();
        assert_eq!(oid, odb.hash(ObjectKind::Blob, &data).unwrap());
        assert!(pack_files(&odb).is_empty(), "hashing stores nothing");
    }

    #[cfg_attr(
        not(feature = "full-tests"),
        ignore = "starts system git or is heavy; run with --features full-tests"
    )]
    #[test]
    fn write_stream_below_threshold_writes_the_same_loose_object_as_write() {
        let (_dir, odb) = repo_with_threshold("1m");
        let data = generated(10_000);
        let oid = odb
            .write_stream(
                ObjectKind::Blob,
                data.len() as u64,
                &mut Cursor::new(&data),
                WriteOptions::default(),
            )
            .unwrap();
        assert_eq!(oid, odb.hash(ObjectKind::Blob, &data).unwrap());
        assert!(odb.object_path(&oid).is_file());
        assert!(pack_files(&odb).is_empty());
    }

    #[cfg_attr(
        not(feature = "full-tests"),
        ignore = "starts system git or is heavy; run with --features full-tests"
    )]
    #[test]
    fn write_stream_at_threshold_packs_with_bounded_reads_and_git_reads_it() {
        let (dir, odb) = repo_with_threshold("64k");
        let size = 3 * 1024 * 1024 + 7;
        let mut reader = ProbeReader::new(size);
        let oid = odb
            .write_stream(ObjectKind::Blob, size, &mut reader, WriteOptions::default())
            .unwrap();
        assert!(
            reader.max_request <= CHUNK,
            "reads of {} bytes exceed the {CHUNK}-byte chunk",
            reader.max_request
        );
        let data = generated(size);
        assert_eq!(oid, odb.hash(ObjectKind::Blob, &data).unwrap());
        assert!(!odb.object_path(&oid).exists(), "big objects are not loose");
        let packs = pack_files(&odb);
        assert_eq!(packs.len(), 2, "{packs:?}");
        assert!(packs.iter().all(|n| !n.starts_with("tmp_")), "{packs:?}");

        assert_eq!(odb.read(&oid).unwrap().data, data);
        let hex = oid.to_hex();
        assert_eq!(git_out(dir.path(), &["cat-file", "blob", &hex]), data);
        git_out(dir.path(), &["fsck", "--strict"]);
        let idx = packs.iter().find(|n| n.ends_with(".idx")).unwrap();
        let verify = git_out(
            dir.path(),
            &[
                "verify-pack",
                "-v",
                &odb.objects_dir().join("pack").join(idx).to_string_lossy(),
            ],
        );
        assert!(
            String::from_utf8_lossy(&verify).contains(&format!("{hex} blob   {size}")),
            "{}",
            String::from_utf8_lossy(&verify)
        );
    }

    #[cfg_attr(
        not(feature = "full-tests"),
        ignore = "starts system git or is heavy; run with --features full-tests"
    )]
    #[test]
    fn write_stream_of_an_existing_object_adds_no_pack() {
        let (_dir, odb) = repo_with_threshold("1k");
        let data = generated(50_000);
        let loose = odb.write(ObjectKind::Blob, &data).unwrap();
        let streamed = odb
            .write_stream(
                ObjectKind::Blob,
                data.len() as u64,
                &mut Cursor::new(&data),
                WriteOptions::default(),
            )
            .unwrap();
        assert_eq!(streamed, loose);
        assert!(pack_files(&odb).is_empty(), "{:?}", pack_files(&odb));

        let other = generated(60_000);
        for _ in 0..2 {
            odb.write_stream(
                ObjectKind::Blob,
                other.len() as u64,
                &mut Cursor::new(&other),
                WriteOptions::default(),
            )
            .unwrap();
        }
        assert_eq!(
            pack_files(&odb).len(),
            2,
            "the second write is deduplicated"
        );
    }

    #[cfg_attr(
        not(feature = "full-tests"),
        ignore = "starts system git or is heavy; run with --features full-tests"
    )]
    #[test]
    fn truncated_write_stream_errors_and_leaves_nothing_behind() {
        let (_dir, odb) = repo_with_threshold("1k");
        let err = odb
            .write_stream(
                ObjectKind::Blob,
                100_000,
                &mut Cursor::new(generated(4_000)),
                WriteOptions::default(),
            )
            .unwrap_err();
        assert!(matches!(
            err,
            Error::ObjectStreamTruncated {
                expected: 100_000,
                actual: 4_000
            }
        ));
        assert!(pack_files(&odb).is_empty(), "{:?}", pack_files(&odb));
    }

    #[test]
    fn write_stream_routes_to_a_custom_primary() {
        use crate::odb::builder::OdbBuilder;
        use crate::odb::store::{MemoryStore, ObjectStore};
        use std::sync::Arc;

        let dir = tempfile::tempdir().unwrap();
        let objects = dir.path().join("objects");
        fs::create_dir_all(&objects).unwrap();
        let store = Arc::new(MemoryStore::new(HashAlgo::Sha1));
        let odb = OdbBuilder::files(&objects)
            .primary(store.clone())
            .alternates(false)
            .build();
        let data = generated(20_000);
        let oid = odb
            .write_stream(
                ObjectKind::Blob,
                data.len() as u64,
                &mut Cursor::new(&data),
                WriteOptions::default(),
            )
            .unwrap();
        assert!(store.contains(&oid).unwrap());
        assert_eq!(fs::read_dir(&objects).unwrap().count(), 0);
    }

    fn stage(pack_dir: &Path, data: &[u8]) -> StagedPack {
        StagedPack::write(
            pack_dir,
            HashAlgo::Sha1,
            ObjectKind::Blob,
            data.len() as u64,
            &mut Cursor::new(data),
            Compression::default(),
        )
        .unwrap()
    }

    fn dir_names(dir: &Path) -> Vec<String> {
        let mut names: Vec<String> = fs::read_dir(dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        names
    }

    /// Identity of the file at `path`: replacing it (rename over it) changes this.
    fn file_id(path: &Path) -> (u64, u64) {
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            let meta = fs::metadata(path).unwrap();
            (meta.dev(), meta.ino())
        }
        #[cfg(not(unix))]
        {
            let _ = path;
            (0, 0)
        }
    }

    #[test]
    fn concurrent_installs_of_the_same_pack_both_succeed() {
        let dir = tempfile::tempdir().unwrap();
        let objects = dir.path().join("objects");
        let pack_dir = objects.join("pack");
        let data = generated(200_000);
        let first = stage(&pack_dir, &data);
        let second = stage(&pack_dir, &data);
        let oid = first.oid();
        assert_eq!(second.oid(), oid);
        first.install().unwrap();
        let installed: Vec<_> = dir_names(&pack_dir)
            .iter()
            .filter(|n| n.starts_with("pack-"))
            .map(|n| file_id(&pack_dir.join(n)))
            .collect();
        second.install().unwrap();
        let after: Vec<_> = dir_names(&pack_dir)
            .iter()
            .filter(|n| n.starts_with("pack-"))
            .map(|n| file_id(&pack_dir.join(n)))
            .collect();
        assert_eq!(
            after, installed,
            "the first writer's files are kept, not replaced"
        );
        let names = dir_names(&pack_dir);
        assert_eq!(
            names.len(),
            2,
            "one .pack and one .idx, no temp files: {names:?}"
        );
        assert_eq!(Odb::new(&objects).read(&oid).unwrap().data, data);
    }

    #[test]
    fn install_completes_a_pack_left_without_its_index() {
        let dir = tempfile::tempdir().unwrap();
        let objects = dir.path().join("objects");
        let pack_dir = objects.join("pack");
        let data = generated(100_000);
        let staged = stage(&pack_dir, &data);
        let oid = staged.oid();
        staged.install().unwrap();
        let idx = dir_names(&pack_dir)
            .into_iter()
            .find(|n| n.ends_with(".idx"))
            .unwrap();
        fs::remove_file(pack_dir.join(&idx)).unwrap();

        stage(&pack_dir, &data).install().unwrap();
        let names = dir_names(&pack_dir);
        assert_eq!(names.len(), 2, "{names:?}");
        assert!(names.contains(&idx), "{names:?}");
        assert_eq!(Odb::new(&objects).read(&oid).unwrap().data, data);
    }

    #[test]
    fn installed_pack_reads_back_through_the_pack_index() {
        let dir = tempfile::tempdir().unwrap();
        let objects = dir.path().join("objects");
        let data = b"streamed into a pack".to_vec();
        let staged = StagedPack::write(
            &objects.join("pack"),
            HashAlgo::Sha1,
            ObjectKind::Blob,
            data.len() as u64,
            &mut Cursor::new(&data),
            Compression::default(),
        )
        .unwrap();
        let oid = staged.oid();
        staged.install().unwrap();
        let names: Vec<String> = fs::read_dir(objects.join("pack"))
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(names.len(), 2, "{names:?}");
        assert!(names.iter().all(|n| n.starts_with("pack-")), "{names:?}");
        let odb = crate::odb::Odb::new(&objects);
        let obj = odb.read(&oid).unwrap();
        assert_eq!(obj.kind, ObjectKind::Blob);
        assert_eq!(obj.data, data);
    }
}

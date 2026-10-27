//! Memory-mapped pack (and MIDX) file bytes with an owned-buffer fallback.
//!
//! This module is the only place that uses `unsafe` for pack backing storage.

use crate::error::{Error, Result};
use crate::objects::HashAlgo;
use memmap2::{Mmap, MmapOptions};
use std::collections::VecDeque;
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::ops::Deref;
use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, Weak};

const PACK_SIGNATURE: &[u8; 4] = b"PACK";
const HEADER_LEN: usize = 12;

/// Files at or below this size are read into an owned `Vec` (tiny packs, tests,
/// platforms or filesystems where mapping fails).
const OWNED_READ_THRESHOLD: u64 = 8192;
const RANDOM_WINDOW_BYTES: usize = 16 * 1024;
const RANDOM_WINDOW_COUNT: usize = 8;

#[derive(Default)]
struct ReadWindow {
    start: Option<usize>,
    bytes: Vec<u8>,
}

#[derive(Default)]
struct ReadWindows {
    entries: [ReadWindow; RANDOM_WINDOW_COUNT],
    next: usize,
}

struct PositionedPack {
    file: File,
    windows: Mutex<ReadWindows>,
}

/// Header plus trailing pack checksum — enough to detect on-disk replacement when
/// mtime/size are unchanged (same-second repack) without re-reading the whole file.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) struct PackFingerprint {
    /// `PACK`, version, and object count (`12` bytes).
    pub header: [u8; HEADER_LEN],
    /// Trailer width in bytes (20 for SHA-1, 32 for SHA-256).
    pub hash_bytes: u8,
    /// Pack trailer hash (only the first `hash_bytes` entries are meaningful).
    pub trailer: [u8; 32],
}

/// Repository-scoped ownership of descriptors used for sparse pack reads.
///
/// Mappings outlive descriptor eviction and continue to serve reads from the same
/// inode. Borrowers hold an Arc only during positioned I/O; mapped packs hold Weak
/// references so an unbounded catalog cannot exhaust the process descriptor limit.
#[derive(Default)]
pub(crate) struct SparseReadFiles {
    files: Mutex<VecDeque<Arc<File>>>,
}

impl SparseReadFiles {
    const CAPACITY: usize = 16;

    fn retain(&self, file: File) -> Weak<File> {
        let file = Arc::new(file);
        let weak = Arc::downgrade(&file);
        let mut files = self.files.lock().unwrap_or_else(|e| e.into_inner());
        if files.len() == Self::CAPACITY {
            files.pop_front();
        }
        files.push_back(file);
        weak
    }

    /// Release descriptors when packs are invalidated or descriptor pressure prevents opening a pack.
    pub(crate) fn clear(&self) {
        self.files.lock().unwrap_or_else(|e| e.into_inner()).clear();
    }
}

enum Storage {
    Mapped { map: Mmap, file: Weak<File> },
    Owned(Vec<u8>),
}

/// Immutable bytes of a `.pack`, pack index, commit-graph, MIDX, or `packed-refs` file.
///
/// # Safety (memory-mapped storage)
///
/// Pack, index, MIDX, and `packed-refs` files are immutable at a stable path: grit
/// and Git publish updates by writing a temporary file and renaming it into place,
/// so active readers never map a file that is being truncated in place. If a pack
/// were truncated while mapped, reads could fault or return stale bytes — the same
/// risk Git accepts for memory-mapped packfiles. `open` uses a single file
/// descriptor and `fstat`s that handle before mapping so length and inode stay paired.
pub struct PackData {
    storage: Storage,
    /// A descriptor paired with the mapping. Bounded catalogs limit retained readers;
    /// active readers keep this exact file alive even after eviction or path replacement.
    random_file: Option<Box<PositionedPack>>,
}

impl std::fmt::Debug for PackData {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PackData")
            .field("len", &self.len())
            .field(
                "backing",
                &match &self.storage {
                    Storage::Mapped { .. } => "mapped",
                    Storage::Owned(_) => "owned",
                },
            )
            .finish()
    }
}

impl Deref for PackData {
    type Target = [u8];

    fn deref(&self) -> &[u8] {
        match &self.storage {
            Storage::Mapped { map, .. } => map.deref(),
            Storage::Owned(v) => v.as_slice(),
        }
    }
}

impl PackData {
    /// Map `path` read-only, or read it into an owned buffer when small or mapping fails.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Io`] when the file cannot be opened or read, and
    /// [`Error::CorruptObject`] when the file is shorter than a pack header.
    pub(crate) fn open(path: &Path) -> Result<Arc<Self>> {
        Self::open_with_min_len(path, HEADER_LEN as u64)
    }

    /// Map or read `path`, requiring at least `min_len` bytes (pack header vs index fanout).
    pub(crate) fn open_with_min_len(path: &Path, min_len: u64) -> Result<Arc<Self>> {
        let file = File::open(path).map_err(Error::Io)?;
        Self::open_from_file(file, min_len, None, false)
    }

    /// Retain the pack's descriptor in a bounded pool for positioned reads.
    /// Indexes, graphs and other mapped metadata keep the descriptor-free `open` path.
    pub(crate) fn open_for_sparse_reads(path: &Path, files: &SparseReadFiles) -> Result<Arc<Self>> {
        let file = match File::open(path) {
            Ok(file) => file,
            #[cfg(unix)]
            Err(err) if matches!(err.raw_os_error(), Some(libc::EMFILE) | Some(libc::ENFILE)) => {
                // Mappings remain usable after eviction; make room before retrying a
                // new open. Never reopen a cached mapping through its current path.
                files.clear();
                File::open(path)?
            }
            Err(err) => return Err(err.into()),
        };
        Self::open_from_file(file, HEADER_LEN as u64, Some(files), false)
    }

    /// Keep a descriptor for positioned entry reads instead of faulting scattered pack pages.
    pub(crate) fn open_random_access(path: &Path) -> Result<Arc<Self>> {
        Self::open_from_file(File::open(path)?, HEADER_LEN as u64, None, true)
    }

    fn open_from_file(
        file: File,
        min_len: u64,
        sparse_reads: Option<&SparseReadFiles>,
        random: bool,
    ) -> Result<Arc<Self>> {
        let len = file.metadata().map_err(Error::Io)?.len();
        if len < min_len {
            return Err(Error::CorruptObject(format!(
                "file is too small ({len} bytes, need at least {min_len})"
            )));
        }
        let len_usize = usize::try_from(len).map_err(|_| {
            Error::CorruptObject(format!(
                "pack file length {len} does not fit in address space"
            ))
        })?;
        if len <= OWNED_READ_THRESHOLD {
            let mut data = Vec::with_capacity(len_usize);
            let mut reader = file;
            reader.read_to_end(&mut data).map_err(Error::Io)?;
            return Ok(Arc::new(Self {
                storage: Storage::Owned(data),
                random_file: None,
            }));
        }
        match map_read_only(&file, len_usize) {
            Ok(mmap) => {
                let (file, random_file) = if random {
                    (
                        Weak::new(),
                        Some(Box::new(PositionedPack {
                            file,
                            windows: Mutex::new(ReadWindows::default()),
                        })),
                    )
                } else {
                    (
                        sparse_reads
                            .map(|files| files.retain(file))
                            .unwrap_or_default(),
                        None,
                    )
                };
                Ok(Arc::new(Self {
                    storage: Storage::Mapped { map: mmap, file },
                    random_file,
                }))
            }
            Err(_) => {
                let mut data = Vec::with_capacity(len_usize);
                let mut reader = file;
                reader.seek(SeekFrom::Start(0)).map_err(Error::Io)?;
                reader.read_to_end(&mut data).map_err(Error::Io)?;
                Ok(Arc::new(Self {
                    storage: Storage::Owned(data),
                    random_file: None,
                }))
            }
        }
    }

    /// Wrap an owned buffer (tests, small index files, and stale-byte injection).
    #[must_use]
    pub(crate) fn from_owned(data: Vec<u8>) -> Arc<Self> {
        Arc::new(Self {
            storage: Storage::Owned(data),
            random_file: None,
        })
    }

    /// Copy a bounded range without faulting scattered mmap windows into the process.
    ///
    /// Reads the same descriptor that backs the mapping, so replacing the path cannot
    /// mix pack generations. Returns `None` after descriptor eviction, on I/O failure,
    /// or at an invalid offset; callers retain their normal mapped parsing and
    /// corruption/replacement recovery paths. Evicted descriptors are never reopened
    /// through a path that might now name a different inode.
    pub(crate) fn read_sparse(&self, offset: u64, out: &mut [u8]) -> Option<usize> {
        let start = usize::try_from(offset).ok()?;
        let len = out.len().min(self.len().checked_sub(start)?);
        let out = &mut out[..len];
        match &self.storage {
            Storage::Mapped { file, .. } => {
                let file = file.upgrade()?;
                #[cfg(unix)]
                {
                    use std::os::unix::fs::FileExt;
                    file.read_exact_at(out, offset).ok()?;
                }
                #[cfg(windows)]
                {
                    use std::os::windows::fs::FileExt;
                    let mut filled = 0;
                    while filled < len {
                        let n = file
                            .seek_read(&mut out[filled..], offset + filled as u64)
                            .ok()?;
                        if n == 0 {
                            return None;
                        }
                        filled += n;
                    }
                }
                #[cfg(not(any(unix, windows)))]
                {
                    let _ = file;
                    out.copy_from_slice(&self[start..start + len]);
                }
            }
            Storage::Owned(bytes) => out.copy_from_slice(&bytes[start..start + len]),
        }
        Some(len)
    }

    /// Read exactly one indexed entry through the retained file descriptor when enabled.
    /// Nearby small entries share eight 16 KiB compressed windows; larger entries read
    /// directly into the caller's reusable `buf`. The mapping stays untouched, and bounds
    /// are checked before IO.
    pub(crate) fn read_range<'a>(
        &'a self,
        start: usize,
        end: usize,
        buf: &'a mut Vec<u8>,
    ) -> Result<&'a [u8]> {
        if start > end || end > self.len() {
            return Err(Error::CorruptObject("pack entry out of range".to_owned()));
        }
        if let Some(reader) = &self.random_file {
            buf.clear();
            let len = end - start;
            if len == 0 {
                return Ok(buf);
            }
            if len > RANDOM_WINDOW_BYTES {
                buf.resize(len, 0);
                crate::pack_delta_walk::read_exact_at(&reader.file, buf, start as u64)?;
                return Ok(buf);
            }
            // Separate small windows retain nearby commit/tree entries across interleaved
            // reads without keeping pack mappings resident. Large entries bypass the cache.
            let mut windows = reader.windows.lock().unwrap_or_else(|e| e.into_inner());
            if let Some(bytes) = windows.entries.iter().find_map(|window| {
                let offset = start.checked_sub(window.start?)?;
                window.bytes.get(offset..offset + len)
            }) {
                buf.extend_from_slice(bytes);
                return Ok(buf);
            }
            let slot = windows.next;
            windows.next = (slot + 1) % RANDOM_WINDOW_COUNT;
            let window = &mut windows.entries[slot];
            window.start = None;
            window
                .bytes
                .resize(RANDOM_WINDOW_BYTES.min(self.len() - start), 0);
            crate::pack_delta_walk::read_exact_at(&reader.file, &mut window.bytes, start as u64)?;
            window.start = Some(start);
            buf.extend_from_slice(&window.bytes[..len]);
            return Ok(buf);
        }
        Ok(&self[start..end])
    }

    pub(crate) fn uses_positioned_reads(&self) -> bool {
        self.random_file.is_some()
    }

    /// Drop this mapping's pages from the process's resident set.
    ///
    /// The bytes stay valid: later reads fault the pages back in from the file (usually
    /// straight from the page cache). Owned buffers and non-Unix platforms are left as is.
    pub(crate) fn release_resident(&self) {
        #[cfg(unix)]
        if let Storage::Mapped { map, .. } = &self.storage {
            // SAFETY: the mapping is a read-only shared file mapping and the file is never
            // modified while mapped (see the type docs), so dropping its pages only makes
            // the next read reload the same bytes from the file; no borrow observes a change.
            let _ = unsafe {
                map.unchecked_advise_range(memmap2::UncheckedAdvice::DontNeed, 0, map.len())
            };
        }
    }

    /// True when backing storage is a memory map (not an owned buffer).
    #[cfg(test)]
    pub(crate) fn is_mapped_backing(&self) -> bool {
        matches!(self.storage, Storage::Mapped { .. })
    }

    /// Fingerprint of the bytes currently held in memory.
    ///
    /// `hash_bytes` is the repository object-id / pack-trailer width (from the `.idx` or
    /// repo format), not the pack header version field.
    ///
    /// # Errors
    ///
    /// Returns [`Error::CorruptObject`] when the buffer is not a well-formed pack prefix/trailer.
    pub(crate) fn fingerprint(&self, hash_bytes: usize) -> Result<PackFingerprint> {
        fingerprint_from_slice(self, hash_bytes)
    }
}

/// Caps how much of a memory-mapped pack a sequential pass over it keeps resident.
///
/// Readers [`charge`](Self::charge) the bytes they read; once the charges reach the
/// limit, the whole mapping's pages are dropped and counting starts over. Charging is
/// lock-free and shared by all worker threads. Meant for passes that move forward
/// through the pack: scattered small reads map whole fault-around windows each, so
/// they are better served by positioned file reads than by a budget.
pub(crate) struct ResidentBudget<'a> {
    pack: Option<&'a PackData>,
    limit: usize,
    charged: AtomicUsize,
}

impl<'a> ResidentBudget<'a> {
    /// A budget that never releases (the caller owns the bytes, or they are not mapped).
    #[must_use]
    pub(crate) fn unbounded() -> Self {
        Self {
            pack: None,
            limit: usize::MAX,
            charged: AtomicUsize::new(0),
        }
    }

    /// Keep at most about `limit` bytes of `pack` resident.
    #[must_use]
    pub(crate) fn new(pack: &'a PackData, limit: usize) -> Self {
        Self {
            pack: Some(pack),
            limit,
            charged: AtomicUsize::new(0),
        }
    }

    /// Record a read of `bytes` from the pack, releasing its pages when the limit is reached.
    pub(crate) fn charge(&self, bytes: usize) {
        let Some(pack) = self.pack else {
            return;
        };
        let before = self.charged.fetch_add(bytes, Ordering::Relaxed);
        if before.saturating_add(bytes) < self.limit {
            return;
        }
        // Of several threads crossing the limit together, only the one that resets wins.
        if self.charged.swap(0, Ordering::Relaxed) >= self.limit {
            pack.release_resident();
        }
    }

    /// Checksum `bytes` with `algo`, charging as it goes so a full pass stays within budget.
    pub(crate) fn checksum(&self, algo: HashAlgo, bytes: &[u8]) -> Vec<u8> {
        const CHUNK: usize = 1024 * 1024;
        let mut hasher = algo.checksum_hasher();
        for chunk in bytes.chunks(CHUNK) {
            hasher.update(chunk);
            self.charge(chunk.len());
        }
        hasher.finalize()
    }

    /// CRC-32 of `bytes` (as recorded in pack index v2), charging as it goes so hashing a
    /// large entry keeps at most about the budget's limit of the pack resident.
    pub(crate) fn crc32(&self, bytes: &[u8]) -> u32 {
        const CHUNK: usize = 1024 * 1024;
        let mut hasher = crc32fast::Hasher::new();
        for chunk in bytes.chunks(CHUNK) {
            hasher.update(chunk);
            self.charge(chunk.len());
        }
        hasher.finalize()
    }
}

fn map_read_only(file: &File, len: usize) -> std::io::Result<Mmap> {
    // SAFETY: the mapped file is only read through `&[u8]`; pack/MIDX paths are
    // replaced atomically via rename, not truncated in place (see module docs).
    // Length comes from fstat on this same `File` (see `open_from_file`).
    unsafe { MmapOptions::new().len(len).map(file) }
}

/// Read only the pack header and trailer from disk for comparison with cached bytes.
///
/// # Errors
///
/// Returns [`Error::Io`] on read failure and [`Error::CorruptObject`] for malformed packs.
pub(crate) fn fingerprint_from_file(path: &Path, hash_bytes: usize) -> Result<PackFingerprint> {
    let mut file = File::open(path).map_err(Error::Io)?;
    let file_len = file.metadata().map_err(Error::Io)?.len();
    read_fingerprint_from_open_file(&mut file, file_len, hash_bytes)
}

fn read_fingerprint_from_open_file(
    file: &mut File,
    file_len: u64,
    hash_bytes: usize,
) -> Result<PackFingerprint> {
    if hash_bytes != 20 && hash_bytes != 32 {
        return Err(Error::CorruptObject(format!(
            "unsupported pack trailer width {hash_bytes}"
        )));
    }
    let mut header = [0u8; HEADER_LEN];
    file.seek(SeekFrom::Start(0)).map_err(Error::Io)?;
    file.read_exact(&mut header).map_err(Error::Io)?;
    validate_pack_header(&header)?;
    if file_len < u64::try_from(HEADER_LEN + hash_bytes).unwrap_or(u64::MAX) {
        return Err(Error::CorruptObject(
            "pack too small for trailer".to_owned(),
        ));
    }
    let mut trailer = [0u8; 32];
    file.seek(SeekFrom::Start(file_len - hash_bytes as u64))
        .map_err(Error::Io)?;
    file.read_exact(&mut trailer[..hash_bytes])
        .map_err(Error::Io)?;
    Ok(PackFingerprint {
        header,
        hash_bytes: hash_bytes as u8,
        trailer,
    })
}

pub(crate) fn fingerprint_from_slice(bytes: &[u8], hash_bytes: usize) -> Result<PackFingerprint> {
    if hash_bytes != 20 && hash_bytes != 32 {
        return Err(Error::CorruptObject(format!(
            "unsupported pack trailer width {hash_bytes}"
        )));
    }
    if bytes.len() < HEADER_LEN {
        return Err(Error::CorruptObject("pack too small".to_owned()));
    }
    let mut header = [0u8; HEADER_LEN];
    header.copy_from_slice(&bytes[..HEADER_LEN]);
    validate_pack_header(&header)?;
    if bytes.len() < HEADER_LEN + hash_bytes {
        return Err(Error::CorruptObject(
            "pack too small for trailer".to_owned(),
        ));
    }
    let mut trailer = [0u8; 32];
    trailer[..hash_bytes].copy_from_slice(&bytes[bytes.len() - hash_bytes..]);
    Ok(PackFingerprint {
        header,
        hash_bytes: hash_bytes as u8,
        trailer,
    })
}

fn validate_pack_header(header: &[u8; HEADER_LEN]) -> Result<()> {
    if header[..4] != *PACK_SIGNATURE {
        return Err(Error::CorruptObject("bad pack signature".to_owned()));
    }
    let version = u32::from_be_bytes(header[4..8].try_into().unwrap_or([0; 4]));
    if version != 2 && version != 3 {
        return Err(Error::CorruptObject(format!(
            "unsupported pack version {version}"
        )));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::io::Write;

    #[test]
    fn positioned_ranges_reuse_buffer_and_preserve_open_file_identity() {
        let dir = tempfile::tempdir().expect("dir");
        let path = dir.path().join("random.pack");
        let bytes: Vec<u8> = (0..131072).map(|i| (i % 251) as u8).collect();
        fs::write(&path, &bytes).expect("write");
        let pack = PackData::open_random_access(&path).expect("open");
        assert!(pack.uses_positioned_reads());
        let mut buf = Vec::with_capacity(8192);
        let ptr = buf.as_ptr();
        for (start, end) in [(20000, 24000), (100, 200), (bytes.len(), bytes.len())] {
            assert_eq!(
                pack.read_range(start, end, &mut buf).expect("range"),
                &bytes[start..end]
            );
            assert_eq!(buf.as_ptr(), ptr);
        }
        assert_eq!(
            pack.read_range(70000, 100000, &mut buf)
                .expect("large entry"),
            &bytes[70000..100000]
        );
        assert!(pack.read_range(50, 40, &mut buf).is_err());
        assert!(pack.read_range(0, bytes.len() + 1, &mut buf).is_err());
        // A replacement at the path cannot pair the old mapping with a new descriptor.
        fs::rename(&path, path.with_extension("old")).expect("rename");
        fs::write(&path, vec![0; bytes.len()]).expect("replace");
        assert_eq!(
            pack.read_range(123, 456, &mut buf).expect("old inode"),
            &bytes[123..456]
        );
    }

    #[test]
    fn positioned_read_rejects_truncated_file() {
        let dir = tempfile::tempdir().expect("dir");
        let path = dir.path().join("truncated.pack");
        fs::write(&path, vec![0; 32768]).expect("write");
        let pack = PackData::open_random_access(&path).expect("open");
        // Only positioned IO is used after truncation; never dereference the mapping.
        fs::OpenOptions::new()
            .write(true)
            .open(&path)
            .expect("file")
            .set_len(0)
            .expect("truncate");
        assert!(matches!(
            pack.read_range(100, 200, &mut Vec::new()),
            Err(Error::Io(_))
        ));
    }

    #[test]
    fn owned_open_roundtrip() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("tiny.pack");
        fs::write(&path, b"PACK\x00\x00\x00\x02\x00\x00\x00\x00").expect("write");
        let data = PackData::open(&path).expect("open");
        assert_eq!(&data[..], &b"PACK\x00\x00\x00\x02\x00\x00\x00\x00"[..]);
    }

    #[test]
    fn fingerprint_detects_trailer_change() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("p.pack");
        let mut pack = Vec::from(b"PACK\x00\x00\x00\x02\x00\x00\x00\x01" as &[u8]);
        pack.extend_from_slice(&[0u8; 20]);
        pack.extend_from_slice(b"aaaaaaaaaaaaaaaaaaaa");
        fs::write(&path, &pack).expect("write");
        let fp1 = fingerprint_from_file(&path, 20).expect("fp1");
        let last = pack.len() - 1;
        pack[last] = b'b';
        fs::write(&path, &pack).expect("rewrite");
        let fp2 = fingerprint_from_file(&path, 20).expect("fp2");
        assert_ne!(fp1, fp2);
    }

    #[test]
    fn fingerprint_sha256_trailer_on_pack_version_two() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("sha256.pack");
        let mut pack = Vec::from(b"PACK\x00\x00\x00\x02\x00\x00\x00\x00" as &[u8]);
        let trailer = [0xAB_u8; 32];
        pack.extend_from_slice(&trailer);
        fs::write(&path, &pack).expect("write");
        let fp = fingerprint_from_file(&path, 32).expect("fp");
        assert_eq!(&fp.trailer[..fp.hash_bytes as usize], &trailer[..]);
        assert_eq!(fp.header[4..8], 2u32.to_be_bytes());
    }

    #[test]
    fn fingerprint_rejects_invalid_trailer_width_and_malformed_header() {
        let tiny = b"PACK\x00\x00\x00\x02\x00\x00\x00\x00";
        let owned = PackData::from_owned(tiny.to_vec());
        assert!(owned.fingerprint(19).is_err());
        assert!(owned.fingerprint(33).is_err());
        assert!(fingerprint_from_file(
            &tempfile::tempdir().expect("d").path().join("missing.pack"),
            20
        )
        .is_err());

        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("bad.pack");
        fs::write(&path, b"NOTPACK\x00\x00\x00\x02\x00\x00\x00\x00").expect("write");
        let err = fingerprint_from_file(&path, 20).expect_err("bad sig");
        assert!(matches!(err, Error::CorruptObject(_)));

        fs::write(&path, b"PACK\x00\x00\x00\x09\x00\x00\x00\x00").expect("bad ver");
        let err = fingerprint_from_file(&path, 20).expect_err("bad ver");
        assert!(matches!(err, Error::CorruptObject(_)));

        fs::write(&path, b"PACK\x00\x00\x00\x02").expect("trunc");
        assert!(matches!(
            PackData::open(&path),
            Err(Error::CorruptObject(_))
        ));

        let mut short_trailer = Vec::from(b"PACK\x00\x00\x00\x02\x00\x00\x00\x00" as &[u8]);
        short_trailer.push(0);
        fs::write(&path, &short_trailer).expect("short trailer");
        let err = fingerprint_from_file(&path, 20).expect_err("short trailer");
        assert!(matches!(err, Error::CorruptObject(_)));
    }

    #[test]
    fn owned_buffer_when_pack_at_or_below_threshold() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("medium.pack");
        let mut pack = Vec::from(b"PACK\x00\x00\x00\x02\x00\x00\x00\x00" as &[u8]);
        pack.resize(8192, 0);
        pack.extend_from_slice(b"aaaaaaaaaaaaaaaaaaaa");
        fs::write(&path, &pack).expect("write");
        let data = PackData::open(&path).expect("open owned");
        assert_eq!(data.len(), pack.len());
        let fp = data.fingerprint(20).expect("fp");
        assert_eq!(&fp.trailer[..20], b"aaaaaaaaaaaaaaaaaaaa");
    }

    #[test]
    fn open_rejects_shorter_than_header() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("short.pack");
        fs::write(&path, b"PAC").expect("write");
        assert!(matches!(
            PackData::open(&path),
            Err(Error::CorruptObject(_))
        ));
    }

    #[test]
    fn fingerprint_rejects_unsupported_trailer_width() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("p.pack");
        fs::write(&path, b"PACK\x00\x00\x00\x02\x00\x00\x00\x00").expect("write");
        assert!(fingerprint_from_file(&path, 16).is_err());
        let data = PackData::open(&path).expect("open");
        assert!(data.fingerprint(16).is_err());
    }

    #[test]
    fn fingerprint_accepts_pack_version_three_header() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("v3.pack");
        let mut pack = Vec::from(b"PACK\x00\x00\x00\x03\x00\x00\x00\x00" as &[u8]);
        pack.extend_from_slice(&[0u8; 20]);
        fs::write(&path, &pack).expect("write");
        let fp = fingerprint_from_file(&path, 20).expect("fp");
        assert_eq!(u32::from_be_bytes(fp.header[4..8].try_into().unwrap()), 3);
    }

    #[test]
    fn fingerprint_from_slice_rejects_bad_signature() {
        let bytes = b"NOPE\x00\x00\x00\x02\x00\x00\x00\x00";
        assert!(matches!(
            fingerprint_from_slice(bytes, 20),
            Err(Error::CorruptObject(_))
        ));
    }

    /// Resident KiB of the mapping that contains `addr`, from `/proc/self/smaps`.
    #[cfg(target_os = "linux")]
    fn mapping_rss_kib(addr: *const u8) -> u64 {
        let smaps = fs::read_to_string("/proc/self/smaps").expect("smaps");
        let addr = addr as usize;
        let mut inside = false;
        for line in smaps.lines() {
            let range = line.split_whitespace().next().unwrap_or_default();
            if let Some((start, end)) = range.split_once('-') {
                if let (Ok(start), Ok(end)) = (
                    usize::from_str_radix(start, 16),
                    usize::from_str_radix(end, 16),
                ) {
                    inside = (start..end).contains(&addr);
                    continue;
                }
            }
            if inside {
                if let Some(kib) = line.strip_prefix("Rss:") {
                    return kib
                        .trim()
                        .trim_end_matches("kB")
                        .trim()
                        .parse()
                        .expect("kB");
                }
            }
        }
        panic!("no mapping contains {addr:#x}");
    }

    #[cfg(target_os = "linux")]
    fn mapped_test_pack(dir: &Path, len: usize) -> (Arc<PackData>, Vec<u8>, SparseReadFiles) {
        let path = dir.join("resident.pack");
        let mut bytes = Vec::from(&b"PACK\x00\x00\x00\x02\x00\x00\x00\x00"[..]);
        bytes.extend((0..len).map(|i| u8::try_from(i % 251).unwrap_or(0)));
        fs::write(&path, &bytes).expect("write");
        let files = SparseReadFiles::default();
        let data = PackData::open_for_sparse_reads(&path, &files).expect("open");
        assert!(data.is_mapped_backing());
        (data, bytes, files)
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn sparse_reads_do_not_make_scattered_mapping_windows_resident() {
        let dir = tempfile::tempdir().expect("tempdir");
        let (data, bytes, _files) = mapped_test_pack(dir.path(), 24 << 20);
        let mut prefix = [0; 16];
        for offset in (0..bytes.len() - prefix.len()).step_by(64 << 10) {
            assert_eq!(data.read_sparse(offset as u64, &mut prefix), Some(16));
            assert_eq!(prefix, bytes[offset..offset + 16]);
        }
        assert!(mapping_rss_kib(data.as_ptr()) < 256);
        // Keep using the inode that was mapped even after its path is replaced.
        let path = dir.path().join("resident.pack");
        fs::rename(&path, dir.path().join("old.pack")).expect("rename");
        fs::write(&path, vec![0; bytes.len()]).expect("replacement");
        assert_eq!(data.read_sparse(100, &mut prefix), Some(16));
        assert_eq!(prefix, bytes[100..116]);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn evicted_sparse_descriptor_keeps_original_mapped_generation() {
        let dir = tempfile::tempdir().expect("tempdir");
        let (old, bytes, files) = mapped_test_pack(dir.path(), 16 << 10);
        let path = dir.path().join("resident.pack");
        let mut mappings = Vec::new();
        for _ in 0..SparseReadFiles::CAPACITY {
            mappings.push(PackData::open_for_sparse_reads(&path, &files).expect("map"));
        }
        assert_eq!(
            files.files.lock().expect("pool").len(),
            SparseReadFiles::CAPACITY
        );
        assert_eq!(old.read_sparse(100, &mut [0; 16]), None);
        fs::rename(&path, dir.path().join("old.pack")).expect("rename");
        fs::write(&path, vec![0; bytes.len()]).expect("replacement");
        assert_eq!(&old[100..116], &bytes[100..116]);
        let newest = mappings.last().expect("newest mapping");
        let mut prefix = [0; 16];
        assert_eq!(newest.read_sparse(100, &mut prefix), Some(16));
        assert_eq!(prefix, bytes[100..116]);
        files.clear();
        assert_eq!(newest.read_sparse(100, &mut prefix), None);
        assert_eq!(&newest[100..116], &bytes[100..116]);
    }

    #[test]
    fn sparse_owned_reads_are_bounded_at_eof() {
        let data = PackData::from_owned(b"some bytes".to_vec());
        let mut buf = [0; 16];
        assert_eq!(data.read_sparse(5, &mut buf), Some(5));
        assert_eq!(&buf[..5], b"bytes");
        assert_eq!(data.read_sparse(10, &mut buf), Some(0));
        assert_eq!(data.read_sparse(11, &mut buf), None);
        assert_eq!(data.read_sparse(u64::MAX, &mut buf), None);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn positioned_random_reads_leave_pack_mapping_nonresident() {
        let dir = tempfile::tempdir().expect("tempdir");
        let (_, bytes, _files) = mapped_test_pack(dir.path(), 24 << 20);
        let pack = PackData::open_random_access(&dir.path().join("resident.pack"))
            .expect("positioned pack");
        let mut buf = Vec::new();
        for start in (HEADER_LEN..bytes.len() - 200).step_by(16381) {
            assert_eq!(
                pack.read_range(start, start + 200, &mut buf).expect("read"),
                &bytes[start..start + 200]
            );
        }
        let rss = mapping_rss_kib(pack.as_ptr());
        assert!(rss < 1024, "random reads left {rss} KiB mapped");
        let windows = pack
            .random_file
            .as_ref()
            .expect("reader")
            .windows
            .lock()
            .expect("windows");
        assert!(windows
            .entries
            .iter()
            .all(|w| w.bytes.len() <= RANDOM_WINDOW_BYTES));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn release_resident_drops_pages_but_keeps_the_bytes() {
        let dir = tempfile::tempdir().expect("tempdir");
        let (data, bytes, _files) = mapped_test_pack(dir.path(), 24 << 20);
        let digest = HashAlgo::Sha1.checksum_digest(&data);
        assert!(
            mapping_rss_kib(data.as_ptr()) > 20 << 10,
            "a full read maps the file"
        );
        data.release_resident();
        assert!(
            mapping_rss_kib(data.as_ptr()) < 1 << 10,
            "release drops the pages"
        );
        assert_eq!(&data[..], &bytes[..]);
        assert_eq!(HashAlgo::Sha1.checksum_digest(&data), digest);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn resident_budget_bounds_a_full_pass() {
        let dir = tempfile::tempdir().expect("tempdir");
        let (data, bytes, _files) = mapped_test_pack(dir.path(), 24 << 20);
        data.release_resident();
        let budget = ResidentBudget::new(&data, 4 << 20);
        assert_eq!(
            budget.checksum(HashAlgo::Sha1, &data),
            HashAlgo::Sha1.checksum_digest(&bytes)
        );
        let rss = mapping_rss_kib(data.as_ptr());
        assert!(rss <= 6 << 10, "budgeted pass left {rss} KiB resident");

        data.release_resident();
        let unbounded = ResidentBudget::unbounded();
        let _ = unbounded.checksum(HashAlgo::Sha1, &data);
        assert!(
            mapping_rss_kib(data.as_ptr()) > 20 << 10,
            "unbounded never releases"
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn resident_budget_bounds_a_crc_pass() {
        let dir = tempfile::tempdir().expect("tempdir");
        let (data, bytes, _files) = mapped_test_pack(dir.path(), 24 << 20);
        data.release_resident();
        let budget = ResidentBudget::new(&data, 4 << 20);
        assert_eq!(budget.crc32(&data), crc32fast::hash(&bytes));
        let rss = mapping_rss_kib(data.as_ptr());
        assert!(rss <= 6 << 10, "budgeted CRC left {rss} KiB resident");
    }

    #[test]
    fn resident_budget_crc_matches_on_owned_bytes() {
        let bytes: Vec<u8> = (0..3_000_000u32).map(|i| (i % 251) as u8).collect();
        let data = PackData::from_owned(bytes.clone());
        assert_eq!(
            ResidentBudget::new(&data, 1).crc32(&data),
            crc32fast::hash(&bytes)
        );
        assert_eq!(ResidentBudget::unbounded().crc32(&[]), crc32fast::hash(&[]));
    }

    #[test]
    fn release_resident_on_owned_bytes_is_a_no_op() {
        let data = PackData::from_owned(b"PACK\x00\x00\x00\x02\x00\x00\x00\x00".to_vec());
        data.release_resident();
        let budget = ResidentBudget::new(&data, 0);
        budget.charge(1);
        assert_eq!(&data[..4], b"PACK");
    }

    #[test]
    fn mmap_large_sparse_pack() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("sparse.pack");
        let mut file = File::create(&path).expect("create");
        file.write_all(b"PACK\x00\x00\x00\x02\x00\x00\x00\x01")
            .expect("header");
        let huge: u64 = 1 << 32;
        file.seek(SeekFrom::Start(huge)).expect("seek");
        file.write_all(b"blob payload padding").expect("payload");
        file.write_all(b"bbbbbbbbbbbbbbbbbbbb").expect("trailer");
        drop(file);
        let len = fs::metadata(&path).expect("meta").len();
        assert!(len > huge);
        let data = PackData::open(&path).expect("mmap sparse");
        assert_eq!(&data[..4], b"PACK");
        assert_eq!(data.len() as u64, len);
    }
}

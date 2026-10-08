//! Memory-mapped pack (and MIDX) file bytes with an owned-buffer fallback.
//!
//! This module is the only place that uses `unsafe` for pack backing storage.

use crate::error::{Error, Result};
use memmap2::{Mmap, MmapOptions};
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::ops::Deref;
use std::path::Path;
use std::sync::Arc;

const PACK_SIGNATURE: &[u8; 4] = b"PACK";
const HEADER_LEN: usize = 12;

/// Files at or below this size are read into an owned `Vec` (tiny packs, tests,
/// platforms or filesystems where mapping fails).
const OWNED_READ_THRESHOLD: u64 = 8192;

/// Header plus trailing pack checksum — enough to detect on-disk replacement when
/// mtime/size are unchanged (same-second repack) without re-reading the whole file.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct PackFingerprint {
    /// `PACK`, version, and object count (`12` bytes).
    pub header: [u8; HEADER_LEN],
    /// Pack trailer hash at the repository hash width (20 for SHA-1, 32 for SHA-256).
    pub trailer: Vec<u8>,
}

enum Storage {
    Mapped(Mmap),
    Owned(Vec<u8>),
}

/// Immutable bytes of a `.pack` or MIDX file.
///
/// # Safety (memory-mapped storage)
///
/// Pack, index, and MIDX files are append-only/immutable at a stable path: grit
/// and Git publish updates by writing a temporary file and renaming it into place,
/// so active readers never map a file that is being truncated in place. If a pack
/// were truncated while mapped, reads could fault or return stale bytes — the same
/// risk Git accepts for memory-mapped packfiles. `open` uses a single file
/// descriptor and `fstat`s that handle before mapping so length and inode stay paired.
pub struct PackData {
    storage: Storage,
}

impl Deref for PackData {
    type Target = [u8];

    fn deref(&self) -> &[u8] {
        match &self.storage {
            Storage::Mapped(m) => m.deref(),
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
        let file = File::open(path).map_err(Error::Io)?;
        Self::open_from_file(file)
    }

    fn open_from_file(file: File) -> Result<Arc<Self>> {
        let len = file.metadata().map_err(Error::Io)?.len();
        if len < HEADER_LEN as u64 {
            return Err(Error::CorruptObject(format!(
                "pack file is too small ({len} bytes)"
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
            }));
        }
        match map_read_only(&file, len_usize) {
            Ok(mmap) => Ok(Arc::new(Self {
                storage: Storage::Mapped(mmap),
            })),
            Err(_) => {
                let mut data = Vec::with_capacity(len_usize);
                let mut reader = file;
                reader.seek(SeekFrom::Start(0)).map_err(Error::Io)?;
                reader.read_to_end(&mut data).map_err(Error::Io)?;
                Ok(Arc::new(Self {
                    storage: Storage::Owned(data),
                }))
            }
        }
    }

    /// Wrap an owned buffer (tests and stale-byte injection).
    #[cfg(test)]
    #[must_use]
    pub(crate) fn from_owned(data: Vec<u8>) -> Arc<Self> {
        Arc::new(Self {
            storage: Storage::Owned(data),
        })
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
    let mut trailer = vec![0u8; hash_bytes];
    file.seek(SeekFrom::Start(file_len - hash_bytes as u64))
        .map_err(Error::Io)?;
    file.read_exact(&mut trailer).map_err(Error::Io)?;
    Ok(PackFingerprint { header, trailer })
}

fn fingerprint_from_slice(bytes: &[u8], hash_bytes: usize) -> Result<PackFingerprint> {
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
    Ok(PackFingerprint {
        header,
        trailer: bytes[bytes.len() - hash_bytes..].to_vec(),
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
        assert_eq!(fp.trailer, trailer.to_vec());
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
        assert_eq!(fp.trailer, b"aaaaaaaaaaaaaaaaaaaa".to_vec());
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

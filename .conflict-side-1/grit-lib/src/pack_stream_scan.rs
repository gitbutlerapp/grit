//! Count the objects in a pack while its bytes are still arriving.
//!
//! A fetched pack is spooled to disk and indexed only once the transfer ends,
//! so during the transfer a caller knows how many bytes have arrived but not
//! how far through the pack's objects it is. [`PackStreamScanner`] walks the
//! entry headers as bytes are fed to it and inflates each entry's zlib stream
//! just far enough to find where the next entry starts. The object total comes
//! from the 12-byte pack header, so callers can show `received / total`.
//!
//! The scanner keeps no inflated data; it costs one inflate pass over the
//! stream, which is why fetch only runs it (on a background thread) when the
//! progress sink asks for it.
//!
//! ```rust
//! use grit_lib::objects::pack_stream_scan::PackStreamScanner;
//! use grit_lib::objects::HashAlgo;
//!
//! let mut scanner = PackStreamScanner::new(HashAlgo::Sha1);
//! // An empty pack: signature, version 2, zero objects.
//! scanner.feed(b"PACK\0\0\0\x02\0\0\0\0").unwrap();
//! let counts = scanner.counts().unwrap();
//! assert_eq!((counts.received, counts.total), (0, 0));
//! assert!(scanner.is_complete());
//! ```

use flate2::{Decompress, FlushDecompress, Status};
use thiserror::Error;

use crate::objects::HashAlgo;
use crate::progress::ReceivedObjects;

/// Why a pack stream could not be scanned.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
#[non_exhaustive]
pub enum PackStreamScanError {
    /// The stream does not begin with the `PACK` signature.
    #[error("stream does not start with a pack signature")]
    BadSignature,
    /// The pack header names a version other than 2 or 3.
    #[error("unsupported pack version {0}")]
    UnsupportedVersion(u32),
    /// An entry header carries a type code that no pack entry uses.
    #[error("invalid pack entry type {0}")]
    InvalidEntryType(u8),
    /// An entry's size or delta offset varint runs past any valid length.
    #[error("pack entry header is too long")]
    HeaderTooLong,
    /// An entry's compressed data is not a valid zlib stream (or the pack
    /// uses a different hash length than the scanner was created with).
    #[error("corrupt compressed data in pack entry {0}")]
    Inflate(u64),
}

/// Longest valid size or offset varint: 64 bits at 7 bits per byte.
const MAX_VARINT_BYTES: u8 = 10;
/// Inflated bytes are discarded; this is only scratch space for zlib.
const SCRATCH_LEN: usize = 64 * 1024;

const OBJ_OFS_DELTA: u8 = 6;
const OBJ_REF_DELTA: u8 = 7;

#[derive(Debug)]
enum State {
    /// Collecting the 12-byte pack header.
    PackHeader { buf: [u8; 12], len: usize },
    /// Reading an entry's type-and-size varint; `kind` is its type code.
    EntryHeader { read: u8, kind: u8 },
    /// Reading an `OFS_DELTA` entry's base offset varint.
    BaseOffset { read: u8 },
    /// Skipping a `REF_DELTA` entry's base object id.
    BaseId { remaining: usize },
    /// Inflating an entry's data to find where it ends.
    Data,
    /// Every object is accounted for; the rest is the trailing checksum.
    Trailer,
}

/// Incremental object counter over a pack byte stream.
///
/// Feed it the pack's bytes in order, starting at the `PACK` signature, in
/// chunks of any size; [`Self::counts`] reports how many entries have been
/// fully received.
pub struct PackStreamScanner {
    hash_len: usize,
    state: State,
    total: Option<u64>,
    received: u64,
    deltas: u64,
    entry_is_delta: bool,
    inflate: Decompress,
    scratch: Box<[u8]>,
}

impl std::fmt::Debug for PackStreamScanner {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PackStreamScanner")
            .field("hash_len", &self.hash_len)
            .field("state", &self.state)
            .field("total", &self.total)
            .field("received", &self.received)
            .field("deltas", &self.deltas)
            .finish_non_exhaustive()
    }
}

impl PackStreamScanner {
    /// Create a scanner for a pack whose `REF_DELTA` bases are `hash` ids.
    ///
    /// # Parameters
    ///
    /// - `hash`: the object format of the pack (decides the length of a
    ///   `REF_DELTA` entry's base id).
    #[must_use]
    pub fn new(hash: HashAlgo) -> Self {
        Self {
            hash_len: hash.len(),
            state: State::PackHeader {
                buf: [0; 12],
                len: 0,
            },
            total: None,
            received: 0,
            deltas: 0,
            entry_is_delta: false,
            inflate: Decompress::new(true),
            scratch: vec![0; SCRATCH_LEN].into_boxed_slice(),
        }
    }

    /// Objects received so far, or `None` until the pack header has arrived.
    #[must_use]
    pub fn counts(&self) -> Option<ReceivedObjects> {
        self.total.map(|total| ReceivedObjects {
            received: self.received,
            total,
            deltas: self.deltas,
        })
    }

    /// Whether every object named in the pack header has been received.
    #[must_use]
    pub fn is_complete(&self) -> bool {
        matches!(self.state, State::Trailer)
    }

    /// Scan the next `data` bytes of the pack.
    ///
    /// # Parameters
    ///
    /// - `data`: the bytes that follow everything fed so far.
    ///
    /// # Errors
    ///
    /// Returns a [`PackStreamScanError`] when the bytes are not a well-formed
    /// pack. The scanner should be discarded after an error.
    pub fn feed(&mut self, mut data: &[u8]) -> Result<(), PackStreamScanError> {
        while !data.is_empty() {
            match &mut self.state {
                State::PackHeader { buf, len } => {
                    let take = (buf.len() - *len).min(data.len());
                    buf[*len..*len + take].copy_from_slice(&data[..take]);
                    *len += take;
                    data = &data[take..];
                    if *len == buf.len() {
                        let header = *buf;
                        self.read_pack_header(&header)?;
                    }
                }
                State::EntryHeader { read, kind } => {
                    let byte = data[0];
                    data = &data[1..];
                    if *read == 0 {
                        *kind = (byte >> 4) & 0x7;
                        if !matches!(*kind, 1..=4 | OBJ_OFS_DELTA | OBJ_REF_DELTA) {
                            return Err(PackStreamScanError::InvalidEntryType(*kind));
                        }
                    }
                    *read += 1;
                    if *read > MAX_VARINT_BYTES {
                        return Err(PackStreamScanError::HeaderTooLong);
                    }
                    if byte & 0x80 == 0 {
                        let kind = *kind;
                        self.entry_is_delta = matches!(kind, OBJ_OFS_DELTA | OBJ_REF_DELTA);
                        self.state = match kind {
                            OBJ_OFS_DELTA => State::BaseOffset { read: 0 },
                            OBJ_REF_DELTA => State::BaseId {
                                remaining: self.hash_len,
                            },
                            _ => State::Data,
                        };
                    }
                }
                State::BaseOffset { read } => {
                    let byte = data[0];
                    data = &data[1..];
                    *read += 1;
                    if *read > MAX_VARINT_BYTES {
                        return Err(PackStreamScanError::HeaderTooLong);
                    }
                    if byte & 0x80 == 0 {
                        self.state = State::Data;
                    }
                }
                State::BaseId { remaining } => {
                    let take = (*remaining).min(data.len());
                    *remaining -= take;
                    data = &data[take..];
                    if *remaining == 0 {
                        self.state = State::Data;
                    }
                }
                State::Data => {
                    data = self.inflate_entry(data)?;
                }
                State::Trailer => return Ok(()),
            }
        }
        Ok(())
    }

    fn read_pack_header(&mut self, header: &[u8; 12]) -> Result<(), PackStreamScanError> {
        if &header[..4] != b"PACK" {
            return Err(PackStreamScanError::BadSignature);
        }
        let version = u32::from_be_bytes([header[4], header[5], header[6], header[7]]);
        if !matches!(version, 2 | 3) {
            return Err(PackStreamScanError::UnsupportedVersion(version));
        }
        let total = u64::from(u32::from_be_bytes([
            header[8], header[9], header[10], header[11],
        ]));
        self.total = Some(total);
        self.state = if total == 0 {
            State::Trailer
        } else {
            State::EntryHeader { read: 0, kind: 0 }
        };
        Ok(())
    }

    /// Inflate as much of the current entry as `data` holds; return the bytes
    /// left over once the entry ends (empty when it needs more input).
    fn inflate_entry<'d>(&mut self, mut data: &'d [u8]) -> Result<&'d [u8], PackStreamScanError> {
        loop {
            let in_before = self.inflate.total_in();
            let out_before = self.inflate.total_out();
            let status = self
                .inflate
                .decompress(data, &mut self.scratch, FlushDecompress::None)
                .map_err(|_| PackStreamScanError::Inflate(self.received))?;
            let used = usize::try_from(self.inflate.total_in() - in_before).unwrap_or(data.len());
            data = &data[used..];
            match status {
                Status::StreamEnd => {
                    self.finish_entry();
                    return Ok(data);
                }
                Status::Ok | Status::BufError => {
                    let produced = self.inflate.total_out() != out_before;
                    if data.is_empty() || (used == 0 && !produced) {
                        return Ok(data);
                    }
                }
            }
        }
    }

    fn finish_entry(&mut self) {
        self.inflate.reset(true);
        self.received += 1;
        if self.entry_is_delta {
            self.deltas += 1;
        }
        self.state = if Some(self.received) == self.total {
            State::Trailer
        } else {
            State::EntryHeader { read: 0, kind: 0 }
        };
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use grit_test_support::objects::{HashAlgo as FixtureAlgo, ObjectKind as PK, PackBuilder};

    /// A pack with every entry shape: full objects, a blob larger than the
    /// scratch buffer, an `OFS_DELTA` and a `REF_DELTA`.
    fn sample_pack() -> Vec<u8> {
        let mut pb = PackBuilder::new(FixtureAlgo::Sha1);
        pb.add_full(
            PK::Commit,
            b"tree 0000000000000000000000000000000000000000\n\nc\n",
        );
        pb.add_full(PK::Tree, b"");
        let base = b"blob base contents\n";
        let base_idx = pb.add_full(PK::Blob, base);
        let big: Vec<u8> = (0..300_000u32).flat_map(u32::to_le_bytes).collect();
        pb.add_full(PK::Blob, &big);
        let delta = crate::delta_encode::encode_lcp_delta(base, b"blob base contents, edited\n")
            .expect("delta");
        pb.add_ofs_delta(base_idx, &delta, delta.len());
        pb.add_ref_delta(&[0xab; 20], &delta, delta.len());
        pb.add_full(
            PK::Tag,
            b"object 0000000000000000000000000000000000000000\n",
        );
        pb.build().bytes
    }

    fn expected() -> ReceivedObjects {
        ReceivedObjects {
            received: 7,
            total: 7,
            deltas: 2,
        }
    }

    #[test]
    fn counts_a_whole_pack_fed_at_once() {
        let mut scanner = PackStreamScanner::new(HashAlgo::Sha1);
        scanner.feed(&sample_pack()).expect("scan");
        assert_eq!(scanner.counts(), Some(expected()));
        assert!(scanner.is_complete());
    }

    #[test]
    fn counts_the_same_however_the_stream_is_split() {
        let pack = sample_pack();
        for chunk in [1usize, 7, 4096, 65_519] {
            let mut scanner = PackStreamScanner::new(HashAlgo::Sha1);
            for piece in pack.chunks(chunk) {
                scanner.feed(piece).expect("scan");
            }
            assert_eq!(scanner.counts(), Some(expected()), "chunk {chunk}");
        }
    }

    #[test]
    fn reports_partial_progress_mid_stream() {
        let pack = sample_pack();
        let mut scanner = PackStreamScanner::new(HashAlgo::Sha1);
        assert_eq!(scanner.counts(), None);
        scanner.feed(&pack[..12]).expect("header");
        let header_only = scanner.counts().expect("total known from header");
        assert_eq!((header_only.received, header_only.total), (0, 7));
        scanner.feed(&pack[12..pack.len() / 2]).expect("half");
        let half = scanner.counts().expect("counts");
        assert!(half.received > 0 && half.received < 7, "{half:?}");
        assert!(!scanner.is_complete());
    }

    #[test]
    fn rejects_streams_that_are_not_packs() {
        let mut scanner = PackStreamScanner::new(HashAlgo::Sha1);
        assert_eq!(
            scanner.feed(b"NOPE\0\0\0\x02\0\0\0\x01"),
            Err(PackStreamScanError::BadSignature)
        );
        let mut scanner = PackStreamScanner::new(HashAlgo::Sha1);
        assert_eq!(
            scanner.feed(b"PACK\0\0\0\x09\0\0\0\x01"),
            Err(PackStreamScanError::UnsupportedVersion(9))
        );
        let mut scanner = PackStreamScanner::new(HashAlgo::Sha1);
        // Type code 5 is reserved.
        assert_eq!(
            scanner.feed(b"PACK\0\0\0\x02\0\0\0\x01\x50"),
            Err(PackStreamScanError::InvalidEntryType(5))
        );
        let mut scanner = PackStreamScanner::new(HashAlgo::Sha1);
        let mut runaway = b"PACK\0\0\0\x02\0\0\0\x01".to_vec();
        runaway.extend([0xb0; 12]);
        assert_eq!(
            scanner.feed(&runaway),
            Err(PackStreamScanError::HeaderTooLong)
        );
        let mut scanner = PackStreamScanner::new(HashAlgo::Sha1);
        assert_eq!(
            scanner.feed(b"PACK\0\0\0\x02\0\0\0\x01\x30\xff\xff\xff\xff"),
            Err(PackStreamScanError::Inflate(0))
        );
    }

    #[test]
    fn ignores_the_trailer_after_the_last_object() {
        let mut pack = sample_pack();
        pack.extend_from_slice(b"anything after the checksum");
        let mut scanner = PackStreamScanner::new(HashAlgo::Sha1);
        scanner.feed(&pack).expect("scan");
        assert_eq!(scanner.counts(), Some(expected()));
    }
}

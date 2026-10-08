//! PACK version 2 byte stream writer for tests.

use std::io::Write;

use flate2::write::ZlibEncoder;
use flate2::Compression;

use super::hash::HashAlgo;

/// Git object type bits in pack object headers (not identical to loose object types).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum ObjectKind {
    /// Commit (type 1).
    Commit = 1,
    /// Tree (type 2).
    Tree = 2,
    /// Blob (type 3).
    Blob = 3,
    /// Annotated tag (type 4).
    Tag = 4,
    /// Offset delta (type 6).
    OfsDelta = 6,
    /// Reference delta (type 7).
    RefDelta = 7,
}

impl ObjectKind {
    const fn type_bits(self) -> u8 {
        self as u8
    }
}

#[derive(Debug, Clone)]
enum Entry {
    Full {
        kind: ObjectKind,
        data: Vec<u8>,
    },
    OfsDelta {
        base_index: usize,
        delta: Vec<u8>,
        uncompressed_delta_size: usize,
    },
    RefDelta {
        base_oid: Vec<u8>,
        delta: Vec<u8>,
        uncompressed_delta_size: usize,
    },
    Raw {
        header: Vec<u8>,
        body: Vec<u8>,
    },
}

/// Labels for well-known byte ranges inside a built pack.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PackOffsetLabel {
    /// First byte of the 4-byte big-endian object count in the pack header.
    HeaderObjectCount,
    /// Start of a given entry's type/size varint (`index` is the entry index).
    EntryHeader(usize),
    /// First byte of the pack hash trailer.
    Trailer,
}

/// Output of [`PackBuilder::build`].
#[derive(Debug, Clone)]
pub struct PackBuilt {
    /// Complete PACK v2 byte stream including trailer (unless configured otherwise).
    pub bytes: Vec<u8>,
    /// Pack offset of each entry's type/size header, in insertion order.
    pub entry_offsets: Vec<usize>,
    hash_len: usize,
}

impl PackBuilt {
    /// Resolve a labeled region to an absolute byte offset in [`Self::bytes`].
    ///
    /// Returns `None` when the label is out of range (for example an unknown entry index).
    #[must_use]
    pub fn resolve_offset(&self, label: PackOffsetLabel) -> Option<usize> {
        match label {
            PackOffsetLabel::HeaderObjectCount => Some(8),
            PackOffsetLabel::EntryHeader(i) => self.entry_offsets.get(i).copied(),
            PackOffsetLabel::Trailer => {
                if self.hash_len == 0 {
                    return None;
                }
                Some(self.bytes.len().saturating_sub(self.hash_len))
            }
        }
    }
}

/// Incremental PACK v2 writer for tests.
#[derive(Debug)]
pub struct PackBuilder {
    algo: HashAlgo,
    entries: Vec<Entry>,
    header_object_count: Option<u32>,
    trailer_mode: TrailerMode,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TrailerMode {
    Correct,
    Corrupt,
    Omit,
}

impl PackBuilder {
    /// Create a builder that seals packs with the given hash algorithm.
    #[must_use]
    pub fn new(algo: HashAlgo) -> Self {
        Self {
            algo,
            entries: Vec::new(),
            header_object_count: None,
            trailer_mode: TrailerMode::Correct,
        }
    }

    /// Override the object count in the 12-byte pack header (wrong counts for tests).
    pub fn set_header_object_count(&mut self, count: u32) -> &mut Self {
        self.header_object_count = Some(count);
        self
    }

    /// Append a zlib-compressed whole object.
    ///
    /// Returns the entry index for use with [`Self::add_ofs_delta`].
    pub fn add_full(&mut self, kind: ObjectKind, data: &[u8]) -> usize {
        let idx = self.entries.len();
        self.entries.push(Entry::Full {
            kind,
            data: data.to_vec(),
        });
        idx
    }

    /// Append an OFS_DELTA object relative to a prior in-pack entry.
    ///
    /// `uncompressed_delta_size` is the size of the delta instruction stream after
    /// zlib decompression (Git's pack object header size for deltas).
    pub fn add_ofs_delta(
        &mut self,
        base_index: usize,
        delta: &[u8],
        uncompressed_delta_size: usize,
    ) -> usize {
        let idx = self.entries.len();
        self.entries.push(Entry::OfsDelta {
            base_index,
            delta: delta.to_vec(),
            uncompressed_delta_size,
        });
        idx
    }

    /// Append a REF_DELTA object with an explicit base object id.
    ///
    /// `uncompressed_delta_size` is the size of the delta instruction stream after
    /// zlib decompression.
    pub fn add_ref_delta(
        &mut self,
        base_oid: &[u8],
        delta: &[u8],
        uncompressed_delta_size: usize,
    ) -> usize {
        let idx = self.entries.len();
        self.entries.push(Entry::RefDelta {
            base_oid: base_oid.to_vec(),
            delta: delta.to_vec(),
            uncompressed_delta_size,
        });
        idx
    }

    /// Append an entry with a caller-supplied header and body (malformed headers).
    pub fn add_raw_entry(&mut self, header: &[u8], body: &[u8]) -> usize {
        let idx = self.entries.len();
        self.entries.push(Entry::Raw {
            header: header.to_vec(),
            body: body.to_vec(),
        });
        idx
    }

    /// Duplicate a prior entry verbatim (including delta linkage).
    pub fn duplicate_entry(&mut self, index: usize) -> Option<usize> {
        let entry = self.entries.get(index)?.clone();
        let idx = self.entries.len();
        self.entries.push(entry);
        Some(idx)
    }

    /// Replace the next added full object's type/size varint with raw `header` bytes.
    pub fn add_full_with_header_override(&mut self, header: &[u8], uncompressed: &[u8]) -> usize {
        let body = zlib_compress(uncompressed);
        self.add_raw_entry(header, &body)
    }

    /// Seal the pack with an incorrect hash trailer.
    pub fn set_corrupt_trailer(&mut self) -> &mut Self {
        self.trailer_mode = TrailerMode::Corrupt;
        self
    }

    /// Omit the hash trailer entirely.
    pub fn set_omit_trailer(&mut self) -> &mut Self {
        self.trailer_mode = TrailerMode::Omit;
        self
    }

    /// Serialize the pack and record per-entry offsets.
    #[must_use]
    pub fn build(self) -> PackBuilt {
        let mut out = Vec::new();
        out.extend_from_slice(b"PACK");
        out.extend_from_slice(&2u32.to_be_bytes());
        let count = self
            .header_object_count
            .unwrap_or_else(|| u32::try_from(self.entries.len()).unwrap_or(u32::MAX));
        out.extend_from_slice(&count.to_be_bytes());

        let mut entry_offsets = Vec::with_capacity(self.entries.len());
        let mut entry_start_offsets = Vec::with_capacity(self.entries.len());

        for entry in &self.entries {
            let start = out.len();
            entry_offsets.push(start);
            entry_start_offsets.push(start);
            match entry {
                Entry::Full { kind, data } => {
                    append_object_header(&mut out, kind.type_bits(), data.len());
                    out.extend_from_slice(&zlib_compress(data));
                }
                Entry::OfsDelta {
                    base_index,
                    delta,
                    uncompressed_delta_size,
                } => {
                    let base_off = entry_start_offsets.get(*base_index).copied().unwrap_or(0);
                    let object_start = start;
                    append_object_header(
                        &mut out,
                        ObjectKind::OfsDelta.type_bits(),
                        *uncompressed_delta_size,
                    );
                    append_ofs_delta_distance(&mut out, object_start, base_off);
                    out.extend_from_slice(&zlib_compress(delta));
                }
                Entry::RefDelta {
                    base_oid,
                    delta,
                    uncompressed_delta_size,
                } => {
                    append_object_header(
                        &mut out,
                        ObjectKind::RefDelta.type_bits(),
                        *uncompressed_delta_size,
                    );
                    out.extend_from_slice(base_oid);
                    out.extend_from_slice(&zlib_compress(delta));
                }
                Entry::Raw { header, body } => {
                    out.extend_from_slice(header);
                    out.extend_from_slice(body);
                }
            }
        }

        match self.trailer_mode {
            TrailerMode::Correct => {
                let digest = self.algo.digest_pack(&out);
                out.extend_from_slice(&digest);
            }
            TrailerMode::Corrupt => {
                let mut digest = self.algo.digest_pack(&out);
                if let Some(b) = digest.first_mut() {
                    *b ^= 0xff;
                }
                out.extend_from_slice(&digest);
            }
            TrailerMode::Omit => {}
        }

        PackBuilt {
            bytes: out,
            entry_offsets,
            hash_len: match self.trailer_mode {
                TrailerMode::Omit => 0,
                _ => self.algo.oid_len(),
            },
        }
    }
}

fn append_object_header(buf: &mut Vec<u8>, type_bits: u8, size: usize) {
    let mut n = size;
    let mut first = (type_bits << 4) | (n & 0x0f) as u8;
    n >>= 4;
    while n > 0 {
        buf.push(first | 0x80);
        first = (n & 0x7f) as u8;
        n >>= 7;
    }
    buf.push(first);
}

fn append_ofs_delta_distance(buf: &mut Vec<u8>, object_start: usize, base_offset: usize) {
    let mut ofs = u64::try_from(object_start.saturating_sub(base_offset)).unwrap_or(u64::MAX);
    let mut dheader = [0u8; 32];
    let mut pos = dheader.len() - 1;
    dheader[pos] = (ofs & 0x7f) as u8;
    while {
        ofs >>= 7;
        ofs != 0
    } {
        pos -= 1;
        ofs -= 1;
        dheader[pos] = 0x80 | ((ofs & 0x7f) as u8);
    }
    buf.extend_from_slice(&dheader[pos..]);
}

fn zlib_compress(data: &[u8]) -> Vec<u8> {
    let mut enc = ZlibEncoder::new(Vec::new(), Compression::default());
    if enc.write_all(data).is_err() {
        return Vec::new();
    }
    enc.finish().unwrap_or_default()
}

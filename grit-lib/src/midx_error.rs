//! Typed errors for multi-pack-index load and verification.

use thiserror::Error;

/// A fatal multi-pack-index condition (Git `die()` after `error:` lines).
#[non_exhaustive]
#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum MidxError {
    /// Trailing or header signature does not match `MIDX`.
    #[error("multi-pack-index signature 0x{found:08x} does not match signature 0x{expected:08x}")]
    SignatureMismatch {
        /// Value read from the file header.
        found: u32,
        /// Expected MIDX magic (`0x4d494458`).
        expected: u32,
    },

    /// On-disk format version byte is not supported.
    #[error("multi-pack-index version {version} not recognized")]
    VersionNotRecognized {
        /// Format version byte read from the file header.
        version: u8,
    },

    /// Header hash-version byte does not match the repository object format.
    #[error("multi-pack-index hash version {found} does not match version {expected}")]
    HashVersionMismatch {
        /// Hash version byte read from the MIDX header.
        found: u8,
        /// Hash version required by the repository object format.
        expected: u8,
    },

    /// Required pack-names chunk is missing or cannot be parsed.
    #[error("multi-pack-index required pack-name chunk missing or corrupted")]
    RequiredPackNameChunkMissing,

    /// Required OID fanout chunk is missing.
    #[error("multi-pack-index required OID fanout chunk missing or corrupted")]
    RequiredOidFanoutChunkMissing,

    /// OID fanout chunk size is not 256 × 4 bytes.
    #[error("multi-pack-index OID fanout is of the wrong size")]
    OidFanoutWrongSize,

    /// OID fanout table is not monotonically non-decreasing.
    #[error("oid fanout out of order: fanout[{index}] = {first:x} > {second:x} = fanout[{next}]")]
    OidFanoutOutOfOrder {
        /// Zero-based fanout bucket index that violated ordering.
        index: usize,
        /// Fanout count at `index` (must be ≤ `second`).
        first: u32,
        /// Fanout count at the next bucket.
        second: u32,
        /// Index of the next fanout bucket (`index + 1`).
        next: usize,
    },

    /// Required OID lookup chunk is missing.
    #[error("multi-pack-index required OID lookup chunk missing or corrupted")]
    RequiredOidLookupChunkMissing,

    /// OID lookup chunk size does not match `num_objects × hash_len`.
    #[error("multi-pack-index OID lookup chunk is the wrong size")]
    OidLookupWrongSize,

    /// Required object-offsets chunk is missing.
    #[error("multi-pack-index required object offsets chunk missing or corrupted")]
    RequiredObjectOffsetsChunkMissing,

    /// Object-offsets chunk size does not match `num_objects × 8`.
    #[error("multi-pack-index object offset chunk is the wrong size")]
    ObjectOffsetsWrongSize,

    /// A pack name in the pack-names chunk is not NUL-terminated.
    #[error("multi-pack-index pack-name chunk is too short")]
    PackNameChunkTooShort,

    /// v1 MIDX pack names are not strictly increasing.
    #[error("multi-pack-index pack names out of order: '{previous}' before '{name}'")]
    PackNamesOutOfOrder {
        /// Previous pack file name in sorted order.
        previous: String,
        /// Pack file name that breaks monotonic ordering.
        name: String,
    },

    /// Large-offset chunk does not cover a referenced large offset slot.
    #[error("multi-pack-index large offset out of bounds")]
    LargeOffsetOutOfBounds,

    /// Chunk table of contents could not be parsed (Git `die()` cases in `read_table_of_contents`).
    #[error("{detail}")]
    InvalidChunkTable {
        /// Parse failure detail for the chunk table of contents.
        detail: String,
    },

    /// Write attempted with no pack indexes to cover.
    #[error("no packs to index")]
    NoPacksToIndex,

    /// A write requested an on-disk version other than `1` or `2` (for example through
    /// `midx.version`).
    #[error("cannot write multi-pack-index version {version}: only versions 1 and 2 exist")]
    UnknownWriteVersion {
        /// Requested version, as configured.
        version: i64,
    },

    /// Preferred pack has no objects in the MIDX.
    #[error("preferred pack '{pack}' is empty in multi-pack-index")]
    PreferredPackEmpty {
        /// Pack file basename (`.pack` suffix).
        pack: String,
    },

    /// Preferred pack name does not appear in the pack list.
    #[error("unknown preferred pack '{name}'")]
    UnknownPreferredPack {
        /// Preferred pack basename that was not found in the MIDX pack list.
        name: String,
    },

    /// An existing MIDX names a pack whose `.pack` file is missing during rewrite.
    #[error("could not load pack {pack_index}")]
    ReferencedPackMissing {
        /// Zero-based index into the MIDX pack list.
        pack_index: usize,
    },

    /// Reachability bitmap generation for a MIDX failed.
    #[error("multi-pack-index bitmap write failed: {detail}")]
    BitmapWriteFailed {
        /// Why bitmap generation or writing failed.
        detail: String,
    },
}

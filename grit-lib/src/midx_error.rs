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
    VersionNotRecognized { version: u8 },

    /// Header hash-version byte does not match the repository object format.
    #[error("multi-pack-index hash version {found} does not match version {expected}")]
    HashVersionMismatch { found: u8, expected: u8 },

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
        index: usize,
        first: u32,
        second: u32,
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
    PackNamesOutOfOrder { previous: String, name: String },

    /// Large-offset chunk does not cover a referenced large offset slot.
    #[error("multi-pack-index large offset out of bounds")]
    LargeOffsetOutOfBounds,

    /// Chunk table of contents could not be parsed (Git `die()` cases in `read_table_of_contents`).
    #[error("{detail}")]
    InvalidChunkTable { detail: String },

    /// Write attempted with no pack indexes to cover.
    #[error("no packs to index")]
    NoPacksToIndex,

    /// Preferred pack has no objects in the MIDX.
    #[error("preferred pack '{pack}' is empty in multi-pack-index")]
    PreferredPackEmpty { pack: String },

    /// Preferred pack name does not appear in the pack list.
    #[error("unknown preferred pack '{name}'")]
    UnknownPreferredPack { name: String },

    /// An existing MIDX names a pack whose `.pack` file is missing during rewrite.
    #[error("could not load pack {pack_index}")]
    ReferencedPackMissing { pack_index: usize },

    /// Reachability bitmap generation for a MIDX failed.
    #[error("multi-pack-index bitmap write failed: {detail}")]
    BitmapWriteFailed { detail: String },
}

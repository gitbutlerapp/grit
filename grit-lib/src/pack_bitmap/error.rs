//! Errors while opening or reading pack / MIDX reachability bitmaps.

use thiserror::Error;

/// Failure mode when a `.bitmap` sidecar cannot be used.
#[derive(Debug, Error, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum BitmapError {
    /// The file is empty or shorter than a bitmap header.
    #[error("bitmap file is too small")]
    TooSmall,
    /// The `BITM` signature or version is wrong.
    #[error("bitmap header is invalid")]
    InvalidHeader,
    /// Required `BITMAP_OPT_FULL_DAG` is missing.
    #[error("bitmap index missing required full-dag flag")]
    MissingFullDag,
    /// On-disk trailer hash does not match file contents.
    #[error("bitmap file checksum mismatch")]
    FileChecksumMismatch,
    /// Header pack/MIDX checksum does not match the object database.
    #[error("bitmap checksum does not match pack or MIDX")]
    PackChecksumMismatch,
    /// An extension or entry could not be parsed safely.
    #[error("bitmap index is corrupt")]
    Corrupt,
    /// Embedded EWAH payload is invalid.
    #[error("bitmap EWAH data is invalid")]
    InvalidEwah,
    /// I/O while mapping or reading the bitmap file.
    #[error("bitmap I/O error: {0}")]
    Io(String),
}

impl BitmapError {
    pub(crate) fn io(err: std::io::Error) -> Self {
        Self::Io(err.to_string())
    }
}

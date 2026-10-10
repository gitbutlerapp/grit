//! Read Git pack and multi-pack-index reachability bitmaps (`.bitmap` / `BITM` v1).
//!
//! See Git's `Documentation/technical/bitmap-format` for the on-disk layout.

mod error;
mod index;
mod order;
mod writer;

pub use error::BitmapError;
pub use index::{BitmapIndex, BitmapIndexCache, CommitReachabilityBitmap, TypeBitmap};
pub use writer::{PackBitmapWriteError, PackBitmapWriteOptions, PackBitmapWriter};

//! Git objects: ids, kinds, in-memory representations, and the object database stack.
//!
//! Canonical layout groups loose/packed storage, hashing, deltas, and reachability
//! bitmaps under this module. Flat [`crate::odb`] paths remain valid.

mod core;
pub mod handle;

pub use core::*;
pub use handle::Objects;

/// Loose and composite object database ([`crate::odb`]).
pub mod odb {
    //! Loose and composite object database ([`crate::odb`]).
    pub use crate::odb::builder;
    pub use crate::odb::store;
    pub use crate::odb::*;
}

/// Pack files, indexing, and object enumeration ([`crate::pack`]).
pub mod pack {
    //! Canonical re-export of [`crate::pack`].
    pub use crate::pack::*;
}

/// Multi-pack index ([`crate::midx`]).
pub mod midx {
    //! Canonical re-export of [`crate::midx`].
    pub use crate::midx::*;
}

/// Object hashing ([`crate::hash`]).
pub mod hash {
    //! Canonical re-export of [`crate::hash`].
    pub use crate::hash::*;
}

/// Commit-graph files ([`crate::commit_graph_file`], [`crate::commit_graph_write`]).
pub mod commit_graph {
    //! Commit-graph read/write ([`crate::commit_graph_file`], [`crate::commit_graph_write`]).
    pub use crate::commit_graph_file::*;
    pub use crate::commit_graph_write::*;
}

/// Pack and MIDX reachability bitmaps ([`crate::pack_bitmap`], [`crate::bitmap_walk`]).
pub mod bitmap {
    //! Pack/MIDX bitmap decode and walks ([`crate::pack_bitmap`], [`crate::bitmap_walk`]).
    pub use crate::bitmap_walk::*;
    pub use crate::pack_bitmap::{
        BitmapError, BitmapIndex, BitmapIndexCache, CommitReachabilityBitmap, MidxBitmapWriter,
        PackBitmapWriteError, PackBitmapWriteOptions, PackBitmapWriter, TypeBitmap,
    };
}

pub mod delta_encode {
    //! Canonical re-export of [`crate::delta_encode`].
    pub use crate::delta_encode::*;
}
pub mod delta_islands {
    //! Canonical re-export of [`crate::delta_islands`].
    pub use crate::delta_islands::*;
}
pub mod index_pack {
    //! Canonical re-export of [`crate::index_pack`].
    pub use crate::index_pack::*;
}
pub mod pack_name_hash {
    //! Canonical re-export of [`crate::pack_name_hash`].
    pub use crate::pack_name_hash::*;
}
pub mod pack_rev {
    //! Canonical re-export of [`crate::pack_rev`].
    pub use crate::pack_rev::*;
}
pub mod pack_store {
    //! Canonical re-export of [`crate::pack_store`].
    pub use crate::pack_store::*;
}
pub mod unpack_objects {
    //! Canonical re-export of [`crate::unpack_objects`].
    pub use crate::unpack_objects::*;
}
pub mod prune_packed {
    //! Canonical re-export of [`crate::prune_packed`].
    pub use crate::prune_packed::*;
}
pub mod promisor {
    //! Canonical re-export of [`crate::promisor`].
    pub use crate::promisor::*;
}
pub mod promisor_remote {
    //! Canonical re-export of [`crate::promisor_remote`].
    pub use crate::promisor_remote::*;
}
pub mod pack_objects {
    //! Canonical re-export of [`crate::pack_objects`].
    pub use crate::pack_objects::*;
}
pub mod pack_object_select {
    //! Canonical re-export of [`crate::pack_object_select`].
    pub use crate::pack_object_select::*;
}
pub mod pack_receive {
    //! Canonical re-export of [`crate::pack_receive`].
    pub use crate::pack_receive::*;
}

//! Git objects: ids, kinds, in-memory representations, and the object database stack.
//!
//! Canonical layout groups loose/packed storage, hashing, deltas, and reachability
//! bitmaps under this module.
//!
//! Library guide: [mod](https://grit-scm.com/docs/library/objects)
//!
//! ```rust
//! use grit_lib::repo::{InitOptions, Repository};
//! # fn main() -> grit_lib::error::Result<()> {
//! let dir = tempfile::tempdir()?;
//! let _repo = Repository::init(dir.path(), &InitOptions::default())?;
//! let _odb = _repo.odb();
//!
//! # Ok(())
//! # }
//! ```

mod commit_ref;
mod core;
mod extra_headers;
pub mod handle;
mod oid_hashtable;
mod write;

pub(crate) use commit_ref::commit_author_and_message_lenient;
pub use commit_ref::CommitRef;
pub use core::*;
pub use extra_headers::{
    extra_header_values, insert_header_before_signature, iter_extra_headers, next_header_block,
    remove_header_lines, set_extra_header, ExtraHeaderIter,
};
pub use handle::Objects;
pub use oid_hashtable::{
    empty_object_id_hash_set, empty_oid_byte_filter_map, empty_oid_set, oid_hash_map,
    oid_hash_map_from, oid_hash_map_with_capacity, oid_hash_set, oid_hash_set_from,
    oid_hash_set_with_capacity, OidBuildHasher, OidHashMap, OidHashSet, OidHasher,
};
pub use write::{NewCommit, TreeEntryError, WriteTreeOptions};

pub use crate::change_id::{
    change_id, find_commits_by_change_id_prefix, is_change_id_spec_token,
    resolve_commit_by_change_id_prefix, resolved_change_id, set_change_id, ChangeId, ChangeIdError,
    ChangeIdGenerator, ChangeIdMatch, ChangeIdPolicy, OsChangeIdGenerator, ResolvedChangeId,
};

pub mod odb {
    //! Object database backends and loose/packed storage.
    pub use crate::odb::builder;
    pub use crate::odb::store;
    pub use crate::odb::*;
}

pub mod pack {
    //! Pack files, indexes, and packfile I/O.
    pub use crate::pack::*;
    pub use crate::pack_reuse_root::*;
}

pub mod bench_fixture {
    //! Synthetic repository fixtures for benchmarks and tests.
    pub use crate::bench_fixture_root::*;
}
pub mod pack_reuse {
    //! Verbatim pack reuse for upload-pack.
    pub use crate::pack_reuse_root::*;
}
pub mod pack_write {
    //! Pack and index installation helpers.
    pub use crate::pack_write_root::*;
}

pub mod midx {
    //! Multi-pack-index (MIDX) read and write.
    pub use crate::midx::*;
}

pub mod hash {
    //! Object-id hashing, file checksums, and hash algorithm selection.
    pub use crate::hash::*;
}

pub mod commit_graph {
    //! Commit-graph files for reachability and generation numbers.
    pub use crate::commit_graph_file::*;
    pub use crate::commit_graph_write::*;
}

pub mod bitmap {
    //! Pack and commit reachability bitmaps.
    pub use crate::bitmap_walk::*;
    pub use crate::pack_bitmap::{
        BitmapError, BitmapIndex, BitmapIndexCache, CommitReachabilityBitmap, MidxBitmapWriter,
        PackBitmapWriteError, PackBitmapWriteOptions, PackBitmapWriter, TypeBitmap,
    };
}

pub mod delta_encode {
    //! Delta compression encoding for pack streams.
    pub use crate::delta_encode::*;
}
pub mod delta_islands {
    //! Delta island boundaries for pack reuse.
    pub use crate::delta_islands::*;
}
pub mod index_pack {
    //! Index-pack and thin-pack handling.
    pub use crate::index_pack::*;
}
pub mod pack_name_hash {
    //! Name-hash tables for delta base selection.
    pub use crate::pack_name_hash::*;
}
pub mod pack_rev {
    //! Reverse indexes for pack lookups.
    pub use crate::pack_rev::*;
}
pub mod pack_store {
    //! Pack-backed object store implementation.
    pub use crate::pack_store::*;
}
pub mod unpack_objects {
    //! Unpack loose objects from pack streams.
    pub use crate::unpack_objects::*;
}
pub mod prune_packed {
    //! Prune unreachable packed objects.
    pub use crate::prune_packed::*;
}
pub mod promisor {
    //! promisor API (re-exported at this path).
    pub use crate::promisor::*;
    pub use crate::promisor_fetch::{
        DefaultPromisorFetcher, PromisorFetcher, PROMISOR_LAZY_FETCH_CONFIG,
    };
}
pub mod promisor_remote {
    //! Promisor remote configuration for partial clones.
    pub use crate::promisor_remote::*;
}
pub mod pack_objects {
    //! Pack-objects negotiation and encoding.
    pub use crate::pack_objects::*;
}
pub mod pack_object_select {
    //! Object selection heuristics for packing.
    pub use crate::pack_object_select::*;
}
pub mod pack_receive {
    //! Receive-pack side pack ingestion.
    pub use crate::pack_receive::*;
}
pub mod pack_stream_scan {
    //! Count a pack's objects while its bytes are still arriving.
    pub use crate::pack_stream_scan::*;
}

#[cfg(feature = "unstable-bench-helpers")]
#[allow(missing_docs)]
pub mod ewah_bench {
    //! Criterion fixtures for EWAH bitmap hot paths (`benches/ewah.rs` only).
    include!("../ewah_bench_api.rs");
}

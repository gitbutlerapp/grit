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
pub(crate) mod handle;
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

pub mod odb {
    //! Object database backends and loose/packed storage.
    pub use crate::odb::builder::OdbBuilder;
    pub use crate::odb::store;
    pub use crate::odb::store::{ObjectStore, WritableObjectStore};
    pub use crate::odb::{
        ObjectStream, Odb, WriteOptions, BULK_NEW_LOOSE_WRITE, BULK_TRACKED_RESTAGE_LOOSE_WRITE,
    };
}

pub mod pack {
    //! Pack files, indexes, and packfile I/O.
    pub use crate::pack::{
        clear_pack_cache, hash_object_bytes, oid_bytes_to_hex, read_alternates_recursive,
        read_idx_object_ids, read_local_pack_indexes, read_object_from_pack_at_offset,
        read_object_from_packs, read_pack_index, read_pack_index_cached,
        read_packed_delta_dependency, skip_one_pack_object, SingleLocalPackReader,
        write_v2_pack_index, write_v2_pack_index_with_trailer, PackIndex, PackIndexEntryRef,
        DEFAULT_PACK_DEPTH,
    };
    pub use crate::pack_reuse_root::*;
}

pub(crate) mod pack_reuse {
    pub use crate::pack_reuse_root::*;
}

#[allow(unused_imports)]
pub(crate) mod pack_write {
    pub use crate::pack_write_root::*;
}

pub mod midx {
    //! Multi-pack-index (MIDX) read and write.
    pub use crate::midx::{
        clear_pack_midx_state, midx_lookup_pack_and_offset, read_midx_btmp_ranges,
        read_midx_objects, try_read_info_via_midx, try_read_object_via_midx,
        write_multi_pack_index_with_options, MidxBitmapWriteOutcome, WriteMultiPackIndexOptions,
    };
}

pub mod hash {
    //! Object-id hashing, file checksums, and hash algorithm selection.
    pub use crate::hash::{
        hash_object, hash_objects_parallel, par_hash_with, parallel_hash_worthwhile,
        try_par_hash_with, try_par_hash_with_force, verify_trailer, ChecksumHasher, HashingWriter,
        ObjectHasher, ParallelHashError, Parallelism, PAR_HASH_MIN_ITEMS, PAR_HASH_MIN_TOTAL_BYTES,
    };
}

pub mod commit_graph {
    //! Commit-graph files for reachability and generation numbers.
    pub use crate::commit_graph_file::BloomWalkStatsHandle;
}

pub mod bitmap {
    //! Pack and commit reachability bitmaps.
    pub use crate::bitmap_walk::{
        BitmapWalkError, BitmapWalkUnsupported, ReachabilityQuery, ReachableSet,
    };
    pub use crate::pack_bitmap::{
        BitmapError, BitmapIndex, BitmapIndexCache, CommitReachabilityBitmap, MidxBitmapWriter,
        PackBitmapWriteError, PackBitmapWriteOptions, PackBitmapWriter, TypeBitmap,
    };
}

pub mod delta_encode {
    //! Delta compression encoding for pack streams.
    pub use crate::delta_encode::{encode_delta, encode_lcp_delta, DeltaIndex};
}

pub(crate) mod delta_islands {}

pub mod index_pack {
    //! Index-pack and thin-pack handling.
    pub use crate::index_pack::{
        build_pack_index_records, ingest_received_pack, ingest_received_pack_path,
        ingest_received_pack_path_with_progress, install_pack_bytes, install_pack_path,
        install_pack_path_with_progress, IngestPackOptions, IngestedPack, ObjectKindCounts,
    };
}

pub(crate) mod pack_name_hash {}

pub(crate) mod pack_rev {}

pub mod pack_store {
    //! Pack-backed object store implementation.
    pub use crate::pack_store::PackStore;
}

pub mod unpack_objects {
    //! Unpack loose objects from pack streams.
    pub use crate::unpack_objects::{
        apply_delta, apply_delta_into, ingest_pack_into, pack_index_records_with_threads,
        UnpackOptions,
    };
}

pub(crate) mod prune_packed {}

pub mod promisor {
    //! promisor API (re-exported at this path).
    pub use crate::promisor::{repo_treats_promisor_packs, PROMISOR_MISSING_FILE};
    pub use crate::promisor_fetch::{
        DefaultPromisorFetcher, PromisorFetcher, PROMISOR_LAZY_FETCH_CONFIG,
    };
}

pub mod promisor_remote {
    //! Promisor remote configuration for partial clones.
    pub use crate::promisor_remote::{
        configured_promisor_remotes, lazy_fetch_order, PromisorAcceptPolicy, PromisorAdvertisement,
        PromisorConfigWarning, PromisorDecision, PromisorField, PromisorHandshake,
        PromisorRejection, PromisorReplyError,
    };
}

pub mod pack_objects {
    //! Pack-objects negotiation and encoding.
    pub use crate::pack_objects::{
        build_pack, build_pack_from_send_list, build_pack_with_shallow_and_filter,
        write_pack_for_push, write_upload_pack_stream, PackBuildOptions, PackObjects,
        PackObjectsOptions, PackProgress, PackStats, SERVE_WINDOW_MEMORY,
    };
}

pub mod pack_object_select {
    //! Object selection heuristics for packing.
    pub use crate::pack_object_select::{
        enumerate_pack_objects, HaveBoundary, PackEnumerateOptions,
    };
}

#[allow(unused_imports)]
pub(crate) mod pack_receive {
    pub use crate::pack_receive::*;
}

pub(crate) mod pack_stream_scan {
    pub use crate::pack_stream_scan::*;
}

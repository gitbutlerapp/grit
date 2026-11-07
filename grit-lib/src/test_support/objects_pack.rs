//! Pack helpers matching historical `grit_lib::objects::pack` paths used by workspace tests.
//!
//! These re-export [`crate::pack`] / [`crate::pack_index`] items for integration tests. The stable
//! library surface keeps a smaller [`crate::objects::pack`] module; see `public-api.txt`.

pub use crate::pack::{
    apply_delta_depth_limit, clear_pack_cache, clear_pack_delta_base_cache,
    collect_local_pack_info, configure_delta_base_cache_from_config, hash_object_bytes,
    max_verify_pack_delta_depth, oid_bytes_to_hex, packed_delta_base_oid, packed_full_object_slice,
    packed_ref_delta_reuse_slice, read_alternates_recursive, read_idx_object_ids,
    read_local_pack_indexes, read_local_pack_indexes_cached, read_object_from_pack,
    read_object_from_pack_at_offset, read_object_from_pack_into, read_object_from_packs,
    read_pack_bytes_cached, read_pack_index, read_pack_index_cached, read_pack_index_no_verify,
    read_packed_delta_dependency, reprepare_pack_directory_on_miss, skip_one_pack_object,
    slice_one_pack_object, verify_pack_and_collect, write_v2_pack_index,
    write_v2_pack_index_from_trailer, PackIndex, PackIndexEntryRef, PackedDeltaDependency,
    PackedType, SingleLocalPackReader, VerifyObjectRecord, DEFAULT_PACK_DEPTH,
};
pub use crate::pack_index::{
    compute_fanout_from_oid_slices, pack_index_entry_matches_sha1_oid, parse_pack_index_bytes,
    PackIndexEntry,
};

pub use crate::pack::pack_cache::revalidate_stale_pack_bytes;

/// Run only upload-pack's bitmap reuse path for `stream`, returning `None` when reuse is not
/// possible (the full upload path then falls back to pack-objects).
///
/// # Errors
///
/// Bitmap, pack read, or write failures.
pub fn try_write_streaming_pack(
    repo: &crate::repo::Repository,
    stream: &crate::pack_objects::UploadPackStream<'_>,
    progress: &mut dyn crate::progress::ProgressSink,
    out: &mut dyn std::io::Write,
) -> crate::error::Result<Option<crate::pack_objects::PackStats>> {
    stream.write_reused_only(repo, progress, out)
}

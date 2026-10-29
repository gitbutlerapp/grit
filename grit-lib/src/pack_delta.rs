//! Parallel pack serialization for delta plans.
//!
//! [`write_pack_with_deltas`] streams a pack in batches: a parallel preparation stage
//! (delta bytes + deflate per planned entry) and a serial emission stage (headers,
//! OFS vs REF selection, trailer), so only one batch of encoded output is in memory.
//! New deltas are chosen by [`crate::delta_search`].

use std::io::Write;

use flate2::write::ZlibEncoder;
use flate2::Compression;

use crate::config::ConfigSet;
use crate::delta_encode::{encode_prefix_extension_delta, DeltaIndex};
use crate::error::{Error, Result};
use crate::hash::HashingWriter;
use crate::objects::{oid_hash_map, HashAlgo, ObjectId, ObjectKind, OidHashMap};
use crate::odb::Odb;
use crate::pack_objects::{
    encode_ofs_delta_distance, encode_pack_object_header, pack_type_code, PackBuildOptions,
    PackStats, PlannedEntry,
};
use crate::parallel::{ParallelOp, ParallelOptions, ThreadPolicy};
use crate::progress::CancelSignal;

/// Resolve [`ParallelOptions`] for windowed delta search from repo config.
pub(crate) fn pack_delta_search_parallel_options(
    odb: &Odb,
    search_threads: Option<usize>,
) -> Result<ParallelOptions> {
    let cfg = pack_config(odb)?;
    let policy = ThreadPolicy::with_threads_override(search_threads);
    Ok(policy.try_resolve(ParallelOp::PackDeltaSearch, &cfg)?)
}

/// Bytes of a local pack, written to the new pack straight from the shared mapping.
struct PackSlice {
    bytes: std::sync::Arc<crate::pack::PackData>,
    start: usize,
    end: usize,
}

impl PackSlice {
    fn as_slice(&self) -> &[u8] {
        &self.bytes[self.start..self.end]
    }
}

/// Per-entry compressed payload produced by the parallel stage.
enum PreparedBody {
    /// Entire pack object encoding copied verbatim (reused on-disk whole object).
    Raw(PackSlice),
    /// zlib-compressed bytes only (header written in the serial stage).
    Zlib(Vec<u8>),
    /// zlib-compressed delta copied verbatim from a local pack (header written in the
    /// serial stage).
    ReusedZlib(PackSlice),
    /// Whole object streamed from the ODB at emit time (large loose blobs).
    StreamWhole { oid: ObjectId, kind: ObjectKind },
}

impl PreparedBody {
    fn as_slice(&self) -> Option<&[u8]> {
        match self {
            Self::Raw(slice) | Self::ReusedZlib(slice) => Some(slice.as_slice()),
            Self::Zlib(bytes) => Some(bytes),
            Self::StreamWhole { .. } => None,
        }
    }
}

struct PreparedEntry {
    body: PreparedBody,
    /// Uncompressed payload size for pack headers (ignored for [`PreparedBody::Raw`]).
    payload_len: usize,
    /// Set for freshly compressed whole objects.
    whole_kind: Option<ObjectKind>,
    /// Body copied verbatim from an existing local pack.
    reused: bool,
}

/// Resolve [`ParallelOptions`] for pack writing from repo config and an optional override.
pub(crate) fn pack_write_parallel_options(
    odb: &Odb,
    write_threads: Option<usize>,
) -> Result<ParallelOptions> {
    let cfg = pack_config(odb)?;
    let policy = ThreadPolicy::with_threads_override(write_threads);
    Ok(policy.try_resolve(ParallelOp::PackWrite, &cfg)?)
}

/// Repository config for pack settings (`pack.*`), or Git's defaults without a git dir.
pub(crate) fn pack_config(odb: &Odb) -> Result<ConfigSet> {
    Ok(match odb.config_git_dir() {
        Some(git_dir) => {
            let env = crate::environment::Environment::empty();
            ConfigSet::load(&env, Some(git_dir), true)?
        }
        None => ConfigSet::new(),
    })
}

/// Serialize a delta plan into an in-memory PACK v2 stream with matching index rows.
pub(crate) fn serialize_pack_with_deltas(
    odb: &Odb,
    entries: &[PlannedEntry],
    opts: &PackBuildOptions,
    write_threads: Option<usize>,
) -> Result<crate::pack_objects::SerializedDeltaPack> {
    let mut bytes = Vec::new();
    let mut index_rows = Vec::with_capacity(entries.len());
    write_pack_with_deltas(
        odb,
        entries,
        opts,
        write_threads,
        &mut bytes,
        Some(&mut index_rows),
        &CancelSignal::never(),
        None,
    )?;
    Ok(crate::pack_objects::SerializedDeltaPack {
        bytes,
        index_rows,
        staging_path: None,
    })
}

/// Planned entries encoded per parallel batch while streaming a pack, which bounds
/// how much compressed output is held in memory at once.
const WRITE_BATCH_ENTRIES: usize = 256;

/// Stream a delta plan to `w` as a PACK v2 stream, including the trailing checksum.
///
/// Entries are delta-encoded and deflated in parallel batches, then emitted in plan
/// order (choosing `OFS_DELTA` when the base was already written and
/// `opts.use_ofs_delta` is set, `REF_DELTA` otherwise). Payloads are read from `odb`
/// as each batch is prepared; deltas the search already compressed are copied, and
/// the others (thin-pack deltas) are computed here. When `index_rows` is set it
/// receives `(oid, pack offset, crc32)` for each entry in pack order. `stop` is
/// polled before each entry is written.
///
/// # Errors
///
/// Returns object read or encoding errors, [`Error::PackDeltaBaseNotWritten`] when the
/// pack is not thin (`opts.thin` unset) and a delta comes before its base (a base
/// missing from `entries`, or a delta cycle), [`Error::Io`] when writing to `w`
/// fails, and [`Error::Cancelled`] once `stop` fires.
#[allow(clippy::too_many_arguments)]
pub(crate) fn write_pack_with_deltas(
    odb: &Odb,
    entries: &[PlannedEntry],
    opts: &PackBuildOptions,
    write_threads: Option<usize>,
    w: &mut dyn Write,
    index_rows: Option<&mut Vec<(ObjectId, u64, u32)>>,
    stop: &CancelSignal<'_>,
    on_objects: Option<&mut dyn FnMut(u64)>,
) -> Result<PackStats> {
    write_pack_in_batches(
        odb,
        entries,
        opts,
        write_threads,
        w,
        index_rows,
        WRITE_BATCH_ENTRIES,
        stop,
        on_objects,
    )
}

#[allow(clippy::too_many_arguments)]
fn write_pack_in_batches(
    odb: &Odb,
    entries: &[PlannedEntry],
    opts: &PackBuildOptions,
    write_threads: Option<usize>,
    w: &mut dyn Write,
    mut index_rows: Option<&mut Vec<(ObjectId, u64, u32)>>,
    batch_entries: usize,
    stop: &CancelSignal<'_>,
    mut on_objects: Option<&mut dyn FnMut(u64)>,
) -> Result<PackStats> {
    let par_opts = pack_write_parallel_options(odb, write_threads)?;
    let compression = pack_zlib_compression(odb)?;
    let algo = odb.hash_algo();
    let count = u32::try_from(entries.len())
        .map_err(|_| Error::CorruptObject("pack object count exceeds u32".to_owned()))?;
    let delta_count = entries.iter().filter(|e| e.base.is_some()).count() as u32;
    let mut verbatim_reuse_objects = 0u32;
    let mut verbatim_reuse_bytes = 0u64;

    let mut out = HashingWriter::new(algo, w);
    let mut header: Vec<u8> = Vec::with_capacity(12 + algo.len());
    header.extend_from_slice(b"PACK");
    header.extend_from_slice(&2u32.to_be_bytes());
    header.extend_from_slice(&count.to_be_bytes());
    out.write_all(&header).map_err(Error::Io)?;
    let mut offset = header.len() as u64;

    let mut oid_to_offset: OidHashMap<u64> = oid_hash_map();
    let mut emit = |entry: &PlannedEntry, prep: &PreparedEntry| -> Result<()> {
        stop.poll()?;
        header.clear();
        match (entry.base, &prep.body) {
            (None, PreparedBody::Raw(_)) => {}
            (None, PreparedBody::Zlib(_) | PreparedBody::ReusedZlib(_)) => {
                let kind = prep.whole_kind.unwrap_or(entry.kind);
                encode_pack_object_header(&mut header, pack_type_code(kind), prep.payload_len);
            }
            (None, PreparedBody::StreamWhole { .. }) => {}
            (Some(base_oid), _) => {
                let in_pack_offset = oid_to_offset.get(&base_oid).copied();
                if let Some(base_off) = in_pack_offset.filter(|_| opts.use_ofs_delta) {
                    let dist = offset.checked_sub(base_off).ok_or_else(|| {
                        Error::CorruptObject("ofs-delta distance underflow".to_owned())
                    })?;
                    encode_pack_object_header(&mut header, 6, prep.payload_len);
                    encode_ofs_delta_distance(&mut header, dist);
                } else {
                    if !opts.thin && in_pack_offset.is_none() {
                        return Err(Error::PackDeltaBaseNotWritten {
                            target: entry.oid,
                            base: base_oid,
                        });
                    }
                    if base_oid.as_bytes().len() != algo.len() {
                        return Err(Error::CorruptObject(
                            "ref-delta base oid width mismatch".to_owned(),
                        ));
                    }
                    encode_pack_object_header(&mut header, 7, prep.payload_len);
                    header.extend_from_slice(base_oid.as_bytes());
                }
            }
        }
        out.write_all(&header).map_err(Error::Io)?;
        let (body_len, entry_crc) = match &prep.body {
            PreparedBody::StreamWhole { oid, kind, .. } => {
                let mut crc_out = crate::pack_objects::CrcWriter::new(&mut out, &header);
                let n = crate::pack_objects::write_whole_pack_object_to_odb(
                    &mut crc_out,
                    odb,
                    *oid,
                    *kind,
                    opts,
                )?;
                (n, crc_out.finalize())
            }
            body => {
                let bytes = body
                    .as_slice()
                    .ok_or_else(|| Error::CorruptObject("missing prepared pack body".to_owned()))?;
                crate::pack_objects::write_all_chunked(&mut out, bytes)?;
                let mut crc = crc32fast::Hasher::new();
                crc.update(&header);
                for chunk in bytes.chunks(crate::pack_objects::PACK_STREAM_COPY_CHUNK) {
                    crc.update(chunk);
                }
                (bytes.len() as u64, crc.finalize())
            }
        };
        if let Some(rows) = index_rows.as_deref_mut() {
            rows.push((entry.oid, offset, entry_crc));
        }
        if prep.reused {
            verbatim_reuse_objects += 1;
            verbatim_reuse_bytes += body_len;
        }
        oid_to_offset.insert(entry.oid, offset);
        offset += header.len() as u64 + body_len;
        Ok(())
    };
    let prepare = |batch: &[PlannedEntry]| prepare_batch(odb, batch, opts, compression);
    let batches: Vec<&[PlannedEntry]> = entries.chunks(batch_entries.max(1)).collect();
    let workers = par_opts.threads.get();
    if workers > 1 && entries.len() >= MIN_ENTRIES_FOR_PARALLEL_PACK_WRITE && batches.len() > 1 {
        // Long-lived workers keep the odb's pack caches across batches; a pool per
        // batch would reopen every pack index on each new thread.
        crate::parallel::run_ordered_batches(
            &batches,
            1,
            workers,
            || odb.pack_read_context_guard(),
            |_, batch| prepare(batch),
            |done, prepared| {
                for (batch, preps) in done.iter().zip(prepared) {
                    for (entry, prep) in batch.iter().zip(&preps) {
                        emit(entry, prep)?;
                    }
                    if let Some(on) = on_objects.as_deref_mut() {
                        on(batch.len() as u64);
                    }
                }
                Ok(())
            },
        )?;
    } else {
        for batch in batches {
            for (entry, prep) in batch.iter().zip(&prepare(batch)?) {
                emit(entry, prep)?;
            }
            if let Some(on) = on_objects.as_deref_mut() {
                on(batch.len() as u64);
            }
        }
    }
    out.finish().map_err(Error::Io)?;
    Ok(PackStats {
        object_count: count,
        pack_bytes: offset + algo.len() as u64,
        delta_count,
        verbatim_reuse_bytes,
        verbatim_reuse_objects,
        ..PackStats::default()
    })
}

/// Avoid thread-pool overhead on small repacks (typical loose-object gc).
// Parallel pack preparation pays off on large packs; on ~600-object gc repacks the
// thread pool and index sharing often lose to a single-threaded pass.
const MIN_ENTRIES_FOR_PARALLEL_PACK_WRITE: usize = 2048;

/// A delta base read while preparing one batch, with its match index once built.
struct BatchBase {
    data: Vec<u8>,
    index: Option<DeltaIndex>,
}

/// Delta-encode and deflate one batch of planned entries, in order.
///
/// Bases shared by several deltas of the batch are read and indexed once.
fn prepare_batch(
    odb: &Odb,
    entries: &[PlannedEntry],
    opts: &PackBuildOptions,
    compression: Compression,
) -> Result<Vec<PreparedEntry>> {
    let mut bases: OidHashMap<BatchBase> = oid_hash_map();
    entries
        .iter()
        .map(|entry| match entry.base {
            None => prepare_whole(odb, entry, opts, compression),
            Some(base_oid) => prepare_delta(odb, entry, base_oid, &mut bases, compression),
        })
        .collect()
}

fn pack_zlib_compression(odb: &Odb) -> Result<Compression> {
    odb.pack_compression()
}

fn prepare_whole(
    odb: &Odb,
    entry: &PlannedEntry,
    opts: &PackBuildOptions,
    compression: Compression,
) -> Result<PreparedEntry> {
    if opts.reuse_objects && odb.hash_algo() == HashAlgo::Sha1 {
        if let Some((bytes, start, end)) =
            crate::pack::packed_full_object_range(odb.objects_dir(), &entry.oid)?
        {
            return Ok(PreparedEntry {
                body: PreparedBody::Raw(PackSlice { bytes, start, end }),
                payload_len: 0,
                whole_kind: None,
                reused: true,
            });
        }
    }
    let info = odb.read_info(&entry.oid)?;
    if entry.kind == ObjectKind::Blob && info.size >= odb.big_file_threshold()? {
        let payload_len = usize::try_from(info.size).map_err(|_| {
            Error::CorruptObject(format!("object size overflow for {}", entry.oid.to_hex()))
        })?;
        return Ok(PreparedEntry {
            body: PreparedBody::StreamWhole {
                oid: entry.oid,
                kind: entry.kind,
            },
            payload_len,
            whole_kind: Some(entry.kind),
            reused: false,
        });
    }
    let payload = odb.read(&entry.oid)?.data;
    let zlib = zlib_compress(&payload, compression)?;
    Ok(PreparedEntry {
        body: PreparedBody::Zlib(zlib),
        payload_len: payload.len(),
        whole_kind: Some(entry.kind),
        reused: false,
    })
}

fn prepare_delta(
    odb: &Odb,
    entry: &PlannedEntry,
    base_oid: ObjectId,
    bases: &mut OidHashMap<BatchBase>,
    compression: Compression,
) -> Result<PreparedEntry> {
    if let Some((delta_size, pack_bytes, zlib_start, zlib_end)) = &entry.reused_delta_zlib {
        let delta_len = usize::try_from(*delta_size)
            .map_err(|_| Error::CorruptObject("reused delta size exceeds usize".to_owned()))?;
        return Ok(PreparedEntry {
            body: PreparedBody::ReusedZlib(PackSlice {
                bytes: std::sync::Arc::clone(pack_bytes),
                start: *zlib_start,
                end: *zlib_end,
            }),
            payload_len: delta_len,
            whole_kind: None,
            reused: true,
        });
    }

    if let Some((delta_len, zlib)) = &entry.searched_delta {
        return Ok(PreparedEntry {
            body: PreparedBody::Zlib(zlib.clone()),
            payload_len: *delta_len,
            whole_kind: None,
            reused: false,
        });
    }
    let delta = compute_delta(odb, entry.oid, base_oid, bases)?;
    let zlib = zlib_compress(&delta, compression)?;
    Ok(PreparedEntry {
        body: PreparedBody::Zlib(zlib),
        payload_len: delta.len(),
        whole_kind: None,
        reused: false,
    })
}

/// Encode the delta from `base_oid` to `target_oid`.
fn compute_delta(
    odb: &Odb,
    target_oid: ObjectId,
    base_oid: ObjectId,
    bases: &mut OidHashMap<BatchBase>,
) -> Result<Vec<u8>> {
    let target = odb.read(&target_oid)?.data;
    let base = match bases.entry(base_oid) {
        hashbrown::hash_map::Entry::Occupied(e) => e.into_mut(),
        hashbrown::hash_map::Entry::Vacant(e) => e.insert(BatchBase {
            data: odb.read(&base_oid)?.data,
            index: None,
        }),
    };
    if target.starts_with(&base.data) && target.len() > base.data.len() {
        return encode_prefix_extension_delta(&base.data, &target);
    }
    let data = &base.data;
    base.index
        .get_or_insert_with(|| DeltaIndex::new(data))
        .encode(&target, 0)
        .ok_or_else(|| Error::CorruptObject("failed to encode pack delta for object".into()))
}

fn zlib_compress(data: &[u8], compression: Compression) -> Result<Vec<u8>> {
    let mut enc = ZlibEncoder::new(Vec::with_capacity(data.len() / 3 + 64), compression);
    enc.write_all(data).map_err(Error::Io)?;
    enc.finish().map_err(Error::Io)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch_odb() -> (tempfile::TempDir, Odb) {
        let dir = tempfile::tempdir().expect("tempdir");
        let objects = dir.path().join("objects");
        std::fs::create_dir_all(&objects).expect("objects");
        let odb = Odb::new(&objects);
        (dir, odb)
    }

    fn whole_entry(odb: &Odb, data: &[u8]) -> PlannedEntry {
        PlannedEntry {
            oid: odb.write(ObjectKind::Blob, data).unwrap(),
            kind: ObjectKind::Blob,
            base: None,
            searched_delta: None,
            reused_delta_zlib: None,
        }
    }

    #[test]
    fn single_thread_prepare_matches_entry_count() {
        let (_dir, odb) = scratch_odb();
        let entry = whole_entry(&odb, b"hello");
        let opts = PackBuildOptions::default();
        let got = prepare_batch(&odb, &[entry], &opts, Compression::default()).expect("prepare");
        assert_eq!(got.len(), 1);
    }

    #[test]
    fn writer_recomputes_deltas_the_search_did_not_keep() {
        let (_dir, odb) = scratch_odb();
        let mut base_data = vec![b'q'; 8192];
        let base = whole_entry(&odb, &base_data);
        base_data[4000] = b'!';
        let mut target = whole_entry(&odb, &base_data);
        target.base = Some(base.oid);
        let opts = PackBuildOptions::default();
        let prepared = prepare_batch(&odb, &[base, target], &opts, Compression::default()).unwrap();
        let PreparedBody::Zlib(zlib) = &prepared[1].body else {
            panic!("delta body is zlib");
        };
        let mut delta = Vec::new();
        std::io::Read::read_to_end(&mut flate2::read::ZlibDecoder::new(&zlib[..]), &mut delta)
            .unwrap();
        assert_eq!(delta.len(), prepared[1].payload_len);
        assert!(
            delta.len() < 64,
            "recomputed delta is small: {}",
            delta.len()
        );
    }

    /// Write `entries` as a pack that is thin or not, discarding the bytes.
    fn write_plan(odb: &Odb, entries: &[PlannedEntry], thin: bool) -> Result<PackStats> {
        let opts = PackBuildOptions {
            thin,
            ..PackBuildOptions::default()
        };
        write_pack_with_deltas(
            odb,
            entries,
            &opts,
            Some(1),
            &mut Vec::new(),
            None,
            &CancelSignal::never(),
            None,
        )
    }

    /// Two blobs that differ in one byte.
    fn similar_pair(odb: &Odb, fill: u8) -> (PlannedEntry, PlannedEntry) {
        let mut data = vec![fill; 4096];
        let first = whole_entry(odb, &data);
        data[100] = b'!';
        (first, whole_entry(odb, &data))
    }

    #[test]
    fn full_pack_refuses_a_delta_whose_base_it_does_not_contain() {
        let (_dir, odb) = scratch_odb();
        let (absent, mut target) = similar_pair(&odb, b'r');
        target.base = Some(absent.oid);
        let entries = [target];
        match write_plan(&odb, &entries, false) {
            Err(Error::PackDeltaBaseNotWritten { target, base }) => {
                assert_eq!((target, base), (entries[0].oid, absent.oid));
            }
            other => panic!("expected PackDeltaBaseNotWritten, got {other:?}"),
        }
        assert!(
            write_plan(&odb, &entries, true).is_ok(),
            "a thin pack leaves the base to the peer"
        );
    }

    #[test]
    fn full_pack_refuses_a_delta_cycle() {
        let (_dir, odb) = scratch_odb();
        let (mut a, mut b) = similar_pair(&odb, b's');
        a.base = Some(b.oid);
        b.base = Some(a.oid);
        let (a_oid, b_oid) = (a.oid, b.oid);
        match write_plan(&odb, &[a, b], false) {
            Err(Error::PackDeltaBaseNotWritten { target, base }) => {
                assert_eq!((target, base), (a_oid, b_oid));
            }
            other => panic!("expected PackDeltaBaseNotWritten, got {other:?}"),
        }
    }

    #[test]
    fn a_fired_cancel_stops_the_writer_before_any_entry() {
        let (_dir, odb) = scratch_odb();
        let entries: Vec<PlannedEntry> = (0..3000u32)
            .map(|i| whole_entry(&odb, format!("blob number {i}\n").as_bytes()))
            .collect();
        let cancel = crate::progress::FlagCancel::new();
        cancel.cancel();
        for threads in [1, 4] {
            let mut out = Vec::new();
            let err = write_pack_with_deltas(
                &odb,
                &entries,
                &PackBuildOptions::default(),
                Some(threads),
                &mut out,
                None,
                &CancelSignal::new(&cancel),
                None,
            )
            .unwrap_err();
            assert!(matches!(err, Error::Cancelled), "{err:?}");
            assert_eq!(out.len(), 12, "only the pack header was written");
        }
    }

    #[test]
    fn full_pack_writes_a_delta_after_its_base() {
        let (_dir, odb) = scratch_odb();
        let (base, mut target) = similar_pair(&odb, b't');
        target.base = Some(base.oid);
        let stats = write_plan(&odb, &[base, target], false).expect("base first");
        assert_eq!((stats.object_count, stats.delta_count), (2, 1));
    }

    #[test]
    fn streamed_pack_is_identical_across_worker_counts() {
        let (_dir, odb) = scratch_odb();
        let entries: Vec<PlannedEntry> = (0..3000u32)
            .map(|i| whole_entry(&odb, format!("blob number {i}\n").as_bytes()))
            .collect();
        let opts = PackBuildOptions::default();
        let write = |threads: usize| {
            let mut out = Vec::new();
            let mut rows = Vec::new();
            let stats = write_pack_with_deltas(
                &odb,
                &entries,
                &opts,
                Some(threads),
                &mut out,
                Some(&mut rows),
                &CancelSignal::never(),
                None,
            )
            .unwrap();
            (out, stats, rows)
        };
        let (serial, serial_stats, serial_rows) = write(1);
        let (parallel, parallel_stats, parallel_rows) = write(4);
        assert_eq!(serial, parallel);
        assert_eq!(serial_stats, parallel_stats);
        assert_eq!(serial_rows, parallel_rows);
        assert_eq!(serial_stats.object_count, 3000);
        assert_eq!(serial_stats.pack_bytes, serial.len() as u64);
        let (body, trailer) = serial.split_at(serial.len() - 20);
        assert_eq!(HashAlgo::Sha1.checksum_digest(body).as_slice(), trailer);
    }

    #[cfg_attr(
        not(feature = "full-tests"),
        ignore = "starts system git or is heavy; run with --features full-tests"
    )]
    #[test]
    fn batched_pack_stream_matches_one_batch_and_git_indexes_it() {
        let (tmp, repo, selection) = crate::pack_objects::tests::packed_selection(60);
        let odb = repo.odb();
        // A fresh-delta plan (no reuse) exercises delta indices across batch boundaries.
        let opts = PackBuildOptions {
            delta: true,
            reuse_deltas: false,
            reuse_objects: false,
            ..PackBuildOptions::default()
        };
        let plan = crate::pack_objects::plan_deltas(
            odb,
            &selection.send,
            &selection.have_closure,
            selection.peer_held.as_deref(),
            &opts,
            &selection.name_hashes,
            None,
            &CancelSignal::never(),
        )
        .unwrap();
        assert!(plan.entries.iter().any(|e| e.base.is_some()));

        let write = |batch: usize| {
            let mut bytes = Vec::new();
            let mut rows = Vec::new();
            let stats = write_pack_in_batches(
                odb,
                &plan.entries,
                &opts,
                Some(2),
                &mut bytes,
                Some(&mut rows),
                batch,
                &CancelSignal::never(),
                None,
            )
            .unwrap();
            (bytes, rows, stats)
        };
        let (one, one_rows, one_stats) = write(usize::MAX);
        let (batched, batched_rows, batched_stats) = write(7);
        assert_eq!(one, batched);
        assert_eq!(one_rows, batched_rows);
        assert_eq!(one_stats, batched_stats);
        assert_eq!(batched_stats.pack_bytes, batched.len() as u64);
        assert_eq!(batched_stats.object_count as usize, plan.entries.len());

        let trailer_start = batched.len() - odb.hash_algo().len();
        let mut ends: Vec<u64> = batched_rows.iter().skip(1).map(|r| r.1).collect();
        ends.push(trailer_start as u64);
        for ((oid, start, crc), end) in batched_rows.iter().zip(ends) {
            let slice = &batched[*start as usize..end as usize];
            assert_eq!(crc32fast::hash(slice), *crc, "crc of {oid}");
        }

        let pack = tmp.path().join("streamed.pack");
        std::fs::write(&pack, &batched).unwrap();
        grit_test_support::git(tmp.path(), &["index-pack", "--strict", "streamed.pack"]);
        let listed = grit_test_support::git(tmp.path(), &["verify-pack", "-v", "streamed.idx"]);
        assert!(listed.contains(&selection.send[0].to_hex()));
    }
}

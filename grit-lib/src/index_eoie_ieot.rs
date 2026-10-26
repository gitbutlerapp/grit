//! EOIE / IEOT index extensions for parallel index load (Git `read-cache.c` behavior).

use std::collections::BTreeMap;
use std::num::NonZeroUsize;

use crate::config::ConfigSet;
use crate::error::{Error, Result};
use crate::index::{
    parse_fsmonitor_extension, CacheTreeNode, IndexEntry, INDEX_EXT_CACHE_TREE,
    INDEX_EXT_FSMONITOR, INDEX_EXT_LINK, INDEX_EXT_RESOLVE_UNDO, INDEX_EXT_SPARSE_DIRECTORIES,
    INDEX_EXT_UNTRACKED,
};
use crate::objects::HashAlgo;
use crate::objects::ObjectId;
use crate::parallel::{run_ordered_pool, ThreadPolicy};
use crate::resolve_undo::ResolveUndoRecord;
use crate::split_index;
use crate::untracked_cache;

/// Git `CACHE_EXT_ENDOFINDEXENTRIES` (`"EOIE"`).
pub(crate) const INDEX_EXT_EOIE: u32 = u32::from_be_bytes(*b"EOIE");
/// Git `CACHE_EXT_INDEXENTRYOFFSETTABLE` (`"IEOT"`).
pub(crate) const INDEX_EXT_IEOT: u32 = u32::from_be_bytes(*b"IEOT");

const IEOT_VERSION: u32 = 1;
/// Git `THREAD_COST` in `read-cache.c` (index entry blocks for parallel load).
pub(crate) const INDEX_LOAD_THREAD_COST: usize = 10_000;

const EOIE_FIXED_PAYLOAD: usize = 4; // offset only; hash width is per algo

#[cfg(test)]
pub(crate) static PARALLEL_IEOT_ENTRY_LOADS: std::sync::atomic::AtomicUsize =
    std::sync::atomic::AtomicUsize::new(0);

#[derive(Debug, Clone)]
pub(crate) struct IeotEntry {
    pub offset: u32,
    pub nr: u32,
}

#[derive(Debug, Clone)]
pub(crate) struct IeotTable {
    pub entries: Vec<IeotEntry>,
}

/// Parsed index extensions (everything after the entry array).
#[derive(Debug, Default)]
pub(crate) struct ParsedIndexExtensions {
    pub sparse_directories: bool,
    pub untracked_cache: Option<untracked_cache::UntrackedCache>,
    pub fsmonitor_last_update: Option<String>,
    pub fsmonitor_dirty: Option<crate::ewah_bitmap::EwahBitmap>,
    pub fsmonitor_raw_extension: Option<Vec<u8>>,
    pub resolve_undo: Option<BTreeMap<Vec<u8>, ResolveUndoRecord>>,
    pub split_link: Option<split_index::SplitIndexLink>,
    pub cache_tree_root: Option<ObjectId>,
    pub cache_tree: Option<CacheTreeNode>,
}

/// Context for index parse/load threading and config.
#[derive(Debug, Clone, Copy, Default)]
pub struct IndexParseContext {
    /// `thread_policy` field.
    pub thread_policy: ThreadPolicy,
}

impl IndexParseContext {
    /// Build from repository policy (config is read separately at parse time).
    #[must_use]
    pub fn from_policy(policy: ThreadPolicy) -> Self {
        Self {
            thread_policy: policy,
        }
    }
}

#[must_use]
pub(crate) fn record_end_of_index_entries(config: Option<&ConfigSet>) -> bool {
    let Some(cfg) = config else {
        return false;
    };
    if let Some(Ok(v)) = cfg.get_bool("index.recordEndOfIndexEntries") {
        return v;
    }
    if let Some(Ok(v)) = cfg.get_bool("index.recordendofindexentries") {
        return v;
    }
    index_threads_explicitly_non_serial(cfg)
}

#[must_use]
pub(crate) fn record_offset_table(config: Option<&ConfigSet>) -> bool {
    let Some(cfg) = config else {
        return false;
    };
    if let Some(Ok(v)) = cfg.get_bool("index.recordOffsetTable") {
        return v;
    }
    if let Some(Ok(v)) = cfg.get_bool("index.recordoffsettable") {
        return v;
    }
    index_threads_explicitly_non_serial(cfg)
}

#[must_use]
fn index_threads_explicitly_non_serial(config: &ConfigSet) -> bool {
    matches!(configured_index_threads_value(config), Some(v) if v != 1)
}

fn configured_index_threads_value(config: &ConfigSet) -> Option<i64> {
    if let Some(Ok(n)) = config.get_i64("index.threads") {
        return Some(n);
    }
    if let Some(result) = config.get_bool("index.threads") {
        return result.ok().map(|b| if b { 0 } else { 1 });
    }
    None
}

/// Git `do_read_index` worker count from `index.threads` (unset → 1).
#[must_use]
pub(crate) fn index_load_worker_count(
    policy: ThreadPolicy,
    config: Option<&ConfigSet>,
    entry_count: usize,
) -> usize {
    if entry_count == 0 {
        return 1;
    }
    if let Some(n) = policy.threads_override {
        return clamp_load_workers(n, entry_count);
    }
    let Some(cfg) = config else {
        return 1;
    };
    match configured_index_threads_value(cfg) {
        None => 1,
        Some(1) => 1,
        Some(0) => auto_load_workers(entry_count),
        Some(n) if n > 1 => clamp_load_workers(n as usize, entry_count),
        _ => 1,
    }
}

fn auto_load_workers(entry_count: usize) -> usize {
    let cpus = std::thread::available_parallelism()
        .map(NonZeroUsize::get)
        .unwrap_or(1);
    let mut nr = entry_count / INDEX_LOAD_THREAD_COST;
    if nr > cpus {
        nr = cpus;
    }
    if nr <= 1 {
        1
    } else {
        nr.min(entry_count)
    }
}

fn clamp_load_workers(requested: usize, entry_count: usize) -> usize {
    requested.max(1).min(entry_count.max(1))
}

/// Scan backwards for a valid EOIE extension; returns byte offset of first extension after entries.
///
/// `body` is the index file without the trailing object-format checksum.
pub(crate) fn read_eoie_extension(body: &[u8], hash_algo: HashAlgo) -> Option<usize> {
    let hash_len = hash_algo.len();
    let eoie_payload = EOIE_FIXED_PAYLOAD + hash_len;
    let eoie_with_header = 8 + eoie_payload;
    if body.len() < 12 + eoie_with_header {
        return None;
    }
    let eoie_start = body.len().checked_sub(eoie_with_header)?;
    if u32::from_be_bytes(body[eoie_start..eoie_start + 4].try_into().ok()?) != INDEX_EXT_EOIE {
        return None;
    }
    let extsize =
        u32::from_be_bytes(body[eoie_start + 4..eoie_start + 8].try_into().ok()?) as usize;
    if extsize != eoie_payload {
        return None;
    }
    let offset =
        u32::from_be_bytes(body[eoie_start + 8..eoie_start + 12].try_into().ok()?) as usize;
    if offset < 12 || offset >= eoie_start {
        return None;
    }
    let stored_hash = &body[eoie_start + 12..eoie_start + 12 + hash_len];
    let computed = hash_extension_headers(body, offset, eoie_start, hash_algo)?;
    if computed.as_slice() != stored_hash {
        return None;
    }
    let mut pos = offset;
    while pos < eoie_start {
        if pos + 8 > eoie_start {
            return None;
        }
        let ext_sz = u32::from_be_bytes(body[pos + 4..pos + 8].try_into().ok()?) as usize;
        pos = pos.checked_add(8)?.checked_add(ext_sz)?;
    }
    if pos != eoie_start {
        return None;
    }
    Some(offset)
}

fn hash_extension_headers(
    body: &[u8],
    start: usize,
    end: usize,
    hash_algo: HashAlgo,
) -> Option<Vec<u8>> {
    let mut pos = start;
    let mut data = Vec::new();
    while pos < end {
        if pos + 8 > end {
            return None;
        }
        data.extend_from_slice(&body[pos..pos + 8]);
        let ext_sz = u32::from_be_bytes(body[pos + 4..pos + 8].try_into().ok()?) as usize;
        pos = pos.checked_add(8)?.checked_add(ext_sz)?;
    }
    Some(hash_algo.checksum_digest(&data))
}

pub(crate) fn read_ieot_extension(body: &[u8], mut offset: usize, end: usize) -> Option<IeotTable> {
    while offset + 8 <= end {
        let sig = u32::from_be_bytes(body[offset..offset + 4].try_into().ok()?);
        let extsize = u32::from_be_bytes(body[offset + 4..offset + 8].try_into().ok()?) as usize;
        if sig == INDEX_EXT_IEOT {
            let payload = body.get(offset + 8..offset + 8 + extsize)?;
            return parse_ieot_payload(payload);
        }
        offset = offset.checked_add(8)?.checked_add(extsize)?;
    }
    None
}

fn parse_ieot_payload(payload: &[u8]) -> Option<IeotTable> {
    if payload.len() < 4 {
        return None;
    }
    let version = u32::from_be_bytes(payload[0..4].try_into().ok()?);
    if version != IEOT_VERSION {
        return None;
    }
    let rest = &payload[4..];
    if !rest.len().is_multiple_of(8) {
        return None;
    }
    let nr = rest.len() / 8;
    if nr == 0 {
        return None;
    }
    let mut entries = Vec::with_capacity(nr);
    for chunk in rest.as_chunks::<8>().0 {
        let offset = u32::from_be_bytes(chunk[0..4].try_into().ok()?);
        let count = u32::from_be_bytes(chunk[4..8].try_into().ok()?);
        entries.push(IeotEntry { offset, nr: count });
    }
    Some(IeotTable { entries })
}

pub(crate) fn parse_index_extensions(
    body: &[u8],
    mut pos: usize,
    end: usize,
    hash_algo: HashAlgo,
    entries_for_fsmonitor: Option<&mut [IndexEntry]>,
) -> Result<ParsedIndexExtensions> {
    let mut out = ParsedIndexExtensions::default();
    let hash_len = hash_algo.len();
    while pos + 8 <= end {
        let sig = u32::from_be_bytes(
            body[pos..pos + 4]
                .try_into()
                .map_err(|_| Error::IndexError("truncated extension sig".to_owned()))?,
        );
        let ext_sz = u32::from_be_bytes(
            body[pos + 4..pos + 8]
                .try_into()
                .map_err(|_| Error::IndexError("truncated extension size".to_owned()))?,
        ) as usize;
        pos += 8;
        if pos + ext_sz > end {
            return Err(Error::IndexError(
                "extension overruns index body".to_owned(),
            ));
        }
        match sig {
            INDEX_EXT_EOIE | INDEX_EXT_IEOT => {}
            INDEX_EXT_SPARSE_DIRECTORIES => out.sparse_directories = true,
            INDEX_EXT_UNTRACKED => {
                let ext_data = &body[pos..pos + ext_sz];
                out.untracked_cache = untracked_cache::parse_untracked_extension(ext_data);
            }
            INDEX_EXT_FSMONITOR => {
                let ext_data = &body[pos..pos + ext_sz];
                let (token, dirty) = parse_fsmonitor_extension(ext_data)?;
                out.fsmonitor_last_update = Some(token);
                out.fsmonitor_dirty = Some(dirty);
                out.fsmonitor_raw_extension = Some(ext_data.to_vec());
            }
            INDEX_EXT_RESOLVE_UNDO => {
                let ext_data = &body[pos..pos + ext_sz];
                out.resolve_undo = Some(crate::resolve_undo::parse_resolve_undo_payload(ext_data)?);
            }
            INDEX_EXT_LINK => {
                let ext_data = &body[pos..pos + ext_sz];
                out.split_link = Some(split_index::parse_link_extension(ext_data, hash_algo)?);
            }
            INDEX_EXT_CACHE_TREE => {
                let ext_data = &body[pos..pos + ext_sz];
                out.cache_tree = crate::index::parse_cache_tree(ext_data, hash_len);
                out.cache_tree_root = out
                    .cache_tree
                    .as_ref()
                    .and_then(|node| node.oid.filter(|_| node.entry_count >= 0));
            }
            other => {
                let first = (other >> 24) as u8 as char;
                if !first.is_ascii_uppercase() {
                    return Err(Error::IndexError(format!(
                        "index uses unsupported extension {other:#010x}"
                    )));
                }
            }
        }
        pos += ext_sz;
    }
    if pos != end {
        return Err(Error::IndexError("junk after index extensions".to_owned()));
    }
    if let (Some(entries), Some(dirty)) = (entries_for_fsmonitor, out.fsmonitor_dirty.as_ref()) {
        crate::index::apply_fsmonitor_dirty_to_entries(entries, dirty);
    }
    Ok(out)
}

pub(crate) fn parse_entries_serial(
    body: &[u8],
    version: u32,
    count: u32,
    hash_len: usize,
    start: usize,
) -> Result<(Vec<IndexEntry>, usize)> {
    let mut pos = start;
    let mut entries = Vec::with_capacity(count as usize);
    for _ in 0..count {
        let prev_path: &[u8] = if version == 4 {
            entries.last().map_or(&[][..], |e: &IndexEntry| &e.path)
        } else {
            &[]
        };
        let (entry, consumed) =
            crate::index::parse_entry(&body[pos..], version, prev_path, hash_len)?;
        entries.push(entry);
        pos += consumed;
    }
    Ok((entries, pos))
}

fn parse_ieot_block_range(
    body: &[u8],
    version: u32,
    hash_len: usize,
    ieot: &IeotTable,
    block_start: usize,
    block_end: usize,
) -> Result<Vec<IndexEntry>> {
    let nr: usize = ieot.entries[block_start..block_end]
        .iter()
        .map(|b| b.nr as usize)
        .sum();
    let mut local = Vec::with_capacity(nr);
    for i in block_start..block_end {
        let block = &ieot.entries[i];
        let mut pos = block.offset as usize;
        if pos > body.len() {
            return Err(Error::IndexError(format!(
                "IEOT block {i} offset {pos} past index body"
            )));
        }
        let mut prev: Vec<u8> = Vec::new();
        for _ in 0..block.nr {
            if pos >= body.len() {
                return Err(Error::IndexError(format!(
                    "IEOT block {i} entry overruns index body at {pos}"
                )));
            }
            let prev_path: &[u8] = if version == 4 { &prev } else { &[] };
            let (entry, consumed) =
                crate::index::parse_entry(&body[pos..], version, prev_path, hash_len)?;
            pos += consumed;
            if version == 4 {
                prev.clear();
                prev.extend_from_slice(&entry.path);
            }
            local.push(entry);
        }
    }
    Ok(local)
}

pub(crate) fn parse_entries_parallel(
    body: &[u8],
    version: u32,
    count: u32,
    hash_len: usize,
    ieot: &IeotTable,
    nr_threads: usize,
) -> Result<Vec<IndexEntry>> {
    let n_blocks = ieot.entries.len();
    if n_blocks == 0 {
        return Err(Error::IndexError("empty IEOT table".to_owned()));
    }
    let nr_threads = nr_threads.max(1).min(n_blocks);
    #[cfg(test)]
    if nr_threads > 1 {
        PARALLEL_IEOT_ENTRY_LOADS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    }

    let block_indices: Vec<usize> = (0..n_blocks).collect();
    // One IEOT block per pool task (chunk=1); the default map chunk size would let a
    // single worker claim every block and defeat parallelism on small IEOT tables.
    let blocks = run_ordered_pool(&block_indices, nr_threads, 1, |&i| {
        parse_ieot_block_range(body, version, hash_len, ieot, i, i + 1)
    })?;
    let mut entries = Vec::with_capacity(count as usize);
    for part in blocks {
        entries.extend(part);
    }
    if entries.len() != count as usize {
        return Err(Error::IndexError(format!(
            "IEOT entry count mismatch: expected {count}, got {}",
            entries.len()
        )));
    }
    Ok(entries)
}

/// IEOT block count for index write (Git `write_index` / `record_ieot`).
#[must_use]
pub(crate) fn ieot_block_count_for_write(
    policy: ThreadPolicy,
    config: Option<&ConfigSet>,
    entry_count: usize,
) -> Option<usize> {
    if entry_count == 0 || !record_offset_table(config) {
        return None;
    }
    let cfg = config?;
    let nr_threads = index_load_worker_count(policy, Some(cfg), entry_count);
    if nr_threads == 1 {
        return None;
    }
    let mut ieot_blocks = if configured_index_threads_value(cfg) == Some(0) {
        let cpus = std::thread::available_parallelism()
            .map(NonZeroUsize::get)
            .unwrap_or(1);
        let mut blocks = entry_count / INDEX_LOAD_THREAD_COST;
        if blocks > cpus.saturating_sub(1) {
            blocks = cpus.saturating_sub(1);
        }
        blocks
    } else {
        nr_threads
    };
    if ieot_blocks > entry_count {
        ieot_blocks = entry_count;
    }
    if ieot_blocks <= 1 {
        None
    } else {
        Some(ieot_blocks)
    }
}

pub(crate) fn entries_per_ieot_block(entry_count: usize, ieot_blocks: usize) -> usize {
    entry_count.div_ceil(ieot_blocks)
}

pub(crate) fn write_ieot_payload(ieot: &IeotTable, out: &mut Vec<u8>) {
    out.extend_from_slice(&IEOT_VERSION.to_be_bytes());
    for e in &ieot.entries {
        out.extend_from_slice(&e.offset.to_be_bytes());
        out.extend_from_slice(&e.nr.to_be_bytes());
    }
}

#[must_use]
pub(crate) fn preload_index_enabled(config: Option<&ConfigSet>) -> bool {
    let Some(cfg) = config else {
        return true;
    };
    cfg.get_bool("core.preloadindex")
        .or_else(|| cfg.get_bool("core.preloadIndex"))
        .and_then(|r| r.ok())
        .unwrap_or(true)
}

/// Whether parallel IEOT load is allowed for this parse (Git: threads + preload + IEOT present).
#[must_use]
pub(crate) fn parallel_index_load_allowed(
    policy: ThreadPolicy,
    config: Option<&ConfigSet>,
    entry_count: usize,
) -> bool {
    if !preload_index_enabled(config) {
        return false;
    }
    if entry_count < 2 * INDEX_LOAD_THREAD_COST {
        return false;
    }
    let workers = index_load_worker_count(policy, config, entry_count);
    workers > 1
}

#[cfg(test)]
mod tests {
    use std::process::Command;
    use std::sync::atomic::Ordering;

    use super::*;
    use grit_test_support::heavy_disk_serial_guard;

    /// Minimum entry count for parallel IEOT load (`2 * INDEX_LOAD_THREAD_COST`).
    const LARGE_INDEX_ENTRIES: usize = 2 * INDEX_LOAD_THREAD_COST;
    use crate::config::ConfigSet;
    use crate::environment::Environment;
    use crate::index::{Index, IndexParseContext};
    use crate::objects::HashAlgo;
    use crate::parallel::ThreadPolicy;

    fn git_index_with_eoie(entry_count: usize) -> (tempfile::TempDir, Vec<u8>, ConfigSet) {
        let tmp = tempfile::tempdir().expect("tempdir");
        let root = tmp.path();
        for i in 0..entry_count {
            let dir = format!("d{:04}", i / 100);
            std::fs::create_dir_all(root.join(&dir)).expect("mkdir");
            std::fs::write(root.join(format!("{dir}/f{i:05}.txt")), format!("x{i}\n"))
                .expect("write");
        }
        assert!(Command::new("git")
            .args(["init", root.to_str().expect("p")])
            .status()
            .expect("git init")
            .success());
        for (k, v) in [
            ("index.threads", "4"),
            ("index.recordOffsetTable", "true"),
            ("index.recordEndOfIndexEntries", "true"),
        ] {
            assert!(Command::new("git")
                .args(["-C", root.to_str().expect("p"), "config", k, v])
                .status()
                .expect("git config")
                .success());
        }
        assert!(Command::new("git")
            .args(["-C", root.to_str().expect("p"), "add", "-A"])
            .status()
            .expect("git add")
            .success());
        let bytes = std::fs::read(root.join(".git/index")).expect("read index");
        let cfg =
            ConfigSet::load(&Environment::empty(), Some(&root.join(".git")), true).expect("config");
        (tmp, bytes, cfg)
    }

    #[cfg_attr(
        not(feature = "full-tests"),
        ignore = "starts system git or is heavy; run with --features full-tests"
    )]
    #[test]
    fn read_eoie_finds_git_written_trailer() {
        let _disk = heavy_disk_serial_guard();
        let (_tmp, bytes, _cfg) = git_index_with_eoie(LARGE_INDEX_ENTRIES);
        let algo = HashAlgo::Sha1;
        let body = &bytes[..bytes.len() - algo.len()];
        let off = read_eoie_extension(body, algo).expect("valid git EOIE");
        assert!(off >= 12);
    }

    #[cfg_attr(
        not(feature = "full-tests"),
        ignore = "starts system git or is heavy; run with --features full-tests"
    )]
    #[test]
    fn parallel_ieot_entry_load_runs_for_large_git_index() {
        let _disk = heavy_disk_serial_guard();
        let (_tmp, bytes, cfg) = git_index_with_eoie(LARGE_INDEX_ENTRIES);
        let algo = HashAlgo::Sha1;
        let body = &bytes[..bytes.len() - algo.len()];
        let ext_off = read_eoie_extension(body, algo).expect("EOIE");
        assert!(read_ieot_extension(body, ext_off, body.len()).is_some());
        PARALLEL_IEOT_ENTRY_LOADS.store(0, Ordering::Relaxed);
        let ctx = IndexParseContext::from_policy(ThreadPolicy::with_threads_override(Some(4)));
        Index::parse_with_algo_and_config(&bytes, algo, Some(&cfg), ctx).expect("parse");
        assert!(
            PARALLEL_IEOT_ENTRY_LOADS.load(Ordering::Relaxed) > 0,
            "IEOT parallel entry load must run when EOIE/IEOT are present"
        );
    }
}

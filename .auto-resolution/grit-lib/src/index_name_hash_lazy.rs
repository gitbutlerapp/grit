//! Lazy, optionally multi-threaded index name/dir hash initialization compatible with Git's
//! `name-hash.c` (used by `test-tool lazy-init-name-hash` and regression test t3008).
//!
//! This implements Git-compatible `memihash` hashing and the `LAZY_THREAD_COST` thresholds, plus the recursive directory
//! scan used when `core.ignorecase` is effectively enabled for the test helper.

use std::collections::HashMap;
use std::io::{self, Write};
use std::sync::{Arc, Mutex};

use crate::index::{Index, IndexEntry};

const FNV32_BASE: u32 = 0x811c9dc5;
const FNV32_PRIME: u32 = 0x0100_0193;
/// Matches Git `name-hash.c` (`LAZY_THREAD_COST`).
pub(crate) const LAZY_THREAD_COST: usize = 2000;
/// Initial hash table size in Git's `hashmap.c` (`HASHMAP_INITIAL_SIZE`).
const HASHMAP_INITIAL_SIZE: usize = 64;

#[inline]
fn fold_byte_case_insensitive(c: u8) -> u32 {
    u32::from(c.to_ascii_lowercase())
}

/// Git `memihash`: case-insensitive FNV-1a over bytes.
#[must_use]
pub fn memihash(buf: &[u8]) -> u32 {
    let mut hash = FNV32_BASE;
    for &b in buf {
        hash = (hash.wrapping_mul(FNV32_PRIME)) ^ fold_byte_case_insensitive(b);
    }
    hash
}

/// Git `memihash_cont`.
#[must_use]
pub fn memihash_cont(hash_seed: u32, buf: &[u8]) -> u32 {
    let mut hash = hash_seed;
    for &b in buf {
        hash = (hash.wrapping_mul(FNV32_PRIME)) ^ fold_byte_case_insensitive(b);
    }
    hash
}

#[inline]
fn hashmap_bucket(hash: u32) -> usize {
    usize::try_from(hash).unwrap_or(0) & (HASHMAP_INITIAL_SIZE - 1)
}

struct DirNode {
    hash: u32,
    name: Vec<u8>,
    /// Reference count (Git `dir_entry.nr`): incremented for child dirs and for indexed files.
    nr: i32,
}

struct DirTable {
    map: HashMap<Vec<u8>, usize>,
    nodes: Vec<DirNode>,
}

impl DirTable {
    fn new() -> Self {
        Self {
            map: HashMap::new(),
            nodes: Vec::new(),
        }
    }
}

struct LazyEntry {
    dir: Option<usize>,
    hash_name: u32,
}

fn dir_entry_name_matches_key(name: &[u8], key: &[u8]) -> bool {
    name.len() == key.len() && name.eq_ignore_ascii_case(key)
}

fn find_dir_entry(table: &DirTable, name: &[u8], hash: u32) -> Option<usize> {
    let idx = *table.map.get(name)?;
    let n = &table.nodes[idx];
    if n.hash == hash && dir_entry_name_matches_key(&n.name, name) {
        Some(idx)
    } else {
        None
    }
}

/// Like C `strncmp(ce_name, prefix, prefix.len())` on byte strings (no NUL termination).
fn strncmp_prefix(ce_name: &[u8], prefix: &[u8]) -> std::cmp::Ordering {
    for (i, &pb) in prefix.iter().enumerate() {
        let cb = ce_name.get(i).copied().unwrap_or(0);
        match cb.cmp(&pb) {
            std::cmp::Ordering::Equal => {}
            o => return o,
        }
    }
    std::cmp::Ordering::Equal
}

fn hash_dir_entry_with_parent_and_prefix(
    table: &mut DirTable,
    parent: Option<usize>,
    prefix: &[u8],
) -> usize {
    let hash = if let Some(pidx) = parent {
        let pn = &table.nodes[pidx];
        memihash_cont(pn.hash, &prefix[pn.name.len()..])
    } else {
        memihash(prefix)
    };

    if let Some(idx) = find_dir_entry(table, prefix, hash) {
        return idx;
    }

    let idx = table.nodes.len();
    table.nodes.push(DirNode {
        hash,
        name: prefix.to_vec(),
        nr: 0,
    });
    table.map.insert(prefix.to_vec(), idx);

    if let Some(pidx) = parent {
        table.nodes[pidx].nr = table.nodes[pidx].nr.saturating_add(1);
    }

    idx
}

#[allow(clippy::too_many_arguments)]
fn handle_range_dir(
    entries: &[IndexEntry],
    k_start: usize,
    k_end: usize,
    parent: Option<usize>,
    prefix: &mut Vec<u8>,
    lazy_entries: &mut [LazyEntry],
    entry_base: usize,
    table: &mut DirTable,
) -> usize {
    let input_prefix_len = prefix.len();
    let dir_new = hash_dir_entry_with_parent_and_prefix(table, parent, prefix);
    prefix.push(b'/');

    let k = if k_start + 1 >= k_end {
        k_end
    } else if strncmp_prefix(&entries[k_start + 1].path, prefix.as_slice())
        != std::cmp::Ordering::Equal
    {
        k_start + 1
    } else if strncmp_prefix(&entries[k_end - 1].path, prefix.as_slice())
        == std::cmp::Ordering::Equal
    {
        k_end
    } else {
        let mut begin = k_start;
        let mut end = k_end;
        while begin < end {
            let mid = begin + ((end - begin) >> 1);
            let cmp = strncmp_prefix(&entries[mid].path, prefix.as_slice());
            match cmp {
                std::cmp::Ordering::Equal => begin = mid + 1,
                std::cmp::Ordering::Greater => end = mid,
                std::cmp::Ordering::Less => panic!("cache entry out of order"),
            }
        }
        begin
    };

    let mut processed = handle_range_1(
        entries,
        k_start,
        k,
        Some(dir_new),
        prefix,
        lazy_entries,
        entry_base,
        table,
    );
    if processed > 0 {
        prefix.truncate(input_prefix_len);
        return processed;
    }

    prefix.push(b'/');
    processed = handle_range_1(
        entries,
        k,
        k_end,
        Some(dir_new),
        prefix,
        lazy_entries,
        entry_base,
        table,
    );
    prefix.truncate(input_prefix_len);
    processed
}

#[allow(clippy::too_many_arguments)]
fn handle_range_1(
    entries: &[IndexEntry],
    k_start: usize,
    k_end: usize,
    parent: Option<usize>,
    prefix: &mut Vec<u8>,
    lazy_entries: &mut [LazyEntry],
    entry_base: usize,
    table: &mut DirTable,
) -> usize {
    let input_prefix_len = prefix.len();
    let mut k = k_start;

    while k < k_end {
        let ce_k = &entries[k];
        if !prefix.is_empty() && !ce_k.path.starts_with(prefix.as_slice()) {
            break;
        }

        let name = if prefix.is_empty() {
            ce_k.path.as_slice()
        } else {
            &ce_k.path[prefix.len()..]
        };

        if let Some(slash_rel) = name.iter().position(|&b| b == b'/') {
            let len = slash_rel;
            prefix.extend_from_slice(&name[..len]);
            let processed = handle_range_dir(
                entries,
                k,
                k_end,
                parent,
                prefix,
                lazy_entries,
                entry_base,
                table,
            );
            k += processed;
            prefix.truncate(input_prefix_len);
            continue;
        }

        let li = k - entry_base;
        lazy_entries[li].dir = parent;
        if let Some(pidx) = parent {
            let pn = &table.nodes[pidx];
            let suffix = &ce_k.path[pn.name.len()..];
            lazy_entries[li].hash_name = memihash_cont(pn.hash, suffix);
        } else {
            lazy_entries[li].hash_name = memihash(&ce_k.path);
        }

        k += 1;
    }

    k - k_start
}

fn lazy_update_dir_ref_counts(table: &mut DirTable, lazy_entries: &[LazyEntry]) {
    for le in lazy_entries {
        if let Some(didx) = le.dir {
            table.nodes[didx].nr = table.nodes[didx].nr.saturating_add(1);
        }
    }
}

fn lookup_lazy_params(
    policy: &crate::parallel::ThreadPolicy,
    config: &crate::config::ConfigSet,
    try_threaded: bool,
    cache_nr: usize,
) -> usize {
    crate::parallel::index_load_lazy_thread_count(policy, config, cache_nr, try_threaded)
}

fn sequential_lazy_build(index: &Index) -> (DirTable, Vec<LazyEntry>) {
    let cache_nr = index.entries.len();
    let mut lazy_entries: Vec<LazyEntry> = (0..cache_nr)
        .map(|_| LazyEntry {
            dir: None,
            hash_name: 0,
        })
        .collect();
    let mut table = DirTable::new();
    let mut prefix = Vec::new();
    handle_range_1(
        &index.entries,
        0,
        cache_nr,
        None,
        &mut prefix,
        &mut lazy_entries,
        0,
        &mut table,
    );
    lazy_update_dir_ref_counts(&mut table, &lazy_entries);
    (table, lazy_entries)
}

fn threaded_lazy_build(
    index: &Index,
    nr_dir_threads: usize,
) -> Result<(DirTable, Vec<LazyEntry>), String> {
    let cache_nr = index.entries.len();
    let mut lazy_entries: Vec<LazyEntry> = (0..cache_nr)
        .map(|_| LazyEntry {
            dir: None,
            hash_name: 0,
        })
        .collect();

    let shared = Arc::new(Mutex::new(DirTable::new()));
    let nr_each = cache_nr.div_ceil(nr_dir_threads);

    std::thread::scope(|s| {
        let mut handles = Vec::new();
        let mut k_start: usize = 0;
        for _ in 0..nr_dir_threads {
            let mut k_end = k_start.saturating_add(nr_each);
            if k_end > cache_nr {
                k_end = cache_nr;
            }
            let shared = Arc::clone(&shared);
            let slice = &index.entries;
            let range_start = k_start;
            let range_end = k_end;
            let seg_len = range_end - range_start;
            handles.push(s.spawn(move || {
                let mut prefix = Vec::new();
                let mut seg: Vec<LazyEntry> = (0..seg_len)
                    .map(|_| LazyEntry {
                        dir: None,
                        hash_name: 0,
                    })
                    .collect();
                let mut table = match shared.lock() {
                    Ok(g) => g,
                    Err(e) => e.into_inner(),
                };
                handle_range_1(
                    slice,
                    range_start,
                    range_end,
                    None,
                    &mut prefix,
                    &mut seg,
                    range_start,
                    &mut table,
                );
                (range_start, seg)
            }));
            k_start = k_end;
            if k_start >= cache_nr {
                break;
            }
        }
        for h in handles {
            match h.join() {
                Ok((rs, seg)) => {
                    for (j, le) in seg.into_iter().enumerate() {
                        lazy_entries[rs + j] = le;
                    }
                }
                Err(e) => std::panic::resume_unwind(e),
            }
        }
    });

    let mutex = Arc::try_unwrap(shared).map_err(|_| "lazy name-hash: arc still shared")?;
    let mut table = mutex.into_inner().unwrap_or_else(|e| e.into_inner());
    lazy_update_dir_ref_counts(&mut table, &lazy_entries);

    Ok((table, lazy_entries))
}

fn single_threaded_hash_pass(index: &Index) {
    let mut name_buckets = vec![Vec::new(); HASHMAP_INITIAL_SIZE];
    for (i, e) in index.entries.iter().enumerate() {
        if e.is_sparse_directory_placeholder() {
            continue;
        }
        let h = memihash(&e.path);
        name_buckets[hashmap_bucket(h)].push(i);
    }
    let _ = name_buckets;
}

/// Print `dir` / `name` lines like Git `test-lazy-init-name-hash.c` `dump_run`.
///
/// When `multi` is true but threading is disabled by Git's thresholds, returns
/// `Err` so callers can match Git's `die("non-threaded code path used")`.
///
/// # Errors
///
/// Returns an error on IO failure or when `multi` was requested without threads.
pub fn dump_lazy_init_name_hash(index: &Index, multi: bool) -> Result<(), String> {
    let cache_nr = index.entries.len();
    let policy = crate::parallel::ThreadPolicy::default();
    let config = crate::config::ConfigSet::new();
    let nr_threads = lookup_lazy_params(&policy, &config, multi, cache_nr);
    if multi && nr_threads == 0 {
        return Err("non-threaded code path used".to_owned());
    }
    let (table, lazy_entries) = if nr_threads == 0 {
        sequential_lazy_build(index)
    } else {
        threaded_lazy_build(index, nr_threads)?
    };

    let stdout = io::stdout();
    let mut out = stdout.lock();
    for dir in &table.nodes {
        let path = String::from_utf8_lossy(&dir.name);
        writeln!(out, "dir {:08x} {:7} {}", dir.hash, dir.nr, path).map_err(|e| e.to_string())?;
    }
    for (k, e) in index.entries.iter().enumerate() {
        if e.is_sparse_directory_placeholder() {
            continue;
        }
        let path = String::from_utf8_lossy(&e.path);
        writeln!(out, "name {:08x} {}", lazy_entries[k].hash_name, path)
            .map_err(|e| e.to_string())?;
    }
    Ok(())
}

/// Run Git-compatible lazy name-hash initialization on `index`.
///
/// When `try_threaded` is true and entry count / CPU count meet Git's thresholds, uses a
/// multi-threaded directory scan (return value is the thread count). Otherwise uses the
/// single-threaded path and returns `0`.
///
/// Sparse-directory placeholder entries are skipped for the name table, matching Git's
/// `S_ISSPARSEDIR` handling.
///
/// # Errors
///
/// Returns an error if the threaded builder cannot recover the shared directory table
/// (should not happen after worker threads have joined).
pub fn test_lazy_init_name_hash(index: &Index, try_threaded: bool) -> Result<usize, String> {
    let cache_nr = index.entries.len();
    let policy = crate::parallel::ThreadPolicy::default();
    let config = crate::config::ConfigSet::new();
    let nr_threads = lookup_lazy_params(&policy, &config, try_threaded, cache_nr);
    if nr_threads == 0 {
        single_threaded_hash_pass(index);
    } else {
        threaded_lazy_build(index, nr_threads)?;
    }
    Ok(nr_threads)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::index::{Index, IndexEntry, MODE_REGULAR};
    use crate::objects::{HashAlgo, ObjectKind};

    fn entry(path: &str, ino: u32) -> IndexEntry {
        IndexEntry {
            ctime_sec: 1,
            ctime_nsec: 0,
            mtime_sec: 1,
            mtime_nsec: 0,
            dev: 1,
            ino,
            mode: MODE_REGULAR,
            uid: 1,
            gid: 1,
            size: 1,
            oid: HashAlgo::Sha1
                .hash_object(ObjectKind::Blob, b"x")
                .expect("hash"),
            flags: path.len().min(0xFFF) as u16,
            flags_extended: None,
            path: path.as_bytes().to_vec(),
            base_index_pos: 0,
        }
    }

    #[test]
    fn memihash_is_case_insensitive() {
        assert_eq!(memihash(b"ABC/def"), memihash(b"abc/DEF"));
        assert_ne!(memihash(b"abc"), memihash_cont(memihash(b"dir"), b"/file"));
    }

    #[test]
    fn sequential_lazy_build_hashes_mixed_case_paths() {
        let mut idx = Index::empty(2);
        idx.add_or_replace(entry("Mixed/Case.txt", 1));
        let (table, lazy) = sequential_lazy_build(&idx);
        assert!(!table.nodes.is_empty());
        assert_eq!(lazy[0].hash_name, memihash(b"Mixed/Case.txt"));
        assert_eq!(memihash(b"Mixed/Case.txt"), memihash(b"mixed/case.txt"));
    }

    #[test]
    fn lazy_init_uses_threads_on_large_index() {
        let mut idx = Index::empty(2);
        for i in 0..5000 {
            idx.add_or_replace(entry(&format!("d{i:04}/f.txt"), i));
        }
        let threads = test_lazy_init_name_hash(&idx, true).expect("lazy init");
        assert!(threads > 0, "large index should use threaded lazy init");
        dump_lazy_init_name_hash(&idx, true).expect("dump multi");
    }
}

//! Parsing Git commit-graph files and Bloom filter lookup (`commit-graph.c` / `bloom.c` compatible).

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use crate::bloom::{
    bloom_filter_contains, bloom_keyvec_for_path, BloomBuildOutcome, BloomFilterSettings,
};
use crate::error::Error;
use crate::objects::ObjectId;
use crate::odb::Odb;

/// Track which commit-graph layers have already emitted the "disabling Bloom
/// filters ... due to incompatible settings" warning this process, so it is
/// printed at most once per layer (matching Git, which loads the chain once).
fn warn_once_for_disabled_bloom_layer(id: &str) -> bool {
    use std::collections::HashSet;
    use std::sync::OnceLock;
    static SEEN: OnceLock<Mutex<HashSet<String>>> = OnceLock::new();
    let set = SEEN.get_or_init(|| Mutex::new(HashSet::new()));
    match set.lock() {
        Ok(mut guard) => guard.insert(id.to_string()),
        Err(_) => true,
    }
}

/// Emit the "base graphs chunk is too small" warning at most once per layer id
/// (grit re-reads the chain several times within one command; Git loads it once).
fn warn_once_for_base_chunk_too_small(id: &str) -> bool {
    use std::collections::HashSet;
    use std::sync::OnceLock;
    static SEEN: OnceLock<Mutex<HashSet<String>>> = OnceLock::new();
    let set = SEEN.get_or_init(|| Mutex::new(HashSet::new()));
    match set.lock() {
        Ok(mut guard) => guard.insert(id.to_string()),
        Err(_) => true,
    }
}

const SIGNATURE: &[u8; 4] = b"CGPH";
const GRAPH_VERSION: u8 = 1;

const CHUNK_OID_FANOUT: u32 = 0x4f49_4446; // OIDF
const CHUNK_OID_LOOKUP: u32 = 0x4f49_444c; // OIDL
const CHUNK_COMMIT_DATA: u32 = 0x4344_4154; // CDAT
const CHUNK_GENERATION_DATA: u32 = 0x4744_4132; // GDA2
const CHUNK_GENERATION_DATA_OVERFLOW: u32 = 0x4744_4f32; // GDO2
const CHUNK_EXTRA_EDGES: u32 = 0x4544_4745; // EDGE

/// CDAT parent word meaning "no parent in this slot" (Git `GRAPH_PARENT_NONE`).
const GRAPH_PARENT_NONE: u32 = 0x7000_0000;
/// CDAT parent-2 high bit meaning "remaining parents live in the EXTRA_EDGES
/// chunk" (octopus merge; Git `GRAPH_EXTRA_EDGES_NEEDED`).
const GRAPH_EXTRA_EDGES_NEEDED: u32 = 0x8000_0000;
const CHUNK_BLOOM_INDEXES: u32 = 0x4249_4458; // BIDX
const CHUNK_BLOOM_DATA: u32 = 0x4244_4154; // BDAT
const CHUNK_BASE_GRAPHS: u32 = 0x4241_5345; // BASE

const BLOOM_HEADER: usize = crate::bloom::BLOOMDATA_HEADER_LEN;

/// `CORRECTED_COMMIT_DATE_OFFSET_OVERFLOW` — high bit marks GDA2 entries that index GDO2.
const CORRECTED_COMMIT_DATE_OFFSET_OVERFLOW: u32 = 1u32 << 31;

fn warn_path_for_graph_file(path: &Path) -> String {
    let s = path.to_string_lossy();
    if let Some(idx) = s.find(".git/") {
        return s[idx..].replace('\\', "/");
    }
    s.replace('\\', "/")
}

/// One layer from `.git/objects/info/commit-graph` or `commit-graphs/<hash>.graph`.
#[derive(Debug, Clone)]
pub struct CommitGraphLayer {
    pub path: PathBuf,
    body: Vec<u8>,
    num_commits: u32,
    oid_lookup_off: usize,
    #[allow(dead_code)]
    chunk_commit_data_off: usize,
    #[allow(dead_code)]
    chunk_generation_data: Option<usize>,
    read_generation_data: bool,
    chunk_bloom_indexes: Option<usize>,
    chunk_bloom_data: Option<(usize, usize)>,
    bloom_settings: Option<BloomFilterSettings>,
    bloom_disabled: bool,
    /// Size in bytes of the BASE chunk (0 if absent). Used to bounds-check the
    /// base-graph list from the commit-graph header (Git `add_graph_to_chain`).
    base_chunk_size: usize,
    /// OID width in bytes implied by the header hash version (20 for SHA-1, 32 for SHA-256).
    hash_len: usize,
}

/// OID width for a commit-graph header hash-version byte (`body[5]`).
const fn commit_graph_hash_len(hash_version: u8) -> Option<usize> {
    match hash_version {
        1 => Some(20),
        2 => Some(32),
        _ => None,
    }
}

impl CommitGraphLayer {
    /// Parse a commit-graph layer; fails if generation overflow chunk is inconsistent with GDA2.
    pub fn try_parse(path: PathBuf, raw: Vec<u8>) -> Result<Self, Error> {
        if raw.len() < 28 {
            return Err(Error::CorruptObject(
                "commit-graph file too small".to_owned(),
            ));
        }
        // The hash version (byte 5) selects the OID width and trailing-checksum
        // width: 1 → SHA-1 (20 bytes), 2 → SHA-256 (32 bytes).
        let hash_len = match commit_graph_hash_len(raw[5]) {
            Some(n) => n,
            None => {
                return Err(Error::CorruptObject(format!(
                    "commit-graph version/hash not supported (version {} hash {})",
                    raw[4], raw[5]
                )))
            }
        };
        let body = raw[..raw.len() - hash_len].to_vec();
        if body.len() < 8 || &body[0..4] != SIGNATURE {
            return Err(Error::CorruptObject(
                "commit-graph has bad signature".to_owned(),
            ));
        }
        if body[4] != GRAPH_VERSION {
            return Err(Error::CorruptObject(format!(
                "commit-graph version/hash not supported (version {} hash {})",
                body[4], body[5]
            )));
        }
        let num_chunks = body[6] as usize;
        let toc_start = 8;
        let toc_end = toc_start + (num_chunks + 1) * 12;
        if body.len() < toc_end {
            return Err(Error::CorruptObject(
                "commit-graph truncated at chunk table".to_owned(),
            ));
        }

        let mut fanout_off = None;
        let mut oid_lookup_off = None;
        let mut commit_data_off = None;
        let mut generation_off = None;
        let mut generation_overflow_off = None;
        let mut bloom_idx_off = None;
        let mut bloom_data_range = None;
        let mut base_graphs_off = None;
        let mut chunk_offsets: Vec<usize> = Vec::new();
        let mut toc_entries: Vec<(u32, usize)> = Vec::with_capacity(num_chunks);

        for i in 0..num_chunks {
            let e = toc_start + i * 12;
            let id = u32::from_be_bytes(
                body[e..e + 4]
                    .try_into()
                    .map_err(|_| Error::CorruptObject("commit-graph bad TOC".to_owned()))?,
            );
            let off = u64::from_be_bytes(
                body[e + 4..e + 12]
                    .try_into()
                    .map_err(|_| Error::CorruptObject("commit-graph bad TOC".to_owned()))?,
            ) as usize;
            toc_entries.push((id, off));
            chunk_offsets.push(off);
            match id {
                CHUNK_OID_FANOUT => fanout_off = Some(off),
                CHUNK_OID_LOOKUP => oid_lookup_off = Some(off),
                CHUNK_COMMIT_DATA => commit_data_off = Some(off),
                CHUNK_GENERATION_DATA => generation_off = Some(off),
                CHUNK_GENERATION_DATA_OVERFLOW => generation_overflow_off = Some(off),
                CHUNK_BLOOM_INDEXES => bloom_idx_off = Some(off),
                CHUNK_BASE_GRAPHS => base_graphs_off = Some(off),
                CHUNK_BLOOM_DATA => {
                    let end = if i + 1 < num_chunks {
                        let e2 = toc_start + (i + 1) * 12;
                        u64::from_be_bytes(body[e2 + 4..e2 + 12].try_into().unwrap_or([0u8; 8]))
                            as usize
                    } else {
                        let term = toc_start + num_chunks * 12;
                        u64::from_be_bytes(body[term + 4..term + 12].try_into().unwrap_or([0u8; 8]))
                            as usize
                    };
                    bloom_data_range = Some((off, end.saturating_sub(off)));
                }
                _ => {}
            }
        }
        let file_end = u64::from_be_bytes(
            body[toc_start + num_chunks * 12 + 4..toc_start + num_chunks * 12 + 12]
                .try_into()
                .map_err(|_| Error::CorruptObject("commit-graph bad file end".to_owned()))?,
        ) as usize;
        chunk_offsets.push(file_end);
        chunk_offsets.sort_unstable();
        chunk_offsets.dedup();

        fn chunk_byte_range(
            start: usize,
            toc_entries: &[(u32, usize)],
            file_end: usize,
        ) -> Result<usize, Error> {
            let mut ends: Vec<usize> = toc_entries
                .iter()
                .map(|&(_, o)| o)
                .filter(|&o| o > start)
                .collect();
            ends.sort_unstable();
            let end = ends.first().copied().unwrap_or(file_end);
            if end < start {
                return Err(Error::CorruptObject(
                    "commit-graph chunk layout invalid".to_owned(),
                ));
            }
            Ok(end)
        }

        if let Some(gda) = generation_off {
            let gda_end = chunk_byte_range(gda, &toc_entries, file_end)?;
            let gda_len = gda_end.saturating_sub(gda);
            let num_commits = fanout_off
                .and_then(|fo| {
                    let slice = body.get(fo + 255 * 4..fo + 256 * 4)?;
                    Some(u32::from_be_bytes(slice.try_into().ok()?))
                })
                .ok_or_else(|| Error::CorruptObject("commit-graph missing fanout".to_owned()))?;
            let expected = num_commits as usize * 4;
            if gda_len < expected {
                return Err(Error::CorruptObject(
                    "commit-graph generation data chunk is too small".to_owned(),
                ));
            }
            let gda_slice = body.get(gda..gda + expected).ok_or_else(|| {
                Error::CorruptObject("commit-graph generation data OOB".to_owned())
            })?;
            let mut max_overflow_idx: Option<u32> = None;
            for w in 0..num_commits as usize {
                let v =
                    u32::from_be_bytes(gda_slice[w * 4..w * 4 + 4].try_into().map_err(|_| {
                        Error::CorruptObject("commit-graph GDA2 corrupt".to_owned())
                    })?);
                if v & CORRECTED_COMMIT_DATE_OFFSET_OVERFLOW != 0 {
                    let pos = v ^ CORRECTED_COMMIT_DATE_OFFSET_OVERFLOW;
                    max_overflow_idx = Some(match max_overflow_idx {
                        None => pos,
                        Some(m) => m.max(pos),
                    });
                }
            }
            if let Some(pos) = max_overflow_idx {
                let Some(gdo_start) = generation_overflow_off else {
                    return Err(Error::CorruptObject(
                        "commit-graph requires overflow generation data but has none".to_owned(),
                    ));
                };
                let gdo_end = chunk_byte_range(gdo_start, &toc_entries, file_end)?;
                let overflow_bytes = gdo_end.saturating_sub(gdo_start);
                let n_slots = overflow_bytes / 8;
                if n_slots <= pos as usize {
                    return Err(Error::CorruptObject(
                        "commit-graph overflow generation data is too small".to_owned(),
                    ));
                }
            }
        }
        let bidx_len = bloom_idx_off.and_then(|b| {
            chunk_offsets
                .iter()
                .find(|&&o| o > b)
                .map(|&next| next.saturating_sub(b))
        });

        let fanout_off = fanout_off.ok_or_else(|| {
            Error::CorruptObject("commit-graph missing OID fanout chunk".to_owned())
        })?;
        let oid_lookup_off = oid_lookup_off.ok_or_else(|| {
            Error::CorruptObject("commit-graph missing OID lookup chunk".to_owned())
        })?;
        let commit_data_off = commit_data_off.ok_or_else(|| {
            Error::CorruptObject("commit-graph missing commit data chunk".to_owned())
        })?;
        if fanout_off + 256 * 4 > body.len() || oid_lookup_off + 4 > body.len() {
            return Err(Error::CorruptObject(
                "commit-graph chunk extends past end of file".to_owned(),
            ));
        }
        let num_commits = u32::from_be_bytes(
            body[fanout_off + 255 * 4..fanout_off + 256 * 4]
                .try_into()
                .map_err(|_| Error::CorruptObject("commit-graph fanout corrupt".to_owned()))?,
        );
        if oid_lookup_off + num_commits as usize * hash_len > body.len() {
            return Err(Error::CorruptObject(
                "commit-graph OID lookup extends past end of file".to_owned(),
            ));
        }
        let graph_data_width = hash_len + 16;
        if commit_data_off + num_commits as usize * graph_data_width > body.len() {
            return Err(Error::CorruptObject(
                "commit-graph commit data extends past end of file".to_owned(),
            ));
        }

        let read_generation_data = generation_off.is_some();
        let mut bloom_settings = None;
        let mut chunk_bloom_data = None;
        if let (Some(_bidx), Some((bdat_off, bdat_len))) = (bloom_idx_off, bloom_data_range) {
            if bdat_len < BLOOM_HEADER {
                eprintln!(
                    "warning: ignoring too-small changed-path chunk ({} < {}) in commit-graph file",
                    bdat_len, BLOOM_HEADER
                );
            } else if bdat_off + bdat_len <= body.len() {
                let hdr = &body[bdat_off..bdat_off + BLOOM_HEADER];
                let hash_version: [u8; 4] = hdr[0..4]
                    .try_into()
                    .map_err(|_| Error::CorruptObject("Bloom header corrupt".to_owned()))?;
                let num_hashes: [u8; 4] = hdr[4..8]
                    .try_into()
                    .map_err(|_| Error::CorruptObject("Bloom header corrupt".to_owned()))?;
                let bits_per_entry: [u8; 4] = hdr[8..12]
                    .try_into()
                    .map_err(|_| Error::CorruptObject("Bloom header corrupt".to_owned()))?;
                bloom_settings = Some(BloomFilterSettings {
                    hash_version: u32::from_be_bytes(hash_version),
                    num_hashes: u32::from_be_bytes(num_hashes),
                    bits_per_entry: u32::from_be_bytes(bits_per_entry),
                    max_changed_paths: 512,
                });
                chunk_bloom_data = Some((bdat_off, bdat_len));
            }
        }

        let bloom_indexes_ok = if let (Some(bidx), Some(bsize)) = (bloom_idx_off, bidx_len) {
            if bsize / 4 != num_commits as usize {
                eprintln!("warning: commit-graph changed-path index chunk is too small");
                false
            } else if bidx + bsize > body.len() {
                eprintln!("warning: commit-graph changed-path index chunk is too small");
                false
            } else {
                true
            }
        } else {
            false
        };
        let bloom_pair_ok = bloom_settings.is_some()
            && chunk_bloom_data.is_some()
            && bloom_indexes_ok
            && chunk_bloom_data.is_some_and(|(_, len)| len >= BLOOM_HEADER);

        let (chunk_bloom_indexes, bloom_settings) = if bloom_pair_ok {
            (bloom_idx_off, bloom_settings)
        } else {
            (None, None)
        };
        let chunk_bloom_data = if bloom_pair_ok {
            chunk_bloom_data
        } else {
            None
        };

        let base_chunk_size = match base_graphs_off {
            Some(off) => {
                let end = chunk_byte_range(off, &toc_entries, file_end)?;
                end.saturating_sub(off)
            }
            None => 0,
        };

        Ok(Self {
            path,
            body,
            num_commits,
            oid_lookup_off,
            chunk_commit_data_off: commit_data_off,
            chunk_generation_data: generation_off,
            read_generation_data,
            chunk_bloom_indexes,
            chunk_bloom_data,
            bloom_settings,
            bloom_disabled: false,
            base_chunk_size,
            hash_len,
        })
    }

    fn parse(path: PathBuf, raw: Vec<u8>) -> Option<Self> {
        Self::try_parse(path, raw).ok()
    }

    fn oid_at_lex(&self, lex_index: u32) -> Option<ObjectId> {
        if lex_index >= self.num_commits {
            return None;
        }
        let off = self.oid_lookup_off + lex_index as usize * self.hash_len;
        ObjectId::from_bytes(self.body.get(off..off + self.hash_len)?).ok()
    }

    /// Decode the CDAT (commit-data) record at `lex_index` into
    /// `(parent1_word, parent2_word, committer_time)`. The parent words are raw
    /// graph-position encodings (see [`GRAPH_PARENT_NONE`] /
    /// [`GRAPH_EXTRA_EDGES_NEEDED`]); the time is the low 34 bits of the trailing
    /// generation/commit-time field. Each record is `hash_len + 16` bytes:
    /// root-tree OID, two 4-byte parent words, then an 8-byte gen/time field.
    fn cdat_record(&self, lex_index: u32) -> Option<(u32, u32, i64)> {
        if lex_index >= self.num_commits {
            return None;
        }
        let base = self.chunk_commit_data_off + lex_index as usize * (self.hash_len + 16);
        let p1_off = base + self.hash_len;
        let p1 = u32::from_be_bytes(self.body.get(p1_off..p1_off + 4)?.try_into().ok()?);
        let p2 = u32::from_be_bytes(self.body.get(p1_off + 4..p1_off + 8)?.try_into().ok()?);
        let gen_time = u64::from_be_bytes(self.body.get(p1_off + 8..p1_off + 16)?.try_into().ok()?);
        let commit_time = (gen_time & 0x3_FFFF_FFFF) as i64;
        Some((p1, p2, commit_time))
    }

    fn bsearch_oid(&self, oid: &ObjectId) -> Option<u32> {
        let mut lo = 0u32;
        let mut hi = self.num_commits;
        let bytes = oid.as_bytes();
        while lo < hi {
            let mid = (lo + hi) / 2;
            let off = self.oid_lookup_off + mid as usize * self.hash_len;
            let slice = &self.body[off..off + self.hash_len];
            match slice.cmp(bytes) {
                std::cmp::Ordering::Less => lo = mid + 1,
                std::cmp::Ordering::Greater => hi = mid,
                std::cmp::Ordering::Equal => return Some(mid),
            }
        }
        None
    }

    fn disable_bloom(&mut self) {
        self.chunk_bloom_indexes = None;
        self.chunk_bloom_data = None;
        self.bloom_settings = None;
        self.bloom_disabled = true;
    }

    fn layer_display_id(&self) -> String {
        self.path
            .file_stem()
            .and_then(|s| s.to_str())
            .map(|s| s.strip_prefix("graph-").unwrap_or(s).to_string())
            .unwrap_or_else(|| "commit-graph".to_string())
    }

    fn bloom_filter_slice(&self, lex_index: u32) -> Option<&[u8]> {
        let _settings = self.bloom_settings.as_ref()?;
        let bidx_base = self.chunk_bloom_indexes?;
        let (bdat_off, bdat_total) = self.chunk_bloom_data?;
        let graph_warn = warn_path_for_graph_file(self.path.as_path());
        if lex_index >= self.num_commits {
            return None;
        }
        let payload_len = bdat_total.saturating_sub(BLOOM_HEADER);
        let end_rel = u32::from_be_bytes(
            self.body[bidx_base + lex_index as usize * 4..bidx_base + lex_index as usize * 4 + 4]
                .try_into()
                .ok()?,
        ) as usize;
        let start_rel = if lex_index == 0 {
            0usize
        } else {
            u32::from_be_bytes(
                self.body[bidx_base + (lex_index as usize - 1) * 4
                    ..bidx_base + (lex_index as usize - 1) * 4 + 4]
                    .try_into()
                    .ok()?,
            ) as usize
        };
        // Git checks both offsets for being out of range *before* comparing them
        // (`load_bloom_filter_from_graph`: two `check_bloom_offset` calls joined by
        // `||`, then the decreasing-offset check). The end offset (at this position)
        // is checked first, short-circuiting the start offset (reported one position
        // back) when it fails.
        let max_payload = payload_len;
        if end_rel > max_payload {
            eprintln!(
                "warning: ignoring out-of-range offset ({end_rel}) for changed-path filter at pos {} of {} (chunk size: {bdat_total})",
                lex_index,
                graph_warn,
                bdat_total = bdat_total
            );
            return None;
        }
        if start_rel > max_payload {
            eprintln!(
                "warning: ignoring out-of-range offset ({start_rel}) for changed-path filter at pos {} of {} (chunk size: {bdat_total})",
                lex_index.saturating_sub(1),
                graph_warn,
                bdat_total = bdat_total
            );
            return None;
        }
        if end_rel < start_rel {
            eprintln!(
                "warning: ignoring decreasing changed-path index offsets ({start_rel} > {end_rel}) for positions {} and {} of {}",
                lex_index.saturating_sub(1),
                lex_index,
                graph_warn
            );
            return None;
        }
        let data_base = bdat_off + BLOOM_HEADER;
        let abs_start = data_base + start_rel;
        let abs_end = data_base + end_rel;
        if abs_end > bdat_off + bdat_total || abs_start > abs_end {
            return None;
        }
        Some(&self.body[abs_start..abs_end])
    }
}

/// Result of consulting Bloom filters before running a tree diff (matches `revision.c`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BloomPrecheck {
    /// No commit-graph, pathspecs disallow Bloom, or wrong parent count — no statistics.
    Inapplicable,
    /// Commit not in graph or generation unavailable — skip Bloom (`-1` without `filter_not_present`).
    NotInGraph,
    /// Bloom filter missing or unusable (`filter_not_present` in Git).
    FilterNotPresent,
    /// Bloom says path cannot be in this commit (`definitely_not`).
    DefinitelyNot,
    /// Bloom says maybe — caller must run diff and may count `false_positive`.
    Maybe,
}

/// Counters for `GIT_TRACE2_PERF` Bloom statistics (`revision.c` `trace2_bloom_filter_statistics_atexit`).
#[derive(Debug, Default, Clone)]
pub struct BloomWalkStats {
    pub filter_not_present: u32,
    pub maybe: u32,
    pub definitely_not: u32,
    pub false_positive: u32,
}

impl BloomWalkStats {
    pub fn record_precheck(&mut self, pre: BloomPrecheck) {
        match pre {
            BloomPrecheck::Inapplicable | BloomPrecheck::NotInGraph => {}
            BloomPrecheck::FilterNotPresent => self.filter_not_present += 1,
            BloomPrecheck::DefinitelyNot => self.definitely_not += 1,
            BloomPrecheck::Maybe => self.maybe += 1,
        }
    }

    pub fn record_false_positive(&mut self) {
        self.false_positive += 1;
    }
}

/// Shared stats handle for [`crate::rev_list::RevListOptions::bloom_stats`].
pub type BloomWalkStatsHandle = Arc<Mutex<BloomWalkStats>>;

/// Loaded commit-graph chain (newest layer first, matching `commit-graph-chain` file order).
#[derive(Debug, Clone)]
pub struct CommitGraphChain {
    layers: Vec<CommitGraphLayer>,
}

impl CommitGraphChain {
    /// Bloom settings from the newest layer, if that layer carries Bloom data.
    #[must_use]
    pub fn top_layer_bloom_settings(&self) -> Option<BloomFilterSettings> {
        self.layers.first()?.bloom_settings
    }

    /// Total commits across all layers (Git `num_commits_in_base` offset for new layers).
    #[must_use]
    pub fn total_commits(&self) -> u32 {
        self.layers.iter().map(|l| l.num_commits).sum()
    }

    /// Layer file paths from oldest base to newest (reverse of chain file order).
    #[must_use]
    pub fn layer_paths_oldest_first(&self) -> Vec<PathBuf> {
        self.layers.iter().rev().map(|l| l.path.clone()).collect()
    }

    /// Number of layers in the chain.
    #[must_use]
    pub fn num_layers(&self) -> usize {
        self.layers.len()
    }

    /// Commit counts per layer, tip-first (layer 0 is the newest tip).
    #[must_use]
    pub fn layer_commit_counts_tip_first(&self) -> Vec<u32> {
        self.layers.iter().map(|l| l.num_commits).collect()
    }

    /// Whether each layer carries a generation-data (GDA2) chunk, tip-first.
    #[must_use]
    pub fn layer_has_generation_data_tip_first(&self) -> Vec<bool> {
        self.layers
            .iter()
            .map(|l| l.chunk_generation_data.is_some())
            .collect()
    }

    /// Layer hex hashes (from `graph-<hash>.graph` file stem), tip-first.
    #[must_use]
    pub fn layer_hashes_tip_first(&self) -> Vec<String> {
        self.layers.iter().map(|l| l.layer_display_id()).collect()
    }

    /// Source object directory each layer was loaded from (its `.git/objects`),
    /// tip-first. Derived from the layer path: `<objdir>/info/commit-graphs/graph-*.graph`
    /// or `<objdir>/info/commit-graph`.
    #[must_use]
    pub fn layer_object_dirs_tip_first(&self) -> Vec<PathBuf> {
        self.layers
            .iter()
            .map(|l| {
                // .../objects/info/commit-graphs/graph-X.graph  -> .../objects
                // .../objects/info/commit-graph                 -> .../objects
                let p = l.path.as_path();
                let info = if p
                    .parent()
                    .and_then(|d| d.file_name())
                    .map(|n| n == "commit-graphs")
                    .unwrap_or(false)
                {
                    p.parent().and_then(|d| d.parent())
                } else {
                    p.parent()
                };
                info.and_then(|d| d.parent())
                    .map(Path::to_path_buf)
                    .unwrap_or_else(|| PathBuf::from("."))
            })
            .collect()
    }

    /// A sub-chain made of the layers at tip-first indices `start..end`
    /// (so `start` becomes the new tip). Used by the writer when only some base
    /// layers are kept after a split merge.
    #[must_use]
    pub fn sub_chain_tip_first(&self, start: usize, end: usize) -> Option<Self> {
        let end = end.min(self.layers.len());
        if start >= end {
            return None;
        }
        Some(Self {
            layers: self.layers[start..end].to_vec(),
        })
    }

    /// All commit OIDs in one layer (by tip-first index), in lexicographic order.
    #[must_use]
    pub fn layer_oids(&self, tip_first_idx: usize) -> Vec<ObjectId> {
        let Some(layer) = self.layers.get(tip_first_idx) else {
            return Vec::new();
        };
        let mut out = Vec::with_capacity(layer.num_commits as usize);
        for i in 0..layer.num_commits {
            if let Some(oid) = layer.oid_at_lex(i) {
                out.push(oid);
            }
        }
        out
    }

    /// Load from `objects/info/commit-graph` or `objects/info/commit-graphs/commit-graph-chain`.
    ///
    /// Returns `Ok(None)` when no commit-graph exists. Corrupt graphs (including invalid GDO2)
    /// return [`Err`].
    pub fn try_load(objects_dir: &Path) -> Result<Option<Self>, Error> {
        let info = objects_dir.join("info");
        let chain_path = info.join("commit-graphs").join("commit-graph-chain");
        if chain_path.is_file() {
            let content = std::fs::read_to_string(&chain_path).map_err(Error::from)?;
            let mut layers = Vec::new();
            for line in content.lines() {
                let h = line.trim();
                if h.len() != 40 {
                    continue;
                }
                let graph_path = info.join("commit-graphs").join(format!("graph-{h}.graph"));
                let raw = std::fs::read(&graph_path).map_err(Error::from)?;
                let layer = CommitGraphLayer::try_parse(graph_path, raw)?;
                // `add_graph_to_chain`: a layer that declares N base graphs must
                // carry a BASE chunk large enough to hold N hashes. If it is too
                // small, Git warns and refuses to add this layer (and anything
                // above it) to the chain, falling back to the object database for
                // those commits. `layers.len()` here is the number of base layers
                // already loaded below this one.
                let n = layers.len();
                if n > 0 && layer.base_chunk_size / layer.hash_len < n {
                    if warn_once_for_base_chunk_too_small(&layer.layer_display_id()) {
                        eprintln!("warning: commit-graph base graphs chunk is too small");
                    }
                    break;
                }
                layers.push(layer);
            }
            if layers.is_empty() {
                return Ok(None);
            }
            // The on-disk chain file lists layers base-first (Git order: line 1
            // is the base graph, the last line is the tip). Grit's internal
            // representation is tip-first, so reverse after reading.
            layers.reverse();
            let mut chain = Self { layers };
            chain.validate_bloom_compatibility();
            return Ok(Some(chain));
        }
        let single = info.join("commit-graph");
        if single.is_file() {
            let raw = std::fs::read(&single).map_err(Error::from)?;
            let layer = CommitGraphLayer::try_parse(single.clone(), raw)?;
            let mut chain = Self {
                layers: vec![layer],
            };
            chain.validate_bloom_compatibility();
            return Ok(Some(chain));
        }
        Ok(None)
    }

    /// Like [`Self::try_load`] but ignores parse errors (returns `None`).
    pub fn load(objects_dir: &Path) -> Option<Self> {
        Self::try_load(objects_dir).ok().flatten()
    }

    /// Load a split commit-graph chain that may live in (and reference layers across) an
    /// alternate object directory.
    ///
    /// Git's commit-graph chain is owned by exactly one object directory, but its layer
    /// `graph-<hash>.graph` files may be split between that directory and the alternate(s) it
    /// inherits from. This is the situation created by a local clone whose `info/alternates`
    /// points at a source repository that already has a split chain: a new `commit-graph write
    /// --split` produces a chain file (in the local object dir) that lists the alternate's base
    /// layer hashes followed by the locally-written tip layer.
    ///
    /// The chain file itself is taken from `objects_dir` if present, otherwise from the first
    /// `alt_dirs` entry that has one. Each referenced `graph-<hash>.graph` layer is then resolved
    /// by searching `objects_dir` first and each alternate in turn.
    ///
    /// # Parameters
    /// - `objects_dir`: the primary object directory (e.g. the local `.git/objects`).
    /// - `alt_dirs`: alternate object directories, in search order.
    ///
    /// # Returns
    /// `Ok(None)` when no chain/single graph is found in any directory; `Ok(Some(_))` on success.
    ///
    /// # Errors
    /// Returns an error if a referenced layer file is missing/unreadable or fails to parse.
    pub fn try_load_across(
        objects_dir: &Path,
        alt_dirs: &[PathBuf],
    ) -> Result<Option<Self>, Error> {
        let layer_dir = |dir: &Path| dir.join("info").join("commit-graphs");
        let resolve_layer = |hash: &str| -> Option<PathBuf> {
            let name = format!("graph-{hash}.graph");
            let local = layer_dir(objects_dir).join(&name);
            if local.is_file() {
                return Some(local);
            }
            alt_dirs
                .iter()
                .map(|d| layer_dir(d).join(&name))
                .find(|p| p.is_file())
        };

        // A split write only chains on an existing *split chain*. Precedence for the chain owner:
        //   1. the local dir's chain file (its layers may live across the alternate),
        //   2. the local dir's single (non-split) `info/commit-graph` (Git migrates it),
        //   3. a *chain file* in an alternate.
        // An alternate's single (non-split) graph file is never used as a base — Git refuses to
        // base a chain on a plain commit-graph file (t5324 "fork and fail to base a chain on a
        // commit-graph file").
        let local_chain = layer_dir(objects_dir).join("commit-graph-chain");
        let local_single = objects_dir.join("info").join("commit-graph");
        let chain_path = if local_chain.is_file() {
            local_chain
        } else if local_single.is_file() {
            return Self::try_load(objects_dir);
        } else {
            match alt_dirs
                .iter()
                .map(PathBuf::as_path)
                .find(|d| layer_dir(d).join("commit-graph-chain").is_file())
            {
                Some(owner) => layer_dir(owner).join("commit-graph-chain"),
                None => return Ok(None),
            }
        };

        let content = std::fs::read_to_string(&chain_path).map_err(Error::from)?;
        let mut layers = Vec::new();
        for line in content.lines() {
            let h = line.trim();
            if h.len() != 40 {
                continue;
            }
            let graph_path = resolve_layer(h).ok_or_else(|| {
                Error::from(std::io::Error::new(
                    std::io::ErrorKind::NotFound,
                    format!("commit-graph layer graph-{h}.graph not found"),
                ))
            })?;
            let raw = std::fs::read(&graph_path).map_err(Error::from)?;
            let layer = CommitGraphLayer::try_parse(graph_path, raw)?;
            let n = layers.len();
            if n > 0 && layer.base_chunk_size / layer.hash_len < n {
                if warn_once_for_base_chunk_too_small(&layer.layer_display_id()) {
                    eprintln!("warning: commit-graph base graphs chunk is too small");
                }
                break;
            }
            layers.push(layer);
        }
        if layers.is_empty() {
            return Ok(None);
        }
        layers.reverse();
        let mut chain = Self { layers };
        chain.validate_bloom_compatibility();
        Ok(Some(chain))
    }

    fn validate_bloom_compatibility(&mut self) {
        // Git walks the chain from the tip down to the base (`for (; g; g =
        // g->base_graph)` in validate_mixed_bloom_settings), so the *topmost*
        // (tip) layer's Bloom settings become the reference and any
        // incompatible *lower* (base) layer is disabled. `self.layers` is
        // stored tip-first internally, so iterate forward to match.
        let mut ref_settings: Option<BloomFilterSettings> = None;
        for layer in &mut self.layers {
            let Some(bs) = layer.bloom_settings else {
                continue;
            };
            match ref_settings {
                None => ref_settings = Some(bs),
                Some(r) => {
                    if r.hash_version != bs.hash_version
                        || r.num_hashes != bs.num_hashes
                        || r.bits_per_entry != bs.bits_per_entry
                    {
                        let id = layer.layer_display_id();
                        // Git loads the commit-graph chain once per process and caches
                        // it, so the "disabling Bloom filters" warning is emitted at
                        // most once per layer. Grit re-reads the chain from disk several
                        // times within a single command (settings probe, commit set,
                        // filter reuse), so dedupe the warning per layer id to match.
                        if warn_once_for_disabled_bloom_layer(&id) {
                            eprintln!(
                                "warning: disabling Bloom filters for commit-graph layer '{id}' due to incompatible settings"
                            );
                        }
                        layer.disable_bloom();
                    }
                }
            }
        }
    }

    /// Existing changed-path Bloom filter bytes for `oid`, if present in any layer
    /// whose Bloom settings are compatible with `want`. Returns `Some(bytes)` (possibly
    /// empty for an empty/no-change filter) when the filter can be reused verbatim, or
    /// `None` when no compatible filter exists. Used by the writer to backfill / reuse
    /// already-computed filters (Git counts these as `filter_not_computed`).
    pub fn existing_filter_bytes(
        &self,
        oid: &ObjectId,
        want: &BloomFilterSettings,
    ) -> Option<Vec<u8>> {
        let (layer_idx, lex) = self.find_commit(oid)?;
        let layer = &self.layers[layer_idx];
        let settings = layer.bloom_settings.as_ref()?;
        if settings.hash_version != want.hash_version
            || settings.num_hashes != want.num_hashes
            || settings.bits_per_entry != want.bits_per_entry
        {
            return None;
        }
        // Git only reuses a loaded filter when its on-disk length is non-zero
        // (`get_or_compute_bloom_filter`: `if (filter->data && filter->len)`).
        // A zero-length entry means the filter was skipped (over the
        // `--max-new-filters` budget) and must be (re)computed. Empty-diff
        // filters are stored with length 1 (a single zero byte) and so are
        // reused. Truncated-large filters are length 1 (0xff) and reused too.
        match layer.bloom_filter_slice(lex) {
            Some(s) if !s.is_empty() => Some(s.to_vec()),
            _ => None,
        }
    }

    /// Existing non-empty filter bytes for `oid` whose stored `hash_version`
    /// differs from `want.hash_version` but is otherwise compatible (same
    /// `num_hashes`/`bits_per_entry`). Used to detect filters that may be
    /// *upgraded* (relabeled to the new version without recomputation) when the
    /// changed paths contain no high-bit bytes.
    pub fn upgradable_filter_bytes(
        &self,
        oid: &ObjectId,
        want: &BloomFilterSettings,
    ) -> Option<Vec<u8>> {
        let (layer_idx, lex) = self.find_commit(oid)?;
        let layer = &self.layers[layer_idx];
        let settings = layer.bloom_settings.as_ref()?;
        if settings.hash_version == want.hash_version {
            return None;
        }
        if settings.num_hashes != want.num_hashes || settings.bits_per_entry != want.bits_per_entry
        {
            return None;
        }
        match layer.bloom_filter_slice(lex) {
            Some(s) if !s.is_empty() => Some(s.to_vec()),
            _ => None,
        }
    }

    /// Lexicographic position in the full chain, or `None` if not in any layer.
    pub fn find_commit(&self, oid: &ObjectId) -> Option<(usize, u32)> {
        for (i, layer) in self.layers.iter().enumerate() {
            if let Some(lex) = layer.bsearch_oid(oid) {
                return Some((i, lex));
            }
        }
        None
    }

    /// Global commit-graph position (Git `graph_pos`): base layers first, then newer layers.
    pub fn global_position(&self, oid: &ObjectId) -> Option<u32> {
        let (layer_idx, lex) = self.find_commit(oid)?;
        let below: u32 = self.layers[layer_idx + 1..]
            .iter()
            .map(|l| l.num_commits)
            .sum();
        Some(below + lex)
    }

    /// Inverse of [`Self::global_position`]: the OID at a base-first global
    /// position (used to resolve CDAT parent words to OIDs).
    fn oid_at_global_position(&self, pos: u32) -> Option<ObjectId> {
        // `layers` is tip-first; global positions count base layers first.
        let mut remaining = pos;
        for layer in self.layers.iter().rev() {
            if remaining < layer.num_commits {
                return layer.oid_at_lex(remaining);
            }
            remaining -= layer.num_commits;
        }
        None
    }

    /// Commit parents and committer time read straight from the commit-graph file
    /// (no object decompression). Returns `None` when `oid` is not in the graph,
    /// or is an octopus merge (>2 parents — stored in the EXTRA_EDGES chunk, which
    /// this reader does not decode); callers fall back to reading the object.
    pub fn graph_commit(&self, oid: &ObjectId) -> Option<(Vec<ObjectId>, i64)> {
        let (layer_idx, lex) = self.find_commit(oid)?;
        let (p1, p2, commit_time) = self.layers[layer_idx].cdat_record(lex)?;
        if p1 == GRAPH_PARENT_NONE {
            return Some((Vec::new(), commit_time));
        }
        let parent1 = self.oid_at_global_position(p1)?;
        if p2 == GRAPH_PARENT_NONE {
            return Some((vec![parent1], commit_time));
        }
        if p2 & GRAPH_EXTRA_EDGES_NEEDED != 0 {
            return None; // octopus merge — fall back to the object
        }
        let parent2 = self.oid_at_global_position(p2)?;
        Some((vec![parent1, parent2], commit_time))
    }

    /// All commit OIDs in the chain (oldest base first, then newer layers).
    pub fn all_oids_in_order(&self) -> Vec<ObjectId> {
        let mut out = Vec::new();
        for layer in self.layers.iter().rev() {
            for i in 0..layer.num_commits {
                if let Some(oid) = layer.oid_at_lex(i) {
                    out.push(oid);
                }
            }
        }
        out
    }

    /// Consult Bloom filters for a single-parent commit before diffing trees.
    pub fn bloom_precheck_for_paths(
        &self,
        _odb: &Odb,
        oid: ObjectId,
        pathspecs: &[String],
        bloom_cwd: Option<&str>,
        requested_hash_version: i32,
        read_changed_paths: bool,
    ) -> std::result::Result<BloomPrecheck, crate::error::Error> {
        if !read_changed_paths {
            return Ok(BloomPrecheck::Inapplicable);
        }
        let Some((layer_idx, lex)) = self.find_commit(&oid) else {
            return Ok(BloomPrecheck::NotInGraph);
        };
        let layer = &self.layers[layer_idx];
        let Some(settings) = layer.bloom_settings.as_ref() else {
            return Ok(BloomPrecheck::FilterNotPresent);
        };
        let effective_version = if requested_hash_version < 0 {
            settings.hash_version as i32
        } else {
            requested_hash_version
        };
        if effective_version != settings.hash_version as i32 {
            return Ok(BloomPrecheck::FilterNotPresent);
        }

        // Git computes the changed-path Bloom filter for every commit (including merges)
        // relative to its first parent, and `rev_compare_tree` consults it for the
        // first-parent comparison only (`nth_parent == 0`). The caller is responsible for
        // restricting the precheck to the first parent, so merge commits are handled here
        // exactly like single-parent commits.
        let filter = match layer.bloom_filter_slice(lex) {
            Some(s) => s,
            None => return Ok(BloomPrecheck::FilterNotPresent),
        };
        if filter.is_empty() {
            return Ok(BloomPrecheck::FilterNotPresent);
        }

        // Git `bloom_filter_contains_vec`: within one pathspec, every prefix key must match;
        // multiple pathspecs are ORed (`revision.c` loop over `bloom_keyvecs_nr`).
        let mut any_pathspec_maybe = false;
        let mut checked_any_keys = false;
        for spec in pathspecs {
            if spec.is_empty() || crate::pathspec::pathspec_is_exclude(spec) {
                continue;
            }
            let Some(norm) = crate::pathspec::bloom_lookup_prefix_with_cwd(spec, bloom_cwd) else {
                continue;
            };
            let keys = bloom_keyvec_for_path(norm.as_str(), settings);
            if keys.is_empty() {
                continue;
            }
            checked_any_keys = true;
            let mut all_keys_maybe = true;
            for key in &keys {
                match bloom_filter_contains(key, filter, settings) {
                    Ok(true) => {}
                    Ok(false) => {
                        all_keys_maybe = false;
                        break;
                    }
                    Err(()) => {
                        all_keys_maybe = true;
                        break;
                    }
                }
            }
            if all_keys_maybe {
                any_pathspec_maybe = true;
                break;
            }
        }
        if checked_any_keys && !any_pathspec_maybe {
            return Ok(BloomPrecheck::DefinitelyNot);
        }
        Ok(BloomPrecheck::Maybe)
    }
}

/// Compute changed paths between parent and commit trees (recursive diff, no rename detection).
pub fn diff_changed_paths_for_bloom(
    odb: &Odb,
    parent_tree: Option<ObjectId>,
    commit_tree: ObjectId,
) -> crate::error::Result<(Vec<String>, usize)> {
    use crate::diff::diff_trees;
    let entries = diff_trees(odb, parent_tree.as_ref(), Some(&commit_tree), "")?;
    let raw_len = entries.len();
    let mut paths = Vec::new();
    for e in entries {
        let p = e.path().to_string_lossy().into_owned();
        if !p.is_empty() {
            paths.push(p);
        }
    }
    Ok((paths, raw_len))
}

/// Re-export for `commit-graph` write.
pub use crate::bloom::collect_changed_paths_for_bloom;

/// Build Bloom filter bytes for one commit; returns cumulative size contribution for BIDX.
pub fn bloom_filter_for_commit_write(
    odb: &Odb,
    parents: &[ObjectId],
    tree_oid: ObjectId,
    settings: &BloomFilterSettings,
) -> crate::error::Result<(Vec<u8>, BloomBuildOutcome)> {
    // Git computes the changed-path filter against the *first* parent only,
    // regardless of how many parents a commit has (see bloom.c:
    // `diff_tree_oid(&c->parents->item->object.oid, ...)`).
    let (changed_paths_vec, raw_count) = if let Some(first_parent) = parents.first() {
        let p = load_commit_tree(odb, *first_parent)?;
        diff_changed_paths_for_bloom(odb, Some(p), tree_oid)?
    } else {
        diff_changed_paths_for_bloom(odb, None, tree_oid)?
    };
    let set = collect_changed_paths_for_bloom(&changed_paths_vec);
    Ok(crate::bloom::build_bloom_filter_data(
        &set, raw_count, settings,
    ))
}

fn load_commit_tree(odb: &Odb, commit_oid: ObjectId) -> crate::error::Result<ObjectId> {
    let obj = odb.read(&commit_oid)?;
    let c = crate::objects::parse_commit(&obj.data)?;
    Ok(c.tree)
}

/// Whether any path component in `tree` (recursively) contains a byte with the
/// high bit set (`& 0x80`). Mirrors `bloom.c:has_entries_with_high_bit`, used to
/// decide whether a v1 changed-path Bloom filter can be relabeled as v2 without
/// recomputation (v1/v2 hashing only differs for bytes >= 0x80).
fn tree_has_high_bit_paths(odb: &Odb, tree_oid: ObjectId) -> bool {
    let Ok(obj) = odb.read(&tree_oid) else {
        // Git treats an unreadable tree conservatively (no upgrade).
        return true;
    };
    let Ok(entries) = crate::objects::parse_tree(&obj.data) else {
        return true;
    };
    for e in &entries {
        if e.name.iter().any(|&b| b & 0x80 != 0) {
            return true;
        }
        if e.mode == 0o040000 && tree_has_high_bit_paths(odb, e.oid) {
            return true;
        }
    }
    false
}

/// Whether a commit's tree (recursively) has any high-bit path bytes
/// (`bloom.c:commit_tree_has_high_bit_paths`).
pub fn commit_tree_has_high_bit_paths(odb: &Odb, commit_oid: ObjectId) -> bool {
    match load_commit_tree(odb, commit_oid) {
        Ok(tree) => tree_has_high_bit_paths(odb, tree),
        Err(_) => true,
    }
}

/// Parse all chunks for `test-tool read-graph` / debugging.
pub fn parse_graph_file(path: &Path) -> Option<ParsedGraphDump> {
    let raw = std::fs::read(path).ok()?;
    if raw.len() < 28 {
        return None;
    }
    let hash_len = commit_graph_hash_len(raw[5])?;
    let body = &raw[..raw.len() - hash_len];
    if body.len() < 8 || &body[0..4] != SIGNATURE {
        return None;
    }
    let header_word = u32::from_be_bytes(body[0..4].try_into().ok()?);
    let num_chunks = body[6] as usize;
    let toc_start = 8;
    let mut present: std::collections::HashSet<u32> = std::collections::HashSet::new();
    for i in 0..num_chunks {
        let e = toc_start + i * 12;
        let id = u32::from_be_bytes(body[e..e + 4].try_into().ok()?);
        present.insert(id);
    }
    // `git/t/helper/test-read-graph.c` prints a fixed set of recognized chunks in
    // a fixed order, omitting the BASE chunk and any unknown chunk.
    let mut chunk_names: Vec<String> = Vec::new();
    for (id, label) in [
        (CHUNK_OID_FANOUT, "oid_fanout"),
        (CHUNK_OID_LOOKUP, "oid_lookup"),
        (CHUNK_COMMIT_DATA, "commit_metadata"),
        (CHUNK_GENERATION_DATA, "generation_data"),
        (CHUNK_GENERATION_DATA_OVERFLOW, "generation_data_overflow"),
        (CHUNK_EXTRA_EDGES, "extra_edges"),
        (CHUNK_BLOOM_INDEXES, "bloom_indexes"),
        (CHUNK_BLOOM_DATA, "bloom_data"),
    ] {
        if present.contains(&id) {
            chunk_names.push(label.to_string());
        }
    }
    let layer = CommitGraphLayer::parse(path.to_path_buf(), raw.clone())?;
    let bloom_opt = layer.bloom_settings.map(|s| {
        format!(
            " bloom({},{},{})",
            s.hash_version, s.bits_per_entry, s.num_hashes
        )
    });
    let mut options = String::new();
    if let Some(b) = bloom_opt {
        options.push_str(&b);
    }
    if layer.read_generation_data {
        options.push_str(" read_generation_data");
    }
    Some(ParsedGraphDump {
        header_word,
        version: body[4],
        hash_ver: body[5],
        num_chunks: body[6],
        reserved: body[7],
        num_commits: layer.num_commits,
        chunks: chunk_names.join(" "),
        options,
    })
}

pub struct ParsedGraphDump {
    pub header_word: u32,
    pub version: u8,
    pub hash_ver: u8,
    pub num_chunks: u8,
    pub reserved: u8,
    pub num_commits: u32,
    pub chunks: String,
    pub options: String,
}

/// Dump hex lines of Bloom filters (one per commit, empty line for empty filter).
pub fn dump_bloom_filters(path: &Path) -> Option<Vec<String>> {
    let raw = std::fs::read(path).ok()?;
    let layer = CommitGraphLayer::parse(path.to_path_buf(), raw)?;
    let mut out = Vec::new();
    for i in 0..layer.num_commits {
        let slice = layer.bloom_filter_slice(i).unwrap_or(&[]);
        if slice.is_empty() {
            out.push(String::new());
        } else {
            let hex: String = slice.iter().map(|b| format!("{b:02x}")).collect();
            out.push(hex);
        }
    }
    Some(out)
}

//! Serialize Git commit-graph v1 files with GDA2 + optional Bloom chunks (`commit-graph.c` compatible).

use std::collections::{HashMap, HashSet};
use std::io::Write;

use crate::bloom::{BloomBuildOutcome, BloomFilterSettings};
use crate::commit_graph_file::CommitGraphChain;
use crate::objects::{parse_commit, HashAlgo, ObjectId, ObjectKind};
use crate::odb::Odb;

const SIGNATURE: &[u8; 4] = b"CGPH";
const VERSION: u8 = 1;
const CHUNK_OID_FANOUT: u32 = 0x4f49_4446;
const CHUNK_OID_LOOKUP: u32 = 0x4f49_444c;
const CHUNK_COMMIT_DATA: u32 = 0x4344_4154;
const CHUNK_GENERATION_DATA: u32 = 0x4744_4132;
const CHUNK_GENERATION_DATA_OVERFLOW: u32 = 0x4744_4f32; // GDO2
const CHUNK_EXTRA_EDGES: u32 = 0x4544_4745;
const CHUNK_BLOOM_INDEXES: u32 = 0x4249_4458;
const CHUNK_BLOOM_DATA: u32 = 0x4244_4154;
const CHUNK_BASE: u32 = 0x4241_5345;

const PARENT_NONE: u32 = 0x7000_0000;
const GRAPH_EXTRA_EDGES_NEEDED: u32 = 0x8000_0000;
const GRAPH_LAST_EDGE: u32 = 0x8000_0000;

/// `GENERATION_NUMBER_V2_OFFSET_MAX` from Git `commit.h`.
const GENERATION_NUMBER_V2_OFFSET_MAX: u64 = (1u64 << 31) - 1;
/// `CORRECTED_COMMIT_DATE_OFFSET_OVERFLOW` from Git `commit-graph.c`.
const CORRECTED_COMMIT_DATE_OFFSET_OVERFLOW: u32 = 1u32 << 31;

/// Per-commit data needed to write CDAT / Bloom.
#[derive(Debug, Clone)]
pub struct CommitGraphCommitInfo {
    pub tree: ObjectId,
    pub parents: Vec<ObjectId>,
    /// Committer Unix timestamp (Git `timestamp_t`; may exceed `u32`).
    pub commit_time: u64,
}

/// Hash a commit-graph/multi-pack-index file body with the repository algorithm.
fn hash_file_body(body: &[u8], algo: HashAlgo) -> Vec<u8> {
    algo.digest(body).as_bytes().to_vec()
}

fn parse_commit_time(committer: &str) -> u64 {
    let parts: Vec<&str> = committer.rsplitn(3, ' ').collect();
    if parts.len() >= 2 {
        parts[1].parse::<u64>().unwrap_or(0)
    } else {
        0
    }
}

/// Load commit metadata from the ODB for graph writing.
pub fn load_commit_graph_commit_info(
    odb: &Odb,
    oid: ObjectId,
) -> crate::error::Result<CommitGraphCommitInfo> {
    let obj = odb.read(&oid)?;
    if obj.kind != ObjectKind::Commit {
        return Err(crate::error::Error::CorruptObject(format!(
            "object {oid} is not a commit"
        )));
    }
    let c = parse_commit(&obj.data)?;
    Ok(CommitGraphCommitInfo {
        tree: c.tree,
        parents: c.parents.clone(),
        commit_time: parse_commit_time(&c.committer),
    })
}

fn compute_topo_generations(
    sorted_oids: &[ObjectId],
    infos: &HashMap<ObjectId, CommitGraphCommitInfo>,
    oid_to_idx: &HashMap<ObjectId, u32>,
) -> Vec<u32> {
    let n = sorted_oids.len();
    let mut gen = vec![0u32; n];
    let mut computed = vec![false; n];
    for i in 0..n {
        if computed[i] {
            continue;
        }
        let mut work_stack: Vec<(usize, bool)> = vec![(i, false)];
        while let Some((idx, parents_done)) = work_stack.pop() {
            if computed[idx] {
                continue;
            }
            let oid = sorted_oids[idx];
            let info = &infos[&oid];
            if parents_done {
                let mut max_parent_gen = 0u32;
                for p in &info.parents {
                    if let Some(&pidx) = oid_to_idx.get(p) {
                        max_parent_gen = max_parent_gen.max(gen[pidx as usize]);
                    }
                }
                gen[idx] = max_parent_gen + 1;
                computed[idx] = true;
            } else {
                let mut all_done = true;
                for p in &info.parents {
                    if let Some(&pidx) = oid_to_idx.get(p) {
                        if !computed[pidx as usize] {
                            all_done = false;
                        }
                    }
                }
                if all_done {
                    let mut max_parent_gen = 0u32;
                    for p in &info.parents {
                        if let Some(&pidx) = oid_to_idx.get(p) {
                            max_parent_gen = max_parent_gen.max(gen[pidx as usize]);
                        }
                    }
                    gen[idx] = max_parent_gen + 1;
                    computed[idx] = true;
                } else {
                    work_stack.push((idx, true));
                    for p in &info.parents {
                        if let Some(&pidx) = oid_to_idx.get(p) {
                            if !computed[pidx as usize] {
                                work_stack.push((pidx as usize, false));
                            }
                        }
                    }
                }
            }
        }
    }
    gen
}

fn compute_corrected_generations(
    sorted_oids: &[ObjectId],
    infos: &HashMap<ObjectId, CommitGraphCommitInfo>,
    oid_to_idx: &HashMap<ObjectId, u32>,
    topo_gen: &[u32],
) -> Vec<u64> {
    let n = sorted_oids.len();
    let mut gen_date = vec![0u64; n];
    let mut computed = vec![false; n];
    for i in 0..n {
        if computed[i] {
            continue;
        }
        let mut work_stack: Vec<(usize, bool)> = vec![(i, false)];
        while let Some((idx, parents_done)) = work_stack.pop() {
            if computed[idx] {
                continue;
            }
            let oid = sorted_oids[idx];
            let info = &infos[&oid];
            let cdate = info.commit_time;
            if parents_done {
                let mut max_g = cdate;
                for p in &info.parents {
                    if let Some(&pidx) = oid_to_idx.get(p) {
                        max_g = max_g.max(gen_date[pidx as usize]);
                    }
                }
                let topo = topo_gen[idx] as u64;
                if max_g < topo {
                    max_g = topo;
                }
                gen_date[idx] = max_g + 1;
                computed[idx] = true;
            } else {
                let mut all_done = true;
                for p in &info.parents {
                    if let Some(&pidx) = oid_to_idx.get(p) {
                        if !computed[pidx as usize] {
                            all_done = false;
                        }
                    }
                }
                if all_done {
                    let mut max_g = cdate;
                    for p in &info.parents {
                        if let Some(&pidx) = oid_to_idx.get(p) {
                            max_g = max_g.max(gen_date[pidx as usize]);
                        }
                    }
                    let topo = topo_gen[idx] as u64;
                    if max_g < topo {
                        max_g = topo;
                    }
                    gen_date[idx] = max_g + 1;
                    computed[idx] = true;
                } else {
                    work_stack.push((idx, true));
                    for p in &info.parents {
                        if let Some(&pidx) = oid_to_idx.get(p) {
                            if !computed[pidx as usize] {
                                work_stack.push((pidx as usize, false));
                            }
                        }
                    }
                }
            }
        }
    }
    gen_date
}

fn resolve_parent_edge(
    parent: ObjectId,
    oid_to_idx: &HashMap<ObjectId, u32>,
    base_count: u32,
    chain: Option<&CommitGraphChain>,
) -> u32 {
    if let Some(&idx) = oid_to_idx.get(&parent) {
        return idx + base_count;
    }
    if let Some(c) = chain {
        if let Some(gpos) = c.global_position(&parent) {
            return gpos;
        }
    }
    PARENT_NONE
}

/// Counters emitted as `GIT_TRACE2_EVENT` for Bloom generation (`commit-graph.c`).
#[derive(Debug, Default, Clone, Copy)]
pub struct BloomWriteStats {
    pub filter_computed: u32,
    pub filter_not_computed: u32,
    pub filter_trunc_empty: u32,
    pub filter_trunc_large: u32,
    pub filter_upgraded: u32,
}
#[expect(clippy::too_many_arguments)]
/// Build raw commit-graph bytes (without touching the filesystem).
pub fn build_commit_graph_bytes(
    sorted_oids: &[ObjectId],
    infos: &HashMap<ObjectId, CommitGraphCommitInfo>,
    odb: &Odb,
    changed_paths: bool,
    bloom_settings: &BloomFilterSettings,
    base_chain: Option<&CommitGraphChain>,
    base_graph_hashes: &[&[u8]],
    max_new_filters: Option<u32>,
    existing_filters: &HashMap<ObjectId, Vec<u8>>,
    upgraded_filters: &HashMap<ObjectId, Vec<u8>>,
    write_generation_data: bool,
) -> crate::error::Result<(Vec<u8>, BloomWriteStats)> {
    let base_count: u32 = base_chain.map(CommitGraphChain::total_commits).unwrap_or(0);
    let algo = odb.hash_algo();
    let hash_len = algo.len();

    let oid_to_idx: HashMap<ObjectId, u32> = sorted_oids
        .iter()
        .enumerate()
        .map(|(i, o)| (*o, i as u32))
        .collect();

    let topo = compute_topo_generations(sorted_oids, infos, &oid_to_idx);
    let gen_date = compute_corrected_generations(sorted_oids, infos, &oid_to_idx, &topo);

    let mut gda2: Vec<u8> = Vec::with_capacity(sorted_oids.len() * 4);
    let mut generation_overflow: Vec<u8> = Vec::new();
    let mut overflow_count: u32 = 0;
    for (i, oid) in sorted_oids.iter().enumerate() {
        let info = &infos[oid];
        let offset_raw = gen_date[i].saturating_sub(info.commit_time);
        if offset_raw > GENERATION_NUMBER_V2_OFFSET_MAX {
            let marker = CORRECTED_COMMIT_DATE_OFFSET_OVERFLOW | overflow_count;
            overflow_count = overflow_count.wrapping_add(1);
            gda2.extend_from_slice(&marker.to_be_bytes());
            generation_overflow.extend_from_slice(&((offset_raw >> 32) as u32).to_be_bytes());
            generation_overflow.extend_from_slice(&((offset_raw as u32).to_be_bytes()));
        } else {
            gda2.extend_from_slice(&(offset_raw as u32).to_be_bytes());
        }
    }

    let mut extra_edges: Vec<u8> = Vec::new();

    let mut cdat: Vec<u8> = Vec::with_capacity(sorted_oids.len() * (hash_len + 16));
    for (i, oid) in sorted_oids.iter().enumerate() {
        let info = &infos[oid];
        cdat.extend_from_slice(info.tree.as_bytes());

        let p1 = info
            .parents
            .first()
            .map(|p| resolve_parent_edge(*p, &oid_to_idx, base_count, base_chain))
            .unwrap_or(PARENT_NONE);
        cdat.extend_from_slice(&p1.to_be_bytes());

        let p2 = if info.parents.len() <= 1 {
            PARENT_NONE
        } else if info.parents.len() == 2 {
            resolve_parent_edge(info.parents[1], &oid_to_idx, base_count, base_chain)
        } else {
            let start_u32 = (extra_edges.len() / 4) as u32;
            for (j, p) in info.parents.iter().enumerate().skip(1) {
                let mut ev = resolve_parent_edge(*p, &oid_to_idx, base_count, base_chain);
                if j + 1 == info.parents.len() {
                    ev |= GRAPH_LAST_EDGE;
                }
                extra_edges.extend_from_slice(&ev.to_be_bytes());
            }
            GRAPH_EXTRA_EDGES_NEEDED | start_u32
        };
        cdat.extend_from_slice(&p2.to_be_bytes());

        let topo = topo[i];
        let date = info.commit_time;
        let packed = (topo << 2) | (((date >> 32) & 0x3) as u32);
        cdat.extend_from_slice(&packed.to_be_bytes());
        cdat.extend_from_slice(&((date & 0xFFFF_FFFF) as u32).to_be_bytes());
    }

    let mut fanout = vec![0u8; 256 * 4];
    let mut counts = [0u32; 256];
    for oid in sorted_oids {
        counts[oid.as_bytes()[0] as usize] += 1;
    }
    let mut cum = 0u32;
    for i in 0..256 {
        cum += counts[i];
        fanout[i * 4..i * 4 + 4].copy_from_slice(&cum.to_be_bytes());
    }

    let mut oid_lookup = Vec::with_capacity(sorted_oids.len() * hash_len);
    for oid in sorted_oids {
        oid_lookup.extend_from_slice(oid.as_bytes());
    }

    let mut bloom_stats = BloomWriteStats::default();
    let max_new = max_new_filters.unwrap_or(u32::MAX);
    let (bidx, bdat, bloom_total_payload) = if changed_paths {
        let mut indexes: Vec<u32> = Vec::with_capacity(sorted_oids.len());
        let mut data_payload = Vec::new();
        let mut cur = 0u32;
        for oid in sorted_oids {
            let info = &infos[oid];
            // Reuse a filter already present (in a compatible layer) for this commit
            // instead of recomputing it. Git counts these as `filter_not_computed`.
            if let Some(existing) = existing_filters.get(oid) {
                bloom_stats.filter_not_computed += 1;
                cur += existing.len() as u32;
                indexes.push(cur);
                data_payload.extend_from_slice(existing);
                continue;
            }
            // A filter present under a different (compatible) version that we can
            // relabel without recomputation: the on-disk bytes are reused as-is
            // and Git counts it as `filter-upgraded`.
            if let Some(upgraded) = upgraded_filters.get(oid) {
                bloom_stats.filter_upgraded += 1;
                cur += upgraded.len() as u32;
                indexes.push(cur);
                data_payload.extend_from_slice(upgraded);
                continue;
            }
            let compute = bloom_stats.filter_computed < max_new;
            let (bytes, outcome) = if compute {
                crate::commit_graph_file::bloom_filter_for_commit_write(
                    odb,
                    &info.parents,
                    info.tree,
                    bloom_settings,
                )?
            } else {
                (Vec::new(), BloomBuildOutcome::Normal)
            };
            if compute {
                bloom_stats.filter_computed += 1;
                match outcome {
                    BloomBuildOutcome::Normal => {}
                    BloomBuildOutcome::TruncatedLarge => bloom_stats.filter_trunc_large += 1,
                    BloomBuildOutcome::TruncatedEmpty => bloom_stats.filter_trunc_empty += 1,
                }
            } else {
                bloom_stats.filter_not_computed += 1;
            }
            cur += bytes.len() as u32;
            indexes.push(cur);
            data_payload.extend_from_slice(&bytes);
        }
        let mut bdat_chunk = Vec::with_capacity(12 + data_payload.len());
        bdat_chunk.extend_from_slice(&bloom_settings.hash_version.to_be_bytes());
        bdat_chunk.extend_from_slice(&bloom_settings.num_hashes.to_be_bytes());
        bdat_chunk.extend_from_slice(&bloom_settings.bits_per_entry.to_be_bytes());
        bdat_chunk.extend_from_slice(&data_payload);
        let mut bidx_bytes = Vec::with_capacity(indexes.len() * 4);
        for v in indexes {
            bidx_bytes.extend_from_slice(&v.to_be_bytes());
        }
        (bidx_bytes, bdat_chunk, data_payload.len())
    } else {
        (Vec::new(), Vec::new(), 0)
    };

    let _ = bloom_total_payload;

    let mut chunks: Vec<(u32, Vec<u8>)> = Vec::new();
    chunks.push((CHUNK_OID_FANOUT, fanout));
    chunks.push((CHUNK_OID_LOOKUP, oid_lookup));
    chunks.push((CHUNK_COMMIT_DATA, cdat));
    if write_generation_data {
        chunks.push((CHUNK_GENERATION_DATA, gda2));
        if !generation_overflow.is_empty() {
            chunks.push((CHUNK_GENERATION_DATA_OVERFLOW, generation_overflow));
        }
    }
    if !extra_edges.is_empty() {
        chunks.push((CHUNK_EXTRA_EDGES, extra_edges));
    }
    if changed_paths {
        chunks.push((CHUNK_BLOOM_INDEXES, bidx));
        chunks.push((CHUNK_BLOOM_DATA, bdat));
    }
    if !base_graph_hashes.is_empty() {
        let mut base_chunk = Vec::new();
        for h in base_graph_hashes {
            base_chunk.extend_from_slice(h);
        }
        chunks.push((CHUNK_BASE, base_chunk));
    }

    let num_chunks = chunks.len() as u8;
    let header_size = 8u64;
    let toc_size = (num_chunks as u64 + 1) * 12;
    let mut offsets = Vec::with_capacity(chunks.len());
    let mut cur = header_size + toc_size;
    for (_, data) in &chunks {
        offsets.push(cur);
        cur += data.len() as u64;
    }
    let end_offset = cur;

    // The commit-graph hash version and trailing checksum follow the repository
    // hash algorithm (SHA-1 → version 1, SHA-256 → version 2).
    let mut out = Vec::with_capacity(end_offset as usize + algo.len());
    out.write_all(SIGNATURE)?;
    let base_layers = base_graph_hashes.len() as u8;
    out.write_all(&[VERSION, algo.oid_version(), num_chunks, base_layers])?;
    for i in 0..chunks.len() {
        out.write_all(&chunks[i].0.to_be_bytes())?;
        out.write_all(&offsets[i].to_be_bytes())?;
    }
    out.write_all(&[0u8; 4])?;
    out.write_all(&end_offset.to_be_bytes())?;
    for (_, data) in &chunks {
        out.write_all(data)?;
    }

    let checksum = hash_file_body(&out, algo);
    out.write_all(&checksum)?;
    Ok((out, bloom_stats))
}

/// Collect reachable commit OIDs from ref tips (same strategy as existing grit commit-graph).
pub fn collect_reachable_commit_oids(
    git_dir: &std::path::Path,
    odb: &Odb,
) -> crate::error::Result<HashSet<ObjectId>> {
    use std::fs;
    let mut commits: HashSet<ObjectId> = HashSet::new();
    let mut stack: Vec<ObjectId> = Vec::new();

    fn collect_ref_tips(
        dir: &std::path::Path,
        stack: &mut Vec<ObjectId>,
    ) -> crate::error::Result<()> {
        if !dir.exists() {
            return Ok(());
        }
        for entry in fs::read_dir(dir)? {
            let entry = entry?;
            let path = entry.path();
            if path.is_dir() {
                collect_ref_tips(&path, stack)?;
            } else if let Ok(content) = fs::read_to_string(&path) {
                if let Ok(oid) = ObjectId::from_hex(content.trim()) {
                    stack.push(oid);
                }
            }
        }
        Ok(())
    }

    let refs_dir = git_dir.join("refs");
    collect_ref_tips(&refs_dir, &mut stack)?;

    let packed_refs = git_dir.join("packed-refs");
    if packed_refs.exists() {
        if let Ok(content) = fs::read_to_string(&packed_refs) {
            for line in content.lines() {
                if line.starts_with('#') || line.starts_with('^') {
                    continue;
                }
                if let Some(hex) = line.split_whitespace().next() {
                    if let Ok(oid) = ObjectId::from_hex(hex) {
                        stack.push(oid);
                    }
                }
            }
        }
    }

    let head_path = git_dir.join("HEAD");
    if head_path.exists() {
        let head = fs::read_to_string(&head_path)?;
        let head = head.trim();
        if let Some(refpath) = head.strip_prefix("ref: ") {
            let full = git_dir.join(refpath);
            if full.exists() {
                if let Ok(content) = fs::read_to_string(&full) {
                    if let Ok(oid) = ObjectId::from_hex(content.trim()) {
                        stack.push(oid);
                    }
                }
            }
        } else if let Ok(oid) = ObjectId::from_hex(head) {
            stack.push(oid);
        }
    }

    while let Some(oid) = stack.pop() {
        if commits.contains(&oid) {
            continue;
        }
        let obj = match odb.read(&oid) {
            Ok(o) => o,
            Err(_) => continue,
        };
        if obj.kind != ObjectKind::Commit {
            if obj.kind == ObjectKind::Tag {
                if let Ok(text) = std::str::from_utf8(&obj.data) {
                    for line in text.lines() {
                        if let Some(rest) = line.strip_prefix("object ") {
                            if let Ok(target) = ObjectId::from_hex(rest.trim()) {
                                stack.push(target);
                            }
                        }
                    }
                }
            }
            continue;
        }
        let commit = parse_commit(&obj.data)?;
        for parent in &commit.parents {
            stack.push(*parent);
        }
        commits.insert(oid);
    }

    Ok(commits)
}

/// Count unique commit OIDs that refs point to directly (peeling annotated tags), matching Git's
/// `add_ref_to_set` accounting for the "Collecting referenced commits" progress meter. Unlike
/// [`collect_reachable_commit_oids`], this does not walk commit parents.
pub fn count_referenced_commit_tips(
    git_dir: &std::path::Path,
    odb: &Odb,
) -> crate::error::Result<usize> {
    use std::fs;
    let mut tips: Vec<ObjectId> = Vec::new();

    fn collect_ref_tips(
        dir: &std::path::Path,
        tips: &mut Vec<ObjectId>,
    ) -> crate::error::Result<()> {
        if !dir.exists() {
            return Ok(());
        }
        for entry in fs::read_dir(dir)? {
            let entry = entry?;
            let path = entry.path();
            if path.is_dir() {
                collect_ref_tips(&path, tips)?;
            } else if let Ok(content) = fs::read_to_string(&path) {
                if let Ok(oid) = ObjectId::from_hex(content.trim()) {
                    tips.push(oid);
                }
            }
        }
        Ok(())
    }

    collect_ref_tips(&git_dir.join("refs"), &mut tips)?;

    let packed_refs = git_dir.join("packed-refs");
    if packed_refs.exists() {
        if let Ok(content) = fs::read_to_string(&packed_refs) {
            for line in content.lines() {
                if line.starts_with('#') || line.starts_with('^') {
                    continue;
                }
                if let Some(hex) = line.split_whitespace().next() {
                    if let Ok(oid) = ObjectId::from_hex(hex) {
                        tips.push(oid);
                    }
                }
            }
        }
    }

    // Peel each ref tip to the commit it ultimately references (an annotated tag points at a
    // commit) and collect distinct commit OIDs. Non-commit tips (e.g. tags pointing at trees)
    // are ignored, exactly like Git's OBJ_COMMIT check.
    let mut commits: HashSet<ObjectId> = HashSet::new();
    for tip in tips {
        if let Some(commit_oid) = peel_to_commit(odb, tip) {
            commits.insert(commit_oid);
        }
    }
    Ok(commits.len())
}

/// Peel `oid` through annotated tags until a commit is reached. Returns `None` if it does not
/// resolve to a commit.
fn peel_to_commit(odb: &Odb, oid: ObjectId) -> Option<ObjectId> {
    let mut current = oid;
    for _ in 0..16 {
        let obj = odb.read(&current).ok()?;
        match obj.kind {
            ObjectKind::Commit => return Some(current),
            ObjectKind::Tag => {
                let text = std::str::from_utf8(&obj.data).ok()?;
                let target = text
                    .lines()
                    .find_map(|line| line.strip_prefix("object "))
                    .and_then(|rest| ObjectId::from_hex(rest.trim()).ok())?;
                current = target;
            }
            _ => return None,
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use crate::bloom::BloomFilterSettings;
    use crate::index::{Index, IndexEntry, MODE_REGULAR};
    use crate::objects::{serialize_commit, CommitData, ObjectId, ObjectKind};
    use crate::repo::{init_repository, Repository};
    use crate::write_tree::write_tree_from_index;

    fn empty_commit(repo: &Repository, parents: Vec<ObjectId>, committer: &str) -> ObjectId {
        let raw = serialize_commit(&CommitData {
            tree: repo.odb.write(ObjectKind::Tree, b"").expect("tree"),
            parents,
            author: committer.into(),
            committer: committer.into(),
            author_raw: Vec::new(),
            committer_raw: Vec::new(),
            encoding: None,
            message: "m\n".into(),
            raw_message: None,
        });
        repo.odb.write(ObjectKind::Commit, &raw).expect("commit")
    }

    #[test]
    fn load_commit_graph_commit_info_rejects_non_commit() {
        let dir = tempfile::tempdir().expect("tempdir");
        let repo = init_repository(dir.path(), false, "main", None, "files").expect("init");
        let blob = repo.odb.write(ObjectKind::Blob, b"x").expect("blob");
        let err = super::load_commit_graph_commit_info(&repo.odb, blob).unwrap_err();
        assert!(matches!(err, crate::error::Error::CorruptObject(_)));
    }

    #[test]
    fn build_octopus_merge_includes_extra_edges_chunk() {
        let dir = tempfile::tempdir().expect("tempdir");
        let repo = init_repository(dir.path(), false, "main", None, "files").expect("init");
        let p1 = empty_commit(&repo, vec![], "T <t@example.com> 1 +0000");
        let p2 = empty_commit(&repo, vec![], "T <t@example.com> 2 +0000");
        let p3 = empty_commit(&repo, vec![], "T <t@example.com> 3 +0000");
        let merge = empty_commit(&repo, vec![p1, p2, p3], "T <t@example.com> 4 +0000");
        let mut sorted = vec![p1, p2, p3, merge];
        sorted.sort();
        let mut infos = HashMap::new();
        for oid in &sorted {
            infos.insert(
                *oid,
                super::load_commit_graph_commit_info(&repo.odb, *oid).expect("info"),
            );
        }
        let bloom = BloomFilterSettings::default();
        let (bytes, _) = super::build_commit_graph_bytes(
            &sorted,
            &infos,
            &repo.odb,
            false,
            &bloom,
            None,
            &[],
            None,
            &HashMap::new(),
            &HashMap::new(),
            true,
        )
        .expect("write");
        assert!(bytes.windows(4).any(|w| w == b"EDGE"));
        crate::commit_graph_file::CommitGraphLayer::try_parse(std::path::PathBuf::from("g"), bytes)
            .expect("parse octopus graph");
    }

    #[test]
    fn build_respects_max_new_filters_budget() {
        let dir = tempfile::tempdir().expect("tempdir");
        let repo = init_repository(dir.path(), false, "main", None, "files").expect("init");
        let mut index = Index::new();
        index.hash_algo = repo.odb.hash_algo();
        let blob = repo.odb.write(ObjectKind::Blob, b"a\n").expect("blob");
        index.add_or_replace(IndexEntry {
            ctime_sec: 0,
            ctime_nsec: 0,
            mtime_sec: 0,
            mtime_nsec: 0,
            dev: 0,
            ino: 0,
            mode: MODE_REGULAR,
            uid: 0,
            gid: 0,
            size: 2,
            oid: blob,
            flags: 4,
            flags_extended: None,
            path: b"a.txt".to_vec(),
            base_index_pos: 0,
        });
        let tree = write_tree_from_index(&repo.odb, &index, "").expect("tree");
        let mut sorted = Vec::new();
        let mut infos = HashMap::new();
        for t in [1i64, 2, 3] {
            let raw = serialize_commit(&CommitData {
                tree,
                parents: sorted.last().copied().map(|p| vec![p]).unwrap_or_default(),
                author: format!("T <t@example.com> {t} +0000"),
                committer: format!("T <t@example.com> {t} +0000"),
                author_raw: Vec::new(),
                committer_raw: Vec::new(),
                encoding: None,
                message: "m\n".into(),
                raw_message: None,
            });
            let c = repo.odb.write(ObjectKind::Commit, &raw).expect("commit");
            infos.insert(
                c,
                super::load_commit_graph_commit_info(&repo.odb, c).expect("info"),
            );
            sorted.push(c);
        }
        let bloom = BloomFilterSettings::default();
        let (_, stats) = super::build_commit_graph_bytes(
            &sorted,
            &infos,
            &repo.odb,
            true,
            &bloom,
            None,
            &[],
            Some(1),
            &HashMap::new(),
            &HashMap::new(),
            true,
        )
        .expect("write");
        assert!(stats.filter_computed <= 1);
        assert!(stats.filter_not_computed >= 1);
    }

    #[test]
    fn collect_reachable_follows_annotated_tag_to_commit() {
        let dir = tempfile::tempdir().expect("tempdir");
        let repo = init_repository(dir.path(), false, "main", None, "files").expect("init");
        let commit = empty_commit(&repo, vec![], "T <t@example.com> 1 +0000");
        let tag_body = format!(
            "object {}\ntype commit\ntag v1\ntagger T <t@example.com> 1 +0000\n\n",
            commit
        );
        let tag = repo
            .odb
            .write(ObjectKind::Tag, tag_body.as_bytes())
            .expect("tag");
        crate::refs::write_ref(&repo.git_dir, "refs/tags/v1", &tag).expect("ref");
        let set = super::collect_reachable_commit_oids(&repo.git_dir, &repo.odb).expect("walk");
        assert!(set.contains(&commit));
        let tips = super::count_referenced_commit_tips(&repo.git_dir, &repo.odb).expect("tips");
        assert_eq!(tips, 1);
    }
}

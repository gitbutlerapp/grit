//! Delta islands — restrict cross-island deltas in `pack-objects` (`--delta-islands`).
//!
//! Port of Git's `delta-islands.c`. Islands group objects by the refs whose history
//! reaches them. Each object gets a bitmap of the islands it belongs to. A delta from
//! `trg` onto base `src` is only allowed when `trg`'s island set is a subset of `src`'s
//! (`in_same_island`), so a packed delta never forces a client that only fetched one
//! island to also download objects from another. `island_delta_cmp` additionally biases
//! the "which object is the preferred base" ordering so that objects living in a superset
//! of islands are preferred as bases.
//!
//! Islands are derived from the `pack.island` regexes (matched against full ref names,
//! left-anchored) and the optional `pack.islandcore` name (whose objects are written
//! first in the pack via layering).

use crate::config::ConfigSet;
use crate::objects::{parse_commit, parse_tag, parse_tree, ObjectId, ObjectKind};
use crate::repo::Repository;
use std::collections::HashMap;

/// One island membership bitmap (one bit per deduplicated island).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct IslandBitmap {
    bits: Vec<u32>,
}

impl IslandBitmap {
    fn new(size: usize) -> Self {
        IslandBitmap {
            bits: vec![0u32; size],
        }
    }

    fn set(&mut self, i: u32) {
        let block = (i / 32) as usize;
        let mask = 1u32 << (i % 32);
        self.bits[block] |= mask;
    }

    fn get(&self, i: u32) -> bool {
        let block = (i / 32) as usize;
        let mask = 1u32 << (i % 32);
        (self.bits[block] & mask) != 0
    }

    fn or(&mut self, other: &IslandBitmap) {
        for (a, b) in self.bits.iter_mut().zip(other.bits.iter()) {
            *a |= *b;
        }
    }

    /// `self` ⊆ `super_`: every island bit set in `self` is also set in `super_`.
    fn is_subset(&self, super_: &IslandBitmap) -> bool {
        for (s, p) in self.bits.iter().zip(super_.bits.iter()) {
            if (s & p) != *s {
                return false;
            }
        }
        true
    }

    fn is_empty(&self) -> bool {
        self.bits.iter().all(|&b| b == 0)
    }
}

/// Computed island marks for a `pack-objects` run.
#[derive(Debug, Default)]
pub struct DeltaIslands {
    /// Per-object island membership.
    marks: HashMap<ObjectId, IslandBitmap>,
    /// Island bit assigned to the core island (`pack.islandcore`), if any.
    core_island_bit: Option<u32>,
    /// Whether any island was actually configured/loaded.
    active: bool,
}

/// A raw island collected from refs, before deduplication.
struct RemoteIsland {
    hash: u64,
    oids: Vec<ObjectId>,
}

impl DeltaIslands {
    /// True when islands are in effect (at least one `pack.island` regex matched a ref).
    #[must_use]
    pub fn is_active(&self) -> bool {
        self.active
    }

    /// Whether `trg` may be delta-compressed against base `src` under island rules.
    ///
    /// Mirrors `in_same_island`: objects with no bitmap can delta against anything (target)
    /// but must not be used as a base (source). Otherwise the target's island set must be a
    /// subset of the source's.
    #[must_use]
    pub fn in_same_island(&self, trg: &ObjectId, src: &ObjectId) -> bool {
        if !self.active {
            return true;
        }
        let Some(trg_marks) = self.marks.get(trg) else {
            // Target isn't important — allow delta against anything.
            return true;
        };
        let Some(src_marks) = self.marks.get(src) else {
            // Base has no island — never base a delta on it.
            return false;
        };
        trg_marks.is_subset(src_marks)
    }

    /// Ordering bias for choosing a delta base (`island_delta_cmp`).
    ///
    /// Returns `-1` when `a` should sort before `b` (preferred as a base because its island
    /// set is a superset), `1` when `b` should, `0` when neither dominates.
    #[must_use]
    pub fn delta_cmp(&self, a: &ObjectId, b: &ObjectId) -> i32 {
        if !self.active {
            return 0;
        }
        let a_marks = self.marks.get(a);
        let b_marks = self.marks.get(b);

        if let Some(am) = a_marks {
            if b_marks.is_none_or(|bm| !am.is_subset(bm)) {
                return -1;
            }
        }
        if let Some(bm) = b_marks {
            if a_marks.is_none_or(|am| !bm.is_subset(am)) {
                return 1;
            }
        }
        0
    }

    /// Whether `oid` belongs to the core island (used for `pack.islandcore` layering).
    #[must_use]
    pub fn is_core_object(&self, oid: &ObjectId) -> bool {
        let Some(bit) = self.core_island_bit else {
            return false;
        };
        self.marks.get(oid).is_some_and(|m| m.get(bit))
    }

    /// Whether a core island is configured (`pack.islandcore`).
    #[must_use]
    pub fn has_core(&self) -> bool {
        self.core_island_bit.is_some()
    }
}

/// Compile the left-anchored island regexes from `pack.island` config (in load order).
fn load_island_regexes(cfg: &ConfigSet) -> Vec<regex::Regex> {
    cfg.get_all("pack.island")
        .into_iter()
        .filter_map(|v| {
            let pat = if v.starts_with('^') {
                v
            } else {
                format!("^{v}")
            };
            regex::Regex::new(&pat).ok()
        })
        .collect()
}

/// Build the island name for a ref: pick the last regex that matches (last-one-wins) and
/// join its non-empty capture groups with `-`. Returns `None` if no regex matches.
fn island_name_for_ref(regexes: &[regex::Regex], ref_name: &str) -> Option<String> {
    // Walk backwards for last-one-wins ordering.
    for rx in regexes.iter().rev() {
        if let Some(caps) = rx.captures(ref_name) {
            let mut name = String::new();
            for m in caps.iter().skip(1).flatten() {
                if m.as_str().is_empty() {
                    continue;
                }
                if !name.is_empty() {
                    name.push('-');
                }
                name.push_str(m.as_str());
            }
            return Some(name);
        }
    }
    None
}

/// Load and compute delta-island marks for the objects in `packed`.
///
/// `packed` is the set of object ids being written, used to bound propagation to objects in
/// this pack (matching Git, which only marks objects in `to_pack`). Returns an inactive
/// [`DeltaIslands`] when no `pack.island` regex matches any ref.
pub fn load_delta_islands(
    repo: &Repository,
    cfg: &ConfigSet,
    packed: &std::collections::HashSet<ObjectId>,
) -> DeltaIslands {
    let regexes = load_island_regexes(cfg);
    if regexes.is_empty() {
        return DeltaIslands::default();
    }
    let core_name = cfg.get("pack.islandcore");

    // Collect refs and assign each to an island by name.
    let mut by_name: HashMap<String, RemoteIsland> = HashMap::new();
    let refs = crate::refs::list_refs(&repo.git_dir, "refs/").unwrap_or_default();
    for (ref_name, oid) in &refs {
        let Some(name) = island_name_for_ref(&regexes, ref_name) else {
            continue;
        };
        let entry = by_name.entry(name).or_insert_with(|| RemoteIsland {
            hash: 0,
            oids: Vec::new(),
        });
        entry.oids.push(*oid);
        // Hash = running sum of the first 8 bytes of each oid (little-endian, as Git memcpy's
        // the leading hash bytes into a uint64_t).
        let b = oid.as_bytes();
        let core = u64::from_le_bytes([b[0], b[1], b[2], b[3], b[4], b[5], b[6], b[7]]);
        entry.hash = entry.hash.wrapping_add(core);
    }

    if by_name.is_empty() {
        return DeltaIslands::default();
    }

    // Resolve which island name is the core (looked up by name before dedup).
    let core_hash = core_name
        .as_deref()
        .and_then(|n| by_name.get(n))
        .map(|ri| ri.hash);

    // Deduplicate islands sharing the same hash (Git keeps the first occurrence). The
    // surviving order is the iteration order over the island list.
    let mut islands: Vec<RemoteIsland> = by_name.into_values().collect();
    // Stable order for determinism (Git's order is hash-map iteration; result is the same
    // because dedup only drops exact-hash duplicates and marking is order-independent).
    islands.sort_by_key(|a| a.hash);
    let mut deduped: Vec<RemoteIsland> = Vec::new();
    for isl in islands {
        if deduped.iter().any(|d| d.hash == isl.hash) {
            continue;
        }
        deduped.push(isl);
    }

    let island_count = deduped.len();
    let bitmap_size = island_count / 32 + 1;

    let mut islands_obj = DeltaIslands {
        marks: HashMap::new(),
        core_island_bit: None,
        active: false,
    };

    // Mark each surviving island's ref tips with its bit.
    for (bit, isl) in deduped.iter().enumerate() {
        let bit = bit as u32;
        if core_hash == Some(isl.hash) {
            islands_obj.core_island_bit = Some(bit);
        }
        for oid in &isl.oids {
            let marks = islands_obj
                .marks
                .entry(*oid)
                .or_insert_with(|| IslandBitmap::new(bitmap_size));
            marks.set(bit);
        }
    }

    islands_obj.active = true;

    // Propagate marks across the object graph: commits -> tree + parents, then trees -> entries.
    propagate_marks(repo, &mut islands_obj, packed, bitmap_size);

    islands_obj
}

/// Helper to OR `marks` into the object's bitmap (`set_island_marks`).
fn add_marks(
    table: &mut HashMap<ObjectId, IslandBitmap>,
    oid: &ObjectId,
    marks: &IslandBitmap,
    bitmap_size: usize,
) {
    let entry = table
        .entry(*oid)
        .or_insert_with(|| IslandBitmap::new(bitmap_size));
    entry.or(marks);
}

/// Propagate island marks from ref tips down through commits, trees, and blobs.
fn propagate_marks(
    repo: &Repository,
    islands: &mut DeltaIslands,
    packed: &std::collections::HashSet<ObjectId>,
    bitmap_size: usize,
) {
    // First, follow tags down to their target object (mirrors mark_remote_island_1's tag loop).
    let tag_oids: Vec<ObjectId> = islands
        .marks
        .keys()
        .copied()
        .filter(|oid| matches!(repo.odb.read(oid).map(|o| o.kind), Ok(ObjectKind::Tag)))
        .collect();
    for tag_oid in tag_oids {
        let marks = islands.marks.get(&tag_oid).cloned();
        let Some(marks) = marks else { continue };
        let mut cur = tag_oid;
        while let Ok(obj) = repo.odb.read(&cur) {
            if obj.kind != ObjectKind::Tag {
                break;
            }
            let Ok(tag) = parse_tag(&obj.data) else { break };
            add_marks(&mut islands.marks, &tag.object, &marks, bitmap_size);
            cur = tag.object;
        }
    }

    // Commits: process all commits that carry island marks, propagating to their tree and
    // parents (commit-graph order is irrelevant — marks are unioned, so order-independent).
    // We iterate to a fixpoint over the commit ancestry so deeper history (shared roots)
    // accumulates the union of every descendant island.
    let mut commit_marks: Vec<(ObjectId, IslandBitmap)> = islands
        .marks
        .iter()
        .filter(|(oid, _)| matches!(repo.odb.read(oid).map(|o| o.kind), Ok(ObjectKind::Commit)))
        .map(|(oid, m)| (*oid, m.clone()))
        .collect();

    let mut tree_roots: HashMap<ObjectId, IslandBitmap> = HashMap::new();
    let mut visited: std::collections::HashSet<ObjectId> = std::collections::HashSet::new();
    while let Some((cid, _)) = commit_marks.pop() {
        // Re-read the (possibly grown) marks for this commit.
        let Some(marks) = islands.marks.get(&cid).cloned() else {
            continue;
        };
        let Ok(obj) = repo.odb.read(&cid) else {
            continue;
        };
        if obj.kind != ObjectKind::Commit {
            continue;
        }
        let Ok(commit) = parse_commit(&obj.data) else {
            continue;
        };
        // Record root tree marks (for tree propagation).
        add_marks(&mut tree_roots, &commit.tree, &marks, bitmap_size);
        add_marks(&mut islands.marks, &commit.tree, &marks, bitmap_size);
        // Propagate to parents; revisit a parent if its marks grew.
        for parent in &commit.parents {
            let before = islands.marks.get(parent).cloned();
            add_marks(&mut islands.marks, parent, &marks, bitmap_size);
            let after = islands.marks.get(parent).cloned();
            if before != after || !visited.contains(parent) {
                visited.insert(*parent);
                let pm = islands
                    .marks
                    .get(parent)
                    .cloned()
                    .unwrap_or_else(|| IslandBitmap::new(bitmap_size));
                commit_marks.push((*parent, pm));
            }
        }
    }

    // Trees: propagate root-tree marks down to all reachable sub-trees and blobs. Process
    // shallowest trees first (Git sorts by tree depth) so marks flow down correctly even when
    // a sub-tree appears under multiple parents.
    let mut tree_queue: Vec<(ObjectId, IslandBitmap)> = tree_roots.into_iter().collect();
    let mut tree_visited: std::collections::HashSet<ObjectId> = std::collections::HashSet::new();
    while let Some((tid, _)) = tree_queue.pop() {
        let Some(marks) = islands.marks.get(&tid).cloned() else {
            continue;
        };
        // Record that we've expanded this tree. Re-expanding is harmless (add_marks only ORs
        // bits) but we only re-queue a sub-tree below when its marks actually grew.
        tree_visited.insert(tid);
        let Ok(obj) = repo.odb.read(&tid) else {
            continue;
        };
        if obj.kind != ObjectKind::Tree {
            continue;
        }
        let Ok(entries) = parse_tree(&obj.data) else {
            continue;
        };
        for ent in entries {
            // Skip gitlinks (submodule commits).
            if ent.mode == crate::index::MODE_GITLINK {
                continue;
            }
            let before = islands.marks.get(&ent.oid).cloned();
            add_marks(&mut islands.marks, &ent.oid, &marks, bitmap_size);
            let after = islands.marks.get(&ent.oid).cloned();
            // Recurse into sub-trees whose marks changed.
            if ent.mode == 0o040000 && (before != after || !tree_visited.contains(&ent.oid)) {
                let em = islands
                    .marks
                    .get(&ent.oid)
                    .cloned()
                    .unwrap_or_else(|| IslandBitmap::new(bitmap_size));
                tree_queue.push((ent.oid, em));
            }
        }
    }

    // Drop marks for objects not in this pack and any all-zero bitmaps (matching Git, where
    // only objects whose bitmap has bits set are considered "important").
    islands
        .marks
        .retain(|oid, m| !m.is_empty() && packed.contains(oid));
}

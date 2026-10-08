//! Pack geometry for `git repack --geometric` (factor-based progression).
//!
//! Mirrors the split logic in Git's `repack-geometry.c`: packs are weighted by
//! object count from their index, sorted ascending, then a split index separates
//! packs that will be rolled into a new pack from those retained.

use std::path::Path;

use crate::error::{Error, Result};
use crate::pack::read_local_pack_indexes_cached;

/// One local pack considered for geometric repacking.
#[derive(Debug, Clone)]
pub struct GeometricPack {
    /// `pack-<hex>` stem (no `.pack` / `.idx` suffix), matching `pack-objects` stdin lines.
    pub stem: String,
    /// Number of objects in the pack index.
    pub object_count: usize,
    /// Modification time of the `.pack` file (seconds), used for include-pack ordering.
    pub mtime_secs: u64,
    /// Whether this pack lives in the repository's primary object directory rather than an alternate.
    pub is_local: bool,
}

/// Split packs into "roll up" vs "keep" using Git's geometric progression rules.
#[must_use]
pub fn compute_geometry_split(weights: &[usize], split_factor: i32) -> usize {
    let sf = split_factor.max(1) as u64;
    let pack_nr = weights.len();
    if pack_nr == 0 {
        return 0;
    }

    // Packs are sorted ascending by weight (caller's responsibility).
    // Match `repack-geometry.c`: compare `pack[i]` vs `pack[i-1]` for i = pack_nr-1 .. 1.
    let mut split = 0usize;
    let mut found = false;
    for idx in (1..pack_nr).rev() {
        let ours = weights[idx] as u64;
        let prev = weights[idx - 1] as u64;
        if ours < sf.saturating_mul(prev) {
            split = idx;
            found = true;
            break;
        }
    }
    if found {
        split += 1;
    }

    let mut total_size: u64 = 0;
    for &w in &weights[..split] {
        total_size = total_size.saturating_add(w as u64);
    }

    let mut j = split;
    while j < pack_nr {
        let ours = weights[j] as u64;
        if ours < sf.saturating_mul(total_size) {
            split += 1;
            total_size = total_size.saturating_add(ours);
            j += 1;
        } else {
            break;
        }
    }

    split
}

/// Load eligible non-promisor packs from `objects_dir/pack` for geometry.
///
/// Skips packs with a `.keep` file unless `pack_kept_objects` is set, and skips
/// basenames listed in `keep_pack_names` (full `pack-*.pack` filename or basename).
pub fn collect_geometry_packs(
    objects_dir: &Path,
    pack_kept_objects: bool,
    keep_pack_names: &[String],
) -> Result<Vec<GeometricPack>> {
    let pack_dir = objects_dir.join("pack");
    let indexes = read_local_pack_indexes_cached(objects_dir)?;
    let mut out = Vec::new();

    for idx in indexes {
        let pack_name = idx
            .pack_path
            .file_name()
            .and_then(|s| s.to_str())
            .map(str::to_owned)
            .ok_or_else(|| Error::CorruptObject("invalid pack path".to_owned()))?;

        if !pack_name.starts_with("pack-") || !pack_name.ends_with(".pack") {
            continue;
        }

        if keep_pack_names.iter().any(|k| {
            k == &pack_name
                || k.strip_prefix("pack/").unwrap_or(k.as_str()) == pack_name
                || Path::new(k).file_name().and_then(|s| s.to_str()) == Some(pack_name.as_str())
        }) {
            continue;
        }

        let stem = pack_name
            .strip_suffix(".pack")
            .unwrap_or(pack_name.as_str())
            .to_string();

        let keep_path = pack_dir.join(format!("{stem}.keep"));
        if keep_path.is_file() && !pack_kept_objects {
            continue;
        }

        if idx.is_promisor {
            continue;
        }

        let mtime_secs = std::fs::metadata(&idx.pack_path)
            .map(|m| {
                m.modified()
                    .ok()
                    .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                    .map(|d| d.as_secs())
                    .unwrap_or(0)
            })
            .unwrap_or(0);

        out.push(GeometricPack {
            stem,
            object_count: idx.len(),
            mtime_secs,
            is_local: true,
        });
    }

    out.sort_by_key(|a| a.object_count);
    Ok(out)
}

/// Promisor packs only (sibling `.promisor` marker), for a second geometry pass.
pub fn collect_promisor_geometry_packs(
    objects_dir: &Path,
    pack_kept_objects: bool,
    keep_pack_names: &[String],
) -> Result<Vec<GeometricPack>> {
    let pack_dir = objects_dir.join("pack");
    let indexes = read_local_pack_indexes_cached(objects_dir)?;
    let mut out = Vec::new();

    for idx in indexes {
        let pack_name = idx
            .pack_path
            .file_name()
            .and_then(|s| s.to_str())
            .map(str::to_owned)
            .ok_or_else(|| Error::CorruptObject("invalid pack path".to_owned()))?;

        if !pack_name.starts_with("pack-") || !pack_name.ends_with(".pack") {
            continue;
        }

        if keep_pack_names.iter().any(|k| {
            k == &pack_name
                || k.strip_prefix("pack/").unwrap_or(k.as_str()) == pack_name
                || Path::new(k).file_name().and_then(|s| s.to_str()) == Some(pack_name.as_str())
        }) {
            continue;
        }

        let stem = pack_name
            .strip_suffix(".pack")
            .unwrap_or(pack_name.as_str())
            .to_string();

        let keep_path = pack_dir.join(format!("{stem}.keep"));
        if keep_path.is_file() && !pack_kept_objects {
            continue;
        }

        if !idx.is_promisor {
            continue;
        }

        let mtime_secs = std::fs::metadata(&idx.pack_path)
            .map(|m| {
                m.modified()
                    .ok()
                    .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                    .map(|d| d.as_secs())
                    .unwrap_or(0)
            })
            .unwrap_or(0);

        out.push(GeometricPack {
            stem,
            object_count: idx.len(),
            mtime_secs,
            is_local: true,
        });
    }

    out.sort_by_key(|a| a.object_count);
    Ok(out)
}

/// Preferred pack stem (largest retained non-promisor pack), for MIDX `--preferred-pack`.
#[must_use]
pub fn preferred_pack_stem_after_split(packs: &[GeometricPack], split: usize) -> Option<String> {
    if split >= packs.len() {
        return None;
    }
    // Largest local pack in the retained suffix: rightmost after ascending sort.
    packs
        .iter()
        .skip(split)
        .rev()
        .find(|p| p.is_local)
        .map(|p| p.stem.clone())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn progression_intact_split_is_zero() {
        // 3, 6, 12 with factor 2 forms a progression.
        let w = vec![3, 6, 12];
        assert_eq!(compute_geometry_split(&w, 2), 0);
    }

    #[test]
    fn duplicate_small_packs_roll_up() {
        // 3, 3, 6 — progression broken between 3 and 3; rollup extends through 6.
        let w = vec![3, 3, 6];
        assert_eq!(compute_geometry_split(&w, 2), 3);
    }
}

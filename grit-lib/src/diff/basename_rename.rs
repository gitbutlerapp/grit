//! Basename-driven rename pairing before the exhaustive similarity matrix (Git `find_basename_matches`).

use std::collections::{HashMap, HashSet};

use crate::diff_rename_parallel::parallel_load_optional_blobs;
use crate::error::Result;
use crate::objects::FileMode;
use crate::odb::Odb;
use crate::parallel::ParallelOptions;

use super::rename_blob_cache::RenameBlobCache;
use super::rename_lines::RenameLineTable;
use super::DiffEntry;

fn poll_rename_cancel(cancel: Option<&crate::progress::CancelToken>) -> Result<()> {
    if let Some(token) = cancel {
        token.poll_result()?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::diff::{DiffEntry, DiffStatus};
    use crate::objects::{FileMode, ObjectId};

    #[test]
    fn disjoint_repeated_basenames_skip_similarity_scoring() {
        const N: usize = 2000;
        let zero = ObjectId::zero();
        let mut deleted = Vec::with_capacity(N);
        let mut added = Vec::with_capacity(N);
        for i in 0..N {
            deleted.push(DiffEntry {
                status: DiffStatus::Deleted,
                old_path: Some(format!("a/d{}/f{}.txt", i / 50, i % 50)),
                new_path: None,
                old_mode: FileMode::REGULAR,
                new_mode: FileMode::ABSENT,
                old_oid: zero,
                new_oid: zero,
                score: None,
            });
            added.push(DiffEntry {
                status: DiffStatus::Added,
                old_path: None,
                new_path: Some(format!("b/d{}/g{}.txt", i / 50, i % 50)),
                old_mode: FileMode::ABSENT,
                new_mode: FileMode::REGULAR,
                old_oid: zero,
                new_oid: zero,
                score: None,
            });
        }
        let used_deleted = vec![false; N];
        let used_added = vec![false; N];
        assert!(!basename_needs_similarity_scoring(
            &deleted,
            &added,
            &used_deleted,
            &used_added
        ));
    }

    #[test]
    fn shared_repeated_basenames_need_similarity_scoring() {
        const N: usize = 200;
        let zero = ObjectId::zero();
        let mut deleted = Vec::with_capacity(N);
        let mut added = Vec::with_capacity(N);
        for i in 0..N {
            deleted.push(DiffEntry {
                status: DiffStatus::Deleted,
                old_path: Some(format!("old/d{}/f{}.txt", i / 50, i % 50)),
                new_path: None,
                old_mode: FileMode::REGULAR,
                new_mode: FileMode::ABSENT,
                old_oid: zero,
                new_oid: zero,
                score: None,
            });
            added.push(DiffEntry {
                status: DiffStatus::Added,
                old_path: None,
                new_path: Some(format!("new/d{}/f{}.txt", i / 50, i % 50)),
                old_mode: FileMode::ABSENT,
                new_mode: FileMode::REGULAR,
                old_oid: zero,
                new_oid: zero,
                score: None,
            });
        }
        let used_deleted = vec![false; N];
        let used_added = vec![false; N];
        assert!(basename_needs_similarity_scoring(
            &deleted,
            &added,
            &used_deleted,
            &used_added
        ));
    }

    #[test]
    fn wide_unique_basename_matrix_needs_similarity_scoring() {
        const N: usize = 2000;
        let zero = ObjectId::zero();
        let mut deleted = Vec::with_capacity(N);
        let mut added = Vec::with_capacity(N);
        for i in 0..N {
            deleted.push(DiffEntry {
                status: DiffStatus::Deleted,
                old_path: Some(format!("old/{i}.txt")),
                new_path: None,
                old_mode: FileMode::REGULAR,
                new_mode: FileMode::ABSENT,
                old_oid: zero,
                new_oid: zero,
                score: None,
            });
            added.push(DiffEntry {
                status: DiffStatus::Added,
                old_path: None,
                new_path: Some(format!("new/{i}.txt")),
                old_mode: FileMode::ABSENT,
                new_mode: FileMode::REGULAR,
                old_oid: zero,
                new_oid: zero,
                score: None,
            });
        }
        let used_deleted = vec![false; N];
        let used_added = vec![false; N];
        assert!(basename_needs_similarity_scoring(
            &deleted,
            &added,
            &used_deleted,
            &used_added
        ));
    }

    #[test]
    fn pairs_two_inexact_unique_basenames() {
        use crate::objects::{ObjectId, ObjectKind};
        use crate::odb::Odb;
        use crate::parallel::ParallelOptions;
        use std::num::NonZeroUsize;
        use tempfile::TempDir;

        let par_opts = ParallelOptions {
            threads: NonZeroUsize::new(1).expect("non-zero"),
            min_items: 0,
            min_total_bytes: None,
            signed_entry_threshold: None,
        };

        let root = TempDir::new().expect("tempdir");
        let git_dir = root.path().join(".git");
        std::fs::create_dir_all(git_dir.join("objects")).unwrap();
        let odb = Odb::new(&git_dir.join("objects"));
        let mut old_data = b"header 0\n".to_vec();
        for line in 1..=400 {
            old_data.extend_from_slice(format!("line {line} file 0\n").as_bytes());
        }
        let mut new_data = old_data.clone();
        new_data.extend_from_slice(b"extra\n");
        let old_oid = odb.write(ObjectKind::Blob, &old_data).expect("old");
        let new_oid = odb.write(ObjectKind::Blob, &new_data).expect("new");
        let deleted = vec![DiffEntry {
            status: DiffStatus::Deleted,
            old_path: Some("old/file0.txt".into()),
            new_path: None,
            old_mode: FileMode::REGULAR,
            new_mode: FileMode::ABSENT,
            old_oid,
            new_oid: ObjectId::zero(),
            score: None,
        }];
        let added = vec![DiffEntry {
            status: DiffStatus::Added,
            old_path: None,
            new_path: Some("new/file0.txt".into()),
            old_mode: FileMode::ABSENT,
            new_mode: FileMode::REGULAR,
            old_oid: ObjectId::zero(),
            new_oid,
            score: None,
        }];
        let mut used_deleted = vec![false];
        let mut used_added = vec![false];
        let mut renames = Vec::new();
        let mut cache = RenameBlobCache::new(1, 1);
        apply_basename_rename_pairs(
            &odb,
            None,
            &deleted,
            &added,
            &mut used_deleted,
            &mut used_added,
            &mut renames,
            50,
            None,
            &mut cache,
            par_opts,
        )
        .expect("basename pass");
        assert_eq!(
            renames.len(),
            1,
            "expected one basename rename, got {renames:?}"
        );
    }
}

fn read_added_entry_bytes(
    odb: &Odb,
    entry: &DiffEntry,
    work_root: Option<&std::path::Path>,
) -> Option<Vec<u8>> {
    if !entry.new_oid.is_zero() {
        if let Ok(obj) = odb.read(&entry.new_oid) {
            return Some(obj.data);
        }
    }
    let path = entry.new_path.as_deref()?;
    let root = work_root?;
    std::fs::read(root.join(path)).ok()
}

fn path_basename(path: &str) -> &str {
    path.rsplit('/').next().unwrap_or(path)
}

fn path_dirname(path: &str) -> String {
    match path.rfind('/') {
        Some(pos) => path[..pos].to_string(),
        None => String::new(),
    }
}

fn min_basename_score(threshold: u32) -> u32 {
    const MAX: u32 = 100;
    const FACTOR_NUM: u32 = 50;
    threshold + (FACTOR_NUM * (MAX - threshold)) / MAX
}

fn is_regularish_mode(mode: FileMode) -> bool {
    mode == FileMode::REGULAR || mode == FileMode::EXECUTABLE
}

/// Add-side paths and every ancestor directory (stop once a prefix is already present).
fn add_paths_and_ancestor_dirs(added: &[DiffEntry], used_added: &[bool]) -> HashSet<String> {
    let mut set = HashSet::new();
    for (j, add) in added.iter().enumerate() {
        if used_added[j] {
            continue;
        }
        let Some(path) = add.new_path.as_deref() else {
            continue;
        };
        if set.insert(path.to_string()) {
            let mut rest = path;
            while let Some(pos) = rest.rfind('/') {
                rest = &rest[..pos];
                if !set.insert(rest.to_string()) {
                    break;
                }
            }
        }
    }
    set
}

/// Directories that lost all files on the delete side without replacements under the same prefix.
fn dirs_removed_from_entries(
    deleted: &[DiffEntry],
    added: &[DiffEntry],
    used_deleted: &[bool],
    used_added: &[bool],
) -> HashSet<String> {
    let add_dirs = add_paths_and_ancestor_dirs(added, used_added);
    let mut removed = HashSet::new();
    for (i, del) in deleted.iter().enumerate() {
        if used_deleted[i] {
            continue;
        }
        let Some(path) = del.old_path.as_deref() else {
            continue;
        };
        let dir = path_dirname(path);
        if dir.is_empty() {
            continue;
        }
        if !add_dirs.contains(&dir) {
            removed.insert(dir);
        }
    }
    removed
}

struct DirRenameGuess {
    counts: HashMap<String, HashMap<String, usize>>,
    guess: HashMap<String, String>,
}

impl DirRenameGuess {
    fn new() -> Self {
        Self {
            counts: HashMap::new(),
            guess: HashMap::new(),
        }
    }

    fn record_pair(&mut self, old_path: &str, new_path: &str, dirs_removed: &HashSet<String>) {
        let old_dir = path_dirname(old_path);
        let new_dir = path_dirname(new_path);
        if dirs_removed.contains(&old_dir) || !old_dir.is_empty() {
            *self
                .counts
                .entry(old_dir.clone())
                .or_default()
                .entry(new_dir.clone())
                .or_insert(0) += 1;
            if let Some(dest_counts) = self.counts.get(&old_dir) {
                if let Some(best_new) = dest_counts
                    .iter()
                    .max_by_key(|(_, c)| *c)
                    .map(|(k, _)| k.clone())
                {
                    self.guess.insert(old_dir, best_new);
                }
            }
        }
    }

    fn idx_possible_rename(
        &self,
        old_path: &str,
        idx_map: &HashMap<String, usize>,
        setup: bool,
    ) -> Option<usize> {
        if !setup {
            return None;
        }
        let old_dir = path_dirname(old_path);
        let new_dir = self.guess.get(&old_dir)?;
        let predicted = if new_dir.is_empty() {
            path_basename(old_path).to_string()
        } else {
            format!("{new_dir}/{}", path_basename(old_path))
        };
        idx_map.get(&predicted).copied()
    }
}

fn build_basename_maps(
    deleted: &[DiffEntry],
    added: &[DiffEntry],
    used_deleted: &[bool],
    used_added: &[bool],
) -> (HashMap<String, isize>, HashMap<String, isize>) {
    let mut sources = HashMap::new();
    for (i, del) in deleted.iter().enumerate() {
        if used_deleted[i] {
            continue;
        }
        let base = path_basename(del.old_path.as_deref().unwrap_or("")).to_string();
        if base.is_empty() {
            continue;
        }
        if sources.insert(base.clone(), i as isize).is_some() {
            sources.insert(base, -1);
        }
    }
    let mut dests = HashMap::new();
    for (i, add) in added.iter().enumerate() {
        if used_added[i] {
            continue;
        }
        let base = path_basename(add.new_path.as_deref().unwrap_or("")).to_string();
        if base.is_empty() {
            continue;
        }
        if dests.insert(base.clone(), i as isize).is_some() {
            dests.insert(base, -1);
        }
    }
    (sources, dests)
}

/// Whether the basename pre-pass may pair paths beyond exact OID matches.
///
/// Git runs this pass when a delete-side basename also appears on the add side (including
/// duplicate basenames), even when each basename is unique within its side.
pub(crate) fn basename_needs_similarity_scoring(
    deleted: &[DiffEntry],
    added: &[DiffEntry],
    used_deleted: &[bool],
    used_added: &[bool],
) -> bool {
    let mut add_basenames = HashSet::new();
    for (j, add) in added.iter().enumerate() {
        if used_added[j] {
            continue;
        }
        let Some(new_path) = add.new_path.as_deref() else {
            continue;
        };
        let base = path_basename(new_path);
        if !base.is_empty() {
            add_basenames.insert(base);
        }
    }
    for (i, del) in deleted.iter().enumerate() {
        if used_deleted[i] {
            continue;
        }
        let Some(old_path) = del.old_path.as_deref() else {
            continue;
        };
        let base = path_basename(old_path);
        if !base.is_empty() && add_basenames.contains(base) {
            return true;
        }
    }
    false
}

fn blob_payload_size(odb: &Odb, oid: &crate::objects::ObjectId) -> Option<usize> {
    if oid.is_zero() {
        return None;
    }
    odb.read_info(oid).ok().map(|info| info.size as usize)
}

fn added_payload_size(
    odb: &Odb,
    add: &DiffEntry,
    work_root: Option<&std::path::Path>,
) -> Option<usize> {
    if !add.new_oid.is_zero() {
        if let Some(size) = blob_payload_size(odb, &add.new_oid) {
            return Some(size);
        }
    }
    read_added_entry_bytes(odb, add, work_root).map(|data| data.len())
}

fn rename_pair_size_reachable(src_size: usize, dst_size: usize, minimum_score: u32) -> bool {
    const RENAME_SCORE_MAX: u32 = 100;
    let max_size = src_size.max(dst_size);
    let min_size = src_size.min(dst_size);
    if max_size == 0 {
        return minimum_score <= RENAME_SCORE_MAX;
    }
    (max_size - min_size).saturating_mul(RENAME_SCORE_MAX as usize)
        <= max_size.saturating_mul((RENAME_SCORE_MAX - minimum_score) as usize)
}

/// Pair remaining deletes/adds when basenames match (Git basename pre-pass).
#[allow(clippy::too_many_arguments)]
pub(crate) fn apply_basename_rename_pairs(
    odb: &Odb,
    work_root: Option<&std::path::Path>,
    deleted: &[DiffEntry],
    added: &[DiffEntry],
    used_deleted: &mut [bool],
    used_added: &mut [bool],
    renames: &mut Vec<DiffEntry>,
    threshold: u32,
    cancel: Option<&crate::progress::CancelToken>,
    cache: &mut RenameBlobCache,
    par_opts: ParallelOptions,
) -> Result<()> {
    let min_score = min_basename_score(threshold);
    let dirs_removed = dirs_removed_from_entries(deleted, added, used_deleted, used_added);
    let dir_setup = !dirs_removed.is_empty();

    let mut dir_info = DirRenameGuess::new();
    for entry in renames.iter() {
        if let (Some(old), Some(new)) = (&entry.old_path, &entry.new_path) {
            dir_info.record_pair(old, new, &dirs_removed);
        }
    }

    let mut idx_map = HashMap::new();
    for (i, add) in added.iter().enumerate() {
        if used_added[i] {
            continue;
        }
        if let Some(path) = &add.new_path {
            idx_map.insert(path.clone(), i);
        }
    }

    let (sources, dests) = build_basename_maps(deleted, added, used_deleted, used_added);

    struct BasenameCandidate {
        src_index: usize,
        ai: usize,
    }
    let mut candidates: Vec<BasenameCandidate> = Vec::new();
    let mut need_deleted = vec![false; deleted.len()];
    let mut need_added = vec![false; added.len()];

    for (i, del) in deleted.iter().enumerate() {
        poll_rename_cancel(cancel)?;
        if used_deleted[i] {
            continue;
        }
        let Some(old_path) = del.old_path.as_deref() else {
            continue;
        };
        if !is_regularish_mode(del.old_mode) {
            continue;
        }
        let base = path_basename(old_path);
        if base.is_empty() || !dests.contains_key(base) {
            continue;
        }

        let src_index;
        let ai = if sources[base] >= 0 && dests[base] >= 0 {
            src_index = sources[base] as usize;
            dests[base] as usize
        } else {
            src_index = i;
            let Some(guessed) = dir_info.idx_possible_rename(old_path, &idx_map, dir_setup) else {
                continue;
            };
            guessed
        };
        if used_deleted[src_index] || used_added[ai] {
            continue;
        }

        let add = &added[ai];
        if !is_regularish_mode(add.new_mode) {
            continue;
        }
        let Some(ds) = blob_payload_size(odb, &del.old_oid) else {
            continue;
        };
        let Some(asz) = added_payload_size(odb, add, work_root) else {
            continue;
        };
        if !rename_pair_size_reachable(ds, asz, min_score) {
            continue;
        }

        need_deleted[src_index] = true;
        need_added[ai] = true;
        candidates.push(BasenameCandidate { src_index, ai });
    }

    if candidates.is_empty() {
        return Ok(());
    }

    let loaded_deleted = parallel_load_optional_blobs(odb, deleted.len(), par_opts, 0, |di| {
        if !need_deleted[di] {
            return None;
        }
        odb.read(&deleted[di].old_oid).ok().map(|o| o.data)
    });
    for (di, opt) in loaded_deleted.into_iter().enumerate() {
        if let Some(data) = opt {
            cache.insert_deleted_bytes(di, data);
        }
    }
    let loaded_added = parallel_load_optional_blobs(odb, added.len(), par_opts, 0, |ai| {
        if !need_added[ai] {
            return None;
        }
        read_added_entry_bytes(odb, &added[ai], work_root)
    });
    for (ai, opt) in loaded_added.into_iter().enumerate() {
        if let Some(data) = opt {
            cache.insert_added_bytes(ai, data);
        }
    }

    let mut scores: Vec<(u32, usize, usize)> = Vec::new();
    for cand in candidates {
        poll_rename_cancel(cancel)?;
        let Some(old_bytes) = cache
            .deleted_blob(odb, deleted, cand.src_index)
            .map(|b| b.to_vec())
        else {
            continue;
        };
        let Some(new_bytes) = cache
            .added_blob(odb, added, cand.ai, work_root)
            .map(|b| b.to_vec())
        else {
            continue;
        };
        let score = RenameLineTable::similarity_score(
            &RenameLineTable::from_blob(&old_bytes),
            &RenameLineTable::from_blob(&new_bytes),
        );
        if score >= min_score {
            scores.push((score, cand.src_index, cand.ai));
        }
    }

    scores.sort_by(|a, b| {
        let a_same = same_basename_entry(&deleted[a.1], &added[a.2]);
        let b_same = same_basename_entry(&deleted[b.1], &added[b.2]);
        b_same.cmp(&a_same).then_with(|| b.0.cmp(&a.0))
    });

    for (score, src_index, ai) in scores {
        if used_deleted[src_index] || used_added[ai] {
            continue;
        }
        used_deleted[src_index] = true;
        used_added[ai] = true;
        let add = &added[ai];
        renames.push(DiffEntry {
            status: super::DiffStatus::Renamed,
            old_path: deleted[src_index].old_path.clone(),
            new_path: add.new_path.clone(),
            old_mode: deleted[src_index].old_mode,
            new_mode: add.new_mode,
            old_oid: deleted[src_index].old_oid,
            new_oid: add.new_oid,
            score: Some(score),
        });
        if let (Some(old), Some(new)) = (&deleted[src_index].old_path, &add.new_path) {
            dir_info.record_pair(old, new, &dirs_removed);
        }
    }
    Ok(())
}

fn same_basename_entry(del: &DiffEntry, add: &DiffEntry) -> bool {
    let old = del.old_path.as_deref().unwrap_or("");
    let new = add.new_path.as_deref().unwrap_or("");
    path_basename(old) == path_basename(new) && !path_basename(old).is_empty()
}

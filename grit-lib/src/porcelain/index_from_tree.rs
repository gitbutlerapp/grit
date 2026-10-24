//! Build stage-0 indexes from tree objects and refresh worktree stat caches.

use std::path::Path;

use crate::diff::refresh_index_stat_content_verified_with_rules;
use crate::error::Result;
use crate::index::{index_file_mtime, Index};
use crate::objects::ObjectId;
use crate::porcelain::merge::tree_to_index_entries;
use crate::repo::Repository;
use crate::worktree_rules::WorktreeRules;
use crate::write_tree::build_cache_tree_from_tree;

/// Build a sorted stage-0 index whose OIDs and modes match `tree_oid` (zeroed stat fields).
pub(crate) fn index_from_tree(repo: &Repository, tree_oid: &ObjectId) -> Result<Index> {
    let entries = tree_to_index_entries(repo, tree_oid, "")?;
    let mut index = Index::new();
    for entry in entries {
        index.push_entry_unsorted(entry);
    }
    index.sort();
    Ok(index)
}

/// Refresh stage-0 stat fields from the worktree when blob content matches the index OID.
///
/// Tree-built indexes start with zeroed stat data; callers must run this before writing the
/// index when the worktree still matches the tree (mixed reset, stash push, merge abort).
pub(crate) fn refresh_stage0_index_stats_from_worktree(
    repo: &Repository,
    work_tree: &Path,
    index: &mut Index,
) -> Result<()> {
    if !index.entries.iter().any(|e| e.stage() == 0) {
        return Ok(());
    }
    let index_mtime = index_file_mtime(&repo.index_path());
    let rules = WorktreeRules::from_repository(repo, index).ok();
    let rules_arc = rules.map(|r| std::sync::Arc::new(std::sync::Mutex::new(r)));
    let _ = refresh_index_stat_content_verified_with_rules(
        repo.odb(),
        repo.git_dir(),
        index,
        work_tree,
        index_mtime,
        repo.config().ok().as_deref(),
        None,
        rules_arc.as_ref(),
    )?;
    Ok(())
}

/// Like [`index_from_tree`], then refresh stat caches from the worktree when a work tree exists.
pub(crate) fn index_from_tree_with_worktree_stats(
    repo: &Repository,
    tree_oid: &ObjectId,
) -> Result<Index> {
    let mut index = index_from_tree(repo, tree_oid)?;
    if let Some(work_tree) = repo.work_tree() {
        refresh_stage0_index_stats_from_worktree(repo, work_tree, &mut index)?;
    }
    Ok(index)
}

/// Like [`index_from_tree_with_worktree_stats`], but first copies cached stat data from `seed`
/// (normally the repository's on-disk index) for entries with the same path, mode and blob id,
/// and primes the cache-tree from `tree_oid`.
///
/// The worktree refresh then only hashes files whose stat no longer matches, instead of every
/// file in the tree. Racy seeded entries are still re-hashed because the refresh compares against
/// the on-disk index's mtime, which is where `seed` stat data was recorded. With the primed
/// cache-tree, writing a tree after staging a few paths rebuilds only the directories those paths
/// invalidate.
pub(crate) fn index_from_tree_with_seeded_worktree_stats(
    repo: &Repository,
    tree_oid: &ObjectId,
    seed: &Index,
) -> Result<Index> {
    let mut index = index_from_tree(repo, tree_oid)?;
    let cache_tree = build_cache_tree_from_tree(repo.odb(), tree_oid)?;
    if usize::try_from(cache_tree.entry_count).ok() == Some(index.entries.len()) {
        index.set_cache_tree(cache_tree);
    }
    if let Some(work_tree) = repo.work_tree() {
        seed_stage0_stat_data(&mut index, seed);
        refresh_stage0_index_stats_from_worktree(repo, work_tree, &mut index)?;
    }
    Ok(index)
}

/// Copy stat fields from `seed` into `index` entries whose path, mode and oid match a clean
/// stage-0 seed entry. Both indexes must be in canonical `(path, stage)` order; entries that are
/// not found keep their zeroed stat data.
fn seed_stage0_stat_data(index: &mut Index, seed: &Index) {
    let mut seeds = seed
        .entries
        .iter()
        .filter(|e| {
            e.stage() == 0
                && !e.stat_cache_unset()
                && !e.skip_worktree()
                && !e.assume_unchanged()
                && !e.intent_to_add()
        })
        .peekable();
    for entry in index.entries.iter_mut().filter(|e| e.stage() == 0) {
        while seeds.next_if(|s| s.path < entry.path).is_some() {}
        let Some(src) = seeds.next_if(|s| s.path == entry.path) else {
            continue;
        };
        if src.oid != entry.oid || src.mode != entry.mode {
            continue;
        }
        entry.ctime_sec = src.ctime_sec;
        entry.ctime_nsec = src.ctime_nsec;
        entry.mtime_sec = src.mtime_sec;
        entry.mtime_nsec = src.mtime_nsec;
        entry.dev = src.dev;
        entry.ino = src.ino;
        entry.uid = src.uid;
        entry.gid = src.gid;
        entry.size = src.size;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::environment::RepositoryOptions;
    use crate::index::{IndexEntry, MODE_EXECUTABLE, MODE_REGULAR};
    use crate::objects::ObjectKind;
    use crate::porcelain::add::{stage, StageOptions};
    use crate::porcelain::commit::{create_commit, CommitRequest};
    use crate::progress::NullProgress;
    use crate::state::resolve_head_for_repository;
    use crate::write_tree::cache_tree_fully_valid;
    use std::fs;
    use tempfile::TempDir;

    /// `mtime == 0` builds a tree-style entry with every stat field zeroed.
    fn entry(path: &str, oid: ObjectId, mode: u32, mtime: u32) -> IndexEntry {
        let stat = |v: u32| if mtime == 0 { 0 } else { v };
        IndexEntry {
            ctime_sec: mtime,
            ctime_nsec: stat(7),
            mtime_sec: mtime,
            mtime_nsec: stat(9),
            dev: stat(1),
            ino: mtime,
            mode,
            uid: stat(2),
            gid: stat(3),
            size: stat(4),
            oid,
            flags: path.len().min(0xFFF) as u16,
            flags_extended: None,
            path: path.as_bytes().to_vec(),
            base_index_pos: 0,
        }
    }

    fn index_of(entries: Vec<IndexEntry>) -> Index {
        let mut index = Index::new();
        for e in entries {
            index.push_entry_unsorted(e);
        }
        index.sort();
        index
    }

    #[test]
    fn seed_copies_stat_only_for_same_oid_and_mode() {
        let a = ObjectId::from_bytes(&[1; 20]).expect("oid");
        let b = ObjectId::from_bytes(&[2; 20]).expect("oid");
        let mut target = index_of(vec![
            entry("same", a, MODE_REGULAR, 0),
            entry("other-oid", a, MODE_REGULAR, 0),
            entry("other-mode", a, MODE_REGULAR, 0),
            entry("only-in-tree", a, MODE_REGULAR, 0),
            entry("skipped", a, MODE_REGULAR, 0),
        ]);
        let mut skipped = entry("skipped", a, MODE_REGULAR, 50);
        skipped.flags |= 0x8000;
        let seed = index_of(vec![
            entry("same", a, MODE_REGULAR, 10),
            entry("other-oid", b, MODE_REGULAR, 20),
            entry("other-mode", a, MODE_EXECUTABLE, 30),
            entry("only-in-seed", a, MODE_REGULAR, 40),
            skipped,
        ]);
        seed_stage0_stat_data(&mut target, &seed);

        let same = target.get(b"same", 0).expect("same");
        assert_eq!((same.mtime_sec, same.ino, same.size), (10, 10, 4));
        for path in ["other-oid", "other-mode", "only-in-tree", "skipped"] {
            let e = target.get(path.as_bytes(), 0).expect(path);
            assert!(e.stat_cache_unset(), "{path} must keep zeroed stat");
        }
    }

    #[test]
    fn seeded_index_matches_tree_and_primes_valid_cache_tree() {
        let dir = TempDir::new().expect("tempdir");
        let root = dir.path();
        crate::repo::init_repository(root, false, "main", None, crate::RefStorageFormat::Files)
            .expect("init");
        for rel in ["a/x.txt", "a/b/y.txt", "z.txt"] {
            let abs = root.join(rel);
            fs::create_dir_all(abs.parent().expect("parent")).expect("mkdir");
            fs::write(abs, rel.as_bytes()).expect("write");
        }
        let repo = Repository::discover(Some(root), &RepositoryOptions::empty()).expect("repo");
        stage(&repo, &StageOptions::default(), &mut NullProgress).expect("stage");
        let ident = "T <t@example.com> 1 +0000".to_owned();
        create_commit(
            &repo,
            &CommitRequest {
                message: "init".into(),
                author: Some(ident.parse().expect("signature")),
                committer: Some(ident.parse().expect("signature")),
                ..Default::default()
            },
            &mut NullProgress,
        )
        .expect("commit");
        let head = resolve_head_for_repository(&repo).expect("head");
        let commit = repo.odb().read(head.oid().expect("oid")).expect("read");
        let tree = crate::objects::parse_commit(&commit.data)
            .expect("parse")
            .tree;
        fs::write(root.join("z.txt"), b"changed").expect("modify");

        let real = repo.load_index().expect("index");
        let seeded =
            index_from_tree_with_seeded_worktree_stats(&repo, &tree, &real).expect("seeded");
        let plain = index_from_tree_with_worktree_stats(&repo, &tree).expect("plain");

        let oids = |idx: &Index| -> Vec<(Vec<u8>, ObjectId, u32)> {
            idx.entries
                .iter()
                .map(|e| (e.path.clone(), e.oid, e.mode))
                .collect()
        };
        assert_eq!(oids(&seeded), oids(&plain));
        assert!(cache_tree_fully_valid(
            repo.odb(),
            seeded.cache_tree.as_ref()
        ));
        assert_eq!(seeded.cache_tree_root, Some(tree));
        assert!(!seeded.get(b"a/x.txt", 0).expect("x").stat_cache_unset());
        assert_eq!(
            seeded.get(b"z.txt", 0).expect("z").oid,
            repo.odb().write(ObjectKind::Blob, b"z.txt").expect("blob")
        );
    }
}

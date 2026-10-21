//! Per-batch memoization for collision-aware object-id abbreviation (`core.abbrev`).

use std::collections::{HashMap, HashSet};

use crate::commit_pretty::abbrev_hex;
use crate::config::ConfigSet;
use crate::error::Result;
use crate::objects::ObjectId;
use crate::parallel::ThreadPolicy;
use crate::repo::Repository;
use crate::rev_parse::{abbreviate_object_id_with_prefix_cache, prefix_is_ambiguous_in_odb};

/// Memoized unique abbreviations using [`Repository::odb`] prefix lookup (pack/MIDX/loose).
#[derive(Clone)]
pub(crate) struct OidAbbrevCache {
    min_len: usize,
    memo: HashMap<ObjectId, String>,
    prefix_ambiguous: HashMap<String, bool>,
}

impl Default for OidAbbrevCache {
    fn default() -> Self {
        Self {
            min_len: 7,
            memo: HashMap::new(),
            prefix_ambiguous: HashMap::new(),
        }
    }
}

impl OidAbbrevCache {
    #[must_use]
    pub(crate) fn new(min_len: usize) -> Self {
        Self {
            min_len: min_len.clamp(4, 40),
            memo: HashMap::new(),
            prefix_ambiguous: HashMap::new(),
        }
    }

    /// Unique abbreviated hex for `oid`, extending past `min_len` when required.
    pub(crate) fn abbrev(&mut self, repo: &Repository, oid: &ObjectId) -> Result<String> {
        if let Some(hit) = self.memo.get(oid) {
            return Ok(hit.clone());
        }
        let abbrev = abbreviate_object_id_with_prefix_cache(
            repo,
            *oid,
            self.min_len,
            &mut self.prefix_ambiguous,
        )?;
        self.memo.insert(*oid, abbrev.clone());
        Ok(abbrev)
    }

    /// Precompute index-line abbreviations with batch-local and ODB disambiguation.
    pub(crate) fn warm_for_patch_batch<I>(
        &mut self,
        repo: &Repository,
        oids: I,
        _thread_policy: &ThreadPolicy,
        _config: &ConfigSet,
    ) -> Result<()>
    where
        I: IntoIterator<Item = ObjectId>,
    {
        let min_len = self.min_len;
        let mut unique: HashSet<ObjectId> = HashSet::new();
        for oid in oids {
            if oid.is_zero() {
                self.memo.insert(oid, abbrev_hex(&oid, min_len));
            } else {
                unique.insert(oid);
            }
        }
        if unique.is_empty() {
            return Ok(());
        }
        let mut list: Vec<ObjectId> = unique.into_iter().collect();
        list.sort_by_key(ObjectId::to_hex);
        if list.len() <= 4 {
            for oid in list {
                let abbrev = abbreviate_object_id_with_prefix_cache(
                    repo,
                    oid,
                    min_len,
                    &mut self.prefix_ambiguous,
                )?;
                self.memo.insert(oid, abbrev);
            }
            return Ok(());
        }
        let hexes: Vec<String> = list.iter().map(ObjectId::to_hex).collect();
        let mut lens = vec![min_len; list.len()];

        loop {
            let mut by_prefix: HashMap<String, Vec<usize>> = HashMap::new();
            for (i, hex) in hexes.iter().enumerate() {
                let end = lens[i].min(hex.len());
                by_prefix.entry(hex[..end].to_owned()).or_default().push(i);
            }
            let mut bumped = false;
            for indices in by_prefix.values() {
                if indices.len() <= 1 {
                    continue;
                }
                bumped = true;
                for &i in indices {
                    if lens[i] < 40 {
                        lens[i] += 1;
                    }
                }
            }
            if !bumped {
                break;
            }
        }

        for (i, oid) in list.iter().enumerate() {
            let hex = &hexes[i];
            let mut len = lens[i].min(hex.len());
            while len <= hex.len() {
                let prefix = &hex[..len];
                if !prefix_is_ambiguous_in_odb(repo, prefix, &mut self.prefix_ambiguous)? {
                    break;
                }
                len += 1;
            }
            len = len.min(hex.len());
            self.memo.insert(*oid, hex[..len].to_owned());
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    use crate::commit_pretty::abbrev_hex;
    use crate::diff::zero_oid;
    use crate::hash::hash_object;
    use crate::objects::HashAlgo;
    use crate::objects::ObjectKind;
    use crate::repo::{InitOptions, Repository};
    use tempfile::TempDir;

    fn two_blobs_sharing_seven_hex_prefix() -> (ObjectId, ObjectId, Vec<u8>, Vec<u8>) {
        let mut first_by_prefix: HashMap<String, (ObjectId, Vec<u8>)> = HashMap::new();
        for n in 0_u64..2_000_000 {
            let data = n.to_string().into_bytes();
            let oid = hash_object(HashAlgo::Sha1, ObjectKind::Blob, &data);
            let prefix = oid.to_hex()[..7].to_string();
            if let Some((other, _)) = first_by_prefix.get(&prefix) {
                if *other != oid {
                    let (first_oid, first_data) = first_by_prefix.remove(&prefix).expect("prefix");
                    return (first_oid, oid, first_data, data);
                }
            } else {
                first_by_prefix.insert(prefix, (oid, data));
            }
        }
        panic!("failed to find two blobs sharing a 7-hex prefix");
    }

    #[test]
    fn warm_for_patch_batch_disambiguates_against_full_odb() -> Result<()> {
        let dir = TempDir::new().expect("tempdir");
        let repo = Repository::init(dir.path(), &InitOptions::default()).expect("init");
        let (referenced, dangling, data_ref, data_dang) = two_blobs_sharing_seven_hex_prefix();
        assert_eq!(referenced.to_hex()[..7], dangling.to_hex()[..7]);
        repo.odb().write(ObjectKind::Blob, &data_ref)?;
        repo.odb().write(ObjectKind::Blob, &data_dang)?;
        let config = repo.config()?.clone();
        let thread_policy = repo.thread_policy();
        let mut cache = OidAbbrevCache::new(7);
        cache.warm_for_patch_batch(&repo, [referenced, dangling], &thread_policy, &config)?;
        let ab = cache.abbrev(&repo, &referenced)?;
        let bb = cache.abbrev(&repo, &dangling)?;
        assert_ne!(ab, bb);
        assert!(ab.len() > 7 || bb.len() > 7);
        Ok(())
    }

    #[test]
    fn abbrev_extends_prefix_when_seven_hex_collide() -> Result<()> {
        let dir = TempDir::new().expect("tempdir");
        let repo = Repository::init(dir.path(), &InitOptions::default()).expect("init");
        let (a, b, data_a, data_b) = two_blobs_sharing_seven_hex_prefix();
        assert_eq!(a.to_hex()[..7], b.to_hex()[..7]);
        assert_eq!(repo.odb().write(ObjectKind::Blob, &data_a)?, a);
        assert_eq!(repo.odb().write(ObjectKind::Blob, &data_b)?, b);
        let mut cache = OidAbbrevCache::new(7);
        let ab = cache.abbrev(&repo, &a)?;
        let bb = cache.abbrev(&repo, &b)?;
        assert_ne!(ab, bb);
        assert!(ab.len() > 7 || bb.len() > 7);
        assert_eq!(&ab[..7], &a.to_hex()[..7]);
        assert_eq!(&bb[..7], &b.to_hex()[..7]);
        assert_eq!(abbrev_hex(&zero_oid(), 7), "0000000");
        Ok(())
    }
}

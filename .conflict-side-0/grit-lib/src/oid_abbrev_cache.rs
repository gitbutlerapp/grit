//! Per-batch memoization for collision-aware object-id abbreviation (`core.abbrev`).

use std::collections::HashMap;

use crate::error::Result;
use crate::objects::ObjectId;
use crate::repo::Repository;
use crate::rev_parse::abbreviate_object_id;

/// Memoized unique abbreviations using [`Repository::odb`] prefix lookup (pack/MIDX/loose).
#[derive(Default)]
pub(crate) struct OidAbbrevCache {
    min_len: usize,
    memo: HashMap<ObjectId, String>,
}

impl OidAbbrevCache {
    #[must_use]
    pub(crate) fn new(min_len: usize) -> Self {
        Self {
            min_len: min_len.clamp(4, 40),
            memo: HashMap::new(),
        }
    }

    /// Unique abbreviated hex for `oid`, extending past `min_len` when required.
    pub(crate) fn abbrev(&mut self, repo: &Repository, oid: &ObjectId) -> Result<String> {
        if let Some(hit) = self.memo.get(oid) {
            return Ok(hit.clone());
        }
        let abbrev = abbreviate_object_id(repo, *oid, self.min_len)?;
        self.memo.insert(*oid, abbrev.clone());
        Ok(abbrev)
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

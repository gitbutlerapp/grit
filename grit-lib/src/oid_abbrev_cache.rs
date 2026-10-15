//! Fast collision-aware object-id abbreviation for patch output (`core.abbrev`).

use std::collections::HashMap;
use std::ops::ControlFlow;

use crate::error::Result;
use crate::objects::ObjectId;
use crate::odb::Odb;

/// Precomputed ambiguous short-prefix buckets plus per-OID memoization.
#[derive(Default)]
pub struct OidAbbrevCache {
    min_len: usize,
    loaded: bool,
    memo: HashMap<ObjectId, String>,
    /// Prefixes at `min_len` that map to more than one object id.
    ambiguous: HashMap<String, Vec<ObjectId>>,
}

impl OidAbbrevCache {
    /// Create a cache that will load prefix groups from `odb` on first use.
    #[must_use]
    pub fn new(min_len: usize) -> Self {
        Self {
            min_len: min_len.clamp(4, 40),
            loaded: false,
            memo: HashMap::new(),
            ambiguous: HashMap::new(),
        }
    }

    /// Unique abbreviated hex for `oid`, extending past `min_len` when required.
    pub fn abbrev(&mut self, odb: &Odb, oid: &ObjectId) -> Result<String> {
        if let Some(hit) = self.memo.get(oid) {
            return Ok(hit.clone());
        }
        self.ensure_loaded(odb)?;
        let hex = oid.to_hex();
        let abbrev = if hex.len() <= self.min_len {
            hex
        } else {
            let key = hex[..self.min_len].to_string();
            if let Some(group) = self.ambiguous.get(&key) {
                let mut len = self.min_len;
                while len < hex.len() {
                    let prefix = &hex[..len];
                    let matches = group
                        .iter()
                        .filter(|candidate| candidate.to_hex().starts_with(prefix))
                        .count();
                    if matches <= 1 {
                        break;
                    }
                    len += 1;
                }
                hex[..len].to_string()
            } else {
                hex[..self.min_len].to_string()
            }
        };
        self.memo.insert(*oid, abbrev.clone());
        Ok(abbrev)
    }

    fn ensure_loaded(&mut self, odb: &Odb) -> Result<()> {
        if self.loaded {
            return Ok(());
        }
        let mut buckets: HashMap<String, Vec<ObjectId>> = HashMap::new();
        odb.for_each_object(&mut |oid| {
            let hex = oid.to_hex();
            if hex.len() >= self.min_len {
                buckets
                    .entry(hex[..self.min_len].to_string())
                    .or_default()
                    .push(*oid);
            }
            ControlFlow::Continue(())
        })?;
        self.ambiguous = buckets
            .into_iter()
            .filter(|(_, ids)| ids.len() > 1)
            .collect();
        self.loaded = true;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

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
        let odb = repo.odb();
        let (a, b, data_a, data_b) = two_blobs_sharing_seven_hex_prefix();
        assert_eq!(a.to_hex()[..7], b.to_hex()[..7]);
        assert_eq!(odb.write(ObjectKind::Blob, &data_a)?, a);
        assert_eq!(odb.write(ObjectKind::Blob, &data_b)?, b);
        let mut cache = OidAbbrevCache::new(7);
        let ab = cache.abbrev(odb, &a)?;
        let bb = cache.abbrev(odb, &b)?;
        assert_ne!(ab, bb);
        assert!(ab.len() > 7 || bb.len() > 7);
        assert_eq!(&ab[..7], &a.to_hex()[..7]);
        assert_eq!(&bb[..7], &b.to_hex()[..7]);
        Ok(())
    }
}

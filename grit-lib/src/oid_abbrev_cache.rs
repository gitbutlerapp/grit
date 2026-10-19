//! Fast collision-aware object-id abbreviation for patch output (`core.abbrev`).

use std::collections::HashMap;

use crate::error::Result;
use crate::objects::ObjectId;
use crate::odb::Odb;

/// Per-OID memoization with lazy extension when two emitted ids share a prefix.
#[derive(Default)]
pub struct OidAbbrevCache {
    min_len: usize,
    memo: HashMap<ObjectId, String>,
}

impl OidAbbrevCache {
    /// Create a cache keyed at `min_len` hex digits (clamped like Git `core.abbrev`).
    #[must_use]
    pub fn new(min_len: usize) -> Self {
        Self {
            min_len: min_len.clamp(4, 40),
            memo: HashMap::new(),
        }
    }

    /// Unique abbreviated hex for `oid`, extending past `min_len` only when needed.
    ///
    /// Collision checks are limited to ids already passed through this cache, so
    /// log-patch workloads never scan the full object database up front.
    pub fn abbrev(&mut self, _odb: &Odb, oid: &ObjectId) -> Result<String> {
        if let Some(hit) = self.memo.get(oid) {
            return Ok(hit.clone());
        }
        let hex = oid.to_hex();
        let mut len = self.min_len.min(hex.len());
        loop {
            let prefix = &hex[..len];
            let mut conflict = false;
            for (other, abbrev) in &self.memo {
                if other == oid {
                    continue;
                }
                let other_hex = other.to_hex();
                if other_hex.len() >= len
                    && &other_hex[..len] == prefix
                    && abbrev.len() >= len
                    && &abbrev[..len] == prefix
                {
                    conflict = true;
                    break;
                }
            }
            if !conflict {
                break;
            }
            len += 1;
            if len >= hex.len() {
                break;
            }
        }
        let abbrev = hex[..len].to_string();
        self.reconcile_collisions(oid, &hex, len);
        self.memo.insert(*oid, abbrev.clone());
        Ok(abbrev)
    }

    fn reconcile_collisions(&mut self, oid: &ObjectId, hex: &str, len: usize) {
        let prefix = &hex[..len.min(hex.len())];
        let mut to_refresh: Vec<ObjectId> = self
            .memo
            .iter()
            .filter_map(|(other, abbrev)| {
                if other == oid {
                    return None;
                }
                let other_hex = other.to_hex();
                (other_hex.starts_with(prefix) && abbrev.len() < len).then_some(*other)
            })
            .collect();
        to_refresh.sort_unstable();
        for other in to_refresh {
            let other_hex = other.to_hex();
            let mut new_len = len;
            while new_len < other_hex.len() {
                let candidate = &other_hex[..new_len];
                let clash = self.memo.iter().any(|(id, ab)| {
                    *id != other && ab.starts_with(candidate) && id.to_hex().starts_with(candidate)
                });
                if !clash {
                    break;
                }
                new_len += 1;
            }
            self.memo.insert(other, other_hex[..new_len].to_string());
        }
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

//! Fast [`HashMap`] / [`HashSet`] keys for [`ObjectId`].
//!
//! Git object ids are already cryptographic digests; hashing them again with
//! SipHash over the full 20–32 byte value is wasted work on hot paths. These
//! types use only the first eight digest bytes for bucket selection. [`OidHasher::finish`]
//! XORs that prefix with a process-wide random value from [`OidBuildHasher::default`]
//! so bucket placement is not predictable to an attacker who can choose object ids.
//!
//! Backing storage is [`hashbrown`] (same approach as `gix-hashtable`): it
//! lowers insert and lookup overhead versus the standard library map for large
//! OID-keyed tables while keeping the same semantics.
//!
//! [`ObjectId`] must be hashed with [`Hash::hash`] (which feeds the active digest
//! bytes only) when used as a key in [`OidHashMap`] or [`OidHashSet`].

use std::hash::{BuildHasher, Hasher};
use std::sync::OnceLock;

use getrandom::getrandom;
use hashbrown::{HashMap, HashSet};

use super::ObjectId;

/// Build hasher for [`OidHashMap`] and [`OidHashSet`].
#[derive(Clone, Copy, Debug)]
pub struct OidBuildHasher {
    multiplier: u64,
}

impl Default for OidBuildHasher {
    fn default() -> Self {
        shared_oid_build_hasher()
    }
}

impl OidBuildHasher {
    /// Creates a hasher with a fresh process-local random value for [`OidHasher::finish`].
    #[must_use]
    pub fn new() -> Self {
        Self {
            multiplier: random_odd_multiplier(),
        }
    }
}

impl BuildHasher for OidBuildHasher {
    type Hasher = OidHasher;

    #[inline]
    fn build_hasher(&self) -> Self::Hasher {
        OidHasher {
            prefix: 0,
            multiplier: self.multiplier,
        }
    }
}

/// Hasher state for OID-keyed maps.
#[derive(Clone, Copy, Debug)]
pub struct OidHasher {
    prefix: u64,
    multiplier: u64,
}

impl Hasher for OidHasher {
    #[inline(always)]
    fn finish(&self) -> u64 {
        self.prefix ^ self.multiplier
    }

    #[inline(always)]
    fn write(&mut self, bytes: &[u8]) {
        self.prefix = u64::from_ne_bytes([
            bytes[0], bytes[1], bytes[2], bytes[3], bytes[4], bytes[5], bytes[6], bytes[7],
        ]);
    }

    fn write_u8(&mut self, _i: u8) {
        oid_hasher_reject_typed_write("u8");
    }

    fn write_u16(&mut self, _i: u16) {
        oid_hasher_reject_typed_write("u16");
    }

    fn write_u32(&mut self, _i: u32) {
        oid_hasher_reject_typed_write("u32");
    }

    fn write_u64(&mut self, _i: u64) {
        oid_hasher_reject_typed_write("u64");
    }

    fn write_usize(&mut self, _i: usize) {
        oid_hasher_reject_typed_write("usize");
    }

    fn write_i8(&mut self, _i: i8) {
        oid_hasher_reject_typed_write("i8");
    }

    fn write_i16(&mut self, _i: i16) {
        oid_hasher_reject_typed_write("i16");
    }

    fn write_i32(&mut self, _i: i32) {
        oid_hasher_reject_typed_write("i32");
    }

    fn write_i64(&mut self, _i: i64) {
        oid_hasher_reject_typed_write("i64");
    }

    fn write_isize(&mut self, _i: isize) {
        oid_hasher_reject_typed_write("isize");
    }
}

#[cold]
fn oid_hasher_reject_typed_write(label: &str) -> ! {
    panic!("OidHasher only supports ObjectId::hash (write), not write_{label}");
}

/// [`HashMap`] keyed by [`ObjectId`] with [`OidBuildHasher`] (hashbrown table).
pub type OidHashMap<V> = HashMap<ObjectId, V, OidBuildHasher>;

/// [`HashSet`] of [`ObjectId`] with [`OidBuildHasher`] (hashbrown table).
pub type OidHashSet = HashSet<ObjectId, OidBuildHasher>;

/// Shared [`OidBuildHasher`] for this process (one process-wide random value for [`OidHasher::finish`]).
#[inline]
fn shared_oid_build_hasher() -> OidBuildHasher {
    // hygiene: process-wide shared OID map hasher seed
    static HASHER: OnceLock<OidBuildHasher> = OnceLock::new();
    *HASHER.get_or_init(OidBuildHasher::new)
}

/// Empty [`OidHashMap`].
#[inline]
pub fn oid_hash_map<V>() -> OidHashMap<V> {
    HashMap::with_hasher(shared_oid_build_hasher())
}

/// Empty [`OidHashMap`] with room for at least `capacity` entries without rehashing.
#[inline]
pub fn oid_hash_map_with_capacity<V>(capacity: usize) -> OidHashMap<V> {
    HashMap::with_capacity_and_hasher(capacity, shared_oid_build_hasher())
}

/// Empty [`OidHashSet`].
#[inline]
pub fn oid_hash_set() -> OidHashSet {
    HashSet::with_hasher(shared_oid_build_hasher())
}

/// Empty [`OidHashSet`] with room for at least `capacity` entries without rehashing.
#[inline]
pub fn oid_hash_set_with_capacity(capacity: usize) -> OidHashSet {
    HashSet::with_capacity_and_hasher(capacity, shared_oid_build_hasher())
}

/// Shared empty OID set for borrow-only call sites (avoids per-call allocation).
#[must_use]
pub fn empty_oid_set() -> &'static OidHashSet {
    // hygiene: shared empty OID set singleton for borrow-only APIs
    static EMPTY: OnceLock<OidHashSet> = OnceLock::new();
    EMPTY.get_or_init(oid_hash_set)
}

/// Copy a standard OID set into an [`OidHashSet`].
#[inline]
pub fn oid_hash_set_from(set: &std::collections::HashSet<ObjectId>) -> OidHashSet {
    let mut out = oid_hash_set_with_capacity(set.len());
    out.extend(set.iter().copied());
    out
}

/// Copy a standard OID map into an [`OidHashMap`].
#[inline]
pub fn oid_hash_map_from<V: Clone>(map: &std::collections::HashMap<ObjectId, V>) -> OidHashMap<V> {
    let mut out = oid_hash_map_with_capacity(map.len());
    for (k, v) in map {
        out.insert(*k, v.clone());
    }
    out
}

/// Shared empty standard OID set for public API boundaries (avoids per-call allocation).
#[must_use]
pub fn empty_object_id_hash_set() -> &'static std::collections::HashSet<ObjectId> {
    // hygiene: shared empty standard OID set for API boundaries
    static EMPTY: OnceLock<std::collections::HashSet<ObjectId>> = OnceLock::new();
    EMPTY.get_or_init(std::collections::HashSet::new)
}

/// Shared empty OID → bytes map for commit-graph filter tests and call sites.
#[must_use]
pub fn empty_oid_byte_filter_map() -> &'static OidHashMap<Vec<u8>> {
    // hygiene: shared empty OID byte map for filter call sites
    static EMPTY: OnceLock<OidHashMap<Vec<u8>>> = OnceLock::new();
    EMPTY.get_or_init(oid_hash_map)
}

fn random_odd_multiplier() -> u64 {
    let mut buf = [0u8; 8];
    if getrandom(&mut buf).is_err() {
        return 1;
    }
    let mut m = u64::from_ne_bytes(buf);
    m |= 1;
    m
}

#[cfg(test)]
mod tests {
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};

    use super::*;

    #[test]
    fn hasher_uses_only_first_eight_id_bytes() {
        let build = OidBuildHasher::new();
        let id_bytes = [0xab_u8; 20];

        let mut h = build.build_hasher();
        h.write(&id_bytes);
        let once = h.finish();

        let mut h2 = build.build_hasher();
        h2.write(&id_bytes[..8]);
        assert_eq!(once, h2.finish());
    }

    #[test]
    fn equal_oids_hash_the_same_in_oid_maps() {
        let a = ObjectId::from_hex("4b825dc642cb6eb9a060e54bf8d69288fbee4904").expect("hex");
        let b = ObjectId::from_hex("4b825dc642cb6eb9a060e54bf8d69288fbee4904").expect("hex");
        assert_eq!(a, b);

        let build = OidBuildHasher::new();
        let mut ha = build.build_hasher();
        let mut hb = build.build_hasher();
        a.hash(&mut ha);
        b.hash(&mut hb);
        assert_eq!(ha.finish(), hb.finish());
    }

    #[test]
    fn map_lookup_follows_eq() {
        let sha1 = ObjectId::from_hex("4b825dc642cb6eb9a060e54bf8d69288fbee4904").expect("hex");
        let altered = ObjectId::from_hex("4b825dc642cb6eb9a060e54bf8d69288fbee4905").expect("hex");
        assert_ne!(sha1, altered);

        let mut map: OidHashMap<&str> = oid_hash_map();
        map.insert(sha1, "a");
        assert_eq!(map.get(&sha1), Some(&"a"));
        assert_eq!(map.get(&altered), None);
    }

    #[test]
    fn oid_hash_map_set_roundtrip() {
        let id = ObjectId::from_hex("deadbeefdeadbeefdeadbeefdeadbeefdeadbeef").expect("hex");
        let mut set: OidHashSet = oid_hash_set();
        assert!(set.insert(id));
        assert!(!set.insert(id));
        assert!(set.contains(&id));
    }

    #[test]
    fn object_id_hash_feeds_active_digest_only() {
        let id = ObjectId::from_hex("0102030405060708090a0b0c0d0e0f1011121314").expect("hex");
        let build = OidBuildHasher::new();
        let mut oid_hasher = build.build_hasher();
        id.hash(&mut oid_hasher);
        let oid_finish = oid_hasher.finish();

        let mut raw = build.build_hasher();
        raw.write(id.as_bytes());
        assert_eq!(oid_finish, raw.finish());

        let mut std_hasher = DefaultHasher::new();
        id.hash(&mut std_hasher);
        let _ = std_hasher.finish();
    }
}

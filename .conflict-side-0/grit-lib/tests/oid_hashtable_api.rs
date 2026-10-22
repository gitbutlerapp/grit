//! Public API smoke test for OID-keyed hash containers.

use grit_lib::objects::{
    oid_hash_map, oid_hash_set, ObjectId, OidBuildHasher, OidHashMap, OidHashSet,
};

#[test]
fn oid_hashtable_public_types_roundtrip() {
    let id = ObjectId::from_hex("4b825dc642cb6eb9a060e54bf8d69288fbee4904").expect("hex");
    let mut map: OidHashMap<&str> = oid_hash_map();
    map.insert(id, "value");
    assert_eq!(map.get(&id), Some(&"value"));

    let mut set: OidHashSet = oid_hash_set();
    assert!(set.insert(id));
    assert!(set.contains(&id));

    let _ = OidBuildHasher::new();
}

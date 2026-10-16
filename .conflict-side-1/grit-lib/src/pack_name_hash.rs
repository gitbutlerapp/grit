//! Git pack bitmap name-hash functions (`pack_name_hash` / `pack_name_hash_v2`).
//!
//! These match the inline implementations in Git's `pack-objects.h` and are used
//! for bitmap index name-hash fields and `test-tool name-hash` stability tests.

/// Git's legacy pack name hash (version 1): a sortable number from the last
/// sixteen non-whitespace characters of `name`.
#[must_use]
pub fn pack_name_hash(name: &str) -> u32 {
    let mut hash: u32 = 0;
    for c in name.bytes() {
        if c.is_ascii_whitespace() {
            continue;
        }
        let c = u32::from(c);
        hash = (hash >> 2).wrapping_add(c << 24);
    }
    hash
}

/// The v2 pack name hash: path-component aware, used for newer bitmap formats.
#[must_use]
pub fn pack_name_hash_v2(name: &[u8]) -> u32 {
    let mut hash: u32 = 0;
    let mut base: u32 = 0;
    for &c in name {
        if c == 0 {
            break;
        }
        if c.is_ascii_whitespace() {
            continue;
        }
        if c == b'/' {
            base = (base >> 6) ^ hash;
            hash = 0;
        } else {
            // `u8::reverse_bits` is the stdlib equivalent of the per-byte
            // bit-reversal step; it lowers to a single instruction.
            let c = u32::from(c.reverse_bits());
            hash = (hash >> 2).wrapping_add(c << 24);
        }
    }
    (base >> 6) ^ hash
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn name_hash_matches_git_test_vectors() {
        let cases = [
            ("first", 2_582_249_472, 1_763_573_760),
            ("second", 2_289_942_528, 1_188_134_912),
            ("third", 2_300_837_888, 1_130_758_144),
            (
                "a/one-long-enough-for-collisions",
                2_544_516_325,
                3_963_087_891,
            ),
            (
                "b/two-long-enough-for-collisions",
                2_544_516_325,
                4_013_419_539,
            ),
            (
                "many/parts/to/this/path/enough/to/collide/in/v2",
                1_420_111_091,
                1_709_547_268,
            ),
            (
                "enough/parts/to/this/path/enough/to/collide/in/v2",
                1_420_111_091,
                1_709_547_268,
            ),
        ];
        for (path, v1, v2) in cases {
            assert_eq!(pack_name_hash(path), v1, "v1 {path}");
            assert_eq!(pack_name_hash_v2(path.as_bytes()), v2, "v2 {path}");
        }
    }
}

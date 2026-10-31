//! Git-compatible case-insensitive name hashing (`memihash`) and the `LAZY_THREAD_COST`
//! threshold Git's `name-hash.c` uses to decide when index work is worth threading.

const FNV32_BASE: u32 = 0x811c9dc5;
const FNV32_PRIME: u32 = 0x0100_0193;
/// Matches Git `name-hash.c` (`LAZY_THREAD_COST`).
pub(crate) const LAZY_THREAD_COST: usize = 2000;

/// Git `memihash`: case-insensitive FNV-1a over bytes.
#[must_use]
pub fn memihash(buf: &[u8]) -> u32 {
    buf.iter().fold(FNV32_BASE, |hash, &b| {
        hash.wrapping_mul(FNV32_PRIME) ^ u32::from(b.to_ascii_lowercase())
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn memihash_is_case_insensitive() {
        assert_eq!(memihash(b"ABC/def"), memihash(b"abc/DEF"));
        assert_eq!(memihash(b"Mixed/Case.txt"), memihash(b"mixed/case.txt"));
        assert_ne!(memihash(b"abc"), memihash(b"abd"));
    }

    #[test]
    fn memihash_matches_fnv1a_of_lowercased_bytes() {
        assert_eq!(memihash(b""), FNV32_BASE);
        let expected = (FNV32_BASE.wrapping_mul(FNV32_PRIME)) ^ u32::from(b'a');
        assert_eq!(memihash(b"A"), expected);
    }
}

//! Changed-path Bloom filters for commit-graph files (Git `bloom.c` compatible).

use std::collections::BTreeSet;
use thiserror::Error;

/// Settings stored in the BDAT chunk header and used for hashing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BloomFilterSettings {
    pub hash_version: u32,
    pub num_hashes: u32,
    pub bits_per_entry: u32,
    pub max_changed_paths: u32,
}

impl Default for BloomFilterSettings {
    fn default() -> Self {
        Self {
            hash_version: 1,
            num_hashes: 7,
            bits_per_entry: 10,
            max_changed_paths: 512,
        }
    }
}

const BITS_PER_WORD: u64 = 8;
pub const BLOOMDATA_HEADER_LEN: usize = 12;

#[inline]
fn rotate_left(value: u32, count: u32) -> u32 {
    value.rotate_left(count)
}

#[inline]
fn signed_char_u32(b: u8) -> u32 {
    ((b as i8) as i32) as u32
}

fn murmur3_seeded_v2(mut seed: u32, data: &[u8]) -> u32 {
    let c1: u32 = 0xcc9e2d51;
    let c2: u32 = 0x1b873593;
    let r1: u32 = 15;
    let r2: u32 = 13;
    let m: u32 = 5;
    let n: u32 = 0xe6546b64;

    let mut i = 0usize;
    while i + 4 <= data.len() {
        let mut k = (data[i] as u32)
            | ((data[i + 1] as u32) << 8)
            | ((data[i + 2] as u32) << 16)
            | ((data[i + 3] as u32) << 24);
        k = k.wrapping_mul(c1);
        k = rotate_left(k, r1);
        k = k.wrapping_mul(c2);
        seed ^= k;
        seed = rotate_left(seed, r2).wrapping_mul(m).wrapping_add(n);
        i += 4;
    }

    let tail = &data[i..];
    let mut k1: u32 = 0;
    match tail.len() {
        3 => {
            k1 ^= (tail[2] as u32) << 16;
            k1 ^= (tail[1] as u32) << 8;
            k1 ^= tail[0] as u32;
        }
        2 => {
            k1 ^= (tail[1] as u32) << 8;
            k1 ^= tail[0] as u32;
        }
        1 => {
            k1 ^= tail[0] as u32;
        }
        _ => {}
    }
    if !tail.is_empty() {
        k1 = k1.wrapping_mul(c1);
        k1 = rotate_left(k1, r1);
        k1 = k1.wrapping_mul(c2);
        seed ^= k1;
    }

    seed ^= data.len() as u32;
    seed ^= seed >> 16;
    seed = seed.wrapping_mul(0x85ebca6b);
    seed ^= seed >> 13;
    seed = seed.wrapping_mul(0xc2b2ae35);
    seed ^= seed >> 16;
    seed
}

fn murmur3_seeded_v1(mut seed: u32, data: &[u8]) -> u32 {
    let c1: u32 = 0xcc9e2d51;
    let c2: u32 = 0x1b873593;
    let r1: u32 = 15;
    let r2: u32 = 13;
    let m: u32 = 5;
    let n: u32 = 0xe6546b64;

    let mut i = 0usize;
    while i + 4 <= data.len() {
        let mut k = signed_char_u32(data[i])
            | (signed_char_u32(data[i + 1]) << 8)
            | (signed_char_u32(data[i + 2]) << 16)
            | (signed_char_u32(data[i + 3]) << 24);
        k = k.wrapping_mul(c1);
        k = rotate_left(k, r1);
        k = k.wrapping_mul(c2);
        seed ^= k;
        seed = rotate_left(seed, r2).wrapping_mul(m).wrapping_add(n);
        i += 4;
    }

    let tail = &data[i..];
    let mut k1: u32 = 0;
    match tail.len() {
        3 => {
            k1 ^= signed_char_u32(tail[2]) << 16;
            k1 ^= signed_char_u32(tail[1]) << 8;
            k1 ^= signed_char_u32(tail[0]);
        }
        2 => {
            k1 ^= signed_char_u32(tail[1]) << 8;
            k1 ^= signed_char_u32(tail[0]);
        }
        1 => {
            k1 ^= signed_char_u32(tail[0]);
        }
        _ => {}
    }
    if !tail.is_empty() {
        k1 = k1.wrapping_mul(c1);
        k1 = rotate_left(k1, r1);
        k1 = k1.wrapping_mul(c2);
        seed ^= k1;
    }

    seed ^= data.len() as u32;
    seed ^= seed >> 16;
    seed = seed.wrapping_mul(0x85ebca6b);
    seed ^= seed >> 13;
    seed = seed.wrapping_mul(0xc2b2ae35);
    seed ^= seed >> 16;
    seed
}

fn murmur3_seeded(seed: u32, data: &[u8], hash_version: u32) -> u32 {
    match hash_version {
        2 => murmur3_seeded_v2(seed, data),
        _ => murmur3_seeded_v1(seed, data),
    }
}

/// Hash values for one path string (same length as `settings.num_hashes`).
pub fn bloom_key_hashes(data: &[u8], settings: &BloomFilterSettings) -> Vec<u32> {
    let seed0 = 0x293ae76f_u32;
    let seed1 = 0x7e646e2c_u32;
    let hash0 = murmur3_seeded(seed0, data, settings.hash_version);
    let hash1 = murmur3_seeded(seed1, data, settings.hash_version);
    let n = settings.num_hashes as usize;
    let mut out = Vec::with_capacity(n);
    for i in 0..n {
        out.push(hash0.wrapping_add((i as u32).wrapping_mul(hash1)));
    }
    out
}

#[inline]
fn get_bitmask(pos: u32) -> u8 {
    1u8 << (pos & (BITS_PER_WORD as u32 - 1))
}

fn add_key_to_filter(key: &[u32], filter: &mut [u8], settings: &BloomFilterSettings) {
    let mod_bits = (filter.len() as u64).saturating_mul(BITS_PER_WORD);
    if mod_bits == 0 {
        return;
    }
    for i in 0..settings.num_hashes as usize {
        let Some(&h) = key.get(i) else {
            break;
        };
        let hash_mod = (h as u64) % mod_bits;
        let block_pos = (hash_mod / BITS_PER_WORD) as usize;
        let bitmask = get_bitmask(hash_mod as u32);
        if let Some(byte) = filter.get_mut(block_pos) {
            *byte |= bitmask;
        }
    }
}

/// Invalid or missing Bloom filter data (zero-length filter, etc.).
#[derive(Copy, Clone, Debug, Eq, PartialEq, Error)]
#[error("invalid or empty Bloom filter")]
pub struct BloomFilterInvalid;

/// Returns `Ok(true)` if all bits for every key are set, `Ok(false)` if definitely not,
/// [`Err(BloomFilterInvalid)`] if the filter has zero length (missing / invalid).
pub fn bloom_filter_contains(
    key: &[u32],
    filter: &[u8],
    settings: &BloomFilterSettings,
) -> Result<bool, BloomFilterInvalid> {
    let mod_bits = (filter.len() as u64).saturating_mul(BITS_PER_WORD);
    if mod_bits == 0 {
        return Err(BloomFilterInvalid);
    }
    for i in 0..settings.num_hashes as usize {
        let Some(&h) = key.get(i) else {
            return Err(BloomFilterInvalid);
        };
        let hash_mod = (h as u64) % mod_bits;
        let block_pos = (hash_mod / BITS_PER_WORD) as usize;
        let bitmask = get_bitmask(hash_mod as u32);
        let Some(byte) = filter.get(block_pos) else {
            return Err(BloomFilterInvalid);
        };
        if *byte & bitmask == 0 {
            return Ok(false);
        }
    }
    Ok(true)
}

/// Build keys for `path` and every directory prefix (Git `bloom_keyvec_new`).
pub fn bloom_keyvec_for_path(path: &str, settings: &BloomFilterSettings) -> Vec<Vec<u32>> {
    if path.is_empty() {
        return Vec::new();
    }
    let bytes = path.as_bytes();
    let mut out = Vec::new();
    out.push(bloom_key_hashes(bytes, settings));
    let mut end = bytes.len();
    while end > 0 {
        let Some(pos) = path[..end].rfind('/') else {
            break;
        };
        if pos == 0 {
            break;
        }
        out.push(bloom_key_hashes(&bytes[..pos], settings));
        end = pos;
    }
    out
}

/// Every distinct path and parent directory for diff paths (Git `pathmap` order doesn't matter).
pub fn collect_changed_paths_for_bloom(paths: &[String]) -> BTreeSet<String> {
    let mut set = BTreeSet::new();
    for p in paths {
        let mut cur = p.clone();
        loop {
            set.insert(cur.clone());
            let Some(pos) = cur.rfind('/') else {
                break;
            };
            cur.truncate(pos);
            if cur.is_empty() {
                break;
            }
        }
    }
    set
}

/// Result of building a Bloom filter payload for one commit.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BloomBuildOutcome {
    Normal,
    TruncatedLarge,
    TruncatedEmpty,
}

/// Compute on-disk filter bytes for a commit's changed paths.
///
/// `raw_diff_entries` is the diff queue length before path de-duplication (Git `diff_queued_diff.nr`).
pub fn build_bloom_filter_data(
    changed_paths: &BTreeSet<String>,
    raw_diff_entries: usize,
    settings: &BloomFilterSettings,
) -> (Vec<u8>, BloomBuildOutcome) {
    let max = settings.max_changed_paths as usize;
    if raw_diff_entries > max || changed_paths.len() > max {
        return (vec![0xff], BloomBuildOutcome::TruncatedLarge);
    }
    let n_unique = changed_paths.len();
    let bits_total = n_unique.saturating_mul(settings.bits_per_entry as usize);
    let mut word_len = bits_total.div_ceil(8);
    if word_len == 0 {
        word_len = 1;
    }
    let outcome = if n_unique == 0 {
        BloomBuildOutcome::TruncatedEmpty
    } else {
        BloomBuildOutcome::Normal
    };
    let mut data = vec![0u8; word_len];
    for p in changed_paths {
        let hashes = bloom_key_hashes(p.as_bytes(), settings);
        add_key_to_filter(&hashes, &mut data, settings);
    }
    (data, outcome)
}

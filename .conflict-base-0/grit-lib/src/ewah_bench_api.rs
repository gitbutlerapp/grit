// Hidden helpers for `benches/ewah.rs` (not part of the stable library API).

use std::sync::OnceLock;

use crate::ewah_bitmap::{Bitmap, EwahBitmap, EwahView};

const GIT_DOT_GIT_BITS: usize = 420_000;

fn build_git_sized() -> (Vec<u8>, Bitmap) {
    let mut bitmap = Bitmap::new();
    let mut rng: u64 = 0xC0FFEE;
    let mut next = || {
        rng = rng.wrapping_mul(6364136223846793005).wrapping_add(1);
        rng
    };

    for word_idx in 0..GIT_DOT_GIT_BITS.div_ceil(64) {
        for bit in 0..64 {
            let pos = word_idx * 64 + bit;
            if pos >= GIT_DOT_GIT_BITS {
                break;
            }
            let roll = (next() % 1000) as u32;
            let set = match pos % 4096 {
                0..=63 => roll < 850,
                64..=511 => roll < 120,
                _ => roll < 45,
            };
            if set {
                bitmap.set(pos);
            }
        }
    }

    let ewah = EwahBitmap::from_bitmap(&bitmap);
    let mut bytes = Vec::new();
    ewah.serialize(&mut bytes);
    (bytes, bitmap)
}

fn fixture() -> &'static (Vec<u8>, Bitmap) {
    // hygiene: one-shot cache for Criterion fixture bytes and base bitmap
    static FIXTURE: OnceLock<(Vec<u8>, Bitmap)> = OnceLock::new();
    FIXTURE.get_or_init(build_git_sized)
}

fn parse_fixture(bytes: &[u8]) -> EwahView<'_> {
    match EwahView::parse(bytes) {
        Ok((view, _)) => view,
        Err(err) => panic!("fixture ewah: {err}"),
    }
}

/// Returns a clone of the cached Git-sized EWAH serialization used by benchmarks.
pub fn git_sized_sample() -> Vec<u8> {
    fixture().0.clone()
}

/// Cached EWAH bytes representing a sparse bitmap roughly the size of a large `.git` index.
pub fn git_sized_bytes() -> &'static [u8] {
    &fixture().0
}

/// Expands `bytes` into a dense bitmap and returns its set-bit count (benchmark body).
pub fn expand_fixture(bytes: &[u8]) -> usize {
    let view = parse_fixture(bytes);
    let mut out = Bitmap::new();
    match view.expand_into(&mut out) {
        Ok(()) => out.count_ones(),
        Err(err) => panic!("expand: {err}"),
    }
}

/// ORs the cached EWAH view into the cached base bitmap and returns the popcount.
pub fn or_into_fixture() -> usize {
    let (bytes, base) = fixture();
    let view = parse_fixture(bytes);
    let mut acc = base.clone();
    match view.or_into(&mut acc) {
        Ok(()) => acc.count_ones(),
        Err(err) => panic!("or_into: {err}"),
    }
}

/// Returns the EWAH popcount for `bytes` without materializing a dense bitmap.
pub fn popcount_fixture(bytes: &[u8]) -> usize {
    let view = parse_fixture(bytes);
    match view.count_ones() {
        Ok(n) => n,
        Err(err) => panic!("popcount: {err}"),
    }
}

#[cfg(test)]
mod coverage_smoke {
    use super::{expand_fixture, git_sized_bytes, git_sized_sample, or_into_fixture, popcount_fixture};

    #[test]
    fn ewah_bench_fixture_helpers() {
        let bytes = git_sized_bytes();
        assert!(!bytes.is_empty());
        let expanded = expand_fixture(bytes);
        let merged = or_into_fixture();
        let pop = popcount_fixture(bytes);
        assert!(expanded > 0);
        assert!(merged > 0);
        assert_eq!(pop, expanded);
        assert_eq!(git_sized_sample(), bytes);
    }
}

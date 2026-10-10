// Hidden helpers for `benches/ewah.rs` (not part of the stable library API).

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

fn parse_fixture(bytes: &[u8]) -> EwahView<'_> {
    match EwahView::parse(bytes) {
        Ok((view, _)) => view,
        Err(err) => panic!("fixture ewah: {err}"),
    }
}

#[doc(hidden)]
pub fn git_sized_sample() -> Vec<u8> {
    build_git_sized().0
}

#[doc(hidden)]
pub fn expand_fixture(bytes: &[u8]) -> usize {
    let view = parse_fixture(bytes);
    let mut out = Bitmap::new();
    match view.expand_into(&mut out) {
        Ok(()) => out.count_ones(),
        Err(err) => panic!("expand: {err}"),
    }
}

#[doc(hidden)]
pub fn or_into_fixture(bytes: &[u8]) -> usize {
    let (_, base) = build_git_sized();
    let view = parse_fixture(bytes);
    let mut acc = base.clone();
    match view.or_into(&mut acc) {
        Ok(()) => acc.count_ones(),
        Err(err) => panic!("or_into: {err}"),
    }
}

#[doc(hidden)]
pub fn popcount_fixture(bytes: &[u8]) -> usize {
    let view = parse_fixture(bytes);
    match view.count_ones() {
        Ok(n) => n,
        Err(err) => panic!("popcount: {err}"),
    }
}

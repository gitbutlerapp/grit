//! Git-compatible EWAH bitmap serialization used by index extensions and pack bitmaps.
#![allow(dead_code)] // Pack/MIDX bitmap readers consume `Bitmap` / `EwahView` in a follow-up step.
//!
//! Layout matches Git's on-disk EWAH format (bit size, word count, big-endian u64 words,
//! RLW index). See `gitformat-pack` bitmap sections and the Enhanced Word-Aligned Hybrid
//! (EWAH) paper for the compression scheme.

use thiserror::Error;

type Eword = u64;
const BITS_IN_EWORD: usize = 64;
const RLW_RUNNING_BITS: usize = 32;
const RLW_LITERAL_BITS: usize = BITS_IN_EWORD - 1 - RLW_RUNNING_BITS;
const RLW_LARGEST_RUNNING_COUNT: Eword = (1u64 << RLW_RUNNING_BITS) - 1;
const RLW_LARGEST_LITERAL_COUNT: Eword = (1u64 << RLW_LITERAL_BITS) - 1;
const RLW_LARGEST_RUNNING_COUNT_SHIFT: Eword = RLW_LARGEST_RUNNING_COUNT << 1;
const RLW_RUNNING_LEN_PLUS_BIT: Eword = (1u64 << (RLW_RUNNING_BITS + 1)) - 1;

/// Errors while parsing or validating serialized EWAH data.
#[derive(Debug, Error, PartialEq, Eq)]
pub(crate) enum EwahError {
    #[error("ewah bitmap: truncated header")]
    TruncatedHeader,
    #[error("ewah bitmap: truncated compressed words")]
    TruncatedWords,
    #[error("ewah bitmap: truncated rlw index")]
    TruncatedRlwIndex,
    #[error("ewah bitmap: invalid rlw word index {index} (buffer has {buffer_size} words)")]
    InvalidRlwIndex { index: usize, buffer_size: usize },
    #[error("ewah bitmap: word count overflow")]
    WordCountOverflow,
}

#[inline]
fn rlw_get_run_bit(word: Eword) -> bool {
    word & 1 != 0
}

#[inline]
fn rlw_set_run_bit(word: &mut Eword, b: bool) {
    if b {
        *word |= 1;
    } else {
        *word &= !1u64;
    }
}

#[inline]
fn rlw_set_running_len(word: &mut Eword, l: Eword) {
    *word |= RLW_LARGEST_RUNNING_COUNT_SHIFT;
    *word &= (l << 1) | !RLW_LARGEST_RUNNING_COUNT_SHIFT;
}

#[inline]
fn rlw_get_running_len(word: Eword) -> Eword {
    (word >> 1) & RLW_LARGEST_RUNNING_COUNT
}

#[inline]
fn rlw_get_literal_words(word: Eword) -> Eword {
    word >> (1 + RLW_RUNNING_BITS)
}

#[inline]
fn rlw_set_literal_words(word: &mut Eword, l: Eword) {
    *word |= !RLW_RUNNING_LEN_PLUS_BIT;
    *word &= (l << (RLW_RUNNING_BITS + 1)) | RLW_RUNNING_LEN_PLUS_BIT;
}

#[inline]
fn rlw_size(word: Eword) -> Eword {
    rlw_get_running_len(word) + rlw_get_literal_words(word)
}

fn min_sz(a: usize, b: usize) -> usize {
    if a < b {
        a
    } else {
        b
    }
}

fn max_sz(a: usize, b: usize) -> usize {
    if a > b {
        a
    } else {
        b
    }
}

fn read_be32(data: &[u8]) -> Result<u32, EwahError> {
    data.get(..4)
        .ok_or(EwahError::TruncatedHeader)
        .and_then(|s| s.try_into().map_err(|_| EwahError::TruncatedHeader))
        .map(u32::from_be_bytes)
}

fn read_be64(data: &[u8]) -> Result<u64, EwahError> {
    data.get(..8)
        .ok_or(EwahError::TruncatedWords)
        .and_then(|s| s.try_into().map_err(|_| EwahError::TruncatedWords))
        .map(u64::from_be_bytes)
}

/// Uncompressed bitmap stored as native-endian u64 words (Git `struct bitmap`).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct Bitmap {
    words: Vec<Eword>,
}

impl Bitmap {
    /// Empty bitmap with no allocated words.
    pub(crate) fn new() -> Self {
        Self { words: Vec::new() }
    }

    /// Number of allocated u64 words (Git `word_alloc`).
    pub(crate) fn word_len(&self) -> usize {
        self.words.len()
    }

    fn grow_words(&mut self, word_alloc: usize) {
        if self.words.len() < word_alloc {
            self.words.resize(word_alloc, 0);
        }
    }

    /// Set bit `pos`, growing the word vector as needed.
    pub(crate) fn set(&mut self, pos: usize) {
        let block = pos / BITS_IN_EWORD;
        self.grow_words(block + 1);
        self.words[block] |= 1u64 << (pos % BITS_IN_EWORD);
    }

    /// Clear bit `pos` if it lies within the allocated word range.
    pub(crate) fn clear(&mut self, pos: usize) {
        let block = pos / BITS_IN_EWORD;
        if block < self.words.len() {
            self.words[block] &= !(1u64 << (pos % BITS_IN_EWORD));
        }
    }

    /// Returns whether bit `pos` is set.
    pub(crate) fn get(&self, pos: usize) -> bool {
        let block = pos / BITS_IN_EWORD;
        block < self.words.len() && (self.words[block] & (1u64 << (pos % BITS_IN_EWORD))) != 0
    }

    /// In-place bitwise OR with `other`, growing to cover `other`'s words.
    pub(crate) fn or_assign(&mut self, other: &Self) {
        self.grow_words(other.words.len());
        for (a, b) in self.words.iter_mut().zip(other.words.iter()) {
            *a |= *b;
        }
    }

    /// In-place bitwise AND with `other` over the overlapping word prefix.
    pub(crate) fn and_assign(&mut self, other: &Self) {
        let common = min_sz(self.words.len(), other.words.len());
        for i in 0..common {
            self.words[i] &= other.words[i];
        }
        self.words.truncate(common);
    }

    /// In-place `self &= !other` over the overlapping prefix (Git `bitmap_and_not`).
    pub(crate) fn and_not_assign(&mut self, other: &Self) {
        let common = min_sz(self.words.len(), other.words.len());
        for i in 0..common {
            self.words[i] &= !other.words[i];
        }
    }

    /// In-place bitwise XOR over the overlapping prefix, growing for longer `other`.
    pub(crate) fn xor_assign(&mut self, other: &Self) {
        self.grow_words(other.words.len());
        for i in 0..other.words.len() {
            self.words[i] ^= other.words[i];
        }
    }

    /// Population count using hardware popcount on each word.
    pub(crate) fn count_ones(&self) -> usize {
        self.words.iter().map(|w| w.count_ones() as usize).sum()
    }

    /// Iterate set bit indices in ascending order.
    pub(crate) fn set_bits(&self) -> impl Iterator<Item = usize> + '_ {
        BitmapSetBitsIter {
            bitmap: self,
            word: 0,
            bit_in_word: 0,
        }
    }

    /// Build from an explicit list of set bit indices.
    pub(crate) fn from_set_bits(bits: impl IntoIterator<Item = usize>) -> Self {
        let mut b = Self::new();
        for i in bits {
            b.set(i);
        }
        b
    }
}

struct BitmapSetBitsIter<'a> {
    bitmap: &'a Bitmap,
    word: usize,
    bit_in_word: usize,
}

impl Iterator for BitmapSetBitsIter<'_> {
    type Item = usize;

    fn next(&mut self) -> Option<Self::Item> {
        while self.word < self.bitmap.words.len() {
            let w = self.bitmap.words[self.word];
            while self.bit_in_word < BITS_IN_EWORD {
                let mask = 1u64 << self.bit_in_word;
                if w & mask != 0 {
                    let pos = self.word * BITS_IN_EWORD + self.bit_in_word;
                    self.bit_in_word += 1;
                    return Some(pos);
                }
                self.bit_in_word += 1;
            }
            self.word += 1;
            self.bit_in_word = 0;
        }
        None
    }
}

/// Borrowed view over serialized EWAH bytes (big-endian), without copying compressed words.
#[derive(Debug, Clone, Copy)]
pub(crate) struct EwahView<'a> {
    data: &'a [u8],
    bit_size: usize,
    buffer_size: usize,
    rlw_index: usize,
}

impl<'a> EwahView<'a> {
    /// Parse and validate `data` as Git's EWAH serialization prefix.
    pub(crate) fn parse(data: &'a [u8]) -> Result<(Self, usize), EwahError> {
        let bit_size = read_be32(data)? as usize;
        let mut pos = 4;
        let buffer_size = read_be32(data.get(pos..).ok_or(EwahError::TruncatedHeader)?)? as usize;
        pos += 4;
        let words_bytes = buffer_size
            .checked_mul(8)
            .ok_or(EwahError::WordCountOverflow)?;
        let words_end = pos
            .checked_add(words_bytes)
            .ok_or(EwahError::WordCountOverflow)?;
        if data.len() < words_end + 4 {
            return Err(EwahError::TruncatedWords);
        }
        pos = words_end;
        let rlw_index = read_be32(data.get(pos..).ok_or(EwahError::TruncatedRlwIndex)?)? as usize;
        pos += 4;
        if buffer_size > 0 && rlw_index >= buffer_size {
            return Err(EwahError::InvalidRlwIndex {
                index: rlw_index,
                buffer_size,
            });
        }
        Ok((
            Self {
                data,
                bit_size,
                buffer_size,
                rlw_index,
            },
            pos,
        ))
    }

    pub(crate) fn bit_size(&self) -> usize {
        self.bit_size
    }

    fn word_be(&self, index: usize) -> Result<Eword, EwahError> {
        let start = 8 + index * 8;
        read_be64(self.data.get(start..).ok_or(EwahError::TruncatedWords)?)
    }

    /// Decompress into `out`, replacing prior contents.
    pub(crate) fn expand_into(&self, out: &mut Bitmap) -> Result<(), EwahError> {
        out.words.clear();
        let mut iter = EwahWordIter::new(self)?;
        while let Some(word) = iter.next_word()? {
            out.words.push(word);
        }
        Ok(())
    }

    /// OR decompressed words into `out` (Git `bitmap_or_ewah`), skipping all-zero runs cheaply.
    pub(crate) fn or_into(&self, out: &mut Bitmap) -> Result<(), EwahError> {
        let other_final = self.bit_size.div_ceil(BITS_IN_EWORD);
        out.grow_words(other_final);
        let mut i = 0usize;
        let mut iter = EwahWordIter::new(self)?;
        while let Some(word) = iter.next_word()? {
            if i < out.words.len() {
                out.words[i] |= word;
            }
            i += 1;
        }
        Ok(())
    }

    /// Population count without materializing an uncompressed bitmap.
    pub(crate) fn count_ones(&self) -> Result<usize, EwahError> {
        let mut count = 0usize;
        let mut pos = 0usize;
        let mut pointer = 0usize;
        while pointer < self.buffer_size {
            let rlw = self.word_be(pointer)?;
            if rlw_get_run_bit(rlw) {
                count += rlw_get_running_len(rlw) as usize * BITS_IN_EWORD;
            }
            pos += rlw_get_running_len(rlw) as usize * BITS_IN_EWORD;
            pointer += 1;
            let mut k = 0u64;
            while k < rlw_get_literal_words(rlw) {
                let lit = self.word_be(pointer)?;
                count += lit.count_ones() as usize;
                pos += BITS_IN_EWORD;
                pointer += 1;
                k += 1;
            }
            let _ = pos;
        }
        Ok(count)
    }

    /// XOR this bitmap with `other`, storing Git-compatible EWAH into `out`.
    pub(crate) fn xor_with(
        &self,
        other: &EwahView<'_>,
        out: &mut EwahBitmap,
    ) -> Result<(), EwahError> {
        let a = EwahBitmap::from_view(self)?;
        let b = EwahBitmap::from_view(other)?;
        *out = EwahBitmap::xor_ewah(&a, &b);
        Ok(())
    }
}

/// Expands compressed EWAH into logical u64 words (Git `ewah_iterator`).
struct EwahWordIter<'a> {
    view: &'a EwahView<'a>,
    pointer: usize,
    rl: u64,
    lw: u64,
    compressed: u64,
    literals: u64,
    run_bit: bool,
}

impl<'a> EwahWordIter<'a> {
    fn new(view: &'a EwahView<'a>) -> Result<Self, EwahError> {
        let mut it = Self {
            view,
            pointer: 0,
            rl: 0,
            lw: 0,
            compressed: 0,
            literals: 0,
            run_bit: false,
        };
        if it.pointer < view.buffer_size {
            it.read_new_rlw()?;
        }
        Ok(it)
    }

    fn read_new_rlw(&mut self) -> Result<(), EwahError> {
        self.literals = 0;
        self.compressed = 0;
        loop {
            let word = self.view.word_be(self.pointer)?;
            self.rl = rlw_get_running_len(word);
            self.lw = rlw_get_literal_words(word);
            self.run_bit = rlw_get_run_bit(word);
            if self.rl != 0 || self.lw != 0 {
                return Ok(());
            }
            if self.pointer < self.view.buffer_size - 1 {
                self.pointer += 1;
            } else {
                self.pointer = self.view.buffer_size;
                return Ok(());
            }
        }
    }

    fn next_word(&mut self) -> Result<Option<Eword>, EwahError> {
        if self.pointer >= self.view.buffer_size {
            return Ok(None);
        }
        let next = if self.compressed < self.rl {
            self.compressed += 1;
            if self.run_bit {
                !0u64
            } else {
                0
            }
        } else {
            self.literals += 1;
            self.pointer += 1;
            self.view.word_be(self.pointer)?
        };

        if self.compressed == self.rl && self.literals == self.lw {
            self.pointer += 1;
            if self.pointer < self.view.buffer_size {
                self.read_new_rlw()?;
            }
        }
        Ok(Some(next))
    }
}

/// In-memory EWAH bitmap matching Git's layout.
#[derive(Debug, Clone)]
pub(crate) struct EwahBitmap {
    buffer: Vec<Eword>,
    buffer_size: usize,
    rlw_index: usize,
    pub bit_size: usize,
}

impl EwahBitmap {
    pub(crate) fn new() -> Self {
        Self {
            buffer: vec![0; 32],
            buffer_size: 1,
            rlw_index: 0,
            bit_size: 0,
        }
    }

    #[inline]
    fn rlw_mut(&mut self) -> &mut Eword {
        &mut self.buffer[self.rlw_index]
    }

    fn buffer_grow(&mut self, new_size: usize) {
        if new_size > self.buffer.len() {
            let n = new_size.max(self.buffer.len() * 2);
            self.buffer.resize(n, 0);
        }
    }

    fn buffer_push(&mut self, value: Eword) {
        self.buffer_grow(self.buffer_size + 1);
        self.buffer[self.buffer_size] = value;
        self.buffer_size += 1;
    }

    fn buffer_push_rlw(&mut self, value: Eword) {
        self.buffer_push(value);
        self.rlw_index = self.buffer_size - 1;
    }

    fn add_empty_words_inner(&mut self, v: bool, mut number: usize) {
        let v_bit = v;
        let rlw = *self.rlw_mut();
        if rlw_get_run_bit(rlw) != v_bit && rlw_size(rlw) == 0 {
            rlw_set_run_bit(self.rlw_mut(), v_bit);
        } else if rlw_get_literal_words(rlw) != 0 || rlw_get_run_bit(rlw) != v_bit {
            self.buffer_push_rlw(0);
            if v_bit {
                rlw_set_run_bit(self.rlw_mut(), true);
            }
        }

        let runlen = rlw_get_running_len(*self.rlw_mut());
        let can_add = min_sz(number, (RLW_LARGEST_RUNNING_COUNT - runlen) as usize);
        rlw_set_running_len(self.rlw_mut(), runlen + can_add as Eword);
        number -= can_add;

        while number >= RLW_LARGEST_RUNNING_COUNT as usize {
            self.buffer_push_rlw(0);
            if v_bit {
                rlw_set_run_bit(self.rlw_mut(), true);
            }
            rlw_set_running_len(self.rlw_mut(), RLW_LARGEST_RUNNING_COUNT);
            number -= RLW_LARGEST_RUNNING_COUNT as usize;
        }

        if number > 0 {
            self.buffer_push_rlw(0);
            if v_bit {
                rlw_set_run_bit(self.rlw_mut(), true);
            }
            rlw_set_running_len(self.rlw_mut(), number as Eword);
        }
    }

    fn add_empty_word(&mut self, v: bool) -> usize {
        let rlw = *self.rlw_mut();
        let no_literal = rlw_get_literal_words(rlw) == 0;
        let run_len = rlw_get_running_len(rlw);

        if no_literal && run_len == 0 {
            rlw_set_run_bit(self.rlw_mut(), v);
        }

        if no_literal
            && rlw_get_run_bit(*self.rlw_mut()) == v
            && run_len < RLW_LARGEST_RUNNING_COUNT
        {
            rlw_set_running_len(self.rlw_mut(), run_len + 1);
            return 0;
        }

        self.buffer_push_rlw(0);
        rlw_set_run_bit(self.rlw_mut(), v);
        rlw_set_running_len(self.rlw_mut(), 1);
        1
    }

    fn add_literal(&mut self, new_data: Eword) -> usize {
        let current_num = rlw_get_literal_words(*self.rlw_mut());
        if current_num >= RLW_LARGEST_LITERAL_COUNT {
            self.buffer_push_rlw(0);
            rlw_set_literal_words(self.rlw_mut(), 1);
            self.buffer_push(new_data);
            return 2;
        }
        rlw_set_literal_words(self.rlw_mut(), current_num + 1);
        self.buffer_push(new_data);
        1
    }

    fn append_empty_words(&mut self, v: bool, number: usize) {
        if number == 0 {
            return;
        }
        self.bit_size += number * BITS_IN_EWORD;
        self.add_empty_words_inner(v, number);
    }

    fn append_word(&mut self, word: Eword) {
        self.bit_size += BITS_IN_EWORD;
        if word == 0 {
            let _ = self.add_empty_word(false);
        } else if word == !0 {
            let _ = self.add_empty_word(true);
        } else {
            let _ = self.add_literal(word);
        }
    }

    fn add_dirty_words(&mut self, buffer: &[Eword], number: usize, negate: bool) {
        let mut offset = 0usize;
        let mut remaining = number;
        while remaining > 0 {
            let literals = rlw_get_literal_words(*self.rlw_mut());
            let can_add = min_sz(remaining, (RLW_LARGEST_LITERAL_COUNT - literals) as usize);
            rlw_set_literal_words(self.rlw_mut(), literals + can_add as Eword);
            self.buffer_grow(self.buffer_size + can_add);
            for i in 0..can_add {
                let w = if negate {
                    !buffer[offset + i]
                } else {
                    buffer[offset + i]
                };
                self.buffer[self.buffer_size] = w;
                self.buffer_size += 1;
            }
            self.bit_size += can_add * BITS_IN_EWORD;
            remaining -= can_add;
            offset += can_add;
            if remaining > 0 {
                self.buffer_push_rlw(0);
            }
        }
    }

    /// Compress an uncompressed bitmap (Git `bitmap_to_ewah`).
    pub(crate) fn from_bitmap(bitmap: &Bitmap) -> Self {
        let mut ewah = Self::new();
        let mut running_empty = 0usize;
        let mut last_word: Eword = 0;
        for &word in &bitmap.words {
            if word == 0 {
                running_empty += 1;
                continue;
            }
            if last_word != 0 {
                ewah.append_word(last_word);
            }
            if running_empty > 0 {
                ewah.append_empty_words(false, running_empty);
                running_empty = 0;
            }
            last_word = word;
        }
        ewah.append_word(last_word);
        // Index extensions build EWAH with `set_bit_extend`, so `bit_size` is the highest
        // set bit plus one, not `word_alloc * 64` from a full word scan (Git `bitmap_to_ewah`).
        ewah.bit_size = Self::highest_set_bit_plus_one(bitmap);
        ewah
    }

    fn highest_set_bit_plus_one(bitmap: &Bitmap) -> usize {
        for (wi, &w) in bitmap.words.iter().enumerate().rev() {
            if w != 0 {
                return wi * BITS_IN_EWORD + (BITS_IN_EWORD - w.leading_zeros() as usize);
            }
        }
        0
    }

    fn from_view(view: &EwahView<'_>) -> Result<Self, EwahError> {
        let mut ewah = Self::new();
        ewah.bit_size = view.bit_size;
        ewah.buffer_size = 0;
        ewah.buffer_grow(view.buffer_size.max(1));
        for i in 0..view.buffer_size {
            ewah.buffer[i] = view.word_be(i)?;
        }
        ewah.buffer_size = view.buffer_size;
        ewah.rlw_index = if view.buffer_size > 0 {
            view.rlw_index
        } else {
            0
        };
        if ewah.buffer_size == 0 {
            ewah.buffer_size = 1;
            ewah.buffer[0] = 0;
            ewah.rlw_index = 0;
        }
        Ok(ewah)
    }

    /// XOR two EWAH bitmaps (Git `ewah_xor`).
    pub(crate) fn xor_ewah(a: &Self, b: &Self) -> Self {
        let mut out = Self::new();
        let mut rlw_a = RlwIter::new(a);
        let mut rlw_b = RlwIter::new(b);

        while rlw_a.word_size() > 0 && rlw_b.word_size() > 0 {
            while rlw_a.running_len() > 0 || rlw_b.running_len() > 0 {
                let (prey, predator) = if rlw_a.running_len() < rlw_b.running_len() {
                    (&mut rlw_a, &mut rlw_b)
                } else {
                    (&mut rlw_b, &mut rlw_a)
                };
                let negate = predator.run_bit();
                let index = prey.discharge(&mut out, predator.running_len(), negate);
                out.append_empty_words(negate, predator.running_len() - index);
                predator.discard_first_words(predator.running_len());
            }

            let literals = min_sz(rlw_a.literal_words(), rlw_b.literal_words());
            if literals > 0 {
                for k in 0..literals {
                    let wa = rlw_a.literal_word(k);
                    let wb = rlw_b.literal_word(k);
                    out.append_word(wa ^ wb);
                }
                rlw_a.discard_first_words(literals);
                rlw_b.discard_first_words(literals);
            }
        }

        if rlw_a.word_size() > 0 {
            rlw_a.discharge(&mut out, usize::MAX, false);
        } else {
            rlw_b.discharge(&mut out, usize::MAX, false);
        }

        out.bit_size = max_sz(a.bit_size, b.bit_size);
        out
    }

    /// Set bit `i` where `i >= self.bit_size` (Git `ewah_set` append-only).
    pub(crate) fn set_bit_extend(&mut self, i: usize) {
        debug_assert!(i >= self.bit_size);
        let dist = (i + 1).div_ceil(BITS_IN_EWORD) - self.bit_size.div_ceil(BITS_IN_EWORD);
        self.bit_size = i + 1;
        if dist > 0 {
            if dist > 1 {
                self.add_empty_words_inner(false, dist - 1);
            }
            let _ = self.add_literal(1u64 << (i % BITS_IN_EWORD));
            return;
        }
        if rlw_get_literal_words(*self.rlw_mut()) == 0 {
            let rl = rlw_get_running_len(*self.rlw_mut());
            rlw_set_running_len(self.rlw_mut(), rl - 1);
            let _ = self.add_literal(1u64 << (i % BITS_IN_EWORD));
            return;
        }
        let last = self.buffer_size - 1;
        self.buffer[last] |= 1u64 << (i % BITS_IN_EWORD);
        if self.buffer[last] == !0u64 {
            self.buffer_size -= 1;
            let rlw_i = self.rlw_index;
            let prev_lit = rlw_get_literal_words(self.buffer[rlw_i]);
            rlw_set_literal_words(&mut self.buffer[rlw_i], prev_lit - 1);
            let _ = self.add_empty_word(true);
        }
    }

    pub(crate) fn serialize(&self, out: &mut Vec<u8>) {
        let bitsize = (self.bit_size as u32).to_be_bytes();
        out.extend_from_slice(&bitsize);
        let word_count = (self.buffer_size as u32).to_be_bytes();
        out.extend_from_slice(&word_count);
        for w in self.buffer.iter().take(self.buffer_size) {
            out.extend_from_slice(&w.to_be_bytes());
        }
        let rlw_pos = (self.rlw_index as u32).to_be_bytes();
        out.extend_from_slice(&rlw_pos);
    }

    /// Deserialize from `data`; returns bytes consumed.
    pub(crate) fn deserialize_prefix(data: &[u8]) -> Option<(Self, usize)> {
        Self::deserialize_prefix_result(data).ok()
    }

    pub(crate) fn deserialize_prefix_result(data: &[u8]) -> Result<(Self, usize), EwahError> {
        let (view, consumed) = EwahView::parse(data)?;
        let ewah = Self::from_view(&view)?;
        Ok((ewah, consumed))
    }

    /// Iterate set bits (`ewah_each_bit`).
    pub(crate) fn each_set_bit(&self, mut f: impl FnMut(usize)) {
        let mut pos = 0usize;
        let mut pointer = 0usize;
        while pointer < self.buffer_size {
            let word = self.buffer[pointer];
            if rlw_get_run_bit(word) {
                let len = rlw_get_running_len(word) as usize * BITS_IN_EWORD;
                for k in 0..len {
                    f(pos + k);
                }
                pos += len;
            } else {
                pos += rlw_get_running_len(word) as usize * BITS_IN_EWORD;
            }
            pointer += 1;
            let mut k = 0u64;
            while k < rlw_get_literal_words(word) {
                let lit = self.buffer[pointer];
                for c in 0..BITS_IN_EWORD {
                    if lit & (1u64 << c) != 0 {
                        f(pos + c);
                    }
                }
                pos += BITS_IN_EWORD;
                pointer += 1;
                k += 1;
            }
        }
    }

    pub(crate) fn count_ones(&self) -> usize {
        let mut count = 0usize;
        self.each_set_bit(|_| count += 1);
        count
    }
}

struct RlwIter<'a> {
    buffer: &'a [Eword],
    size: usize,
    pointer: usize,
    running_len: usize,
    literal_words: usize,
    run_bit: bool,
    literal_word_start: usize,
}

impl<'a> RlwIter<'a> {
    fn new(parent: &'a EwahBitmap) -> Self {
        let mut it = Self {
            buffer: &parent.buffer[..parent.buffer_size],
            size: parent.buffer_size,
            pointer: 0,
            running_len: 0,
            literal_words: 0,
            run_bit: false,
            literal_word_start: 0,
        };
        let _ = it.next_word();
        it
    }

    fn next_word(&mut self) -> bool {
        if self.pointer >= self.size {
            return false;
        }
        let word = self.buffer[self.pointer];
        self.literal_words = rlw_get_literal_words(word) as usize;
        self.running_len = rlw_get_running_len(word) as usize;
        self.run_bit = rlw_get_run_bit(word);
        self.literal_word_start = self.pointer + 1;
        self.pointer += self.literal_words + 1;
        true
    }

    fn word_size(&self) -> usize {
        self.running_len + self.literal_words
    }

    fn running_len(&self) -> usize {
        self.running_len
    }

    fn literal_words(&self) -> usize {
        self.literal_words
    }

    fn run_bit(&self) -> bool {
        self.run_bit
    }

    fn literal_word(&self, k: usize) -> Eword {
        self.buffer[self.literal_word_start + k]
    }

    fn discard_first_words(&mut self, mut x: usize) {
        while x > 0 {
            if self.running_len > x {
                self.running_len -= x;
                return;
            }
            x -= self.running_len;
            self.running_len = 0;
            let discard = min_sz(x, self.literal_words);
            self.literal_word_start += discard;
            self.literal_words -= discard;
            x -= discard;
            if (x > 0 || self.word_size() == 0) && !self.next_word() {
                break;
            }
        }
    }

    fn discharge(&mut self, out: &mut EwahBitmap, max: usize, negate: bool) -> usize {
        let mut index = 0usize;
        while index < max && self.word_size() > 0 {
            let mut pl = self.running_len;
            if index + pl > max {
                pl = max - index;
            }
            out.append_empty_words(self.run_bit ^ negate, pl);
            index += pl;

            let mut pd = self.literal_words;
            if pd + index > max {
                pd = max - index;
            }
            if pd > 0 {
                let slice = &self.buffer[self.literal_word_start..self.literal_word_start + pd];
                out.add_dirty_words(slice, pd, negate);
            }
            self.discard_first_words(pd + pl);
            index += pd;
        }
        index
    }
}

impl Default for EwahBitmap {
    fn default() -> Self {
        Self::new()
    }
}

/// Whether two EWAH bitmaps encode the same set bits (Git-compatible equality for FSMN).
pub(crate) fn ewah_bitmaps_equal(a: &EwahBitmap, b: &EwahBitmap) -> bool {
    if a.bit_size != b.bit_size {
        return false;
    }
    let mut a_bits = Vec::new();
    a.each_set_bit(|i| a_bits.push(i));
    let mut b_bits = Vec::new();
    b.each_set_bit(|i| b_bits.push(i));
    a_bits == b_bits
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use super::*;

    fn naive_from_ewah(ewah: &EwahBitmap) -> BTreeSet<usize> {
        let mut s = BTreeSet::new();
        ewah.each_set_bit(|i| {
            s.insert(i);
        });
        s
    }

    fn append_build(bits: &[usize]) -> EwahBitmap {
        let mut e = EwahBitmap::new();
        for &b in bits {
            if b >= e.bit_size {
                e.set_bit_extend(b);
            }
        }
        e
    }

    #[test]
    fn serialize_append_matches_from_bitmap() {
        let bits: Vec<usize> = (0..500).map(|i| i * 3).collect();
        let append = append_build(&bits);
        let mut append_bytes = Vec::new();
        append.serialize(&mut append_bytes);

        let mut bitmap = Bitmap::new();
        for b in bits {
            bitmap.set(b);
        }
        let compressed = EwahBitmap::from_bitmap(&bitmap);
        let mut compressed_bytes = Vec::new();
        compressed.serialize(&mut compressed_bytes);

        assert_eq!(append_bytes, compressed_bytes);
    }

    #[test]
    fn roundtrip_view_expand() {
        let bits: Vec<usize> = vec![0, 1, 63, 64, 127, 1000, 1001, 65535];
        let ewah = append_build(&bits);
        let mut bytes = Vec::new();
        ewah.serialize(&mut bytes);
        let (view, _) = EwahView::parse(&bytes).expect("parse");
        let mut expanded = Bitmap::new();
        view.expand_into(&mut expanded).expect("expand");
        let len = bits.len();
        for b in &bits {
            assert!(expanded.get(*b));
        }
        assert_eq!(expanded.count_ones(), len);
    }

    #[test]
    fn popcount_matches_naive() {
        let ewah = append_build(&(0..200).filter(|i| i % 7 != 0).collect::<Vec<_>>());
        let mut bytes = Vec::new();
        ewah.serialize(&mut bytes);
        let (view, _) = EwahView::parse(&bytes).unwrap();
        assert_eq!(view.count_ones().unwrap(), ewah.count_ones());
    }

    #[test]
    fn bitmap_ops_match_btree() {
        let mut rng_state: u64 = 0xDEAD_BEEF;
        let mut next = || {
            rng_state = rng_state.wrapping_mul(6364136223846793005).wrapping_add(1);
            rng_state
        };

        for _ in 0..50 {
            let mut naive_a = BTreeSet::new();
            let mut naive_b = BTreeSet::new();
            let mut a = Bitmap::new();
            let mut b = Bitmap::new();
            let n = (next() % 80) as usize + 10;
            for _ in 0..n {
                let bit = (next() % 512) as usize;
                if next() & 1 == 0 {
                    naive_a.insert(bit);
                    a.set(bit);
                } else {
                    naive_b.insert(bit);
                    b.set(bit);
                }
            }

            let mut or_a = a.clone();
            or_a.or_assign(&b);
            let naive_or: BTreeSet<_> = naive_a.union(&naive_b).copied().collect();
            assert_eq!(or_a.set_bits().collect::<BTreeSet<_>>(), naive_or);

            let mut and_a = a.clone();
            and_a.and_assign(&b);
            let naive_and: BTreeSet<_> = naive_a.intersection(&naive_b).copied().collect();
            assert_eq!(and_a.set_bits().collect::<BTreeSet<_>>(), naive_and);

            let mut xor_a = a.clone();
            xor_a.xor_assign(&b);
            let naive_xor: BTreeSet<_> = naive_a.symmetric_difference(&naive_b).copied().collect();
            assert_eq!(xor_a.set_bits().collect::<BTreeSet<_>>(), naive_xor);

            let mut and_not_a = a.clone();
            and_not_a.and_not_assign(&b);
            let naive_and_not: BTreeSet<_> = naive_a.difference(&naive_b).copied().collect();
            assert_eq!(and_not_a.set_bits().collect::<BTreeSet<_>>(), naive_and_not);
        }
    }

    #[test]
    fn compress_random_property() {
        let mut rng_state: u64 = 0x1234_5678;
        let mut next = || {
            rng_state = rng_state.wrapping_mul(1103515245).wrapping_add(12345);
            rng_state
        };

        for trial in 0..30 {
            let mut bitmap = Bitmap::new();
            let max_bit = 2000 + (trial * 37);
            let density = (next() % 100) as usize;
            for bit in 0..max_bit {
                if ((next() % 100) as usize) < density {
                    bitmap.set(bit);
                }
            }

            let ewah = EwahBitmap::from_bitmap(&bitmap);
            let mut bytes = Vec::new();
            ewah.serialize(&mut bytes);
            let (view, _) = EwahView::parse(&bytes).unwrap();
            let mut back = Bitmap::new();
            view.expand_into(&mut back).unwrap();

            assert_eq!(
                bitmap.set_bits().collect::<BTreeSet<_>>(),
                back.set_bits().collect::<BTreeSet<_>>()
            );
            assert_eq!(view.count_ones().unwrap(), bitmap.count_ones());
        }
    }

    #[test]
    fn run_heavy_literal_heavy_roundtrip() {
        for &pattern in &[0usize, 1, 63, 64, 65] {
            let mut bitmap = Bitmap::new();
            for block in 0..200 {
                let base = block * 64;
                match pattern {
                    0 => bitmap.set(base),
                    1 => {
                        for i in 0..64 {
                            bitmap.set(base + i);
                        }
                    }
                    63 => {
                        for i in (0..64).step_by(2) {
                            bitmap.set(base + i);
                        }
                    }
                    64 => {}
                    _ => {
                        if block % 3 == 0 {
                            for i in 0..64 {
                                bitmap.set(base + i);
                            }
                        }
                    }
                }
            }
            let ewah = EwahBitmap::from_bitmap(&bitmap);
            let mut bytes = Vec::new();
            ewah.serialize(&mut bytes);
            let (view, _) = EwahView::parse(&bytes).unwrap();
            let mut back = Bitmap::new();
            view.expand_into(&mut back).unwrap();
            assert_eq!(
                bitmap.set_bits().collect::<Vec<_>>(),
                back.set_bits().collect::<Vec<_>>()
            );
        }
    }

    #[test]
    fn or_into_matches_expand_or() {
        let a_bits: Vec<usize> = (0..300).step_by(5).collect();
        let b_bits: Vec<usize> = (0..300).step_by(7).collect();
        let mut a = Bitmap::from_set_bits(a_bits.iter().copied());
        let b_ewah = append_build(&b_bits);
        let mut b_bytes = Vec::new();
        b_ewah.serialize(&mut b_bytes);
        let (view, _) = EwahView::parse(&b_bytes).unwrap();

        let mut direct = a.clone();
        view.or_into(&mut direct).unwrap();

        let mut expanded = Bitmap::new();
        view.expand_into(&mut expanded).unwrap();
        a.or_assign(&expanded);

        assert_eq!(direct, a);
    }

    #[test]
    fn xor_with_matches_xor_ewah() {
        let a = append_build(&(0..400).step_by(11).collect::<Vec<_>>());
        let b = append_build(&(0..400).step_by(13).collect::<Vec<_>>());
        let mut a_bytes = Vec::new();
        let mut b_bytes = Vec::new();
        a.serialize(&mut a_bytes);
        b.serialize(&mut b_bytes);
        let (va, _) = EwahView::parse(&a_bytes).unwrap();
        let (vb, _) = EwahView::parse(&b_bytes).unwrap();
        let mut out = EwahBitmap::new();
        va.xor_with(&vb, &mut out).unwrap();
        let direct = EwahBitmap::xor_ewah(
            &EwahBitmap::deserialize_prefix(&a_bytes).unwrap().0,
            &EwahBitmap::deserialize_prefix(&b_bytes).unwrap().0,
        );
        assert!(ewah_bitmaps_equal(&out, &direct));
    }

    #[test]
    fn deserialize_errors_on_truncation() {
        let err = EwahBitmap::deserialize_prefix_result(&[0, 0, 0, 1]).unwrap_err();
        assert!(matches!(
            err,
            EwahError::TruncatedHeader | EwahError::TruncatedWords
        ));
    }
}

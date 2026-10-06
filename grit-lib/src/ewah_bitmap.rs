//! Git-compatible EWAH bitmap serialization (`git/ewah/ewah_io.c` + `ewah_bitmap.c`).
//! Used by the index UNTR extension (`dir.c`).

type Eword = u64;
const BITS_IN_EWORD: usize = 64;
const RLW_RUNNING_BITS: usize = 32;
const RLW_LITERAL_BITS: usize = BITS_IN_EWORD - 1 - RLW_RUNNING_BITS;
const RLW_LARGEST_RUNNING_COUNT: Eword = (1u64 << RLW_RUNNING_BITS) - 1;
const RLW_LARGEST_LITERAL_COUNT: Eword = (1u64 << RLW_LITERAL_BITS) - 1;
const RLW_LARGEST_RUNNING_COUNT_SHIFT: Eword = RLW_LARGEST_RUNNING_COUNT << 1;
const RLW_RUNNING_LEN_PLUS_BIT: Eword = (1u64 << (RLW_RUNNING_BITS + 1)) - 1;

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

    /// Append `number` words of all-0 or all-1 without changing `bit_size` (Git `add_empty_words`).
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
        // Git `ewah_serialize_to`: RLW offset is in 64-bit words, not bytes.
        let rlw_pos = (self.rlw_index as u32).to_be_bytes();
        out.extend_from_slice(&rlw_pos);
    }

    /// Deserialize from `data`; returns bytes consumed.
    pub(crate) fn deserialize_prefix(data: &[u8]) -> Option<(Self, usize)> {
        if data.len() < 8 {
            return None;
        }
        let bit_size = u32::from_be_bytes(data[0..4].try_into().ok()?) as usize;
        let mut pos = 4;
        let buffer_size = u32::from_be_bytes(data[pos..pos + 4].try_into().ok()?) as usize;
        pos += 4;
        let need = buffer_size.checked_mul(8)?;
        if data.len() < pos + need + 4 {
            return None;
        }
        let mut buffer = vec![0u64; buffer_size.max(32)];
        for i in 0..buffer_size {
            buffer[i] = u64::from_be_bytes(data[pos + i * 8..pos + i * 8 + 8].try_into().ok()?);
        }
        pos += need;
        let rlw_word_index = u32::from_be_bytes(data[pos..pos + 4].try_into().ok()?) as usize;
        pos += 4;
        // Git may serialize an empty bitmap (`buffer_size == 0`); RLW index is still present.
        if buffer_size > 0 && rlw_word_index >= buffer_size {
            return None;
        }
        Some((
            Self {
                buffer,
                buffer_size,
                rlw_index: rlw_word_index,
                bit_size,
            },
            pos,
        ))
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

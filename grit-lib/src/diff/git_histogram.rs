//! Git histogram line diff with whitespace-aware record comparison (`xdl_recmatch` semantics).

use crate::diff::WhitespaceMode;

const MAX_CHAIN_LENGTH: u32 = 64;

#[derive(Default, Clone, Copy)]
struct Region {
    begin1: usize,
    end1: usize,
    begin2: usize,
    end2: usize,
}

struct Record {
    ptr: usize,
    cnt: u32,
    next: Option<usize>,
}

struct HistIndex<'a> {
    old_lines: &'a [&'a str],
    new_lines: &'a [&'a str],
    ws: WhitespaceMode,
    table_bits: u32,
    records: Vec<Option<usize>>,
    record_pool: Vec<Record>,
    line_map: Vec<Option<usize>>,
    next_ptrs: Vec<usize>,
    ptr_shift: usize,
    max_chain_length: u32,
    cnt: u32,
    has_common: bool,
}

impl<'a> HistIndex<'a> {
    fn new(old_lines: &'a [&'a str], count1: usize, ws: WhitespaceMode) -> Self {
        let table_bits = hash_bits(count1);
        let records_size = 1usize << table_bits;
        Self {
            old_lines,
            new_lines: &[],
            ws,
            table_bits,
            records: vec![None; records_size],
            record_pool: Vec::new(),
            line_map: vec![None; count1],
            next_ptrs: vec![0; count1],
            ptr_shift: 0,
            max_chain_length: MAX_CHAIN_LENGTH,
            cnt: MAX_CHAIN_LENGTH + 1,
            has_common: false,
        }
    }

    fn table_hash(&self, side2_line: usize) -> usize {
        let line = self.new_lines[side2_line];
        let h = hash_long(&self.ws.normalize_line_for_compare(line));
        (h as usize) & ((1 << self.table_bits) - 1)
    }

    fn table_hash_side1(&self, line: usize) -> usize {
        let h = hash_long(&self.ws.normalize_line_for_compare(self.old_lines[line]));
        (h as usize) & ((1 << self.table_bits) - 1)
    }

    fn cmp(&self, side1_line: usize, side2_line: usize) -> bool {
        self.ws
            .normalize_line_for_compare(self.old_lines[side1_line])
            == self
                .ws
                .normalize_line_for_compare(self.new_lines[side2_line])
    }

    fn cmp_side1(&self, a: usize, b: usize) -> bool {
        self.ws.normalize_line_for_compare(self.old_lines[a])
            == self.ws.normalize_line_for_compare(self.old_lines[b])
    }

    fn scan_a(&mut self, line1: usize, count1: usize) -> bool {
        let end1 = line1 + count1;
        for ptr in (line1..end1).rev() {
            let tbl_idx = self.table_hash_side1(ptr);
            let mut chain_len = 0u32;
            let mut rec_idx = self.records[tbl_idx];
            let mut matched = false;
            while let Some(idx) = rec_idx {
                let rec = &self.record_pool[idx];
                if self.cmp_side1(rec.ptr, ptr) {
                    self.next_ptrs[ptr - self.ptr_shift] = rec.ptr;
                    self.record_pool[idx].ptr = ptr;
                    self.record_pool[idx].cnt =
                        self.record_pool[idx].cnt.saturating_add(1);
                    self.line_map[ptr - self.ptr_shift] = Some(idx);
                    matched = true;
                    break;
                }
                chain_len += 1;
                rec_idx = self.record_pool[idx].next;
            }
            if matched {
                continue;
            }
            if chain_len == self.max_chain_length {
                return true;
            }
            let new_rec = Record {
                ptr,
                cnt: 1,
                next: self.records[tbl_idx],
            };
            let new_idx = self.record_pool.len();
            self.record_pool.push(new_rec);
            self.records[tbl_idx] = Some(new_idx);
            self.line_map[ptr - self.ptr_shift] = Some(new_idx);
        }
        false
    }

    fn try_lcs(
        &mut self,
        lcs: &mut Region,
        b_ptr: usize,
        line1: usize,
        count1: usize,
        line2: usize,
        count2: usize,
    ) -> usize {
        let end2 = line2 + count2;
        let mut b_next = b_ptr + 1;
        let tbl_idx = self.table_hash(b_ptr);
        let mut rec_idx = self.records[tbl_idx];
        while let Some(idx) = rec_idx {
            let rec = &self.record_pool[idx];
            if rec.cnt > self.cnt {
                if !self.has_common {
                    self.has_common = self.cmp(rec.ptr, b_ptr);
                }
                rec_idx = self.record_pool[idx].next;
                continue;
            }
            let mut as_ = rec.ptr;
            if !self.cmp(as_, b_ptr) {
                rec_idx = self.record_pool[idx].next;
                continue;
            }
            self.has_common = true;
            loop {
                let mut np = self.next_ptrs[as_ - self.ptr_shift];
                let mut bs = b_ptr;
                let mut ae = as_;
                let mut be = bs;
                let mut rc = rec.cnt;
                let end1 = line1 + count1;

                while line1 < as_ && line2 < bs && self.cmp(as_ - 1, bs - 1) {
                    as_ -= 1;
                    bs -= 1;
                    if rc > 1 {
                        rc = rc.min(cnt_index(self, as_));
                    }
                }
                while ae + 1 < end1 && be + 1 < end2 && self.cmp(ae + 1, be + 1) {
                    ae += 1;
                    be += 1;
                    if rc > 1 {
                        rc = rc.min(cnt_index(self, ae));
                    }
                }

                if b_next <= be {
                    b_next = be + 1;
                }
                if lcs.end1.saturating_sub(lcs.begin1) < ae.saturating_sub(as_) || rc < self.cnt {
                    lcs.begin1 = as_;
                    lcs.begin2 = bs;
                    lcs.end1 = ae;
                    lcs.end2 = be;
                    self.cnt = rc;
                }

                if np == 0 {
                    break;
                }
                while np <= ae {
                    np = self.next_ptrs[np - self.ptr_shift];
                    if np == 0 {
                        break;
                    }
                }
                if np == 0 {
                    break;
                }
                as_ = np;
            }
            rec_idx = self.record_pool[idx].next;
        }
        b_next
    }
}

fn cnt_index(index: &HistIndex<'_>, line: usize) -> u32 {
    index
        .line_map
        .get(line.saturating_sub(index.ptr_shift))
        .and_then(|o| *o)
        .map(|idx| index.record_pool[idx].cnt)
        .unwrap_or(1)
}

fn hash_bits(count: usize) -> u32 {
    let mut bits = 0u32;
    let mut n = count.max(1);
    while n > 1 {
        bits += 1;
        n >>= 1;
    }
    bits.max(1)
}

fn hash_long(key: &str) -> u32 {
    crc32fast::hash(key.as_bytes())
}

fn find_lcs(
    index: &mut HistIndex<'_>,
    lcs: &mut Region,
    line1: usize,
    count1: usize,
    line2: usize,
    count2: usize,
) -> i32 {
    index.ptr_shift = line1;
    index.line_map.fill(None);
    index.next_ptrs.fill(0);
    if index.scan_a(line1, count1) {
        return -1;
    }
    index.cnt = index.max_chain_length + 1;
    let end2 = line2 + count2;
    let mut b_ptr = line2;
    while b_ptr < end2 {
        b_ptr = index.try_lcs(lcs, b_ptr, line1, count1, line2, count2);
    }
    if index.has_common && index.max_chain_length < index.cnt {
        1
    } else {
        0
    }
}

#[allow(clippy::too_many_arguments)]
fn fall_back_classic(
    old_lines: &[&str],
    new_lines: &[&str],
    changed1: &mut [bool],
    changed2: &mut [bool],
    line1: usize,
    count1: usize,
    line2: usize,
    count2: usize,
    ws: WhitespaceMode,
) {
    let o_slice = &old_lines[line1..line1 + count1];
    let n_slice = &new_lines[line2..line2 + count2];
    let (sub1, sub2) = super::xdiff_changed_flags_for_line_slices(o_slice, n_slice, ws, false);
    for (i, flag) in sub1.iter().enumerate() {
        if *flag {
            changed1[line1 + i] = true;
        }
    }
    for (j, flag) in sub2.iter().enumerate() {
        if *flag {
            changed2[line2 + j] = true;
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn histogram_diff(
    old_lines: &[&str],
    new_lines: &[&str],
    changed1: &mut [bool],
    changed2: &mut [bool],
    mut line1: usize,
    mut count1: usize,
    mut line2: usize,
    mut count2: usize,
    ws: WhitespaceMode,
) -> i32 {
    loop {
        if count1 == 0 && count2 == 0 {
            return 0;
        }
        if count1 == 0 {
            for flag in changed2.iter_mut().skip(line2).take(count2) {
                *flag = true;
            }
            return 0;
        }
        if count2 == 0 {
            for flag in changed1.iter_mut().skip(line1).take(count1) {
                *flag = true;
            }
            return 0;
        }

        let mut lcs = Region::default();
        let mut index = HistIndex::new(old_lines, count1, ws);
        index.new_lines = new_lines;
        let lcs_found = find_lcs(&mut index, &mut lcs, line1, count1, line2, count2);
        if lcs_found < 0 {
            return -1;
        }
        if lcs_found == 1 {
            fall_back_classic(
                old_lines, new_lines, changed1, changed2, line1, count1, line2, count2, ws,
            );
            return 0;
        }
        if lcs.begin1 == line1 && lcs.begin2 == line2 {
            for flag in changed1.iter_mut().skip(line1).take(count1) {
                *flag = true;
            }
            for flag in changed2.iter_mut().skip(line2).take(count2) {
                *flag = true;
            }
            return 0;
        }
        let before_result = histogram_diff(
            old_lines,
            new_lines,
            changed1,
            changed2,
            line1,
            lcs.begin1 - line1,
            line2,
            lcs.begin2 - line2,
            ws,
        );
        if before_result != 0 {
            return before_result;
        }
        let end1 = line1 + count1 - 1;
        let end2 = line2 + count2 - 1;
        count1 = end1 - lcs.end1;
        line1 = lcs.end1 + 1;
        count2 = end2 - lcs.end2;
        line2 = lcs.end2 + 1;
    }
}

/// Per-line changed flags using Git's histogram algorithm with whitespace-aware line matching.
pub(crate) fn histogram_changed_flags(
    old_lines: &[&str],
    new_lines: &[&str],
    ws: WhitespaceMode,
) -> (Vec<bool>, Vec<bool>) {
    let mut changed1 = vec![false; old_lines.len()];
    let mut changed2 = vec![false; new_lines.len()];
    if old_lines.len() > u32::MAX as usize || new_lines.len() > u32::MAX as usize {
        fall_back_classic(
            old_lines,
            new_lines,
            &mut changed1,
            &mut changed2,
            0,
            old_lines.len(),
            0,
            new_lines.len(),
            ws,
        );
        return (changed1, changed2);
    }
    if histogram_diff(
        old_lines,
        new_lines,
        &mut changed1,
        &mut changed2,
        0,
        old_lines.len(),
        0,
        new_lines.len(),
        ws,
    ) < 0
    {
        changed1.fill(false);
        changed2.fill(false);
        fall_back_classic(
            old_lines,
            new_lines,
            &mut changed1,
            &mut changed2,
            0,
            old_lines.len(),
            0,
            new_lines.len(),
            ws,
        );
    }
    (changed1, changed2)
}

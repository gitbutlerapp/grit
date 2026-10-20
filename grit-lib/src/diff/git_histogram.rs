// Git histogram diff (JGit/xdiff `xhistogram.c`), using interned line ids for equality.
// Line numbers follow Git's 1-based convention within each side.

use std::cmp::min;

const MAX_CHAIN_LENGTH: usize = 64;

struct Record {
    ptr: usize,
    cnt: u32,
    next: Option<usize>,
}

struct HistIndex<'a> {
    ids1: &'a [u32],
    ids2: &'a [u32],
    ptr_shift: usize,
    table_bits: u32,
    records: Vec<Option<usize>>,
    record_pool: Vec<Record>,
    line_map: Vec<Option<usize>>,
    next_ptrs: Vec<usize>,
    max_chain_length: usize,
    cnt: u32,
    has_common: bool,
}

impl HistIndex<'_> {
    fn id1(&self, line1: usize) -> u32 {
        self.ids1[line1 - 1]
    }

    fn id2(&self, line2: usize) -> u32 {
        self.ids2[line2 - 1]
    }

    fn cmp(&self, side1_line: usize, side2_line: usize) -> bool {
        self.id1(side1_line) == self.id2(side2_line)
    }

    fn cmp_same_side(&self, a: usize, b: usize) -> bool {
        self.id1(a) == self.id1(b)
    }

    fn table_hash(&self, id: u32) -> usize {
        let v = id as u64;
        let b = self.table_bits;
        let mixed = v + (v >> b);
        (mixed & ((1u64 << b) - 1)) as usize
    }

    fn line_map_get(&self, line1: usize) -> Option<usize> {
        self.line_map
            .get(line1 - self.ptr_shift)
            .copied()
            .flatten()
    }

    fn set_line_map(&mut self, line1: usize, rec_idx: usize) {
        let slot = line1 - self.ptr_shift;
        if slot < self.line_map.len() {
            self.line_map[slot] = Some(rec_idx);
        }
    }

    fn next_ptr(&self, line1: usize) -> usize {
        self.next_ptrs[line1 - self.ptr_shift]
    }

    fn set_next_ptr(&mut self, line1: usize, val: usize) {
        self.next_ptrs[line1 - self.ptr_shift] = val;
    }

    fn record_cnt(&self, rec_idx: usize) -> u32 {
        self.record_pool[rec_idx].cnt
    }

    fn alloc_record(&mut self, ptr: usize) -> usize {
        let idx = self.record_pool.len();
        self.record_pool.push(Record {
            ptr,
            cnt: 1,
            next: None,
        });
        idx
    }
}

fn hashbits(size: usize) -> u32 {
    let mut val = 1usize;
    let mut bits = 0u32;
    while val < size && bits < u32::BITS {
        val <<= 1;
        bits += 1;
    }
    if bits == 0 { 1 } else { bits }
}

fn line_end(line1: usize, count1: usize) -> usize {
    line1 + count1 - 1
}

fn scan_a(index: &mut HistIndex<'_>, line1: usize, count1: usize) -> bool {
    let mut ptr = line_end(line1, count1);
    loop {
        let tbl_idx = index.table_hash(index.id1(ptr));
        let mut chain_len = 0usize;
        let mut head = index.records[tbl_idx];
        let mut matched = false;

        while let Some(rec_idx) = head {
            if index.cmp_same_side(index.record_pool[rec_idx].ptr, ptr) {
                let old_ptr = index.record_pool[rec_idx].ptr;
                index.set_next_ptr(ptr, old_ptr);
                index.record_pool[rec_idx].ptr = ptr;
                index.record_pool[rec_idx].cnt = index.record_pool[rec_idx].cnt.saturating_add(1);
                index.set_line_map(ptr, rec_idx);
                matched = true;
                break;
            }
            head = index.record_pool[rec_idx].next;
            chain_len += 1;
        }

        if matched {
            if ptr == line1 {
                break;
            }
            ptr -= 1;
            continue;
        }

        if chain_len == index.max_chain_length {
            return false;
        }

        let rec_idx = index.alloc_record(ptr);
        index.record_pool[rec_idx].next = index.records[tbl_idx];
        index.records[tbl_idx] = Some(rec_idx);
        index.set_line_map(ptr, rec_idx);

        if ptr == line1 {
            break;
        }
        ptr -= 1;
    }
    true
}

struct Region {
    begin1: usize,
    end1: usize,
    begin2: usize,
    end2: usize,
}

fn try_lcs(
    index: &mut HistIndex<'_>,
    lcs: &mut Region,
    b_ptr: usize,
    line1: usize,
    count1: usize,
    line2: usize,
    count2: usize,
) -> usize {
    let mut b_next = b_ptr + 1;
    let end1 = line_end(line1, count1);
    let end2 = line_end(line2, count2);
    let tbl_idx = index.table_hash(index.id2(b_ptr));
    let mut head = index.records[tbl_idx];

    while let Some(rec_idx) = head {
        let rec = &index.record_pool[rec_idx];
        if rec.cnt > index.cnt {
            if !index.has_common && index.cmp(rec.ptr, b_ptr) {
                index.has_common = true;
            }
            head = rec.next;
            continue;
        }

        let mut as_idx = rec.ptr;
        if !index.cmp(as_idx, b_ptr) {
            head = rec.next;
            continue;
        }

        index.has_common = true;
        loop {
            let mut np = index.next_ptr(as_idx);
            let mut bs = b_ptr;
            let mut ae = as_idx;
            let mut be = bs;
            let mut rc = rec.cnt;

            while line1 < as_idx && line2 < bs && index.cmp(as_idx - 1, bs - 1) {
                as_idx -= 1;
                bs -= 1;
                if rc > 1 {
                    rc = min(rc, index.record_cnt(index.line_map_get(as_idx).unwrap_or(0)));
                }
            }
            while ae < end1 && be < end2 && index.cmp(ae + 1, be + 1) {
                ae += 1;
                be += 1;
                if rc > 1 {
                    rc = min(rc, index.record_cnt(index.line_map_get(ae).unwrap_or(0)));
                }
            }

            if b_next <= be {
                b_next = be + 1;
            }
            let lcs_len = lcs.end1.saturating_sub(lcs.begin1);
            let cand_len = ae.saturating_sub(as_idx);
            if lcs_len < cand_len || rc < index.cnt {
                lcs.begin1 = as_idx;
                lcs.begin2 = bs;
                lcs.end1 = ae;
                lcs.end2 = be;
                index.cnt = rc;
            }

            if np == 0 {
                break;
            }
            while np <= ae {
                np = index.next_ptr(np);
                if np == 0 {
                    break;
                }
            }
            if np == 0 {
                break;
            }
            as_idx = np;
        }
        head = rec.next;
    }
    b_next
}

fn find_lcs(
    ids1: &[u32],
    ids2: &[u32],
    line1: usize,
    count1: usize,
    line2: usize,
    count2: usize,
) -> Result<(Region, bool), ()> {
    let table_bits = hashbits(count1);
    let records_size = 1usize << table_bits;

    let mut index = HistIndex {
        ids1,
        ids2,
        ptr_shift: line1,
        table_bits,
        records: vec![None; records_size],
        record_pool: Vec::new(),
        line_map: vec![None; count1],
        next_ptrs: vec![0; count1],
        max_chain_length: MAX_CHAIN_LENGTH,
        cnt: (MAX_CHAIN_LENGTH + 1) as u32,
        has_common: false,
    };

    if !scan_a(&mut index, line1, count1) {
        return Err(());
    }

    let mut lcs = Region {
        begin1: 0,
        end1: 0,
        begin2: 0,
        end2: 0,
    };
    let end2 = line_end(line2, count2);
    let mut b_ptr = line2;
    while b_ptr <= end2 {
        b_ptr = try_lcs(
            &mut index,
            &mut lcs,
            b_ptr,
            line1,
            count1,
            line2,
            count2,
        );
    }

    let chain_too_long = index.has_common && index.max_chain_length < index.cnt as usize;
    Ok((lcs, chain_too_long))
}

fn mark_1based(changed: &mut [bool], line1: usize, count: usize) {
    for i in 0..count {
        let idx = line1 - 1 + i;
        if idx < changed.len() {
            changed[idx] = true;
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn histogram_diff_region(
    lines1: &[&str],
    lines2: &[&str],
    ids1: &[u32],
    ids2: &[u32],
    changed1: &mut [bool],
    changed2: &mut [bool],
    line1: usize,
    count1: usize,
    line2: usize,
    count2: usize,
) {
    if count1 == 0 && count2 == 0 {
        return;
    }
    if count1 == 0 {
        mark_1based(changed2, line2, count2);
        return;
    }
    if count2 == 0 {
        mark_1based(changed1, line1, count1);
        return;
    }

    let (lcs, chain_too_long) = match find_lcs(ids1, ids2, line1, count1, line2, count2) {
        Ok(v) => v,
        Err(()) => {
            merge_myers_region(
                lines1,
                lines2,
                changed1,
                changed2,
                line1 - 1,
                line1 - 1 + count1,
                line2 - 1,
                line2 - 1 + count2,
            );
            return;
        }
    };

    if chain_too_long {
        merge_myers_region(
            lines1,
            lines2,
            changed1,
            changed2,
            line1 - 1,
            line1 - 1 + count1,
            line2 - 1,
            line2 - 1 + count2,
        );
        return;
    }

    if lcs.begin1 == 0 && lcs.begin2 == 0 {
        mark_1based(changed1, line1, count1);
        mark_1based(changed2, line2, count2);
        return;
    }

    let before_c1 = lcs.begin1.saturating_sub(line1);
    let before_c2 = lcs.begin2.saturating_sub(line2);
    if before_c1 > 0 || before_c2 > 0 {
        histogram_diff_region(
            lines1,
            lines2,
            ids1,
            ids2,
            changed1,
            changed2,
            line1,
            before_c1,
            line2,
            before_c2,
        );
    }

    let end1 = line_end(line1, count1);
    let end2 = line_end(line2, count2);
    let tail_line1 = lcs.end1 + 1;
    let tail_line2 = lcs.end2 + 1;
    let tail_c1 = end1.saturating_sub(lcs.end1);
    let tail_c2 = end2.saturating_sub(lcs.end2);
    if tail_c1 > 0 || tail_c2 > 0 {
        histogram_diff_region(
            lines1,
            lines2,
            ids1,
            ids2,
            changed1,
            changed2,
            tail_line1,
            tail_c1,
            tail_line2,
            tail_c2,
        );
    }
}

pub fn histogram_changed_flags(
    lines1: &[&str],
    lines2: &[&str],
    ids1: &[u32],
    ids2: &[u32],
) -> (Vec<bool>, Vec<bool>) {
    let n1 = ids1.len();
    let n2 = ids2.len();
    let mut changed1 = vec![false; n1];
    let mut changed2 = vec![false; n2];
    if n1 == 0 && n2 == 0 {
        return (changed1, changed2);
    }

    let lim = n1.min(n2);
    let mut i = 0usize;
    while i < lim && ids1[i] == ids2[i] {
        i += 1;
    }
    let mut j = 0usize;
    let rem = lim.saturating_sub(i);
    while j < rem && ids1[n1 - 1 - j] == ids2[n2 - 1 - j] {
        j += 1;
    }
    let dstart1 = i;
    let dend1 = n1.saturating_sub(j);
    let dstart2 = i;
    let dend2 = n2.saturating_sub(j);
    if dstart1 >= dend1 && dstart2 >= dend2 {
        return (changed1, changed2);
    }

    let line1 = dstart1 + 1;
    let count1 = dend1 - dstart1;
    let line2 = dstart2 + 1;
    let count2 = dend2 - dstart2;

    histogram_diff_region(
        lines1,
        lines2,
        ids1,
        ids2,
        &mut changed1,
        &mut changed2,
        line1,
        count1,
        line2,
        count2,
    );
    (changed1, changed2)
}

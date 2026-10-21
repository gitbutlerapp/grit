//! Merge adjacent line-level edits into unified-diff hunk spans (Git-compatible gaps).

#![allow(dead_code)]

/// One contiguous removed/added run on the old and new sides.
#[derive(Debug, Clone, Copy)]
pub(crate) struct LineEdit {
    pub old_at: i64,
    pub old_run: i64,
    pub new_at: i64,
    pub new_run: i64,
    pub ignorable: bool,
}

#[derive(Clone, Copy)]
struct FuseLimits {
    fuse: i64,
    nearby: i64,
}

#[derive(Clone, Copy)]
struct OpenHunk {
    tail: usize,
    last_substantive: usize,
    /// Old-side slack from ignorable-only inserts absorbed after `last_substantive`.
    slack_new_lines: i64,
}

impl OpenHunk {
    fn new(head: usize) -> Self {
        Self {
            tail: head,
            last_substantive: head,
            slack_new_lines: 0,
        }
    }

    fn old_gap_to(&self, edits: &[LineEdit], idx: usize) -> i64 {
        edits[idx].old_at - (edits[self.tail].old_at + edits[self.tail].old_run)
    }

    fn absorb(&mut self, idx: usize, edit: LineEdit) {
        self.tail = idx;
        if edit.ignorable {
            self.slack_new_lines += edit.new_run;
        } else {
            self.last_substantive = idx;
            self.slack_new_lines = 0;
        }
    }
}

fn may_join(hunk: &OpenHunk, edits: &[LineEdit], next: usize, limits: FuseLimits) -> bool {
    let gap = hunk.old_gap_to(edits, next);
    if gap > limits.fuse {
        return false;
    }
    let edit = edits[next];
    if gap < limits.nearby {
        return true;
    }

    if hunk.last_substantive != hunk.tail {
        let anchor = edits[hunk.last_substantive];
        let virtual_old_end = edit.old_at + hunk.slack_new_lines;
        let span = virtual_old_end - (anchor.old_at + anchor.old_run);
        if span > limits.fuse {
            return false;
        }
    }
    true
}

fn skip_orphan_ignorable_head(edits: &[LineEdit], mut idx: usize, nearby: i64) -> Option<usize> {
    while idx < edits.len() && edits[idx].ignorable {
        let far = idx + 1 >= edits.len()
            || edits[idx + 1].old_at - (edits[idx].old_at + edits[idx].old_run) >= nearby;
        if far {
            idx += 1;
        } else {
            return Some(idx);
        }
    }
    (idx < edits.len()).then_some(idx)
}

fn grow_hunk(edits: &[LineEdit], head: usize, limits: FuseLimits) -> usize {
    let mut open = OpenHunk::new(head);
    let mut cursor = head;
    while cursor + 1 < edits.len() {
        let next = cursor + 1;
        if !may_join(&open, edits, next, limits) {
            break;
        }
        open.absorb(next, edits[next]);
        cursor = next;
    }
    open.tail
}

/// `(first_edit_index, last_edit_index)` for each hunk, in file order.
pub(crate) fn hunk_edit_spans(
    edits: &[LineEdit],
    fuse_limit: i64,
    nearby_limit: i64,
) -> Vec<(usize, usize)> {
    let limits = FuseLimits {
        fuse: fuse_limit,
        nearby: nearby_limit,
    };
    let mut spans = Vec::new();
    let mut cursor = 0usize;
    while cursor < edits.len() {
        let Some(head) = skip_orphan_ignorable_head(edits, cursor, nearby_limit) else {
            break;
        };
        let tail = grow_hunk(edits, head, limits);
        spans.push((head, tail));
        cursor = tail + 1;
    }
    spans
}

#[cfg(test)]
mod tests {
    use super::*;

    fn edit(old_at: i64, old_run: i64, new_run: i64, ignorable: bool) -> LineEdit {
        LineEdit {
            old_at,
            old_run,
            new_at: old_at,
            new_run,
            ignorable,
        }
    }

    #[test]
    fn orphan_ignorable_prefix_is_skipped() {
        let edits = vec![edit(0, 0, 1, true), edit(10, 1, 1, false)];
        let spans = hunk_edit_spans(&edits, 6, 3);
        assert_eq!(spans, vec![(1, 1)]);
    }
}

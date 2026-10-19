//! Merge adjacent line-level edits into unified-diff hunk spans (Git-compatible gaps).

/// One contiguous removed/added run on the old and new sides.
#[derive(Debug, Clone, Copy)]
pub(crate) struct LineEdit {
    pub old_at: i64,
    pub old_run: i64,
    pub new_at: i64,
    pub new_run: i64,
    pub ignorable: bool,
}

/// Unchanged old-side lines between the end of `left` and the start of `right`.
#[inline]
fn unchanged_old_gap(left: &LineEdit, right: &LineEdit) -> i64 {
    right.old_at - (left.old_at + left.old_run)
}

/// Drop leading ignorable edits that sit too far from the next substantive edit.
fn skip_detached_prefix(edits: &[LineEdit], start: usize, nearby_limit: i64) -> Option<usize> {
    let mut head = start;
    while head < edits.len() && edits[head].ignorable {
        let successor = head + 1;
        let far_from_next = successor >= edits.len()
            || unchanged_old_gap(&edits[head], &edits[successor]) >= nearby_limit;
        if far_from_next {
            head = successor;
        } else {
            break;
        }
    }
    (head < edits.len()).then_some(head)
}

/// Index of the last edit included when fusing from `head`.
fn fused_tail_index(edits: &[LineEdit], head: usize, fuse_limit: i64, nearby_limit: i64) -> usize {
    let mut tail = head;
    let mut last_kept = head;
    let mut pending_ignored_new: i64 = 0;

    for idx in head + 1..edits.len() {
        let gap = unchanged_old_gap(&edits[tail], &edits[idx]);
        if gap > fuse_limit {
            break;
        }

        let edit = &edits[idx];
        let nearby = gap < nearby_limit;

        if nearby && (!edit.ignorable || last_kept == tail) {
            last_kept = idx;
            pending_ignored_new = 0;
        } else if nearby && edit.ignorable {
            pending_ignored_new += edit.new_run;
        } else if last_kept != tail {
            let anchor = &edits[last_kept];
            let virtual_old_end = edit.old_at + pending_ignored_new;
            let span = virtual_old_end - (anchor.old_at + anchor.old_run);
            if span > fuse_limit {
                break;
            }
            if !edit.ignorable {
                last_kept = idx;
                pending_ignored_new = 0;
            } else {
                pending_ignored_new += edit.new_run;
            }
        } else if !edit.ignorable {
            last_kept = idx;
            pending_ignored_new = 0;
        } else {
            pending_ignored_new += edit.new_run;
        }

        tail = idx;
    }

    last_kept
}

/// `(first_edit_index, last_edit_index)` for each hunk, in file order.
pub(crate) fn hunk_edit_spans(
    edits: &[LineEdit],
    fuse_limit: i64,
    nearby_limit: i64,
) -> Vec<(usize, usize)> {
    let mut spans = Vec::new();
    let mut at = 0usize;
    while at < edits.len() {
        let Some(head) = skip_detached_prefix(edits, at, nearby_limit) else {
            break;
        };
        let tail = fused_tail_index(edits, head, fuse_limit, nearby_limit);
        spans.push((head, tail));
        at = tail + 1;
    }
    spans
}

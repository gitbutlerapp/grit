//! Index-write and rewrite-apply failure injection (tests and `test-support` only).
#![allow(missing_docs)]

use crate::error::{Error, Result};

mod index_write_fail_inject {
    use std::cell::RefCell;

    thread_local! { // hygiene: test-support-only index write failure injection
        pub(super) static INJECT: RefCell<bool> = const { RefCell::new(false) };
        pub(super) static INJECT_REMAINING: RefCell<Option<usize>> = const { RefCell::new(None) };
    }
}

mod rewrite_apply_inject {
    use std::cell::RefCell;

    thread_local! { // hygiene: test-support-only rewrite abort injection after refs
        pub(super) static ABORT_AFTER_REFS: RefCell<bool> = const { RefCell::new(false) };
    }
}

/// Fail every index write path while enabled.
pub fn set_index_write_fail(enabled: bool) {
    index_write_fail_inject::INJECT.with(|c| *c.borrow_mut() = enabled);
    if !enabled {
        index_write_fail_inject::INJECT_REMAINING.with(|c| *c.borrow_mut() = None);
    }
}

/// Fail the next `remaining` index write attempts, then allow writes.
pub fn set_index_write_fail_remaining(remaining: usize) {
    index_write_fail_inject::INJECT.with(|c| *c.borrow_mut() = true);
    index_write_fail_inject::INJECT_REMAINING.with(|c| *c.borrow_mut() = Some(remaining));
}

/// Stop [`crate::history::apply_rewrite`] after ref updates without rolling back.
pub fn set_rewrite_apply_abort_after_refs(enabled: bool) {
    rewrite_apply_inject::ABORT_AFTER_REFS.with(|c| *c.borrow_mut() = enabled);
}

pub(crate) fn rewrite_apply_abort_after_refs() -> bool {
    rewrite_apply_inject::ABORT_AFTER_REFS.with(|c| *c.borrow())
}

pub(crate) fn index_write_fail() -> Result<()> {
    index_write_fail_inject::INJECT.with(|enabled| {
        if !*enabled.borrow() {
            return Ok(());
        }
        index_write_fail_inject::INJECT_REMAINING.with(|remaining| {
            if let Some(left) = remaining.borrow_mut().as_mut() {
                if *left == 0 {
                    return Ok(());
                }
                *left -= 1;
            }
            Err(Error::Io(std::io::Error::other(
                "injected index write failure",
            )))
        })
    })
}

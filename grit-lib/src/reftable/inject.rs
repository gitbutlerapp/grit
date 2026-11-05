//! Reftable transaction failure injection for tests and `test-support`.
#![allow(missing_docs)]

use std::cell::RefCell;

use crate::error::{Error, Result};

thread_local! { // hygiene: test-support-only reftable transaction failure injection
    static INJECT: RefCell<bool> = const { RefCell::new(false) };
}

/// When enabled, the next [`super::ReftableStack::write_transaction`] fails before writing.
pub fn set_reftable_transaction_fail(enabled: bool) {
    INJECT.with(|c| *c.borrow_mut() = enabled);
}

pub(crate) fn reftable_transaction_fail() -> Result<()> {
    INJECT.with(|c| {
        if *c.borrow() {
            return Err(Error::Io(std::io::Error::other(
                "injected reftable transaction failure",
            )));
        }
        Ok(())
    })
}

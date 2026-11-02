//! Thread-local stack [`Cancel`] for smart-HTTP waits on the owning thread.
//!
//! HTTP POST/GET workers poll a shared [`AtomicBool`](std::sync::atomic::AtomicBool)
//! while the socket blocks. When the caller's [`Cancel`] lives on the stack (no
//! [`Cancel::shared_watch`]), the owning thread must mirror it into that flag
//! during waits. An [`HttpOwnerCancelGuard`] registers the active cancel for
//! the current thread only for the guard's lifetime; it is not stored on the
//! connection.

use std::cell::Cell;

use crate::progress::Cancel;

/// Two-word fat pointer to a stack [`Cancel`], valid only while the matching
/// [`HttpOwnerCancelGuard`] is live on the installing thread.
#[derive(Clone, Copy)]
struct StackCancelSlot {
    data: *const (),
    vtable: *const (),
}

impl StackCancelSlot {
    fn capture(cancel: &dyn Cancel) -> Self {
        let ptr: *const dyn Cancel = cancel;
        let (data, vtable) =
            unsafe { std::mem::transmute::<*const dyn Cancel, (*const (), *const ())>(ptr) };
        Self { data, vtable }
    }

    /// # Safety
    ///
    /// The slot must have been captured on this thread and the matching
    /// [`HttpOwnerCancelGuard`] must still be installed.
    unsafe fn as_cancel(&self) -> &dyn Cancel {
        let ptr: *const dyn Cancel = unsafe { std::mem::transmute((self.data, self.vtable)) };
        unsafe { &*ptr }
    }
}

thread_local! {
    static OWNER_HTTP_CANCEL: Cell<Option<StackCancelSlot>> = const { Cell::new(None) };
}

/// Pins a stack [`Cancel`] for smart-HTTP on this thread until the guard drops.
pub(crate) struct HttpOwnerCancelGuard {
    previous: Option<StackCancelSlot>,
}

impl HttpOwnerCancelGuard {
    /// Register `cancel` for [`with_current`] on this thread.
    #[must_use]
    pub(crate) fn install(cancel: &dyn Cancel) -> Self {
        let slot = StackCancelSlot::capture(cancel);
        let previous = OWNER_HTTP_CANCEL.with(|cell| cell.replace(Some(slot)));
        Self { previous }
    }

    /// Invoke `f` with the installed owner cancel, if any.
    pub(crate) fn with_current<R>(f: impl FnOnce(Option<&dyn Cancel>) -> R) -> R {
        OWNER_HTTP_CANCEL.with(|cell| {
            let Some(slot) = cell.get() else {
                return f(None);
            };
            // SAFETY: `slot` is only set by [`Self::install`] on this thread and
            // cleared when that guard drops.
            f(Some(unsafe { slot.as_cancel() }))
        })
    }
}

impl Drop for HttpOwnerCancelGuard {
    fn drop(&mut self) {
        OWNER_HTTP_CANCEL.with(|cell| {
            cell.set(self.previous);
        });
    }
}

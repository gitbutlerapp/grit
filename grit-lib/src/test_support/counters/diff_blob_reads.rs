//! Bounded blob ODB read counting during tree content diff loads.

use std::cell::RefCell;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

thread_local! { // hygiene: test-support diff blob-read counter (thread-local)
    static BLOB_READ_COUNTER: RefCell<Option<Arc<AtomicUsize>>> = const { RefCell::new(None) };
}

/// Tracks blob reads on this thread until dropped.
/// Restores the previous thread-local blob read counter on drop.
pub struct BlobReadCounterScope {
    previous: Option<Arc<AtomicUsize>>,
}

impl Drop for BlobReadCounterScope {
    fn drop(&mut self) {
        BLOB_READ_COUNTER.with(|slot| {
            *slot.borrow_mut() = self.previous.take();
        });
    }
}

/// Count each blob ODB read during tree content loading on the current thread.
pub fn track_blob_reads(counter: Arc<AtomicUsize>) -> BlobReadCounterScope {
    let previous = BLOB_READ_COUNTER.with(|slot| slot.borrow_mut().replace(counter));
    BlobReadCounterScope { previous }
}

pub(crate) fn note_blob_read() {
    BLOB_READ_COUNTER.with(|slot| {
        if let Some(counter) = slot.borrow().as_ref() {
            counter.fetch_add(1, Ordering::Relaxed);
        }
    });
}

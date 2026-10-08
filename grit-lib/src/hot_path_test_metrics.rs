//! Per-[`crate::odb::Odb`] hot-path counters for regression tests (test builds only).

use std::cell::RefCell;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;

/// Counters and opt-in recording flags for one object database / repository.
#[derive(Default)]
pub struct HotPathTestMetrics {
    tree_writes: AtomicUsize,
    freshen_calls: AtomicUsize,
    blob_content_reads: AtomicUsize,
    pack_signature_stats: AtomicUsize,
    midx_stamp_stats: AtomicUsize,
    loose_path_open_attempts: AtomicUsize,
    freshen_counting: AtomicBool,
    blob_counting: AtomicBool,
    stamp_counting: AtomicBool,
    loose_open_counting: AtomicBool,
}

impl HotPathTestMetrics {
    /// Allocate a fresh counter bundle for one [`crate::odb::Odb`].
    #[must_use]
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    /// Clear tree-write count.
    pub fn reset_tree_writes(&self) {
        self.tree_writes.store(0, Ordering::SeqCst);
    }

    /// Tree objects written through this ODB since the last reset.
    #[must_use]
    pub fn tree_writes(&self) -> usize {
        self.tree_writes.load(Ordering::SeqCst)
    }

    pub(crate) fn record_tree_write(&self) {
        self.tree_writes.fetch_add(1, Ordering::Relaxed);
    }

    /// Clear freshen-touch count.
    pub fn reset_freshen_calls(&self) {
        self.freshen_calls.store(0, Ordering::SeqCst);
    }

    pub fn set_freshen_counting(&self, enabled: bool) {
        self.freshen_counting.store(enabled, Ordering::SeqCst);
    }

    #[must_use]
    pub fn freshen_calls(&self) -> usize {
        self.freshen_calls.load(Ordering::SeqCst)
    }

    pub(crate) fn record_freshen(&self) {
        if self.freshen_counting.load(Ordering::Relaxed) {
            self.freshen_calls.fetch_add(1, Ordering::Relaxed);
        }
    }

    /// Clear worktree blob read count.
    pub fn reset_blob_content_reads(&self) {
        self.blob_content_reads.store(0, Ordering::SeqCst);
    }

    pub fn set_blob_counting(&self, enabled: bool) {
        self.blob_counting.store(enabled, Ordering::Relaxed);
    }

    #[must_use]
    pub fn blob_content_reads(&self) -> usize {
        self.blob_content_reads.load(Ordering::SeqCst)
    }

    pub(crate) fn record_blob_content_read(&self) {
        if self.blob_counting.load(Ordering::Relaxed) {
            self.blob_content_reads.fetch_add(1, Ordering::Relaxed);
        }
    }

    pub fn reset_pack_stamp_stats(&self) {
        self.pack_signature_stats.store(0, Ordering::SeqCst);
    }

    pub fn reset_midx_stamp_stats(&self) {
        self.midx_stamp_stats.store(0, Ordering::SeqCst);
    }

    /// Clear loose-object `open` attempt count.
    pub fn reset_loose_path_open_attempts(&self) {
        self.loose_path_open_attempts.store(0, Ordering::SeqCst);
    }

    pub fn set_loose_open_counting(&self, enabled: bool) {
        self.loose_open_counting.store(enabled, Ordering::Relaxed);
    }

    #[must_use]
    pub fn loose_path_open_attempts(&self) -> usize {
        self.loose_path_open_attempts.load(Ordering::SeqCst)
    }

    pub(crate) fn record_loose_path_open(&self) {
        if self.loose_open_counting.load(Ordering::Relaxed) {
            self.loose_path_open_attempts
                .fetch_add(1, Ordering::Relaxed);
        }
    }

    pub fn set_stamp_counting(&self, enabled: bool) {
        self.stamp_counting.store(enabled, Ordering::SeqCst);
    }

    #[must_use]
    pub fn pack_signature_stat_calls(&self) -> usize {
        self.pack_signature_stats.load(Ordering::SeqCst)
    }

    #[must_use]
    pub fn midx_stamp_stat_calls(&self) -> usize {
        self.midx_stamp_stats.load(Ordering::SeqCst)
    }

    pub(crate) fn record_pack_signature_stat(&self) {
        if self.stamp_counting.load(Ordering::Relaxed) {
            self.pack_signature_stats.fetch_add(1, Ordering::Relaxed);
        }
    }

    #[allow(dead_code)]
    pub(crate) fn record_midx_stamp_stat(&self) {
        if self.stamp_counting.load(Ordering::Relaxed) {
            self.midx_stamp_stats.fetch_add(1, Ordering::Relaxed);
        }
    }

    /// Disable all opt-in recording (safe to call from [`HotPathMetricsScope`] drop).
    pub fn disable_all_counting(&self) {
        self.set_freshen_counting(false);
        self.set_blob_counting(false);
        self.set_stamp_counting(false);
        self.set_loose_open_counting(false);
    }
}

// hygiene: per-thread hot-path test metrics scope (test builds only)
thread_local! {
    static ACTIVE: RefCell<Option<Arc<HotPathTestMetrics>>> = const { RefCell::new(None) };
}

fn with_active<F, R>(f: F) -> R
where
    F: FnOnce(Option<Arc<HotPathTestMetrics>>) -> R,
{
    ACTIVE.with(|cell| f(cell.borrow().clone()))
}

pub(crate) fn record_pack_signature_stat_for_active_scope() {
    with_active(|active| {
        if let Some(m) = active {
            m.record_pack_signature_stat();
        }
    });
}

#[allow(dead_code)]
pub(crate) fn record_midx_stamp_stat_for_active_scope() {
    with_active(|active| {
        if let Some(m) = active {
            m.record_midx_stamp_stat();
        }
    });
}

pub(crate) fn record_loose_path_open_for_active_scope() {
    with_active(|active| {
        if let Some(m) = active {
            m.record_loose_path_open();
        }
    });
}

/// Installs `metrics` as the active scope for pack/MIDX stat attribution on this thread.
pub struct HotPathMetricsScope {
    prev: Option<Arc<HotPathTestMetrics>>,
    installed: Option<Arc<HotPathTestMetrics>>,
}

impl HotPathMetricsScope {
    /// Begin attributing pack/MIDX stat probes to `metrics` on this thread.
    #[must_use]
    pub fn install(metrics: Arc<HotPathTestMetrics>) -> Self {
        let prev = ACTIVE.with(|cell| cell.borrow_mut().replace(Arc::clone(&metrics)));
        Self {
            prev,
            installed: Some(metrics),
        }
    }
}

impl Drop for HotPathMetricsScope {
    fn drop(&mut self) {
        if let Some(m) = self.installed.take() {
            m.disable_all_counting();
        }
        ACTIVE.with(|cell| {
            *cell.borrow_mut() = self.prev.take();
        });
    }
}

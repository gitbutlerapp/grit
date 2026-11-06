//! Progress reporting and cancellation across the library/CLI boundary.
//!
//! Library operations never touch the terminal. Long-running work reports
//! progress through a [`ProgressSink`] supplied by the caller and checks a
//! [`CancelToken`] at loop boundaries. The CLI provides a concrete sink that
//! draws to stderr (gated on `isatty`) and decides colour, redraw rate, and
//! whether a pager is attached; the library makes none of those decisions.
//!
//! Sinks are invoked from the **calling thread only**, at **coarse** granularity
//! (phase boundaries and periodic counts, not per-object spam). Totals passed
//! to [`ProgressSink::start`] may be [`None`] when the operation cannot know
//! the final count up front.
//!
//! Tests, `grit-cli`, and any call site that does not want output use the
//! no-op [`NullProgress`] and a default [`CancelToken::never`] handle.
//!
//! Library guide: [progress](https://grit-scm.com/docs/library/parallelism)
//!
//! ```rust
//! use grit_lib::progress::{CancelToken, NullProgress};
//! # fn main() {
//! let _sink = NullProgress;
//! let _cancel = CancelToken::never();
//! # }
//! ```

mod estimate;

pub use estimate::{OverallEstimate, OverallEstimator, PhaseRates};

/// A coarse phase of a long-running Git operation.
///
/// New variants may be added over time; match exhaustively only in the CLI or
/// with a wildcard arm when forwarding unknown phases.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ProgressPhase {
    /// Receiving a pack or loose objects from a remote or bundle stream.
    ReceivingObjects,
    /// Walking reachability to decide which objects a local fetch must copy.
    EnumeratingObjects,
    /// Building or reading a pack index after objects are on disk.
    IndexingObjects,
    /// Applying delta chains inside a pack.
    ResolvingDeltas,
    /// Copying objects between local repositories (e.g. local fetch).
    CopyingObjects,
    /// Updating the work tree to match a tree (checkout, reset --hard, clone).
    CheckingOutFiles,
    /// Hashing work-tree files into the index (add, auto-staging commit, stash push).
    StagingFiles,
    /// Downloading or uploading a bundle that can resume where it stopped
    /// (clone's `bundle-uri` bundle, a large push). Counted in bytes, including
    /// any bytes an earlier run already moved; the detailed events go to
    /// [`crate::fetch::Progress::resumable_transfer`].
    TransferringBundle,
    /// Compressing objects while building a pack for upload.
    CompressingObjects,
    /// Writing object entries into a pack stream.
    WritingObjects,
    /// Streaming a pack to a remote during push (counted in bytes).
    SendingObjects,
}

impl ProgressPhase {
    /// Returns the unit callers should use when reporting counts for `self`.
    #[must_use]
    pub fn default_unit(self) -> ProgressUnit {
        match self {
            Self::ReceivingObjects
            | Self::EnumeratingObjects
            | Self::IndexingObjects
            | Self::ResolvingDeltas
            | Self::CopyingObjects
            | Self::CompressingObjects
            | Self::WritingObjects => ProgressUnit::Objects,
            Self::CheckingOutFiles | Self::StagingFiles => ProgressUnit::Files,
            Self::TransferringBundle | Self::SendingObjects => ProgressUnit::Bytes,
        }
    }
}

/// Unit for [`ProgressSink::inc`] / [`ProgressSink::set`] during a phase.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ProgressUnit {
    /// Git objects (commits, trees, blobs, tags).
    Objects,
    /// Raw bytes (e.g. pack stream length when known).
    Bytes,
    /// Work-tree paths.
    Files,
}

/// How far a pack transfer has got, counted in objects rather than bytes.
///
/// Reported through [`crate::fetch::Progress::received_objects`] while a pack
/// streams in, when the sink asks for it with
/// [`crate::fetch::Progress::wants_received_objects`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ReceivedObjects {
    /// Objects fully received so far.
    pub received: u64,
    /// Objects in the pack, from its header.
    pub total: u64,
    /// How many of the received objects are deltas.
    pub deltas: u64,
}

/// A sink for progress updates emitted by a library operation.
///
/// Every method has a no-op default, so a sink implements only what it needs.
/// The library calls these from the caller's thread at coarse intervals; it
/// never decides whether output is a tty, what colour to use, or how often to
/// redraw — that is the CLI's responsibility.
pub trait ProgressSink {
    /// Begin a new progress `phase`, optionally with a known total count of
    /// [`ProgressPhase::default_unit`] units for that phase.
    ///
    /// `total` may be [`None`] when the operation cannot estimate the final
    /// count (common for network receives).
    ///
    fn start(&mut self, phase: ProgressPhase, total: Option<u64>) {
        let _ = (phase, total);
    }

    /// Begin a phase with an explicit count unit (for example [`ProgressUnit::Bytes`]
    /// when streaming a pack whose size is known in bytes).
    ///
    /// Defaults to [`Self::start`]; override when the sink renders counts differently
    /// per [`ProgressUnit`].
    fn start_with_unit(&mut self, phase: ProgressPhase, unit: ProgressUnit, total: Option<u64>) {
        let _ = unit;
        self.start(phase, total);
    }

    /// Advance the current phase by `units` in the active phase unit.
    fn inc(&mut self, units: u64) {
        let _ = units;
    }

    /// Set the absolute progress of the current phase to `current` units.
    fn set(&mut self, current: u64) {
        let _ = current;
    }

    /// Finish the current phase.
    fn finish(&mut self) {}
}

/// Forwarding impl so a `&mut P` can be threaded into sub-operations that also
/// take `impl ProgressSink` without re-borrowing gymnastics at every call site.
impl<T: ProgressSink + ?Sized> ProgressSink for &mut T {
    fn start(&mut self, phase: ProgressPhase, total: Option<u64>) {
        (**self).start(phase, total);
    }
    fn start_with_unit(&mut self, phase: ProgressPhase, unit: ProgressUnit, total: Option<u64>) {
        (**self).start_with_unit(phase, unit, total);
    }
    fn inc(&mut self, units: u64) {
        (**self).inc(units);
    }
    fn set(&mut self, current: u64) {
        (**self).set(current);
    }
    fn finish(&mut self) {
        (**self).finish();
    }
}

/// A [`ProgressSink`] that discards every update. The default for tests,
/// `grit-cli`, and any call site that does not want progress output.
#[derive(Debug, Default, Clone, Copy)]
pub struct NullProgress;

impl ProgressSink for NullProgress {}

/// How often long object walks and pack builds report [`ProgressSink::set`].
pub(crate) const OBJECT_PROGRESS_STRIDE: u64 = 256;

/// How often worktree blob stat scans and similar loops poll [`poll_watch`].
pub(crate) const CANCEL_POLL_STRIDE: usize = 256;

/// Wraps a [`Write`] and reports bytes written to a [`ProgressSink`] as
/// [`ProgressPhase::SendingObjects`].
pub(crate) struct ProgressWrite<'a, W: ?Sized> {
    inner: &'a mut W,
    sink: &'a mut dyn ProgressSink,
    started: bool,
}

impl<'a, W: ?Sized + std::io::Write> ProgressWrite<'a, W> {
    /// Create a wrapper that forwards writes to `inner` and counts them on `sink`.
    pub fn new(inner: &'a mut W, sink: &'a mut dyn ProgressSink) -> Self {
        Self {
            inner,
            sink,
            started: false,
        }
    }

    /// End the sending phase when one was started.
    pub fn finish(&mut self) {
        if self.started {
            self.sink.finish();
            self.started = false;
        }
    }
}

impl<W: ?Sized + std::io::Write> std::io::Write for ProgressWrite<'_, W> {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        let n = self.inner.write(buf)?;
        if n > 0 {
            if !self.started {
                self.sink
                    .start_with_unit(ProgressPhase::SendingObjects, ProgressUnit::Bytes, None);
                self.started = true;
            }
            self.sink.inc(n as u64);
        }
        Ok(n)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.inner.flush()
    }
}

/// Report monotonic object progress at [`OBJECT_PROGRESS_STRIDE`] boundaries.
///
/// `done` is the absolute number of objects processed so far; `last_reported` is
/// updated to match each emitted `set`.
pub(crate) fn report_object_progress_stride(
    sink: &mut dyn ProgressSink,
    done: u64,
    last_reported: &mut u64,
    total: Option<u64>,
) {
    let stride = OBJECT_PROGRESS_STRIDE;
    while done.saturating_sub(*last_reported) >= stride {
        *last_reported += stride;
        sink.set(*last_reported);
    }
    if total == Some(done) && *last_reported < done {
        sink.set(done);
        *last_reported = done;
    }
}

/// Returned from [`CancelToken::poll`] once cancellation was requested.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Cancelled;

/// Historical type alias for [`CancelToken`] retained for older examples and tests.
pub type Token = CancelToken;

/// Cooperative cancellation handle shared across threads (`Clone` shares the same flag).
#[derive(Debug, Clone)]
pub struct CancelToken {
    flag: std::sync::Arc<std::sync::atomic::AtomicBool>,
    #[cfg(feature = "test-support")]
    polls_until_cancel: Option<std::sync::Arc<std::sync::atomic::AtomicU64>>,
    #[cfg(feature = "test-support")]
    check_counter: Option<std::sync::Arc<std::sync::atomic::AtomicUsize>>,
}

impl CancelToken {
    /// Create a handle that starts uncancelled.
    #[must_use]
    pub fn new() -> Self {
        Self {
            flag: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
            #[cfg(feature = "test-support")]
            polls_until_cancel: None,
            #[cfg(feature = "test-support")]
            check_counter: None,
        }
    }

    /// Alias for [`Self::new`]: a handle that never cancels unless [`Self::cancel`] is called.
    #[must_use]
    pub fn never() -> Self {
        Self::new()
    }

    /// Shared uncancelled handle for default `&Self` parameters (see [`Self::never`] for an owned clone).
    #[must_use]
    pub fn never_ref() -> &'static Self {
        static NEVER: std::sync::LazyLock<CancelToken> = std::sync::LazyLock::new(CancelToken::new);
        &NEVER
    }

    pub(crate) fn from_shared_flag(flag: std::sync::Arc<std::sync::atomic::AtomicBool>) -> Self {
        Self {
            flag,
            #[cfg(feature = "test-support")]
            polls_until_cancel: None,
            #[cfg(feature = "test-support")]
            check_counter: None,
        }
    }

    /// Returns a token and the number of cancellation checks (integration tests only).
    #[cfg(feature = "test-support")]
    #[must_use]
    pub fn with_check_counter() -> (Self, std::sync::Arc<std::sync::atomic::AtomicUsize>) {
        let counter = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let token = Self {
            flag: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
            polls_until_cancel: None,
            check_counter: Some(std::sync::Arc::clone(&counter)),
        };
        (token, counter)
    }

    /// Request stop at the next poll boundary on every clone of this handle.
    pub fn cancel(&self) {
        self.flag.store(true, std::sync::atomic::Ordering::Relaxed);
    }

    /// Clears a prior cancellation (for example when reinstalling signal handlers).
    pub fn reset(&self) {
        self.flag.store(false, std::sync::atomic::Ordering::Relaxed);
    }

    /// Returns `true` once [`Self::cancel`] was called or a test countdown reached zero.
    #[must_use]
    pub fn is_cancelled(&self) -> bool {
        #[cfg(feature = "test-support")]
        self.record_check_only();
        self.flag.load(std::sync::atomic::Ordering::Relaxed)
    }

    #[cfg(feature = "test-support")]
    fn record_check_only(&self) {
        if let Some(counter) = self.check_counter.as_ref() {
            counter.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        }
    }

    /// Like [`Self::poll`], but only applies the test [`Self::after_polls`] budget — avoids
    /// double-counting when [`Self::poll`] also reads cancellation.
    #[cfg(feature = "test-support")]
    fn tick_poll_budget_only(&self) {
        if let Some(remaining) = self.polls_until_cancel.as_ref() {
            let prev = remaining.fetch_sub(1, std::sync::atomic::Ordering::Relaxed);
            if prev == 0 {
                // Undo wrap when the budget was already exhausted.
                remaining.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                return;
            }
            if prev == 1 {
                self.cancel();
            }
        }
    }

    /// Poll hook before blocking waits (in tests, counts toward the poll budget from
    /// `CancelToken::after_polls`).
    pub fn poll_before_wait(&self) {
        #[cfg(feature = "test-support")]
        self.tick_poll_budget_only();
        let _ = self.is_cancelled();
    }

    /// Returns [`Cancelled`] once this handle was cancelled.
    ///
    /// Long-running library loops call this at coarse boundaries.
    /// Like [`Self::poll`], mapping to [`crate::error::Error::Cancelled`].
    pub(crate) fn poll_result(&self) -> crate::error::Result<()> {
        self.poll()
            .map_err(|Cancelled| crate::error::Error::Cancelled)
    }

    pub(crate) fn poll(&self) -> std::result::Result<(), Cancelled> {
        #[cfg(feature = "test-support")]
        {
            self.tick_poll_budget_only();
            // Match `is_cancelled()` for the integration-test hook (`CancelToken::with_check_counter`):
            // hot paths poll instead of calling `is_cancelled()`, but tests still expect every poll to
            // count as a cancellation check.
            if let Some(counter) = self.check_counter.as_ref() {
                counter.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            }
        }
        #[cfg(not(feature = "test-support"))]
        let _ = ();
        // Read the flag directly so we do not run `is_cancelled()` here: that would double-apply
        // `tick_poll_budget_only` in non-test builds and advance `polls_until_cancel` against
        // `CancelToken::after_polls` budgets in the same boundary as this `poll`.
        if self.flag.load(std::sync::atomic::Ordering::Relaxed) {
            Err(Cancelled)
        } else {
            Ok(())
        }
    }

    /// Shared flag for worker threads; clones share cancellation with this handle.
    pub(crate) fn watch_arc(&self) -> std::sync::Arc<std::sync::atomic::AtomicBool> {
        std::sync::Arc::clone(&self.flag)
    }

    #[allow(dead_code)]
    pub(crate) fn as_atomic(&self) -> &std::sync::atomic::AtomicBool {
        &self.flag
    }

    /// Whether cancellation was requested, reading only the shared flag (no `after_polls`
    /// countdown, no check counter). Used when mirroring into stack watches without consuming a
    /// poll budget.
    #[inline]
    pub(crate) fn is_cancelled_raw(&self) -> bool {
        self.flag.load(std::sync::atomic::Ordering::Relaxed)
    }

    /// Cancels on the `n`th call to [`Self::poll`] (for deterministic tests).
    #[cfg(feature = "test-support")]
    #[must_use]
    pub fn after_polls(n: u64) -> Self {
        Self {
            flag: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
            polls_until_cancel: Some(std::sync::Arc::new(std::sync::atomic::AtomicU64::new(n))),
            check_counter: None,
        }
    }
}

impl Default for CancelToken {
    fn default() -> Self {
        Self::new()
    }
}

/// Copy a stack [`CancelToken`] into `watch` for worker threads (call on the owning thread).
#[inline]
pub(crate) fn mirror_stack_cancel(cancel: &CancelToken, watch: &std::sync::atomic::AtomicBool) {
    if cancel.is_cancelled_raw() {
        watch.store(true, std::sync::atomic::Ordering::Relaxed);
    }
}

/// Shared flag for smart-HTTP waits; the owning thread mirrors a stack [`CancelToken`] into it.
pub(crate) struct HttpStackWatch(pub(crate) std::sync::Arc<std::sync::atomic::AtomicBool>);

impl HttpStackWatch {
    #[must_use]
    pub fn new(cancel: &CancelToken) -> Self {
        Self(cancel.watch_arc())
    }
}

/// Keeps [`HttpStackWatch`] alive for a fetch/push scope.
pub(crate) struct HttpWaitGuard {
    _watch: std::sync::Arc<std::sync::atomic::AtomicBool>,
}

impl HttpWaitGuard {
    pub(crate) fn bind(
        cancel: &CancelToken,
    ) -> (std::sync::Arc<std::sync::atomic::AtomicBool>, Self) {
        let watch = HttpStackWatch::new(cancel).0;
        (std::sync::Arc::clone(&watch), Self { _watch: watch })
    }
}

/// Share a [`CancelToken`] with smart-HTTP connect options for worker threads.
///
/// The owning thread mirrors the stack token into `stack_watch` during HTTP waits
/// (see [`mirror_stack_cancel`]).
#[must_use]
pub(crate) fn connect_cancel_arc(
    cancel: &CancelToken,
    stack_watch: std::sync::Arc<std::sync::atomic::AtomicBool>,
) -> std::sync::Arc<CancelToken> {
    mirror_stack_cancel(cancel, &stack_watch);
    std::sync::Arc::new(CancelToken::from_shared_flag(cancel.watch_arc()))
}

/// Returns [`crate::error::Error::Cancelled`] when `watch` was set.
#[inline]
pub(crate) fn poll_watch(watch: &std::sync::atomic::AtomicBool) -> crate::error::Result<()> {
    if watch.load(std::sync::atomic::Ordering::Relaxed) {
        Err(crate::error::Error::Cancelled)
    } else {
        Ok(())
    }
}

/// Poll hook for long worktree stat scans on the thread that owns the cancel token.
pub(crate) struct StackCancelPoll {
    watch: std::sync::Arc<std::sync::atomic::AtomicBool>,
}

impl StackCancelPoll {
    #[must_use]
    pub fn new(watch: std::sync::Arc<std::sync::atomic::AtomicBool>) -> Self {
        Self { watch }
    }

    pub(crate) fn poll_owner(&self) -> crate::error::Result<()> {
        poll_watch(&self.watch)
    }
}

/// Cancellation inputs for worktree stat scans: worker-visible `watch` plus an owner-thread poll hook.
pub(crate) struct StatScanCancel {
    pub watch: Option<std::sync::Arc<std::sync::atomic::AtomicBool>>,
    pub owner_poll: Option<std::sync::Arc<StackCancelPoll>>,
}

impl StatScanCancel {
    /// Poll owner progress (when present), otherwise the shared watch flag.
    pub(crate) fn poll(&self) -> crate::error::Result<()> {
        if let Some(poll) = self.owner_poll.as_ref() {
            poll.poll_owner()
        } else if let Some(w) = self.watch.as_ref() {
            poll_watch(w)
        } else {
            Ok(())
        }
    }
}

/// Like [`poll_cancel`], but also records cancellation in `watch` for worker threads.
#[inline]
pub(crate) fn poll_cancel_watch(
    cancel: &CancelToken,
    watch: &std::sync::atomic::AtomicBool,
) -> crate::error::Result<()> {
    match cancel.poll() {
        Err(_) => {
            watch.store(true, std::sync::atomic::Ordering::Relaxed);
            Err(crate::error::Error::Cancelled)
        }
        Ok(()) => poll_watch(watch),
    }
}

/// A [`CancelToken`] polled on the calling thread, plus the flag its worker threads read.
pub(crate) struct CancelSignal<'a> {
    cancel: &'a CancelToken,
}

impl<'a> CancelSignal<'a> {
    pub fn new(cancel: &'a CancelToken) -> Self {
        Self { cancel }
    }

    /// A signal that never fires, for callers that cannot be interrupted.
    pub(crate) fn never() -> CancelSignal<'static> {
        static NEVER: std::sync::LazyLock<CancelToken> = std::sync::LazyLock::new(CancelToken::new);
        CancelSignal::new(&NEVER)
    }

    /// Returns [`crate::error::Error::Cancelled`] once the signal fired.
    #[inline]
    pub(crate) fn poll(&self) -> crate::error::Result<()> {
        self.cancel
            .poll()
            .map_err(|_| crate::error::Error::Cancelled)
    }

    /// The flag worker threads check; set once [`Self::poll`] observed a cancel.
    pub(crate) fn flag(&self) -> &std::sync::atomic::AtomicBool {
        self.cancel.as_atomic()
    }

    /// Shared flag for long walks on other threads or modules.
    pub(crate) fn watch_arc(&self) -> std::sync::Arc<std::sync::atomic::AtomicBool> {
        self.cancel.watch_arc()
    }
}

/// The progress sink, cancel signal and shared watch flag one operation threads through its
/// helpers, so worker threads and the calling thread stop on the same signal.
pub(crate) struct ProgressScope<'a> {
    sink: &'a mut dyn ProgressSink,
    cancel: &'a CancelToken,
    watch: std::sync::Arc<std::sync::atomic::AtomicBool>,
}

impl<'a> ProgressScope<'a> {
    pub fn new(sink: &'a mut dyn ProgressSink, cancel: &'a CancelToken) -> Self {
        let watch = cancel.watch_arc();
        Self {
            sink,
            cancel,
            watch,
        }
    }

    /// Returns [`crate::error::Error::Cancelled`] once the cancel signal fired.
    pub(crate) fn poll(&mut self) -> crate::error::Result<()> {
        poll_cancel_watch(self.cancel, &self.watch)
    }

    /// Flag for worker threads; set once [`Self::poll`] observed a cancel.
    pub(crate) fn watch(&self) -> &std::sync::atomic::AtomicBool {
        &self.watch
    }

    pub(crate) fn sink(&mut self) -> &mut dyn ProgressSink {
        self.sink
    }
}

/// [`ProgressSink`] adapter that forwards every update but never asks to stop, for steps that
/// must run to completion once an operation has published its state.
pub(crate) struct UncancellableProgress<'a> {
    inner: &'a mut dyn ProgressSink,
}

impl<'a> UncancellableProgress<'a> {
    pub fn new(inner: &'a mut dyn ProgressSink) -> Self {
        Self { inner }
    }
}

impl ProgressSink for UncancellableProgress<'_> {
    fn start(&mut self, phase: ProgressPhase, total: Option<u64>) {
        self.inner.start(phase, total);
    }
    fn start_with_unit(&mut self, phase: ProgressPhase, unit: ProgressUnit, total: Option<u64>) {
        self.inner.start_with_unit(phase, unit, total);
    }
    fn inc(&mut self, units: u64) {
        self.inner.inc(units);
    }
    fn set(&mut self, current: u64) {
        self.inner.set(current);
    }
    fn finish(&mut self) {
        self.inner.finish();
    }
}

/// Returns [`crate::error::Error::Cancelled`] when `cancel` requests stop.
#[inline]
pub fn poll_cancel(
    _progress: &mut dyn ProgressSink,
    cancel: &CancelToken,
) -> crate::error::Result<()> {
    cancel.poll().map_err(|_| crate::error::Error::Cancelled)
}

/// [`ProgressSink`] that stops forwarding once the [`CancelToken`] fires.
///
/// Long checkout loops call [`Self::finish_poll`] after apply so callers get [`crate::error::Error::Cancelled`]
/// even when cancellation happened on an [`ProgressSink::inc`] boundary.
pub(crate) struct CancelProgress<'a> {
    inner: &'a mut dyn ProgressSink,
    cancel: &'a CancelToken,
    stopped: std::cell::Cell<bool>,
}

impl<'a> CancelProgress<'a> {
    pub fn new(inner: &'a mut dyn ProgressSink, cancel: &'a CancelToken) -> Self {
        Self {
            inner,
            cancel,
            stopped: std::cell::Cell::new(false),
        }
    }

    #[must_use]
    #[allow(dead_code)]
    pub fn stopped(&self) -> bool {
        self.stopped.get()
    }

    #[allow(dead_code)]
    pub fn finish_poll(&self) -> crate::error::Result<()> {
        if self.stopped() {
            Err(crate::error::Error::Cancelled)
        } else {
            Ok(())
        }
    }

    fn note_stop(&self) {
        if self.cancel.is_cancelled() {
            self.stopped.set(true);
        }
    }
}

impl ProgressSink for CancelProgress<'_> {
    fn start(&mut self, phase: ProgressPhase, total: Option<u64>) {
        self.note_stop();
        if !self.stopped.get() {
            self.inner.start(phase, total);
        }
    }

    fn start_with_unit(&mut self, phase: ProgressPhase, unit: ProgressUnit, total: Option<u64>) {
        self.note_stop();
        if !self.stopped.get() {
            self.inner.start_with_unit(phase, unit, total);
        }
    }

    fn inc(&mut self, units: u64) {
        self.note_stop();
        if !self.stopped.get() {
            self.inner.inc(units);
        }
    }

    fn set(&mut self, current: u64) {
        self.note_stop();
        if !self.stopped.get() {
            self.inner.set(current);
        }
    }

    fn finish(&mut self) {
        self.note_stop();
        if !self.stopped.get() {
            self.inner.finish();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fetch::{NoProgress, Progress};

    /// A sink that records typed phase updates, to prove the trait + forwarding
    /// impl route calls correctly.
    #[derive(Default)]
    struct PhaseRecorder {
        started: Vec<(ProgressPhase, Option<u64>)>,
        incs: u64,
        finished: usize,
    }

    impl ProgressSink for PhaseRecorder {
        fn start(&mut self, phase: ProgressPhase, total: Option<u64>) {
            self.started.push((phase, total));
        }
        fn inc(&mut self, units: u64) {
            self.incs += units;
        }
        fn finish(&mut self) {
            self.finished += 1;
        }
    }

    fn drive_typed(sink: &mut impl ProgressSink) {
        sink.start(ProgressPhase::ReceivingObjects, Some(3));
        sink.inc(1);
        sink.inc(2);
        sink.finish();
    }

    #[test]
    fn null_progress_is_inert() {
        drive_typed(&mut NullProgress);
    }

    #[test]
    fn no_progress_is_inert() {
        drive_typed(&mut NoProgress);
    }

    #[test]
    fn typed_phase_recorder_roundtrip() {
        let mut rec = PhaseRecorder::default();
        drive_typed(&mut &mut rec);
        assert_eq!(
            rec.started,
            vec![(ProgressPhase::ReceivingObjects, Some(3))]
        );
        assert_eq!(rec.incs, 3);
        assert_eq!(rec.finished, 1);
    }

    #[test]
    fn progress_supertrait_upcasts() {
        struct SidebandCap(Vec<u8>);

        impl ProgressSink for SidebandCap {}

        impl Progress for SidebandCap {
            fn message(&mut self, bytes: &[u8]) {
                self.0.extend_from_slice(bytes);
            }
        }

        fn takes_sink(sink: &mut dyn ProgressSink) {
            sink.start(ProgressPhase::IndexingObjects, None);
            sink.finish();
        }

        let mut cap = SidebandCap(Vec::new());
        let progress: &mut dyn Progress = &mut cap;
        takes_sink(progress);
    }

    #[test]
    fn poll_cancel_returns_typed_error() {
        let token = CancelToken::new();
        token.cancel();
        let mut sink = NullProgress;
        let err = poll_cancel(&mut sink, &token).expect_err("cancelled");
        assert!(matches!(err, crate::error::Error::Cancelled));
    }

    #[test]
    fn cancel_token_never_starts_cancelled() {
        assert!(!CancelToken::never().is_cancelled());
        assert!(!CancelToken::default().is_cancelled());
    }

    #[test]
    fn cancel_token_clone_shares_flag() {
        let token = CancelToken::new();
        let clone = token.clone();
        token.cancel();
        assert!(clone.is_cancelled());
    }

    #[cfg(feature = "test-support")]
    #[test]
    fn cancel_token_after_polls() {
        let token = CancelToken::after_polls(3);
        assert!(token.poll().is_ok());
        assert!(token.poll().is_ok());
        assert!(token.poll().is_err());
        assert!(token.is_cancelled());
    }

    #[cfg(feature = "test-support")]
    #[test]
    fn every_cancellation_check_path_is_counted_once() {
        let (token, checks) = CancelToken::with_check_counter();
        assert!(!token.is_cancelled());
        assert_eq!(checks.load(std::sync::atomic::Ordering::Relaxed), 1);
        assert!(token.poll().is_ok());
        assert_eq!(checks.load(std::sync::atomic::Ordering::Relaxed), 2);
        token.poll_before_wait();
        assert_eq!(checks.load(std::sync::atomic::Ordering::Relaxed), 3);
    }

    #[test]
    fn cancel_token_cross_thread() {
        let token = CancelToken::new();
        let peer = token.clone();
        std::thread::spawn(move || peer.cancel())
            .join()
            .expect("join");
        assert!(token.is_cancelled());
    }

    #[test]
    fn phase_default_unit() {
        assert_eq!(
            ProgressPhase::ReceivingObjects.default_unit(),
            ProgressUnit::Objects
        );
        assert_eq!(
            ProgressPhase::CheckingOutFiles.default_unit(),
            ProgressUnit::Files
        );
        assert_eq!(
            ProgressPhase::StagingFiles.default_unit(),
            ProgressUnit::Files
        );
    }
}

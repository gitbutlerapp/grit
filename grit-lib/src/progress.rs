//! Progress reporting and cancellation across the library/CLI boundary.
//!
//! Library operations never touch the terminal. Long-running work reports
//! progress through a [`ProgressSink`] supplied by the caller and checks a
//! [`Cancel`] signal at loop boundaries. The CLI provides a concrete sink that
//! draws to stderr (gated on `isatty`) and decides colour, redraw rate, and
//! whether a pager is attached; the library makes none of those decisions.
//!
//! Sinks are invoked from the **calling thread only**, at **coarse** granularity
//! (phase boundaries and periodic counts, not per-object spam). Totals passed
//! to [`ProgressSink::start`] may be [`None`] when the operation cannot know
//! the final count up front.
//!
//! Tests, `grit-cli`, and any call site that does not want output use the
//! no-op [`NullProgress`] / [`NeverCancel`].
//!
//! Library guide: [progress](https://grit-scm.com/docs/library/parallelism)
//!
//! ```rust
//! use grit_lib::progress::{NeverCancel, NullProgress};
//! # fn main() {
//! let _sink = NullProgress;
//! let _cancel = NeverCancel;
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
            | Self::CopyingObjects => ProgressUnit::Objects,
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

/// A cancellation signal checked by long-running library operations at loop
/// boundaries. Implementations must be cheap to query.
pub trait Cancel {
    /// Returns `true` once the operation should stop as soon as possible.
    fn is_cancelled(&self) -> bool;

    /// When cancellation is backed by a shared flag (for example [`CancelToken`]), returns it
    /// so worker threads can observe the same signal.
    fn shared_watch(&self) -> Option<std::sync::Arc<std::sync::atomic::AtomicBool>> {
        None
    }

    /// Mirror a stack-scoped cancel into a shared watch before blocking HTTP waits.
    fn poll_before_wait(&self) {}
}

/// A [`Cancel`] that never signals cancellation. The default for call sites
/// that do not support interruption.
#[derive(Debug, Default, Clone, Copy)]
pub struct NeverCancel;

impl Cancel for NeverCancel {
    fn is_cancelled(&self) -> bool {
        false
    }
}

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

/// Cooperative cancellation handle shared across threads (`Clone` shares the same flag).
#[derive(Debug, Clone)]
pub struct CancelToken {
    flag: std::sync::Arc<std::sync::atomic::AtomicBool>,
    #[cfg(feature = "test-support")]
    polls_until_cancel: Option<std::sync::Arc<std::sync::atomic::AtomicU64>>,
}

impl CancelToken {
    /// Create a handle that starts uncancelled.
    #[must_use]
    pub fn new() -> Self {
        Self {
            flag: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
            #[cfg(feature = "test-support")]
            polls_until_cancel: None,
        }
    }

    /// Alias for [`Self::new`]: a handle that never cancels unless [`Self::cancel`] is called.
    #[must_use]
    pub fn never() -> Self {
        Self::new()
    }

    pub(crate) fn from_shared_flag(flag: std::sync::Arc<std::sync::atomic::AtomicBool>) -> Self {
        Self {
            flag,
            #[cfg(feature = "test-support")]
            polls_until_cancel: None,
        }
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
        self.flag.load(std::sync::atomic::Ordering::Relaxed)
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
        if let Some(remaining) = self.polls_until_cancel.as_ref() {
            if remaining.fetch_sub(1, std::sync::atomic::Ordering::Relaxed) == 1 {
                self.cancel();
            }
        }
        if self.is_cancelled() {
            Err(Cancelled)
        } else {
            Ok(())
        }
    }

    /// Shared flag for worker threads; clones share cancellation with this handle.
    pub fn watch_arc(&self) -> std::sync::Arc<std::sync::atomic::AtomicBool> {
        std::sync::Arc::clone(&self.flag)
    }

    #[allow(dead_code)]
    pub(crate) fn as_atomic(&self) -> &std::sync::atomic::AtomicBool {
        &self.flag
    }

    /// Cancels on the `n`th call to [`Self::poll`] (for deterministic tests).
    #[cfg(feature = "test-support")]
    #[must_use]
    pub fn after_polls(n: u64) -> Self {
        Self {
            flag: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
            polls_until_cancel: Some(std::sync::Arc::new(std::sync::atomic::AtomicU64::new(n))),
        }
    }
}

impl Default for CancelToken {
    fn default() -> Self {
        Self::new()
    }
}

impl Cancel for CancelToken {
    fn is_cancelled(&self) -> bool {
        self.flag.load(std::sync::atomic::Ordering::Relaxed)
    }

    fn shared_watch(&self) -> Option<std::sync::Arc<std::sync::atomic::AtomicBool>> {
        Some(self.watch_arc())
    }
}

/// Copy a stack [`Cancel`] into `watch` for worker threads (call on the owning thread).
#[inline]
pub(crate) fn mirror_stack_cancel(cancel: &dyn Cancel, watch: &std::sync::atomic::AtomicBool) {
    cancel.poll_before_wait();
    if cancel.is_cancelled() {
        watch.store(true, std::sync::atomic::Ordering::Relaxed);
    }
}

/// Shared flag for smart-HTTP waits; the owning thread mirrors a stack [`Cancel`] into it.
pub(crate) struct HttpStackWatch(pub(crate) std::sync::Arc<std::sync::atomic::AtomicBool>);

impl HttpStackWatch {
    #[must_use]
    pub fn new(cancel: &dyn Cancel) -> Self {
        Self(cancel.shared_watch().unwrap_or_else(|| {
            std::sync::Arc::new(std::sync::atomic::AtomicBool::new(cancel.is_cancelled()))
        }))
    }
}

/// Keeps [`HttpStackWatch`] alive for a fetch/push scope.
pub(crate) struct HttpWaitGuard {
    _watch: std::sync::Arc<std::sync::atomic::AtomicBool>,
}

impl HttpWaitGuard {
    pub(crate) fn bind(
        cancel: &dyn Cancel,
    ) -> (std::sync::Arc<std::sync::atomic::AtomicBool>, Self) {
        let watch = HttpStackWatch::new(cancel).0;
        (std::sync::Arc::clone(&watch), Self { _watch: watch })
    }
}

/// Share a [`Cancel`] handle with smart-HTTP connect options for worker threads.
///
/// When [`Cancel::shared_watch`] is set, workers observe the same flag as the caller.
/// Otherwise `stack_watch` must be the fetch/push scope flag; the owning thread mirrors
/// the stack [`Cancel`] into it during HTTP waits (see [`mirror_stack_cancel`]).
#[must_use]
pub(crate) fn connect_cancel_arc(
    cancel: &dyn Cancel,
    stack_watch: std::sync::Arc<std::sync::atomic::AtomicBool>,
) -> std::sync::Arc<dyn Cancel + Send + Sync> {
    mirror_stack_cancel(cancel, &stack_watch);
    if let Some(flag) = cancel.shared_watch() {
        return std::sync::Arc::new(CancelToken {
            flag,
            #[cfg(feature = "test-support")]
            polls_until_cancel: None,
        });
    }
    std::sync::Arc::new(ConnectOperationCancel { watch: stack_watch })
}

/// Shared watch for HTTP worker threads when the caller's [`Cancel`] has no [`Cancel::shared_watch`].
struct ConnectOperationCancel {
    watch: std::sync::Arc<std::sync::atomic::AtomicBool>,
}

impl Cancel for ConnectOperationCancel {
    fn is_cancelled(&self) -> bool {
        self.watch.load(std::sync::atomic::Ordering::Relaxed)
    }

    fn shared_watch(&self) -> Option<std::sync::Arc<std::sync::atomic::AtomicBool>> {
        Some(std::sync::Arc::clone(&self.watch))
    }
}

/// Shared [`AtomicBool`](std::sync::atomic::AtomicBool) for cancellation visible across threads.
///
/// For [`CancelToken`], this is the same flag as the handle (so embedders can cancel mid-scan).
/// For other [`Cancel`] implementations, a private flag is seeded from [`Cancel::is_cancelled`]
/// at creation time; combine with [`poll_cancel_watch`] on the main thread.
#[must_use]
pub(crate) fn cancel_watch_arc(
    cancel: &dyn Cancel,
) -> std::sync::Arc<std::sync::atomic::AtomicBool> {
    cancel.shared_watch().unwrap_or_else(|| {
        std::sync::Arc::new(std::sync::atomic::AtomicBool::new(cancel.is_cancelled()))
    })
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
    cancel: &dyn Cancel,
    watch: &std::sync::atomic::AtomicBool,
) -> crate::error::Result<()> {
    cancel.poll_before_wait();
    if cancel.is_cancelled() {
        watch.store(true, std::sync::atomic::Ordering::Relaxed);
        return Err(crate::error::Error::Cancelled);
    }
    poll_watch(watch)
}

/// A [`Cancel`] polled on the calling thread, plus the flag its worker threads read.
///
/// [`Self::poll`] records a cancel in [`Self::flag`], so workers also stop for
/// [`Cancel`] implementations that are not backed by a shared flag (once the calling
/// thread next polls). For a [`CancelToken`] the flag is the handle's own, so workers
/// see a cancel from another thread right away.
pub(crate) struct CancelSignal<'a> {
    cancel: &'a dyn Cancel,
    flag: std::sync::Arc<std::sync::atomic::AtomicBool>,
}

impl<'a> CancelSignal<'a> {
    pub fn new(cancel: &'a dyn Cancel) -> Self {
        Self {
            cancel,
            flag: cancel_watch_arc(cancel),
        }
    }

    /// A signal that never fires, for callers that cannot be interrupted.
    pub(crate) fn never() -> CancelSignal<'static> {
        CancelSignal::new(&NeverCancel)
    }

    /// Returns [`crate::error::Error::Cancelled`] once the signal fired.
    #[inline]
    pub(crate) fn poll(&self) -> crate::error::Result<()> {
        self.cancel.poll_before_wait();
        if self.cancel.is_cancelled() {
            self.flag.store(true, std::sync::atomic::Ordering::Relaxed);
        }
        poll_watch(&self.flag)
    }

    /// The flag worker threads check; set once [`Self::poll`] observed a cancel.
    pub(crate) fn flag(&self) -> &std::sync::atomic::AtomicBool {
        &self.flag
    }

    /// Shared flag for long walks on other threads or modules.
    pub(crate) fn watch_arc(&self) -> std::sync::Arc<std::sync::atomic::AtomicBool> {
        std::sync::Arc::clone(&self.flag)
    }
}

/// The progress sink, cancel signal and shared watch flag one operation threads through its
/// helpers, so worker threads and the calling thread stop on the same signal.
pub(crate) struct ProgressScope<'a> {
    sink: &'a mut dyn ProgressSink,
    cancel: &'a dyn Cancel,
    watch: std::sync::Arc<std::sync::atomic::AtomicBool>,
}

impl<'a> ProgressScope<'a> {
    pub fn new(sink: &'a mut dyn ProgressSink, cancel: &'a dyn Cancel) -> Self {
        let watch = cancel_watch_arc(cancel);
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

/// Build a [`CancelToken`] that shares the same flag as `cancel` (for option structs).
#[must_use]
pub(crate) fn cancel_token_from(cancel: &dyn Cancel) -> CancelToken {
    CancelToken {
        flag: cancel_watch_arc(cancel),
        #[cfg(feature = "test-support")]
        polls_until_cancel: None,
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
    cancel: &dyn Cancel,
) -> crate::error::Result<()> {
    cancel.poll_before_wait();
    if cancel.is_cancelled() {
        Err(crate::error::Error::Cancelled)
    } else {
        Ok(())
    }
}

/// [`ProgressSink`] that stops forwarding once [`Cancel`] fires.
///
/// Long checkout loops call [`Self::finish_poll`] after apply so callers get [`crate::error::Error::Cancelled`]
/// even when cancellation happened on an [`ProgressSink::inc`] boundary.
pub(crate) struct CancelProgress<'a> {
    inner: &'a mut dyn ProgressSink,
    cancel: &'a dyn Cancel,
    stopped: std::cell::Cell<bool>,
}

impl<'a> CancelProgress<'a> {
    pub fn new(inner: &'a mut dyn ProgressSink, cancel: &'a dyn Cancel) -> Self {
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
    fn never_cancel_never_cancels() {
        assert!(!NeverCancel.is_cancelled());
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

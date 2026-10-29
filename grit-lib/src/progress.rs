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
            Self::TransferringBundle => ProgressUnit::Bytes,
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

    /// When `true`, the operation should stop at the next [`poll_cancel`] check.
    ///
    /// CLI progress sinks may set this when the user interrupts (Ctrl+C). Most
    /// call sites rely on a separate [`Cancel`] argument instead; either signal
    /// produces [`crate::error::Error::Cancelled`].
    fn should_cancel(&self) -> bool {
        false
    }
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
    fn should_cancel(&self) -> bool {
        (**self).should_cancel()
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

    /// When cancellation is backed by a shared flag (for example [`FlagCancel`]), returns it
    /// so worker threads can observe the same signal.
    fn shared_watch(&self) -> Option<std::sync::Arc<std::sync::atomic::AtomicBool>> {
        None
    }
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

/// Shared flag for tests and embedders that flip cancellation mid-operation.
#[derive(Debug, Clone)]
pub struct FlagCancel(pub std::sync::Arc<std::sync::atomic::AtomicBool>);

impl FlagCancel {
    /// Create a cancel signal that starts unset.
    #[must_use]
    pub fn new() -> Self {
        Self(std::sync::Arc::new(std::sync::atomic::AtomicBool::new(
            false,
        )))
    }

    /// Signal cancellation to every consumer polling this handle.
    pub fn cancel(&self) {
        self.0.store(true, std::sync::atomic::Ordering::Relaxed);
    }
}

impl Default for FlagCancel {
    fn default() -> Self {
        Self::new()
    }
}

impl Cancel for FlagCancel {
    fn is_cancelled(&self) -> bool {
        self.0.load(std::sync::atomic::Ordering::Relaxed)
    }

    fn shared_watch(&self) -> Option<std::sync::Arc<std::sync::atomic::AtomicBool>> {
        Some(std::sync::Arc::clone(&self.0))
    }
}

/// Shared [`AtomicBool`](std::sync::atomic::AtomicBool) for cancellation visible across threads.
///
/// For [`FlagCancel`], this is the same flag as the handle (so embedders can cancel mid-scan).
/// For other [`Cancel`] implementations, a private flag is seeded from [`Cancel::is_cancelled`]
/// at creation time; combine with [`poll_cancel_watch`] on the main thread for progress signals.
#[must_use]
pub fn cancel_watch_arc(cancel: &dyn Cancel) -> std::sync::Arc<std::sync::atomic::AtomicBool> {
    cancel.shared_watch().unwrap_or_else(|| {
        std::sync::Arc::new(std::sync::atomic::AtomicBool::new(cancel.is_cancelled()))
    })
}

/// Returns [`crate::error::Error::Cancelled`] when `watch` was set.
#[inline]
pub fn poll_watch(watch: &std::sync::atomic::AtomicBool) -> crate::error::Result<()> {
    if watch.load(std::sync::atomic::Ordering::Relaxed) {
        Err(crate::error::Error::Cancelled)
    } else {
        Ok(())
    }
}

/// Like [`poll_cancel`], but also records cancellation in `watch` for worker threads.
#[inline]
pub fn poll_cancel_watch(
    progress: &mut dyn ProgressSink,
    cancel: &dyn Cancel,
    watch: &std::sync::atomic::AtomicBool,
) -> crate::error::Result<()> {
    if cancel.is_cancelled() || progress.should_cancel() {
        watch.store(true, std::sync::atomic::Ordering::Relaxed);
        return Err(crate::error::Error::Cancelled);
    }
    poll_watch(watch)
}

/// A [`Cancel`] polled on the calling thread, plus the flag its worker threads read.
///
/// [`Self::poll`] records a cancel in [`Self::flag`], so workers also stop for
/// [`Cancel`] implementations that are not backed by a shared flag (once the calling
/// thread next polls). For a [`FlagCancel`] the flag is the handle's own, so workers
/// see a cancel from another thread right away.
pub(crate) struct CancelSignal<'a> {
    cancel: &'a dyn Cancel,
    flag: std::sync::Arc<std::sync::atomic::AtomicBool>,
}

impl<'a> CancelSignal<'a> {
    pub(crate) fn new(cancel: &'a dyn Cancel) -> Self {
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

/// [`ProgressSink`] adapter that mirrors [`ProgressSink::should_cancel`] into `watch`.
pub struct WatchProgress<'a> {
    inner: &'a mut dyn ProgressSink,
    watch: std::sync::Arc<std::sync::atomic::AtomicBool>,
}

impl<'a> WatchProgress<'a> {
    /// Wrap `inner` so any cancel request on the sink sets `watch`.
    #[must_use]
    pub fn new(
        inner: &'a mut dyn ProgressSink,
        watch: std::sync::Arc<std::sync::atomic::AtomicBool>,
    ) -> Self {
        Self { inner, watch }
    }

    fn note_cancel(&self) {
        if self.inner.should_cancel() {
            self.watch.store(true, std::sync::atomic::Ordering::Relaxed);
        }
    }
}

impl ProgressSink for WatchProgress<'_> {
    fn start(&mut self, phase: ProgressPhase, total: Option<u64>) {
        self.note_cancel();
        self.inner.start(phase, total);
    }
    fn start_with_unit(&mut self, phase: ProgressPhase, unit: ProgressUnit, total: Option<u64>) {
        self.note_cancel();
        self.inner.start_with_unit(phase, unit, total);
    }
    fn inc(&mut self, units: u64) {
        self.note_cancel();
        self.inner.inc(units);
    }
    fn set(&mut self, current: u64) {
        self.note_cancel();
        self.inner.set(current);
    }
    fn finish(&mut self) {
        self.note_cancel();
        self.inner.finish();
    }
    fn should_cancel(&self) -> bool {
        self.inner.should_cancel()
    }
}

/// The progress sink, cancel signal and shared watch flag one operation threads through its
/// helpers, so worker threads and the calling thread stop on the same signal.
pub(crate) struct ProgressScope<'a> {
    sink: WatchProgress<'a>,
    cancel: &'a dyn Cancel,
    watch: std::sync::Arc<std::sync::atomic::AtomicBool>,
}

impl<'a> ProgressScope<'a> {
    pub(crate) fn new(sink: &'a mut dyn ProgressSink, cancel: &'a dyn Cancel) -> Self {
        let watch = cancel_watch_arc(cancel);
        Self {
            sink: WatchProgress::new(sink, std::sync::Arc::clone(&watch)),
            cancel,
            watch,
        }
    }

    /// Returns [`crate::error::Error::Cancelled`] once either signal fired.
    pub(crate) fn poll(&mut self) -> crate::error::Result<()> {
        poll_cancel_watch(&mut self.sink, self.cancel, &self.watch)
    }

    /// Flag for worker threads; set once [`Self::poll`] observed a cancel.
    pub(crate) fn watch(&self) -> &std::sync::atomic::AtomicBool {
        &self.watch
    }

    pub(crate) fn sink(&mut self) -> &mut dyn ProgressSink {
        &mut self.sink
    }
}

/// [`ProgressSink`] adapter that forwards every update but never asks to stop, for steps that
/// must run to completion once an operation has published its state.
pub(crate) struct UncancellableProgress<'a> {
    inner: &'a mut dyn ProgressSink,
}

impl<'a> UncancellableProgress<'a> {
    pub(crate) fn new(inner: &'a mut dyn ProgressSink) -> Self {
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

/// Returns [`crate::error::Error::Cancelled`] when `cancel` or `progress` request stop.
#[inline]
pub fn poll_cancel(
    progress: &mut dyn ProgressSink,
    cancel: &dyn Cancel,
) -> crate::error::Result<()> {
    if cancel.is_cancelled() || progress.should_cancel() {
        Err(crate::error::Error::Cancelled)
    } else {
        Ok(())
    }
}

/// [`ProgressSink`] that stops forwarding once [`Cancel`] or [`ProgressSink::should_cancel`] fires.
///
/// Long checkout loops call [`Self::finish_poll`] after apply so callers get [`crate::error::Error::Cancelled`]
/// even when cancellation happened on an [`ProgressSink::inc`] boundary.
pub struct CancelProgress<'a> {
    inner: &'a mut dyn ProgressSink,
    cancel: &'a dyn Cancel,
    stopped: std::cell::Cell<bool>,
}

impl<'a> CancelProgress<'a> {
    /// Wrap `inner` so every progress update observes `cancel`.
    pub fn new(inner: &'a mut dyn ProgressSink, cancel: &'a dyn Cancel) -> Self {
        Self {
            inner,
            cancel,
            stopped: std::cell::Cell::new(false),
        }
    }

    /// Whether a cancel signal was observed.
    #[must_use]
    pub fn stopped(&self) -> bool {
        self.stopped.get()
    }

    /// Returns [`crate::error::Error::Cancelled`] when [`Self::stopped`].
    pub fn finish_poll(&self) -> crate::error::Result<()> {
        if self.stopped() {
            Err(crate::error::Error::Cancelled)
        } else {
            Ok(())
        }
    }

    fn note_stop(&self) {
        if self.cancel.is_cancelled() || self.inner.should_cancel() {
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

    fn should_cancel(&self) -> bool {
        self.stopped.get() || self.cancel.is_cancelled() || self.inner.should_cancel()
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
        let flag = FlagCancel::new();
        flag.cancel();
        let mut sink = NullProgress;
        let err = poll_cancel(&mut sink, &flag).expect_err("cancelled");
        assert!(matches!(err, crate::error::Error::Cancelled));
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

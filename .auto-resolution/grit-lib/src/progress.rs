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
    /// Building or reading a pack index after objects are on disk.
    IndexingObjects,
    /// Applying delta chains inside a pack.
    ResolvingDeltas,
    /// Copying objects between local repositories (e.g. local fetch).
    CopyingObjects,
    /// Updating the work tree to match a tree (checkout, reset --hard, clone).
    CheckingOutFiles,
}

impl ProgressPhase {
    /// Returns the unit callers should use when reporting counts for `self`.
    #[must_use]
    pub fn default_unit(self) -> ProgressUnit {
        match self {
            Self::ReceivingObjects
            | Self::IndexingObjects
            | Self::ResolvingDeltas
            | Self::CopyingObjects => ProgressUnit::Objects,
            Self::CheckingOutFiles => ProgressUnit::Files,
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
    fn phase_default_unit() {
        assert_eq!(
            ProgressPhase::ReceivingObjects.default_unit(),
            ProgressUnit::Objects
        );
        assert_eq!(
            ProgressPhase::CheckingOutFiles.default_unit(),
            ProgressUnit::Files
        );
    }
}

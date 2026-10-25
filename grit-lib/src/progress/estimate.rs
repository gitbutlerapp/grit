//! One overall completion estimate across the phases of a fetch or clone.
//!
//! A clone runs as a sequence of phases — transfer the pack, index it, resolve
//! its deltas, check out files — each with its own unit and total.
//! [`OverallEstimator`] follows the same [`ProgressPhase`] events a sink sees
//! and turns them into a single fraction and a time-left estimate: phases that
//! are under way are projected from their own measured pace, and phases that
//! have not started are sized from what is already known (the pack's object
//! and delta counts) at typical [`PhaseRates`].
//!
//! The estimator never reads the clock; every call takes the time elapsed
//! since the operation began.

use std::time::Duration;

use super::{ProgressPhase, ProgressUnit, ReceivedObjects};

/// Typical throughput used to size phases that have not started yet.
///
/// Phases already under way are projected from their own measured pace, so
/// these only shape the estimate until each phase begins.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PhaseRates {
    /// Objects indexed per second.
    pub indexing: f64,
    /// Deltas resolved per second.
    pub resolving: f64,
    /// Files checked out per second.
    pub checkout: f64,
}

impl Default for PhaseRates {
    /// Rates measured from `grit clone` of large public repositories on a
    /// current laptop.
    fn default() -> Self {
        Self {
            indexing: 250_000.0,
            resolving: 300_000.0,
            checkout: 10_000.0,
        }
    }
}

/// Share of deltas assumed in a pack whose delta count is not known yet.
const ASSUMED_DELTA_SHARE: f64 = 0.7;
/// Work-tree files assumed per pack object before checkout reports its total.
const ASSUMED_FILES_PER_OBJECT: f64 = 0.015;
/// Below this share of a phase done, its measured pace is too noisy to trust
/// over the typical rate.
const TRUST_MEASURED_AFTER: f64 = 0.1;
/// The fraction reported while any phase is still ahead.
const MAX_UNFINISHED_FRACTION: f64 = 0.99;

/// A snapshot of overall progress.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct OverallEstimate {
    /// Share of the whole operation done, from `0.0` to `1.0`. It never goes
    /// down between calls, and stays below `1.0` until every phase finished.
    pub fraction: f64,
    /// Estimated time until the operation finishes.
    pub remaining: Duration,
}

/// The phases of a clone, in the order they run.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Stage {
    Transfer,
    Indexing,
    Resolving,
    Checkout,
}

const STAGES: [Stage; 4] = [
    Stage::Transfer,
    Stage::Indexing,
    Stage::Resolving,
    Stage::Checkout,
];

impl Stage {
    fn of(phase: ProgressPhase) -> Option<Self> {
        match phase {
            ProgressPhase::ReceivingObjects | ProgressPhase::CopyingObjects => Some(Self::Transfer),
            ProgressPhase::IndexingObjects => Some(Self::Indexing),
            ProgressPhase::ResolvingDeltas => Some(Self::Resolving),
            ProgressPhase::CheckingOutFiles => Some(Self::Checkout),
            // Shown by its own display; the estimate covers the fetch after it.
            ProgressPhase::TransferringBundle => None,
        }
    }
}

#[derive(Debug, Clone, Copy)]
struct Active {
    stage: Stage,
    /// Total for the phase in its own unit; `None` for a byte-counted receive.
    total: Option<u64>,
    current: u64,
    started: Duration,
}

/// Combines per-phase progress into one overall estimate.
///
/// Phases under way are projected from their own measured pace; phases not
/// yet started are sized from the pack's object and delta counts at typical
/// [`PhaseRates`]. The estimator never reads the clock: every call takes the
/// time elapsed since the operation began.
///
/// ```rust
/// use std::time::Duration;
/// use grit_lib::progress::{OverallEstimator, ProgressPhase, ProgressUnit, ReceivedObjects};
///
/// let secs = Duration::from_secs;
/// let mut est = OverallEstimator::new(true);
/// est.start(ProgressPhase::ReceivingObjects, ProgressUnit::Bytes, None, secs(1));
/// est.received_objects(ReceivedObjects { received: 500, total: 1_000, deltas: 300 }, secs(3));
/// let now = est.estimate(secs(3)).unwrap();
/// assert!(now.fraction > 0.3 && now.fraction < 0.7);
/// ```
#[derive(Debug, Clone)]
pub struct OverallEstimator {
    rates: PhaseRates,
    checkout: bool,
    objects: Option<u64>,
    deltas: Option<u64>,
    files: Option<u64>,
    received: Option<ReceivedObjects>,
    active: Option<Active>,
    /// The furthest stage that has finished.
    finished: Option<Stage>,
    best: f64,
}

impl OverallEstimator {
    /// Create an estimator with the default [`PhaseRates`].
    ///
    /// # Parameters
    ///
    /// - `checkout`: whether a work-tree checkout follows the transfer (true
    ///   for a non-bare clone, false for a fetch).
    #[must_use]
    pub fn new(checkout: bool) -> Self {
        Self {
            rates: PhaseRates::default(),
            checkout,
            objects: None,
            deltas: None,
            files: None,
            received: None,
            active: None,
            finished: None,
            best: 0.0,
        }
    }

    /// Use `rates` to size phases that have not started.
    #[must_use]
    pub fn with_rates(mut self, rates: PhaseRates) -> Self {
        self.rates = rates;
        self
    }

    /// A phase began at `at`, counting `unit`s towards `total` when known.
    ///
    /// Starting a phase ends the previous one.
    pub fn start(
        &mut self,
        phase: ProgressPhase,
        unit: ProgressUnit,
        total: Option<u64>,
        at: Duration,
    ) {
        self.finish(at);
        let Some(stage) = Stage::of(phase) else {
            return;
        };
        let total = total.filter(|_| unit != ProgressUnit::Bytes);
        match stage {
            Stage::Transfer | Stage::Indexing => {
                if total.is_some() {
                    self.objects = total;
                }
            }
            Stage::Resolving => self.deltas = total,
            Stage::Checkout => self.files = total,
        }
        self.active = Some(Active {
            stage,
            total,
            current: 0,
            started: at,
        });
    }

    /// The current phase reached `current` units.
    pub fn set(&mut self, current: u64) {
        if let Some(active) = &mut self.active {
            active.current = current;
        }
    }

    /// The current phase advanced by `units`.
    pub fn inc(&mut self, units: u64) {
        if let Some(active) = &mut self.active {
            active.current = active.current.saturating_add(units);
        }
    }

    /// The pack being received now holds `counts` objects.
    ///
    /// Sizes the later phases: the pack's object total drives indexing, and
    /// the share of deltas seen so far projects how many will need resolving.
    pub fn received_objects(&mut self, counts: ReceivedObjects, _at: Duration) {
        self.objects = Some(counts.total);
        if counts.received > 0 {
            let share = counts.deltas as f64 / counts.received as f64;
            self.deltas = Some((share * counts.total as f64).round() as u64);
        }
        self.received = Some(counts);
    }

    /// The current phase finished at `at`.
    pub fn finish(&mut self, _at: Duration) {
        if let Some(active) = self.active.take() {
            self.finished = self.finished.max(Some(active.stage));
        }
    }

    /// Overall progress at `at`, or `None` while there is not enough to go on
    /// (before the transfer has reported any objects).
    pub fn estimate(&mut self, at: Duration) -> Option<OverallEstimate> {
        let remaining = self.remaining_secs(at)?;
        let elapsed = at.as_secs_f64();
        let all_done = self.active.is_none() && self.finished == Some(self.last_stage());
        let raw = if all_done {
            1.0
        } else if elapsed + remaining <= 0.0 {
            0.0
        } else {
            (elapsed / (elapsed + remaining)).min(MAX_UNFINISHED_FRACTION)
        };
        self.best = self.best.max(raw);
        Some(OverallEstimate {
            fraction: self.best,
            remaining: Duration::from_secs_f64(remaining.max(0.0)),
        })
    }

    fn last_stage(&self) -> Stage {
        if self.checkout {
            Stage::Checkout
        } else {
            Stage::Resolving
        }
    }

    fn remaining_secs(&self, at: Duration) -> Option<f64> {
        let mut total = 0.0;
        for stage in STAGES {
            if stage > self.last_stage() || Some(stage) <= self.finished {
                continue;
            }
            match self.active {
                Some(active) if active.stage == stage => total += self.active_secs(active, at)?,
                // Started later than this stage, so this one was skipped.
                Some(active) if active.stage > stage => {}
                _ => total += self.pending_secs(stage)?,
            }
        }
        Some(total)
    }

    /// Seconds left in the phase under way.
    fn active_secs(&self, active: Active, at: Duration) -> Option<f64> {
        let elapsed = at.saturating_sub(active.started).as_secs_f64();
        let (done, total) = match (active.stage, active.total) {
            (_, Some(total)) => (active.current, total),
            (Stage::Transfer, None) => {
                let counts = self.received?;
                (counts.received, counts.total)
            }
            (_, None) => return self.pending_secs(active.stage),
        };
        let left = total.saturating_sub(done) as f64;
        if left == 0.0 {
            return Some(0.0);
        }
        let share = if total == 0 {
            1.0
        } else {
            done as f64 / total as f64
        };
        let typical = self.rate(active.stage).map(|rate| left / rate);
        if share >= TRUST_MEASURED_AFTER || typical.is_none() {
            if done == 0 {
                return None;
            }
            return Some(elapsed * left / done as f64);
        }
        typical
    }

    /// Seconds a phase that has not started is expected to take.
    fn pending_secs(&self, stage: Stage) -> Option<f64> {
        let objects = self.objects? as f64;
        let units = match stage {
            // Nothing predicts how fast the network will be.
            Stage::Transfer => return None,
            Stage::Indexing => objects,
            Stage::Resolving => self
                .deltas
                .map_or(objects * ASSUMED_DELTA_SHARE, |d| d as f64),
            Stage::Checkout => self
                .files
                .map_or(objects * ASSUMED_FILES_PER_OBJECT, |f| f as f64),
        };
        Some(units / self.rate(stage)?)
    }

    fn rate(&self, stage: Stage) -> Option<f64> {
        let rate = match stage {
            Stage::Transfer => return None,
            Stage::Indexing => self.rates.indexing,
            Stage::Resolving => self.rates.resolving,
            Stage::Checkout => self.rates.checkout,
        };
        (rate > 0.0).then_some(rate)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn secs(s: f64) -> Duration {
        Duration::from_secs_f64(s)
    }

    fn rates() -> PhaseRates {
        PhaseRates {
            indexing: 1_000.0,
            resolving: 1_000.0,
            checkout: 100.0,
        }
    }

    fn receiving(est: &mut OverallEstimator, at: f64) {
        est.start(
            ProgressPhase::ReceivingObjects,
            ProgressUnit::Bytes,
            None,
            secs(at),
        );
    }

    #[test]
    fn no_estimate_until_objects_are_counted() {
        let mut est = OverallEstimator::new(true);
        assert!(est.estimate(secs(1.0)).is_none());
        receiving(&mut est, 1.0);
        est.inc(4096);
        assert!(
            est.estimate(secs(2.0)).is_none(),
            "bytes alone are not enough"
        );
    }

    #[test]
    fn receive_projects_its_pace_and_sizes_later_phases() {
        let mut est = OverallEstimator::new(true).with_rates(rates());
        receiving(&mut est, 0.0);
        // Half of 2,000 objects in 4s; 60% deltas so far.
        est.received_objects(
            ReceivedObjects {
                received: 1_000,
                total: 2_000,
                deltas: 600,
            },
            secs(4.0),
        );
        let e = est.estimate(secs(4.0)).unwrap();
        // 4s more receiving + 2s indexing + 1.2s deltas + 0.3s checkout.
        assert!(
            (e.remaining.as_secs_f64() - 7.5).abs() < 1e-6,
            "{:?}",
            e.remaining
        );
        assert!((e.fraction - 4.0 / 11.5).abs() < 1e-6, "{}", e.fraction);
    }

    #[test]
    fn active_phase_uses_measured_pace_once_trustworthy() {
        let mut est = OverallEstimator::new(false).with_rates(rates());
        est.start(
            ProgressPhase::IndexingObjects,
            ProgressUnit::Objects,
            Some(1_000),
            secs(10.0),
        );
        // Early on, the typical rate wins: 990 left at 1,000/s.
        est.set(10);
        let early = est.estimate(secs(10.5)).unwrap();
        assert!((early.remaining.as_secs_f64() - (0.99 + 0.7)).abs() < 1e-6);
        // At 50% after 2s, 2s more indexing at the measured pace.
        est.set(500);
        let later = est.estimate(secs(12.0)).unwrap();
        assert!((later.remaining.as_secs_f64() - (2.0 + 0.7)).abs() < 1e-6);
    }

    #[test]
    fn known_totals_replace_assumptions() {
        let mut est = OverallEstimator::new(true).with_rates(rates());
        est.start(
            ProgressPhase::ResolvingDeltas,
            ProgressUnit::Objects,
            Some(0),
            secs(1.0),
        );
        est.finish(secs(1.0));
        est.start(
            ProgressPhase::CheckingOutFiles,
            ProgressUnit::Files,
            Some(200),
            secs(1.0),
        );
        // Earlier stages count as done; 200 files at 100/s.
        let e = est.estimate(secs(1.0)).unwrap();
        assert!((e.remaining.as_secs_f64() - 2.0).abs() < 1e-6);
    }

    #[test]
    fn fraction_never_goes_backwards_and_reaches_one_when_done() {
        let mut est = OverallEstimator::new(false).with_rates(rates());
        receiving(&mut est, 0.0);
        est.received_objects(
            ReceivedObjects {
                received: 900,
                total: 1_000,
                deltas: 0,
            },
            secs(9.0),
        );
        let high = est.estimate(secs(9.0)).unwrap().fraction;
        // The last tenth stalls: the projection gets worse, the bar holds.
        let stalled = est.estimate(secs(30.0)).unwrap().fraction;
        assert!(stalled >= high);
        est.start(
            ProgressPhase::IndexingObjects,
            ProgressUnit::Objects,
            Some(1_000),
            secs(30.0),
        );
        est.set(1_000);
        est.start(
            ProgressPhase::ResolvingDeltas,
            ProgressUnit::Objects,
            Some(0),
            secs(31.0),
        );
        let unfinished = est.estimate(secs(31.0)).unwrap().fraction;
        assert!(unfinished < 1.0);
        est.finish(secs(31.0));
        let done = est.estimate(secs(31.0)).unwrap();
        assert_eq!(done.fraction, 1.0);
        assert_eq!(done.remaining, Duration::ZERO);
    }

    #[test]
    fn local_copy_with_known_total_is_estimated() {
        let mut est = OverallEstimator::new(false).with_rates(rates());
        est.start(
            ProgressPhase::CopyingObjects,
            ProgressUnit::Objects,
            Some(100),
            secs(0.0),
        );
        est.set(50);
        let e = est.estimate(secs(1.0)).unwrap();
        // 1s more copying + 0.1s indexing + 0.07s deltas.
        assert!((e.remaining.as_secs_f64() - 1.17).abs() < 1e-6, "{e:?}");
    }
}

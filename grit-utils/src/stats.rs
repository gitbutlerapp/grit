//! Statistics helpers for benchmark results.

use crate::hyperfine::HyperfineResultEntry;
use crate::schema::TimingStats;

/// Convert hyperfine seconds to millisecond [`TimingStats`].
pub fn timing_from_hyperfine(entry: &HyperfineResultEntry) -> TimingStats {
    let runs_ms = entry
        .times
        .as_ref()
        .map(|times| times.iter().map(|s| s * 1000.0).collect())
        .unwrap_or_default();

    TimingStats {
        mean_ms: entry.mean * 1000.0,
        median_ms: entry.median * 1000.0,
        stddev_ms: entry.stddev.unwrap_or(0.0) * 1000.0,
        min_ms: entry.min * 1000.0,
        max_ms: entry.max * 1000.0,
        runs_ms,
    }
}

/// Ratio of grit median to git median.
pub fn median_ratio(git_median_ms: f64, grit_median_ms: f64) -> f64 {
    if git_median_ms <= 0.0 {
        return f64::NAN;
    }
    grit_median_ms / git_median_ms
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn median_ratio_computation() {
        assert!((median_ratio(100.0, 50.0) - 0.5).abs() < f64::EPSILON);
        assert!((median_ratio(200.0, 400.0) - 2.0).abs() < f64::EPSILON);
    }
}

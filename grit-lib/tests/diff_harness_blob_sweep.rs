//! Drive `diff_harness` blob matrices through grit diff APIs for line coverage.

use crate::support;

use grit_lib::diff::{
    line_change_counts, normalize_ignore_space_change, unified_diff, DiffAlgorithm, HunkExtent,
    UnifiedDiffOptions,
};
use support::{blob_pairs, grit_blob_patch, grit_numstat, GritBlobPatchOpts, LineDiffAlgo};

#[test]
fn all_blob_pairs_exercise_diff_printers() {
    for pair in blob_pairs() {
        let old_s = std::str::from_utf8(pair.old).unwrap_or("");
        let new_s = std::str::from_utf8(pair.new).unwrap_or("");
        for algo in [
            LineDiffAlgo::Myers,
            LineDiffAlgo::Histogram,
            LineDiffAlgo::Patience,
            LineDiffAlgo::Minimal,
        ] {
            let _ = grit_numstat(old_s, new_s, algo);
            let opts = GritBlobPatchOpts {
                algo,
                ..GritBlobPatchOpts::default()
            };
            let patch = grit_blob_patch(pair.old, pair.new, pair.name, &opts);
            assert!(!patch.is_empty() || pair.old == pair.new);
        }
        let _ = normalize_ignore_space_change(old_s);
        for algorithm in [
            DiffAlgorithm::Myers,
            DiffAlgorithm::Patience,
            DiffAlgorithm::Histogram,
        ] {
            let options = UnifiedDiffOptions::new()
                .with_paths("x", "x")
                .with_algorithm(algorithm)
                .with_extent(HunkExtent::Function);
            let _ = unified_diff(old_s, new_s, &options);
            let _ = line_change_counts(old_s, new_s, &options);
        }
        let anchored = UnifiedDiffOptions::new()
            .with_paths("x", "x")
            .with_anchors(["A", "x"]);
        let _ = unified_diff(old_s, new_s, &anchored);
        let _ = line_change_counts(old_s, new_s, &anchored);
    }
}

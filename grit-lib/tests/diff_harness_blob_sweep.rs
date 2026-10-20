//! Drive `diff_harness` blob matrices through grit diff APIs for line coverage.

mod support;

use grit_lib::diff::{
    anchored_unified_diff, annotate_unified_diff_no_newline_at_eof, count_changes_with_algorithm,
    normalize_ignore_space_change, unified_diff_histogram_hunks_only,
    unified_diff_with_prefix_and_funcname_and_algorithm,
};
use similar::Algorithm;
use support::{
    blob_pairs, grit_blob_patch, grit_diffstat_block, grit_numstat, grit_stat_line,
    GritBlobPatchOpts, LineDiffAlgo,
};

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
            let _ = grit_diffstat_block(pair.name, pair.old, pair.new, algo);
            let (ins, del) = grit_numstat(old_s, new_s, algo);
            let _ = grit_stat_line(pair.name, ins, del, 80);
            let opts = GritBlobPatchOpts {
                algo,
                ..GritBlobPatchOpts::default()
            };
            let patch = grit_blob_patch(pair.old, pair.new, pair.name, &opts);
            assert!(!patch.is_empty() || pair.old == pair.new);
            let _ = annotate_unified_diff_no_newline_at_eof(&patch, old_s, new_s);
        }
        let _ = normalize_ignore_space_change(old_s);
        let _ = unified_diff_histogram_hunks_only(old_s, new_s, 3, 0, false);
        for algo in [Algorithm::Myers, Algorithm::Patience, Algorithm::Histogram] {
            let _ = unified_diff_with_prefix_and_funcname_and_algorithm(
                old_s, new_s, "a/x", "b/x", 3, 0, "a/", "b/", None, algo, true, false, true, false,
            );
            let _ = count_changes_with_algorithm(old_s, new_s, algo, algo == Algorithm::Myers);
        }
        let anchors = vec!["A".to_string(), "x".to_string()];
        let _ = anchored_unified_diff(
            old_s,
            new_s,
            "a/x",
            "b/x",
            3,
            &anchors,
            Algorithm::Myers,
            false,
            true,
            false,
        );
    }
}

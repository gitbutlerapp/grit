//! Maintenance matrix scenarios.

use super::super::helpers::headline_spec;
use super::super::matrix_helpers::{
    correctness_repack, fsck_full_bench, gc_default_bench, prepare_daily_reset, prepare_gc_seed,
    repack_commit_graph_bench, repack_full_bench, setup_big_blobs, setup_git_git,
    setup_git_git_repack_smoke, setup_synth10k_gc,
};
use super::super::plan::DailyHeadlinePlan;

pub fn plans() -> Vec<DailyHeadlinePlan> {
    vec![
        DailyHeadlinePlan {
            spec: headline_spec("gc/default@synth-10k"),
            pending: false,
            setup: setup_synth10k_gc,
            prepare: prepare_gc_seed,
            build_git: |c| gc_default_bench(c).1,
            build_grit: |c| gc_default_bench(c).0,
            correctness: None,
        },
        DailyHeadlinePlan {
            spec: headline_spec("repack/full@git-git"),
            pending: false,
            setup: setup_git_git_repack_smoke,
            prepare: prepare_daily_reset,
            build_git: |c| repack_full_bench(c).1,
            build_grit: |c| repack_full_bench(c).0,
            correctness: Some(correctness_repack),
        },
        DailyHeadlinePlan {
            spec: headline_spec("fsck/full@git-git"),
            pending: false,
            setup: setup_git_git,
            prepare: super::super::helpers::prepare_none,
            build_git: |c| fsck_full_bench(c).1,
            build_grit: |c| fsck_full_bench(c).0,
            correctness: None,
        },
        DailyHeadlinePlan {
            spec: headline_spec("fsck/full@big-blobs"),
            pending: false,
            setup: setup_big_blobs,
            prepare: super::super::helpers::prepare_none,
            build_git: |c| fsck_full_bench(c).1,
            build_grit: |c| fsck_full_bench(c).0,
            correctness: None,
        },
        DailyHeadlinePlan {
            spec: headline_spec("repack/commit-graph@git-git"),
            pending: false,
            setup: setup_git_git_repack_smoke,
            prepare: prepare_daily_reset,
            build_git: |c| repack_commit_graph_bench(c).1,
            build_grit: |c| repack_commit_graph_bench(c).0,
            correctness: Some(correctness_repack),
        },
    ]
}

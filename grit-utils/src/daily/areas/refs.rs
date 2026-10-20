//! Refs matrix scenarios.

use super::super::helpers::headline_spec;
use super::super::matrix_helpers::{gc_pack_refs_bench, prepare_gc_seed, setup_synth10k_gc};
use super::super::plan::DailyHeadlinePlan;

pub fn plans() -> Vec<DailyHeadlinePlan> {
    vec![DailyHeadlinePlan {
        spec: headline_spec("gc/pack-refs@synth-10k"),
        pending: false,
        setup: setup_synth10k_gc,
        prepare: prepare_gc_seed,
        build_git: |c| gc_pack_refs_bench(c).1,
        build_grit: |c| gc_pack_refs_bench(c).0,
        correctness: None,
    }]
}

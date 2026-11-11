//! Thin wrappers around [`grit_lib::merge::Merge`] for integration tests (legacy call shapes).

use grit_lib::ident::Signature;
use grit_lib::merge::{
    BranchIntegrateRequest, ContinueOptions, ContinueOutcome, IntegrateBranchOutcome,
    MergeDryRunPlan, MergeOptions, MergePrediction, MergePredictionOptions, MergeSource,
    MergeSourceKind, ReplaySequenceRequest, SequenceOutcome, SequencePrediction, WrittenMergeTree,
};
use grit_lib::merge::{RerereAutoupdate, RerereEvent};
use grit_lib::objects::ObjectId;
use grit_lib::porcelain::replay::{ReplayOutcome, ReplayRequest};
use grit_lib::progress::{CancelToken, ProgressSink};
use grit_lib::repo::Repository;

/// Integrate `other_oid` into `into_ref` at `into_oid` (test helper).
pub fn integrate_branch(
    repo: &Repository,
    into_ref: &str,
    into_oid: ObjectId,
    other_oid: ObjectId,
    label: &str,
    progress: &mut dyn ProgressSink,
    cancel: &CancelToken,
) -> grit_lib::error::Result<IntegrateBranchOutcome> {
    repo.merge().integrate_into_branch(
        &BranchIntegrateRequest {
            into_ref,
            into_oid,
            source: &MergeSource::new(other_oid, MergeSourceKind::Commit),
            label,
        },
        &MergeOptions::default(),
        progress,
        cancel,
    )
}

/// Integrate with explicit [`MergeOptions`].
#[allow(clippy::too_many_arguments)] // legacy positional shape for integration tests
pub fn integrate_branch_with(
    repo: &Repository,
    into_ref: &str,
    into_oid: ObjectId,
    other_oid: ObjectId,
    label: &str,
    options: &MergeOptions,
    progress: &mut dyn ProgressSink,
    cancel: &CancelToken,
) -> grit_lib::error::Result<IntegrateBranchOutcome> {
    repo.merge().integrate_into_branch(
        &BranchIntegrateRequest {
            into_ref,
            into_oid,
            source: &MergeSource::new(other_oid, MergeSourceKind::Commit),
            label,
        },
        options,
        progress,
        cancel,
    )
}

/// Integrate into detached `HEAD`.
pub fn integrate_detached_head(
    repo: &Repository,
    into_oid: ObjectId,
    other_oid: ObjectId,
    label: &str,
    progress: &mut dyn ProgressSink,
    cancel: &CancelToken,
) -> grit_lib::error::Result<IntegrateBranchOutcome> {
    let _ = into_oid;
    repo.merge().integrate(
        &MergeSource::new(other_oid, MergeSourceKind::Commit),
        label,
        &MergeOptions::default(),
        progress,
        cancel,
    )
}

/// Integrate into detached `HEAD` with explicit [`MergeOptions`].
#[allow(clippy::too_many_arguments)]
pub fn integrate_detached_head_with(
    repo: &Repository,
    into_oid: ObjectId,
    other_oid: ObjectId,
    label: &str,
    options: &MergeOptions,
    progress: &mut dyn ProgressSink,
    cancel: &CancelToken,
) -> grit_lib::error::Result<IntegrateBranchOutcome> {
    let _ = into_oid;
    repo.merge().integrate(
        &MergeSource::new(other_oid, MergeSourceKind::Commit),
        label,
        options,
        progress,
        cancel,
    )
}

/// Test-only access to tree flattening (hostile path validation).
pub fn tree_to_index_entries(
    repo: &Repository,
    oid: &ObjectId,
    prefix: &str,
) -> grit_lib::error::Result<Vec<grit_lib::index::IndexEntry>> {
    grit_lib::test_support::merge::tree_to_index_entries(repo, oid, prefix)
}

pub fn abort_branch_merge(repo: &Repository) -> grit_lib::error::Result<()> {
    repo.merge().abort()
}

pub fn continue_merge(
    repo: &Repository,
    options: &ContinueOptions,
    progress: &mut dyn ProgressSink,
    cancel: &CancelToken,
) -> grit_lib::error::Result<ContinueOutcome> {
    repo.merge().continue_merge(options, progress, cancel)
}

pub fn resolve_merge_source(repo: &Repository, spec: &str) -> grit_lib::error::Result<MergeSource> {
    repo.merge().resolve_source(spec)
}

pub fn predict_merge(
    repo: &Repository,
    into_oid: ObjectId,
    other_oid: ObjectId,
    options: &MergePredictionOptions,
) -> grit_lib::error::Result<MergePrediction> {
    repo.merge().predict(into_oid, other_oid, options)
}

pub fn plan_branch_merge(
    repo: &Repository,
    into_oid: ObjectId,
    other_oid: ObjectId,
    label: &str,
    options: &MergeOptions,
) -> grit_lib::error::Result<MergeDryRunPlan> {
    repo.merge()
        .plan_integration(into_oid, other_oid, label, options)
}

pub fn write_merge_tree(
    repo: &Repository,
    into_oid: ObjectId,
    other_oid: ObjectId,
    options: &MergePredictionOptions,
) -> grit_lib::error::Result<WrittenMergeTree> {
    repo.merge().write_tree(into_oid, other_oid, options)
}

pub fn replay_commit(
    repo: &Repository,
    req: &ReplayRequest,
    progress: &mut dyn ProgressSink,
    cancel: &CancelToken,
) -> grit_lib::error::Result<ReplayOutcome> {
    match req.direction {
        grit_lib::porcelain::replay::ReplayDirection::Pick => {
            repo.merge().pick(req.clone(), progress, cancel)
        }
        grit_lib::porcelain::replay::ReplayDirection::Revert => {
            repo.merge().revert_commit(req.clone(), progress, cancel)
        }
    }
}

pub fn start_replay_sequence(
    repo: &Repository,
    req: &ReplaySequenceRequest,
    progress: &mut dyn ProgressSink,
    cancel: &CancelToken,
) -> grit_lib::error::Result<SequenceOutcome> {
    repo.merge().start_sequence(req, progress, cancel)
}

pub fn continue_replay_sequence(
    repo: &Repository,
    identity: &Signature,
    progress: &mut dyn ProgressSink,
    cancel: &CancelToken,
) -> grit_lib::error::Result<SequenceOutcome> {
    repo.merge().continue_sequence(identity, progress, cancel)
}

pub fn predict_replay_sequence(
    repo: &Repository,
    req: &ReplaySequenceRequest,
) -> grit_lib::error::Result<SequencePrediction> {
    repo.merge().predict_sequence(req)
}

pub fn abort_replay_sequence(
    repo: &Repository,
) -> grit_lib::error::Result<grit_lib::merge::ReplayAbortOutcome> {
    repo.merge().abort_sequence()
}

pub fn skip_replay_step(
    repo: &Repository,
    identity: &Signature,
    progress: &mut dyn ProgressSink,
    cancel: &CancelToken,
) -> grit_lib::error::Result<SequenceOutcome> {
    repo.merge().skip_sequence_step(identity, progress, cancel)
}

pub fn commits_to_replay(
    repo: &Repository,
    specs: &[String],
) -> grit_lib::error::Result<Vec<ObjectId>> {
    repo.merge().commits_to_pick(specs)
}

pub fn repo_rerere(
    repo: &Repository,
    autoupdate: RerereAutoupdate,
) -> grit_lib::error::Result<Vec<RerereEvent>> {
    repo.merge().rerere().apply(autoupdate)
}

pub fn rerere_status_lines(repo: &Repository) -> grit_lib::error::Result<Vec<String>> {
    repo.merge().rerere().status_lines()
}

pub fn rerere_remaining_lines(repo: &Repository) -> grit_lib::error::Result<Vec<String>> {
    repo.merge().rerere().remaining_lines()
}

pub fn rerere_post_commit(repo: &Repository) -> grit_lib::error::Result<Vec<RerereEvent>> {
    repo.merge().rerere().post_commit()
}

pub fn rerere_diff_for_path(
    repo: &Repository,
    path: &str,
) -> grit_lib::error::Result<Option<String>> {
    repo.merge().rerere().diff_for_path(path)
}

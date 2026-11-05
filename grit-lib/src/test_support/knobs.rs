//! Test-only knobs on public options types (unstable; `test-support` feature only).

use crate::objects::ObjectId;
use crate::porcelain::add::StageOptions;
use crate::porcelain::bulk_index::IndexStagingThresholds;
use crate::porcelain::commit::CommitRequest;
use crate::transfer::FetchOptions;

/// Override bulk/parallel staging thresholds for integration tests.
#[must_use]
pub fn stage_options_staging_thresholds(
    mut opts: StageOptions,
    thresholds: IndexStagingThresholds,
) -> StageOptions {
    opts.staging_thresholds = Some(thresholds);
    opts
}

/// Skip pathspec-ignored validation on [`StageOptions`] (caller already checked ignore rules).
#[must_use]
pub fn stage_options_skip_pathspec_ignore_check(
    mut opts: StageOptions,
    skip: bool,
) -> StageOptions {
    opts.skip_pathspec_ignore_check = skip;
    opts
}

/// Skip the pre-commit index↔parent tree match on [`CommitRequest`].
#[must_use]
pub fn commit_skip_pre_commit_tree_match(mut req: CommitRequest, skip: bool) -> CommitRequest {
    req.skip_pre_commit_tree_match = skip;
    req
}

/// Spool the received pack without index-pack (transport tests and diagnostics).
#[must_use]
pub fn fetch_options_pack_spool_only(mut opts: FetchOptions, enabled: bool) -> FetchOptions {
    opts.pack_spool_only = enabled;
    opts
}

/// Promisor on-demand object fetch by oid (partial-clone handshake tests).
#[must_use]
pub fn fetch_options_promisor_object_wants(
    mut opts: FetchOptions,
    wants: Vec<ObjectId>,
) -> FetchOptions {
    opts.object_wants = wants;
    opts.promisor_object_fetch = true;
    opts
}

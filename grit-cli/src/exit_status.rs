//! Process exit codes for the `grit` binary (single mapping table).

use anyhow::Error;
use grit_lib::porcelain::hunk_select::HunkSelectError;

use crate::commands;
use crate::commands::blame::BlameUsageError;
use crate::commands::branch::BranchUsageError;
use crate::commands::bundle::MissingPrerequisites;
use crate::commands::history::RebaseExecExit;
use crate::commands::merge::{MergeOutcome, MergeResult};
use crate::commands::pick::PickResult;
use crate::commands::revert::RevertResult;
use crate::error_kind::ErrorKind;
use crate::json_error::InvalidCommitFilterExit;

/// Exit status for the grit process (maps to `i32` for `std::process::exit`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExitStatus {
    /// Success (0).
    Success,
    /// Generic command failure (1).
    Failure,
    /// Usage / invalid arguments (2).
    Usage,
    /// Clone, fetch, pull, push, or `remote refs` could not reach or read the remote (128).
    RemoteUnreadable,
    /// Bundle unbundle missing prerequisite commits (128).
    MissingPrerequisite,
    /// `merge --ff-only` refused: not fast-forward (128).
    RefusedNotFf,
    /// Invalid commit filter / pickaxe pattern (129).
    InvalidCommitFilter,
    /// Passthrough from hooks or `rebase --exec` (any code).
    Custom(i32),
}

impl ExitStatus {
    /// Unix exit code for this status.
    #[must_use]
    pub const fn code(self) -> i32 {
        match self {
            Self::Success => 0,
            Self::Failure => 1,
            Self::Usage => 2,
            Self::RemoteUnreadable | Self::MissingPrerequisite | Self::RefusedNotFf => 128,
            Self::InvalidCommitFilter => 129,
            Self::Custom(code) => code,
        }
    }

    /// Map a clap usage error code (typically 2).
    #[must_use]
    pub const fn from_clap_code(code: i32) -> Self {
        if code == 2 {
            Self::Usage
        } else if code == 0 {
            Self::Success
        } else {
            Self::Custom(code)
        }
    }

    /// Map a top-level command error after output has been emitted.
    #[must_use]
    pub fn for_error(err: &Error) -> Self {
        if err.downcast_ref::<MissingPrerequisites>().is_some() {
            return Self::MissingPrerequisite;
        }
        if err
            .chain()
            .any(|cause| cause.downcast_ref::<InvalidCommitFilterExit>().is_some())
        {
            return Self::InvalidCommitFilter;
        }
        if err
            .chain()
            .any(|cause| cause.downcast_ref::<BranchUsageError>().is_some())
        {
            return Self::Usage;
        }
        if err
            .chain()
            .any(|cause| cause.downcast_ref::<BlameUsageError>().is_some())
        {
            return Self::Usage;
        }
        if err.chain().any(|cause| {
            cause
                .downcast_ref::<commands::clone::CloneUsageError>()
                .is_some()
        }) {
            return Self::Usage;
        }
        if commands::remote::is_remote_usage_error(err) {
            return Self::Usage;
        }
        if crate::message_input::is_message_input_usage_error(err) {
            return Self::Usage;
        }
        if err
            .chain()
            .any(|cause| cause.downcast_ref::<HunkSelectError>().is_some())
        {
            return Self::Usage;
        }
        if let Some(payload) = crate::json_error::structured_json_payload(err) {
            return Self::for_structured_kind(payload.kind);
        }
        if let Some(exec) = err.downcast_ref::<RebaseExecExit>() {
            return Self::Custom(exec.code);
        }
        Self::Failure
    }

    #[must_use]
    fn for_structured_kind(kind: ErrorKind) -> Self {
        match kind {
            ErrorKind::InvalidPattern => Self::InvalidCommitFilter,
            ErrorKind::Usage => Self::Usage,
            ErrorKind::RemoteUnreadable => Self::RemoteUnreadable,
            _ => Self::Failure,
        }
    }

    /// Exit code for `grit diff --exit-code` / `--quiet` / `--no-index`.
    #[must_use]
    pub fn from_diff_outcome(outcome: &commands::diff::DiffOutcome) -> Self {
        Self::from_i32(commands::diff::exit_code(outcome))
    }

    /// Exit when diff fails while `--exit-code` or `--quiet` is set.
    #[must_use]
    pub const fn diff_command_error(wants_exit_code: bool) -> Self {
        if wants_exit_code {
            Self::from_i32(commands::diff::DIFF_ERROR_EXIT)
        } else {
            Self::Failure
        }
    }

    #[must_use]
    pub fn from_fsck(outcome: &commands::fsck::FsckOutcome) -> Self {
        Self::from_i32(commands::fsck::exit_code(outcome))
    }

    #[must_use]
    pub fn from_ignore_check(outcome: &commands::ignore::IgnoreCheckOutcome) -> Self {
        Self::from_i32(commands::ignore::exit_code(outcome))
    }

    #[must_use]
    pub fn from_merge_base(outcome: &commands::merge_base_cmd::MergeBaseOutcome) -> Self {
        Self::from_i32(commands::merge_base_cmd::exit_code(outcome))
    }

    #[must_use]
    pub fn from_hooks(outcome: &commands::hooks::HooksOutcome) -> Self {
        Self::from_i32(commands::hooks::run_exit_code(outcome))
    }

    #[must_use]
    pub fn from_tag_verify(outcome: &commands::tag::TagOutcome) -> Self {
        Self::from_i32(commands::tag::verify_exit_code(outcome))
    }

    #[must_use]
    pub(crate) const fn from_i32(code: i32) -> Self {
        match code {
            0 => Self::Success,
            1 => Self::Failure,
            2 => Self::Usage,
            128 => Self::RemoteUnreadable,
            129 => Self::InvalidCommitFilter,
            other => Self::Custom(other),
        }
    }

    #[must_use]
    pub fn from_merge_result(result: MergeResult) -> Self {
        match result {
            MergeResult::Conflicts => Self::Failure,
            MergeResult::RefusedNotFf => Self::RefusedNotFf,
            _ => Self::Success,
        }
    }

    #[must_use]
    pub fn from_merge_outcome(outcome: &MergeOutcome) -> Self {
        if outcome.dry_run == Some(true) {
            if outcome.result == MergeResult::RefusedNotFf {
                return Self::RefusedNotFf;
            }
            return Self::Success;
        }
        if outcome.check {
            if outcome.result == MergeResult::Conflicts {
                return Self::Failure;
            }
            if outcome.result == MergeResult::RefusedNotFf {
                return Self::RefusedNotFf;
            }
            return Self::Success;
        }
        Self::from_merge_result(outcome.result)
    }

    #[must_use]
    pub fn from_pick_result(result: PickResult) -> Self {
        match result {
            PickResult::Conflicts | PickResult::Empty => Self::Failure,
            _ => Self::Success,
        }
    }

    #[must_use]
    pub fn from_revert_result(result: RevertResult) -> Self {
        match result {
            RevertResult::Conflicts | RevertResult::Empty => Self::Failure,
            _ => Self::Success,
        }
    }

    #[must_use]
    pub fn from_rebase_run(outcome: &commands::history::RebaseRunOutcome) -> Self {
        use commands::history::{RebaseCheckResult, RebaseResult, RebaseRunOutcome};
        match outcome {
            RebaseRunOutcome::Rebase(r) if r.result == RebaseResult::Conflicts => Self::Failure,
            RebaseRunOutcome::Check(c) if c.result == RebaseCheckResult::Conflicts => Self::Failure,
            _ => Self::Success,
        }
    }

    #[must_use]
    pub fn from_pull(outcome: &commands::pull::PullOutcome) -> Self {
        Self::from_i32(commands::pull::pull_check_exit_code(outcome))
    }

    #[must_use]
    pub fn from_fetch(outcome: &commands::fetch::FetchRunOutcome) -> Self {
        if outcome.exit_nonzero() {
            Self::Failure
        } else {
            Self::Success
        }
    }

    #[must_use]
    pub fn from_push_rejected(rejected: bool) -> Self {
        if rejected {
            Self::Failure
        } else {
            Self::Success
        }
    }

    #[must_use]
    pub fn from_bundle_verify_ok(ok: bool) -> Self {
        if ok {
            Self::Success
        } else {
            Self::Failure
        }
    }

    #[must_use]
    pub fn from_stash_conflicts(conflicts: bool) -> Self {
        if conflicts {
            Self::Failure
        } else {
            Self::Success
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error_kind::ErrorKind;
    use crate::json_error::{
        hash_collision, incompatible_output_flags, invalid_commit_filter, invalid_pickaxe,
        operation_conflict, would_overwrite_error,
    };
    use grit_lib::command_runner::hooks::{HookError, HookOutput};
    use grit_lib::error::OverwriteReason;
    use grit_lib::objects::ObjectId;
    use grit_lib::porcelain::branch::BranchError;

    #[test]
    fn exit_status_for_every_error_kind() {
        // Typed errors built in-process; CLI triggers live in `tests/exit_status_contract.rs`.
        let oid = ObjectId::from_hex("f92d74e3874587aaf443d1db961d4e26dde13e9c").expect("oid");
        let structured: &[(ErrorKind, anyhow::Error, ExitStatus)] = &[
            (
                ErrorKind::Conflict,
                operation_conflict("merge", vec!["f.txt".into()], None),
                ExitStatus::Failure,
            ),
            (
                ErrorKind::HashCollision,
                hash_collision(oid),
                ExitStatus::Failure,
            ),
            (
                ErrorKind::WouldOverwrite,
                would_overwrite_error(
                    OverwriteReason::LocalChanges,
                    vec!["f.txt".into()],
                    "would overwrite".into(),
                ),
                ExitStatus::Failure,
            ),
            (
                ErrorKind::IncompatibleFlags,
                incompatible_output_flags("bad combo", ErrorKind::IncompatibleFlags),
                ExitStatus::Failure,
            ),
            (
                ErrorKind::HookFailed,
                anyhow::Error::new(grit_lib::error::Error::Hook(HookError::Failed {
                    hook_name: "pre-commit".into(),
                    status: 1,
                    output: HookOutput {
                        stdout: b"out\n".to_vec(),
                        stderr: b"err\n".to_vec(),
                    },
                })),
                ExitStatus::Failure,
            ),
        ];
        for (kind, err, expected) in structured {
            let payload =
                crate::json_error::structured_json_payload(&err).expect("structured payload");
            assert_eq!(payload.kind, *kind);
            assert_eq!(ExitStatus::for_error(&err), *expected);
        }

        let filter_err = grit_lib::revwalk::filter::CommitFilterError::InvalidPattern {
            pattern: "[".into(),
            detail: "unclosed".into(),
        };
        let err = invalid_commit_filter(filter_err);
        assert_eq!(ExitStatus::for_error(&err), ExitStatus::InvalidCommitFilter);

        let unreadable = || {
            grit_lib::error::Error::remote_unreadable(
                "/nope.git",
                grit_lib::error::RemoteUnreadableReason::RepositoryNotFound,
            )
        };
        for err in [
            anyhow::Error::new(unreadable()),
            commands::fetch::map_remote_error(grit_lib::remote::RemoteError::Library(unreadable())),
            anyhow::Error::new(grit_lib::remote::clone::CloneError::Library(unreadable())),
            anyhow::Error::new(grit_lib::porcelain::pull::PullError::Remote(
                grit_lib::remote::RemoteError::Library(unreadable()),
            )),
        ] {
            let payload = crate::json_error::structured_json_payload(&err).expect("payload");
            assert_eq!(payload.kind, ErrorKind::RemoteUnreadable);
            assert_eq!(payload.error, "could not find repository at '/nope.git'");
            assert_eq!(ExitStatus::for_error(&err), ExitStatus::RemoteUnreadable);
        }
        let auth = commands::fetch::map_remote_error(grit_lib::remote::RemoteError::Library(
            grit_lib::error::Error::Auth {
                url: "https://example.com/r.git".into(),
                reason: grit_lib::error::AuthFailure::Rejected,
            },
        ));
        assert_eq!(ExitStatus::for_error(&auth), ExitStatus::Failure);

        let pickaxe_err = grit_lib::diff::PickaxeError::InvalidPattern {
            pattern: "x".into(),
            detail: "bad".into(),
        };
        let err = invalid_pickaxe(pickaxe_err);
        assert_eq!(ExitStatus::for_error(&err), ExitStatus::InvalidCommitFilter);

        for branch_err in [
            BranchError::NotFullyMerged {
                name: "x".into(),
                base: "main".into(),
            },
            BranchError::CheckedOut {
                name: "main".into(),
                worktree: "/wt".into(),
            },
            BranchError::NoCommits,
            BranchError::InvalidStartPoint {
                label: "nope".into(),
                reason: "missing".into(),
            },
            BranchError::UpstreamNotTracking {
                upstream: "u".into(),
            },
            BranchError::NoUpstream { name: "b".into() },
        ] {
            let mapped =
                commands::branch::map_branch_error(grit_lib::error::Error::Branch(branch_err));
            let payload = crate::json_error::structured_json_payload(&mapped).expect("payload");
            assert!(ErrorKind::all().contains(&payload.kind));
            assert_eq!(ExitStatus::for_error(&mapped), ExitStatus::Failure);
        }
    }
}

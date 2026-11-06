//! Pinned [`super::classify`] expectations (kind, exit, human, hint).

use std::path::Path;

use anyhow::Error;
use grit_lib::command_runner::hooks::{HookError, HookOutput};
use grit_lib::error::{AuthFailure, Error as LibError, NotARepositoryReason, OverwriteReason};
use grit_lib::error::{RemoteUnreadableReason, WorktreeOperation};
use grit_lib::objects::ObjectId;
use grit_lib::porcelain::branch::BranchError;
use grit_lib::porcelain::merge::ContinueMergeError;
use grit_lib::refs::store::RefStoreError;
use grit_lib::remote::push_plan::PushPlanError;
use grit_lib::remote::RemoteError;

use crate::error_kind::ErrorKind;
use crate::error_map::{classify, human_message_for_lib, map_branch_error, map_remote_error};
use crate::exit_status::ExitStatus;
use crate::json_error::{
    gitbutler_workspace_commit, incompatible_output_flags, invalid_commit_filter, invalid_pickaxe,
    lock_held, map_lib_error, no_operation_in_progress, operation_in_progress, replay_conflict,
    revision_not_found, would_overwrite_error, StructuredCliError, StructuredJsonError,
};
use grit_lib::error::{GitDirPathOperation, WorktreePathOperation};
use grit_lib::lockfile::LockHolder;
use grit_lib::porcelain::pull::PullError;
use grit_lib::porcelain::tag::TagError;
use grit_lib::remote::clone::CloneError;
use grit_lib::revwalk::history::RebaseSessionError;
use grit_lib::revwalk::rev_parse_error::RevParseError;
use grit_lib::state::InProgressOperation;

struct ContractRow {
    label: &'static str,
    build: fn() -> Error,
    kind: Option<ErrorKind>,
    exit: ExitStatus,
    human: fn() -> String,
    hint: Option<&'static str>,
}

fn oid() -> ObjectId {
    ObjectId::from_hex("f92d74e3874587aaf443d1db961d4e26dde13e9c").expect("oid")
}

fn structured(kind: ErrorKind, human: &str, hint: Option<&str>) -> Error {
    Error::new(StructuredCliError {
        payload: StructuredJsonError {
            hint: hint.map(str::to_owned),
            ..StructuredJsonError::new(human, kind)
        },
        human: human.to_owned(),
    })
}

fn human_ref_branch_missing() -> String {
    human_message_for_lib(&LibError::RefStore(RefStoreError::NotFound {
        name: "refs/heads/missing".into(),
    }))
}

static CONTRACT: &[ContractRow] = &[
    ContractRow {
        label: "ref_store_branch_not_found",
        build: || Error::new(LibError::RefStore(RefStoreError::NotFound {
            name: "refs/heads/missing".into(),
        })),
        kind: None,
        exit: ExitStatus::Failure,
        human: human_ref_branch_missing,
        hint: None,
    },
    ContractRow {
        label: "no_remote",
        build: || Error::new(PushPlanError::NoRemote),
        kind: Some(ErrorKind::NoRemote),
        exit: ExitStatus::Failure,
        human: || PushPlanError::NoRemote.to_string(),
        hint: Some("add one with `grit remote add origin <url>`, or push to a URL with `grit push <url> <branch>`"),
    },
    ContractRow {
        label: "remote_not_found",
        build: || Error::new(RemoteError::UnknownRemote("up".into())),
        kind: Some(ErrorKind::RemoteNotFound),
        exit: ExitStatus::Failure,
        human: || "unknown remote 'up'".to_owned(),
        hint: Some("add it with `grit remote add up <url>`, or pass a URL or path instead; `grit remote` lists the configured remotes"),
    },
    ContractRow {
        label: "remote_no_url",
        build: || Error::new(RemoteError::NoUrl {
            remote: Some("half".into()),
        }),
        kind: Some(ErrorKind::RemoteNoUrl),
        exit: ExitStatus::Failure,
        human: || RemoteError::NoUrl {
            remote: Some("half".into()),
        }
        .to_string(),
        hint: Some("set it with `grit remote set-url half <url>`"),
    },
    ContractRow {
        label: "branch_no_upstream",
        build: || Error::new(PullError::NoUpstream {
            branch: "main".into(),
        }),
        kind: Some(ErrorKind::BranchNoUpstream),
        exit: ExitStatus::Failure,
        human: || PullError::NoUpstream {
            branch: "main".into(),
        }
        .to_string(),
        hint: Some("pull a branch by name with `grit pull origin main`, or publish this one and track it with `grit push -u origin main`"),
    },
    ContractRow {
        label: "no_operation_in_progress",
        build: || Error::new(LibError::NoMergeInProgress),
        kind: Some(ErrorKind::NoOperationInProgress),
        exit: ExitStatus::Failure,
        human: || LibError::NoMergeInProgress.to_string(),
        hint: None,
    },
    ContractRow {
        label: "revision_not_found",
        build: || revision_not_found("missing"),
        kind: Some(ErrorKind::RevisionNotFound),
        exit: ExitStatus::Failure,
        human: || "no branch, tag or commit named 'missing'".to_owned(),
        hint: None,
    },
    ContractRow {
        label: "hash_collision",
        build: || map_lib_error(LibError::HashCollision { oid: oid() }),
        kind: Some(ErrorKind::HashCollision),
        exit: ExitStatus::Failure,
        human: || format!(
            "refusing to store content that triggers an SHA-1 hash collision (digest {})",
            oid().to_hex()
        ),
        hint: None,
    },
    ContractRow {
        label: "remote_unreadable",
        build: || Error::new(LibError::remote_unreadable(
            "/nope.git",
            RemoteUnreadableReason::RepositoryNotFound,
        )),
        kind: Some(ErrorKind::RemoteUnreadable),
        exit: ExitStatus::RemoteUnreadable,
        human: || "could not find repository at '/nope.git'".to_owned(),
        hint: Some("check the URL, and that you have access to the repository"),
    },
    ContractRow {
        label: "branch_not_found",
        build: || map_branch_error(LibError::Branch(BranchError::NotFound {
            name: "x".into(),
        })),
        kind: Some(ErrorKind::BranchNotFound),
        exit: ExitStatus::Failure,
        human: || "no branch named 'x'".to_owned(),
        hint: None,
    },
    ContractRow {
        label: "conflict",
        build: || {
            replay_conflict(
                "rebase",
                oid(),
                "subject",
                vec!["f.txt".into()],
                Vec::new(),
            )
        },
        kind: Some(ErrorKind::Conflict),
        exit: ExitStatus::Failure,
        human: || {
            "rebase has conflicts in:\n  commit f92d74e subject\n  f.txt\n\nNothing was changed. grit can't resolve conflicts yet.".to_owned()
        },
        hint: None,
    },
    ContractRow {
        label: "hook_failed",
        build: || {
            Error::new(LibError::Hook(HookError::Failed {
                hook_name: "pre-commit".into(),
                status: 1,
                output: HookOutput {
                    stdout: b"out\n".to_vec(),
                    stderr: b"err\n".to_vec(),
                },
            }))
        },
        kind: Some(ErrorKind::HookFailed),
        exit: ExitStatus::Failure,
        human: || "hook pre-commit failed with exit status 1".to_owned(),
        hint: None,
    },
    ContractRow {
        label: "lock_held",
        build: || {
            lock_held(
                Path::new("/repo/.git/index.lock"),
                Some(LockHolder {
                    pid: 42,
                    running: false,
                }),
            )
        },
        kind: Some(ErrorKind::LockHeld),
        exit: ExitStatus::Failure,
        human: || {
            lock_held(
                Path::new("/repo/.git/index.lock"),
                Some(LockHolder {
                    pid: 42,
                    running: false,
                }),
            )
            .to_string()
        },
        hint: None,
    },
    ContractRow {
        label: "would_overwrite",
        build: || {
            would_overwrite_error(
                OverwriteReason::LocalChanges,
                vec!["f.txt".into()],
                "local changes would be overwritten by checkout".into(),
            )
        },
        kind: Some(ErrorKind::WouldOverwrite),
        exit: ExitStatus::Failure,
        human: || {
            format!(
                "local changes would be overwritten by checkout{}",
                crate::error_map::indented_paths(&["f.txt".to_owned()])
            )
        },
        hint: Some("commit them with `grit commit`, or set them aside with `grit stash`, then try again"),
    },
    ContractRow {
        label: "unmerged_paths",
        build: || {
            Error::new(ContinueMergeError::UnmergedPaths {
                paths: vec!["f.txt".into()],
            })
        },
        kind: Some(ErrorKind::UnmergedPaths),
        exit: ExitStatus::Failure,
        human: || "you still have unmerged paths: f.txt".to_owned(),
        hint: Some("resolve the conflict markers in f.txt, then run `grit merge --continue` (or `grit merge --abort` to start over)"),
    },
    ContractRow {
        label: "dirty_worktree",
        build: || {
            Error::new(LibError::dirty_worktree(
                WorktreeOperation::Switch,
                vec!["a".into(), "b".into()],
            ))
        },
        kind: Some(ErrorKind::DirtyWorktree),
        exit: ExitStatus::Failure,
        human: || format!(
            "{}{}",
            LibError::dirty_worktree(WorktreeOperation::Switch, vec!["a".into(), "b".into()]),
            crate::error_map::indented_paths(&["a".to_owned(), "b".to_owned()])
        ),
        hint: Some("commit them with `grit commit`, or set them aside with `grit stash`, then try again"),
    },
    ContractRow {
        label: "not_a_repository",
        build: || {
            Error::new(LibError::NotARepository(NotARepositoryReason::NoRepositoryAt {
                path: "/tmp".into(),
            }))
        },
        kind: Some(ErrorKind::NotARepository),
        exit: ExitStatus::Failure,
        human: || {
            LibError::NotARepository(NotARepositoryReason::NoRepositoryAt {
                path: "/tmp".into(),
            })
            .to_string()
        },
        hint: None,
    },
    ContractRow {
        label: "pathspec_no_match",
        build: || {
            Error::new(LibError::PathspecNoMatch {
                spec: "nosuch".into(),
            })
        },
        kind: Some(ErrorKind::PathspecNoMatch),
        exit: ExitStatus::Failure,
        human: || {
            LibError::PathspecNoMatch {
                spec: "nosuch".into(),
            }
            .to_string()
        },
        hint: None,
    },
    ContractRow {
        label: "stash_not_found",
        build: || Error::new(LibError::StashNotFound { n: 0 }),
        kind: Some(ErrorKind::StashNotFound),
        exit: ExitStatus::Failure,
        human: || "no stash entries".to_owned(),
        hint: Some("save local changes with `grit stash push`"),
    },
    ContractRow {
        label: "auth_required_github",
        build: || {
            Error::new(LibError::Auth {
                url: "https://github.com/o/r.git".into(),
                reason: AuthFailure::CredentialsUnavailable {
                    detail: "none".into(),
                    helpers: Vec::new(),
                },
            })
        },
        kind: Some(ErrorKind::AuthRequired),
        exit: ExitStatus::Failure,
        human: || {
            LibError::Auth {
                url: "https://github.com/o/r.git".into(),
                reason: AuthFailure::CredentialsUnavailable {
                    detail: "none".into(),
                    helpers: Vec::new(),
                },
            }
            .to_string()
        },
        hint: Some("sign in to GitHub with `grit auth` and try again; if it still fails, check the URL"),
    },
    ContractRow {
        label: "invalid_pattern",
        build: || {
            invalid_commit_filter(grit_lib::revwalk::filter::CommitFilterError::InvalidPattern {
                pattern: "[".into(),
                detail: "unclosed".into(),
            })
        },
        kind: Some(ErrorKind::InvalidPattern),
        exit: ExitStatus::InvalidCommitFilter,
        human: || {
            grit_lib::revwalk::filter::CommitFilterError::InvalidPattern {
                pattern: "[".into(),
                detail: "unclosed".into(),
            }
            .to_string()
        },
        hint: None,
    },
    ContractRow {
        label: "incompatible_flags",
        build: || incompatible_output_flags("bad combo", ErrorKind::IncompatibleFlags),
        kind: Some(ErrorKind::IncompatibleFlags),
        exit: ExitStatus::Failure,
        human: || "bad combo".to_owned(),
        hint: None,
    },
    ContractRow {
        label: "operation_in_progress",
        build: || operation_in_progress("commit", InProgressOperation::Merge, None),
        kind: Some(ErrorKind::OperationInProgress),
        exit: ExitStatus::Failure,
        human: || "cannot commit while a merge is in progress".to_owned(),
        hint: None,
    },
    ContractRow {
        label: "gitbutler_workspace",
        build: gitbutler_workspace_commit,
        kind: Some(ErrorKind::GitButlerWorkspace),
        exit: ExitStatus::Failure,
        human: || {
            "HEAD is a GitButler workspace, which GitButler rewrites, so grit does not commit to it"
                .to_owned()
        },
        hint: Some("commit with `but commit`, or switch to a branch first"),
    },
    ContractRow {
        label: "no_operation_in_progress_explicit",
        build: || no_operation_in_progress("nothing to abort"),
        kind: Some(ErrorKind::NoOperationInProgress),
        exit: ExitStatus::Failure,
        human: || "nothing to abort".to_owned(),
        hint: None,
    },
    ContractRow {
        label: "branch_already_exists",
        build: || {
            map_branch_error(LibError::Branch(BranchError::AlreadyExists {
                name: "main".into(),
            }))
        },
        kind: Some(ErrorKind::BranchAlreadyExists),
        exit: ExitStatus::Failure,
        human: || "branch 'main' already exists (use --force to replace its tip)".to_owned(),
        hint: None,
    },
    ContractRow {
        label: "branch_no_commits",
        build: || map_branch_error(LibError::Branch(BranchError::NoCommits)),
        kind: Some(ErrorKind::BranchNoCommits),
        exit: ExitStatus::Failure,
        human: || BranchError::NoCommits.to_string(),
        hint: None,
    },
    ContractRow {
        label: "empty_commit_message",
        build: || Error::new(LibError::EmptyCommitMessage),
        kind: Some(ErrorKind::EmptyCommitMessage),
        exit: ExitStatus::Failure,
        human: || LibError::EmptyCommitMessage.to_string(),
        hint: None,
    },
    ContractRow {
        label: "remote_library_object_not_found",
        build: || {
            map_remote_error(RemoteError::Library(LibError::ObjectNotFound(
                "deadbeef".into(),
            )))
        },
        kind: None,
        exit: ExitStatus::Failure,
        human: || "object not found: deadbeef".to_owned(),
        hint: None,
    },
    ContractRow {
        label: "pickaxe_invalid",
        build: || {
            invalid_pickaxe(grit_lib::diff::PickaxeError::InvalidPattern {
                pattern: "x".into(),
                detail: "bad".into(),
            })
        },
        kind: Some(ErrorKind::InvalidPattern),
        exit: ExitStatus::InvalidCommitFilter,
        human: || {
            grit_lib::diff::PickaxeError::InvalidPattern {
                pattern: "x".into(),
                detail: "bad".into(),
            }
            .to_string()
        },
        hint: None,
    },
    ContractRow {
        label: "rebase_unmerged",
        build: || Error::new(RebaseSessionError::UnmergedPaths),
        kind: Some(ErrorKind::UnmergedPaths),
        exit: ExitStatus::Failure,
        human: || RebaseSessionError::UnmergedPaths.to_string(),
        hint: Some("resolve conflicts and stage with `grit add`, then run `grit rebase --continue` (or `grit rebase --abort` to start over)"),
    },
    ContractRow {
        label: "usage",
        build: || structured(ErrorKind::Usage, "usage error", None),
        kind: Some(ErrorKind::Usage),
        exit: ExitStatus::Usage,
        human: || "usage error".to_owned(),
        hint: None,
    },
    ContractRow {
        label: "commit_message_required",
        build: || {
            structured(
                ErrorKind::CommitMessageRequired,
                "provide a commit message, e.g. grit commit \"what changed\"",
                None,
            )
        },
        kind: Some(ErrorKind::CommitMessageRequired),
        exit: ExitStatus::Failure,
        human: || "provide a commit message, e.g. grit commit \"what changed\"".to_owned(),
        hint: None,
    },
    ContractRow {
        label: "identity_unknown",
        build: || {
            structured(
                ErrorKind::IdentityUnknown,
                "author identity unknown\n\nTell grit who you are:\n  grit config --global user.name \"Your Name\"\n  grit config --global user.email \"you@example.com\"",
                Some("Tell grit who you are:\n  grit config --global user.name \"Your Name\"\n  grit config --global user.email \"you@example.com\""),
            )
        },
        kind: Some(ErrorKind::IdentityUnknown),
        exit: ExitStatus::Failure,
        human: || "author identity unknown\n\nTell grit who you are:\n  grit config --global user.name \"Your Name\"\n  grit config --global user.email \"you@example.com\"".to_owned(),
        hint: Some("Tell grit who you are:\n  grit config --global user.name \"Your Name\"\n  grit config --global user.email \"you@example.com\""),
    },
    ContractRow {
        label: "tag_already_exists",
        build: || {
            Error::new(LibError::Tag(TagError::AlreadyExists {
                name: "v1".into(),
            }))
        },
        kind: Some(ErrorKind::TagAlreadyExists),
        exit: ExitStatus::Failure,
        human: || "tag 'v1' already exists (use --force to replace)".to_owned(),
        hint: None,
    },
    ContractRow {
        label: "tag_not_found",
        build: || Error::new(LibError::Tag(TagError::NotFound { name: "v1".into() })),
        kind: Some(ErrorKind::TagNotFound),
        exit: ExitStatus::Failure,
        human: || "no tag named 'v1'".to_owned(),
        hint: None,
    },
    ContractRow {
        label: "path_not_found",
        build: || {
            Error::new(LibError::RevParse(RevParseError::PathNotInRevision {
                path: "missing.txt".into(),
                revision: "HEAD".into(),
            }))
        },
        kind: Some(ErrorKind::PathNotFound),
        exit: ExitStatus::Failure,
        human: || "path 'missing.txt' does not exist in 'HEAD'".to_owned(),
        hint: None,
    },
    ContractRow {
        label: "embedded_repo_without_commit",
        build: || {
            Error::new(LibError::EmbeddedRepoWithoutCommit {
                path: "sub".into(),
            })
        },
        kind: Some(ErrorKind::EmbeddedRepoWithoutCommit),
        exit: ExitStatus::Failure,
        human: || "embedded git repository 'sub' has no commit checked out".to_owned(),
        hint: None,
    },
    ContractRow {
        label: "worktree_path_io",
        build: || {
            Error::new(LibError::WorktreePathIo {
                path: "a.txt".into(),
                operation: WorktreePathOperation::Replace,
                source: std::io::Error::from_raw_os_error(16),
            })
        },
        kind: Some(ErrorKind::WorktreePathIo),
        exit: ExitStatus::Failure,
        human: || LibError::WorktreePathIo {
            path: "a.txt".into(),
            operation: WorktreePathOperation::Replace,
            source: std::io::Error::from_raw_os_error(16),
        }
        .to_string(),
        hint: Some("close programs that may have the file open, then try again"),
    },
    ContractRow {
        label: "checkout_incomplete",
        build: || {
            Error::new(LibError::CheckoutRollbackIncomplete {
                source: Box::new(LibError::Cancelled),
                unrestored: vec!["x".into()],
            })
        },
        kind: Some(ErrorKind::CheckoutIncomplete),
        exit: ExitStatus::Failure,
        human: || format!(
            "{}{}",
            LibError::CheckoutRollbackIncomplete {
                source: Box::new(LibError::Cancelled),
                unrestored: vec!["x".into()],
            },
            crate::error_map::indented_paths(&["x".to_owned()])
        ),
        hint: Some("HEAD and the index did not move; `grit status` shows the listed paths, and `grit reset --hard` plus `grit clean -f` put them back"),
    },
    ContractRow {
        label: "git_dir_path_io",
        build: || {
            Error::new(LibError::GitDirPathIo {
                path: ".git/index".into(),
                operation: GitDirPathOperation::WriteIndex,
                source: std::io::Error::from_raw_os_error(16),
            })
        },
        kind: Some(ErrorKind::GitDirPathIo),
        exit: ExitStatus::Failure,
        human: || LibError::GitDirPathIo {
            path: ".git/index".into(),
            operation: GitDirPathOperation::WriteIndex,
            source: std::io::Error::from_raw_os_error(16),
        }
        .to_string(),
        hint: Some("another thread or process is updating the same git metadata; retry shortly"),
    },
    ContractRow {
        label: "nothing_staged",
        build: || {
            structured(
                ErrorKind::NothingStaged,
                "nothing staged to commit (1 file has unstaged or untracked changes)",
                Some("run `grit add <file>` first, or `grit commit -- <path>`"),
            )
        },
        kind: Some(ErrorKind::NothingStaged),
        exit: ExitStatus::Failure,
        human: || "nothing staged to commit (1 file has unstaged or untracked changes)".to_owned(),
        hint: Some("run `grit add <file>` first, or `grit commit -- <path>`"),
    },
    ContractRow {
        label: "nothing_to_commit",
        build: || structured(ErrorKind::NothingToCommit, "nothing to commit — working tree clean", None),
        kind: Some(ErrorKind::NothingToCommit),
        exit: ExitStatus::Failure,
        human: || "nothing to commit — working tree clean".to_owned(),
        hint: None,
    },
    ContractRow {
        label: "branch_invalid_name",
        build: || {
            map_branch_error(LibError::Branch(BranchError::InvalidName {
                name: "bad name".into(),
                reason: "contains a space".into(),
            }))
        },
        kind: Some(ErrorKind::BranchInvalidName),
        exit: ExitStatus::Failure,
        human: || {
            BranchError::InvalidName {
                name: "bad name".into(),
                reason: "contains a space".into(),
            }
            .to_string()
        },
        hint: None,
    },
    ContractRow {
        label: "branch_not_fully_merged",
        build: || {
            map_branch_error(LibError::Branch(BranchError::NotFullyMerged {
                name: "feat".into(),
                base: "main".into(),
            }))
        },
        kind: Some(ErrorKind::BranchNotFullyMerged),
        exit: ExitStatus::Failure,
        human: || {
            format!(
                "{}\nIf you are sure you want to delete it, run 'grit branch --force-delete feat'",
                BranchError::NotFullyMerged {
                    name: "feat".into(),
                    base: "main".into(),
                }
            )
        },
        hint: None,
    },
    ContractRow {
        label: "branch_checked_out",
        build: || {
            map_branch_error(LibError::BranchCheckedOutInWorktree {
                branch: "main".into(),
                worktree_path: "/wt".into(),
            })
        },
        kind: Some(ErrorKind::BranchCheckedOut),
        exit: ExitStatus::Failure,
        human: || "branch is checked out in '/wt'".to_owned(),
        hint: None,
    },
    ContractRow {
        label: "no_previous_branch",
        build: || map_branch_error(LibError::Branch(BranchError::NoPreviousBranch)),
        kind: Some(ErrorKind::NoPreviousBranch),
        exit: ExitStatus::Failure,
        human: || BranchError::NoPreviousBranch.to_string(),
        hint: None,
    },
    ContractRow {
        label: "branch_invalid_start",
        build: || {
            map_branch_error(LibError::Branch(BranchError::InvalidStartPoint {
                label: "nope".into(),
                reason: "missing".into(),
            }))
        },
        kind: Some(ErrorKind::BranchInvalidStartPoint),
        exit: ExitStatus::Failure,
        human: || {
            BranchError::InvalidStartPoint {
                label: "nope".into(),
                reason: "missing".into(),
            }
            .to_string()
        },
        hint: None,
    },
    ContractRow {
        label: "branch_upstream_not_tracking",
        build: || {
            map_branch_error(LibError::Branch(BranchError::UpstreamNotTracking {
                upstream: "origin/main".into(),
            }))
        },
        kind: Some(ErrorKind::BranchUpstreamNotTracking),
        exit: ExitStatus::Failure,
        human: || {
            BranchError::UpstreamNotTracking {
                upstream: "origin/main".into(),
            }
            .to_string()
        },
        hint: None,
    },
    ContractRow {
        label: "branch_error_move",
        build: || {
            Error::new(LibError::MoveDestinationExists {
                destination: "dest".into(),
                src: "src".into(),
            })
        },
        kind: Some(ErrorKind::BranchError),
        exit: ExitStatus::Failure,
        human: || LibError::MoveDestinationExists {
            destination: "dest".into(),
            src: "src".into(),
        }
        .to_string(),
        hint: Some("remove or rename 'dest' first, or run `grit mv --force` to replace it"),
    },
    ContractRow {
        label: "auth_rejected",
        build: || {
            Error::new(LibError::Auth {
                url: "https://git.example.com/o/r.git".into(),
                reason: AuthFailure::Rejected,
            })
        },
        kind: Some(ErrorKind::AuthRejected),
        exit: ExitStatus::Failure,
        human: || {
            LibError::Auth {
                url: "https://git.example.com/o/r.git".into(),
                reason: AuthFailure::Rejected,
            }
            .to_string()
        },
        hint: Some("update the credentials your credential helper stores for this host (for example with `grit auth https://git.example.com/o/r.git --username <user> --token <token>`), then try again"),
    },
    ContractRow {
        label: "clone_checkout_failed",
        build: || {
            Error::new(CloneError::CheckoutFailed {
                path: "/tmp/r".into(),
                commit: oid(),
                source: Box::new(LibError::Message("checkout failed".into())),
            })
        },
        kind: Some(ErrorKind::CloneCheckoutFailed),
        exit: ExitStatus::Failure,
        human: || "the clone into '/tmp/r' succeeded, but its checkout failed: checkout failed".to_owned(),
        hint: Some("the repository was kept; run `grit reset --hard` in '/tmp/r' to finish the checkout"),
    },
    ContractRow {
        label: "clone_remote_unreadable_wrapped",
        build: || {
            Error::new(CloneError::Remote(RemoteError::Library(
                LibError::remote_unreadable(
                    "https://example.com/x.git",
                    RemoteUnreadableReason::HttpStatus { status: 404 },
                ),
            )))
            .context("clone failed")
        },
        kind: Some(ErrorKind::RemoteUnreadable),
        exit: ExitStatus::RemoteUnreadable,
        human: || "HTTP 404 from https://example.com/x.git".to_owned(),
        hint: Some("check the URL, and that you have access to the repository"),
    },
];

pub fn assert_error_map_contract() {
    let required_kinds = ErrorKind::all();
    for row in CONTRACT {
        let err = (row.build)();
        let c = classify(&err);
        assert_eq!(c.kind, row.kind, "kind mismatch for {}", row.label);
        assert_eq!(c.exit, row.exit, "exit mismatch for {}", row.label);
        assert_eq!(c.human, (row.human)(), "human mismatch for {}", row.label);
        assert_eq!(
            c.hint.as_deref(),
            row.hint,
            "hint mismatch for {}",
            row.label
        );
        if let Some(kind) = row.kind {
            let payload = c.payload.as_ref().expect("payload");
            assert_eq!(payload.kind, kind, "payload kind for {}", row.label);
            if row.hint.is_some() {
                assert_eq!(payload.hint.as_deref(), row.hint);
            }
        }
    }
    for kind in required_kinds {
        assert!(
            CONTRACT.iter().any(|r| r.kind == Some(*kind)),
            "CONTRACT missing ErrorKind::{kind:?}"
        );
    }
}

struct MatchBudget {
    path: &'static str,
    max: usize,
}

/// Per-file caps on `LibError::` / `grit_lib::error::Error::` matches outside `error_map.rs`.
/// Lower a file's `max` only after removing matches (ratchet — never increase).
static MATCH_BUDGETS: &[MatchBudget] = &[
    MatchBudget {
        path: "src/revision_errors.rs",
        max: 11,
    },
    MatchBudget {
        path: "src/exit_status.rs",
        max: 9,
    },
    MatchBudget {
        path: "src/json_error.rs",
        max: 5,
    },
    MatchBudget {
        path: "src/commands/add.rs",
        max: 4,
    },
    MatchBudget {
        path: "src/context.rs",
        max: 3,
    },
    MatchBudget {
        path: "src/commands/pull.rs",
        max: 3,
    },
    MatchBudget {
        path: "src/commands/config.rs",
        max: 3,
    },
    MatchBudget {
        path: "src/cli_messages.rs",
        max: 2,
    },
    MatchBudget {
        path: "src/unborn_errors.rs",
        max: 2,
    },
    MatchBudget {
        path: "src/commands/fetch.rs",
        max: 2,
    },
    MatchBudget {
        path: "src/commands/history.rs",
        max: 2,
    },
    MatchBudget {
        path: "src/commands/rm.rs",
        max: 1,
    },
    MatchBudget {
        path: "src/commands/switch.rs",
        max: 1,
    },
    MatchBudget {
        path: "src/commands/auth.rs",
        max: 1,
    },
    MatchBudget {
        path: "src/commands/log.rs",
        max: 1,
    },
    MatchBudget {
        path: "src/commands/stash.rs",
        max: 1,
    },
    MatchBudget {
        path: "src/commands/bundle.rs",
        max: 1,
    },
    MatchBudget {
        path: "src/commands/serve.rs",
        max: 1,
    },
    MatchBudget {
        path: "src/commands/checkin.rs",
        max: 1,
    },
    MatchBudget {
        path: "src/commands/resolve.rs",
        max: 1,
    },
    MatchBudget {
        path: "src/commands/diff.rs",
        max: 1,
    },
    MatchBudget {
        path: "src/commands/restore.rs",
        max: 1,
    },
    MatchBudget {
        path: "src/commands/cat.rs",
        max: 1,
    },
];

const LIB_ERROR_MATCH_PREFIXES: &[&str] = &["LibError::", "grit_lib::error::Error::"];

fn lib_error_match_count(text: &str) -> usize {
    LIB_ERROR_MATCH_PREFIXES
        .iter()
        .map(|prefix| text.matches(prefix).count())
        .sum()
}

fn visit_rs_sources(
    manifest: &Path,
    dir: &Path,
    budgets: &std::collections::HashMap<&'static str, usize>,
    total: &mut usize,
) {
    for entry in std::fs::read_dir(dir).unwrap_or_else(|e| panic!("read {}: {e}", dir.display())) {
        let entry = entry.unwrap_or_else(|e| panic!("dir entry: {e}"));
        let path = entry.path();
        if path.is_dir() {
            visit_rs_sources(manifest, &path, budgets, total);
            continue;
        }
        if path.extension().is_none_or(|ext| ext != "rs") {
            continue;
        }
        let rel = path
            .strip_prefix(manifest)
            .unwrap_or(&path)
            .to_string_lossy()
            .replace('\\', "/");
        if rel == "src/error_map.rs" || rel == "src/error_map_contract.rs" {
            continue;
        }
        let text = std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {rel}: {e}"));
        let count = lib_error_match_count(&text);
        if count == 0 {
            continue;
        }
        let max = *budgets.get(rel.as_str()).unwrap_or_else(|| {
            panic!(
                "{rel} has {count} LibError matches but no MATCH_BUDGETS entry (add one or centralize in error_map.rs)"
            )
        });
        assert!(
            count <= max,
            "{rel} has {count} LibError matches (max {max})"
        );
        *total += count;
    }
}

pub fn assert_lib_error_match_ratchet() {
    let manifest = Path::new(env!("CARGO_MANIFEST_DIR"));
    let budgets: std::collections::HashMap<&'static str, usize> = MATCH_BUDGETS
        .iter()
        .map(|entry| (entry.path, entry.max))
        .collect();
    let mut total = 0usize;
    visit_rs_sources(manifest, &manifest.join("src"), &budgets, &mut total);
    const TOTAL_MATCHES_OUTSIDE_ERROR_MAP: usize = 58;
    assert_eq!(
        total, TOTAL_MATCHES_OUTSIDE_ERROR_MAP,
        "update TOTAL_MATCHES_OUTSIDE_ERROR_MAP downward after removing matches (now {total})"
    );
}

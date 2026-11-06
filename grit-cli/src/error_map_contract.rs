//! Pinned [`super::classify`] expectations (kind, exit, human, hint).

#![allow(deprecated)]

use std::path::Path;

use anyhow::Error;
use grit_lib::command_runner::hooks::{HookError, HookOutput};
use grit_lib::error::{
    AuthFailure, ConfigError, Error as LibError, LockHeldError, NotARepositoryReason,
    OverwriteReason,
};
use grit_lib::error::{RemoteUnreadableReason, WorktreeOperation};
use grit_lib::objects::ObjectId;
use grit_lib::porcelain::branch::BranchError;
use grit_lib::porcelain::merge::ContinueMergeError;
use grit_lib::refs::store::RefStoreError;
use grit_lib::remote::push_plan::PushPlanError;
use grit_lib::remote::RemoteError;
use grit_lib::repo::RepositoryError;

use crate::error_kind::ErrorKind;
use crate::error_map::{
    classify, human_message_for_lib, identity_error, map_branch_error, map_remote_error,
    map_stage_lib_error, pathspec_ignored_human, stage_conflict_markers_human,
};
use crate::exit_status::ExitStatus;
use crate::json_error::{
    gitbutler_workspace_commit, incompatible_output_flags, invalid_commit_filter, invalid_pickaxe,
    map_lib_error, no_operation_in_progress, replay_conflict, revision_not_found,
    StructuredCliError, StructuredJsonError,
};
use grit_lib::config::ident_resolve::{IdentRole, IdentityError};
use grit_lib::error::{GitDirPathOperation, WorktreePathOperation};
use grit_lib::lockfile::LockHolder;
use grit_lib::path_validate::PathComponentError;
use grit_lib::porcelain::paths::{BlockedRemoval, RemoveBlockReason};
use grit_lib::porcelain::pull::PullError;
use grit_lib::porcelain::tag::TagError;
use grit_lib::remote::clone::CloneError;
use grit_lib::revwalk::history::RebaseSessionError;
use grit_lib::revwalk::rev_parse_error::{AmbiguousChangeIdHint, RevParseError};
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
            Error::new(LibError::LockHeld(LockHeldError {
                path: Path::new("/repo/.git/index.lock").into(),
                holder: Some(LockHolder {
                    pid: 42,
                    running: false,
                }),
            }))
        },
        kind: Some(ErrorKind::LockHeld),
        exit: ExitStatus::Failure,
        human: || {
            "/repo/.git/index.lock is a stale lock left by process 42, which is no longer running; remove it and retry".to_owned()
        },
        hint: None,
    },
    ContractRow {
        label: "would_overwrite",
        build: || {
            Error::new(LibError::WouldOverwrite {
                reason: OverwriteReason::LocalChanges,
                paths: vec!["f.txt".into()],
                message: "local changes would be overwritten by checkout".into(),
            })
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
            Error::new(LibError::Repository(RepositoryError::NotARepository(
                NotARepositoryReason::NoRepositoryAt {
                    path: "/tmp".into(),
                },
            )))
        },
        kind: Some(ErrorKind::NotARepository),
        exit: ExitStatus::Failure,
        human: || {
            LibError::Repository(RepositoryError::NotARepository(
                NotARepositoryReason::NoRepositoryAt {
                    path: "/tmp".into(),
                },
            ))
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
        build: || {
            Error::new(grit_lib::stash::StashError::NotFound { n: 0 })
        },
        kind: Some(ErrorKind::StashNotFound),
        exit: ExitStatus::Failure,
        human: || "no stash entries".to_owned(),
        hint: Some("save local changes with `grit stash push`"),
    },
    ContractRow {
        label: "auth_required_github",
        build: || {
            Error::new(LibError::Remote(RemoteError::Auth {
                url: "https://github.com/o/r.git".into(),
                reason: AuthFailure::CredentialsUnavailable {
                    detail: "none".into(),
                    helpers: Vec::new(),
                },
            }))
        },
        kind: Some(ErrorKind::AuthRequired),
        exit: ExitStatus::Failure,
        human: || {
            RemoteError::Auth {
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
        build: || Error::new(LibError::OperationInProgress(InProgressOperation::Merge)),
        kind: Some(ErrorKind::OperationInProgress),
        exit: ExitStatus::Failure,
        human: || "cannot proceed while a merge is in progress".to_owned(),
        hint: None,
    },
    ContractRow {
        label: "config_bad_line",
        build: || {
            Error::new(LibError::Config(ConfigError::BadConfigLineInFile {
                file: ".git/config".into(),
                line: 6,
            }))
        },
        kind: None,
        exit: ExitStatus::Failure,
        human: || "fatal: bad config line 6 in file '.git/config'".to_owned(),
        hint: None,
    },
    ContractRow {
        label: "remove_blocked",
        build: || {
            Error::new(LibError::RemoveBlocked {
                paths: vec![BlockedRemoval {
                    path: "tracked.txt".into(),
                    reason: RemoveBlockReason::LocalModifications,
                }],
            })
        },
        kind: None,
        exit: ExitStatus::Failure,
        human: || {
            "not removed, these files have content that would be lost:\n    tracked.txt (local modifications)\n(use --cached to stop tracking them and keep the files, or --force to remove them anyway)".to_owned()
        },
        hint: None,
    },
    ContractRow {
        label: "rev_parse_ambiguous_change_id",
        build: || {
            Error::new(LibError::RevParse(RevParseError::AmbiguousChangeId {
                    prefix: "abc".into(),
                    candidates: vec![AmbiguousChangeIdHint {
                        oid_hex: "f92d74e3874587aaf443d1db961d4e26dde13e9c".into(),
                        change_id: "changeid123456".into(),
                        subject: "fix bug".into(),
                    }],
                }))
        },
        kind: None,
        exit: ExitStatus::Failure,
        human: || {
            "ambiguous change id 'abc'\n  f92d74e3874587aaf443d1db961d4e26dde13e9c changeid fix bug".to_owned()
        },
        hint: None,
    },
    ContractRow {
        label: "rev_parse_invalid_object_name",
        build: || {
            Error::new(LibError::RevParse(RevParseError::InvalidObjectName {
                    name: "deadbeef".into(),
                }))
        },
        kind: Some(ErrorKind::RevisionNotFound),
        exit: ExitStatus::Failure,
        human: || "revision 'deadbeef' not found".to_owned(),
        hint: None,
    },
    ContractRow {
        label: "rev_parse_ambiguous_argument",
        build: || {
            Error::new(LibError::RevParse(RevParseError::AmbiguousArgument {
                    spec: "HEAD:foo".into(),
                }))
        },
        kind: Some(ErrorKind::RevisionNotFound),
        exit: ExitStatus::Failure,
        human: || "ambiguous revision or path 'HEAD:foo'".to_owned(),
        hint: None,
    },
    ContractRow {
        label: "rev_parse_reflog_date_unparseable",
        build: || {
            Error::new(LibError::RevParse(RevParseError::ReflogDateUnparseable {
                    date: "not-a-date".into(),
                }))
        },
        kind: None,
        exit: ExitStatus::Failure,
        human: || "invalid reflog date 'not-a-date'".to_owned(),
        hint: None,
    },
    ContractRow {
        label: "invalid_path_component",
        build: || {
            Error::new(LibError::invalid_path_component(
                "foo/../bar",
                PathComponentError::DotDot,
            ))
        },
        kind: None,
        exit: ExitStatus::Failure,
        human: || "cannot use path 'foo/../bar': path component is parent directory".to_owned(),
        hint: None,
    },
    ContractRow {
        label: "index_unmerged",
        build: || Error::new(LibError::IndexUnmerged),
        kind: Some(ErrorKind::UnmergedPaths),
        exit: ExitStatus::Failure,
        human: || {
            "cannot commit: unresolved merge conflicts remain — resolve each conflicted path (grit add or grit rm), then try again".to_owned()
        },
        hint: Some("resolve conflicts with `grit add` or `grit rm`, then commit, or finish with `grit merge --continue` / `grit merge --abort`"),
    },
    ContractRow {
        label: "no_replay_in_progress",
        build: || Error::new(LibError::NoReplayInProgress),
        kind: Some(ErrorKind::NoOperationInProgress),
        exit: ExitStatus::Failure,
        human: || "no cherry-pick or revert in progress".to_owned(),
        hint: None,
    },
    ContractRow {
        label: "rev_parse_path_missing_worktree_and_index",
        build: || {
            Error::new(LibError::RevParse(
                RevParseError::PathMissingFromWorktreeAndIndex {
                    path: "missing.txt".into(),
                },
            ))
        },
        kind: Some(ErrorKind::PathNotFound),
        exit: ExitStatus::Failure,
        human: || {
            "path 'missing.txt' does not exist (neither on disk nor in the index)".to_owned()
        },
        hint: None,
    },
    ContractRow {
        label: "move_source_not_tracked",
        build: || {
            Error::new(LibError::MoveSourceNotTracked {
                src: "untracked.txt".into(),
            })
        },
        kind: Some(ErrorKind::BranchError),
        exit: ExitStatus::Failure,
        human: || "'untracked.txt' is not under version control".to_owned(),
        hint: Some("track 'untracked.txt' with `grit add` before moving it"),
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
        label: "lib_object_not_found",
        build: || Error::new(LibError::ObjectNotFound("deadbeef".into())),
        kind: None,
        exit: ExitStatus::Failure,
        human: || "revision 'deadbeef' not found".to_owned(),
        hint: None,
    },
    ContractRow {
        label: "remote_library_object_not_found",
        build: || {
            map_remote_error(RemoteError::Nested(Box::new(LibError::ObjectNotFound(
                "deadbeef".into(),
            ))))
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
        label: "identity_missing_author_email",
        build: || {
            Error::new(LibError::Identity(IdentityError::MissingConfiguredEmail {
                role: IdentRole::Author,
            }))
        },
        kind: Some(ErrorKind::IdentityUnknown),
        exit: ExitStatus::Failure,
        human: || {
            identity_error(IdentityError::MissingConfiguredEmail {
                role: IdentRole::Author,
            })
            .to_string()
        },
        hint: Some("Tell grit who you are:\n  grit config --global user.name \"Your Name\"\n  grit config --global user.email \"you@example.com\""),
    },
    ContractRow {
        label: "lib_revision_not_found",
        build: || map_lib_error(LibError::RevisionNotFound("missing".into())),
        kind: Some(ErrorKind::RevisionNotFound),
        exit: ExitStatus::Failure,
        human: || "no branch, tag or commit named 'missing'".to_owned(),
        hint: None,
    },
    ContractRow {
        label: "lib_invalid_ref",
        build: || Error::new(LibError::InvalidRef("not a valid object name: bad".into())),
        kind: None,
        exit: ExitStatus::Failure,
        human: || "invalid ref: not a valid object name: bad".to_owned(),
        hint: None,
    },
    ContractRow {
        label: "pathspec_ignored",
        build: || Error::new(LibError::PathspecIgnored {
            paths: vec!["build".into()],
        }),
        kind: None,
        exit: ExitStatus::Failure,
        human: || pathspec_ignored_human(&["build".into()]),
        hint: None,
    },
    ContractRow {
        label: "stage_conflict_markers",
        build: || Error::new(LibError::UnmergedConflictMarkers {
            paths: vec!["f.txt".into()],
        }),
        kind: Some(ErrorKind::UnmergedPaths),
        exit: ExitStatus::Failure,
        human: || stage_conflict_markers_human(&["f.txt".into()]),
        hint: Some("edit out the conflict markers in f.txt, then try again"),
    },
    ContractRow {
        label: "restore_path_unmerged",
        build: || {
            Error::new(LibError::RestorePathUnmerged {
                paths: vec!["a b".into()],
            })
        },
        kind: Some(ErrorKind::UnmergedPaths),
        exit: ExitStatus::Failure,
        human: || LibError::RestorePathUnmerged {
            paths: vec!["a b".into()],
        }
        .to_string(),
        hint: Some("take one side with `grit restore --ours 'a b'` or `grit restore --theirs 'a b'`, bring the conflict markers back with `grit restore --merge 'a b'`, or edit out the markers and run `grit add 'a b'`"),
    },
    ContractRow {
        label: "unborn_head",
        build: || map_lib_error(LibError::UnbornHead),
        kind: Some(ErrorKind::BranchNoCommits),
        exit: ExitStatus::Failure,
        human: || {
            "no commits yet on this branch — make one with `grit commit \"message\"`".to_owned()
        },
        hint: Some(r#"make one with `grit commit "message"`"#),
    },
    ContractRow {
        label: "embedded_repo_stage_refused",
        build: || {
            map_stage_lib_error(LibError::EmbeddedRepoWithoutCommit {
                path: "sub".into(),
            })
        },
        kind: Some(ErrorKind::EmbeddedRepoWithoutCommit),
        exit: ExitStatus::Failure,
        human: || {
            "cannot stage 'sub': embedded git repository has no commit checked out".to_owned()
        },
        hint: None,
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
            Error::new(LibError::Repository(RepositoryError::git_dir_path_io(
                ".git/index",
                GitDirPathOperation::WriteIndex,
                std::io::Error::from_raw_os_error(16),
            )))
        },
        kind: Some(ErrorKind::GitDirPathIo),
        exit: ExitStatus::Failure,
        human: || {
            LibError::Repository(RepositoryError::git_dir_path_io(
                ".git/index",
                GitDirPathOperation::WriteIndex,
                std::io::Error::from_raw_os_error(16),
            ))
            .to_string()
        },
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
            Error::new(LibError::Remote(RemoteError::Auth {
                url: "https://git.example.com/o/r.git".into(),
                reason: AuthFailure::Rejected,
            }))
        },
        kind: Some(ErrorKind::AuthRejected),
        exit: ExitStatus::Failure,
        human: || {
            RemoteError::Auth {
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
            Error::new(CloneError::Remote(RemoteError::Unreadable {
                url: Some("https://example.com/x.git".to_owned()),
                reason: RemoteUnreadableReason::HttpStatus { status: 404 },
            }))
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

struct ControlFlowAllowance {
    file: &'static str,
    line_contains: &'static str,
}

/// Explicit control-flow matches on library errors outside `error_map.rs` (not presentation).
static CONTROL_FLOW_ALLOWANCES: &[ControlFlowAllowance] = &[
    ControlFlowAllowance {
        file: "src/unborn_errors.rs",
        line_contains: "Some(LibError::UnbornHead)",
    },
    ControlFlowAllowance {
        file: "src/json_error.rs",
        line_contains: "Some(LibError::Hook(hook))",
    },
    ControlFlowAllowance {
        file: "src/exit_status.rs",
        line_contains: "grit_lib::error::Error::is_cancelled",
    },
    ControlFlowAllowance {
        file: "src/exit_status.rs",
        line_contains: "grit_lib::error::Error::Hook(HookError::Failed",
    },
    ControlFlowAllowance {
        file: "src/exit_status.rs",
        line_contains: "grit_lib::error::Error::remote_unreadable(",
    },
    ControlFlowAllowance {
        file: "src/exit_status.rs",
        line_contains: "grit_lib::error::Error::Auth {",
    },
    ControlFlowAllowance {
        file: "src/exit_status.rs",
        line_contains: "grit_lib::error::Error::Cancelled",
    },
    ControlFlowAllowance {
        file: "src/exit_status.rs",
        line_contains: "RemoteError::Library(grit_lib::error::Error::Cancelled)",
    },
    ControlFlowAllowance {
        file: "src/exit_status.rs",
        line_contains: "map_branch_error(grit_lib::error::Error::Branch",
    },
    ControlFlowAllowance {
        file: "src/commands/add.rs",
        line_contains: "LibError::Cancelled",
    },
    ControlFlowAllowance {
        file: "src/commands/serve.rs",
        line_contains: "LibError::Cancelled",
    },
    ControlFlowAllowance {
        file: "src/commands/bundle.rs",
        line_contains: "grit_lib::error::Error::Cancelled",
    },
    ControlFlowAllowance {
        file: "src/commands/stash.rs",
        line_contains: "LibError::HunkSelection(_)",
    },
    ControlFlowAllowance {
        file: "src/commands/auth.rs",
        line_contains: "grit_lib::error::Error::Auth { .. }",
    },
    ControlFlowAllowance {
        file: "src/commands/log.rs",
        line_contains: "grit_lib::error::Error::Io(_)",
    },
    ControlFlowAllowance {
        file: "src/commands/cat.rs",
        line_contains: "map_err(grit_lib::error::Error::Io)",
    },
    ControlFlowAllowance {
        file: "src/commands/diff.rs",
        line_contains: "grit_lib::error::Error::PathError",
    },
    ControlFlowAllowance {
        file: "src/commands/history.rs",
        line_contains: "LibError::Hook(_)",
    },
    ControlFlowAllowance {
        file: "src/cli_messages.rs",
        line_contains: "LibError::LockHeld(LockHeldError",
    },
    ControlFlowAllowance {
        file: "src/cli_messages.rs",
        line_contains: "LibError::Config(ConfigError::BadConfigLineInFile",
    },
    ControlFlowAllowance {
        file: "src/commands/fetch.rs",
        line_contains: "LibError::ObjectNotFound(",
    },
    ControlFlowAllowance {
        file: "src/commands/fetch.rs",
        line_contains: "LibError::HashCollision",
    },
    ControlFlowAllowance {
        file: "src/commands/auth.rs",
        line_contains: "RemoteError::Auth { .. }",
    },
];

const LIB_ERROR_MATCH_PREFIXES: &[&str] = &["LibError::", "grit_lib::error::Error::"];

fn line_allowed(rel: &str, line: &str) -> bool {
    CONTROL_FLOW_ALLOWANCES
        .iter()
        .any(|entry| entry.file == rel && line.contains(entry.line_contains))
}

fn visit_rs_sources(manifest: &Path, dir: &Path) {
    for entry in std::fs::read_dir(dir).unwrap_or_else(|e| panic!("read {}: {e}", dir.display())) {
        let entry = entry.unwrap_or_else(|e| panic!("dir entry: {e}"));
        let path = entry.path();
        if path.is_dir() {
            visit_rs_sources(manifest, &path);
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
        if rel == "src/error_map.rs"
            || rel == "src/error_map_contract.rs"
            || rel == "src/cli_messages.rs"
        {
            continue;
        }
        let text = std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {rel}: {e}"));
        for (line_no, line) in text.lines().enumerate() {
            if !LIB_ERROR_MATCH_PREFIXES
                .iter()
                .any(|prefix| line.contains(prefix))
            {
                continue;
            }
            assert!(
                line_allowed(&rel, line),
                "{rel}:{} presentation or unlisted LibError match — centralize in error_map.rs or add a control-flow allowance:\n{line}",
                line_no + 1
            );
        }
    }
}

pub fn assert_lib_error_match_ratchet() {
    let manifest = Path::new(env!("CARGO_MANIFEST_DIR"));
    visit_rs_sources(manifest, &manifest.join("src"));
}

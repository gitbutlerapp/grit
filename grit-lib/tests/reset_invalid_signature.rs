//! Invalid reset identities must fail preflight without changing repository files.

#![allow(clippy::unwrap_used, clippy::expect_used)]

#[path = "support/reset_snapshot.rs"]
mod reset_snapshot;

use std::fs;
use std::path::Path;

use grit_lib::config::environment::RepositoryOptions;
use grit_lib::error::Error;
use grit_lib::ident::{Signature, SignatureError, SignatureField};
use grit_lib::porcelain::reset::{preview_reset, reset_to_commit, ResetMode, ResetOptions};
use grit_lib::porcelain::switch::resolve_revision_spec;
use grit_lib::progress::{NeverCancel, NullProgress};
use grit_lib::repo::Repository;
use grit_test_support::git_cmd;
use reset_snapshot::{assert_unchanged, snapshot};
use time::{OffsetDateTime, UtcOffset};

fn identity() -> Signature {
    "Tester <tester@example.com> 1700000000 +0000"
        .parse()
        .unwrap()
}

fn open_repo(root: &Path) -> Repository {
    Repository::open(
        &root.join(".git"),
        &RepositoryOptions::empty().with_work_tree(root),
    )
    .unwrap()
}

fn two_commits(root: &Path) {
    git_cmd(&["init", "-q", "-b", "main"]).in_dir(root).suc();
    git_cmd(&["config", "user.name", "Tester"])
        .in_dir(root)
        .suc();
    git_cmd(&["config", "user.email", "tester@example.com"])
        .in_dir(root)
        .suc();
    fs::write(root.join("a"), "one\n").unwrap();
    fs::write(root.join("b"), "unchanged\n").unwrap();
    git_cmd(&["add", "."]).in_dir(root).suc();
    git_cmd(&["commit", "-qm", "one"]).in_dir(root).suc();
    fs::write(root.join("a"), "two\n").unwrap();
    git_cmd(&["commit", "-qam", "two"]).in_dir(root).suc();
}

fn invalid_identities() -> Vec<(Signature, SignatureError)> {
    let mut name = identity();
    name.name = "bad\nname".into();
    let mut email = identity();
    email.email = "bad>email".into();
    let mut before_epoch = identity();
    before_epoch.time = OffsetDateTime::from_unix_timestamp(-1).unwrap();
    let mut offset = identity();
    offset.time = offset.time.to_offset(UtcOffset::from_hms(0, 0, 1).unwrap());
    vec![
        (
            name,
            SignatureError::ForbiddenCharacter {
                field: SignatureField::Name,
                ch: '\n',
            },
        ),
        (
            email,
            SignatureError::ForbiddenCharacter {
                field: SignatureField::Email,
                ch: '>',
            },
        ),
        (before_epoch, SignatureError::BeforeEpoch),
        (offset, SignatureError::SubMinuteOffset),
    ]
}

fn assert_invalid(error: Error, expected: &SignatureError) {
    match error {
        Error::InvalidSignature(actual) => assert_eq!(&actual, expected),
        other => panic!("expected InvalidSignature({expected:?}), got {other:?}"),
    }
}

#[test]
fn hard_reset_invalid_name_preserves_head_and_valuable_edit() {
    let dir = tempfile::tempdir().unwrap();
    two_commits(dir.path());
    fs::write(dir.path().join("a"), "valuable uncommitted edit\n").unwrap();
    let repo = open_repo(dir.path());
    let before = resolve_revision_spec(&repo, "HEAD").unwrap();
    let target = resolve_revision_spec(&repo, "HEAD~1").unwrap();
    let mut identity = identity();
    identity.name = "bad\nname".into();
    let files = snapshot(dir.path());
    let result = reset_to_commit(
        &repo,
        &ResetOptions::new(target, ResetMode::Hard, identity, "reset probe"),
        &mut NullProgress,
        &NeverCancel,
    );
    assert_invalid(
        result.unwrap_err(),
        &SignatureError::ForbiddenCharacter {
            field: SignatureField::Name,
            ch: '\n',
        },
    );
    assert_eq!(resolve_revision_spec(&repo, "HEAD").unwrap(), before);
    assert_eq!(
        fs::read_to_string(dir.path().join("a")).unwrap(),
        "valuable uncommitted edit\n"
    );
    assert_unchanged(dir.path(), &files);
}

fn check_invalid_signatures(mode: ResetMode) {
    for detached in [false, true] {
        for current_head in [false, true] {
            for log_updates in [false, true] {
                let dir = tempfile::tempdir().unwrap();
                let root = dir.path();
                two_commits(root);
                if detached {
                    git_cmd(&["switch", "--detach", "HEAD"]).in_dir(root).suc();
                }
                git_cmd(&[
                    "config",
                    "core.logAllRefUpdates",
                    if log_updates { "true" } else { "false" },
                ])
                .in_dir(root)
                .suc();
                // Keep's edits are on a path unchanged between the commits, so a valid reset is permitted.
                let edited = if mode == ResetMode::Keep { "b" } else { "a" };
                fs::write(root.join(edited), "staged edit\n").unwrap();
                git_cmd(&["add", edited]).in_dir(root).suc();
                fs::write(root.join(edited), "unstaged edit\n").unwrap();
                fs::write(root.join("untracked"), "untracked edit\n").unwrap();
                let repo = open_repo(root);
                let head = resolve_revision_spec(&repo, "HEAD").unwrap();
                let target =
                    resolve_revision_spec(&repo, if current_head { "HEAD" } else { "HEAD~1" })
                        .unwrap();
                let files = snapshot(root);
                for (identity, expected) in invalid_identities() {
                    let mut opts = ResetOptions::new(target, mode, identity, "reset probe");
                    assert_invalid(preview_reset(&repo, &opts).unwrap_err(), &expected);
                    assert_unchanged(root, &files);
                    for dry_run in [true, false] {
                        opts.dry_run = dry_run;
                        let error = reset_to_commit(&repo, &opts, &mut NullProgress, &NeverCancel)
                            .unwrap_err();
                        assert_invalid(error, &expected);
                        assert_unchanged(root, &files);
                        assert_eq!(resolve_revision_spec(&repo, "HEAD").unwrap(), head);
                    }
                }
                let opts = ResetOptions::new(target, mode, identity(), "valid reset");
                reset_to_commit(&repo, &opts, &mut NullProgress, &NeverCancel).unwrap();
                assert_eq!(resolve_revision_spec(&repo, "HEAD").unwrap(), target);
                assert_eq!(
                    git_cmd(&["rev-parse", "HEAD"])
                        .in_dir(root)
                        .suc()
                        .stdout
                        .trim(),
                    target.to_hex()
                );
                git_cmd(&["fsck", "--strict"]).in_dir(root).suc();
            }
        }
    }
}

#[test]
fn soft_reset_invalid_signature_is_non_mutating() {
    check_invalid_signatures(ResetMode::Soft);
}
#[test]
fn mixed_reset_invalid_signature_is_non_mutating() {
    check_invalid_signatures(ResetMode::Mixed);
}
#[test]
fn hard_reset_invalid_signature_is_non_mutating() {
    check_invalid_signatures(ResetMode::Hard);
}
#[test]
fn keep_reset_invalid_signature_is_non_mutating() {
    check_invalid_signatures(ResetMode::Keep);
}

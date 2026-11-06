//! A malformed gitfile stops discovery in grit-lib the same way it does in system `git`.

use std::fs;

use grit_lib::error::{Error, NotARepositoryReason};
use grit_lib::repo::RepositoryError;
use grit_lib::revwalk::rev_parse::discover_optional;
use grit_test_support::{git, git_cmd};
use tempfile::TempDir;

#[test]
fn malformed_gitfile_below_a_repository_is_not_skipped() {
    let td = TempDir::new().expect("tempdir");
    git(td.path(), &["init", "-q"]);
    let sub = td.path().join("sub");
    fs::create_dir(&sub).expect("mkdir");

    let found = discover_optional(Some(&sub))
        .expect("discover")
        .expect("repo");
    assert_eq!(
        found.git_dir().canonicalize().expect("canonical"),
        td.path().join(".git").canonicalize().expect("canonical")
    );

    fs::write(sub.join(".git"), "not a gitdir line\n").expect("write");
    let git_out = git_cmd(&["rev-parse", "--git-dir"]).in_dir(&sub).exec();
    assert!(!git_out.ok(), "git skipped the malformed gitfile");

    let err = discover_optional(Some(&sub)).expect_err("malformed gitfile");
    assert!(
        matches!(
            &err,
            Error::Repository(RepositoryError::NotARepository(
                NotARepositoryReason::GitfileMissingGitdir { path }
            )) if *path == sub.join(".git")
        ),
        "{err:?}"
    );
}

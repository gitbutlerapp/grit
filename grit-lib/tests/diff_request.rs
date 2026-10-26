//! `resolve_diff_request` / `resolve_commit_diff_pathspecs`: operand disambiguation, range and
//! revision resolution, pathspec rules, and every typed error, on repositories built by `git`.

use std::error::Error as _;
use std::path::Path;

use grit_lib::config::environment::RepositoryOptions;
use grit_lib::diff::{
    resolve_commit_diff_pathspecs, resolve_diff_request, DiffArguments, DiffRequest, DiffTarget,
    ResolveDiffArgumentsError,
};
use grit_lib::error::{Error, PathspecResolveError};
use grit_lib::objects::ObjectId;
use grit_lib::repo::Repository;
use grit_test_support::git;
use tempfile::TempDir;

fn rev(dir: &Path, spec: &str) -> ObjectId {
    git(dir, &["rev-parse", spec]).trim().parse().expect("oid")
}

fn commit_file(dir: &Path, path: &str, content: &str, msg: &str) -> ObjectId {
    std::fs::write(dir.join(path), content).expect("write");
    git(dir, &["add", "-A"]);
    git(dir, &["commit", "-qm", msg]);
    rev(dir, "HEAD")
}

/// Two commits on `main`: `a.txt` then `b.txt`; `a.txt` is then modified in the worktree.
fn two_commit_repo() -> (TempDir, Repository) {
    let dir = TempDir::new().expect("tempdir");
    let root = dir.path();
    git(root, &["init", "-q", "-b", "main", "."]);
    git(root, &["config", "user.email", "u@example.com"]);
    git(root, &["config", "user.name", "U"]);
    commit_file(root, "a.txt", "1\n", "c1");
    commit_file(root, "b.txt", "2\n", "c2");
    std::fs::write(root.join("a.txt"), "dirty\n").expect("write");
    let repo = Repository::open(
        &root.join(".git"),
        &RepositoryOptions::empty().with_work_tree(root),
    )
    .expect("open");
    (dir, repo)
}

fn strings(items: &[&str]) -> Vec<String> {
    items.iter().map(|s| (*s).to_owned()).collect()
}

fn positionals(items: &[&str]) -> DiffRequest {
    DiffRequest {
        positionals: strings(items),
        ..DiffRequest::default()
    }
}

fn resolve(repo: &Repository, request: &DiffRequest) -> DiffArguments {
    resolve_diff_request(repo, None, request).expect("resolve")
}

fn resolve_err(repo: &Repository, request: &DiffRequest) -> ResolveDiffArgumentsError {
    resolve_diff_request(repo, None, request).expect_err("should fail")
}

#[test]
fn no_operands_compare_worktree_with_head() {
    let (_dir, repo) = two_commit_repo();
    let args = resolve(&repo, &DiffRequest::default());
    assert_eq!(args.target, DiffTarget::WorktreeVsHead);
    assert!(args.pathspecs.is_empty());
}

#[test]
fn lone_known_path_operand_limits_the_worktree_diff() {
    let (_dir, repo) = two_commit_repo();
    let args = resolve(&repo, &positionals(&["a.txt"]));
    assert_eq!(args.target, DiffTarget::WorktreeVsHead);
    assert_eq!(args.pathspecs, strings(&["a.txt"]));
}

#[test]
fn revision_then_path_operand_limits_that_commit() {
    let (dir, repo) = two_commit_repo();
    let args = resolve(&repo, &positionals(&["HEAD~1", "a.txt"]));
    assert_eq!(args.target, DiffTarget::Commit(rev(dir.path(), "HEAD~1")));
    assert_eq!(args.pathspecs, strings(&["a.txt"]));
}

#[test]
fn trailing_path_operands_after_two_revisions_become_pathspecs() {
    let (dir, repo) = two_commit_repo();
    let mut args = resolve(&repo, &positionals(&["HEAD~1", "HEAD", "a.txt", "b.txt"]));
    args.pathspecs.sort();
    assert_eq!(
        args.target,
        DiffTarget::Trees {
            old: Some(rev(dir.path(), "HEAD~1")),
            new: rev(dir.path(), "HEAD"),
        }
    );
    assert_eq!(args.pathspecs, strings(&["a.txt", "b.txt"]));
}

#[test]
fn explicit_paths_are_never_reinterpreted() {
    let (dir, repo) = two_commit_repo();
    let request = DiffRequest {
        positionals: strings(&["HEAD"]),
        paths: strings(&["b.txt"]),
        ..DiffRequest::default()
    };
    let args = resolve(&repo, &request);
    assert_eq!(args.target, DiffTarget::Commit(rev(dir.path(), "HEAD")));
    assert_eq!(args.pathspecs, strings(&["b.txt"]));
}

#[test]
fn unknown_lone_operand_is_reported_as_a_path_matching_nothing() {
    let (_dir, repo) = two_commit_repo();
    let err = resolve_err(&repo, &positionals(&["no-such-thing"]));
    assert!(
        matches!(&err, ResolveDiffArgumentsError::Library(lib) if matches!(lib.as_ref(), Error::PathspecNoMatch { spec } if spec == "no-such-thing")),
        "{err:?}"
    );
}

#[test]
fn unknown_revision_with_paths_reports_the_revision_and_keeps_the_cause() {
    let (_dir, repo) = two_commit_repo();
    let request = DiffRequest {
        positionals: strings(&["no-such-rev"]),
        paths: strings(&["a.txt"]),
        ..DiffRequest::default()
    };
    let err = resolve_err(&repo, &request);
    assert!(
        matches!(&err, ResolveDiffArgumentsError::UnresolvedRevision { spec, .. } if spec == "no-such-rev"),
        "{err:?}"
    );
    assert_eq!(err.to_string(), "could not resolve 'no-such-rev'");
    assert!(err.source().is_some());
}

#[test]
fn two_dot_range_diffs_the_endpoint_trees() {
    let (dir, repo) = two_commit_repo();
    let args = resolve(&repo, &positionals(&["HEAD~1..HEAD"]));
    let expected = DiffTarget::Trees {
        old: Some(rev(dir.path(), "HEAD~1")),
        new: rev(dir.path(), "HEAD"),
    };
    assert_eq!(args.target, expected);
    let open_left = resolve(&repo, &positionals(&["HEAD~1.."]));
    assert_eq!(open_left.target, expected);
}

#[test]
fn range_then_known_path_operand_limits_the_range() {
    let (dir, repo) = two_commit_repo();
    let request = DiffRequest {
        positionals: strings(&["HEAD~1..HEAD", "a.txt"]),
        paths: strings(&["b.txt"]),
        pathspec_file: Some(strings(&["from-file.txt"])),
        ..DiffRequest::default()
    };
    let args = resolve(&repo, &request);
    assert_eq!(
        args.target,
        DiffTarget::Trees {
            old: Some(rev(dir.path(), "HEAD~1")),
            new: rev(dir.path(), "HEAD"),
        }
    );
    assert_eq!(args.pathspecs, strings(&["from-file.txt", "a.txt"]));
}

#[test]
fn range_then_revision_operand_is_rejected() {
    let (_dir, repo) = two_commit_repo();
    let request = DiffRequest {
        positionals: strings(&["HEAD~1..HEAD", "HEAD"]),
        paths: strings(&["b.txt"]),
        ..DiffRequest::default()
    };
    let err = resolve_err(&repo, &request);
    assert!(
        matches!(err, ResolveDiffArgumentsError::RangeWithSecondRevision),
        "{err:?}"
    );
}

#[test]
fn three_dot_range_without_common_history_has_no_merge_base() {
    let (dir, repo) = two_commit_repo();
    git(dir.path(), &["checkout", "-q", "--", "a.txt"]);
    git(dir.path(), &["switch", "-q", "--orphan", "lonely"]);
    commit_file(dir.path(), "l.txt", "l\n", "lonely root");
    git(dir.path(), &["switch", "-q", "-f", "main"]);
    let err = resolve_err(&repo, &positionals(&["main...lonely"]));
    assert!(
        matches!(err, ResolveDiffArgumentsError::NoMergeBase),
        "{err:?}"
    );
}

#[test]
fn three_dot_range_over_criss_cross_history_uses_gits_first_merge_base() {
    let (dir, repo) = two_commit_repo();
    let root = dir.path();
    git(root, &["checkout", "-q", "--", "a.txt"]);
    git(root, &["branch", "y"]);
    commit_file(root, "x.txt", "x\n", "x1");
    git(root, &["branch", "x1"]);
    git(root, &["switch", "-q", "y"]);
    commit_file(root, "y.txt", "y\n", "y1");
    git(root, &["branch", "y1"]);
    git(root, &["merge", "-q", "--no-edit", "--no-ff", "x1"]);
    git(root, &["switch", "-q", "main"]);
    git(root, &["merge", "-q", "--no-edit", "--no-ff", "y1"]);
    assert_eq!(
        git(root, &["merge-base", "--all", "main", "y"])
            .lines()
            .count(),
        2
    );

    let args = resolve(&repo, &positionals(&["main...y"]));
    assert_eq!(
        args.target,
        DiffTarget::Trees {
            old: Some(rev(
                root,
                &git(root, &["merge-base", "main", "y"]).trim().to_owned()
            )),
            new: rev(root, "y"),
        }
    );
}

#[test]
fn against_compares_the_worktree_with_that_revisions_tree() {
    let (dir, repo) = two_commit_repo();
    let request = DiffRequest {
        against: Some("HEAD~1".into()),
        ..DiffRequest::default()
    };
    let args = resolve(&repo, &request);
    assert_eq!(
        args.target,
        DiffTarget::WorktreeVsTree(rev(dir.path(), "HEAD~1^{tree}"))
    );
}

#[test]
fn against_a_tag_of_a_blob_is_not_a_commit() {
    let (dir, repo) = two_commit_repo();
    let blob = rev(dir.path(), "HEAD:b.txt");
    git(
        dir.path(),
        &["tag", "-a", "-m", "blob", "blob-tag", &blob.to_hex()],
    );
    let request = DiffRequest {
        against: Some("blob-tag".into()),
        ..DiffRequest::default()
    };
    let err = resolve_err(&repo, &request);
    assert!(
        matches!(err, ResolveDiffArgumentsError::NotACommit { oid } if oid == blob),
        "{err:?}"
    );
}

#[test]
fn flag_conflicts_are_typed() {
    let (_dir, repo) = two_commit_repo();
    let staged = DiffRequest {
        staged: true,
        positionals: strings(&["HEAD"]),
        ..DiffRequest::default()
    };
    assert!(matches!(
        resolve_err(&repo, &staged),
        ResolveDiffArgumentsError::StagedWithRevisions
    ));
    let unstaged = DiffRequest {
        unstaged: true,
        against: Some("HEAD".into()),
        ..DiffRequest::default()
    };
    assert!(matches!(
        resolve_err(&repo, &unstaged),
        ResolveDiffArgumentsError::UnstagedWithRevisions
    ));
}

#[test]
fn staged_and_unstaged_resolve_worktree_pathspecs() {
    let (_dir, repo) = two_commit_repo();
    let staged = DiffRequest {
        staged: true,
        positionals: strings(&["a.txt"]),
        ..DiffRequest::default()
    };
    let args = resolve(&repo, &staged);
    assert_eq!(args.target, DiffTarget::IndexVsHead);
    assert_eq!(args.pathspecs, strings(&["a.txt"]));
    let unstaged = DiffRequest {
        unstaged: true,
        ..DiffRequest::default()
    };
    assert_eq!(
        resolve(&repo, &unstaged).target,
        DiffTarget::WorktreeVsIndex
    );
}

#[test]
fn pathspec_file_entries_come_first_and_must_not_be_empty() {
    let (_dir, repo) = two_commit_repo();
    let request = DiffRequest {
        paths: strings(&["a.txt"]),
        pathspec_file: Some(strings(&["b.txt"])),
        ..DiffRequest::default()
    };
    assert_eq!(
        resolve(&repo, &request).pathspecs,
        strings(&["b.txt", "a.txt"])
    );

    let empty_file = DiffRequest {
        pathspec_file: Some(Vec::new()),
        ..DiffRequest::default()
    };
    let err = resolve_err(&repo, &empty_file);
    assert!(
        matches!(
            err,
            ResolveDiffArgumentsError::Pathspec(PathspecResolveError::EmptyPathspec)
        ),
        "{err:?}"
    );
    let empty_entry = DiffRequest {
        paths: strings(&[""]),
        ..DiffRequest::default()
    };
    assert!(matches!(
        resolve_err(&repo, &empty_entry),
        ResolveDiffArgumentsError::Pathspec(PathspecResolveError::EmptyPathspec)
    ));
}

#[test]
fn worktree_pathspecs_in_a_bare_repository_need_a_work_tree() {
    let dir = TempDir::new().expect("tempdir");
    git(dir.path(), &["init", "-q", "--bare", "repo.git"]);
    let repo = Repository::open(&dir.path().join("repo.git"), &RepositoryOptions::empty())
        .expect("open bare");
    let request = DiffRequest {
        staged: true,
        paths: strings(&["a.txt"]),
        ..DiffRequest::default()
    };
    let err = resolve_err(&repo, &request);
    assert!(
        matches!(err, ResolveDiffArgumentsError::NoWorkTree),
        "{err:?}"
    );
}

#[test]
fn commit_pathspecs_resolve_from_the_cwd_and_allow_missing_paths() {
    let (_dir, repo) = two_commit_repo();
    let specs =
        resolve_commit_diff_pathspecs(&repo, Some("sub"), &strings(&["deleted.txt", "../a.txt"]))
            .expect("resolve");
    assert_eq!(specs, strings(&["sub/deleted.txt", "a.txt"]));
    assert!(resolve_commit_diff_pathspecs(&repo, None, &[])
        .expect("empty")
        .is_empty());
}

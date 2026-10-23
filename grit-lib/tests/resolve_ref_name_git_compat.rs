//! `Repository::resolve_ref_name` matches `git rev-parse --abbrev-ref` / `--symbolic-full-name`.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use grit_lib::config::environment::RepositoryOptions;
use grit_lib::object_inspect::ResolvedRefName;
use grit_lib::refs::ref_storage::RefStorageFormat;
use grit_lib::repo::{InitOptions, Repository};
use grit_lib::revwalk::rev_parse_error::RevParseError;

mod support;
use support::{git, git_init_reftable_repo, require_reftable_git};

fn git_rev_parse_ref(dir: &std::path::Path, abbrev: bool, spec: &str) -> Option<String> {
    let flag = if abbrev {
        "--abbrev-ref"
    } else {
        "--symbolic-full-name"
    };
    let out = std::process::Command::new("git")
        .current_dir(dir)
        .args(["rev-parse", flag, spec])
        .env("GIT_CONFIG_GLOBAL", support::null_device_for_tests())
        .env("GIT_CONFIG_SYSTEM", support::null_device_for_tests())
        .output()
        .expect("git");
    if out.status.success() {
        let s = String::from_utf8_lossy(&out.stdout).trim().to_string();
        if s.is_empty() {
            None
        } else {
            Some(s)
        }
    } else {
        None
    }
}

fn assert_ref_names(repo: &Repository, wt: &std::path::Path, spec: &str) {
    for abbrev in [true, false] {
        let grit = match repo.resolve_ref_name(spec) {
            Ok(v) => v,
            Err(RevParseError::AmbiguousRefname { .. }) => None,
            Err(e) => panic!("resolve_ref_name({spec}): {e}"),
        };
        let git_line = git_rev_parse_ref(wt, abbrev, spec);
        match (&grit, &git_line) {
            (None, None) => {}
            (Some(ResolvedRefName { full, short }), Some(expected)) => {
                let grit_out = if abbrev { short } else { full };
                assert_eq!(
                    grit_out, expected,
                    "backend mismatch for spec `{spec}` abbrev={abbrev}"
                );
            }
            (g, e) => panic!("grit {g:?} vs git {e:?} for `{spec}` abbrev={abbrev}",),
        }
    }
}

fn open_grit(wt: &std::path::Path) -> Repository {
    Repository::discover(Some(wt), &RepositoryOptions::empty()).expect("discover")
}

/// Shared Git parity matrix (uses system `git` in `wt` for oracle lines).
fn exercise_resolve_ref_name_git_parity(wt: &std::path::Path) {
    git(wt, &["commit", "--allow-empty", "-m", "seed"]);
    let grit = open_grit(wt);

    git(wt, &["branch", "topic"]);
    git(wt, &["tag", "v1"]);
    git(wt, &["remote", "add", "origin", wt.to_str().unwrap()]);
    git(wt, &["fetch", "origin", "main:refs/remotes/origin/main"]);
    git(wt, &["branch", "--set-upstream-to", "origin/main", "main"]);

    assert_ref_names(&grit, wt, "HEAD");
    assert_ref_names(&grit, wt, "main");
    assert_ref_names(&grit, wt, "v1");
    assert_ref_names(&grit, wt, "origin/main");
    assert_ref_names(&grit, wt, "@{u}");

    git(wt, &["symbolic-ref", "refs/heads/alias", "refs/heads/main"]);
    git(wt, &["symbolic-ref", "HEAD", "refs/heads/alias"]);
    assert_ref_names(&open_grit(wt), wt, "HEAD");

    git(wt, &["symbolic-ref", "HEAD", "refs/heads/main"]);

    git(wt, &["checkout", "-q", "topic"]);
    git(wt, &["checkout", "-q", "main"]);
    assert_ref_names(&open_grit(wt), wt, "@{-1}");

    let head = git(wt, &["rev-parse", "HEAD"]).trim().to_string();
    git(wt, &["checkout", "-q", &head]);
    assert_ref_names(&open_grit(wt), wt, "HEAD");

    git(wt, &["checkout", "-q", "main"]);
    git(wt, &["branch", "x"]);
    git(wt, &["tag", "x"]);
    git(wt, &["branch", "heads/x"]);
    assert_ref_names(&open_grit(wt), wt, "refs/heads/x");
    assert_ref_names(&open_grit(wt), wt, "refs/tags/x");
    assert_ref_names(&open_grit(wt), wt, "x");
    assert_ref_names(&open_grit(wt), wt, "heads/x");

    git(wt, &["config", "core.warnAmbiguousRefs", "FALSE"]);
    assert_ref_names(&open_grit(wt), wt, "refs/tags/x");
}

#[test]
fn resolve_ref_name_matches_git_on_both_backends() {
    let files_root = tempfile::tempdir().expect("tempdir");
    let files_wt = files_root.path();
    Repository::init(
        files_wt,
        &InitOptions::new()
            .with_initial_branch("main".to_owned())
            .with_ref_storage(RefStorageFormat::Files),
    )
    .expect("init files");
    exercise_resolve_ref_name_git_parity(files_wt);

    if !require_reftable_git() {
        return;
    }
    let reftable_root = git_init_reftable_repo("main");
    exercise_resolve_ref_name_git_parity(reftable_root.path());
}

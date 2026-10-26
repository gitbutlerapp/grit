//! Ref query sort, pattern, and limit parity with `git for-each-ref`.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use crate::support;

use grit_lib::refs::listing::{
    query_refs, RefNamespace, RefQuery, RefQueryOptions, RefSort, SortKey,
};
use grit_lib::repo::{InitOptions, Repository};
use support::{each_backend, git, git_interop_available, require_reftable_git, Backend};

fn git_sorted_refnames(
    worktree: &std::path::Path,
    sort_key: &str,
    reverse: bool,
    glob_patterns: &[String],
) -> Vec<String> {
    let sort = if reverse {
        format!("-{sort_key}")
    } else {
        sort_key.to_owned()
    };
    let sort_arg = format!("--sort={sort}");
    let mut args = vec!["for-each-ref", sort_arg.as_str(), "--format=%(refname)"];
    if glob_patterns.is_empty() {
        args.push("refs/tags/");
    } else {
        for p in glob_patterns {
            args.push(p.as_str());
        }
    }
    git(worktree, &args)
        .lines()
        .filter(|l| !l.is_empty())
        .map(str::to_owned)
        .collect()
}

fn grit_sorted_refnames(
    repo: &Repository,
    sort_key: SortKey,
    reverse: bool,
    patterns: Vec<String>,
    limit: Option<usize>,
    options: &RefQueryOptions,
) -> Vec<String> {
    let query = RefQuery {
        namespaces: vec![RefNamespace::Tags],
        patterns,
        sort: RefSort {
            key: sort_key,
            reverse,
        },
        limit,
        ..RefQuery::default()
    };
    query_refs(repo, &query, options)
        .expect("query_refs")
        .into_iter()
        .map(|r| r.full_name)
        .collect()
}

fn seed_tricky_tags(worktree: &std::path::Path) {
    git(worktree, &["commit", "--allow-empty", "-q", "-m", "seed"]);
    for name in [
        "1.9",
        "1.10",
        "01.10",
        "v1.0",
        "v1.0-rc1",
        "v1.0-rc2",
        "feature/foo",
        "feature/bar",
        "release-rc1",
        "leading-zero-007",
    ] {
        git(worktree, &["tag", name]);
    }
    git(worktree, &["tag", "-a", "annotated-old", "-m", "m", "HEAD"]);
    std::process::Command::new("git")
        .current_dir(worktree)
        .args(["tag", "-a", "annotated-new", "-m", "m", "HEAD"])
        .env("GIT_COMMITTER_DATE", "1800000000 +0000")
        .env("GIT_AUTHOR_DATE", "1800000000 +0000")
        .status()
        .expect("annotated-new tag");
}

fn assert_sort_parity(
    repo: &Repository,
    worktree: &std::path::Path,
    git_sort: &str,
    grit_key: SortKey,
    reverse: bool,
    patterns: &[&str],
    options: &RefQueryOptions,
) {
    let git_names = git_sorted_refnames(worktree, git_sort, reverse, &[]);
    let grit_names = grit_sorted_refnames(
        repo,
        grit_key,
        reverse,
        patterns.iter().map(|s| (*s).to_owned()).collect(),
        None,
        options,
    );
    assert_eq!(
        grit_names, git_names,
        "sort={git_sort} reverse={reverse} patterns={patterns:?}"
    );
}

fn run_query_matrix(worktree: &std::path::Path, backend: Backend, storage: &str) {
    if !git_interop_available(backend) {
        return;
    }
    seed_tricky_tags(worktree);
    git(worktree, &["config", "versionsort.suffix", "-rc"]);
    if storage == "packed" {
        git(worktree, &["pack-refs", "--all"]);
    }
    let repository = Repository::discover(
        Some(worktree),
        &grit_lib::config::environment::RepositoryOptions::empty(),
    )
    .expect("discover");
    let options = RefQueryOptions {
        versionsort_suffixes: vec!["-rc".to_owned()],
        ignore_case: false,
    };

    for (git_sort, grit_key) in [
        ("refname", SortKey::Name),
        ("version:refname", SortKey::Version),
        ("creatordate", SortKey::Date),
    ] {
        for reverse in [false, true] {
            assert_sort_parity(
                &repository,
                worktree,
                git_sort,
                grit_key,
                reverse,
                &[],
                &options,
            );
        }
    }

    for (short_pattern, git_glob) in [
        ("v1.*", "refs/tags/v1.*"),
        ("feature/", "refs/tags/feature/"),
        ("*rc*", "refs/tags/*rc*"),
    ] {
        let git_names = git_sorted_refnames(worktree, "refname", false, &[git_glob.to_owned()]);
        let grit_names = grit_sorted_refnames(
            &repository,
            SortKey::Name,
            false,
            vec![short_pattern.to_owned()],
            None,
            &options,
        );
        assert_eq!(grit_names, git_names, "pattern={short_pattern}");
    }

    let git_limited = git_sorted_refnames(worktree, "refname", false, &[]);
    let grit_limited =
        grit_sorted_refnames(&repository, SortKey::Name, false, vec![], Some(5), &options);
    assert_eq!(
        grit_limited,
        git_limited.into_iter().take(5).collect::<Vec<_>>()
    );
}

#[test]
fn ref_query_matches_git_for_each_ref_loose() {
    each_backend(|backend, repo| {
        if backend == Backend::Reftable && !require_reftable_git() {
            return;
        }
        run_query_matrix(repo.worktree(), backend, "loose");
    });
}

#[test]
fn ref_query_matches_git_for_each_ref_packed() {
    let root = tempfile::tempdir().expect("tempdir");
    let worktree = root.path();
    Repository::init(
        worktree,
        &InitOptions::new().with_initial_branch("main".to_owned()),
    )
    .expect("init");
    run_query_matrix(worktree, Backend::Files, "packed");
}

#[test]
fn ref_query_matches_git_for_each_ref_reftable() {
    if !require_reftable_git() {
        return;
    }
    let root = tempfile::tempdir().expect("tempdir");
    let worktree = root.path();
    Repository::init(
        worktree,
        &InitOptions::new()
            .with_initial_branch("main".to_owned())
            .with_ref_storage(grit_lib::refs::ref_storage::RefStorageFormat::Reftable),
    )
    .expect("init");
    run_query_matrix(worktree, Backend::Reftable, "loose");
}

//! Regression: `--grep` on body-only text when walks retain through-subject commit bytes.

use crate::support;

use std::path::Path;
use std::sync::Arc;

use grit_lib::config::environment::{Environment, RepositoryOptions};
use grit_lib::objects::ObjectId;
use grit_lib::repo::Repository;
use grit_lib::revwalk::filter::CommitFilterBuilder;
use grit_lib::revwalk::{CommitExtent, RevWalkListOptions};

use support::{commit_labeled_with_dates_and_message, git, git_rev_list, HistoryRepo};

fn open_grit(wt: &Path) -> Repository {
    let mut env = Environment::empty();
    env.cwd = wt.to_path_buf();
    env.pwd = Some(wt.to_string_lossy().into_owned());
    let options = RepositoryOptions::with_environment(env);
    Repository::discover(Some(wt), &options).expect("discover")
}

fn grit_rev_list_oids(
    repo: &Repository,
    tokens: &[&str],
    opts: &RevWalkListOptions,
) -> Vec<ObjectId> {
    repo.rev_walk().commits(tokens, opts).expect("grit commits")
}

#[test]
fn grep_body_only_with_through_subject_retention_matches_git() {
    let hist = HistoryRepo::init();
    let wt = hist.worktree();
    let subject = "title line";
    let body_only = "needle-only-in-body lives here";
    let message = format!("{subject}\n\n{body_only}\n");
    let tip = commit_labeled_with_dates_and_message(
        wt,
        "body-grep-tip",
        &[],
        "@1000 +0000",
        "@1000 +0000",
        &message,
    );
    git(wt, &["update-ref", "refs/heads/main", &tip.to_hex()]);

    let repo = open_grit(wt);
    let filter = Arc::new(
        CommitFilterBuilder::new()
            .grep("needle-only-in-body")
            .build()
            .expect("filter"),
    );
    let git_oids = git_rev_list(wt, &["--grep=needle-only-in-body", "main"]);
    let walk = RevWalkListOptions {
        commit_filter: Some(filter),
        retain_commit_data: true,
        retained_commit_extent: CommitExtent::ThroughSubject,
        ..RevWalkListOptions::default()
    };
    let grit_oids = grit_rev_list_oids(&repo, &["main"], &walk);
    assert_eq!(
        grit_oids.iter().map(ObjectId::to_hex).collect::<Vec<_>>(),
        git_oids.iter().map(ObjectId::to_hex).collect::<Vec<_>>(),
        "grep must scan full message when only subject bytes are retained for output"
    );
}

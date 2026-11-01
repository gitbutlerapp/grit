//! `RevWalk::describe` distance matches `git rev-list --count tag..rev`.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use grit_lib::config::environment::RepositoryOptions;
use grit_lib::repo::Repository;
use grit_lib::revwalk::DescribeOptions;

mod support;
use support::{git, git_interop_available, Backend};

#[test]
fn describe_distance_matches_git_rev_list_count() {
    if !git_interop_available(Backend::Files) {
        return;
    }
    let root = tempfile::tempdir().expect("tempdir");
    let wt = root.path();
    git(wt, &["init", "-q"]);
    git(wt, &["commit", "--allow-empty", "-q", "-m", "one"]);
    git(wt, &["commit", "--allow-empty", "-q", "-m", "two"]);
    git(wt, &["commit", "--allow-empty", "-q", "-m", "three"]);
    git(wt, &["tag", "-a", "v0", "-m", "t", "HEAD~2"]);
    git(wt, &["tag", "-a", "v1", "-m", "t", "HEAD~1"]);

    let repo = Repository::open(
        &wt.join(".git"),
        &RepositoryOptions::empty().with_work_tree(wt),
    )
    .expect("open");
    let described = repo
        .rev_walk()
        .describe("HEAD", &DescribeOptions { tags: true, ..Default::default() })
        .expect("describe")
        .expect("some tag");
    let tag_short = described
        .tag
        .as_str()
        .strip_prefix("refs/tags/")
        .unwrap_or(described.tag.as_str());
    let git_count = git(
        wt,
        &[
            "rev-list",
            "--count",
            &format!("{tag_short}..HEAD"),
        ],
    )
    .trim()
    .parse::<usize>()
    .expect("count");
    assert_eq!(described.distance, git_count);
}

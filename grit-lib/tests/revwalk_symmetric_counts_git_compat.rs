//! Symmetric count queries over Git- and grit-written commit graphs.

mod support;

use grit_lib::config::environment::RepositoryOptions;
use grit_lib::objects::{ObjectId, ObjectKind};
use grit_lib::repo::Repository;
use grit_lib::revwalk::{LeftRightCounts, RevWalkListOptions};
use support::{git, HistoryRepo};

fn git_counts(path: &std::path::Path, range: &str) -> LeftRightCounts {
    let output = git(path, &["rev-list", "--left-right", "--count", range]);
    let counts: Vec<usize> = output
        .split_whitespace()
        .map(|s| s.parse().expect("count"))
        .collect();
    assert_eq!(counts.len(), 2);
    LeftRightCounts {
        left: counts[0],
        right: counts[1],
    }
}

#[test]
fn parent_navigation_and_counts_match_git_across_storage() {
    let hist = HistoryRepo::init();
    let wt = hist.worktree();
    let repo = Repository::open(&wt.join(".git"), &RepositoryOptions::empty()).unwrap();
    let tree = repo
        .odb()
        .write_loose_materialize(ObjectKind::Tree, b"")
        .unwrap();
    // Two criss-cross merges have two best common ancestors. The other tips cover
    // disconnected roots, octopus merges, and equal or skewed committer dates.
    let shapes: &[(&[usize], i64)] = &[
        (&[], 100),
        (&[0], 101),
        (&[0], 120),
        (&[1, 2], 70),
        (&[2, 1], 100),
        (&[3], 60),
        (&[4], 300),
        (&[5, 6, 0], 50),
        (&[], 200),
        (&[5], 60),
        (&[6], 1),
        (&[8, 7], 400),
        (&[11, 10], 5),
    ];
    let mut commits = Vec::<ObjectId>::new();
    for (i, (parents, time)) in shapes.iter().enumerate() {
        let parents: Vec<_> = parents.iter().map(|&p| commits[p]).collect();
        let label = format!("node-{i}");
        let oid = if i % 2 == 0 {
            let date = format!("@{time} +0000");
            support::commit_labeled_with_dates(wt, &label, &parents, &date, &date)
        } else {
            let parent_headers: String = parents.iter().map(|p| format!("parent {p}\n")).collect();
            let body = format!(
                "tree {tree}\n{parent_headers}author Test <t@example.com> {time} +0000\n\
                 committer Test <t@example.com> {time} +0000\n\n{label}\n"
            );
            repo.odb()
                .write_loose_materialize(ObjectKind::Commit, body.as_bytes())
                .unwrap()
        };
        git(
            wt,
            &["update-ref", &format!("refs/heads/node-{i}"), &oid.to_hex()],
        );
        commits.push(oid);
    }
    git(wt, &["symbolic-ref", "HEAD", "refs/heads/node-12"]);
    git(
        wt,
        &["tag", "-a", "annotated", "node-5", "-m", "range endpoint"],
    );
    git(wt, &["fsck", "--strict", "--no-dangling"]);

    for storage in ["loose", "packed", "commit-graph"] {
        match storage {
            "packed" => {
                git(wt, &["repack", "-ad"]);
                // Navigation must cross a new loose tip and its packed ancestors before
                // a commit-graph is available.
                let date = "@500 +0000";
                let loose =
                    support::commit_labeled_with_dates(wt, "loose-tip", &[commits[12]], date, date);
                git(wt, &["update-ref", "refs/heads/loose-tip", &loose.to_hex()]);
                let repo = Repository::open(&wt.join(".git"), &RepositoryOptions::empty()).unwrap();
                for expression in ["loose-tip~3", "loose-tip^1^2", "annotated~2"] {
                    let oid = repo
                        .odb()
                        .with_pack_read_context(|| repo.rev_walk().resolve(expression))
                        .unwrap();
                    assert_eq!(oid.to_hex(), git(wt, &["rev-parse", expression]).trim());
                }
            }
            "commit-graph" => {
                git(wt, &["commit-graph", "write", "--reachable"]);
            }
            _ => {}
        }
        for expression in [
            "node-5~3",
            "node-6~3",
            "node-7^2~2",
            "node-11^1",
            "node-12^2~2",
            "annotated~2",
        ] {
            let repo = Repository::open(&wt.join(".git"), &RepositoryOptions::empty()).unwrap();
            let oid = repo
                .odb()
                .with_pack_read_context(|| repo.rev_walk().resolve(expression))
                .unwrap();
            assert_eq!(
                oid.to_hex(),
                git(wt, &["rev-parse", expression]).trim(),
                "{storage}: {expression}"
            );
        }
        for range in [
            "...",
            "node-6...",
            "...node-6",
            "annotated...node-6",
            "node-6...annotated",
            "node-12~2...node-12",
            "node-12...node-12~2",
            "node-11^2~1...node-6",
        ] {
            let repo = Repository::open(&wt.join(".git"), &RepositoryOptions::empty()).unwrap();
            assert_eq!(
                repo.odb()
                    .with_pack_read_context(|| {
                        repo.rev_walk()
                            .left_right_counts(&[range], &RevWalkListOptions::default())
                    })
                    .unwrap(),
                git_counts(wt, range),
                "{storage}: {range}"
            );
        }
    }
}

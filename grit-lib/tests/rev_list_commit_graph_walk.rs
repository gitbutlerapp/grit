//! `rev_list` with `use_commit_graph` matches object reads and uses the graph file.
//!
//! Full-history walks answer parents and dates from a commit-graph written by system `git`,
//! without reading commit objects (issue #1745).

mod support;

use std::fs;
use std::path::Path;
use std::process::Command;

use grit_lib::index::write_tree::write_tree_from_index;
use grit_lib::index::{Index, IndexEntry, MODE_REGULAR};
use grit_lib::objects::{serialize_commit, CommitData, ObjectId, ObjectKind};
use grit_lib::refs::write_ref;
use grit_lib::repo::{InitOptions, Repository};
use grit_lib::revwalk::rev_list::{rev_list, OrderingMode, RevListOptions};
use support::{commit_labeled_with_dates, git, git_rev_list, open_grit_repo, HistoryRepo};
use tempfile::TempDir;

fn write_linear_commits(repo: &Repository, count: usize) -> grit_lib::objects::ObjectId {
    let mut index = Index::new();
    index.hash_algo = repo.odb().hash_algo();
    let blob = repo.odb().write(ObjectKind::Blob, b"seed\n").expect("blob");
    index.add_or_replace(IndexEntry {
        ctime_sec: 0,
        ctime_nsec: 0,
        mtime_sec: 0,
        mtime_nsec: 0,
        dev: 0,
        ino: 0,
        mode: MODE_REGULAR,
        uid: 0,
        gid: 0,
        size: 5,
        oid: blob,
        flags: 4,
        flags_extended: None,
        path: b"f.txt".to_vec(),
        base_index_pos: 0,
    });
    let mut parent = None;
    let mut tip = blob;
    for i in 0..count {
        let tree = write_tree_from_index(repo.odb(), &index, "").expect("tree");
        let ts = 1_700_000_000i64 + i as i64;
        let ident = format!("T <t@example.com> {ts} +0000");
        let raw = serialize_commit(&CommitData {
            tree,
            parents: parent.into_iter().collect(),
            author: ident.clone(),
            committer: ident,
            author_raw: Vec::new(),
            committer_raw: Vec::new(),
            encoding: None,
            message: format!("c{i}\n"),
            raw_message: None,
            extra_headers: Vec::new(),
        });
        tip = repo.odb().write(ObjectKind::Commit, &raw).expect("commit");
        parent = Some(tip);
    }
    write_ref(repo.git_dir(), "refs/heads/main", &tip).expect("ref");
    tip
}

fn git_commit_graph_write(repo_root: &tempfile::TempDir) {
    let out = Command::new("git")
        .current_dir(repo_root.path())
        .args(["commit-graph", "write", "--reachable"])
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .output()
        .expect("spawn git");
    assert!(
        out.status.success(),
        "git commit-graph write: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

#[test]
fn rev_list_commit_graph_walk_matches_object_walk() {
    let dir = TempDir::new().expect("tempdir");
    let repo = Repository::init(
        dir.path(),
        &InitOptions::new()
            .with_initial_branch("main".to_owned())
            .with_ref_storage(grit_lib::RefStorageFormat::Files),
    )
    .expect("init");
    write_linear_commits(&repo, 80);
    git_commit_graph_write(&dir);
    assert!(dir.path().join(".git/objects/info/commit-graph").is_file());

    let opts_base = RevListOptions {
        count: true,
        use_commit_graph: false,
        ..Default::default()
    };
    let without = rev_list(&repo, &["HEAD".to_owned()], &[], &opts_base).expect("walk");
    let with_graph = RevListOptions {
        count: true,
        ..Default::default()
    };
    let with = rev_list(&repo, &["HEAD".to_owned()], &[], &with_graph).expect("graph walk");
    assert_eq!(without.commits.len(), 80);
    assert_eq!(without.commit_count(), with.commit_count());
    assert_eq!(with.commit_count(), 80);
}

/// A merge-heavy history whose side branches interleave by committer date, so default (date)
/// order, topo order and first-parent walks all produce different sequences. Returns every
/// commit, tip last.
fn build_history(worktree: &Path) -> Vec<ObjectId> {
    let mut all = Vec::new();
    let mut commit = |label: &str, parents: &[ObjectId], date: i64| {
        let oid = commit_labeled_with_dates(
            worktree,
            label,
            parents,
            &date.to_string(),
            &date.to_string(),
        );
        all.push(oid);
        oid
    };
    let mut main = commit("root", &[], 1_700_000_000);
    for round in 0..6i64 {
        let base = 1_700_000_000 + round * 1_000;
        let side_a = commit(&format!("a{round}"), &[main], base + 500);
        let side_b = commit(&format!("b{round}"), &[side_a], base + 550);
        let other = commit(&format!("o{round}"), &[main], base + 300);
        let mid = commit(&format!("m{round}"), &[other, side_b], base + 600);
        main = commit(&format!("t{round}"), &[mid, side_a], base + 700);
    }
    git(worktree, &["update-ref", "refs/heads/main", &main.to_hex()]);
    git(worktree, &["symbolic-ref", "HEAD", "refs/heads/main"]);
    all
}

fn walk_cases() -> [(&'static str, RevListOptions, &'static [&'static str]); 3] {
    [
        ("default", RevListOptions::default(), &[]),
        (
            "topo",
            RevListOptions {
                ordering: OrderingMode::Topo,
                ..Default::default()
            },
            &["--topo-order"],
        ),
        (
            "first-parent",
            RevListOptions {
                first_parent: true,
                ..Default::default()
            },
            &["--first-parent"],
        ),
    ]
}

fn list(
    repo: &grit_lib::repo::Repository,
    options: &RevListOptions,
    use_graph: bool,
) -> Vec<ObjectId> {
    let options = RevListOptions {
        use_commit_graph: use_graph,
        ..options.clone()
    };
    rev_list(repo, &["HEAD".to_owned()], &[], &options)
        .expect("rev_list")
        .commits
}

#[test]
fn commit_graph_walk_matches_object_walk_and_git() {
    let history = HistoryRepo::init();
    let worktree = history.worktree();
    build_history(worktree);
    git(worktree, &["commit-graph", "write", "--reachable"]);
    let repo = open_grit_repo(worktree);

    for (name, options, git_args) in walk_cases() {
        let mut args = git_args.to_vec();
        args.push("HEAD");
        let expected = git_rev_list(worktree, &args);
        assert_eq!(
            list(&repo, &options, false),
            expected,
            "{name} without graph"
        );
        assert_eq!(list(&repo, &options, true), expected, "{name} with graph");
    }
}

#[test]
fn commit_graph_walk_does_not_read_commit_objects() {
    let history = HistoryRepo::init();
    let worktree = history.worktree();
    let all = build_history(worktree);
    git(worktree, &["commit-graph", "write", "--reachable"]);
    let repo = open_grit_repo(worktree);
    let expected: Vec<Vec<ObjectId>> = walk_cases()
        .iter()
        .map(|(_, options, _)| list(&repo, options, false))
        .collect();

    // Every commit except the tip (which revision parsing peels) now exists only in the graph.
    let tip = *all.last().expect("tip");
    let objects = worktree.join(".git/objects");
    for oid in all.iter().filter(|oid| **oid != tip) {
        let hex = oid.to_hex();
        fs::remove_file(objects.join(&hex[..2]).join(&hex[2..])).expect("remove loose commit");
    }
    let repo = open_grit_repo(worktree);

    for ((name, options, _), expected) in walk_cases().iter().zip(expected) {
        assert_eq!(list(&repo, options, true), expected, "{name}");
    }
}

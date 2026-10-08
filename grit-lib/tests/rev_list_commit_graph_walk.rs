//! `rev_list` with `use_commit_graph` matches object reads and uses the graph file.

use std::process::Command;

use grit_lib::index::{Index, IndexEntry, MODE_REGULAR};
use grit_lib::objects::{serialize_commit, CommitData, ObjectKind};
use grit_lib::refs::write_ref;
use grit_lib::repo::{init_repository, Repository};
use grit_lib::rev_list::{rev_list, RevListOptions};
use grit_lib::write_tree::write_tree_from_index;
use tempfile::TempDir;

fn write_linear_commits(repo: &Repository, count: usize) -> grit_lib::objects::ObjectId {
    let mut index = Index::new();
    index.hash_algo = repo.odb.hash_algo();
    let blob = repo.odb.write(ObjectKind::Blob, b"seed\n").expect("blob");
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
        let tree = write_tree_from_index(&repo.odb, &index, "").expect("tree");
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
        tip = repo.odb.write(ObjectKind::Commit, &raw).expect("commit");
        parent = Some(tip);
    }
    write_ref(&repo.git_dir, "refs/heads/main", &tip).expect("ref");
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
    let repo = init_repository(dir.path(), false, "main", None, "files").expect("init");
    write_linear_commits(&repo, 80);
    git_commit_graph_write(&dir);
    assert!(dir.path().join(".git/objects/info/commit-graph").is_file());

    let opts_base = RevListOptions {
        count: true,
        ..Default::default()
    };
    let without = rev_list(&repo, &["HEAD".to_owned()], &[], &opts_base).expect("walk");
    let mut with_graph = opts_base.clone();
    with_graph.use_commit_graph = true;
    let with = rev_list(&repo, &["HEAD".to_owned()], &[], &with_graph).expect("graph walk");
    assert_eq!(without.commits, with.commits);
    assert_eq!(without.commits.len(), 80);
}

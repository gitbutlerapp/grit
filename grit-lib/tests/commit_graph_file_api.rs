//! Additional commit-graph file parsing and Bloom API coverage.

use std::path::Path;
use std::process::Command;

use grit_lib::commit_graph_file::{parse_graph_file, CommitGraphChain, CommitGraphLayer};
use grit_lib::objects::ObjectId;
use grit_lib::repo::Repository;

fn git_cmd(dir: &Path, args: &[&str]) {
    let out = Command::new("git")
        .current_dir(dir)
        .args(args)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .env("GIT_AUTHOR_NAME", "T")
        .env("GIT_AUTHOR_EMAIL", "t@example.com")
        .env("GIT_COMMITTER_NAME", "T")
        .env("GIT_COMMITTER_EMAIL", "t@example.com")
        .output()
        .expect("git");
    assert!(out.status.success(), "git {args:?}");
}

#[test]
fn parse_graph_dump_and_layer_reload() {
    let dir = tempfile::tempdir().expect("tempdir");
    git_cmd(dir.path(), &["init", "-q", "-b", "main"]);
    std::fs::write(dir.path().join("a.txt"), b"a\n").unwrap();
    git_cmd(dir.path(), &["add", "a.txt"]);
    git_cmd(dir.path(), &["commit", "-q", "-m", "one"]);
    git_cmd(
        dir.path(),
        &["commit-graph", "write", "--reachable", "--changed-paths"],
    );
    let path = dir.path().join(".git/objects/info/commit-graph");
    let dump = parse_graph_file(&path).expect("parse_graph_file");
    assert!(dump.chunks.contains("oid_fanout"));
    let raw = std::fs::read(&path).expect("read");
    let _layer = CommitGraphLayer::try_parse(path.clone(), raw).expect("try_parse");
    assert!(dump.num_commits >= 1);
}

#[test]
fn incompatible_bloom_settings_disable_lower_layer() {
    let dir = tempfile::tempdir().expect("tempdir");
    git_cmd(dir.path(), &["init", "-q", "-b", "main"]);
    for i in 0..3 {
        std::fs::write(dir.path().join(format!("f{i}.txt")), format!("{i}\n")).unwrap();
        git_cmd(dir.path(), &["add", &format!("f{i}.txt")]);
        git_cmd(dir.path(), &["commit", "-q", "-m", &format!("c{i}")]);
    }
    git_cmd(
        dir.path(),
        &[
            "commit-graph",
            "write",
            "--reachable",
            "--changed-paths",
            "--split",
        ],
    );
    std::fs::write(dir.path().join("g.txt"), b"g\n").unwrap();
    git_cmd(dir.path(), &["add", "g.txt"]);
    git_cmd(dir.path(), &["commit", "-q", "-m", "tip"]);
    git_cmd(
        dir.path(),
        &[
            "-c",
            "commitGraph.changedPathsVersion=2",
            "commit-graph",
            "write",
            "--reachable",
            "--changed-paths",
            "--split",
        ],
    );
    let repo = Repository::discover(Some(dir.path())).expect("open");
    let chain = CommitGraphChain::load(repo.odb.objects_dir()).expect("chain");
    assert!(chain.num_layers() >= 2);
    let tip: ObjectId = chain.all_oids_in_order().into_iter().last().expect("tip");
    let pre = chain.bloom_precheck_for_paths(&repo.odb, tip, &["g.txt".to_owned()], None, -1, true);
    assert!(pre.is_ok());
}

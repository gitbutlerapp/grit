//! Calls commit-graph chain/layer public APIs on a split, Bloom-enabled repository.

use std::path::Path;
use std::process::Command;

use grit_lib::bloom::BloomFilterSettings;
use grit_lib::commit_graph_file::{
    diff_changed_paths_for_bloom, dump_bloom_filters, CommitGraphChain,
};
use grit_lib::commit_graph_write::load_commit_graph_commit_info;
use grit_lib::objects::parse_commit;
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

fn merge_repo(root: &Path) {
    git_cmd(root, &["init", "-q", "-b", "main"]);
    for i in 1..=3 {
        std::fs::write(root.join(format!("f{i}.txt")), format!("{i}\n")).unwrap();
        git_cmd(root, &["add", &format!("f{i}.txt")]);
        git_cmd(root, &["commit", "-q", "-m", &format!("c{i}")]);
        git_cmd(root, &["branch", &format!("commits/{i}")]);
    }
    git_cmd(root, &["reset", "--hard", "commits/1"]);
    for i in 4..=5 {
        std::fs::write(root.join(format!("f{i}.txt")), format!("{i}\n")).unwrap();
        git_cmd(root, &["add", &format!("f{i}.txt")]);
        git_cmd(root, &["commit", "-q", "-m", &format!("c{i}")]);
        git_cmd(root, &["branch", &format!("commits/{i}")]);
    }
    git_cmd(root, &["reset", "--hard", "commits/2"]);
    git_cmd(root, &["merge", "-q", "-m", "m", "commits/4"]);
    git_cmd(root, &["repack", "-ad"]);
}

#[test]
fn chain_accessors_and_bloom_helpers_on_split_graph() {
    let dir = tempfile::tempdir().expect("tempdir");
    merge_repo(dir.path());
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
    std::fs::write(dir.path().join("z.txt"), b"z\n").unwrap();
    git_cmd(dir.path(), &["add", "z.txt"]);
    git_cmd(dir.path(), &["commit", "-q", "-m", "z"]);
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
    let repo = Repository::discover(Some(dir.path())).expect("open");
    let chain = CommitGraphChain::load(repo.odb.objects_dir()).expect("chain");
    let _ = chain.top_layer_bloom_settings();
    let _ = chain.total_commits();
    let _ = chain.layer_paths_oldest_first();
    let _ = chain.num_layers();
    let _ = chain.layer_commit_counts_tip_first();
    let _ = chain.layer_has_generation_data_tip_first();
    let _ = chain.layer_hashes_tip_first();
    let _ = chain.layer_object_dirs_tip_first();
    let _ = chain.sub_chain_tip_first(0, chain.num_layers());
    for layer in 0..chain.num_layers() {
        let _ = chain.layer_oids(layer);
    }
    let settings = BloomFilterSettings::default();
    let graph_path = dir.path().join(".git/objects/info/commit-graphs");
    for path in chain.layer_paths_oldest_first() {
        let _ = dump_bloom_filters(&path);
        let _ = grit_lib::commit_graph_file::parse_graph_file(&path);
    }
    for oid in chain.all_oids_in_order() {
        let _ = chain.find_commit(&oid);
        let _ = chain.global_position(&oid);
        let _ = chain.existing_filter_bytes(&oid, &settings);
        let _ = chain.upgradable_filter_bytes(
            &oid,
            &BloomFilterSettings {
                hash_version: 2,
                ..settings
            },
        );
        if let Some((parents, time)) = chain.graph_commit(&oid) {
            let obj = repo.odb.read(&oid).expect("read");
            let c = parse_commit(&obj.data).expect("parse");
            if c.parents.len() <= 2 {
                assert_eq!(c.parents, parents);
            }
            let info = load_commit_graph_commit_info(&repo.odb, oid).expect("info");
            if let Some(p) = c.parents.first() {
                let ptree = load_commit_graph_commit_info(&repo.odb, *p)
                    .expect("pinfo")
                    .tree;
                let _ = diff_changed_paths_for_bloom(&repo.odb, Some(ptree), info.tree);
            }
            let _ = chain.bloom_precheck_for_paths(
                &repo.odb,
                oid,
                &["z.txt".to_owned(), ":exclude:missing".to_owned()],
                None,
                -1,
                true,
            );
            let _ = time;
        }
    }
    let _ = graph_path;
}

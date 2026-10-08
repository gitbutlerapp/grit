//! Exercises commit-graph write helpers and chain-base writes for coverage.

use std::collections::HashMap;
use std::path::Path;
use std::process::Command;

use grit_lib::bloom::BloomFilterSettings;
use grit_lib::commit_graph_file::CommitGraphChain;
use grit_lib::commit_graph_write::{
    build_commit_graph_bytes, collect_reachable_commit_oids, count_referenced_commit_tips,
    load_commit_graph_commit_info,
};
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

fn linear_repo(n: usize) -> (tempfile::TempDir, Repository) {
    let dir = tempfile::tempdir().expect("tempdir");
    git_cmd(dir.path(), &["init", "-q", "-b", "main"]);
    for i in 0..n {
        std::fs::write(dir.path().join(format!("f{i}.txt")), format!("{i}\n")).unwrap();
        git_cmd(dir.path(), &["add", &format!("f{i}.txt")]);
        git_cmd(dir.path(), &["commit", "-q", "-m", &format!("c{i}")]);
    }
    let repo = Repository::discover(Some(dir.path())).expect("open");
    (dir, repo)
}

#[test]
fn build_commit_graph_with_base_chain_and_filter_reuse() {
    let (dir, repo) = linear_repo(5);
    let mut sorted: Vec<ObjectId> = collect_reachable_commit_oids(&repo.git_dir, &repo.odb)
        .expect("commits")
        .into_iter()
        .collect();
    sorted.sort();
    let mut infos = HashMap::new();
    for oid in &sorted {
        infos.insert(
            *oid,
            load_commit_graph_commit_info(&repo.odb, *oid).expect("info"),
        );
    }
    let bloom = BloomFilterSettings::default();
    let (base_bytes, _) = build_commit_graph_bytes(
        &sorted,
        &infos,
        &repo.odb,
        true,
        &bloom,
        None,
        &[],
        None,
        &HashMap::new(),
        &HashMap::new(),
        true,
    )
    .expect("base graph");
    let info = repo.git_dir.join("objects/info");
    std::fs::create_dir_all(&info).expect("info");
    std::fs::write(info.join("commit-graph"), base_bytes).expect("write base");

    let chain = CommitGraphChain::load(repo.odb.objects_dir()).expect("chain");
    let mut existing = HashMap::new();
    for oid in &sorted {
        if let Some(bytes) = chain.existing_filter_bytes(oid, &bloom) {
            existing.insert(*oid, bytes);
        }
    }
    let tips = count_referenced_commit_tips(&repo.git_dir, &repo.odb).expect("tips");
    assert!(tips >= 1);

    std::fs::write(dir.path().join("extra.txt"), b"x\n").unwrap();
    git_cmd(dir.path(), &["add", "extra.txt"]);
    git_cmd(dir.path(), &["commit", "-q", "-m", "extra tip"]);
    sorted = collect_reachable_commit_oids(&repo.git_dir, &repo.odb)
        .expect("commits")
        .into_iter()
        .collect();
    sorted.sort();
    infos.clear();
    for oid in &sorted {
        infos.insert(
            *oid,
            load_commit_graph_commit_info(&repo.odb, *oid).expect("info"),
        );
    }
    let (split_bytes, stats) = build_commit_graph_bytes(
        &sorted[sorted.len() - 1..].to_vec(),
        &infos,
        &repo.odb,
        true,
        &bloom,
        Some(&chain),
        &[],
        Some(0),
        &existing,
        &HashMap::new(),
        true,
    )
    .expect("split layer write");
    assert!(stats.filter_not_computed > 0 || stats.filter_computed == 0);
    assert!(!split_bytes.is_empty());
}

#[test]
fn build_without_generation_or_bloom_chunks() {
    let (_dir, repo) = linear_repo(2);
    let mut sorted: Vec<ObjectId> = collect_reachable_commit_oids(&repo.git_dir, &repo.odb)
        .expect("commits")
        .into_iter()
        .collect();
    sorted.sort();
    let mut infos = HashMap::new();
    for oid in &sorted {
        infos.insert(
            *oid,
            load_commit_graph_commit_info(&repo.odb, *oid).expect("info"),
        );
    }
    let bloom = BloomFilterSettings::default();
    let (bytes, _) = build_commit_graph_bytes(
        &sorted,
        &infos,
        &repo.odb,
        false,
        &bloom,
        None,
        &[],
        None,
        &HashMap::new(),
        &HashMap::new(),
        false,
    )
    .expect("minimal graph");
    assert!(bytes.windows(4).any(|w| w == b"CGPH"));
}

#[test]
fn build_reuses_upgraded_bloom_filter_bytes() {
    let (dir, repo) = linear_repo(2);
    git_cmd(
        dir.path(),
        &["commit-graph", "write", "--reachable", "--changed-paths"],
    );
    let chain = CommitGraphChain::load(repo.odb.objects_dir()).expect("chain");
    let bloom = BloomFilterSettings::default();
    let v2 = BloomFilterSettings {
        hash_version: 2,
        ..bloom
    };
    let oid = chain.all_oids_in_order().into_iter().last().expect("tip");
    let upgraded = chain
        .upgradable_filter_bytes(&oid, &v2)
        .expect("upgradable filter");
    let mut upgraded_map = HashMap::new();
    upgraded_map.insert(oid, upgraded);
    let mut sorted: Vec<ObjectId> = collect_reachable_commit_oids(&repo.git_dir, &repo.odb)
        .expect("commits")
        .into_iter()
        .collect();
    sorted.sort();
    let mut infos = HashMap::new();
    for o in &sorted {
        infos.insert(
            *o,
            load_commit_graph_commit_info(&repo.odb, *o).expect("info"),
        );
    }
    let (_bytes, stats) = build_commit_graph_bytes(
        &sorted,
        &infos,
        &repo.odb,
        true,
        &v2,
        None,
        &[],
        None,
        &HashMap::new(),
        &upgraded_map,
        true,
    )
    .expect("upgrade write");
    assert!(stats.filter_upgraded > 0 || stats.filter_computed > 0);
}

#[test]
fn build_includes_base_graph_hash_chunk() {
    let (dir, repo) = linear_repo(2);
    git_cmd(
        dir.path(),
        &["commit-graph", "write", "--reachable", "--split"],
    );
    let chain = CommitGraphChain::load(repo.odb.objects_dir()).expect("chain");
    let layer_hashes = chain.layer_hashes_tip_first();
    let base_hash = layer_hashes.last().expect("base hash");
    let hash_bytes: Vec<u8> = (0..base_hash.len() / 2)
        .map(|i| u8::from_str_radix(&base_hash[i * 2..i * 2 + 2], 16).expect("hex"))
        .collect();
    let mut sorted: Vec<ObjectId> = collect_reachable_commit_oids(&repo.git_dir, &repo.odb)
        .expect("commits")
        .into_iter()
        .collect();
    sorted.sort();
    let mut infos = HashMap::new();
    for oid in &sorted {
        infos.insert(
            *oid,
            load_commit_graph_commit_info(&repo.odb, *oid).expect("info"),
        );
    }
    let bloom = BloomFilterSettings::default();
    let tip_only = sorted[sorted.len() - 1..].to_vec();
    let (bytes, _) = build_commit_graph_bytes(
        &tip_only,
        &infos,
        &repo.odb,
        false,
        &bloom,
        Some(&chain),
        &[&hash_bytes],
        None,
        &HashMap::new(),
        &HashMap::new(),
        true,
    )
    .expect("base chunk write");
    assert!(bytes.len() > 80);
}

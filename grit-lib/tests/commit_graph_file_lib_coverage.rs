//! Exercises commit-graph file APIs for line coverage (t5318/t5324 helpers).

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Command;

use grit_lib::bloom::BloomFilterSettings;
use grit_lib::commit_graph_file::{
    commit_tree_has_high_bit_paths, dump_bloom_filters, parse_graph_file, BloomPrecheck,
    BloomWalkStats, CommitGraphChain, CommitGraphLayer,
};
use grit_lib::commit_graph_write::{build_commit_graph_bytes, load_commit_graph_commit_info};
use grit_lib::error::Error;
use grit_lib::index::{Index, IndexEntry, MODE_REGULAR};
use grit_lib::objects::{serialize_commit, CommitData, HashAlgo, ObjectId, ObjectKind};
use grit_lib::repo::{init_repository, Repository};
use grit_lib::repo_caches::RepoCaches;
use grit_lib::write_tree::write_tree_from_index;

fn one_commit_repo() -> (tempfile::TempDir, Repository, ObjectId) {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = init_repository(dir.path(), false, "main", None, "files").expect("init");
    let mut index = Index::new();
    index.hash_algo = repo.odb.hash_algo();
    let blob = repo.odb.write(ObjectKind::Blob, b"x\n").expect("blob");
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
        size: 2,
        oid: blob,
        flags: 4,
        flags_extended: None,
        path: b"f.txt".to_vec(),
        base_index_pos: 0,
    });
    let tree = write_tree_from_index(&repo.odb, &index, "").expect("tree");
    let raw = serialize_commit(&CommitData {
        tree,
        parents: vec![],
        author: "T <t@example.com> 100 +0000".into(),
        committer: "T <t@example.com> 100 +0000".into(),
        author_raw: Vec::new(),
        committer_raw: Vec::new(),
        encoding: None,
        message: "m\n".into(),
        raw_message: None,
    });
    let commit = repo.odb.write(ObjectKind::Commit, &raw).expect("commit");
    grit_lib::refs::write_ref(&repo.git_dir, "refs/heads/main", &commit).expect("ref");
    (dir, repo, commit)
}

fn git_available() -> bool {
    Command::new("git")
        .arg("--version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

fn make_writable(path: &Path) {
    let mut perms = std::fs::metadata(path).expect("meta").permissions();
    perms.set_readonly(false);
    std::fs::set_permissions(path, perms).expect("chmod");
}

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

fn octopus_merge_graph() -> (tempfile::TempDir, Repository, ObjectId, Vec<ObjectId>) {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = init_repository(dir.path(), false, "main", None, "files").expect("init");
    let mk = |parents: Vec<ObjectId>, t: i64| -> ObjectId {
        let tree = repo.odb.write(ObjectKind::Tree, b"").expect("tree");
        let raw = serialize_commit(&CommitData {
            tree,
            parents,
            author: format!("T <t@example.com> {t} +0000"),
            committer: format!("T <t@example.com> {t} +0000"),
            author_raw: Vec::new(),
            committer_raw: Vec::new(),
            encoding: None,
            message: "m\n".into(),
            raw_message: None,
        });
        repo.odb.write(ObjectKind::Commit, &raw).expect("commit")
    };
    let p1 = mk(vec![], 1);
    let p2 = mk(vec![], 2);
    let p3 = mk(vec![], 3);
    let merge = mk(vec![p1, p2, p3], 4);
    let mut sorted = vec![p1, p2, p3, merge];
    sorted.sort();
    (dir, repo, merge, sorted)
}

#[test]
fn octopus_graph_commit_returns_none_and_validates_edges() {
    let (_dir, repo, merge, sorted) = octopus_merge_graph();
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
        true,
    )
    .expect("write");
    std::fs::write(repo.odb.objects_dir().join("info/commit-graph"), &bytes).expect("write");
    let chain = CommitGraphChain::load(repo.odb.objects_dir()).expect("load");
    assert!(chain.graph_commit(&merge).is_none());
    let gp = chain.global_position(&merge).expect("pos");
    assert_eq!(chain.find_commit(&merge), Some((0, gp)));
}

#[test]
fn bloom_precheck_definitely_not_for_missing_path() {
    let (_dir, repo, commit) = one_commit_repo();
    let info = load_commit_graph_commit_info(&repo.odb, commit).expect("info");
    let mut infos = HashMap::new();
    infos.insert(commit, info);
    let bloom = BloomFilterSettings::default();
    let (bytes, _) = build_commit_graph_bytes(
        &[commit],
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
    .expect("write");
    std::fs::write(repo.odb.objects_dir().join("info/commit-graph"), bytes).expect("write");
    let chain = CommitGraphChain::load(repo.odb.objects_dir()).expect("chain");
    let pre = chain
        .bloom_precheck_for_paths(
            &repo.odb,
            commit,
            &["no/such/path.txt".to_owned()],
            None,
            -1,
            true,
        )
        .expect("precheck");
    assert_eq!(pre, BloomPrecheck::DefinitelyNot);
}

#[test]
fn bloom_walk_stats_counters() {
    let mut stats = BloomWalkStats::default();
    stats.record_precheck(BloomPrecheck::Maybe);
    stats.record_precheck(BloomPrecheck::DefinitelyNot);
    stats.record_precheck(BloomPrecheck::FilterNotPresent);
    stats.record_false_positive();
    assert_eq!(stats.maybe, 1);
    assert_eq!(stats.definitely_not, 1);
    assert_eq!(stats.filter_not_present, 1);
    assert_eq!(stats.false_positive, 1);
}

#[test]
fn commit_tree_high_bit_and_parse_graph_dump() {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = init_repository(dir.path(), false, "main", None, "files").expect("init");
    let mut index = Index::new();
    index.hash_algo = repo.odb.hash_algo();
    let blob = repo.odb.write(ObjectKind::Blob, b"x\n").expect("blob");
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
        size: 2,
        oid: blob,
        flags: 4,
        flags_extended: None,
        path: vec![0x80, b'x'],
        base_index_pos: 0,
    });
    let tree = write_tree_from_index(&repo.odb, &index, "").expect("tree");
    let raw = serialize_commit(&CommitData {
        tree,
        parents: vec![],
        author: "T <t@example.com> 1 +0000".into(),
        committer: "T <t@example.com> 1 +0000".into(),
        author_raw: Vec::new(),
        committer_raw: Vec::new(),
        encoding: None,
        message: "m\n".into(),
        raw_message: None,
    });
    let commit = repo.odb.write(ObjectKind::Commit, &raw).expect("commit");
    assert!(commit_tree_has_high_bit_paths(&repo.odb, commit));
    let info = load_commit_graph_commit_info(&repo.odb, commit).expect("info");
    let mut infos = HashMap::new();
    infos.insert(commit, info);
    let bloom = BloomFilterSettings::default();
    let (bytes, _) = build_commit_graph_bytes(
        &[commit],
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
    .expect("write");
    let path = dir.path().join("g.graph");
    std::fs::write(&path, bytes).expect("write");
    let dump = parse_graph_file(&path).expect("dump");
    assert!(dump.chunks.contains("bloom_data"));
    let lines = dump_bloom_filters(&path).expect("hex");
    assert_eq!(lines.len(), 1);
}

#[test]
fn try_load_with_caches_and_sub_chain() {
    let (_dir, repo, commit) = one_commit_repo();
    let info = load_commit_graph_commit_info(&repo.odb, commit).expect("info");
    let mut infos = HashMap::new();
    infos.insert(commit, info);
    let bloom = BloomFilterSettings::default();
    let (bytes, _) = build_commit_graph_bytes(
        &[commit],
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
    .expect("write");
    std::fs::write(repo.odb.objects_dir().join("info/commit-graph"), bytes).expect("write");
    let objects = repo.odb.objects_dir();
    let caches = RepoCaches::new();
    let first = CommitGraphChain::try_load_with_caches(objects, Some(&caches))
        .expect("load")
        .expect("graph");
    let second = CommitGraphChain::try_load_with_caches(objects, Some(&caches))
        .expect("reload")
        .expect("graph again");
    assert_eq!(first.total_commits(), second.total_commits());
    assert!(first.sub_chain_tip_first(0, 1).is_some());
    assert!(first.sub_chain_tip_first(1, 0).is_none());
}

#[test]
fn bloom_filter_slice_warns_on_bad_offsets_in_memory() {
    let (_dir, repo, commit) = one_commit_repo();
    let info = load_commit_graph_commit_info(&repo.odb, commit).expect("info");
    let mut infos = HashMap::new();
    infos.insert(commit, info);
    let bloom = BloomFilterSettings::default();
    let (mut bytes, _) = build_commit_graph_bytes(
        &[commit],
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
    .expect("write");
    let num_chunks = bytes[6] as usize;
    let mut bidx_off = None;
    for i in 0..num_chunks {
        let e = 8 + i * 12;
        let id = u32::from_be_bytes(bytes[e..e + 4].try_into().unwrap());
        if id == 0x4249_4458 {
            bidx_off = Some(u64::from_be_bytes(bytes[e + 4..e + 12].try_into().unwrap()) as usize);
        }
    }
    let off = bidx_off.expect("bidx chunk");
    bytes[off..off + 4].copy_from_slice(&u32::MAX.to_be_bytes());
    let algo = repo.odb.hash_algo();
    let hash_len = algo.len();
    let body_len = bytes.len() - hash_len;
    let digest = algo.digest(&bytes[..body_len]);
    bytes.truncate(body_len);
    bytes.extend_from_slice(digest.as_bytes());
    std::fs::write(repo.odb.objects_dir().join("info/commit-graph"), &bytes).expect("write");
    let chain = CommitGraphChain::load(repo.odb.objects_dir()).expect("load");
    let settings = chain.top_layer_bloom_settings().expect("bloom");
    assert!(chain.existing_filter_bytes(&commit, &settings).is_none());
    let pre = chain
        .bloom_precheck_for_paths(&repo.odb, commit, &["f.txt".to_owned()], None, -1, true)
        .expect("precheck");
    assert_eq!(pre, BloomPrecheck::FilterNotPresent);
}

#[test]
fn two_parent_graph_commit_matches_object() {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = init_repository(dir.path(), false, "main", None, "files").expect("init");
    let tree = repo.odb.write(ObjectKind::Tree, b"").expect("tree");
    let parent = {
        let raw = serialize_commit(&CommitData {
            tree,
            parents: vec![],
            author: "T <t@example.com> 1 +0000".into(),
            committer: "T <t@example.com> 1 +0000".into(),
            author_raw: Vec::new(),
            committer_raw: Vec::new(),
            encoding: None,
            message: "p\n".into(),
            raw_message: None,
        });
        repo.odb.write(ObjectKind::Commit, &raw).expect("p")
    };
    let merge = {
        let raw = serialize_commit(&CommitData {
            tree,
            parents: vec![parent, parent],
            author: "T <t@example.com> 2 +0000".into(),
            committer: "T <t@example.com> 2 +0000".into(),
            author_raw: Vec::new(),
            committer_raw: Vec::new(),
            encoding: None,
            message: "m\n".into(),
            raw_message: None,
        });
        repo.odb.write(ObjectKind::Commit, &raw).expect("m")
    };
    let mut sorted = vec![parent, merge];
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
        true,
    )
    .expect("write");
    std::fs::write(repo.odb.objects_dir().join("info/commit-graph"), bytes).expect("write");
    let chain = CommitGraphChain::load(repo.odb.objects_dir()).expect("chain");
    let (parents, _time) = chain.graph_commit(&merge).expect("graph parents");
    assert_eq!(parents.len(), 2);
}

fn graph_content_hash(bytes: &[u8], algo: HashAlgo) -> String {
    algo.digest(bytes).to_string()
}

fn two_commit_repo() -> (tempfile::TempDir, Repository, Vec<ObjectId>) {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = init_repository(dir.path(), false, "main", None, "files").expect("init");
    let tree = repo.odb.write(ObjectKind::Tree, b"").expect("tree");
    let mut sorted = Vec::new();
    for t in [1i64, 2] {
        let parents = sorted.last().copied().map(|p| vec![p]).unwrap_or_default();
        let raw = serialize_commit(&CommitData {
            tree,
            parents,
            author: format!("T <t@example.com> {t} +0000"),
            committer: format!("T <t@example.com> {t} +0000"),
            author_raw: Vec::new(),
            committer_raw: Vec::new(),
            encoding: None,
            message: "m\n".into(),
            raw_message: None,
        });
        sorted.push(repo.odb.write(ObjectKind::Commit, &raw).expect("c"));
    }
    sorted.sort();
    (dir, repo, sorted)
}

fn reseal_commit_graph(bytes: &mut Vec<u8>, algo: HashAlgo) {
    let hash_len = algo.len();
    let body_len = bytes.len() - hash_len;
    let digest = algo.digest(&bytes[..body_len]);
    bytes.truncate(body_len);
    bytes.extend_from_slice(digest.as_bytes());
}

#[test]
fn split_chain_stops_when_base_chunk_missing_for_stacked_layer() {
    let (_dir, repo, sorted) = two_commit_repo();
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
        false,
        &bloom,
        None,
        &[],
        None,
        &HashMap::new(),
        &HashMap::new(),
        true,
    )
    .expect("base");
    let algo = repo.odb.hash_algo();
    let base_name = graph_content_hash(&base_bytes, algo);
    std::fs::write(
        repo.odb.objects_dir().join("info/commit-graph"),
        &base_bytes,
    )
    .expect("single");
    let base_chain = CommitGraphChain::try_load(repo.odb.objects_dir())
        .expect("load")
        .expect("chain");
    let _ = std::fs::remove_file(repo.odb.objects_dir().join("info/commit-graph"));
    let tip_only = sorted[sorted.len() - 1..].to_vec();
    let (tip_bytes, _) = build_commit_graph_bytes(
        &tip_only,
        &infos,
        &repo.odb,
        false,
        &bloom,
        Some(&base_chain),
        &[],
        None,
        &HashMap::new(),
        &HashMap::new(),
        true,
    )
    .expect("tip without base chunk");
    let tip_name = graph_content_hash(&tip_bytes, algo);
    let graphs = repo.odb.objects_dir().join("info/commit-graphs");
    std::fs::create_dir_all(&graphs).expect("mkdir");
    std::fs::write(graphs.join(format!("graph-{base_name}.graph")), &base_bytes).expect("base");
    std::fs::write(graphs.join(format!("graph-{tip_name}.graph")), &tip_bytes).expect("tip");
    std::fs::write(
        graphs.join("commit-graph-chain"),
        format!("{base_name}\n{tip_name}\n"),
    )
    .expect("chain");
    let caches = RepoCaches::new();
    let chain = CommitGraphChain::try_load_with_caches(repo.odb.objects_dir(), Some(&caches))
        .expect("load")
        .expect("partial chain");
    assert_eq!(chain.num_layers(), 1);
}

#[test]
fn grit_octopus_graph_rejects_invalid_extra_edge_parent() {
    let (_dir, repo, _merge, sorted) = octopus_merge_graph();
    let mut infos = HashMap::new();
    for oid in &sorted {
        infos.insert(
            *oid,
            load_commit_graph_commit_info(&repo.odb, *oid).expect("info"),
        );
    }
    let bloom = BloomFilterSettings::default();
    let (mut bytes, _) = build_commit_graph_bytes(
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
        true,
    )
    .expect("write");
    let num_chunks = bytes[6] as usize;
    let mut edge_off = None;
    for i in 0..num_chunks {
        let e = 8 + i * 12;
        let id = u32::from_be_bytes(bytes[e..e + 4].try_into().unwrap());
        if id == 0x4544_4745 {
            edge_off = Some(u64::from_be_bytes(bytes[e + 4..e + 12].try_into().unwrap()) as usize);
        }
    }
    if let Some(off) = edge_off {
        bytes[off..off + 4].copy_from_slice(&99u32.to_be_bytes());
        reseal_commit_graph(&mut bytes, repo.odb.hash_algo());
        let err = CommitGraphLayer::try_parse(std::path::PathBuf::from("g"), bytes).unwrap_err();
        assert!(matches!(err, Error::CorruptObject(_)));
    }
}

#[test]
fn grit_graph_rejects_invalid_parent_index_in_cdat() {
    let (_dir, repo, sorted) = two_commit_repo();
    let mut infos = HashMap::new();
    for oid in &sorted {
        infos.insert(
            *oid,
            load_commit_graph_commit_info(&repo.odb, *oid).expect("info"),
        );
    }
    let bloom = BloomFilterSettings::default();
    let (mut bytes, _) = build_commit_graph_bytes(
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
        true,
    )
    .expect("write");
    let num_chunks = bytes[6] as usize;
    let mut cdat_off = None;
    for i in 0..num_chunks {
        let e = 8 + i * 12;
        let id = u32::from_be_bytes(bytes[e..e + 4].try_into().unwrap());
        if id == 0x4344_4154 {
            cdat_off = Some(u64::from_be_bytes(bytes[e + 4..e + 12].try_into().unwrap()) as usize);
        }
    }
    let cdat_off = cdat_off.expect("cdat");
    let hash_len = repo.odb.hash_algo().len();
    bytes[cdat_off + hash_len..cdat_off + hash_len + 4].copy_from_slice(&99u32.to_be_bytes());
    reseal_commit_graph(&mut bytes, repo.odb.hash_algo());
    let err = CommitGraphLayer::try_parse(std::path::PathBuf::from("g"), bytes).unwrap_err();
    assert!(matches!(err, Error::CorruptObject(_)));
}

#[test]
fn split_chain_with_different_bloom_versions_disables_base() {
    let (_dir, repo, sorted) = two_commit_repo();
    let mut infos = HashMap::new();
    for oid in &sorted {
        infos.insert(
            *oid,
            load_commit_graph_commit_info(&repo.odb, *oid).expect("info"),
        );
    }
    let v1 = BloomFilterSettings {
        hash_version: 1,
        ..BloomFilterSettings::default()
    };
    let (base_bytes, _) = build_commit_graph_bytes(
        &sorted,
        &infos,
        &repo.odb,
        true,
        &v1,
        None,
        &[],
        None,
        &HashMap::new(),
        &HashMap::new(),
        true,
    )
    .expect("base write");
    let algo = repo.odb.hash_algo();
    let base_name = graph_content_hash(&base_bytes, algo);
    let graphs = repo.odb.objects_dir().join("info/commit-graphs");
    std::fs::create_dir_all(&graphs).expect("mkdir");
    std::fs::write(
        repo.odb.objects_dir().join("info/commit-graph"),
        &base_bytes,
    )
    .expect("single base");
    let base_chain = CommitGraphChain::try_load(repo.odb.objects_dir())
        .expect("load")
        .expect("chain");
    let _ = std::fs::remove_file(repo.odb.objects_dir().join("info/commit-graph"));
    std::fs::write(graphs.join(format!("graph-{base_name}.graph")), &base_bytes).expect("base");
    let hash_bytes: Vec<u8> = (0..base_name.len() / 2)
        .map(|i| u8::from_str_radix(&base_name[i * 2..i * 2 + 2], 16).expect("hex"))
        .collect();
    let v2 = BloomFilterSettings {
        hash_version: 2,
        ..BloomFilterSettings::default()
    };
    let tip_only = sorted[sorted.len() - 1..].to_vec();
    let (tip_bytes, _) = build_commit_graph_bytes(
        &tip_only,
        &infos,
        &repo.odb,
        true,
        &v2,
        Some(&base_chain),
        &[&hash_bytes],
        None,
        &HashMap::new(),
        &HashMap::new(),
        true,
    )
    .expect("tip write");
    let tip_name = graph_content_hash(&tip_bytes, algo);
    std::fs::write(graphs.join(format!("graph-{tip_name}.graph")), &tip_bytes).expect("tip");
    std::fs::write(
        graphs.join("commit-graph-chain"),
        format!("{base_name}\n{tip_name}\n"),
    )
    .expect("chain");
    let caches = RepoCaches::new();
    let chain = CommitGraphChain::try_load_with_caches(repo.odb.objects_dir(), Some(&caches))
        .expect("load split")
        .expect("chain");
    assert_eq!(chain.num_layers(), 2);
    assert_eq!(
        chain.top_layer_bloom_settings().expect("tip").hash_version,
        2
    );
    let base_oid = sorted[0];
    let tip_settings = chain.top_layer_bloom_settings().expect("settings");
    assert!(chain
        .existing_filter_bytes(&base_oid, &tip_settings)
        .is_none());
}

#[test]
fn grit_graph_rejects_unsupported_hash_version() {
    let (_dir, repo, sorted) = two_commit_repo();
    let mut infos = HashMap::new();
    for oid in &sorted {
        infos.insert(
            *oid,
            load_commit_graph_commit_info(&repo.odb, *oid).expect("info"),
        );
    }
    let bloom = BloomFilterSettings::default();
    let (mut bytes, _) = build_commit_graph_bytes(
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
        true,
    )
    .expect("write");
    bytes[5] = 99;
    reseal_commit_graph(&mut bytes, repo.odb.hash_algo());
    let err = CommitGraphLayer::try_parse(std::path::PathBuf::from("g"), bytes).unwrap_err();
    assert!(matches!(err, Error::CorruptObject(_)));
}

#[test]
fn try_load_across_resolves_layers_from_alternate() {
    if !git_available() {
        eprintln!("SKIP: git unavailable");
        return;
    }
    let upstream = tempfile::tempdir().expect("upstream");
    git_cmd(upstream.path(), &["init", "-q", "-b", "main"]);
    git_cmd(
        upstream.path(),
        &["commit", "--allow-empty", "-q", "-m", "u"],
    );
    git_cmd(
        upstream.path(),
        &["commit-graph", "write", "--reachable", "--split"],
    );
    let clone = tempfile::tempdir().expect("clone");
    git_cmd(
        clone.path(),
        &[
            "clone",
            "-q",
            "--reference",
            upstream.path().to_str().expect("utf8"),
            upstream.path().to_str().expect("utf8"),
            ".",
        ],
    );
    git_cmd(
        clone.path(),
        &["commit", "--allow-empty", "-q", "-m", "local"],
    );
    git_cmd(
        clone.path(),
        &["commit-graph", "write", "--reachable", "--split"],
    );
    let objects = clone.path().join(".git/objects");
    let alt_dirs: Vec<PathBuf> = std::fs::read_to_string(objects.join("info/alternates"))
        .ok()
        .map(|s| {
            s.lines()
                .filter(|l| !l.trim().is_empty())
                .map(PathBuf::from)
                .collect()
        })
        .unwrap_or_default();
    let caches = RepoCaches::new();
    let chain = CommitGraphChain::try_load_across_with_caches(&objects, &alt_dirs, Some(&caches))
        .expect("across")
        .expect("chain");
    assert!(chain.num_layers() >= 1);
    let tip = chain.all_oids_in_order().pop().expect("tip");
    let settings = BloomFilterSettings::default();
    assert!(chain.existing_filter_bytes(&tip, &settings).is_none());
    let wrong = BloomFilterSettings {
        hash_version: 99,
        ..settings
    };
    assert!(chain.existing_filter_bytes(&tip, &wrong).is_none());
}

#[test]
fn try_load_across_errors_when_layer_file_missing() {
    if !git_available() {
        eprintln!("SKIP: git unavailable");
        return;
    }
    let dir = tempfile::tempdir().expect("tempdir");
    git_cmd(dir.path(), &["init", "-q", "-b", "main"]);
    git_cmd(dir.path(), &["commit", "--allow-empty", "-q", "-m", "c"]);
    git_cmd(
        dir.path(),
        &["commit-graph", "write", "--reachable", "--split"],
    );
    let graphs = dir.path().join(".git/objects/info/commit-graphs");
    let chain_path = graphs.join("commit-graph-chain");
    let mut content = std::fs::read_to_string(&chain_path).expect("chain");
    content.push_str("0000000000000000000000000000000000000001\n");
    make_writable(&chain_path);
    std::fs::write(&chain_path, content).expect("write chain");
    let objects = dir.path().join(".git/objects");
    let err = CommitGraphChain::try_load_across(&objects, &[]).expect_err("missing layer");
    assert!(matches!(err, grit_lib::error::Error::Io(_)));
}

#[test]
fn try_load_skips_invalid_chain_hash_lines() {
    if !git_available() {
        eprintln!("SKIP: git unavailable");
        return;
    }
    let dir = tempfile::tempdir().expect("tempdir");
    git_cmd(dir.path(), &["init", "-q", "-b", "main"]);
    git_cmd(dir.path(), &["commit", "--allow-empty", "-q", "-m", "c"]);
    git_cmd(
        dir.path(),
        &["commit-graph", "write", "--reachable", "--split"],
    );
    let graphs = dir.path().join(".git/objects/info/commit-graphs");
    let chain_path = graphs.join("commit-graph-chain");
    let content = std::fs::read_to_string(&chain_path).expect("chain");
    let patched = format!("not-a-valid-hash-line\n{content}");
    make_writable(&chain_path);
    std::fs::write(&chain_path, patched).expect("patch chain");
    let objects = dir.path().join(".git/objects");
    let chain = CommitGraphChain::try_load(&objects)
        .expect("load")
        .expect("chain");
    assert!(chain.total_commits() >= 1);
}

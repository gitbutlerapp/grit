//! Commit-graph parse/load corruption handling (t5318/t5324 scenarios).

use std::path::{Path, PathBuf};
use std::process::Command;

use grit_lib::bloom::BloomFilterSettings;
use grit_lib::commit_graph_file::{CommitGraphChain, CommitGraphLayer};
use grit_lib::commit_graph_write::{
    build_commit_graph_bytes, collect_reachable_commit_oids, load_commit_graph_commit_info,
};
use grit_lib::error::Error;
use grit_lib::objects::{HashAlgo, ObjectId};
use grit_lib::repo::Repository;
use grit_lib::rev_list::{rev_list, RevListOptions};
use std::collections::HashMap;

fn git_cmd(dir: &Path, args: &[&str]) -> String {
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
        .expect("spawn git");
    assert!(
        out.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8(out.stdout).expect("utf8 stdout")
}

fn git_available() -> bool {
    Command::new("git")
        .arg("--version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

fn setup_merge_repo(root: &Path) {
    git_cmd(root, &["init", "-q", "-b", "main"]);
    for i in 1..=3 {
        std::fs::write(root.join(format!("f{i}.txt")), format!("b{i}\n")).unwrap();
        git_cmd(root, &["add", &format!("f{i}.txt")]);
        git_cmd(root, &["commit", "-q", "-m", &format!("c{i}")]);
        git_cmd(root, &["branch", &format!("commits/{i}")]);
    }
    git_cmd(root, &["reset", "--hard", "commits/1"]);
    for i in 4..=5 {
        std::fs::write(root.join(format!("f{i}.txt")), format!("b{i}\n")).unwrap();
        git_cmd(root, &["add", &format!("f{i}.txt")]);
        git_cmd(root, &["commit", "-q", "-m", &format!("c{i}")]);
        git_cmd(root, &["branch", &format!("commits/{i}")]);
    }
    git_cmd(root, &["reset", "--hard", "commits/2"]);
    for i in 6..=7 {
        std::fs::write(root.join(format!("f{i}.txt")), format!("b{i}\n")).unwrap();
        git_cmd(root, &["add", &format!("f{i}.txt")]);
        git_cmd(root, &["commit", "-q", "-m", &format!("c{i}")]);
        git_cmd(root, &["branch", &format!("commits/{i}")]);
    }
    git_cmd(root, &["reset", "--hard", "commits/2"]);
    git_cmd(root, &["merge", "-q", "-m", "M1", "commits/4"]);
    git_cmd(root, &["reset", "--hard", "commits/4"]);
    git_cmd(root, &["merge", "-q", "-m", "M2", "commits/6"]);
    git_cmd(root, &["reset", "--hard", "commits/3"]);
    git_cmd(root, &["merge", "-q", "-m", "M3", "commits/5", "commits/7"]);
    git_cmd(root, &["repack", "-ad"]);
}

fn write_verify_fixture_graph(root: &Path) -> PathBuf {
    setup_merge_repo(root);
    git_cmd(
        root,
        &[
            "-c",
            "commitGraph.generationVersion=1",
            "commit-graph",
            "write",
            "--reachable",
        ],
    );
    git_cmd(root, &["commit-graph", "verify"]);
    root.join(".git/objects/info/commit-graph")
}

fn expect_corrupt_parse(path: PathBuf, bytes: Vec<u8>) -> Error {
    let err = CommitGraphLayer::try_parse(path.clone(), bytes.clone()).unwrap_err();
    assert!(
        matches!(err, Error::CorruptObject(_)),
        "expected CorruptObject, got {err:?}"
    );
    err
}

fn make_graph_writable(path: &Path) {
    let mut perms = std::fs::metadata(path).expect("meta").permissions();
    perms.set_readonly(false);
    std::fs::set_permissions(path, perms).expect("chmod graph");
}

fn write_graph_bytes(path: &Path, bytes: &[u8]) {
    make_graph_writable(path);
    std::fs::write(path, bytes).expect("write corrupt graph");
}

fn expect_corrupt_load(objects: &Path, graph_path: &Path, bytes: Vec<u8>) {
    write_graph_bytes(graph_path, &bytes);
    let err = CommitGraphChain::try_load(objects).unwrap_err();
    assert!(
        matches!(err, Error::CorruptObject(_)),
        "try_load: expected CorruptObject, got {err:?}"
    );
}

fn mutate_byte(bytes: &mut [u8], pos: usize, value: u8) {
    bytes[pos] = value;
}

fn reseal_commit_graph(bytes: &mut Vec<u8>) {
    let hash_len = if bytes[5] == 2 { 32 } else { 20 };
    let algo = HashAlgo::try_from(bytes[5]).expect("hash algo");
    let body_len = bytes.len().saturating_sub(hash_len);
    let digest = algo.digest(&bytes[..body_len]);
    bytes.truncate(body_len);
    bytes.extend_from_slice(digest.as_bytes());
}

fn mutate_byte_resealed(bytes: &mut Vec<u8>, pos: usize, value: u8) {
    mutate_byte(bytes, pos, value);
    reseal_commit_graph(bytes);
}

fn toc_entry_offset(bytes: &[u8], chunk_id: u32) -> Option<usize> {
    let num_chunks = bytes[6] as usize;
    for i in 0..num_chunks {
        let e = 8 + i * 12;
        let id = u32::from_be_bytes(bytes[e..e + 4].try_into().ok()?);
        if id == chunk_id {
            return Some(e + 4);
        }
    }
    None
}

#[test]
fn corrupt_graph_signature_version_and_hash_version() {
    if !git_available() {
        eprintln!("SKIP: git unavailable");
        return;
    }
    let dir = tempfile::tempdir().expect("tempdir");
    let path = write_verify_fixture_graph(dir.path());
    let mut bytes = std::fs::read(&path).expect("read graph");
    let backup = bytes.clone();
    mutate_byte(&mut bytes, 0, 0);
    expect_corrupt_parse(path.clone(), bytes);
    bytes = backup.clone();
    mutate_byte(&mut bytes, 4, 2);
    expect_corrupt_parse(path.clone(), bytes);
    bytes = backup;
    mutate_byte(&mut bytes, 5, 3);
    expect_corrupt_parse(path, bytes);
}

#[test]
fn corrupt_graph_chunk_table_and_missing_chunks() {
    if !git_available() {
        eprintln!("SKIP: git unavailable");
        return;
    }
    let dir = tempfile::tempdir().expect("tempdir");
    let path = write_verify_fixture_graph(dir.path());
    let objects = dir.path().join(".git/objects");
    let mut bytes = std::fs::read(&path).expect("read graph");
    let backup = bytes.clone();
    let mut b = bytes.clone();
    mutate_byte_resealed(&mut b, 6, 1);
    expect_corrupt_load(&objects, &path, b);
    bytes = backup.clone();
    let mut b = bytes.clone();
    mutate_byte_resealed(&mut b, 8, 0);
    expect_corrupt_parse(path.clone(), b);
    bytes = backup.clone();
    let mut b = bytes.clone();
    mutate_byte_resealed(&mut b, 20, 0);
    expect_corrupt_parse(path.clone(), b);
    bytes = backup.clone();
    let mut b = bytes.clone();
    mutate_byte_resealed(&mut b, 32, 0);
    expect_corrupt_parse(path.clone(), b);
    let oidf = toc_entry_offset(&bytes, 0x4f49_4446).expect("OIDF toc");
    let mut b = bytes.clone();
    b[oidf..oidf + 8].copy_from_slice(&u64::MAX.to_be_bytes());
    reseal_commit_graph(&mut b);
    expect_corrupt_parse(path, b);
}

#[test]
fn corrupt_graph_fanout_oid_order_and_parent_edges() {
    if !git_available() {
        eprintln!("SKIP: git unavailable");
        return;
    }
    let dir = tempfile::tempdir().expect("tempdir");
    let path = write_verify_fixture_graph(dir.path());
    let objects = dir.path().join(".git/objects");
    let bytes = std::fs::read(&path).expect("read graph");
    let hash_len = if bytes[5] == 2 { 32 } else { 20 };
    let num_chunks = bytes[6] as usize;
    let mut fanout_off = None;
    let mut oid_lookup_off = None;
    let mut commit_data_off = None;
    for i in 0..num_chunks {
        let e = 8 + i * 12;
        let id = u32::from_be_bytes(bytes[e..e + 4].try_into().unwrap());
        let off = u64::from_be_bytes(bytes[e + 4..e + 12].try_into().unwrap()) as usize;
        match id {
            0x4f49_4446 => fanout_off = Some(off),
            0x4f49_444c => oid_lookup_off = Some(off),
            0x4344_4154 => commit_data_off = Some(off),
            _ => {}
        }
    }
    let fanout_off = fanout_off.expect("OIDF");
    let oid_lookup_off = oid_lookup_off.expect("OIDL");
    let commit_data_off = commit_data_off.expect("CDAT");
    let num_commits = u32::from_be_bytes(
        bytes[fanout_off + 255 * 4..fanout_off + 256 * 4]
            .try_into()
            .unwrap(),
    ) as usize;

    let mut b = bytes.clone();
    b[fanout_off..fanout_off + 4]
        .copy_from_slice(&(num_commits as u32).to_be_bytes());
    b[fanout_off + 4..fanout_off + 8].copy_from_slice(&0u32.to_be_bytes());
    reseal_commit_graph(&mut b);
    expect_corrupt_load(&objects, &path, b);

    let mut b = bytes.clone();
    if num_commits >= 2 {
        let a = oid_lookup_off;
        let b_off = oid_lookup_off + hash_len;
        let (left, right) = b.split_at_mut(b_off);
        left[a..a + hash_len].swap_with_slice(&mut right[..hash_len]);
        reseal_commit_graph(&mut b);
        expect_corrupt_parse(path.clone(), b);
    }

    let mut b = bytes.clone();
    let parent_off = commit_data_off + hash_len;
    let invalid_parent = num_commits as u32;
    b[parent_off..parent_off + 4].copy_from_slice(&invalid_parent.to_be_bytes());
    reseal_commit_graph(&mut b);
    expect_corrupt_parse(path.clone(), b);

    let edge_off = commit_data_off + num_commits * (hash_len + 16);
    if edge_off + 4 < bytes.len() - hash_len {
        let mut b = bytes.clone();
        b[edge_off] = 0xff;
        reseal_commit_graph(&mut b);
        expect_corrupt_parse(path, b);
    } else {
        let _ = num_chunks;
    }
}

#[test]
fn corrupt_graph_generation_inconsistency_and_trailer() {
    if !git_available() {
        eprintln!("SKIP: git unavailable");
        return;
    }
    let dir = tempfile::tempdir().expect("tempdir");
    setup_merge_repo(dir.path());
    git_cmd(dir.path(), &["commit-graph", "write", "--reachable"]);
    let path = dir.path().join(".git/objects/info/commit-graph");
    let objects = dir.path().join(".git/objects");
    let mut bytes = std::fs::read(&path).expect("read");
    let hash_len = if bytes[5] == 2 { 32 } else { 20 };
    if bytes.len() < hash_len + 40 {
        return;
    }
    let trailer_start = bytes.len() - hash_len;
    bytes[trailer_start] ^= 0xff;
    expect_corrupt_load(&objects, &path, bytes.clone());

    let repo = Repository::discover(Some(dir.path())).expect("open");
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
    let (good, _) = build_commit_graph_bytes(
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
    .expect("build");
    let mut g = good.clone();
    if g.len() > 80 {
        g[72] = 0x80;
        g[73] = 0x00;
        g[74] = 0x00;
        g[75] = 0x00;
        reseal_commit_graph(&mut g);
        expect_corrupt_parse(path.clone(), g);
    }
}

#[test]
fn corrupt_chain_missing_and_mismatched_layer() {
    if !git_available() {
        eprintln!("SKIP: git unavailable");
        return;
    }
    let dir = tempfile::tempdir().expect("tempdir");
    setup_merge_repo(dir.path());
    git_cmd(
        dir.path(),
        &["commit-graph", "write", "--reachable", "--split"],
    );
    std::fs::write(dir.path().join("n.txt"), b"n\n").unwrap();
    git_cmd(dir.path(), &["add", "n.txt"]);
    git_cmd(dir.path(), &["commit", "-q", "-m", "n"]);
    git_cmd(
        dir.path(),
        &["commit-graph", "write", "--reachable", "--split"],
    );
    let objects = dir.path().join(".git/objects");
    let graphdir = objects.join("info/commit-graphs");
    let chain_path = graphdir.join("commit-graph-chain");
    let chain_text = std::fs::read_to_string(&chain_path).expect("chain");
    let hash = chain_text.lines().next().expect("hash").trim().to_string();
    let layer_path = graphdir.join(format!("graph-{hash}.graph"));
    std::fs::remove_file(&layer_path).expect("remove layer");
    let err = CommitGraphChain::try_load(&objects).unwrap_err();
    assert!(matches!(err, Error::CorruptObject(_) | Error::Io(_)));

    std::fs::write(&layer_path, b"not a graph").expect("bad layer");
    let err = CommitGraphChain::try_load(&objects).unwrap_err();
    assert!(matches!(err, Error::CorruptObject(_)));
}

#[test]
fn revwalk_ignores_corrupt_on_disk_graph() {
    if !git_available() {
        eprintln!("SKIP: git unavailable");
        return;
    }
    let dir = tempfile::tempdir().expect("tempdir");
    let path = write_verify_fixture_graph(dir.path());
    let repo = Repository::discover(Some(dir.path())).expect("open");
    let good = grit_rev_list(&repo, "commits/3", false);
    let mut bytes = std::fs::read(&path).expect("read");
    bytes[0] = 0;
    write_graph_bytes(&path, &bytes);
    let with_bad_graph = grit_rev_list(&repo, "commits/3", true);
    assert_eq!(good, with_bad_graph);
}

fn grit_rev_list(repo: &Repository, tip: &str, use_graph: bool) -> Vec<ObjectId> {
    let opts = RevListOptions {
        reverse: true,
        use_commit_graph: use_graph,
        ..Default::default()
    };
    rev_list(repo, &[tip.to_owned()], &[], &opts)
        .expect("rev_list")
        .commits
}

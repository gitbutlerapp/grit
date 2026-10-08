//! Checksummed Git file trailers: grit writes pass system `git` verification, and
//! grit reads (with trailer checks) accept `git`-written artifacts.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Command;

use grit_lib::bloom::BloomFilterSettings;
use grit_lib::commit_graph_write::{
    build_commit_graph_bytes, collect_reachable_commit_oids, load_commit_graph_commit_info,
};
use grit_lib::hash::{verify_trailer, TrailerMismatch};
use grit_lib::index::Index;
use grit_lib::index_pack::{install_pack_bytes, IngestPackOptions};
use grit_lib::midx::{
    verify_midx, write_multi_pack_index_with_options, WriteMultiPackIndexOptions,
};
use grit_lib::objects::{HashAlgo, ObjectId};
use grit_lib::pack::read_pack_index;
use grit_lib::pack_rev::{
    build_pack_rev_bytes_from_index_order_offsets_and_checksum, rev_path_for_index,
};
use grit_lib::repo::Repository;
use grit_lib::state::{resolve_head, HeadState};
use grit_lib::transfer::{build_pack, PackBuildOptions};
use grit_test_support::git;
use tempfile::TempDir;

fn git_cmd(dir: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .current_dir(dir)
        .args(args)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .env("GIT_AUTHOR_NAME", "Test")
        .env("GIT_AUTHOR_EMAIL", "t@example.com")
        .env("GIT_COMMITTER_NAME", "Test")
        .env("GIT_COMMITTER_EMAIL", "t@example.com")
        .output()
        .expect("spawn git");
    assert!(
        out.status.success(),
        "git {:?} failed: {}",
        args,
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).trim().to_owned()
}

fn init_git_repo(algo: HashAlgo) -> TempDir {
    let dir = TempDir::new().expect("tempdir");
    if algo == HashAlgo::Sha256 {
        git(
            dir.path(),
            &[
                "init",
                "-q",
                "--object-format=sha256",
                "--initial-branch=main",
                ".",
            ],
        );
    } else {
        git(dir.path(), &["init", "-q", "--initial-branch=main", "."]);
    }
    std::fs::write(dir.path().join("sample.txt"), b"trailer compat\n").unwrap();
    git(dir.path(), &["add", "sample.txt"]);
    git(dir.path(), &["commit", "-q", "-m", "seed"]);
    dir
}

fn tip_oid(repo: &Repository) -> ObjectId {
    match resolve_head(&repo.git_dir).expect("head") {
        HeadState::Branch { oid: Some(oid), .. } | HeadState::Detached { oid } => oid,
        _ => panic!("expected a commit at HEAD"),
    }
}

fn pack_dir(git_dir: &Path) -> PathBuf {
    git_dir.join("objects/pack")
}

fn find_pack_and_idx(pack_dir: &Path) -> (PathBuf, PathBuf) {
    for ent in std::fs::read_dir(pack_dir).expect("read pack dir") {
        let path = ent.expect("dirent").path();
        if path.extension().is_some_and(|e| e == "pack") {
            let idx = path.with_extension("idx");
            assert!(idx.is_file(), "missing idx for {}", path.display());
            return (path, idx);
        }
    }
    panic!("no pack in {}", pack_dir.display());
}

fn grit_write_trailer_artifacts(repo: &Repository) {
    let tip = tip_oid(repo);
    let pack =
        build_pack(&repo.odb, &[tip], &[], &PackBuildOptions::default()).expect("build pack");
    install_pack_bytes(pack, &repo.odb, &IngestPackOptions::default()).expect("install pack");

    let (pack_path, idx_path) = find_pack_and_idx(&pack_dir(&repo.git_dir));
    let index = read_pack_index(&idx_path).expect("read idx after install");
    let pack_bytes = std::fs::read(&pack_path).expect("read pack");
    let hb = repo.odb.hash_algo().len();
    let pack_checksum = &pack_bytes[pack_bytes.len() - hb..];
    let offsets: Vec<u64> = index.iter().map(|e| e.offset()).collect();
    let rev_bytes =
        build_pack_rev_bytes_from_index_order_offsets_and_checksum(&offsets, pack_checksum);
    std::fs::write(rev_path_for_index(&idx_path), rev_bytes).expect("write rev");

    // Match the on-disk MIDX format version system `git` writes (v1 on Git 2.43).
    write_multi_pack_index_with_options(
        &pack_dir(&repo.git_dir),
        &WriteMultiPackIndexOptions {
            version: Some(1),
            ..WriteMultiPackIndexOptions::default()
        },
    )
    .expect("write midx");

    let objects_dir = repo.git_dir.join("objects");
    let commits = collect_reachable_commit_oids(&repo.git_dir, &repo.odb).expect("commits");
    let mut sorted: Vec<ObjectId> = commits.into_iter().collect();
    sorted.sort();
    let mut infos = HashMap::new();
    for oid in &sorted {
        infos.insert(
            *oid,
            load_commit_graph_commit_info(&repo.odb, *oid).expect("commit info"),
        );
    }
    let bloom = BloomFilterSettings::default();
    let (graph_bytes, _) = build_commit_graph_bytes(
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
    .expect("commit-graph bytes");
    let info_dir = objects_dir.join("info");
    std::fs::create_dir_all(&info_dir).expect("info dir");
    std::fs::write(info_dir.join("commit-graph"), graph_bytes).expect("write commit-graph");

    let index = repo.load_index().expect("load index");
    index
        .write_to_path(&repo.git_dir.join("index"), false)
        .expect("write index");
    let _ = (pack_path,);
}

fn git_verify_all(repo_root: &Path, pack_path: &Path) {
    git_cmd(repo_root, &["fsck", "--full"]);
    git_cmd(
        repo_root,
        &["verify-pack", "-v", pack_path.to_str().expect("utf8 pack")],
    );
    git_cmd(repo_root, &["multi-pack-index", "verify"]);
    git_cmd(repo_root, &["commit-graph", "verify"]);
}

fn git_write_trailer_artifacts(repo_root: &Path) {
    git(repo_root, &["repack", "-adf"]);
    git(repo_root, &["multi-pack-index", "write"]);
    git(repo_root, &["commit-graph", "write", "--reachable"]);
}

fn grit_verify_git_artifacts(repo: &Repository) {
    let (_, idx_path) = find_pack_and_idx(&pack_dir(&repo.git_dir));
    read_pack_index(&idx_path).expect("grit verify pack idx trailer");
    let index_bytes = std::fs::read(repo.git_dir.join("index")).expect("read index");
    Index::parse_with_algo(&index_bytes, repo.odb.hash_algo()).expect("grit parse index trailer");
    verify_midx(&repo.git_dir.join("objects")).expect("grit verify midx");
    let graph_path = repo.git_dir.join("objects/info/commit-graph");
    let graph_bytes = std::fs::read(&graph_path).expect("read commit-graph");
    verify_trailer(repo.odb.hash_algo(), &graph_bytes).expect("commit-graph trailer");
}

#[test]
fn grit_written_trailers_pass_git_verify_sha1() {
    let tmp = init_git_repo(HashAlgo::Sha1);
    let repo = Repository::discover(Some(tmp.path())).expect("open");
    grit_write_trailer_artifacts(&repo);
    let (pack_path, _) = find_pack_and_idx(&pack_dir(&repo.git_dir));
    git_verify_all(tmp.path(), &pack_path);
}

#[test]
fn grit_written_trailers_pass_git_verify_sha256() {
    let tmp = init_git_repo(HashAlgo::Sha256);
    let repo = Repository::discover(Some(tmp.path())).expect("open");
    grit_write_trailer_artifacts(&repo);
    let (pack_path, _) = find_pack_and_idx(&pack_dir(&repo.git_dir));
    git_verify_all(tmp.path(), &pack_path);
}

#[test]
fn git_written_trailers_pass_grit_verify_sha1() {
    let tmp = init_git_repo(HashAlgo::Sha1);
    git_write_trailer_artifacts(tmp.path());
    let repo = Repository::discover(Some(tmp.path())).expect("open");
    grit_verify_git_artifacts(&repo);
}

#[test]
fn git_written_trailers_pass_grit_verify_sha256() {
    let tmp = init_git_repo(HashAlgo::Sha256);
    git_write_trailer_artifacts(tmp.path());
    let repo = Repository::discover(Some(tmp.path())).expect("open");
    grit_verify_git_artifacts(&repo);
}

fn corrupt_last_trailer_byte(path: &Path) {
    let mut bytes = std::fs::read(path).expect("read");
    assert!(!bytes.is_empty());
    let last = bytes.len() - 1;
    bytes[last] ^= 0xff;
    std::fs::write(path, bytes).expect("write corrupt");
}

#[test]
fn corrupt_trailer_byte_yields_typed_checksum_error() {
    let tmp = init_git_repo(HashAlgo::Sha1);
    let repo = Repository::discover(Some(tmp.path())).expect("open");
    grit_write_trailer_artifacts(&repo);
    let (_, idx_path) = find_pack_and_idx(&pack_dir(&repo.git_dir));
    let algo = repo.odb.hash_algo();

    corrupt_last_trailer_byte(&idx_path);
    let idx_bytes = std::fs::read(&idx_path).expect("read idx");
    let err = verify_trailer(algo, &idx_bytes).expect_err("trailer mismatch");
    assert!(matches!(err, TrailerMismatch { .. }));

    assert!(
        read_pack_index(&idx_path).is_err(),
        "read_pack_index must reject corrupt idx trailer"
    );

    let index_path = repo.git_dir.join("index");
    corrupt_last_trailer_byte(&index_path);
    let index_bytes = std::fs::read(&index_path).expect("read index");
    assert!(verify_trailer(algo, &index_bytes).is_err());
    assert!(Index::parse_with_algo(&index_bytes, algo).is_err());
}

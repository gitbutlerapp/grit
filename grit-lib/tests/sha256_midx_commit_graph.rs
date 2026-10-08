//! SHA-256 multi-pack-index, reverse-index, and commit-graph round-trips with system `git`.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::process::Command;

use grit_lib::bloom::BloomFilterSettings;
use grit_lib::commit_graph_file::CommitGraphChain;
use grit_lib::commit_graph_write::{
    build_commit_graph_bytes, collect_reachable_commit_oids, load_commit_graph_commit_info,
};
use grit_lib::midx::{
    read_midx_objects, try_read_object_via_midx, write_multi_pack_index_with_options,
    WriteMultiPackIndexOptions,
};
use grit_lib::objects::{HashAlgo, ObjectId, ObjectKind};
use grit_lib::odb::{hash_algo_for_git_dir, hash_algo_for_objects_dir, Odb};
use grit_lib::pack::read_idx_object_ids;
use grit_lib::pack_rev::hashfile_checksum_valid;

fn git(dir: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .current_dir(dir)
        .args(args)
        .env("GIT_AUTHOR_NAME", "T")
        .env("GIT_AUTHOR_EMAIL", "t@example.com")
        .env("GIT_COMMITTER_NAME", "T")
        .env("GIT_COMMITTER_EMAIL", "t@example.com")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .output()
        .expect("run git");
    assert!(
        out.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8(out.stdout).expect("utf8 stdout")
}

fn git_ok(dir: &Path, args: &[&str]) -> bool {
    Command::new("git")
        .current_dir(dir)
        .args(args)
        .env("GIT_AUTHOR_NAME", "T")
        .env("GIT_AUTHOR_EMAIL", "t@example.com")
        .env("GIT_COMMITTER_NAME", "T")
        .env("GIT_COMMITTER_EMAIL", "t@example.com")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

fn git_available() -> bool {
    Command::new("git")
        .arg("--version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

fn sha256_repo_supported() -> bool {
    let tmp = tempfile::tempdir().expect("tempdir");
    git_ok(
        tmp.path(),
        &["init", "-q", "--object-format=sha256", "-b", "main"],
    )
}

fn init_sha256_repo(root: &Path) -> PathBuf {
    git(
        root,
        &["init", "-q", "--object-format=sha256", "-b", "main"],
    );
    git(root, &["config", "core.multiPackIndex", "true"]);
    // Newer Git runs detached maintenance after commits; a background repack can delete a pack
    // while grit writes the MIDX, leaving it naming a pack git then fails to load.
    git(root, &["config", "gc.auto", "0"]);
    git(root, &["config", "maintenance.auto", "false"]);
    root.join(".git")
}

fn pack_idx_paths(objects: &Path) -> Vec<PathBuf> {
    let pack_dir = objects.join("pack");
    let mut out = Vec::new();
    for ent in std::fs::read_dir(&pack_dir).expect("read pack dir") {
        let ent = ent.expect("dirent");
        let name = ent.file_name().to_string_lossy().into_owned();
        if name.ends_with(".idx") {
            out.push(ent.path());
        }
    }
    out.sort();
    out
}

fn all_packed_oids(objects: &Path) -> HashSet<ObjectId> {
    let mut oids = HashSet::new();
    for idx in pack_idx_paths(objects) {
        for oid in read_idx_object_ids(&idx).expect("read idx oids") {
            oids.insert(oid);
        }
    }
    oids
}

fn build_three_pack_sha256_repo() -> Option<(tempfile::TempDir, PathBuf)> {
    if !git_available() || !sha256_repo_supported() {
        return None;
    }
    let tmp = tempfile::tempdir().ok()?;
    let git_dir = init_sha256_repo(tmp.path());
    let objects = git_dir.join("objects");
    for i in 0..3 {
        std::fs::write(
            tmp.path().join(format!("blob-{i}.txt")),
            format!("pack layer {i}\n"),
        )
        .ok()?;
        git(tmp.path(), &["add", &format!("blob-{i}.txt")]);
        git(tmp.path(), &["commit", "-q", "-m", &format!("c{i}")]);
        git(tmp.path(), &["repack", "-d"]);
    }
    if pack_idx_paths(&objects).len() < 3 {
        eprintln!("SKIP: could not create three pack files");
        return None;
    }
    Some((tmp, git_dir))
}

fn grit_write_midx_with_rev(pack_dir: &Path) {
    let opts = WriteMultiPackIndexOptions {
        write_bitmap_placeholders: true,
        write_rev_placeholder: true,
        version: Some(1),
        ..WriteMultiPackIndexOptions::default()
    };
    write_multi_pack_index_with_options(pack_dir, &opts).expect("grit write MIDX");
}

#[test]
fn sha256_grit_midx_and_rev_passes_git_verify() {
    let Some((_tmp, git_dir)) = build_three_pack_sha256_repo() else {
        eprintln!("SKIP: sha256 or git unavailable");
        return;
    };
    let objects = git_dir.join("objects");
    let pack_dir = objects.join("pack");
    let algo = hash_algo_for_objects_dir(&objects);
    assert_eq!(algo, HashAlgo::Sha256);

    grit_write_midx_with_rev(&pack_dir);
    assert!(pack_dir.join("multi-pack-index").is_file());

    let midx_d = pack_dir.join("multi-pack-index.d");
    let rev_sidecars: Vec<_> = [pack_dir.as_path(), midx_d.as_path()]
        .into_iter()
        .flat_map(|dir| {
            std::fs::read_dir(dir)
                .into_iter()
                .flatten()
                .filter_map(|e| e.ok())
                .map(|e| e.path())
        })
        .filter(|p| {
            p.file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.starts_with("multi-pack-index-") && n.ends_with(".rev"))
        })
        .collect();
    assert!(
        !rev_sidecars.is_empty(),
        "expected a MIDX .rev sidecar after grit write"
    );
    for rev in &rev_sidecars {
        let data = std::fs::read(rev).expect("read .rev");
        let hash_len = algo.len();
        assert!(hashfile_checksum_valid(&data, hash_len), "RIDX checksum");
        assert_eq!(data.len() % 4, 0, "RIDX length aligned");
        assert!(data.len() >= 12 + hash_len + hash_len);
    }

    git(_tmp.path(), &["multi-pack-index", "verify"]);
}

#[test]
fn sha256_git_midx_grit_reads_every_object() {
    let Some((_tmp, git_dir)) = build_three_pack_sha256_repo() else {
        eprintln!("SKIP: sha256 or git unavailable");
        return;
    };
    let objects = git_dir.join("objects");
    let pack_dir = objects.join("pack");
    let algo = hash_algo_for_objects_dir(&objects);
    assert_eq!(algo, HashAlgo::Sha256);

    git(_tmp.path(), &["multi-pack-index", "write"]);
    assert!(pack_dir.join("multi-pack-index").is_file());

    let expected = all_packed_oids(&objects);
    assert!(expected.len() > 3, "fixture should list many OIDs");

    let (_names, listed) = read_midx_objects(&objects).expect("read MIDX objects");
    let listed: HashSet<ObjectId> = listed.into_iter().map(|r| r.oid).collect();
    assert_eq!(listed, expected);

    let odb = Odb::new(&objects).with_config_git_dir(git_dir.clone());
    for oid in &expected {
        let via_midx = try_read_object_via_midx(&objects, oid)
            .expect("midx read")
            .expect("oid in midx");
        let via_odb = odb.read(oid).expect("odb read");
        assert_eq!(via_midx.kind, via_odb.kind);
        assert_eq!(via_midx.data, via_odb.data);
    }
}

#[test]
fn sha256_commit_graph_git_verify_and_grit_lookups() {
    let Some((_tmp, git_dir)) = build_three_pack_sha256_repo() else {
        eprintln!("SKIP: sha256 or git unavailable");
        return;
    };
    let objects = git_dir.join("objects");
    let odb = Odb::new(&objects).with_config_git_dir(git_dir.clone());
    assert_eq!(odb.hash_algo(), HashAlgo::Sha256);

    // Git-written graph, grit lookups.
    git(_tmp.path(), &["commit-graph", "write", "--reachable"]);
    git(_tmp.path(), &["commit-graph", "verify"]);
    let chain = CommitGraphChain::load(&objects).expect("load git commit-graph");
    let commits = collect_reachable_commit_oids(&git_dir, &odb).expect("commits");
    for oid in &commits {
        assert!(
            chain.find_commit(oid).is_some(),
            "commit {oid} missing from git-written graph"
        );
    }

    // Grit-written graph, git verify.
    std::fs::remove_file(objects.join("info/commit-graph")).ok();
    let mut sorted: Vec<ObjectId> = commits.into_iter().collect();
    sorted.sort();
    let mut infos = HashMap::new();
    for oid in &sorted {
        infos.insert(
            *oid,
            load_commit_graph_commit_info(&odb, *oid).expect("commit info"),
        );
    }
    let bloom = BloomFilterSettings::default();
    let (bytes, _stats) = build_commit_graph_bytes(
        &sorted,
        &infos,
        &odb,
        false,
        &bloom,
        None,
        &[],
        None,
        &HashMap::new(),
        &HashMap::new(),
        true,
    )
    .expect("build commit-graph");
    std::fs::write(objects.join("info/commit-graph"), bytes).expect("write graph");
    git(_tmp.path(), &["commit-graph", "verify"]);

    let chain = CommitGraphChain::load(&objects).expect("load grit commit-graph");
    for oid in &sorted {
        assert!(
            chain.find_commit(oid).is_some(),
            "commit {oid} missing from grit-written graph"
        );
    }
}

#[test]
fn sha256_objectformat_odd_casing_resolves_for_midx() {
    let Some((_tmp, git_dir)) = build_three_pack_sha256_repo() else {
        eprintln!("SKIP: sha256 or git unavailable");
        return;
    };
    let objects = git_dir.join("objects");
    let config = git_dir.join("config");
    let mut text = std::fs::read_to_string(&config).expect("read config");
    text.push_str("\n[ExTeNsIoNs]\n\tObjectFormat = Sha256\n");
    std::fs::write(&config, text).expect("append extensions section");

    assert_eq!(hash_algo_for_git_dir(&git_dir), HashAlgo::Sha256);
    assert_eq!(hash_algo_for_objects_dir(&objects), HashAlgo::Sha256);

    grit_write_midx_with_rev(&objects.join("pack"));

    let odb = Odb::new(&objects).with_config_git_dir(git_dir.clone());
    assert_eq!(odb.hash_algo(), HashAlgo::Sha256);
    let head = std::fs::read_to_string(git_dir.join("refs/heads/main")).expect("read main");
    let oid = ObjectId::from_hex(head.trim()).expect("parse HEAD");
    let obj = odb.read(&oid).expect("read HEAD through MIDX");
    assert_eq!(obj.kind, ObjectKind::Commit);
}

#[test]
fn sha256_git_split_commit_graph_chain_loads() {
    let Some((_tmp, git_dir)) = build_three_pack_sha256_repo() else {
        eprintln!("SKIP: sha256 or git unavailable");
        return;
    };
    let objects = git_dir.join("objects");

    git(
        _tmp.path(),
        &["commit-graph", "write", "--reachable", "--split"],
    );
    std::fs::write(_tmp.path().join("split.txt"), b"split layer\n").unwrap();
    git(_tmp.path(), &["add", "split.txt"]);
    git(_tmp.path(), &["commit", "-q", "-m", "split tip"]);
    git(
        _tmp.path(),
        &["commit-graph", "write", "--reachable", "--split"],
    );
    git(_tmp.path(), &["commit-graph", "verify"]);

    let chain = CommitGraphChain::try_load(&objects)
        .expect("load chain")
        .expect("split commit-graph chain present");
    assert!(
        chain.num_layers() >= 2,
        "expected at least two split layers, got {}",
        chain.num_layers()
    );
    let odb = Odb::new(&objects).with_config_git_dir(git_dir.clone());
    let commits = collect_reachable_commit_oids(&git_dir, &odb).expect("commits");
    for oid in commits {
        assert!(
            chain.find_commit(&oid).is_some(),
            "commit {oid} missing from git split chain"
        );
    }
}

#[test]
fn sha256_incremental_midx_read_objects() {
    let Some((_tmp, git_dir)) = build_three_pack_sha256_repo() else {
        eprintln!("SKIP: sha256 or git unavailable");
        return;
    };
    let objects = git_dir.join("objects");
    let pack_dir = objects.join("pack");

    grit_write_midx_with_rev(&pack_dir);
    assert!(pack_dir.join("multi-pack-index").is_file());

    std::fs::write(_tmp.path().join("incr.txt"), b"incremental pack\n").unwrap();
    git(_tmp.path(), &["add", "incr.txt"]);
    git(_tmp.path(), &["commit", "-q", "-m", "incr"]);
    git(_tmp.path(), &["repack", "-d"]);

    let opts = WriteMultiPackIndexOptions {
        incremental: true,
        version: Some(1),
        ..WriteMultiPackIndexOptions::default()
    };
    write_multi_pack_index_with_options(&pack_dir, &opts).expect("incremental MIDX write");
    assert!(
        !pack_dir.join("multi-pack-index").exists(),
        "incremental write removes root MIDX"
    );
    assert!(
        pack_dir
            .join("multi-pack-index.d/multi-pack-index-chain")
            .is_file(),
        "expected MIDX chain file"
    );

    let (_names, listed) = read_midx_objects(&objects).expect("read incremental MIDX");
    assert!(
        !listed.is_empty(),
        "incremental tip MIDX should list objects from the new layer"
    );

    let chain_text =
        std::fs::read_to_string(pack_dir.join("multi-pack-index.d/multi-pack-index-chain"))
            .expect("chain file");
    for line in chain_text.lines() {
        let h = line.trim();
        if h.is_empty() {
            continue;
        }
        assert_eq!(
            h.len(),
            HashAlgo::Sha256.hex_len(),
            "SHA-256 MIDX chain entries must be 64 hex chars"
        );
    }

    let packed = all_packed_oids(&objects);
    for entry in &listed {
        assert!(
            packed.contains(&entry.oid),
            "MIDX-listed oid {} not in any pack index",
            entry.oid
        );
    }

    let algo = hash_algo_for_objects_dir(&objects);
    let sample = listed[0].oid;
    assert!(
        try_read_object_via_midx(&objects, &sample)
            .expect("midx read")
            .is_some(),
        "sample object from incremental tip should load via MIDX"
    );
}

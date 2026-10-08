//! Commit-graph and Bloom filter round-trips with system `git` (t5318/t5324/t5328 scenarios).

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Command;

use grit_lib::bloom::BloomFilterSettings;
use grit_lib::commit_graph_file::{
    bloom_filter_for_commit_write, commit_tree_has_high_bit_paths, dump_bloom_filters,
    CommitGraphChain,
};
use grit_lib::commit_graph_write::{
    build_commit_graph_bytes, collect_reachable_commit_oids, load_commit_graph_commit_info,
};
use grit_lib::index::{Index, IndexEntry, MODE_REGULAR};
use grit_lib::objects::{parse_commit, serialize_commit, CommitData, ObjectId, ObjectKind};
use grit_lib::odb::Odb;
use grit_lib::repo::{init_repository, Repository};
use grit_lib::rev_list::{rev_list, RevListOptions};
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

fn sorted_commit_set(repo: &Repository) -> Vec<ObjectId> {
    let mut sorted: Vec<ObjectId> = collect_reachable_commit_oids(&repo.git_dir, &repo.odb)
        .expect("commits")
        .into_iter()
        .collect();
    sorted.sort();
    sorted
}

fn commit_infos(
    repo: &Repository,
    sorted: &[ObjectId],
) -> HashMap<ObjectId, grit_lib::commit_graph_write::CommitGraphCommitInfo> {
    let mut infos = HashMap::new();
    for oid in sorted {
        infos.insert(
            *oid,
            load_commit_graph_commit_info(&repo.odb, *oid).expect("commit info"),
        );
    }
    infos
}

fn write_grit_graph_file(
    repo: &Repository,
    sorted: &[ObjectId],
    infos: &HashMap<ObjectId, grit_lib::commit_graph_write::CommitGraphCommitInfo>,
    changed_paths: bool,
    write_generation_data: bool,
) -> Vec<u8> {
    let bloom = BloomFilterSettings::default();
    let (bytes, _stats) = build_commit_graph_bytes(
        sorted,
        infos,
        &repo.odb,
        changed_paths,
        &bloom,
        None,
        &[],
        None,
        &HashMap::new(),
        &HashMap::new(),
        write_generation_data,
    )
    .expect("build commit-graph");
    let info_dir = repo.git_dir.join("objects/info");
    std::fs::create_dir_all(&info_dir).expect("info dir");
    let graph_path = info_dir.join("commit-graph");
    if graph_path.is_file() {
        let mut perms = std::fs::metadata(&graph_path).expect("meta").permissions();
        perms.set_readonly(false);
        std::fs::set_permissions(&graph_path, perms).expect("chmod graph");
    }
    std::fs::write(&graph_path, &bytes).expect("write graph");
    bytes
}

fn git_rev_list_topo_with_config(repo_root: &Path, tip: &str, commit_graph: bool) -> Vec<ObjectId> {
    let flag = if commit_graph { "true" } else { "false" };
    let out = Command::new("git")
        .current_dir(repo_root)
        .args([
            "-c",
            &format!("core.commitGraph={flag}"),
            "rev-list",
            "--topo-order",
            "--reverse",
            tip,
        ])
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
        "git rev-list: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8(out.stdout)
        .expect("utf8 stdout")
        .lines()
        .filter_map(|l| ObjectId::from_hex(l.trim()).ok())
        .collect()
}

fn grit_rev_list_topo(repo: &Repository, tip: &str, use_graph: bool) -> Vec<ObjectId> {
    let opts = RevListOptions {
        reverse: true,
        use_commit_graph: use_graph,
        ..Default::default()
    };
    rev_list(repo, &[tip.to_owned()], &[], &opts)
        .expect("rev_list")
        .commits
}

fn parents_and_time_from_object(odb: &Odb, oid: &ObjectId) -> (Vec<ObjectId>, i64) {
    let obj = odb.read(oid).expect("read commit");
    let c = parse_commit(&obj.data).expect("parse");
    let parts: Vec<&str> = c.committer.rsplitn(3, ' ').collect();
    let time = if parts.len() >= 2 {
        parts[1].parse::<i64>().unwrap_or(0)
    } else {
        0
    };
    (c.parents, time)
}

/// Build the merge-heavy history from upstream t5318 (through commit 8).
fn setup_t5318_merge_repo(root: &Path) {
    git_cmd(root, &["init", "-q", "-b", "main"]);
    git_cmd(root, &["config", "gc.auto", "0"]);
    for i in 1..=3 {
        std::fs::write(root.join(format!("f{i}.txt")), format!("blob {i}\n")).unwrap();
        git_cmd(root, &["add", &format!("f{i}.txt")]);
        git_cmd(root, &["commit", "-q", "-m", &format!("c{i}")]);
        git_cmd(root, &["branch", &format!("commits/{i}")]);
    }
    git_cmd(root, &["reset", "--hard", "commits/1"]);
    for i in 4..=5 {
        std::fs::write(root.join(format!("f{i}.txt")), format!("blob {i}\n")).unwrap();
        git_cmd(root, &["add", &format!("f{i}.txt")]);
        git_cmd(root, &["commit", "-q", "-m", &format!("c{i}")]);
        git_cmd(root, &["branch", &format!("commits/{i}")]);
    }
    git_cmd(root, &["reset", "--hard", "commits/2"]);
    for i in 6..=7 {
        std::fs::write(root.join(format!("f{i}.txt")), format!("blob {i}\n")).unwrap();
        git_cmd(root, &["add", &format!("f{i}.txt")]);
        git_cmd(root, &["commit", "-q", "-m", &format!("c{i}")]);
        git_cmd(root, &["branch", &format!("commits/{i}")]);
    }
    git_cmd(root, &["reset", "--hard", "commits/2"]);
    git_cmd(root, &["merge", "-q", "-m", "M1", "commits/4"]);
    git_cmd(root, &["branch", "merge/1"]);
    git_cmd(root, &["reset", "--hard", "commits/4"]);
    git_cmd(root, &["merge", "-q", "-m", "M2", "commits/6"]);
    git_cmd(root, &["branch", "merge/2"]);
    git_cmd(root, &["reset", "--hard", "commits/3"]);
    git_cmd(root, &["merge", "-q", "-m", "M3", "commits/5", "commits/7"]);
    git_cmd(root, &["branch", "merge/3"]);
    git_cmd(root, &["repack", "-ad"]);
    std::fs::write(root.join("f8.txt"), b"tip\n").unwrap();
    git_cmd(root, &["add", "f8.txt"]);
    git_cmd(root, &["commit", "-q", "-m", "c8"]);
    git_cmd(root, &["branch", "commits/8"]);
    git_cmd(root, &["repack", "-ad"]);
}

fn assert_chain_matches_objects(chain: &CommitGraphChain, odb: &Odb) {
    for oid in chain.all_oids_in_order() {
        assert!(
            chain.find_commit(&oid).is_some(),
            "find_commit missing {oid}"
        );
        assert!(chain.global_position(&oid).is_some());
        if let Some((g_parents, g_time)) = chain.graph_commit(&oid) {
            let (o_parents, o_time) = parents_and_time_from_object(odb, &oid);
            if o_parents.len() <= 2 {
                assert_eq!(g_parents, o_parents, "parents for {oid}");
                assert_eq!(g_time, o_time, "committer time for {oid}");
            }
        }
    }
}

#[test]
fn grit_written_graph_verify_and_topo_rev_list() {
    if !git_available() {
        eprintln!("SKIP: git unavailable");
        return;
    }
    let dir = tempfile::tempdir().expect("tempdir");
    setup_t5318_merge_repo(dir.path());
    let repo = Repository::discover(Some(dir.path())).expect("open");
    let sorted = sorted_commit_set(&repo);
    let infos = commit_infos(&repo, &sorted);
    write_grit_graph_file(&repo, &sorted, &infos, false, true);
    git_cmd(dir.path(), &["commit-graph", "verify"]);

    let git_no_cg = git_rev_list_topo_with_config(dir.path(), "commits/8", false);
    let git_with_cg = git_rev_list_topo_with_config(dir.path(), "commits/8", true);
    assert_eq!(
        git_no_cg, git_with_cg,
        "git topo-order must not depend on commit-graph"
    );
    let grit_no_graph = grit_rev_list_topo(&repo, "commits/8", false);
    let grit_with_graph = grit_rev_list_topo(&repo, "commits/8", true);
    assert_eq!(
        grit_no_graph, grit_with_graph,
        "grit topo-order must not depend on commit-graph"
    );

    let chain = CommitGraphChain::load(repo.odb.objects_dir()).expect("load grit graph");
    assert_chain_matches_objects(&chain, &repo.odb);
    assert!(chain.layer_commit_counts_tip_first()[0] > 0);
}

#[test]
fn grit_written_graph_with_bloom_and_generation_passes_git_verify() {
    if !git_available() {
        eprintln!("SKIP: git unavailable");
        return;
    }
    let dir = tempfile::tempdir().expect("tempdir");
    setup_t5318_merge_repo(dir.path());
    let repo = Repository::discover(Some(dir.path())).expect("open");
    let sorted = sorted_commit_set(&repo);
    let infos = commit_infos(&repo, &sorted);
    write_grit_graph_file(&repo, &sorted, &infos, true, true);
    git_cmd(dir.path(), &["commit-graph", "verify"]);
}

#[test]
fn git_written_graph_grit_chain_api_matches_objects() {
    if !git_available() {
        eprintln!("SKIP: git unavailable");
        return;
    }
    let dir = tempfile::tempdir().expect("tempdir");
    setup_t5318_merge_repo(dir.path());
    git_cmd(
        dir.path(),
        &["commit-graph", "write", "--reachable", "--changed-paths"],
    );
    git_cmd(dir.path(), &["commit-graph", "verify"]);
    let repo = Repository::discover(Some(dir.path())).expect("open");
    let chain = CommitGraphChain::try_load(repo.odb.objects_dir())
        .expect("load")
        .expect("graph");
    assert!(chain.num_layers() >= 1);
    assert!(chain.top_layer_bloom_settings().is_some());
    assert_chain_matches_objects(&chain, &repo.odb);
    let _ = chain.layer_hashes_tip_first();
    let _ = chain.layer_has_generation_data_tip_first();
    let _ = chain.sub_chain_tip_first(0, 1);
    let _ = chain.layer_oids(0);
}

#[test]
fn git_split_chain_grit_loads_and_verifies() {
    if !git_available() {
        eprintln!("SKIP: git unavailable");
        return;
    }
    let dir = tempfile::tempdir().expect("tempdir");
    setup_t5318_merge_repo(dir.path());
    git_cmd(
        dir.path(),
        &["commit-graph", "write", "--reachable", "--split"],
    );
    std::fs::write(dir.path().join("extra.txt"), b"x\n").unwrap();
    git_cmd(dir.path(), &["add", "extra.txt"]);
    git_cmd(dir.path(), &["commit", "-q", "-m", "split layer"]);
    git_cmd(
        dir.path(),
        &["commit-graph", "write", "--reachable", "--split"],
    );
    git_cmd(dir.path(), &["commit-graph", "verify"]);
    let repo = Repository::discover(Some(dir.path())).expect("open");
    let chain = CommitGraphChain::try_load(repo.odb.objects_dir())
        .expect("load")
        .expect("split chain");
    assert!(chain.num_layers() >= 2);
    for oid in sorted_commit_set(&repo) {
        assert!(chain.find_commit(&oid).is_some());
    }
}

#[test]
fn bloom_filter_bytes_match_git_changed_paths_graph() {
    if !git_available() {
        eprintln!("SKIP: git unavailable");
        return;
    }
    let dir = tempfile::tempdir().expect("tempdir");
    setup_t5318_merge_repo(dir.path());
    git_cmd(
        dir.path(),
        &["commit-graph", "write", "--reachable", "--changed-paths"],
    );
    let repo = Repository::discover(Some(dir.path())).expect("open");
    let graph_path = repo.git_dir.join("objects/info/commit-graph");
    let git_lines = dump_bloom_filters(&graph_path).expect("git graph bloom dump");
    let chain = CommitGraphChain::load(repo.odb.objects_dir()).expect("chain");
    let sorted = chain.all_oids_in_order();
    let settings = chain.top_layer_bloom_settings().expect("bloom settings");
    assert_eq!(git_lines.len(), sorted.len());
    for (i, oid) in sorted.iter().enumerate() {
        let info = load_commit_graph_commit_info(&repo.odb, *oid).expect("info");
        let (grit_bytes, _) =
            bloom_filter_for_commit_write(&repo.odb, &info.parents, info.tree, &settings)
                .expect("grit bloom");
        let git_hex = &git_lines[i];
        let grit_hex: String = grit_bytes.iter().map(|b| format!("{b:02x}")).collect();
        assert_eq!(
            git_hex, &grit_hex,
            "bloom mismatch for commit {oid} (empty git line means empty filter)"
        );
    }
}

#[test]
fn high_bit_paths_bloom_and_upgrade_detection() {
    if !git_available() {
        eprintln!("SKIP: git unavailable");
        return;
    }
    let dir = tempfile::tempdir().expect("tempdir");
    git_cmd(dir.path(), &["init", "-q", "-b", "main"]);
    let high = "\u{0080}file.txt";
    std::fs::write(dir.path().join(high), b"high bit path\n").unwrap();
    git_cmd(dir.path(), &["add", "--", high]);
    git_cmd(dir.path(), &["commit", "-q", "-m", "high-bit"]);
    git_cmd(
        dir.path(),
        &["commit-graph", "write", "--reachable", "--changed-paths"],
    );
    let repo = Repository::discover(Some(dir.path())).expect("open");
    let tip = sorted_commit_set(&repo)[0];
    assert!(commit_tree_has_high_bit_paths(&repo.odb, tip));
    let graph_path = repo.git_dir.join("objects/info/commit-graph");
    let git_lines = dump_bloom_filters(&graph_path).expect("git bloom dump");
    let chain = CommitGraphChain::load(repo.odb.objects_dir()).expect("chain");
    let settings = chain.top_layer_bloom_settings().expect("bloom settings");
    let info = load_commit_graph_commit_info(&repo.odb, tip).expect("info");
    let (grit_bytes, _) =
        bloom_filter_for_commit_write(&repo.odb, &info.parents, info.tree, &settings)
            .expect("grit bloom");
    let grit_hex: String = grit_bytes.iter().map(|b| format!("{b:02x}")).collect();
    assert_eq!(
        git_lines[0], grit_hex,
        "high-bit path bloom mismatch for tip {tip}"
    );
    let existing = chain
        .existing_filter_bytes(&tip, &settings)
        .expect("filter in git graph");
    assert_eq!(existing, grit_bytes.as_slice());
    let sorted = sorted_commit_set(&repo);
    let infos = commit_infos(&repo, &sorted);
    write_grit_graph_file(&repo, &sorted, &infos, true, true);
    git_cmd(dir.path(), &["commit-graph", "verify"]);
}

#[test]
fn commit_graph_across_alternate_split_chain() {
    if !git_available() {
        eprintln!("SKIP: git unavailable");
        return;
    }
    let upstream = tempfile::tempdir().expect("upstream");
    setup_t5318_merge_repo(upstream.path());
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
    std::fs::write(clone.path().join("local.txt"), b"local\n").unwrap();
    git_cmd(clone.path(), &["add", "local.txt"]);
    git_cmd(clone.path(), &["commit", "-q", "-m", "local"]);
    git_cmd(
        clone.path(),
        &["commit-graph", "write", "--reachable", "--split"],
    );
    git_cmd(clone.path(), &["commit-graph", "verify"]);
    let git_dir = clone.path().join(".git");
    let objects = git_dir.join("objects");
    let alt_file = objects.join("info/alternates");
    let alt_dirs: Vec<PathBuf> = if alt_file.is_file() {
        std::fs::read_to_string(&alt_file)
            .expect("alternates")
            .lines()
            .filter(|l| !l.trim().is_empty())
            .map(PathBuf::from)
            .collect()
    } else {
        Vec::new()
    };
    let chain = CommitGraphChain::try_load_across(&objects, &alt_dirs)
        .expect("load across")
        .expect("chain across alternates");
    assert!(chain.num_layers() >= 2);
}

#[test]
fn generation_overflow_and_wide_commit_times() {
    if !git_available() {
        eprintln!("SKIP: git unavailable");
        return;
    }
    let dir = tempfile::tempdir().expect("tempdir");
    git_cmd(dir.path(), &["init", "-q", "-b", "main"]);
    git_cmd(
        dir.path(),
        &[
            "commit",
            "--allow-empty",
            "-q",
            "-m",
            "old",
            "--date",
            "@0 +0000",
        ],
    );
    git_cmd(dir.path(), &["commit", "--allow-empty", "-q", "-m", "now"]);
    git_cmd(
        dir.path(),
        &[
            "commit",
            "--allow-empty",
            "-q",
            "-m",
            "future",
            "--date",
            "@4147483646 +0000",
        ],
    );
    if !git_ok(dir.path(), &["commit-graph", "write", "--reachable"]) {
        eprintln!("SKIP: platform git lacks 64-bit commit-graph support");
        return;
    }
    git_cmd(dir.path(), &["commit-graph", "verify"]);
    let repo = Repository::discover(Some(dir.path())).expect("open");
    let sorted = sorted_commit_set(&repo);
    let infos = commit_infos(&repo, &sorted);
    write_grit_graph_file(&repo, &sorted, &infos, false, true);
    git_cmd(dir.path(), &["commit-graph", "verify"]);
    let chain = CommitGraphChain::load(repo.odb.objects_dir()).expect("grit graph");
    assert!(chain.layer_has_generation_data_tip_first()[0]);
}

#[test]
fn root_commit_and_grit_only_write_without_generation_chunk() {
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
    let tree = grit_lib::write_tree::write_tree_from_index(&repo.odb, &index, "").expect("tree");
    let raw = serialize_commit(&CommitData {
        tree,
        parents: vec![],
        author: "T <t@example.com> 1 +0000".into(),
        committer: "T <t@example.com> 1 +0000".into(),
        author_raw: Vec::new(),
        committer_raw: Vec::new(),
        encoding: None,
        message: "root\n".into(),
        raw_message: None,
    });
    let commit = repo.odb.write(ObjectKind::Commit, &raw).expect("commit");
    grit_lib::refs::write_ref(&repo.git_dir, "refs/heads/main", &commit).expect("ref");
    let sorted = vec![commit];
    let infos = commit_infos(&repo, &sorted);
    write_grit_graph_file(&repo, &sorted, &infos, false, false);
    if git_available() {
        git_cmd(dir.path(), &["commit-graph", "verify"]);
    }
    let chain = CommitGraphChain::load(repo.odb.objects_dir()).expect("graph");
    let (parents, _) = chain.graph_commit(&commit).expect("root graph_commit");
    assert!(parents.is_empty());
}

#[test]
fn existing_and_upgradable_filter_bytes_from_git_graph() {
    if !git_available() {
        eprintln!("SKIP: git unavailable");
        return;
    }
    let dir = tempfile::tempdir().expect("tempdir");
    setup_t5318_merge_repo(dir.path());
    git_cmd(
        dir.path(),
        &["commit-graph", "write", "--reachable", "--changed-paths"],
    );
    let repo = Repository::discover(Some(dir.path())).expect("open");
    let chain = CommitGraphChain::load(repo.odb.objects_dir()).expect("chain");
    let settings = BloomFilterSettings::default();
    let tip = chain.all_oids_in_order().into_iter().last().expect("tip");
    assert!(chain
        .existing_filter_bytes(&tip, &settings)
        .is_some_and(|b| !b.is_empty()));
    assert!(chain.upgradable_filter_bytes(&tip, &settings).is_none());
    let layer_settings = chain.top_layer_bloom_settings().expect("bloom header");
    let v2 = BloomFilterSettings {
        hash_version: 2,
        ..settings
    };
    if layer_settings.hash_version == 1 {
        assert!(chain
            .upgradable_filter_bytes(&tip, &v2)
            .is_some_and(|b| !b.is_empty()));
    } else {
        assert!(chain.upgradable_filter_bytes(&tip, &v2).is_none());
    }
}

#[test]
fn bloom_precheck_for_paths_on_git_graph() {
    if !git_available() {
        eprintln!("SKIP: git unavailable");
        return;
    }
    let dir = tempfile::tempdir().expect("tempdir");
    setup_t5318_merge_repo(dir.path());
    git_cmd(
        dir.path(),
        &["commit-graph", "write", "--reachable", "--changed-paths"],
    );
    let repo = Repository::discover(Some(dir.path())).expect("open");
    let chain = CommitGraphChain::load(repo.odb.objects_dir()).expect("chain");
    let tip = sorted_commit_set(&repo).into_iter().max().expect("tip");
    let pre = chain
        .bloom_precheck_for_paths(&repo.odb, tip, &["f8.txt".to_owned()], None, -1, true)
        .expect("precheck");
    assert_ne!(
        pre,
        grit_lib::commit_graph_file::BloomPrecheck::Inapplicable
    );
}

//! History Criterion fixtures (s7): deterministic repos for revwalk, rev-parse, and diff benches.
//!
//! Builds repositories with `grit-lib` and uses the system `git` binary only for
//! `index-pack`, which is allowed in benchmark harness code (same as `fixture.rs`).

#![allow(clippy::expect_used, clippy::unwrap_used)]

use std::collections::HashMap;
use std::path::Path;
use std::process::Command;
use std::sync::OnceLock;

use grit_lib::bloom::BloomFilterSettings;
use grit_lib::commit_graph_write::{
    build_commit_graph_bytes, collect_reachable_commit_oids, load_commit_graph_commit_info,
    CommitGraphCommitInfo,
};
use grit_lib::error::Result as GritResult;
use grit_lib::index::{Index, IndexEntry, MODE_REGULAR};
use grit_lib::objects::{
    serialize_commit, serialize_tag, CommitData, ObjectId, ObjectKind, TagData,
};
use grit_lib::odb::Odb;
use grit_lib::pack::{clear_pack_cache, read_pack_index};
use grit_lib::refs::write_ref;
use grit_lib::repo::{init_repository, Repository};
use grit_lib::transfer::{build_pack, PackBuildOptions};
use grit_lib::write_tree::{write_tree_from_index, write_tree_update_index, WriteTreeFlags};
use tempfile::TempDir;

const GIT_ENV: &[(&str, &str)] = &[
    ("GIT_CONFIG_GLOBAL", "/dev/null"),
    ("GIT_CONFIG_SYSTEM", "/dev/null"),
    ("GIT_CONFIG_NOSYSTEM", "1"),
];

/// Target commit count for revwalk benches (override with `GRIT_HISTORY_BENCH_COMMITS`).
#[must_use]
pub fn history_commit_count() -> usize {
    std::env::var("GRIT_HISTORY_BENCH_COMMITS")
        .ok()
        .and_then(|v| v.parse().ok())
        .filter(|&n| n > 100)
        .unwrap_or(10_000)
}

/// Distance used by the `HEAD~N` rev-parse benchmark (requires more than 100 commits).
#[must_use]
pub fn head_tilde_distance() -> usize {
    100
}

fn bench_ident(seq: i64) -> String {
    format!("History Bench <bench@grit-scm.test> {seq} +0000")
}

fn git_index_pack(dir: &Path, pack_path: &Path) -> GritResult<()> {
    let mut cmd = Command::new("git");
    cmd.current_dir(dir).args([
        "index-pack",
        pack_path
            .file_name()
            .and_then(|s| s.to_str())
            .expect("pack file name"),
    ]);
    for (k, v) in GIT_ENV {
        cmd.env(k, v);
    }
    let out = cmd.output().map_err(grit_lib::error::Error::Io)?;
    if !out.status.success() {
        return Err(grit_lib::error::Error::Message(format!(
            "git index-pack failed: {}",
            String::from_utf8_lossy(&out.stderr)
        )));
    }
    Ok(())
}

fn install_indexed_pack(objects_dir: &Path, stem: &str, pack_bytes: &[u8]) {
    let pack_dir = objects_dir.join("pack");
    std::fs::create_dir_all(&pack_dir).expect("pack dir");
    let scratch = tempfile::tempdir().expect("index-pack scratch");
    let pack_path = scratch.path().join(format!("{stem}.pack"));
    std::fs::write(&pack_path, pack_bytes).expect("write pack bytes");
    git_index_pack(scratch.path(), &pack_path).expect("index-pack");
    let idx_path = scratch.path().join(format!("{stem}.idx"));
    let dest_pack = pack_dir.join(format!("{stem}.pack"));
    let dest_idx = pack_dir.join(format!("{stem}.idx"));
    std::fs::copy(&pack_path, &dest_pack).expect("copy pack");
    std::fs::copy(&idx_path, &dest_idx).expect("copy idx");
    clear_pack_cache();
    read_pack_index(&dest_idx).expect("read installed idx");
}

fn git_commit_graph_write(repo_root: &Path) -> std::result::Result<(), String> {
    let mut cmd = Command::new("git");
    cmd.current_dir(repo_root)
        .args(["commit-graph", "write", "--reachable", "--changed-paths"]);
    for (k, v) in GIT_ENV {
        cmd.env(k, v);
    }
    let out = cmd.output().map_err(|e| e.to_string())?;
    if out.status.success() {
        return Ok(());
    }
    Err(format!(
        "git commit-graph write failed: {}",
        String::from_utf8_lossy(&out.stderr)
    ))
}

fn write_grit_commit_graph(objects_dir: &Path, odb: &Odb) -> std::result::Result<(), String> {
    let git_dir = objects_dir
        .parent()
        .expect("objects dir has parent git_dir");
    let commits = collect_reachable_commit_oids(git_dir, odb).map_err(|e| e.to_string())?;
    let mut sorted: Vec<ObjectId> = commits.into_iter().collect();
    sorted.sort();
    let mut infos: HashMap<ObjectId, CommitGraphCommitInfo> = HashMap::new();
    for oid in &sorted {
        infos.insert(
            *oid,
            load_commit_graph_commit_info(odb, *oid).map_err(|e| e.to_string())?,
        );
    }
    let bloom = BloomFilterSettings::default();
    let (bytes, _stats) = build_commit_graph_bytes(
        &sorted,
        &infos,
        odb,
        true,
        &bloom,
        None,
        &[],
        None,
        &HashMap::new(),
        &HashMap::new(),
        true,
    )
    .map_err(|e| e.to_string())?;
    let info_dir = objects_dir.join("info");
    std::fs::create_dir_all(&info_dir).map_err(|e| e.to_string())?;
    std::fs::write(info_dir.join("commit-graph"), bytes).map_err(|e| e.to_string())?;
    Ok(())
}

fn index_entry(path: &[u8], oid: ObjectId, size: usize, mode: u32) -> IndexEntry {
    IndexEntry {
        ctime_sec: 1_700_000_000,
        ctime_nsec: 0,
        mtime_sec: 1_700_000_000,
        mtime_nsec: 0,
        dev: 0,
        ino: 0,
        mode,
        uid: 0,
        gid: 0,
        size: size as u32,
        oid,
        flags: (path.len().min(0xfff)) as u16,
        flags_extended: None,
        path: path.to_vec(),
        base_index_pos: 0,
    }
}

fn build_history_repo(dir: &Path, commit_count: usize) -> Repository {
    let repo = init_repository(dir, false, "main", None, "files").expect("init history repo");
    let mut index = Index::new();
    index.hash_algo = repo.odb.hash_algo();
    let blob = repo
        .odb
        .write(ObjectKind::Blob, b"history bench seed\n")
        .expect("seed blob");
    index.add_or_replace(index_entry(b"seed.txt", blob, 18, MODE_REGULAR));

    let mut parent: Option<ObjectId> = None;
    let mut commits: Vec<ObjectId> = Vec::with_capacity(commit_count);
    for i in 0..commit_count {
        if i > 0 && i % 50 == 0 {
            let body = format!("history churn {i}\n");
            let oid = repo
                .odb
                .write(ObjectKind::Blob, body.as_bytes())
                .expect("churn blob");
            index.add_or_replace(index_entry(
                format!("churn/{i:05}.txt").as_bytes(),
                oid,
                body.len(),
                MODE_REGULAR,
            ));
        }
        let tree = if i == 0 {
            write_tree_from_index(&repo.odb, &index, "").expect("initial tree")
        } else {
            write_tree_update_index(&repo.odb, &mut index, "", WriteTreeFlags::default())
                .expect("update tree")
        };
        let ts = 1_700_000_000i64 + i as i64;
        let ident = bench_ident(ts);
        let raw = serialize_commit(&CommitData {
            tree,
            parents: parent.into_iter().collect(),
            author: ident.clone(),
            committer: ident,
            author_raw: Vec::new(),
            committer_raw: Vec::new(),
            encoding: None,
            message: format!("history {i}\n"),
            raw_message: None,
            extra_headers: Vec::new(),
        });
        let oid = repo
            .odb
            .write(ObjectKind::Commit, &raw)
            .expect("write commit");
        commits.push(oid);
        parent = Some(oid);
    }
    let head = *commits.last().expect("commits");
    write_ref(&repo.git_dir, "refs/heads/main", &head).expect("write main");

    let branch_refs = 200usize.min(commits.len());
    let step = commits.len() / branch_refs.max(1);
    for i in 0..branch_refs {
        let oid = commits[i * step];
        write_ref(&repo.git_dir, &format!("refs/heads/bench/b{i:05}"), &oid)
            .expect("write branch ref");
    }

    let tag_commit = commits[commits.len() / 2];
    let tag = TagData {
        object: tag_commit,
        object_type: "commit".to_owned(),
        tag: "bench-tag".to_owned(),
        tagger: Some(bench_ident(1_700_050_000)),
        message: "history bench tag\n".to_owned(),
    };
    let tag_oid = repo
        .odb
        .write(ObjectKind::Tag, &serialize_tag(&tag))
        .expect("write tag");
    write_ref(&repo.git_dir, "refs/tags/bench-tag", &tag_oid).expect("write tag ref");

    let pack = build_pack(
        &repo.odb,
        &[head],
        &[],
        &PackBuildOptions {
            delta: false,
            ..PackBuildOptions::default()
        },
    )
    .expect("history pack");
    install_indexed_pack(repo.odb.objects_dir(), "history", &pack);

    repo
}

fn build_flat_tree(odb: &Odb, entry_count: usize, label: &str) -> ObjectId {
    let mut index = Index::new();
    index.hash_algo = odb.hash_algo();
    for i in 0..entry_count {
        let body = format!("{label} entry {i}\n");
        let oid = odb
            .write(ObjectKind::Blob, body.as_bytes())
            .expect("flat blob");
        index.add_or_replace(index_entry(
            format!("f{i:05}.txt").as_bytes(),
            oid,
            body.len(),
            MODE_REGULAR,
        ));
    }
    write_tree_from_index(odb, &index, "").expect("flat tree")
}

fn build_flat_tree_changed(
    odb: &Odb,
    entry_count: usize,
    label: &str,
    change_count: usize,
) -> ObjectId {
    let mut index = Index::new();
    index.hash_algo = odb.hash_algo();
    for i in 0..entry_count {
        let body = if i < change_count {
            format!("{label} changed {i}\n")
        } else {
            format!("wide entry {i}\n")
        };
        let oid = odb
            .write(ObjectKind::Blob, body.as_bytes())
            .expect("flat blob");
        index.add_or_replace(index_entry(
            format!("f{i:05}.txt").as_bytes(),
            oid,
            body.len(),
            MODE_REGULAR,
        ));
    }
    write_tree_from_index(odb, &index, "").expect("flat tree changed")
}

fn deep_leaf_path(depth: usize, leaf: usize) -> String {
    let mut path = String::from("deep");
    for level in 0..depth {
        path.push_str(&format!("/{:02x}", (leaf.wrapping_add(level * 17)) & 0xff));
    }
    path.push_str(&format!("/leaf{leaf:04}.txt"));
    path
}

fn build_deep_tree(odb: &Odb, depth: usize, leaves: usize) -> ObjectId {
    let mut index = Index::new();
    index.hash_algo = odb.hash_algo();
    for leaf in 0..leaves {
        let path = deep_leaf_path(depth, leaf);
        let body = format!("deep depth={depth} leaf={leaf}\n");
        let oid = odb
            .write(ObjectKind::Blob, body.as_bytes())
            .expect("deep blob");
        index.add_or_replace(index_entry(path.as_bytes(), oid, body.len(), MODE_REGULAR));
    }
    write_tree_from_index(odb, &index, "").expect("deep tree")
}

fn build_deep_tree_changed(
    odb: &Odb,
    depth: usize,
    leaves: usize,
    change_count: usize,
) -> ObjectId {
    let mut index = Index::new();
    index.hash_algo = odb.hash_algo();
    for leaf in 0..leaves {
        let path = deep_leaf_path(depth, leaf);
        let body = if leaf < change_count {
            format!("deep changed depth={depth} leaf={leaf}\n")
        } else {
            format!("deep depth={depth} leaf={leaf}\n")
        };
        let oid = odb
            .write(ObjectKind::Blob, body.as_bytes())
            .expect("deep blob");
        index.add_or_replace(index_entry(path.as_bytes(), oid, body.len(), MODE_REGULAR));
    }
    write_tree_from_index(odb, &index, "").expect("deep tree changed")
}

fn blob_lines(count: usize, pattern: &str) -> String {
    (0..count)
        .map(|i| format!("{pattern} line {i}"))
        .collect::<Vec<_>>()
        .join("\n")
        + "\n"
}

fn pathological_blob_lines(count: usize) -> (String, String) {
    let mut old = String::new();
    let mut new = String::new();
    for i in 0..count {
        if i % 2 == 0 {
            old.push_str(&format!("A{i}\n"));
            new.push_str(&format!("B{i}\n"));
        } else {
            old.push_str(&format!("X{i}\n"));
            new.push_str(&format!("X{i}\n"));
        }
    }
    (old, new)
}

/// Shared history benchmark fixtures (built once per process).
pub struct HistoryBenchFixtures {
    pub _root: TempDir,
    pub repo: Repository,
    pub head: ObjectId,
    pub commit_count: usize,
    pub wide_tree: ObjectId,
    pub wide_tree_few_changes: ObjectId,
    pub wide_tree_many_changes: ObjectId,
    pub deep_tree: ObjectId,
    pub deep_tree_few_changes: ObjectId,
    pub deep_tree_many_changes: ObjectId,
    pub blob_small_old: String,
    pub blob_small_new: String,
    pub blob_large_old: String,
    pub blob_large_new: String,
    pub blob_pathological_old: String,
    pub blob_pathological_new: String,
}

impl HistoryBenchFixtures {
    fn build() -> Self {
        let commit_count = history_commit_count();
        let root = tempfile::tempdir().expect("history tempdir");
        let repo = build_history_repo(root.path(), commit_count);
        let head =
            grit_lib::refs::resolve_ref(&repo.git_dir, "refs/heads/main").expect("read head");

        if git_commit_graph_write(root.path()).is_err() {
            write_grit_commit_graph(repo.odb.objects_dir(), &repo.odb).expect("grit commit-graph");
        }

        let wide_tree = build_flat_tree(&repo.odb, 10_000, "wide");
        let wide_tree_few_changes = build_flat_tree_changed(&repo.odb, 10_000, "wide-few", 8);
        let wide_tree_many_changes = build_flat_tree_changed(&repo.odb, 10_000, "wide-many", 2_000);

        const DEEP_TREE_DEPTH: usize = 32;
        const DEEP_TREE_LEAVES: usize = 64;
        let deep_tree = build_deep_tree(&repo.odb, DEEP_TREE_DEPTH, DEEP_TREE_LEAVES);
        let deep_tree_few_changes =
            build_deep_tree_changed(&repo.odb, DEEP_TREE_DEPTH, DEEP_TREE_LEAVES, 4);
        let deep_tree_many_changes =
            build_deep_tree_changed(&repo.odb, DEEP_TREE_DEPTH, DEEP_TREE_LEAVES, 48);

        let blob_small_old = "alpha\nbeta\ngamma\n".to_owned();
        let blob_small_new = "alpha\nbeta delta\ngamma\n".to_owned();
        let blob_large_old = blob_lines(10_000, "same");
        let blob_large_new = {
            let mut s = blob_large_old.clone();
            s.push_str("extra tail line\n");
            s
        };
        let (blob_pathological_old, blob_pathological_new) = pathological_blob_lines(4_000);

        Self {
            _root: root,
            repo,
            head,
            commit_count,
            wide_tree,
            wide_tree_few_changes,
            wide_tree_many_changes,
            deep_tree,
            deep_tree_few_changes,
            deep_tree_many_changes,
            blob_small_old,
            blob_small_new,
            blob_large_old,
            blob_large_new,
            blob_pathological_old,
            blob_pathological_new,
        }
    }

    /// Remove commit-graph file so revwalk runs without on-disk graph acceleration.
    pub fn remove_commit_graph(&self) {
        let path = self.repo.git_dir.join("objects/info/commit-graph");
        let _ = std::fs::remove_file(path);
    }

    /// Restore commit-graph (rebuild if missing).
    pub fn ensure_commit_graph(&self) {
        let path = self.repo.git_dir.join("objects/info/commit-graph");
        if path.is_file() {
            return;
        }
        if git_commit_graph_write(self._root.path()).is_err() {
            write_grit_commit_graph(self.repo.odb.objects_dir(), &self.repo.odb)
                .expect("restore commit-graph");
        }
    }

    #[must_use]
    pub fn global() -> &'static Self {
        static FIX: OnceLock<HistoryBenchFixtures> = OnceLock::new();
        FIX.get_or_init(HistoryBenchFixtures::build)
    }
}

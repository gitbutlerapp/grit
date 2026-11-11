//! Criterion bench: [`History::plan_rebase`](grit_lib::revwalk::history::History::plan_rebase)
//! patch-id scan cost.

#![allow(clippy::expect_used, clippy::unwrap_used)]

use std::hint::black_box;

use criterion::{criterion_group, criterion_main, Criterion, Throughput};
use grit_lib::index::write_tree::write_tree_from_index;
use grit_lib::index::{Index, IndexEntry, MODE_REGULAR};
use grit_lib::objects::{serialize_commit, CommitData, ObjectKind};
use grit_lib::repo::{InitOptions, Repository};
use grit_lib::revwalk::history::{RebaseOntoOptions, RebaseRequest, RefScope, RewriteOptions};
use grit_lib::test_support::inject::{
    test_patch_id_compute_call_count, test_reset_patch_id_compute_call_count,
};

/// Onto-side commit count (override with `GRIT_HISTORY_BENCH_COMMITS`; at least 10 000).
fn history_commit_count() -> usize {
    std::env::var("GRIT_HISTORY_BENCH_COMMITS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(10_000)
}

fn index_entry(path: &[u8], oid: grit_lib::objects::ObjectId, size: usize) -> IndexEntry {
    IndexEntry {
        ctime_sec: 1_700_000_000,
        ctime_nsec: 0,
        mtime_sec: 1_700_000_000,
        mtime_nsec: 0,
        dev: 0,
        ino: 0,
        mode: MODE_REGULAR,
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

fn build_rebase_plan_fixture(
    onto_side: usize,
) -> (
    tempfile::TempDir,
    Repository,
    grit_lib::objects::ObjectId,
    grit_lib::objects::ObjectId,
    grit_lib::objects::ObjectId,
) {
    let root = tempfile::tempdir().expect("tempdir");
    let (repo, _) = Repository::init(
        root.path(),
        &InitOptions::new()
            .with_initial_branch("main".to_owned())
            .with_ref_storage(grit_lib::refs::ref_storage::RefStorageFormat::Files),
    )
    .expect("init");
    let mut index = Index::new();
    index.hash_algo = repo.odb().hash_algo();
    let base_blob = repo.odb().write(ObjectKind::Blob, b"base\n").expect("blob");
    index.add_or_replace(index_entry(b"f.txt", base_blob, 5));
    for path in [
        b"wide/shared/a.txt",
        b"wide/shared/b.txt",
        b"wide/shared/c.txt",
    ] {
        let blob = repo.odb().write(ObjectKind::Blob, b"x\n").expect("blob");
        index.add_or_replace(index_entry(path, blob, 2));
    }
    let fork_tree = write_tree_from_index(repo.odb(), &index, "").expect("tree");
    let ident = "Bench <b@grit.test> 1 +0000";
    let fork_raw = serialize_commit(&CommitData::new(fork_tree, vec![], ident, ident, "fork\n"));
    let fork = repo
        .odb()
        .write(ObjectKind::Commit, &fork_raw)
        .expect("fork");
    let mut parent = fork;
    for i in 0..onto_side {
        let body = format!("noise {i}\n");
        let blob = repo
            .odb()
            .write(ObjectKind::Blob, body.as_bytes())
            .expect("blob");
        // Each noise commit adds one file to its parent's tree.
        index.add_or_replace(index_entry(
            format!("n/{i}.txt").as_bytes(),
            blob,
            body.len(),
        ));
        let tree = write_tree_from_index(repo.odb(), &index, "").expect("tree");
        let raw = serialize_commit(&CommitData::new(
            tree,
            vec![parent],
            ident,
            ident,
            format!("noise {i}\n"),
        ));
        parent = repo.odb().write(ObjectKind::Commit, &raw).expect("commit");
    }
    let onto = parent;
    let topic_blob = repo
        .odb()
        .write(ObjectKind::Blob, b"topic\n")
        .expect("blob");
    let mut idx = Index::new();
    idx.hash_algo = repo.odb().hash_algo();
    idx.add_or_replace(index_entry(b"f.txt", topic_blob, 6));
    let tip_tree = write_tree_from_index(repo.odb(), &idx, "").expect("tree");
    let tip_raw = serialize_commit(&CommitData::new(
        tip_tree,
        vec![fork],
        ident,
        ident,
        "topic\n",
    ));
    let tip = repo.odb().write(ObjectKind::Commit, &tip_raw).expect("tip");
    grit_test_support::refs::set_ref(&repo, "refs/heads/main", &tip).expect("ref");
    (root, repo, fork, onto, tip)
}

fn bench_history_plan_rebase(c: &mut Criterion) {
    let onto_side = history_commit_count().max(10_000);
    let (_dir, repo, fork, onto, tip) = build_rebase_plan_fixture(onto_side);
    let opts = RebaseOntoOptions::new(
        RewriteOptions::new("Bench <b@grit.test> 2 +0000".parse().expect("signature"))
            .with_scope(RefScope::Head)
            .with_sign(false),
    );
    let request = RebaseRequest {
        upstream: fork,
        onto: Some(onto),
        branch_tip: tip,
    };
    let mut group = c.benchmark_group("history_plan_rebase");
    group.throughput(Throughput::Elements(onto_side as u64));
    group.bench_function("plan_with_10k_onto_side", |b| {
        b.iter(|| {
            test_reset_patch_id_compute_call_count();
            let plan = repo.history().plan_rebase(&request, &opts).expect("plan");
            black_box((plan, test_patch_id_compute_call_count()))
        });
    });
    group.finish();
}

criterion_group!(benches, bench_history_plan_rebase);
criterion_main!(benches);

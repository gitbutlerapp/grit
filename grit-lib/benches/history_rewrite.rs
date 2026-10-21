//! Criterion bench: replay 1,000 linear commits after rewording the oldest in a wide tree fixture.

#![allow(clippy::expect_used, clippy::unwrap_used)]

mod s7;

use std::hint::black_box;

use criterion::{criterion_group, criterion_main, Criterion, Throughput};
use grit_lib::index::write_tree::{write_tree_update_index, WriteTreeFlags};
use grit_lib::index::{Index, IndexEntry, MODE_REGULAR};
use grit_lib::objects::{parse_commit, serialize_commit, CommitData, ObjectKind};
use grit_lib::refs::write_ref;
use grit_lib::repo::{InitOptions, Repository};
use grit_lib::revwalk::history::{
    plan_rewrite, CommitEdit, EmptyPolicy, HistoryEdits, RefScope, RewriteOptions,
};

use s7::HistoryBenchFixtures;

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

fn build_linear_wide_repo(
    commit_count: usize,
) -> (tempfile::TempDir, Repository, grit_lib::objects::ObjectId) {
    let root = tempfile::tempdir().expect("tempdir");
    let repo = Repository::init(
        root.path(),
        &InitOptions::new()
            .with_initial_branch("main".to_owned())
            .with_ref_storage(grit_lib::RefStorageFormat::Files),
    )
    .expect("init");
    let wide_tree = HistoryBenchFixtures::global().wide_tree;

    let mut parent: Option<grit_lib::objects::ObjectId> = None;
    let mut oldest = None;
    let mut index = Index::new();
    index.hash_algo = repo.odb().hash_algo();

    for i in 0..commit_count {
        let tree = if i == 0 {
            wide_tree
        } else if i % 50 == 0 {
            let body = format!("churn {i}\n");
            let blob = repo
                .odb()
                .write(ObjectKind::Blob, body.as_bytes())
                .expect("blob");
            index.add_or_replace(index_entry(
                format!("churn/{i:04}.txt").as_bytes(),
                blob,
                body.len(),
            ));
            write_tree_update_index(repo.odb(), &mut index, "", WriteTreeFlags::default())
                .expect("tree")
        } else {
            parent
                .and_then(|p| {
                    repo.odb()
                        .read(&p)
                        .ok()
                        .and_then(|obj| parse_commit(&obj.data).ok())
                        .map(|c| c.tree)
                })
                .unwrap_or(wide_tree)
        };

        let ts = 1_700_000_000i64 + i as i64;
        let ident = format!("Bench <b@grit.test> {ts} +0000");
        let raw = serialize_commit(&CommitData {
            tree,
            parents: parent.into_iter().collect(),
            author: ident.clone(),
            committer: ident,
            author_raw: Vec::new(),
            committer_raw: Vec::new(),
            encoding: None,
            message: format!("commit {i}\n"),
            raw_message: None,
            extra_headers: Vec::new(),
        });
        let oid = repo.odb().write(ObjectKind::Commit, &raw).expect("commit");
        if i == 0 {
            oldest = Some(oid);
        }
        parent = Some(oid);
    }
    let head = parent.expect("head");
    write_ref(repo.git_dir(), "refs/heads/main", &head).expect("ref");
    (root, repo, oldest.expect("oldest"))
}

fn bench_history_rewrite(c: &mut Criterion) {
    let count = 1_001usize;
    let (_dir, repo, oldest) = build_linear_wide_repo(count);
    let mut group = c.benchmark_group("history_rewrite");
    group.throughput(Throughput::Elements(1_000));
    group.bench_function("reword_oldest_replay_1000_wide10k", |b| {
        b.iter(|| {
            let mut edits = HistoryEdits::new();
            edits.insert(
                oldest,
                CommitEdit::Rewrite {
                    tree: None,
                    message: Some("reworded oldest subject\n".into()),
                    author: None,
                    parents: None,
                },
            );
            let plan = plan_rewrite(
                &repo,
                &edits,
                &RewriteOptions {
                    scope: RefScope::Branches,
                    empty: EmptyPolicy::Drop,
                    committer: "Bench <b@grit.test> 9 +0000".into(),
                    preserve_committer: false,
                    sign: true,
                },
            )
            .expect("plan");
            black_box(plan.commits.len())
        });
    });
    group.finish();
}

criterion_group!(benches, bench_history_rewrite);
criterion_main!(benches);

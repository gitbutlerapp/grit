//! A backdated push with many advertised haves exercises frontier bookkeeping.

#![allow(clippy::expect_used, clippy::unwrap_used)]

use std::collections::HashSet;
use std::hint::black_box;

use criterion::{criterion_group, criterion_main, Criterion};
use grit_lib::objects::odb::Odb;
use grit_lib::objects::pack_object_select::{
    enumerate_pack_objects, HaveBoundary, PackEnumerateOptions,
};
use grit_lib::objects::{ObjectId, ObjectKind};

fn write_commit(odb: &Odb, tree: ObjectId, parent: Option<ObjectId>, time: usize) -> ObjectId {
    let parent = parent
        .map(|oid| format!("parent {oid}\n"))
        .unwrap_or_default();
    let bytes = format!("tree {tree}\n{parent}author Bench <b@example.com> {time} +0000\ncommitter Bench <b@example.com> {time} +0000\n\nfrontier\n");
    odb.write(ObjectKind::Commit, bytes.as_bytes())
        .expect("commit")
}

fn bench_frontier(c: &mut Criterion) {
    let dir = tempfile::TempDir::new().expect("tempdir");
    let odb = Odb::new(dir.path());
    let tree = odb.write(ObjectKind::Tree, &[]).expect("tree");
    let mut parent = None;
    let mut haves = Vec::new();
    for i in 0..1000 {
        let oid = write_commit(&odb, tree, parent, 1_700_000_000 + i);
        haves.push(oid);
        parent = Some(oid);
    }
    let want = write_commit(&odb, tree, parent, 1);
    let empty = HashSet::new();
    let opts = PackEnumerateOptions {
        filter: None,
        shallow_grafts: &empty,
        have_shallow_grafts: None,
        use_bitmaps: false,
        exclude_objects: None,
        peer_odb: None,
        have_boundary: HaveBoundary::TipsOnly,
    };
    let selection = enumerate_pack_objects(&odb, &[want], &haves, &opts).expect("selection");
    assert_eq!(selection.send, [want]);
    c.bench_function("send_pack/backdated_want_1000_haves", |b| {
        b.iter(|| {
            black_box(enumerate_pack_objects(&odb, &[want], &haves, &opts).expect("selection"))
        });
    });
}

criterion_group!(benches, bench_frontier);
criterion_main!(benches);

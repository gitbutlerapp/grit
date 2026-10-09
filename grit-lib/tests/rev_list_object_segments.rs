//! Public API regression: [`grit_lib::rev_list::RevListResult::object_segments`].

use grit_lib::objects::ObjectKind;
use grit_lib::odb::Odb;
use grit_lib::repo::{init_bare_clone_minimal, Repository};
use grit_lib::rev_list::{rev_list, RevListOptions};
use std::path::Path;
use tempfile::tempdir;

fn write_blob(odb: &Odb, data: &[u8]) -> grit_lib::error::Result<grit_lib::objects::ObjectId> {
    odb.write_loose_materialize(ObjectKind::Blob, data)
}

fn write_tree(
    odb: &Odb,
    entries: &[(u32, &str, grit_lib::objects::ObjectId)],
) -> grit_lib::error::Result<grit_lib::objects::ObjectId> {
    let mut body = Vec::new();
    for (mode, name, oid) in entries {
        body.extend_from_slice(format!("{mode:o} {name}\0").as_bytes());
        body.extend_from_slice(oid.as_bytes());
    }
    odb.write_loose_materialize(ObjectKind::Tree, &body)
}

fn write_commit(
    odb: &Odb,
    tree: grit_lib::objects::ObjectId,
    parents: &[grit_lib::objects::ObjectId],
    msg: &str,
) -> grit_lib::error::Result<grit_lib::objects::ObjectId> {
    let mut body = format!("tree {tree}\n");
    for p in parents {
        body.push_str(&format!("parent {p}\n"));
    }
    body.push_str("author T <t@e.com> 100 +0000\ncommitter T <t@e.com> 100 +0000\n\n");
    body.push_str(msg);
    body.push('\n');
    odb.write_loose_materialize(ObjectKind::Commit, body.as_bytes())
}

fn open_bare(dir: &Path) -> grit_lib::error::Result<Repository> {
    init_bare_clone_minimal(dir, "main", "files")?;
    Repository::open(dir, None)
}

#[test]
fn rev_list_object_segments_one_per_commit_plus_roots_trailer() {
    let dir = tempdir().expect("tempdir");
    let repo = open_bare(dir.path()).expect("repo");
    let blob_a = write_blob(&repo.odb, b"a\n").expect("blob a");
    let tree1 = write_tree(&repo.odb, &[(0o100644, "a.txt", blob_a)]).expect("tree1");
    let root = write_commit(&repo.odb, tree1, &[], "root").expect("root");

    let blob_b = write_blob(&repo.odb, b"b\n").expect("blob b");
    let tree2 = write_tree(
        &repo.odb,
        &[(0o100644, "a.txt", blob_a), (0o100644, "b.txt", blob_b)],
    )
    .expect("tree2");
    let child = write_commit(&repo.odb, tree2, &[root], "child").expect("child");

    let opts = RevListOptions {
        objects: true,
        in_commit_order: false,
        ..Default::default()
    };
    let result = rev_list(&repo, &[child.to_hex()], &[], &opts).expect("rev-list");

    assert_eq!(result.commits.len(), 2);
    assert_eq!(
        result.object_segments.len(),
        result.commits.len() + 1,
        "expected one segment per commit walk plus a trailing object_roots segment"
    );
    assert!(
        result.object_segments.iter().any(|seg| !seg.is_empty()),
        "expected at least one commit segment to list newly discovered objects"
    );
    assert!(
        result.object_segments.last().is_some_and(|s| s.is_empty()),
        "trailing object_roots segment should be empty when no roots were supplied"
    );

    let mut flattened_oids = Vec::new();
    for segment in &result.object_segments {
        flattened_oids.extend(segment.iter().map(|(oid, _)| *oid));
    }
    let flat_oids: Vec<_> = result.objects.iter().map(|(oid, _)| *oid).collect();
    assert_eq!(
        flattened_oids.len(),
        flat_oids.len(),
        "segment flattening should match the flat objects list length"
    );
}

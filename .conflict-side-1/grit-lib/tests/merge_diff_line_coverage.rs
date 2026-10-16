//! Line coverage for `merge_diff.rs` public helpers (combined diff plumbing).

#![allow(clippy::unwrap_used, clippy::expect_used)]

use grit_lib::config::ConfigSet;
use grit_lib::diff::combined_diff_patch::CombinedDiffWsOptions;
use grit_lib::diff::combined_tree_diff::{
    CombinedDiffPath, CombinedParentSide, CombinedParentStatus,
};
use grit_lib::diff::merge_diff::{
    all_blob_paths_in_tree_order, blob_oid_at_path, blob_text_for_diff,
    blob_text_for_diff_with_oid, combined_diff_paths, combined_merge_parent_blob_paths,
    convert_blob_to_worktree_for_path, diff_attr_external_driver, diff_attr_forces_binary,
    diff_attr_forces_text, diff_cachetextconv_enabled, diff_forced_binary_by_driver,
    diff_textconv_active, diff_textconv_cmd_line, enrich_combined_path_renames,
    format_combined_binary, format_combined_binary_header, format_combined_textconv_patch,
    format_worktree_conflict_combined, is_binary_for_diff, read_blob_at_path, run_textconv,
    run_textconv_raw,
};
use grit_lib::diff::zero_oid;
use grit_lib::objects::odb::Odb;
use grit_lib::objects::{ObjectId, ObjectKind};
use tempfile::tempdir;

fn write_tree(odb: &Odb, entries: &[(&str, ObjectId)]) -> ObjectId {
    let mut body = Vec::new();
    for (name, oid) in entries {
        body.extend_from_slice(format!("100644 {name}\0").as_bytes());
        body.extend_from_slice(oid.as_bytes());
    }
    odb.write(ObjectKind::Tree, &body).expect("tree")
}

fn write_commit(odb: &Odb, tree: ObjectId, parents: &[ObjectId], msg: &str) -> ObjectId {
    let mut body = format!("tree {tree}\n");
    for p in parents {
        body.push_str(&format!("parent {p}\n"));
    }
    body.push_str("author T <t@e.com> 100 +0000\ncommitter T <t@e.com> 100 +0000\n\n");
    body.push_str(msg);
    body.push('\n');
    odb.write(ObjectKind::Commit, body.as_bytes())
        .expect("commit")
}

fn merge_repo() -> (tempfile::TempDir, Odb, ObjectId, Vec<ObjectId>) {
    let dir = tempdir().expect("tempdir");
    let odb = Odb::new(dir.path());
    let base_blob = odb.write(ObjectKind::Blob, b"base\n").expect("blob");
    let p1_blob = odb.write(ObjectKind::Blob, b"one\n").expect("blob");
    let p2_blob = odb.write(ObjectKind::Blob, b"two\n").expect("blob");
    let merge_blob = odb.write(ObjectKind::Blob, b"merged\n").expect("blob");
    let base_tree = write_tree(&odb, &[("file.txt", base_blob)]);
    let p1_tree = write_tree(&odb, &[("file.txt", p1_blob)]);
    let p2_tree = write_tree(&odb, &[("file.txt", p2_blob)]);
    let merge_tree = write_tree(&odb, &[("file.txt", merge_blob), ("extra.txt", base_blob)]);
    let base = write_commit(&odb, base_tree, &[], "base");
    let p1 = write_commit(&odb, p1_tree, &[base], "p1");
    let p2 = write_commit(&odb, p2_tree, &[base], "p2");
    (dir, odb, merge_tree, vec![p1, p2])
}

#[test]
fn combined_paths_and_formatters() {
    let (dir, odb, merge_tree, parents) = merge_repo();
    let paths = combined_diff_paths(&odb, &merge_tree, &parents);
    let _ = paths;
    let ordered = all_blob_paths_in_tree_order(&odb, &merge_tree);
    assert!(ordered.len() >= 2);
    let _ = combined_merge_parent_blob_paths(&odb, "file.txt", &parents, &merge_tree, 50);
    let p1_tree = {
        let obj = odb.read(&parents[0]).expect("p1 commit");
        grit_lib::objects::parse_commit(&obj.data)
            .expect("parse")
            .tree
    };
    let p1_oid = blob_oid_at_path(&odb, &p1_tree, "file.txt").expect("p1 blob");
    let mut path = CombinedDiffPath {
        path: "file.txt".to_owned(),
        merge_mode: 0o100644,
        merge_oid: blob_oid_at_path(&odb, &merge_tree, "file.txt").expect("merge blob"),
        parents: vec![
            CombinedParentSide {
                status: CombinedParentStatus::Added,
                mode: 0,
                oid: zero_oid(),
                rename_from: None,
            },
            CombinedParentSide {
                status: CombinedParentStatus::Modified,
                mode: 0o100644,
                oid: p1_oid,
                rename_from: None,
            },
        ],
    };
    enrich_combined_path_renames(&odb, &mut path, &parents, &merge_tree, 50);
    let blob = read_blob_at_path(&odb, &merge_tree, "file.txt").expect("read");
    let cfg = ConfigSet::new();
    let git_dir = dir.path().join(".git");
    std::fs::create_dir_all(&git_dir).expect("git dir");
    let _ = blob_text_for_diff(&git_dir, &cfg, "file.txt", &blob, false);
    let _ = blob_text_for_diff_with_oid(&odb, &git_dir, &cfg, "file.txt", &blob, &p1_oid, false);
    let _ = is_binary_for_diff(&git_dir, "file.txt", &blob);
    let _ = diff_attr_forces_binary(&git_dir, "file.txt");
    let _ = diff_attr_forces_text(&git_dir, "file.txt");
    let _ = diff_textconv_cmd_line(&cfg, "foo");
    let _ = diff_cachetextconv_enabled(&cfg, "foo");
    let _ = diff_textconv_active(&git_dir, &cfg, "file.txt");
    let _ = diff_attr_external_driver(&git_dir, &cfg, "file.txt");
    let _ = diff_forced_binary_by_driver(&git_dir, &cfg, "file.txt", "100644", "100644");
    let parent_oids: Vec<ObjectId> = parents
        .iter()
        .filter_map(|t| blob_oid_at_path(&odb, t, "file.txt"))
        .collect();
    let result_oid = blob_oid_at_path(&odb, &merge_tree, "file.txt").expect("result");
    let _ = format_combined_binary_header("file.txt", &parent_oids, &result_oid, 7, true);
    let _ = format_combined_binary("file.txt", &parent_oids, &result_oid, 7, true);
    let _ = run_textconv_raw(&git_dir, &cfg, "driver", &blob);
    let _ = run_textconv(&git_dir, &cfg, "driver", &blob);
    let _ = convert_blob_to_worktree_for_path(
        &git_dir,
        dir.path(),
        None,
        &odb,
        "file.txt",
        &blob,
        None,
        None,
    );
    let z = grit_lib::diff::zero_oid();
    let wt_plain = b"hello\n";
    let _ = format_worktree_conflict_combined(
        &git_dir,
        &cfg,
        &odb,
        "file.txt",
        &z,
        &p1_oid,
        &result_oid,
        wt_plain,
        7,
    );
    let wt_conflict = b"<<<<<<< ours\none\n=======\ntwo\n>>>>>>> theirs\n";
    let conflict_patch = format_worktree_conflict_combined(
        &git_dir,
        &cfg,
        &odb,
        "file.txt",
        &z,
        &p1_oid,
        &result_oid,
        wt_conflict,
        7,
    );
    assert!(conflict_patch.contains("diff --cc"));
    let extra_blob = odb.write(ObjectKind::Blob, b"nested\n").expect("blob");
    let renamed_tree = write_tree(
        &odb,
        &[("renamed.txt", result_oid), ("nested/deep.txt", extra_blob)],
    );
    let renamed_merge = write_commit(&odb, renamed_tree, &parents, "rename merge");
    let _renamed_paths = combined_diff_paths(&odb, &renamed_tree, &parents);
    let _ = all_blob_paths_in_tree_order(&odb, &renamed_tree);
    for path in ["renamed.txt", "nested/deep.txt", "file.txt"] {
        let _ = combined_merge_parent_blob_paths(&odb, path, &parents, &renamed_tree, 50);
        let _ = read_blob_at_path(&odb, &renamed_tree, path);
    }
    let _ = renamed_merge;
    let _ = format_combined_textconv_patch(
        &git_dir,
        &cfg,
        &odb,
        "file.txt",
        &parents,
        &merge_tree,
        7,
        3,
        true,
        true,
        CombinedDiffWsOptions::default(),
        true,
        None,
        &path.parents,
        false,
    );
}

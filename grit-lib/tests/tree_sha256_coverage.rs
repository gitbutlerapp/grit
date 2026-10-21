//! Coverage touch for public tree-sha256 API items.

#![allow(clippy::unwrap_used)]

use std::str::FromStr;

use grit_lib::error::Error;
use grit_lib::repo::{InitOptions, Repository};
use grit_lib::tree_sha256::{
    path_matches_glob, tree_sha256, MissingSubmodule, MissingSubmoduleReason, TreeSha256,
    TreeSha256Options, TreeSha256Report,
};

#[test]
fn public_tree_sha256_types() {
    let _ = (
        TreeSha256Options::default(),
        MissingSubmoduleReason::NotCheckedOut,
        MissingSubmoduleReason::MissingCommit,
    );
    let digest = <TreeSha256 as FromStr>::from_str(
        "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855",
    )
    .unwrap();
    let _display = format!("{digest}");
    let _debug = format!("{digest:?}");
    assert!(path_matches_glob("a/b", "a/*"));
    assert!(matches!(
        <TreeSha256 as FromStr>::from_str("not-hex"),
        Err(Error::TreeSha256InvalidHex)
    ));
    let tmp = tempfile::tempdir().unwrap();
    let repo = Repository::init(tmp.path(), &InitOptions::default()).unwrap();
    let blob = repo
        .odb()
        .write_loose_materialize(grit_lib::objects::ObjectKind::Blob, b"x")
        .unwrap();
    assert!(matches!(
        tree_sha256(&repo, blob, &TreeSha256Options::default()),
        Err(Error::TreeSha256InvalidRevision { .. })
    ));
    let empty_tree = repo
        .odb()
        .write_loose_materialize(grit_lib::objects::ObjectKind::Tree, b"")
        .unwrap();
    let body =
        format!("tree {empty_tree}\nauthor T <t@e.com> 1 +0000\ncommitter T <t@e.com> 1 +0000\n\n");
    let commit = repo
        .odb()
        .write_loose_materialize(grit_lib::objects::ObjectKind::Commit, body.as_bytes())
        .unwrap();
    let report: TreeSha256Report =
        tree_sha256(&repo, commit, &TreeSha256Options::default()).unwrap();
    let _ = report.digest.to_hex();
    let _ = report.files;
    let _ = report.submodules;
    let _ = report.excluded;
    let _ = report.cycles;
    let _miss: MissingSubmodule = MissingSubmodule {
        path: "x".into(),
        reason: MissingSubmoduleReason::NotCheckedOut,
    };
    match tree_sha256(&repo, commit, &TreeSha256Options::default()) {
        Ok(_) | Err(Error::TreeSha256MissingSubmodules(_)) => {}
        Err(e) => panic!("unexpected {e:?}"),
    }
}

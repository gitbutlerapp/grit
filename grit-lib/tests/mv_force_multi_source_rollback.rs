//! Forced multi-source move rollback when a later source is missing from the work tree.

use std::fs;
use std::path::Path;

use grit_lib::porcelain::add::{stage, StageOptions};
use grit_lib::porcelain::paths::{move_paths, MoveOptions};
use grit_lib::progress::NullProgress;
use grit_lib::repo::{InitOptions, Repository};

fn init_repo(root: &Path) -> Repository {
    Repository::init(root, &InitOptions::default()).expect("init")
}

#[test]
fn multi_source_missing_later_source_rolls_back_earlier_forced_move() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();
    let repo = init_repo(root);

    fs::create_dir_all(root.join("src")).expect("src");
    fs::create_dir_all(root.join("dest")).expect("dest");
    fs::write(root.join("src/f0000"), b"SOURCE-0\n").expect("source 0");
    fs::write(root.join("src/f0001"), b"SOURCE-1\n").expect("source 1");
    fs::write(root.join("dest/f0000"), b"OLDDEST-0\n").expect("old 0");
    fs::write(root.join("dest/f0001"), b"OLDDEST-1\n").expect("old 1");
    stage(&repo, &StageOptions::default(), &mut NullProgress).expect("stage");

    fs::remove_file(root.join("src/f0001")).expect("drop later source");

    let err = move_paths(
        &repo,
        &MoveOptions {
            sources: vec!["src/f0000".into(), "src/f0001".into()],
            destination: "dest".into(),
            force: true,
            dry_run: false,
        },
    )
    .expect_err("missing later source");

    assert!(
        matches!(err, grit_lib::error::Error::MoveBadSource { .. }),
        "unexpected error: {err:?}"
    );
    assert_eq!(
        fs::read_to_string(root.join("src/f0000")).expect("src f0000"),
        "SOURCE-0\n"
    );
    assert_eq!(
        fs::read_to_string(root.join("dest/f0000")).expect("dest f0000"),
        "OLDDEST-0\n"
    );
}

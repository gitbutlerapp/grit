//! Bulk checkout (>32 paths) must still apply per-path gitattributes (issue #2280 review).

use std::fs;
use std::path::Path;

use grit_lib::config::environment::RepositoryOptions;
use grit_lib::objects::ObjectId;
use grit_lib::porcelain::checkout::checkout_between_trees_with_progress;
use grit_lib::progress::{NeverCancel, NullProgress};
use grit_lib::repo::Repository;
use grit_test_support::git;

const LF_FILE_COUNT: usize = 21;
const CRLF_FILE_COUNT: usize = 20;
const TOTAL_FILES: usize = LF_FILE_COUNT + CRLF_FILE_COUNT;

fn init_mixed_eol_repo(root: &Path) -> (ObjectId, ObjectId) {
    git(root, &["init", "-q", "-b", "main"]);
    git(root, &["config", "user.name", "Test"]);
    git(root, &["config", "user.email", "t@example.com"]);
    fs::write(
        root.join(".gitattributes"),
        "*.txt text eol=lf\n*.crlf text eol=crlf\n",
    )
    .expect("attrs");
    for i in 0..LF_FILE_COUNT {
        fs::write(root.join(format!("f{i}.txt")), b"base\n").expect("base txt");
    }
    for i in 0..CRLF_FILE_COUNT {
        fs::write(root.join(format!("f{i}.crlf")), b"base\n").expect("base crlf");
    }
    git(root, &["add", "-A"]);
    git(root, &["commit", "-qm", "base"]);
    let main_tree =
        ObjectId::from_hex(git(root, &["rev-parse", "main^{tree}"]).trim()).expect("main tree");

    git(root, &["checkout", "-q", "-b", "other"]);
    for i in 0..LF_FILE_COUNT {
        fs::write(root.join(format!("f{i}.txt")), b"other\n").expect("other txt");
    }
    for i in 0..CRLF_FILE_COUNT {
        fs::write(root.join(format!("f{i}.crlf")), b"other\n").expect("other crlf");
    }
    git(root, &["commit", "-qam", "other"]);
    let other_tree =
        ObjectId::from_hex(git(root, &["rev-parse", "other^{tree}"]).trim()).expect("other tree");
    git(root, &["checkout", "-q", "main"]);
    (main_tree, other_tree)
}

#[test]
fn bulk_checkout_applies_path_specific_eol_crlf() {
    assert!(
        TOTAL_FILES > 32,
        "fixture must exceed bulk checkout threshold ({TOTAL_FILES} paths)"
    );
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();
    let (main_tree, other_tree) = init_mixed_eol_repo(root);
    let repo = Repository::discover(Some(root), &RepositoryOptions::empty()).expect("open");

    checkout_between_trees_with_progress(
        &repo,
        Some(&main_tree),
        &other_tree,
        &mut NullProgress,
        &NeverCancel,
    )
    .expect("grit checkout");

    let lf_bytes = fs::read(root.join("f0.txt")).expect("grit lf file");
    assert_eq!(
        lf_bytes, b"other\n",
        "bulk checkout must keep eol=lf paths on LF line endings"
    );
    let crlf_bytes = fs::read(root.join("f0.crlf")).expect("grit crlf file");
    assert_eq!(
        crlf_bytes, b"other\r\n",
        "bulk checkout must smudge eol=crlf paths to CRLF on disk"
    );

    git(root, &["checkout", "-q", "main"]);
    git(root, &["checkout", "-q", "other"]);
    assert_eq!(
        lf_bytes,
        fs::read(root.join("f0.txt")).expect("git lf file"),
        "grit and git must materialize identical bytes for *.txt"
    );
    assert_eq!(
        crlf_bytes,
        fs::read(root.join("f0.crlf")).expect("git crlf file"),
        "grit and git must materialize identical bytes for *.crlf"
    );
    git(root, &["fsck", "--strict"]);
}

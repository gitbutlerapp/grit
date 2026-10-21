//! [`TreeRefIter`] must match [`parse_tree`] on real trees and preserve parse errors.

use std::collections::VecDeque;

use grit_lib::diff::tree_oid_of_commit;
use grit_lib::error::Error;
use grit_lib::objects::bench_fixture::synth::{generate_synth_repo_with_spec, RepoShape};
use grit_lib::objects::{
    parse_tree, parse_tree_with_oid_len, HashAlgo, ObjectId, ObjectKind, TreeEntry, TreeRefIter,
};
use grit_lib::objects::odb::Odb;
use grit_lib::repo::Repository;
use grit_test_support::objects::{git_supports_sha256, HashAlgo as FixtureAlgo, RepoFixture};

fn entries_from_ref_iter(data: &[u8]) -> grit_lib::error::Result<Vec<TreeEntry>> {
    TreeRefIter::infer_oid_len(data)?
        .map(|entry| entry.map(|e| e.to_owned()))
        .collect()
}

fn assert_tree_parsers_match(data: &[u8]) {
    let parsed = parse_tree(data).expect("parse_tree");
    let from_iter = entries_from_ref_iter(data).expect("TreeRefIter::infer_oid_len");
    assert_eq!(parsed, from_iter);

    let oid_len = if parsed.is_empty() {
        HashAlgo::Sha1.len()
    } else {
        parsed[0].oid.as_bytes().len()
    };
    let explicit: Vec<_> = TreeRefIter::with_oid_len(data, oid_len)
        .map(|entry| entry.expect("entry").to_owned())
        .collect();
    assert_eq!(parsed, explicit);
}

fn walk_trees_from_root(odb: &Odb, root: ObjectId) -> grit_lib::error::Result<()> {
    let mut queue = VecDeque::from([root]);
    let oid_len = odb.hash_algo().len();
    while let Some(tree_oid) = queue.pop_front() {
        let obj = odb.read(&tree_oid)?;
        assert_eq!(obj.kind, ObjectKind::Tree);
        assert_tree_parsers_match(&obj.data);
        for entry in TreeRefIter::with_oid_len(&obj.data, oid_len) {
            let entry = entry?;
            if entry.mode == 0o040000 {
                queue.push_back(entry.oid);
            }
        }
    }
    Ok(())
}

fn trees_from_repo_head(repo: &Repository) -> grit_lib::error::Result<()> {
    let head = grit_lib::refs::resolve_ref(repo.git_dir(), "HEAD")?;
    let tree = tree_oid_of_commit(repo.odb(), &head)?;
    walk_trees_from_root(repo.odb(), tree)
}

#[test]
fn tree_ref_iter_matches_parse_tree_bench_shapes() {
    for shape in [RepoShape::Small, RepoShape::Synth10k] {
        let dir = tempfile::tempdir().expect("tempdir");
        let spec = shape.spec(true);
        let report = generate_synth_repo_with_spec(dir.path(), spec).expect("synth repo");
        let repo = Repository::open(
            &report.repo_path.join(".git"),
            &grit_lib::config::environment::RepositoryOptions::empty()
                .with_work_tree(&report.repo_path),
        )
        .expect("open synth");
        trees_from_repo_head(&repo).expect("walk trees");
    }
}

#[test]
fn tree_ref_iter_matches_parse_tree_sha256_repo() {
    if !git_supports_sha256() {
        return;
    }
    let fixture = RepoFixture::init(FixtureAlgo::Sha256).expect("init sha256");
    assert!(
        fixture
            .git(&["commit", "--allow-empty", "-m", "seed"])
            .ok,
        "seed commit"
    );
    let tree_oid = fixture
        .git(&["rev-parse", "HEAD^{tree}"])
        .stdout
        .trim()
        .to_string();
    let tree_body = fixture.git(&["cat-file", "-p", &tree_oid]).stdout;
    assert_tree_parsers_match(tree_body.as_bytes());

    let repo = Repository::open(
        fixture.path().join(".git").as_path(),
        &grit_lib::config::environment::RepositoryOptions::empty().with_work_tree(fixture.path()),
    )
    .expect("open");
    trees_from_repo_head(&repo).expect("walk");
}

#[test]
fn tree_ref_iter_malformed_matches_parse_tree_errors() {
    let bad = b"100644 incomplete";
    let parse_err = parse_tree(bad).expect_err("parse_tree");
    let iter_err = entries_from_ref_iter(bad).expect_err("infer iter");
    assert_eq!(format!("{parse_err}"), format!("{iter_err}"));

    let truncated = b"100644 file\0\x01\x02";
    let parse_err = parse_tree_with_oid_len(truncated, HashAlgo::Sha1.len()).expect_err("trunc");
    let iter_err = TreeRefIter::with_oid_len(truncated, HashAlgo::Sha1.len())
        .collect::<Result<Vec<_>, _>>()
        .expect_err("iter trunc");
    assert_eq!(format!("{parse_err}"), format!("{iter_err}"));
    assert!(matches!(parse_err, Error::CorruptObject(_)));
}

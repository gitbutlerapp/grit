//! Smoke test for parallel commit-tree structured diff (library path behind `grit diff A B`).

use grit_lib::diff::{DiffOptions, TreeDiffOptions};
use grit_lib::objects::FileMode;
use grit_lib::objects::{serialize_tree, ObjectKind, TreeEntry};
use grit_lib::repo::{InitOptions, Repository};

const FILE_COUNT: usize = 48;

fn write_tree(repo: &Repository, payloads: &[(&str, &str)]) -> grit_lib::objects::ObjectId {
    let odb = repo.odb();
    let entries: Vec<TreeEntry> = payloads
        .iter()
        .map(|(name, text)| {
            let oid = odb.write(ObjectKind::Blob, text.as_bytes()).expect("blob");
            TreeEntry::new(name.as_bytes().to_vec(), FileMode::REGULAR, oid)
        })
        .collect();
    odb.write(ObjectKind::Tree, &serialize_tree(&entries))
        .expect("tree")
}

#[test]
fn commit_range_structured_diff_parallel_matches_serial() {
    let dir = tempfile::tempdir().expect("tempdir");
    let (repo, _) = Repository::init(dir.path(), &InitOptions::default()).expect("init");
    let mut old_payloads = Vec::new();
    let mut new_payloads = Vec::new();
    for i in 0..FILE_COUNT {
        let name = format!("f/{i:03}.txt");
        old_payloads.push((name.clone(), format!("old {i}\n")));
        new_payloads.push((name, format!("new {i}\n")));
    }
    let old_refs: Vec<(&str, &str)> = old_payloads
        .iter()
        .map(|(n, t)| (n.as_str(), t.as_str()))
        .collect();
    let new_refs: Vec<(&str, &str)> = new_payloads
        .iter()
        .map(|(n, t)| (n.as_str(), t.as_str()))
        .collect();
    let old_tree = write_tree(&repo, &old_refs);
    let new_tree = write_tree(&repo, &new_refs);
    let entries = repo
        .diff()
        .tree_to_tree(
            Some(&old_tree),
            Some(&new_tree),
            &TreeDiffOptions::default(),
        )
        .map(|r| r.entries)
        .expect("diff");
    assert_eq!(entries.len(), FILE_COUNT);

    let opts = DiffOptions::default()
        .with_context_lines(3)
        .with_word_level_inline(false);
    let serial = repo
        .diff()
        .structured(entries.clone(), &opts)
        .expect("serial");
    let parallel = repo.diff().structured(entries, &opts).expect("parallel");
    assert_eq!(serial, parallel);
    assert!(serial.iter().all(|f| !f.structured.hunks.is_empty()));
}

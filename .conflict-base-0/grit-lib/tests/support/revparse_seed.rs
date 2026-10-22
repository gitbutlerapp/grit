//! Shared helpers for seeding ambiguous short-hex prefixes in coverage tests.

use std::fs;

use grit_lib::config::environment::RepositoryOptions;
use grit_lib::objects::{ObjectId, ObjectKind};
use grit_lib::refs::ref_storage::RefStorageFormat;
use grit_lib::repo::{InitOptions, Repository};

pub fn ambiguous_hex_prefix_among(
    oids: &[ObjectId],
    min_len: usize,
    max_len: usize,
) -> Option<String> {
    for i in 0..oids.len() {
        for j in (i + 1)..oids.len() {
            let a = oids[i].to_hex();
            let b = oids[j].to_hex();
            let cap = max_len.min(a.len()).min(b.len());
            for len in (min_len..=cap).rev() {
                if a[..len] == b[..len] {
                    return Some(a[..len].to_string());
                }
            }
        }
    }
    None
}

/// Bare repository with many commits and a guaranteed shared hex prefix (4+ chars).
pub fn bare_repo_with_ambiguous_prefix(
    max_commits: usize,
) -> (tempfile::TempDir, Repository, String) {
    let dir = tempfile::tempdir().expect("tempdir");
    Repository::init(
        dir.path(),
        &InitOptions::new()
            .with_bare(true)
            .with_initial_branch("main")
            .with_ref_storage(RefStorageFormat::Files),
    )
    .expect("init");
    let repo = Repository::open(dir.path(), &RepositoryOptions::empty()).expect("open");
    let tree = repo
        .odb()
        .write_loose_materialize(ObjectKind::Tree, b"")
        .expect("tree");
    let mut oids = Vec::new();
    let mut counter = 0_i32;
    let prefix = loop {
        let body = format!(
            "tree {tree}\nauthor T <t@e.com> {counter} +0000\ncommitter T <t@e.com> {counter} +0000\n\nseed{counter}\n"
        );
        counter += 1;
        let oid = repo
            .odb()
            .write_loose_materialize(ObjectKind::Commit, body.as_bytes())
            .expect("commit");
        oids.push(oid);
        if let Some(p) = ambiguous_hex_prefix_among(&oids, 4, 12) {
            break p;
        }
        assert!(
            oids.len() < max_commits,
            "could not seed ambiguous commits for coverage"
        );
    };
    let tip = *oids.last().expect("tip");
    fs::write(repo.git_dir().join("refs/heads/main"), format!("{tip}\n")).expect("main");
    (dir, repo, prefix)
}

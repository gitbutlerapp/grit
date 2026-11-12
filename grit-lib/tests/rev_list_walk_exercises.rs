//! Additional `rev_list` walks on synthetic repos to exercise simplification and object paths.

use grit_lib::progress::{CancelToken, NullProgress};

use grit_lib::config::environment::RepositoryOptions;
use grit_lib::objects::odb::Odb;
use grit_lib::objects::{ObjectId, ObjectKind};
use grit_lib::repo::{InitOptions, Repository};
use grit_lib::revwalk::rev_list::{MissingAction, ObjectFilter, OrderingMode, RevListOptions};
use std::collections::HashSet;
use std::fs;
use std::path::Path;
use tempfile::tempdir;

fn write_blob(odb: &Odb, data: &[u8]) -> grit_lib::error::Result<ObjectId> {
    odb.write_loose_materialize(ObjectKind::Blob, data)
}

fn write_tree(odb: &Odb, entries: &[(u32, &str, ObjectId)]) -> grit_lib::error::Result<ObjectId> {
    let mut body = Vec::new();
    for (mode, name, oid) in entries {
        body.extend_from_slice(format!("{mode:o} {name}\0").as_bytes());
        body.extend_from_slice(oid.as_bytes());
    }
    odb.write_loose_materialize(ObjectKind::Tree, &body)
}

fn write_commit(
    odb: &Odb,
    tree: ObjectId,
    parents: &[ObjectId],
    msg: &str,
) -> grit_lib::error::Result<ObjectId> {
    let mut body = format!("tree {tree}\n");
    for p in parents {
        body.push_str(&format!("parent {p}\n"));
    }
    body.push_str("author T <t@e.com> 100 +0000\ncommitter T <t@e.com> 100 +0000\n\n");
    body.push_str(msg);
    body.push('\n');
    odb.write_loose_materialize(ObjectKind::Commit, body.as_bytes())
}

fn write_tag(odb: &Odb, target: ObjectId, name: &str) -> grit_lib::error::Result<ObjectId> {
    let body = format!("object {target}\ntype commit\ntag {name}\n\nannotated\n");
    odb.write_loose_materialize(ObjectKind::Tag, body.as_bytes())
}

fn open_bare(dir: &Path) -> grit_lib::error::Result<Repository> {
    Repository::init(
        dir,
        &InitOptions::new()
            .with_bare(true)
            .with_initial_branch("main".to_owned())
            .with_ref_storage(grit_lib::refs::ref_storage::RefStorageFormat::Files),
    )?;
    Repository::open(dir, &RepositoryOptions::empty())
}

fn seed_linear(repo: &Repository, n: usize) -> Vec<ObjectId> {
    let mut commits = Vec::new();
    let mut parent: Option<ObjectId> = None;
    for i in 0..n {
        let blob = write_blob(repo.odb(), format!("file{i}\n").as_bytes()).expect("blob");
        let tree = write_tree(repo.odb(), &[(0o100644, "path/file", blob)]).expect("tree");
        let parents: Vec<ObjectId> = parent.into_iter().collect();
        let c = write_commit(repo.odb(), tree, &parents, &format!("c{i}")).expect("commit");
        parent = Some(c);
        commits.push(c);
        fs::write(repo.git_dir().join("refs/heads/main"), format!("{c}\n")).expect("ref");
    }
    commits
}

fn seed_merge(repo: &Repository) -> (ObjectId, ObjectId, ObjectId) {
    let blob = write_blob(repo.odb(), b"shared\n").expect("blob");
    let tree = write_tree(repo.odb(), &[(0o100644, "path/file", blob)]).expect("tree");
    let base = write_commit(repo.odb(), tree, &[], "base").expect("base");
    let blob2 = write_blob(repo.odb(), b"side\n").expect("blob2");
    let tree2 = write_tree(
        repo.odb(),
        &[(0o100644, "path/file", blob), (0o100644, "other", blob2)],
    )
    .expect("tree2");
    let side = write_commit(repo.odb(), tree2, &[base], "side").expect("side");
    let tip = write_commit(repo.odb(), tree2, &[base, side], "merge").expect("merge");
    fs::write(repo.git_dir().join("refs/heads/main"), format!("{tip}\n")).expect("ref");
    fs::write(repo.git_dir().join("refs/heads/side"), format!("{side}\n")).expect("side ref");
    (base, side, tip)
}

#[test]
fn path_limited_simplify_and_full_history() {
    let dir = tempdir().expect("tempdir");
    let repo = open_bare(dir.path()).expect("repo");
    let commits = seed_linear(&repo, 4);
    let tip = commits.last().copied().expect("tip");
    for (full, simplify) in [(false, false), (true, false), (false, true), (true, true)] {
        let opts = RevListOptions::default()
            .with_paths(vec!["path/file".to_owned()])
            .with_full_history(full)
            .with_simplify_merges(simplify)
            .with_show_pulls(full && simplify);
        let out = repo
            .revwalk()
            .rev_list(
                &[tip.to_hex()],
                &[],
                &opts,
                &mut NullProgress,
                &CancelToken::never(),
            )
            .expect("rev-list");
        assert!(!out.commits.is_empty());
    }
}

#[test]
fn object_walk_filters() {
    let dir = tempdir().expect("tempdir");
    let repo = open_bare(dir.path()).expect("repo");
    let commits = seed_linear(&repo, 2);
    let tip = commits.last().copied().expect("tip");
    let filter = ObjectFilter::Combine(vec![
        ObjectFilter::BlobLimit(1024),
        ObjectFilter::TreeDepth(2),
    ]);
    let opts = RevListOptions::default()
        .with_objects(true)
        .with_filter(Some(filter))
        .with_filter_print_omitted(true)
        .with_filter_provided_objects(true);
    let out = repo
        .revwalk()
        .rev_list(
            &[tip.to_hex()],
            &[],
            &opts,
            &mut NullProgress,
            &CancelToken::never(),
        )
        .expect("objects");
    assert!(!out.objects.is_empty());
}

#[test]
fn merge_bases_linear() {
    let dir = tempdir().expect("tempdir");
    let repo = open_bare(dir.path()).expect("repo");
    let commits = seed_linear(&repo, 5);
    let tip = *commits.last().expect("tip");
    let mid = commits[2];
    let bases = repo
        .revwalk()
        .merge_bases(&[tip, mid], grit_lib::revwalk::MergeBaseMode::All)
        .expect("merge-base");
    assert_eq!(bases, vec![mid]);
}

#[test]
fn merge_history_simplify_sparse_and_ordering() {
    let dir = tempdir().expect("tempdir");
    let repo = open_bare(dir.path()).expect("repo");
    let (_base, _side, tip) = seed_merge(&repo);
    for ordering in [
        OrderingMode::Default,
        OrderingMode::Topo,
        OrderingMode::DateOrderWalk,
        OrderingMode::AuthorDateWalk,
        OrderingMode::AuthorDateTopo,
    ] {
        let opts = RevListOptions::default()
            .with_paths(vec!["path/file".to_owned()])
            .with_simplify_merges(true)
            .with_sparse(true)
            .with_ordering(ordering)
            .with_first_parent(ordering == OrderingMode::Default);
        let out = repo
            .revwalk()
            .rev_list(
                &[tip.to_hex()],
                &[],
                &opts,
                &mut NullProgress,
                &CancelToken::never(),
            )
            .expect("walk");
        assert!(!out.commits.is_empty());
    }
}

#[test]
fn rev_list_limit_boundary_cherry_and_all_refs() {
    let dir = tempdir().expect("tempdir");
    let repo = open_bare(dir.path()).expect("repo");
    let commits = seed_linear(&repo, 4);
    let tip = commits.last().copied().expect("tip");
    let opts = RevListOptions::default()
        .with_max_count(Some(2))
        .with_skip(1)
        .with_objects(true)
        .with_in_commit_order(true)
        .with_missing_action(MissingAction::Allow)
        .with_parent_rewrite(true)
        .with_reverse(true)
        .with_boundary(true)
        .with_cherry_mark(true)
        .with_all_refs(true);
    let out = repo
        .revwalk()
        .rev_list(
            &[tip.to_hex()],
            &[],
            &opts,
            &mut NullProgress,
            &CancelToken::never(),
        )
        .expect("limited");
    assert!(out.commits.len() <= 2);
    assert!(!out.objects.is_empty());
}

#[test]
fn revision_resolve_and_tag_targets() {
    let dir = tempdir().expect("tempdir");
    let repo = open_bare(dir.path()).expect("repo");
    seed_linear(&repo, 2);
    let resolved = repo
        .revwalk()
        .resolve_commits(&["main".to_owned()])
        .expect("resolve");
    assert_eq!(resolved.len(), 1);
    let tags = repo.revwalk().tag_targets().expect("tags");
    assert!(tags.is_empty());
}

#[test]
fn shallow_and_filtered_reachability() {
    let dir = tempdir().expect("tempdir");
    let repo = open_bare(dir.path()).expect("repo");
    let commits = seed_linear(&repo, 3);
    let tip = commits.last().copied().expect("tip");
    fs::write(repo.git_dir().join("shallow"), format!("{tip}\n")).expect("shallow");
    let bounds = repo.revwalk().shallow_boundary_oids();
    assert!(bounds.contains(&tip));
    let walk = |filter: Option<ObjectFilter>| {
        let opts = RevListOptions::default()
            .with_all_refs(true)
            .with_objects(true)
            .with_no_object_names(true)
            .with_filter(filter);
        repo.revwalk()
            .rev_list(&[], &[], &opts, &mut NullProgress, &CancelToken::never())
            .expect("walk")
            .objects
            .len()
    };
    assert!(walk(Some(ObjectFilter::BlobNone)) < walk(None));
}

#[test]
fn tagged_tip_object_walk_and_symmetric_range() {
    let dir = tempdir().expect("tempdir");
    let repo = open_bare(dir.path()).expect("repo");
    let commits = seed_linear(&repo, 3);
    let tip = commits.last().copied().expect("tip");
    let tag_oid = write_tag(repo.odb(), tip, "v1").expect("tag");
    fs::write(repo.git_dir().join("refs/tags/v1"), format!("{tag_oid}\n")).expect("tag ref");
    let opts = RevListOptions::default()
        .with_objects(true)
        .with_no_object_names(true);
    let out = repo
        .revwalk()
        .rev_list(
            &[String::from("refs/tags/v1")],
            &[],
            &opts,
            &mut NullProgress,
            &CancelToken::never(),
        )
        .expect("tag walk");
    assert!(!out.objects.is_empty());
    let range = repo
        .revwalk()
        .rev_list(
            &[commits[2].to_hex()],
            &[commits[0].to_hex()],
            &RevListOptions::default().with_left_right(true),
            &mut NullProgress,
            &CancelToken::never(),
        )
        .expect("range");
    assert!(!range.commits.is_empty());
    let tag_set: HashSet<_> = repo
        .revwalk()
        .tag_targets()
        .expect("tags")
        .into_iter()
        .collect();
    assert!(tag_set.contains(&tag_oid));
}

#[test]
fn dense_pathspec_visibility_and_parent_filters() {
    let dir = tempdir().expect("tempdir");
    let repo = open_bare(dir.path()).expect("repo");
    let (_base, _side, tip) = seed_merge(&repo);
    let out = repo
        .revwalk()
        .rev_list(
            &[tip.to_hex()],
            &[],
            &RevListOptions::default()
                .with_min_parents(Some(2))
                .with_max_parents(Some(2))
                .with_until_cutoff(Some(i64::MAX))
                .with_since_cutoff(Some(0))
                .with_show_pulls(true)
                .with_exclude_promisor_objects(true),
            &mut NullProgress,
            &CancelToken::never(),
        )
        .expect("parents filter");
    assert_eq!(out.commits.len(), 1);
    let sparse = ObjectFilter::SparseOid(format!("{}:patterns", tip.to_hex()));
    let _ = repo.revwalk().rev_list(
        &[tip.to_hex()],
        &[],
        &RevListOptions::default()
            .with_objects(true)
            .with_filter(Some(sparse)),
        &mut NullProgress,
        &CancelToken::never(),
    );
}

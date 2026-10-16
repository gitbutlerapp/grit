//! Additional `rev_list` walks on synthetic repos to exercise simplification and object paths.

use grit_lib::config::environment::RepositoryOptions;
use grit_lib::objects::odb::Odb;
use grit_lib::objects::{ObjectId, ObjectKind};
use grit_lib::refs;
use grit_lib::refs::ref_exclusions::RefExclusions;
use grit_lib::repo::{InitOptions, Repository};
use grit_lib::revwalk::rev_list::{
    commit_tips_from_named_refs, commit_visible_for_dense_pathspecs, merge_bases,
    object_ids_for_cat_file_filtered, reachable_object_ids_for_cat_file, render_commit,
    resolve_object_walk_roots, resolve_revision_commits, rev_list, select_bisect_commits,
    shallow_borders_reachable_from_wants, shallow_boundary_oids, tag_targets, MissingAction,
    ObjectFilter, OrderingMode, OutputMode, RevListOptions,
};
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
        let opts = RevListOptions {
            paths: vec!["path/file".to_owned()],
            full_history: full,
            simplify_merges: simplify,
            show_pulls: full && simplify,
            ..RevListOptions::default()
        };
        let out = rev_list(&repo, &[tip.to_hex()], &[], &opts).expect("rev-list");
        assert!(!out.commits.is_empty());
    }
}

#[test]
fn object_walk_filters_and_roots() {
    let dir = tempdir().expect("tempdir");
    let repo = open_bare(dir.path()).expect("repo");
    let commits = seed_linear(&repo, 2);
    let tip = commits.last().copied().expect("tip");
    let filter = ObjectFilter::Combine(vec![
        ObjectFilter::BlobLimit(1024),
        ObjectFilter::TreeDepth(2),
    ]);
    let opts = RevListOptions {
        objects: true,
        filter: Some(filter.clone()),
        filter_print_omitted: true,
        filter_provided_objects: true,
        ..RevListOptions::default()
    };
    let out = rev_list(&repo, &[tip.to_hex()], &[], &opts).expect("objects");
    assert!(!out.objects.is_empty());
    let roots = resolve_object_walk_roots(&repo, &[tip.to_hex()]).expect("roots");
    assert!(!roots.1.is_empty() || !roots.0.is_empty());
    let cat_ids = object_ids_for_cat_file_filtered(&repo, &filter).expect("cat");
    assert!(!cat_ids.is_empty());
}

#[test]
fn bisect_merge_bases_and_render() {
    let dir = tempdir().expect("tempdir");
    let repo = open_bare(dir.path()).expect("repo");
    let commits = seed_linear(&repo, 5);
    let tip = *commits.last().expect("tip");
    let mid = commits[2];
    let selection = select_bisect_commits(&repo, &commits, false).expect("bisect");
    assert_eq!(selection.all.len(), commits.len());
    let bases = merge_bases(&repo, tip, mid, false).expect("merge-base");
    assert!(!bases.is_empty());
    let bases_fp = merge_bases(&repo, tip, mid, true).expect("merge-base fp");
    assert!(!bases_fp.is_empty());
    let line =
        render_commit(&repo, tip, &OutputMode::Format(String::from("%H %s")), 40).expect("render");
    assert!(line.contains('c'));
    let oid_line = render_commit(&repo, tip, &OutputMode::OidOnly, 7).expect("oid");
    assert_eq!(oid_line, tip.to_hex());
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
        let opts = RevListOptions {
            paths: vec!["path/file".to_owned()],
            simplify_merges: true,
            sparse: true,
            ordering,
            first_parent: ordering == OrderingMode::Default,
            ..RevListOptions::default()
        };
        let out = rev_list(&repo, &[tip.to_hex()], &[], &opts).expect("walk");
        assert!(!out.commits.is_empty());
    }
}

#[test]
fn rev_list_limit_boundary_cherry_and_all_refs() {
    let dir = tempdir().expect("tempdir");
    let repo = open_bare(dir.path()).expect("repo");
    let commits = seed_linear(&repo, 4);
    let tip = commits.last().copied().expect("tip");
    let opts = RevListOptions {
        max_count: Some(2),
        skip: 1,
        objects: true,
        in_commit_order: true,
        missing_action: MissingAction::Allow,
        output_mode: OutputMode::Parents,
        reverse: true,
        boundary: true,
        cherry_mark: true,
        all_refs: true,
        ..RevListOptions::default()
    };
    let out = rev_list(&repo, &[tip.to_hex()], &[], &opts).expect("limited");
    assert!(out.commits.len() <= 2);
    assert!(!out.objects.is_empty());
    let parents_line = render_commit(&repo, tip, &OutputMode::Parents, 40).expect("parents");
    assert!(parents_line.contains(' '));
}

#[test]
fn ref_tips_exclusions_and_revision_resolve() {
    let dir = tempdir().expect("tempdir");
    let repo = open_bare(dir.path()).expect("repo");
    let commits = seed_linear(&repo, 2);
    let hidden = commits[0];
    fs::create_dir_all(repo.git_dir().join("refs/hidden")).expect("hidden dir");
    fs::write(
        repo.git_dir().join("refs/hidden/only"),
        format!("{hidden}\n"),
    )
    .expect("hidden ref");
    let pairs = refs::list_refs(repo.git_dir(), "refs/").expect("refs");
    let mut exclusions = RefExclusions::default();
    exclusions.add_excluded_ref("refs/hidden/*");
    let tips = commit_tips_from_named_refs(&repo, &pairs, &exclusions).expect("tips");
    assert!(!tips.contains(&hidden));
    let resolved = resolve_revision_commits(&repo, &["main".to_owned()]).expect("resolve");
    assert_eq!(resolved.len(), 1);
    let tags = tag_targets(repo.git_dir()).expect("tags");
    assert!(tags.is_empty());
}

#[test]
fn shallow_and_cat_file_reachability() {
    let dir = tempdir().expect("tempdir");
    let repo = open_bare(dir.path()).expect("repo");
    let commits = seed_linear(&repo, 3);
    let tip = commits.last().copied().expect("tip");
    fs::write(repo.git_dir().join("shallow"), format!("{tip}\n")).expect("shallow");
    let bounds = shallow_boundary_oids(repo.git_dir());
    assert!(bounds.contains(&tip));
    let borders = shallow_borders_reachable_from_wants(&repo, &[tip]);
    assert_eq!(borders, vec![tip]);
    let all = reachable_object_ids_for_cat_file(&repo, None, false).expect("all");
    let none = reachable_object_ids_for_cat_file(&repo, Some(&ObjectFilter::BlobNone), false)
        .expect("no blobs");
    assert!(none.len() < all.len());
}

#[test]
fn tagged_tip_object_walk_and_symmetric_range() {
    let dir = tempdir().expect("tempdir");
    let repo = open_bare(dir.path()).expect("repo");
    let commits = seed_linear(&repo, 3);
    let tip = commits.last().copied().expect("tip");
    let tag_oid = write_tag(repo.odb(), tip, "v1").expect("tag");
    fs::write(repo.git_dir().join("refs/tags/v1"), format!("{tag_oid}\n")).expect("tag ref");
    let opts = RevListOptions {
        objects: true,
        no_object_names: true,
        ..RevListOptions::default()
    };
    let out = rev_list(&repo, &[String::from("refs/tags/v1")], &[], &opts).expect("tag walk");
    assert!(!out.objects.is_empty());
    let range = rev_list(
        &repo,
        &[commits[2].to_hex()],
        &[commits[0].to_hex()],
        &RevListOptions {
            left_right: true,
            ..RevListOptions::default()
        },
    )
    .expect("range");
    assert!(!range.commits.is_empty());
    let tag_set: HashSet<_> = tag_targets(repo.git_dir())
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
    let _visible = commit_visible_for_dense_pathspecs(&repo, tip, &["other".to_owned()])
        .expect("visible check");
    let out = rev_list(
        &repo,
        &[tip.to_hex()],
        &[],
        &RevListOptions {
            min_parents: Some(2),
            max_parents: Some(2),
            until_cutoff: Some(i64::MAX),
            since_cutoff: Some(0),
            show_pulls: true,
            exclude_promisor_objects: true,
            unpacked_only: false,
            ..RevListOptions::default()
        },
    )
    .expect("parents filter");
    assert_eq!(out.commits.len(), 1);
    let sparse = ObjectFilter::SparseOid(format!("{}:patterns", tip.to_hex()));
    let _ = rev_list(
        &repo,
        &[tip.to_hex()],
        &[],
        &RevListOptions {
            objects: true,
            filter: Some(sparse),
            ..RevListOptions::default()
        },
    );
}

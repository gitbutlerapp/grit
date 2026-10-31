//! Broad `rev_list` / `diff` API sweeps on synthetic repos (coverage-oriented).

use grit_lib::config::environment::RepositoryOptions;
use grit_lib::config::ConfigSet;
use grit_lib::diff::{
    diff_commits, diff_trees, diff_trees_with_renames, postprocess_diff_entries,
    DiffPostprocessOptions, DiffStatus,
};
use grit_lib::error::Error;
use grit_lib::objects::odb::Odb;
use grit_lib::objects::{ObjectId, ObjectKind};
use grit_lib::parallel::ThreadPolicy;
use grit_lib::refs::ref_storage::RefStorageFormat;
use grit_lib::repo::{InitOptions, Repository};
use grit_lib::revwalk::rev_list::{
    merge_bases, rev_list, ObjectFilter, OrderingMode, RevListOptions,
};
use std::collections::HashSet;
use std::fs;
use tempfile::tempdir;

fn write_tree(odb: &Odb, name: &str, blob: ObjectId) -> grit_lib::error::Result<ObjectId> {
    let mut body = format!("100644 {name}\0").into_bytes();
    body.extend_from_slice(blob.as_bytes());
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

fn open_repo() -> grit_lib::error::Result<(tempfile::TempDir, Repository)> {
    let dir = tempdir().map_err(Error::Io)?;
    let path = dir.path().to_path_buf();
    Repository::init(
        &path,
        &InitOptions::new()
            .with_bare(true)
            .with_initial_branch("main")
            .with_ref_storage(RefStorageFormat::Files),
    )?;
    Ok((dir, Repository::open(&path, &RepositoryOptions::empty())?))
}

#[test]
fn symmetric_range_ancestry_and_object_roots() -> grit_lib::error::Result<()> {
    let (_dir, repo) = open_repo()?;
    let b1 = repo
        .odb()
        .write_loose_materialize(ObjectKind::Blob, b"a\n")?;
    let b2 = repo
        .odb()
        .write_loose_materialize(ObjectKind::Blob, b"b\n")?;
    let t1 = write_tree(repo.odb(), "f.txt", b1)?;
    let t2 = write_tree(repo.odb(), "f.txt", b2)?;
    let base = write_commit(repo.odb(), t1, &[], "base")?;
    let tip = write_commit(repo.odb(), t2, &[base], "tip")?;
    fs::write(repo.git_dir().join("refs/heads/main"), format!("{tip}\n")).map_err(Error::Io)?;
    fs::write(
        repo.git_dir().join("refs/heads/ancestor"),
        format!("{base}\n"),
    )
    .map_err(Error::Io)?;

    let opts = RevListOptions {
        ordering: OrderingMode::AuthorDateWalk,
        objects: true,
        in_commit_order: true,
        ..RevListOptions::default()
    };
    let out = rev_list(&repo, &[String::from("main")], &[], &opts)?;
    assert_eq!(out.commits.len(), 2);
    Ok(())
}

#[test]
fn diff_postprocess_across_commit_pair() -> grit_lib::error::Result<()> {
    let (_dir, repo) = open_repo()?;
    let unchanged = repo
        .odb()
        .write_loose_materialize(ObjectKind::Blob, b"same\n")?;
    let old_only = repo
        .odb()
        .write_loose_materialize(ObjectKind::Blob, b"removed\n")?;
    let new_only = repo
        .odb()
        .write_loose_materialize(ObjectKind::Blob, b"added line\n")?;
    let old_tree = {
        let mut body = Vec::new();
        for (n, oid) in [("keep.txt", unchanged), ("gone.txt", old_only)] {
            body.extend_from_slice(format!("100644 {n}\0").as_bytes());
            body.extend_from_slice(oid.as_bytes());
        }
        repo.odb()
            .write_loose_materialize(ObjectKind::Tree, &body)?
    };
    let new_tree = {
        let mut body = Vec::new();
        for (n, oid) in [("keep.txt", unchanged), ("fresh.txt", new_only)] {
            body.extend_from_slice(format!("100644 {n}\0").as_bytes());
            body.extend_from_slice(oid.as_bytes());
        }
        repo.odb()
            .write_loose_materialize(ObjectKind::Tree, &body)?
    };
    let base = write_commit(repo.odb(), old_tree, &[], "base")?;
    let tip = write_commit(repo.odb(), new_tree, &[base], "tip")?;

    let raw = diff_trees(repo.odb(), Some(&old_tree), Some(&new_tree), "")?;
    assert!(raw.iter().any(|e| e.status == DiffStatus::Deleted));
    let renamed = diff_trees_with_renames(repo.odb(), Some(&old_tree), Some(&new_tree), "", 30)?;
    assert!(!renamed.is_empty());

    let config = ConfigSet::new();
    let policy = ThreadPolicy::default();
    let opts = DiffPostprocessOptions {
        rename_threshold: 30,
        copy_threshold: Some(30),
        find_copies_harder: true,
        ..DiffPostprocessOptions::default()
    };
    let processed = postprocess_diff_entries(
        repo.odb(),
        raw,
        Some(&old_tree),
        None,
        None,
        &config,
        &policy,
        &opts,
    )?;
    assert!(!processed.is_empty());

    let via_commits = diff_commits(repo.odb(), &base, &tip)?;
    assert!(!via_commits.is_empty());
    let _ = merge_bases(&repo, tip, base, false)?;
    Ok(())
}

#[test]
fn long_merged_history_object_walk_and_decoration() -> grit_lib::error::Result<()> {
    let (_dir, repo) = open_repo()?;
    let ident = "T <t@e.com> 100 +0000";
    let blob = repo
        .odb()
        .write_loose_materialize(ObjectKind::Blob, b"payload\n")?;
    let tree = write_tree(repo.odb(), "tracked.txt", blob)?;
    let mut parent: Option<ObjectId> = None;
    let mut linear = Vec::new();
    for i in 0..180 {
        let mut body = format!("tree {tree}\n");
        if let Some(p) = parent {
            body.push_str(&format!("parent {p}\n"));
        }
        body.push_str(&format!("author {ident}\ncommitter {ident}\n\nc{i}\n"));
        let oid = repo
            .odb()
            .write_loose_materialize(ObjectKind::Commit, body.as_bytes())?;
        parent = Some(oid);
        linear.push(oid);
    }
    let tip = *linear.last().expect("tip");
    let base = linear[linear.len() / 3];
    let side_body =
        format!("tree {tree}\nparent {base}\nauthor {ident}\ncommitter {ident}\n\nside\n");
    let side = repo
        .odb()
        .write_loose_materialize(ObjectKind::Commit, side_body.as_bytes())?;
    let merge_body = format!(
        "tree {tree}\nparent {tip}\nparent {side}\nauthor {ident}\ncommitter {ident}\n\nmerge\n"
    );
    let merge = repo
        .odb()
        .write_loose_materialize(ObjectKind::Commit, merge_body.as_bytes())?;
    fs::write(repo.git_dir().join("refs/heads/main"), format!("{merge}\n")).map_err(Error::Io)?;
    fs::write(repo.git_dir().join("refs/heads/side"), format!("{side}\n")).map_err(Error::Io)?;
    let tag_body = format!("object {tip}\ntype commit\ntag v1\n\nmsg\n");
    let tag = repo
        .odb()
        .write_loose_materialize(ObjectKind::Tag, tag_body.as_bytes())?;
    fs::write(repo.git_dir().join("refs/tags/v1"), format!("{tag}\n")).map_err(Error::Io)?;
    repo.odb().write_commit_graph()?;

    let mut deco = HashSet::new();
    deco.insert(merge);
    deco.insert(tip);
    for ordering in [
        OrderingMode::Default,
        OrderingMode::Topo,
        OrderingMode::DateOrderWalk,
        OrderingMode::AuthorDateWalk,
        OrderingMode::AuthorDateTopo,
    ] {
        let opts = RevListOptions {
            ordering,
            objects: true,
            in_commit_order: true,
            simplify_by_decoration: true,
            simplify_by_decoration_oids: deco.clone(),
            simplify_merges: true,
            parent_rewrite: true,
            full_history: true,
            paths: vec!["tracked.txt".into()],
            filter: Some(ObjectFilter::Combine(vec![
                ObjectFilter::BlobLimit(8192),
                ObjectFilter::TreeDepth(6),
            ])),
            filter_print_omitted: true,
            ancestry_path: true,
            ancestry_path_bottoms: vec![base],
            left_right: true,
            boundary: true,
            max_count: Some(96),
            ..RevListOptions::default()
        };
        let _ = rev_list(
            &repo,
            &[String::from("main"), String::from("side")],
            &[],
            &opts,
        )?;
    }
    let _ = rev_list(
        &repo,
        &[
            String::from("main^{tree}"),
            String::from("refs/tags/v1"),
            String::from("main^{commit}"),
        ],
        &[],
        &RevListOptions {
            objects: true,
            ..RevListOptions::default()
        },
    )?;
    Ok(())
}

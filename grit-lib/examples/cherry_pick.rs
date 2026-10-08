//! Minimal cherry-pick: apply another commit's tree onto `HEAD` using a three-way merge
//! ([`merge_trees::merge_trees_three_way`]), then create a new commit.
//!
//! This mirrors Git’s merge setup for a pick: **base** = the picked commit’s parent tree,
//! **ours** = `HEAD`’s tree, **theirs** = the picked commit’s tree.
//!
//! Run: `cargo run -p grit-lib --example cherry_pick`

use grit_lib::commit_trailers;
use grit_lib::config::ConfigSet;
use grit_lib::merge_file::MergeFavor;
use grit_lib::merge_trees::{merge_trees_three_way, WhitespaceMergeOptions};
use grit_lib::objects::{parse_commit, serialize_commit, CommitData, ObjectKind};
use grit_lib::refs;
use grit_lib::repo::init_repository;
use grit_lib::rev_parse::resolve_revision;
use grit_lib::write_tree::write_tree_from_index;

fn commit_from_tree(
    repo: &grit_lib::repo::Repository,
    tree: grit_lib::objects::ObjectId,
    parents: &[grit_lib::objects::ObjectId],
    message: &str,
) -> grit_lib::error::Result<grit_lib::objects::ObjectId> {
    let commit = CommitData {
        tree,
        parents: parents.to_vec(),
        author: "Example <example@example.com> 1700000000 +0000".to_owned(),
        committer: "Example <example@example.com> 1700000000 +0000".to_owned(),
        author_raw: Vec::new(),
        committer_raw: Vec::new(),
        encoding: None,
        message: message.to_owned(),
        raw_message: None,
        extra_headers: Vec::new(),
    };
    repo.odb
        .write(ObjectKind::Commit, &serialize_commit(&commit))
}

fn tree_of_commit(
    repo: &grit_lib::repo::Repository,
    commit_oid: grit_lib::objects::ObjectId,
) -> grit_lib::error::Result<grit_lib::objects::ObjectId> {
    let obj = repo.odb.read(&commit_oid)?;
    Ok(parse_commit(&obj.data)?.tree)
}

fn main() -> grit_lib::error::Result<()> {
    let root = tempfile::tempdir()?;
    let repo = init_repository(root.path(), false, "main", None, "files")?;

    use grit_lib::index::{Index, IndexEntry, MODE_REGULAR};

    // Base commit on main: one file.
    let blob_a = repo.odb.write(ObjectKind::Blob, b"base\n")?;
    let mut index = Index::new();
    index.add_or_replace(IndexEntry {
        ctime_sec: 0,
        ctime_nsec: 0,
        mtime_sec: 0,
        mtime_nsec: 0,
        dev: 0,
        ino: 0,
        mode: MODE_REGULAR,
        uid: 0,
        gid: 0,
        size: 0,
        oid: blob_a,
        flags: 7,
        flags_extended: None,
        path: b"base.txt".to_vec(),
        base_index_pos: 0,
    });
    repo.write_index(&mut index)?;
    let index = repo.load_index()?;
    let tree_a = write_tree_from_index(&repo.odb, &index, "")?;
    let commit_a = commit_from_tree(&repo, tree_a, &[], "initial\n")?;
    refs::write_ref(&repo.git_dir, "refs/heads/main", &commit_a)?;

    // Topic commit: parent A, adds picked.txt (not on main yet).
    let blob_pick = repo.odb.write(ObjectKind::Blob, b"hello from topic\n")?;
    let mut index = Index::new();
    index.add_or_replace(IndexEntry {
        ctime_sec: 0,
        ctime_nsec: 0,
        mtime_sec: 0,
        mtime_nsec: 0,
        dev: 0,
        ino: 0,
        mode: MODE_REGULAR,
        uid: 0,
        gid: 0,
        size: 0,
        oid: blob_a,
        flags: 7,
        flags_extended: None,
        path: b"base.txt".to_vec(),
        base_index_pos: 0,
    });
    index.add_or_replace(IndexEntry {
        ctime_sec: 0,
        ctime_nsec: 0,
        mtime_sec: 0,
        mtime_nsec: 0,
        dev: 0,
        ino: 0,
        mode: MODE_REGULAR,
        uid: 0,
        gid: 0,
        size: 0,
        oid: blob_pick,
        flags: 9,
        flags_extended: None,
        path: b"picked.txt".to_vec(),
        base_index_pos: 0,
    });
    repo.write_index(&mut index)?;
    let index = repo.load_index()?;
    let tree_b = write_tree_from_index(&repo.odb, &index, "")?;
    let commit_b = commit_from_tree(&repo, tree_b, &[commit_a], "add picked file\n")?;
    refs::write_ref(&repo.git_dir, "refs/heads/topic", &commit_b)?;

    // Cherry-pick `topic` onto `main` (still at A).
    let head = resolve_revision(&repo, "main")?;
    let picked = resolve_revision(&repo, "topic")?;
    let picked_obj = repo.odb.read(&picked)?;
    let picked_data = parse_commit(&picked_obj.data)?;
    let parent = picked_data.parents.first().copied().ok_or_else(|| {
        grit_lib::error::Error::CorruptObject("picked commit has no parent".into())
    })?;

    let base_tree = tree_of_commit(&repo, parent)?;
    let ours_tree = tree_of_commit(&repo, head)?;
    let theirs_tree = picked_data.tree;

    let merged = merge_trees_three_way(
        &repo,
        base_tree,
        ours_tree,
        theirs_tree,
        MergeFavor::default(),
        WhitespaceMergeOptions::default(),
        None,
        grit_lib::merge_trees::TreeMergeConflictPresentation {
            label_ours: "HEAD",
            label_theirs: grit_lib::merge_trees::TheirsConflictLabel::Fixed("picked"),
            label_base: "parent of picked commit",
            style: grit_lib::merge_file::ConflictStyle::Merge,
            checkout_merge: false,
        },
    )?;

    if !merged.conflict_content.is_empty() {
        return Err(grit_lib::error::Error::Message(format!(
            "merge produced {} conflict path(s); this example expects a clean pick",
            merged.conflict_content.len()
        )));
    }

    let new_tree = write_tree_from_index(&repo.odb, &merged.index, "")?;
    let config = ConfigSet::load_repo_local_only(&repo.git_dir)?;
    let msg = commit_trailers::finalize_cherry_pick_message(
        &picked_data.message,
        true,
        false,
        "Example",
        "example@example.com",
        &config,
        &picked.to_hex(),
    );
    let new_commit = commit_from_tree(&repo, new_tree, &[head], &msg)?;
    refs::write_ref(&repo.git_dir, "refs/heads/main", &new_commit)?;

    println!("cherry-picked {} onto {}", picked, head);
    println!("new main: {new_commit}");
    let out = repo.odb.read(&new_commit)?;
    println!("message:\n{}", parse_commit(&out.data)?.message);

    Ok(())
}

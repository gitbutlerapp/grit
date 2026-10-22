//! Round-trip `grit reset` with system `git fsck` / `git status`.

use grit_lib::index::write_tree::{write_tree_update_index, WriteTreeFlags};
use grit_lib::objects::ObjectId;
use grit_lib::porcelain::reset::{reset_to_commit, ResetMode, ResetOptions};
use grit_lib::repo::{InitOptions, Repository};
use grit_lib::{objects, refs, RefStorageFormat};
use grit_test_support::git_cmd;
use tempfile::TempDir;

fn identity() -> String {
    "T <t@t> 0 +0000".into()
}

fn commit_file(repo: &Repository, path: &str, contents: &str, parent: Option<ObjectId>) {
    let wt = repo.work_tree().expect("wt");
    std::fs::write(wt.join(path), contents).expect("write");
    let mut index = repo.index_handle().load().expect("index");
    let oid = repo
        .odb()
        .write(objects::ObjectKind::Blob, contents.as_bytes())
        .expect("blob");
    let entry = grit_lib::index::entry_from_stat(
        &wt.join(path),
        path.as_bytes(),
        oid,
        grit_lib::index::MODE_REGULAR,
    )
    .expect("entry");
    index.add_or_replace(entry);
    index.sort();
    let tree = write_tree_update_index(repo.odb(), &mut index, "", WriteTreeFlags::silent())
        .expect("tree");
    repo.index_handle().write(&mut index).expect("write index");
    let commit = objects::CommitData {
        tree,
        parents: parent.into_iter().collect(),
        author: "T <t@t> 0 +0000".into(),
        committer: "T <t@t> 0 +0000".into(),
        author_raw: vec![],
        committer_raw: vec![],
        encoding: None,
        message: "c\n".into(),
        raw_message: None,
        extra_headers: Vec::new(),
    };
    let co = repo
        .odb()
        .write(
            objects::ObjectKind::Commit,
            &objects::serialize_commit(&commit),
        )
        .expect("commit");
    refs::write_ref(repo.git_dir(), "HEAD", &co).expect("head");
    refs::write_ref(repo.git_dir(), "refs/heads/main", &co).expect("branch");
}

#[test]
fn mixed_reset_readable_by_git() {
    let dir = TempDir::new().expect("tempdir");
    let repo = Repository::init(
        dir.path(),
        &InitOptions::new()
            .with_initial_branch("main".to_owned())
            .with_ref_storage(RefStorageFormat::Files),
    )
    .expect("init");
    let c1 = {
        commit_file(&repo, "f", "1\n", None);
        refs::resolve_ref(repo.git_dir(), "HEAD").expect("head")
    };
    commit_file(&repo, "f", "2\n", Some(c1));

    reset_to_commit(
        &repo,
        &ResetOptions {
            target: c1,
            mode: ResetMode::Mixed,
            identity: identity(),
            message: "reset".into(),
        },
    )
    .expect("reset");

    git_cmd(&["fsck"]).in_dir(dir.path()).suc();
    git_cmd(&["status", "--porcelain"]).in_dir(dir.path()).suc();
}

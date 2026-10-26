//! Hard reset at the current commit must not rewrite clean tracked files.
//!
//! Regression for issue #981: `grit reset --hard` at HEAD should behave like Git's
//! stat-only refresh on a clean tree (no content writes, mtimes unchanged).

use grit_lib::index::write_tree::{write_tree_update_index, WriteTreeFlags};
use grit_lib::objects::{serialize_commit, CommitData, ObjectKind};
use grit_lib::porcelain::reset::{reset_to_commit, ResetMode, ResetOptions};
use grit_lib::refs;
use grit_lib::repo::{InitOptions, Repository};
use grit_test_support::git;
use tempfile::TempDir;

fn init_repo() -> (TempDir, Repository) {
    let dir = TempDir::new().expect("tempdir");
    let repo = Repository::init(dir.path(), &InitOptions::default()).expect("init");
    (dir, repo)
}

fn commit_many_files(repo: &Repository, count: usize) -> grit_lib::objects::ObjectId {
    use grit_lib::index::{entry_from_stat, MODE_REGULAR};
    let work_tree = repo.work_tree().expect("wt");
    for i in 0..count {
        std::fs::write(work_tree.join(format!("f{i}.txt")), format!("body {i}\n")).expect("write");
    }
    let mut index = repo.index_handle().load().expect("index");
    for i in 0..count {
        let rel = format!("f{i}.txt");
        let abs = work_tree.join(&rel);
        let data = std::fs::read(&abs).expect("read");
        let oid = repo.odb().write(ObjectKind::Blob, &data).expect("blob");
        let entry = entry_from_stat(&abs, rel.as_bytes(), oid, MODE_REGULAR).expect("entry");
        index.add_or_replace(entry);
    }
    index.sort();
    let tree = write_tree_update_index(repo.odb(), &mut index, "", WriteTreeFlags::silent())
        .expect("tree");
    repo.index_handle().write(&mut index).expect("write index");
    let commit = CommitData {
        tree,
        parents: vec![],
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
        .write(ObjectKind::Commit, &serialize_commit(&commit))
        .expect("commit");
    refs::write_ref(repo.git_dir(), "refs/heads/main", &co).expect("branch");
    refs::write_symbolic_ref(repo.git_dir(), "HEAD", "refs/heads/main").expect("symhead");
    co
}

fn reset_opts(target: grit_lib::objects::ObjectId, mode: ResetMode) -> ResetOptions {
    ResetOptions::new(target, mode, "T <t@t> 0 +0000", "reset")
}

#[test]
fn hard_reset_at_head_preserves_file_mtimes_on_clean_tree() {
    let (dir, repo) = init_repo();
    let head = commit_many_files(&repo, 120);
    let work_tree = repo.work_tree().expect("wt");

    let before: Vec<(String, filetime::FileTime)> = (0..120)
        .map(|i| {
            let rel = format!("f{i}.txt");
            let path = work_tree.join(&rel);
            let meta = std::fs::metadata(&path).expect("stat");
            let m = filetime::FileTime::from_last_modification_time(&meta);
            (rel, m)
        })
        .collect();

    reset_to_commit(
        &repo,
        &reset_opts(head, ResetMode::Hard),
        &mut grit_lib::progress::NullProgress,
        &grit_lib::progress::NeverCancel,
    )
    .expect("hard reset at HEAD");

    for (rel, old_mtime) in before {
        let path = work_tree.join(&rel);
        let meta = std::fs::metadata(&path).expect("stat");
        let after = filetime::FileTime::from_last_modification_time(&meta);
        assert_eq!(
            after, old_mtime,
            "hard reset at HEAD must not rewrite {rel}"
        );
    }

    let porcelain = git(dir.path(), &["status", "--porcelain"]);
    assert!(
        porcelain.trim().is_empty(),
        "git must see a clean tree after grit hard reset at HEAD, got:\n{porcelain}"
    );
}

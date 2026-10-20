//! `load_commits_with_decorations` loads parents and ref labels in one pass.

mod support;

use grit_lib::objects::ObjectId;
use grit_lib::porcelain::log::{
    collect_decorations, load_commits_with_decorations, DecorationKind,
};
use support::{commit_file, open_grit_repo, HistoryRepo};

#[test]
fn load_commits_attaches_branch_and_parents() -> grit_lib::error::Result<()> {
    let hist = HistoryRepo::init();
    let wt = hist.worktree();
    commit_file(wt, "a.txt", "a\n", "root");
    commit_file(wt, "a.txt", "b\n", "tip");
    let repo = open_grit_repo(wt);
    let head: ObjectId = repo.refs().resolve("HEAD")?;

    let decorations = collect_decorations(&repo, false)?;
    let loaded = load_commits_with_decorations(&repo, std::slice::from_ref(&head), &decorations)?;
    assert_eq!(loaded.len(), 1);
    assert_eq!(loaded[0].oid, head);
    assert_eq!(loaded[0].parents.len(), 1);
    assert!(loaded[0]
        .refs
        .iter()
        .any(|r| r.kind == DecorationKind::Branch));
    Ok(())
}

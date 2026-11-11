//! True merges on a repository whose primary object store is injected ([`MemoryStore`]).

use std::fs;
use std::sync::Arc;

use grit_lib::config::environment::RepositoryOptions;
use grit_lib::merge::IntegrateBranchOutcome;
use grit_lib::objects::odb::store::{MemoryStore, ObjectStore};
use grit_lib::objects::{parse_commit, parse_tree};
use grit_lib::objects::{ObjectId, ObjectKind};
use grit_lib::porcelain::add::StageOptions;
use grit_lib::porcelain::branch::BranchCreateOptions;
use grit_lib::porcelain::commit::{CommitRequest, CommitScope};
use grit_lib::porcelain::switch::{SwitchMode, SwitchOptions};
use grit_lib::progress::CancelToken;
use grit_lib::progress::NullProgress;
use grit_lib::refs::store::{MemoryRefStore, RawRef, RefNameRef, RefStore};
use grit_lib::repo::Repository;
use grit_test_support::integrate_branch;

const IDENT_TS: &str = "T <t@e.x> 1700000000 +0000";

fn memory_primary_repo(
    dir: &tempfile::TempDir,
) -> (Repository, Arc<MemoryStore>, Arc<MemoryRefStore>) {
    let git_dir = dir.path().join("mem.git");
    let wt = dir.path().join("wt");
    fs::create_dir_all(&git_dir).expect("mkdir git dir");
    fs::create_dir_all(&wt).expect("mkdir worktree");
    fs::write(
        git_dir.join("config"),
        "[user]\n\tname = T\n\temail = t@e.x\n",
    )
    .expect("config");
    let store = Arc::new(MemoryStore::new(grit_lib::objects::HashAlgo::Sha1));
    let ref_store = Arc::new(MemoryRefStore::new());
    let repo = Repository::open(
        &git_dir,
        &(RepositoryOptions::empty()
            .with_object_store(store.clone())
            .with_ref_store(ref_store.clone())
            .with_initial_branch("main")
            .with_work_tree(&wt)),
    )
    .expect("open");
    (repo, store, ref_store)
}

fn commit_all(repo: &Repository, message: &str) -> ObjectId {
    repo.index()
        .stage(
            &StageOptions::default(),
            &mut NullProgress,
            &CancelToken::never(),
        )
        .expect("stage");
    let mut req = CommitRequest::new(message);
    req.author = Some(IDENT_TS.parse().expect("signature"));
    req.committer = Some(IDENT_TS.parse().expect("signature"));
    repo.commits()
        .create(
            &grit_lib::commits::CommitOptions::new(req.clone(), CommitScope::Staged),
            &mut NullProgress,
            &CancelToken::never(),
        )
        .expect("commit")
        .oid
}

fn switch_to(repo: &Repository, branch: &str) {
    let oid = repo
        .revwalk()
        .resolve(
            &format!("refs/heads/{branch}"),
            &grit_lib::revwalk::ResolveOptions::default(),
        )
        .expect("branch tip");
    repo.checkout()
        .switch(
            &SwitchOptions::new(SwitchMode::Existing {
                branch: branch.into(),
                oid,
            }),
            &mut NullProgress,
            &CancelToken::never(),
        )
        .expect("switch");
}

fn assert_merge_commit_in_store(
    store: &MemoryStore,
    merge_oid: ObjectId,
    main_tip: ObjectId,
    topic_tip: ObjectId,
) {
    let commit = store
        .read(&merge_oid)
        .expect("contains merge")
        .expect("merge");
    assert_eq!(commit.kind, ObjectKind::Commit);
    let parsed = parse_commit(&commit.data).expect("parse");
    assert_eq!(parsed.parents, vec![main_tip, topic_tip]);
    assert!(store.contains(&parsed.tree).expect("tree in store"));
}

#[test]
fn integrate_branch_disjoint_files_on_custom_primary() {
    let dir = tempfile::tempdir().expect("tempdir");
    let (repo, store, ref_store) = memory_primary_repo(&dir);
    let wt = dir.path().join("wt");

    fs::write(wt.join("a.txt"), "hi\n").expect("write");
    commit_all(&repo, "first");
    repo.branches()
        .create(&BranchCreateOptions::at_head("topic"))
        .expect("branch");

    switch_to(&repo, "topic");
    fs::write(wt.join("b.txt"), "topic\n").expect("write");
    let topic_tip = commit_all(&repo, "on topic");

    switch_to(&repo, "main");
    fs::write(wt.join("c.txt"), "main\n").expect("write");
    let main_tip = commit_all(&repo, "on main");

    let outcome = integrate_branch(
        &repo,
        "refs/heads/main",
        main_tip,
        topic_tip,
        "topic",
        &mut NullProgress,
        &CancelToken::never(),
    )
    .expect("integrate");
    let IntegrateBranchOutcome::Merged { oid: merge_oid, .. } = outcome else {
        panic!("expected merge commit, got {outcome:?}");
    };

    assert_eq!(
        ref_store
            .read_raw(RefNameRef::from_trusted("refs/heads/main"))
            .expect("main ref"),
        Some(RawRef::Direct(merge_oid))
    );
    assert_merge_commit_in_store(&store, merge_oid, main_tip, topic_tip);
    let objects = dir.path().join("mem.git/objects");
    let loose_count = fs::read_dir(&objects).map_or(0, |d| d.count());
    assert_eq!(loose_count, 0, "no loose shards beside custom primary");
}

#[test]
fn integrate_branch_clean_content_merge_on_custom_primary() {
    let dir = tempfile::tempdir().expect("tempdir");
    let (repo, store, ref_store) = memory_primary_repo(&dir);
    let wt = dir.path().join("wt");

    fs::write(wt.join("a.txt"), "line1\nline2\nline3\n").expect("write");
    commit_all(&repo, "first");
    repo.branches()
        .create(&BranchCreateOptions::at_head("topic"))
        .expect("branch");

    switch_to(&repo, "topic");
    fs::write(wt.join("a.txt"), "line1\nline2\nline3 topic\n").expect("write");
    let topic_tip = commit_all(&repo, "topic line3");

    switch_to(&repo, "main");
    fs::write(wt.join("a.txt"), "line1 main\nline2\nline3\n").expect("write");
    let main_tip = commit_all(&repo, "main line1");

    let outcome = integrate_branch(
        &repo,
        "refs/heads/main",
        main_tip,
        topic_tip,
        "topic",
        &mut NullProgress,
        &CancelToken::never(),
    )
    .expect("integrate");
    let IntegrateBranchOutcome::Merged { oid: merge_oid, .. } = outcome else {
        panic!("expected clean content merge, got {outcome:?}");
    };

    assert_eq!(
        ref_store
            .read_raw(RefNameRef::from_trusted("refs/heads/main"))
            .expect("main ref"),
        Some(RawRef::Direct(merge_oid))
    );
    assert_merge_commit_in_store(&store, merge_oid, main_tip, topic_tip);
    let tree_oid = parse_commit(
        &store
            .read(&merge_oid)
            .expect("read merge")
            .expect("merge")
            .data,
    )
    .expect("parse")
    .tree;
    let tree = parse_tree(
        &store
            .read(&tree_oid)
            .expect("read tree")
            .expect("tree")
            .data,
    )
    .expect("parse tree");
    let blob_oid = tree[0].oid;
    let merged_text = String::from_utf8(
        store
            .read(&blob_oid)
            .expect("read blob")
            .expect("blob")
            .data
            .to_vec(),
    )
    .expect("utf8");
    assert_eq!(merged_text, "line1 main\nline2\nline3 topic\n");
}

#[test]
fn integrate_branch_fast_forward_and_conflict_unchanged_on_custom_primary() {
    let dir = tempfile::tempdir().expect("tempdir");
    let (repo, _store, _ref_store) = memory_primary_repo(&dir);
    let wt = dir.path().join("wt");

    fs::write(wt.join("a.txt"), "hi\n").expect("write");
    let base = commit_all(&repo, "first");
    repo.branches()
        .create(&BranchCreateOptions::at_head("topic"))
        .expect("branch");

    switch_to(&repo, "topic");
    fs::write(wt.join("b.txt"), "topic\n").expect("write");
    let topic_tip = commit_all(&repo, "on topic");

    switch_to(&repo, "main");
    let ff = integrate_branch(
        &repo,
        "refs/heads/main",
        base,
        topic_tip,
        "topic",
        &mut NullProgress,
        &CancelToken::never(),
    )
    .expect("ff integrate");
    assert!(matches!(ff, IntegrateBranchOutcome::FastForward { .. }));

    fs::write(wt.join("a.txt"), "main edit\n").expect("write");
    let main_tip = commit_all(&repo, "main edit");
    switch_to(&repo, "topic");
    fs::write(wt.join("a.txt"), "topic edit\n").expect("write");
    let topic_tip2 = commit_all(&repo, "topic edit");
    switch_to(&repo, "main");

    let conflict = integrate_branch(
        &repo,
        "refs/heads/main",
        main_tip,
        topic_tip2,
        "topic",
        &mut NullProgress,
        &CancelToken::never(),
    )
    .expect("conflict integrate");
    assert!(matches!(conflict, IntegrateBranchOutcome::Conflicts { .. }));
}

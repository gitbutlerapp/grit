//! Issue #1184: preserve valid cache-tree across no-op tracked restage and allow-empty commits.

use grit_lib::config::environment::RepositoryOptions;
use grit_lib::index::write_tree::{cache_tree_extension_valid, cache_tree_fully_valid};
use grit_lib::index::Index;
use grit_lib::porcelain::commit::{commit_worktree, CommitRequest, HookPolicy};
use grit_lib::porcelain::stage_tracked::{
    stage_tracked_modifications_with_thresholds, IndexStagingThresholds,
};
use grit_lib::progress::NullProgress;
use grit_lib::repo::Repository;
use grit_test_support::git;

fn open_repo(root: &std::path::Path) -> Repository {
    Repository::open(
        &root.join(".git"),
        &RepositoryOptions::empty().with_work_tree(root),
    )
    .expect("open")
}

const ENTRIES: usize = 4096;

fn ident() -> String {
    "Test <t@example.com> 1000000000 +0000".to_owned()
}

fn large_clean_fixture(root: &std::path::Path) {
    git(root, &["init", "-q", "-b", "main", "."]);
    git(root, &["config", "user.email", "t@example.com"]);
    git(root, &["config", "user.name", "Test"]);
    for i in 0..ENTRIES {
        let rel = format!("d{i:04}/f.txt");
        let path = root.join(&rel);
        std::fs::create_dir_all(path.parent().unwrap()).expect("mkdir");
        std::fs::write(&path, format!("{i}\n")).expect("write");
    }
    git(root, &["add", "-A"]);
    git(root, &["commit", "-qm", "base"]);
    git(root, &["status", "-s"]);
}

fn campaign_thresholds() -> IndexStagingThresholds {
    IndexStagingThresholds {
        large_index_restage: 64,
        parallel_tracked_restage: 64,
    }
}

#[test]
fn noop_tracked_restage_preserves_cache_tree_at_campaign_scale() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();
    large_clean_fixture(root);

    let repo = open_repo(root);
    let index_path = root.join(".git/index");
    let before = Index::parse(&std::fs::read(&index_path).expect("read index")).expect("parse");
    assert!(
        before.cache_tree.is_some(),
        "git commit should leave a TREE extension on the index"
    );

    let summary = stage_tracked_modifications_with_thresholds(&repo, root, campaign_thresholds())
        .expect("tracked scan");
    assert_eq!(
        summary.change_count(),
        0,
        "clean worktree should not restage"
    );

    let index =
        Index::parse(&std::fs::read(&index_path).expect("read index after")).expect("parse");
    assert!(
        index.cache_tree.is_some(),
        "no-op tracked scan must not drop cache-tree upfront (issue #1184)"
    );
    assert!(
        cache_tree_extension_valid(index.cache_tree.as_ref()),
        "cache-tree extension should remain structurally valid"
    );
    assert!(
        cache_tree_fully_valid(repo.odb(), index.cache_tree.as_ref()),
        "cache-tree should stay fully valid after no-op scan"
    );
}

fn commit_worktree_req(message: &str, allow_empty: bool) -> CommitRequest {
    CommitRequest {
        message: message.to_owned(),
        author: ident(),
        committer: ident(),
        allow_empty,
        sign_override: None,
        amend: false,
        assume_worktree_synced: true,
        skip_pre_commit_tree_match: true,
        staged_change_count: None,
        hooks: HookPolicy::default(),
        dry_run: false,
    }
}

#[test]
fn allow_empty_commit_preserves_cache_tree_without_full_restage() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();
    large_clean_fixture(root);

    let repo = open_repo(root);
    let index_path = root.join(".git/index");
    let before = std::fs::read(&index_path).expect("index bytes");
    let before_idx = Index::parse(&before).expect("parse");
    assert!(before_idx.cache_tree.is_some());

    commit_worktree(
        &repo,
        &commit_worktree_req("empty", true),
        &mut NullProgress,
    )
    .expect("allow-empty commit");

    let after = std::fs::read(&index_path).expect("index after");
    let after_idx = Index::parse(&after).expect("parse after");
    assert!(
        after_idx.cache_tree.is_some(),
        "allow-empty on clean tree should keep cache-tree for incremental write-tree"
    );
    assert!(
        cache_tree_fully_valid(repo.odb(), after_idx.cache_tree.as_ref()),
        "post-commit cache-tree should validate against the ODB"
    );
    let head_tree = git(root, &["rev-parse", "HEAD^{tree}"]).trim().to_owned();
    let head_oid = grit_lib::objects::ObjectId::from_hex(&head_tree).expect("parse");
    assert_eq!(
        after_idx.cache_tree_root,
        Some(head_oid),
        "index cache_tree_root must match HEAD tree for allow-empty fast path"
    );
}

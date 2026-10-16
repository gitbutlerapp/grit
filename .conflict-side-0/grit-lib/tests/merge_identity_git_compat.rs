//! Issue #1034: clean merge must fail on missing identity before mutating repo state.

use std::ffi::OsString;
use std::fs;
use std::path::PathBuf;

use grit_lib::config::environment::{Environment, RepositoryOptions};
use grit_lib::index::Index;
use grit_lib::objects::ObjectId;
use grit_lib::porcelain::merge::integrate_branch;
use grit_lib::repo::Repository;
use grit_test_support::git;

fn index_bytes(root: &std::path::Path) -> Vec<u8> {
    fs::read(root.join(".git/index")).expect("read index")
}

#[test]
fn clean_merge_missing_identity_leaves_repo_unchanged() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();
    git(root, &["init", "-q", "-b", "main"]);
    git(root, &["config", "user.name", "T"]);
    git(root, &["config", "user.email", "t@e.com"]);

    fs::write(root.join("shared.txt"), "base\n").expect("shared");
    git(root, &["add", "shared.txt"]);
    git(root, &["commit", "-qm", "base"]);

    git(root, &["switch", "-c", "feature"]);
    fs::write(root.join("feature.txt"), "on feature\n").expect("feature file");
    git(root, &["add", "feature.txt"]);
    git(root, &["commit", "-qm", "feature tip"]);

    git(root, &["switch", "main"]);
    fs::write(root.join("main.txt"), "on main\n").expect("main file");
    git(root, &["add", "main.txt"]);
    git(root, &["commit", "-qm", "main tip"]);

    git(root, &["config", "--unset", "user.name"]);
    git(root, &["config", "--unset", "user.email"]);

    let main_oid = ObjectId::from_hex(git(root, &["rev-parse", "HEAD"]).trim()).expect("main");
    let feature_oid =
        ObjectId::from_hex(git(root, &["rev-parse", "feature"]).trim()).expect("feature");

    let head_before = main_oid;
    let index_before = index_bytes(root);
    let worktree_before = fs::read(root.join("main.txt")).expect("main.txt");

    let mut env = Environment::from_vars(
        [
            ("GIT_CONFIG_GLOBAL", "/dev/null"),
            ("GIT_CONFIG_SYSTEM", "/dev/null"),
        ]
        .into_iter()
        .map(|(k, v)| (OsString::from(k), OsString::from(v))),
        PathBuf::from(root),
    );
    env.git_config_nosystem = Some("true".into());
    let options = RepositoryOptions::with_environment(env);
    let repo = Repository::discover(Some(root), &options).expect("discover");

    let err = integrate_branch(&repo, "refs/heads/main", main_oid, feature_oid, "feature")
        .expect_err("merge without identity");
    assert!(
        err.to_string().contains("no configured email")
            || err.to_string().contains("auto-detection"),
        "expected identity error, got {err}"
    );

    let head_after =
        ObjectId::from_hex(git(root, &["rev-parse", "HEAD"]).trim()).expect("head after");
    assert_eq!(head_after, head_before);
    assert!(!root.join(".git/MERGE_HEAD").exists());
    assert_eq!(index_bytes(root), index_before);
    assert_eq!(
        fs::read(root.join("main.txt")).expect("main.txt after"),
        worktree_before
    );
    assert!(!root.join("feature.txt").exists());

    let index = Index::load(&root.join(".git/index")).expect("index");
    assert!(index.get(b"feature.txt", 0).is_none());
}

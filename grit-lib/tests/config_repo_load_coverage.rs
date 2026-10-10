//! Repository-scoped config load and reload coverage (`RepoCaches` path).

#![allow(clippy::unwrap_used, clippy::expect_used)]

use grit_lib::environment::Environment;
use grit_lib::repo::{init_repository, Repository};
use tempfile::tempdir;

#[test]
fn repository_config_load_and_reload_hits_cache() {
    let dir = tempdir().expect("tempdir");
    init_repository(
        dir.path(),
        false,
        "main",
        None,
        grit_lib::RefStorageFormat::Files,
    )
    .expect("init");
    let git_dir = dir.path().join(".git");
    std::fs::write(
        git_dir.join("config"),
        "[repo]\n\tk = v1\n[include]\n\tpath = extra.conf\n",
    )
    .expect("cfg");
    std::fs::write(git_dir.join("extra.conf"), "[extra]\n\tk = inc\n").expect("extra");

    let mut env = Environment::empty();
    env.cwd = dir.path().to_path_buf();
    let repo = Repository::open(&git_dir, Some(dir.path())).expect("open");
    let cfg = repo.config().expect("config");
    assert_eq!(cfg.get("repo.k").as_deref(), Some("v1"));
    assert_eq!(cfg.get("extra.k").as_deref(), Some("inc"));
    repo.reload_config().expect("reload");
    let cfg2 = repo.config().expect("config again");
    assert_eq!(cfg2.get("extra.k").as_deref(), Some("inc"));
}

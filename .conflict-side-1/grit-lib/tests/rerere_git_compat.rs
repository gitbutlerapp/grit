//! Rerere records written by grit must be readable by `git rerere` (t4200 core).

use std::process::Command;

use grit_lib::diff::rerere::{repo_rerere, rerere_enabled, RerereAutoupdate};
use grit_lib::repo::Repository;
use tempfile::TempDir;

fn git_cmd(root: &std::path::Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .current_dir(root)
        .args(args)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .env("GIT_AUTHOR_NAME", "T")
        .env("GIT_AUTHOR_EMAIL", "t@e.com")
        .env("GIT_COMMITTER_NAME", "T")
        .env("GIT_COMMITTER_EMAIL", "t@e.com")
        .output()
        .expect("git");
    assert!(
        out.status.success(),
        "git {:?}: {}",
        args,
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).to_string()
}

fn init_repo() -> TempDir {
    let dir = TempDir::new().expect("tempdir");
    git_cmd(dir.path(), &["init", "-q", "-b", "main", "."]);
    git_cmd(dir.path(), &["config", "rerere.enabled", "true"]);
    dir
}

#[test]
fn t4200_rerere_records_readable_by_git() {
    let dir = init_repo();
    std::fs::write(dir.path().join("f.txt"), b"base\n").unwrap();
    git_cmd(dir.path(), &["add", "f.txt"]);
    git_cmd(dir.path(), &["commit", "-qm", "base"]);
    git_cmd(dir.path(), &["checkout", "-q", "-b", "side"]);
    std::fs::write(dir.path().join("f.txt"), b"side change\n").unwrap();
    git_cmd(dir.path(), &["commit", "-am", "side"]);
    git_cmd(dir.path(), &["checkout", "-q", "main"]);
    std::fs::write(dir.path().join("f.txt"), b"main change\n").unwrap();
    git_cmd(dir.path(), &["commit", "-am", "main"]);
    let merge_out = Command::new("git")
        .current_dir(dir.path())
        .args(["merge", "side"])
        .output()
        .expect("merge");
    assert!(!merge_out.status.success(), "expect merge conflict");

    let repo = Repository::discover(
        Some(dir.path()),
        &grit_lib::config::environment::RepositoryOptions::empty(),
    )
    .expect("open");
    let cfg = repo.config().expect("cfg");
    assert!(rerere_enabled(cfg.as_ref(), repo.git_dir()));
    let events = repo_rerere(&repo, RerereAutoupdate::No).expect("rerere");
    assert!(!events.is_empty(), "grit should record rerere preimage");

    let status = git_cmd(dir.path(), &["rerere", "status"]);
    assert!(
        status.contains("f.txt"),
        "git rerere status should list conflict path, got {status:?}"
    );
    let _cfg2 = repo.config().expect("cfg2");
}

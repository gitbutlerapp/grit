//! Release-gated timing guard for repack rewrite on an existing single pack.

use std::path::Path;
use std::process::Command;
use std::time::Instant;

use grit_lib::config::environment::RepositoryOptions;
use grit_lib::maintenance::{repack_all, RepackOptions};
use grit_lib::repo::Repository;
use tempfile::TempDir;

fn git(dir: &Path, args: &[&str]) {
    let out = Command::new("git")
        .current_dir(dir)
        .args(args)
        .env("GIT_AUTHOR_NAME", "Test")
        .env("GIT_AUTHOR_EMAIL", "test@example.com")
        .env("GIT_COMMITTER_NAME", "Test")
        .env("GIT_COMMITTER_EMAIL", "test@example.com")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .output()
        .expect("git");
    assert!(
        out.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

/// Many small commits so repack rewrite exercises streaming reuse, not delta search.
fn seed_single_pack_repo(dir: &Path, commits: usize) {
    git(dir, &["init", "-q", "-b", "main"]);
    for i in 0..commits {
        std::fs::write(dir.join(format!("f{i}.txt")), format!("body {i}\n")).expect("write");
        git(dir, &["add", "."]);
        git(dir, &["commit", "-qm", &format!("c{i}")]);
    }
    git(dir, &["repack", "-a", "-d", "-q"]);
}

#[test]
fn repack_rewrite_single_pack_bounded_in_release() {
    if cfg!(debug_assertions) && std::env::var("GRIT_REPACK_REWRITE_PERF").is_err() {
        return;
    }
    let dir = TempDir::new().expect("tempdir");
    seed_single_pack_repo(dir.path(), 120);
    let repo = Repository::discover(Some(dir.path()), &RepositoryOptions::empty()).expect("open");
    let start = Instant::now();
    let stats = repack_all(&repo, &RepackOptions::default()).expect("repack");
    let elapsed = start.elapsed();
    assert!(
        stats.objects_packed.is_some_and(|n| n > 0),
        "explicit repack must rewrite the pack"
    );
    assert!(
        elapsed.as_secs() < 15,
        "repack rewrite on single-pack repo took {:.2}s (budget 15s)",
        elapsed.as_secs_f64()
    );
}

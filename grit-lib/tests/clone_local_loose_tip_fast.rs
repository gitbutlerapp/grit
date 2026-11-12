//! Local clone with a reachable loose tip must stay on the pack-link fast path.

use std::path::Path;
use std::process::Command;
use std::time::{Duration, Instant};

use grit_lib::config::environment::Environment;
use grit_lib::progress::CancelToken;
use grit_lib::remote::clone::{clone as clone_repo, CloneOptions};

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
        "git {:?}: {}",
        args,
        String::from_utf8_lossy(&out.stderr)
    );
}

fn pack_count(git_dir: &Path) -> usize {
    std::fs::read_dir(git_dir.join("objects/pack"))
        .expect("pack dir")
        .filter_map(|e| e.ok())
        .filter(|e| e.path().extension().is_some_and(|x| x == "pack"))
        .count()
}

#[test]
fn clone_local_repacked_plus_loose_tip_stays_fast_and_links_pack() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let work = tmp.path().join("w");
    std::fs::create_dir_all(&work).unwrap();
    git(&work, &["init", "-q", "-b", "main", "."]);
    git(&work, &["config", "user.name", "t"]);
    git(&work, &["config", "user.email", "t@e"]);
    for i in 0..64 {
        std::fs::write(work.join(format!("f{i}.txt")), format!("payload {i}\n")).unwrap();
        git(&work, &["add", "."]);
        git(&work, &["commit", "-qm", &format!("c{i}")]);
    }

    let bare = tmp.path().join("w.git");
    git(tmp.path(), &["clone", "-q", "--bare", "w", "w.git"]);
    git(&bare, &["repack", "-a", "-d", "-q"]);
    assert_eq!(pack_count(&bare), 1, "fixture needs one source pack");

    std::fs::write(work.join("tip.txt"), b"tip\n").unwrap();
    git(&work, &["add", "."]);
    git(&work, &["commit", "-qm", "loose tip"]);
    git(&work, &["push", "-q", "../w.git", "HEAD"]);

    let dest = tmp.path().join("dest");
    let started = Instant::now();
    let outcome = clone_repo(
        CloneOptions::new(bare.to_string_lossy(), dest.clone()),
        &mut NullProgress,
        &CancelToken::never(),
        None,
    )
    .expect("clone");
    let elapsed = started.elapsed();

    assert_eq!(
        pack_count(&dest.join(".git")),
        1,
        "clone must hardlink the existing pack, not ingest a second one"
    );
    assert!(
        elapsed < Duration::from_secs(3),
        "reachable loose tip must not trigger a full history walk (took {elapsed:?})"
    );
    assert!(outcome.transfer.objects > 0);
    git(&dest, &["fsck", "--strict"]);
}

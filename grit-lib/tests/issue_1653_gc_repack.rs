//! Regression for issue #1653: loose-object gc/repack speed and pack size vs system `git`.

use std::path::Path;
use std::process::Command;

use grit_lib::config::environment::RepositoryOptions;
use grit_lib::maintenance::{gc, GcOptions};
use grit_lib::repo::Repository;
use tempfile::TempDir;

const GIT_ENV: &[(&str, &str)] = &[
    ("GIT_CONFIG_GLOBAL", "/dev/null"),
    ("GIT_CONFIG_SYSTEM", "/dev/null"),
    ("GIT_CONFIG_NOSYSTEM", "1"),
    ("GIT_AUTHOR_NAME", "T"),
    ("GIT_AUTHOR_EMAIL", "t@example.com"),
    ("GIT_COMMITTER_NAME", "T"),
    ("GIT_COMMITTER_EMAIL", "t@example.com"),
];

fn git(dir: &Path, args: &[&str]) {
    let mut cmd = Command::new("git");
    cmd.current_dir(dir).args(args);
    for (k, v) in GIT_ENV {
        cmd.env(k, v);
    }
    let out = cmd.output().expect("git");
    assert!(
        out.status.success(),
        "git {:?}: {}",
        args,
        String::from_utf8_lossy(&out.stderr)
    );
}

fn populate_loose_history(dir: &Path, commits: usize) {
    git(dir, &["init", "-q", "-b", "main"]);
    git(dir, &["config", "gc.auto", "0"]);
    for i in 0..commits {
        for name in ["a.txt", "b.txt", "c.txt"] {
            let path = dir.join(name);
            let mut body = String::new();
            if path.is_file() {
                body = std::fs::read_to_string(&path).expect("read");
            }
            let extra = 5 + (i * 17 + name.len()) % 56;
            for j in 0..extra {
                body.push_str(&format!("line {i}-{j} lorem ipsum dolor\n"));
            }
            std::fs::write(&path, body).expect("write blob");
        }
        git(dir, &["add", "a.txt", "b.txt", "c.txt"]);
        git(dir, &["commit", "-q", "-m", &format!("c{i}")]);
    }
}

fn pack_bytes(git_dir: &Path) -> usize {
    let pack_dir = git_dir.join("objects/pack");
    std::fs::read_dir(&pack_dir)
        .expect("pack dir")
        .filter_map(|e| e.ok())
        .filter(|e| e.path().extension().is_some_and(|ext| ext == "pack"))
        .map(|e| std::fs::metadata(e.path()).expect("meta").len() as usize)
        .sum()
}

#[test]
fn issue_1653_loose_gc_pack_near_git_size_and_fsck_clean() {
    let grit_dir = TempDir::new().expect("grit tempdir");
    let git_dir = TempDir::new().expect("git tempdir");
    populate_loose_history(grit_dir.path(), 120);
    populate_loose_history(git_dir.path(), 120);

    let repo = Repository::discover(Some(grit_dir.path()), &RepositoryOptions::empty())
        .expect("open grit repo");
    gc(&repo, &GcOptions::with_reflog_policy(1_700_000_000, false)).expect("grit gc");

    git(git_dir.path(), &["gc", "-q"]);

    git(grit_dir.path(), &["fsck", "--strict"]);
    let grit_pack = pack_bytes(&grit_dir.path().join(".git"));
    let git_pack = pack_bytes(&git_dir.path().join(".git"));
    assert!(
        grit_pack <= git_pack * 12 / 10,
        "issue #1653: grit pack {grit_pack} vs git pack {git_pack} (ratio {:.2})",
        grit_pack as f64 / git_pack as f64
    );
}

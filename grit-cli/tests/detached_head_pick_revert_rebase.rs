//! CLI pick, revert, and rebase on detached HEAD (issue #2530).

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};

type TestResult = Result<(), Box<dyn std::error::Error>>;

const GS: &str = env!("CARGO_BIN_EXE_grit");

struct Scratch {
    path: PathBuf,
}

impl Scratch {
    fn new(tag: &str) -> Result<Self, Box<dyn std::error::Error>> {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let n = NEXT.fetch_add(1, Ordering::SeqCst);
        let mut path = std::env::temp_dir();
        path.push(format!(
            "grit-cli-detached-replay-{tag}-{}-{n}",
            std::process::id()
        ));
        if path.exists() {
            fs::remove_dir_all(&path)?;
        }
        fs::create_dir_all(&path)?;
        Ok(Self { path })
    }

    fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
    }
}

fn grit(dir: &Path, args: &[&str]) -> (bool, String) {
    let out = Command::new(GS)
        .current_dir(dir)
        .args(args)
        .env("GIT_AUTHOR_NAME", "R")
        .env("GIT_AUTHOR_EMAIL", "r@e.com")
        .env("GIT_COMMITTER_NAME", "R")
        .env("GIT_COMMITTER_EMAIL", "r@e.com")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .output()
        .expect("spawn grit");
    let ok = out.status.success();
    let mut combined = String::from_utf8_lossy(&out.stdout).into_owned();
    if !out.stderr.is_empty() {
        combined.push_str(&String::from_utf8_lossy(&out.stderr));
    }
    (ok, combined)
}

fn git(dir: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .current_dir(dir)
        .args(args)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .output()
        .expect("git");
    assert!(
        out.status.success(),
        "git: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).trim().to_owned()
}

fn init_fixture(dir: &Path) -> String {
    grit(dir, &["init"]).0.then_some(()).expect("init");
    for i in 1..=3 {
        fs::write(dir.join("f"), format!("b{i}\n")).expect("write f");
        grit(dir, &["commit", "-q", "-m", &format!("b{i}")]);
    }
    let main = git(dir, &["rev-parse", "main"]);
    grit(dir, &["switch", "-c", "side", "main~2"]);
    fs::write(dir.join("s"), b"s\n").expect("write s");
    grit(dir, &["commit", "-q", "-m", "side"]);
    grit(dir, &["switch", "--detach", "main"]);
    main
}

#[test]
fn pick_revert_rebase_on_detached_head() -> TestResult {
    let scratch = Scratch::new("cli")?;
    let dir = scratch.path();
    let main_before = init_fixture(dir);

    let (ok, err) = grit(dir, &["pick", "side"]);
    assert!(ok, "pick on detached HEAD: {err}");

    let (ok, err) = grit(dir, &["switch", "--detach", "main"]);
    assert!(ok, "switch detach: {err}");

    let (ok, err) = grit(dir, &["revert", "HEAD"]);
    assert!(ok, "revert on detached HEAD: {err}");

    let (ok, err) = grit(dir, &["switch", "--detach", "main"]);
    assert!(ok, "switch detach again: {err}");

    let (ok, err) = grit(dir, &["rebase", "side"]);
    assert!(ok, "rebase on detached HEAD: {err}");

    assert_eq!(git(dir, &["rev-parse", "main"]), main_before);
    git(dir, &["fsck", "--strict"]);
    Ok(())
}

#[test]
fn rebase_without_upstream_on_detached_head() -> TestResult {
    let scratch = Scratch::new("no-upstream")?;
    let dir = scratch.path();
    init_fixture(dir);
    let (ok, msg) = grit(dir, &["rebase"]);
    assert!(!ok, "expected failure without upstream on detached HEAD");
    assert!(
        msg.contains("cannot rebase detached HEAD without an upstream argument"),
        "unexpected message: {msg}"
    );
    Ok(())
}

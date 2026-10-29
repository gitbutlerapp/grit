//! Issue #2330: late cancel during a wide fast-forward checkout should not take much longer
//! than finishing the checkout (rollback uses the same parallel writer as the forward path).
//!
//! `GRIT_CHECKOUT_CANCEL_PERF=1 cargo test -p grit-lib --test checkout_cancel_rollback_wide_perf --release`

use std::process::Command;
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use grit_lib::config::environment::RepositoryOptions;
use grit_lib::error::Error;
use grit_lib::objects::ObjectId;
use grit_lib::porcelain::merge::{integrate_branch, resolve_merge_source};
use grit_lib::progress::{FlagCancel, NullProgress};
use grit_lib::refs::store::RefName;
use grit_lib::repo::Repository;

const DIRS: u32 = 150;
const FILES: u32 = 150;
const MAX_LATE_CANCEL_RATIO: f64 = 2.0;

fn git(root: &std::path::Path, args: &[&str]) {
    let out = Command::new("git")
        .args(args)
        .current_dir(root)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .output()
        .expect("spawn git");
    assert!(
        out.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

fn build_fixture(root: &std::path::Path) -> ObjectId {
    git(root, &["init", "-q", "-b", "main"]);
    git(root, &["config", "core.autocrlf", "false"]);
    git(root, &["config", "user.name", "Test"]);
    git(root, &["config", "user.email", "t@example.com"]);
    for i in 0..(DIRS * FILES) {
        let d = i % DIRS;
        let dir = root.join(format!("d{d:03}"));
        std::fs::create_dir_all(&dir).expect("mkdir");
        std::fs::write(
            dir.join(format!("f{i:06}.txt")),
            format!("file {i} variant a\n"),
        )
        .expect("write");
    }
    git(root, &["add", "-A"]);
    git(root, &["commit", "-qm", "base"]);
    let main_oid = ObjectId::from_hex(&git_rev_parse(root, "HEAD")).expect("main oid");

    git(root, &["switch", "-qc", "other"]);
    for i in 0..(DIRS * FILES) {
        let d = i % DIRS;
        std::fs::write(
            root.join(format!("d{d:03}/f{i:06}.txt")),
            format!("file {i} variant b\n"),
        )
        .expect("write other");
    }
    git(root, &["add", "-A"]);
    git(root, &["commit", "-qm", "other"]);
    git(root, &["switch", "-q", "main"]);
    main_oid
}

fn git_rev_parse(root: &std::path::Path, rev: &str) -> String {
    String::from_utf8(
        Command::new("git")
            .args(["rev-parse", rev])
            .current_dir(root)
            .output()
            .expect("rev-parse")
            .stdout,
    )
    .expect("utf8")
    .trim()
    .to_owned()
}

fn run_integrate(root: &std::path::Path, cancel_ms: u64) -> Result<Duration, Error> {
    let repo = Repository::discover(Some(root), &RepositoryOptions::from_process())?;
    let into = repo.references().resolve(
        &RefName::try_from("refs/heads/main").map_err(|e| Error::PathError(e.to_string()))?,
    )?;
    let other = resolve_merge_source(&repo, "other")?.oid;
    let cancel = FlagCancel::new();
    if cancel_ms > 0 {
        let c = Arc::new(cancel.clone());
        thread::spawn(move || {
            thread::sleep(Duration::from_millis(cancel_ms));
            c.cancel();
        });
    }
    let start = Instant::now();
    let result = integrate_branch(
        &repo,
        "main",
        into,
        other,
        "other",
        &mut NullProgress,
        &cancel,
    );
    let elapsed = start.elapsed();
    match result {
        Ok(_) if cancel_ms == 0 => Ok(elapsed),
        Err(Error::Cancelled) if cancel_ms > 0 => Ok(elapsed),
        Ok(_) => Err(Error::PathError(
            "expected cancelled integrate_branch".into(),
        )),
        Err(e) => Err(e),
    }
}

#[test]
fn late_cancel_rollback_wall_time_bounded() {
    if std::env::var("GRIT_CHECKOUT_CANCEL_PERF").ok().as_deref() != Some("1") {
        eprintln!("skip checkout_cancel_rollback_wide_perf (set GRIT_CHECKOUT_CANCEL_PERF=1)");
        return;
    }
    let root = tempfile::tempdir().expect("tempdir");
    let main_oid = build_fixture(root.path());

    let baseline = run_integrate(root.path(), 0).expect("uncancelled ff");
    eprintln!("uncancelled fast-forward: {baseline:?}");
    git(root.path(), &["reset", "--hard", &main_oid.to_hex()]);
    git(root.path(), &["clean", "-fd"]);

    let baseline_ms = u64::try_from(baseline.as_millis()).unwrap_or(0);
    assert!(
        baseline_ms >= 200,
        "baseline {baseline:?} too fast for a meaningful late-cancel sample"
    );
    let late_cancel_ms = baseline_ms * 4 / 5;
    let cancelled = match run_integrate(root.path(), late_cancel_ms) {
        Ok(elapsed) => elapsed,
        Err(Error::PathError(msg)) if msg.contains("expected cancelled") => {
            eprintln!(
                "skip ratio check: checkout finished before cancel at {late_cancel_ms}ms (baseline {baseline:?})"
            );
            return;
        }
        Err(e) => panic!("late cancel integrate_branch: {e:?}"),
    };
    eprintln!("late cancel@{late_cancel_ms}ms wall: {cancelled:?}");
    assert!(
        cancelled.as_secs_f64() <= baseline.as_secs_f64() * MAX_LATE_CANCEL_RATIO,
        "late cancel@{}ms took {:?}, uncancelled {:?}, ratio limit {MAX_LATE_CANCEL_RATIO}",
        late_cancel_ms,
        cancelled,
        baseline
    );
    assert_eq!(
        git_rev_parse(root.path(), "HEAD"),
        main_oid.to_hex(),
        "HEAD unchanged after cancel"
    );
}

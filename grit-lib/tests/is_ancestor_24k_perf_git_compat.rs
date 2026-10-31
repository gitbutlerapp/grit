//! Release-mode `is_ancestor` on a 24k-commit first-parent chain vs `git merge-base --is-ancestor`.
//!
//! Build the library in release, then run:
//! `GRIT_IS_ANCESTOR_PERF=1 cargo test -p grit-lib --release --features full-tests --test is_ancestor_24k_perf_git_compat -- --test-threads=1 --nocapture`

use std::process::Command;
use std::time::{Duration, Instant};

use grit_lib::config::environment::{Environment, RepositoryOptions};
use grit_lib::objects::ObjectId;
use grit_lib::repo::Repository;
use grit_lib::revwalk::merge_base::is_ancestor;
use grit_test_support::git;

const CHAIN_LEN: usize = 24_000;
const WARM_RUNS: usize = 5;
const MAX_GIT_RATIO: f64 = 3.0;

fn median(mut samples: Vec<Duration>) -> Duration {
    samples.sort();
    let mid = samples.len() / 2;
    if samples.len() % 2 == 0 {
        (samples[mid - 1] + samples[mid]) / 2
    } else {
        samples[mid]
    }
}

fn build_linear_chain(root: &std::path::Path) -> (ObjectId, ObjectId) {
    git(root, &["init", "-q", "-b", "main", "."]);
    git(root, &["config", "user.email", "t@example.com"]);
    git(root, &["config", "user.name", "Test"]);
    for i in 0..CHAIN_LEN {
        git(root, &["commit", "--allow-empty", "-qm", &format!("c{i}")]);
    }
    let tip_hex = git(root, &["rev-parse", "HEAD"]).trim().to_string();
    let root_hex = git(root, &["rev-parse", &format!("HEAD~{}", CHAIN_LEN - 1)])
        .trim()
        .to_string();
    (
        tip_hex.parse().expect("tip oid"),
        root_hex.parse().expect("root oid"),
    )
}

#[test]
fn is_ancestor_24k_chain_within_git_ratio() {
    if std::env::var("GRIT_IS_ANCESTOR_PERF").ok().as_deref() != Some("1") {
        return;
    }
    if Command::new("git").arg("--version").output().is_err() {
        return;
    }

    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();
    let (tip, ancestor) = build_linear_chain(root);

    let mut env = Environment::empty();
    env.cwd = root.to_path_buf();
    env.pwd = Some(root.to_string_lossy().into_owned());
    let options = RepositoryOptions::with_environment(env);
    let repo = Repository::discover(Some(root), &options).expect("open repo for grit is_ancestor");

    for _ in 0..2 {
        assert!(
            is_ancestor(&repo, ancestor, tip).expect("grit is_ancestor"),
            "root must be ancestor of tip"
        );
        let git_ok = Command::new("git")
            .args([
                "merge-base",
                "--is-ancestor",
                &ancestor.to_hex(),
                &tip.to_hex(),
            ])
            .current_dir(root)
            .status()
            .expect("git merge-base --is-ancestor");
        assert!(git_ok.success(), "git must agree");
    }

    let mut grit_samples = Vec::with_capacity(WARM_RUNS);
    let mut git_samples = Vec::with_capacity(WARM_RUNS);
    for _ in 0..WARM_RUNS {
        let t0 = Instant::now();
        assert!(is_ancestor(&repo, ancestor, tip).expect("grit"));
        grit_samples.push(t0.elapsed());

        let t1 = Instant::now();
        let git_ok = Command::new("git")
            .args([
                "merge-base",
                "--is-ancestor",
                &ancestor.to_hex(),
                &tip.to_hex(),
            ])
            .current_dir(root)
            .status()
            .expect("git merge-base --is-ancestor");
        assert!(git_ok.success());
        git_samples.push(t1.elapsed());
    }

    let grit_med = median(grit_samples);
    let git_med = median(git_samples);
    let ratio = grit_med.as_secs_f64() / git_med.as_secs_f64().max(1e-9);
    eprintln!(
        "is_ancestor {CHAIN_LEN}-commit chain: grit {grit_med:?}, git {git_med:?}, ratio {ratio:.2}"
    );
    assert!(
        ratio <= MAX_GIT_RATIO,
        "grit is_ancestor median {grit_med:?} vs git {git_med:?} (ratio {ratio:.2}) exceeds {MAX_GIT_RATIO}×"
    );
}

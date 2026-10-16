//! Integration test: tier S synthetic fixture is git-fsck clean and deterministic.

use std::process::Command;

use grit_utils::synth::{generate_synth_repo, verify_tier_s_with_git, SynthTier};
use tempfile::TempDir;

#[test]
fn synth_tier_s_passes_git_fsck_and_is_deterministic() {
    let spec = SynthTier::S.spec();
    let a = TempDir::new().expect("tempdir");
    let b = TempDir::new().expect("tempdir");
    let ra = generate_synth_repo(a.path(), SynthTier::S).expect("generate a");
    let rb = generate_synth_repo(b.path(), SynthTier::S).expect("generate b");
    assert_eq!(ra.head, rb.head, "HEAD must be deterministic for tier S");
    verify_tier_s_with_git(a.path(), spec).expect("verify a");
    verify_tier_s_with_git(b.path(), spec).expect("verify b");
}

#[test]
fn synth_tier_s_commit_and_ref_counts() {
    let dir = TempDir::new().expect("tempdir");
    generate_synth_repo(dir.path(), SynthTier::S).expect("generate");
    let git = grit_utils::binary::resolve_binary("git", None).expect("git");
    let commits = Command::new(&git)
        .args(["rev-list", "--count", "HEAD"])
        .current_dir(dir.path())
        .output()
        .expect("rev-list");
    assert!(commits.status.success());
    assert_eq!(
        String::from_utf8_lossy(&commits.stdout).trim(),
        "500",
        "expected 500 commits on tier S"
    );
    let refs = Command::new(&git)
        .args(["for-each-ref"])
        .current_dir(dir.path())
        .output()
        .expect("for-each-ref");
    assert!(refs.status.success());
    let n = String::from_utf8_lossy(&refs.stdout)
        .lines()
        .filter(|l| !l.is_empty())
        .count();
    assert_eq!(n, 1000, "expected 1000 refs on tier S");
}

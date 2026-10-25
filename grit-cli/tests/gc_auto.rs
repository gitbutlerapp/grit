//! Automatic maintenance: object-writing commands honor `gc.auto` like `git gc --auto`.

#![allow(clippy::expect_used, clippy::unwrap_used)]

use std::path::Path;
use std::time::{Duration, Instant};

use grit_test_support::{git, grit_cmd, CliFixture, Cmd, Output};
use serde_json::Value;

/// Blobs written by [`seed_loose_blobs`]; their fixed content puts at least two of them in
/// `objects/17`, which is above `gc.auto = 1` (`ceil(1 / 256) = 1` sampled object).
const SEED_FILES: usize = 1500;

/// Run grit with repository config in charge of maintenance: the harness otherwise sets
/// `gc.auto=0` and `maintenance.auto=false` on the command line for every test process.
fn grit_with_repo_maintenance(dir: &Path, args: &[&str]) -> Output {
    let cmd: Cmd = grit_cmd(args)
        .in_dir(dir)
        .env("GIT_CONFIG_COUNT", "1")
        .env("GIT_CONFIG_KEY_0", "grit.testing.changeId")
        .env("GIT_CONFIG_VALUE_0", "content-hash");
    cmd.exec()
}

fn loose_in_fanout(root: &Path, fanout: &str) -> usize {
    std::fs::read_dir(root.join(".git/objects").join(fanout))
        .map(|entries| entries.filter_map(|e| e.ok()).count())
        .unwrap_or(0)
}

fn pack_count(root: &Path) -> usize {
    std::fs::read_dir(root.join(".git/objects/pack"))
        .map(|entries| {
            entries
                .filter_map(|e| e.ok())
                .filter(|e| e.path().extension().is_some_and(|x| x == "pack"))
                .count()
        })
        .unwrap_or(0)
}

fn git_loose_count(root: &Path) -> usize {
    git(root, &["count-objects"])
        .split_whitespace()
        .next()
        .and_then(|n| n.parse().ok())
        .expect("count-objects output")
}

/// A repository with one commit, `SEED_FILES` new files written to the worktree and staged
/// (their blobs are loose), and the given maintenance config.
fn seeded_repo(config: &[(&str, &str)]) -> CliFixture {
    let fixture = CliFixture::new();
    let root = fixture.path();
    fixture.write_repo_file("README.md", "hello\n");
    fixture.run_ok(&["commit", "-m", "initial"]);
    std::fs::create_dir_all(root.join("bulk")).expect("bulk dir");
    for i in 0..SEED_FILES {
        fixture.write_repo_file(&format!("bulk/f{i}.txt"), &format!("payload {i}\n"));
    }
    fixture.run_ok(&["add", "bulk"]);
    for (key, value) in config {
        git(root, &["config", key, value]);
    }
    assert!(
        loose_in_fanout(root, "17") >= 2,
        "fixture must put at least two loose objects in objects/17"
    );
    fixture
}

#[test]
fn commit_runs_maintenance_in_the_foreground_past_gc_auto() {
    let fixture = seeded_repo(&[("gc.auto", "1"), ("gc.autoDetach", "false")]);
    let root = fixture.path();
    assert_eq!(pack_count(root), 0);

    let out = grit_with_repo_maintenance(root, &["commit", "-m", "bulk"]);
    assert!(out.ok(), "{}", out.dump("commit"));
    assert!(
        out.stderr.contains("Packing the repository"),
        "human mode announces maintenance on stderr: {}",
        out.stderr
    );

    assert_eq!(git_loose_count(root), 0, "every loose object is packed");
    assert_eq!(pack_count(root), 1);
    assert!(!root.join(".git/gc.pid").exists());
    git(root, &["fsck", "--strict", "--no-dangling"]);
    assert_eq!(git(root, &["rev-list", "--count", "HEAD"]).trim(), "4");
}

#[test]
fn json_commit_output_stays_clean_when_maintenance_runs() {
    let fixture = seeded_repo(&[("gc.auto", "1"), ("gc.autoDetach", "false")]);
    let root = fixture.path();

    let out = grit_with_repo_maintenance(root, &["commit", "--json", "-m", "bulk"]);
    assert!(out.ok(), "{}", out.dump("commit --json"));
    let doc: Value = serde_json::from_str(&out.stdout).expect("commit --json parses");
    assert!(doc.is_object(), "{doc}");
    assert!(
        !out.stderr.contains("Packing"),
        "machine output modes stay quiet: {}",
        out.stderr
    );
    assert_eq!(pack_count(root), 1, "maintenance still ran");
    git(root, &["fsck", "--strict", "--no-dangling"]);
}

#[test]
fn commit_detaches_maintenance_by_default() {
    let fixture = seeded_repo(&[("gc.auto", "1")]);
    let root = fixture.path();

    let out = grit_with_repo_maintenance(root, &["commit", "-m", "bulk"]);
    assert!(out.ok(), "{}", out.dump("commit"));
    assert!(out.stderr.contains("in the background"), "{}", out.stderr);

    let deadline = Instant::now() + Duration::from_secs(60);
    while (pack_count(root) == 0 || root.join(".git/gc.pid").exists()) && Instant::now() < deadline
    {
        std::thread::sleep(Duration::from_millis(50));
    }
    assert_eq!(pack_count(root), 1, "background maintenance packed objects");
    assert!(!root.join(".git/gc.pid").exists(), "gc.pid released");
    git(root, &["fsck", "--strict", "--no-dangling"]);
}

#[test]
fn dry_run_and_disabled_maintenance_leave_loose_objects() {
    let fixture = seeded_repo(&[("gc.auto", "1"), ("gc.autoDetach", "false")]);
    let root = fixture.path();

    let out = grit_with_repo_maintenance(root, &["commit", "--dry-run", "-m", "bulk"]);
    assert!(out.ok(), "{}", out.dump("commit --dry-run"));
    assert_eq!(pack_count(root), 0, "--dry-run never starts maintenance");

    git(root, &["config", "maintenance.auto", "false"]);
    let out = grit_with_repo_maintenance(root, &["commit", "-m", "bulk"]);
    assert!(out.ok(), "{}", out.dump("commit"));
    assert_eq!(pack_count(root), 0, "maintenance.auto=false skips it");

    git(root, &["config", "maintenance.auto", "true"]);
    git(root, &["config", "gc.auto", "0"]);
    fixture.write_repo_file("more.txt", "more\n");
    let out = grit_with_repo_maintenance(root, &["commit", "-m", "more"]);
    assert!(out.ok(), "{}", out.dump("commit"));
    assert_eq!(pack_count(root), 0, "gc.auto=0 turns it off");
    assert!(git_loose_count(root) > SEED_FILES);
}

#[test]
fn gc_auto_reports_its_decision_in_every_output_mode() {
    let fixture = seeded_repo(&[("gc.auto", "100000")]);
    let root = fixture.path();

    let out = grit_with_repo_maintenance(root, &["gc", "--auto", "--json"]);
    assert!(out.ok(), "{}", out.dump("gc --auto --json"));
    let doc: Value = serde_json::from_str(&out.stdout).expect("gc --auto --json parses");
    assert_eq!(doc["auto"]["status"], "not-needed");
    assert_eq!(doc["tasks"].as_array().map(Vec::len), Some(0));
    assert_eq!(pack_count(root), 0);

    git(root, &["config", "gc.auto", "1"]);
    let out = grit_with_repo_maintenance(root, &["gc", "--auto", "--dry-run", "--json"]);
    assert!(out.ok(), "{}", out.dump("gc --auto --dry-run --json"));
    let doc: Value = serde_json::from_str(&out.stdout).expect("dry-run json parses");
    assert_eq!(doc["dry_run"], true);
    assert_eq!(doc["auto"]["status"], "ran");
    assert_eq!(doc["auto"]["trigger"], "loose-objects");
    assert_eq!(doc["auto"]["limit"], 1);
    assert!(doc["auto"]["estimate"].as_u64().is_some_and(|n| n >= 512));
    assert_eq!(pack_count(root), 0, "dry run writes nothing");

    let out = grit_with_repo_maintenance(root, &["gc", "--auto", "--markdown"]);
    assert!(out.ok(), "{}", out.dump("gc --auto --markdown"));
    assert!(out.stdout.contains("- **auto:** ran"), "{}", out.stdout);
    assert!(out.stdout.contains("- **trigger:** loose-objects"));
    assert_eq!(pack_count(root), 1);

    let out = grit_with_repo_maintenance(root, &["gc", "--auto"]);
    assert!(out.ok(), "{}", out.dump("gc --auto"));
    assert!(out.stdout.contains("auto: nothing to do"), "{}", out.stdout);
    git(root, &["fsck", "--strict", "--no-dangling"]);
}

#[test]
fn gc_auto_rejects_task_subcommands() {
    let fixture = CliFixture::new();
    let out = fixture.run(&["gc", "--auto", "repack"]);
    assert!(!out.ok(), "{}", out.dump("gc --auto repack"));
}

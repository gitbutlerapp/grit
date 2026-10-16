//! `grit gc` integration: git fsck / commit-graph / pack layout after maintenance.

use std::process::Command;

use grit_test_support::CliFixture;
use serde_json::Value;

fn git_ok(repo: &std::path::Path, args: &[&str]) {
    let out = Command::new("git")
        .current_dir(repo)
        .args(args)
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

#[test]
fn gc_full_json_lists_prune_when_nothing_removed() {
    let fixture = CliFixture::empty();
    git_ok(fixture.path(), &["config", "gc.auto", "0"]);
    fixture.write_repo_file("README.md", "hello\n");
    fixture.run_ok(&["add", "README.md"]);
    fixture.run_ok(&["commit", "-m", "initial"]);

    let out = fixture.run_ok(&["gc", "--json"]);
    let doc: Value = serde_json::from_str(&out.stdout).expect("gc --json parses");
    assert_eq!(doc.get("dry_run").and_then(Value::as_bool), Some(false));
    let tasks = doc
        .get("tasks")
        .and_then(Value::as_array)
        .expect("tasks array");
    let prune = tasks
        .iter()
        .find(|t| t.get("task").and_then(Value::as_str) == Some("prune"))
        .expect("full gc must report prune even when nothing was removed");
    assert_eq!(prune.get("pruned").and_then(Value::as_u64), Some(0));
    assert_eq!(prune.get("kept").and_then(Value::as_u64), Some(0));
    assert!(prune.get("duration_ms").and_then(Value::as_u64).is_some());
}

#[test]
fn gc_cli_roundtrip() {
    let fixture = CliFixture::new();
    let root = fixture.path();
    git_ok(root, &["config", "gc.auto", "0"]);

    for i in 0..25 {
        fixture.write_repo_file(&format!("bulk_{i}.txt"), &format!("payload {i}\n"));
        fixture.run_ok(&["add", &format!("bulk_{i}.txt")]);
        fixture.run_ok(&["commit", "-m", &format!("commit {i}")]);
    }

    for i in 0..30 {
        git_ok(root, &["tag", "-f", &format!("t{i}"), "-m", "tag"]);
    }

    git_ok(root, &["checkout", "-b", "side"]);
    git_ok(root, &["commit", "--allow-empty", "-m", "side tip"]);
    git_ok(root, &["checkout", "main"]);

    let loose_before: usize = std::fs::read_dir(root.join(".git/objects"))
        .expect("objects")
        .filter_map(|e| e.ok())
        .filter(|e| e.file_type().map(|t| t.is_dir()).unwrap_or(false))
        .filter(|e| {
            e.file_name()
                .to_string_lossy()
                .chars()
                .all(|c| c.is_ascii_hexdigit())
        })
        .count();
    assert!(
        loose_before > 5,
        "expected loose object shards, got {loose_before}"
    );

    fixture.run_ok(&["gc"]);

    git_ok(root, &["fsck", "--strict"]);
    git_ok(root, &["commit-graph", "verify"]);

    let count_out = Command::new("git")
        .current_dir(root)
        .args(["count-objects", "-v"])
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .output()
        .expect("count-objects");
    assert!(count_out.status.success());
    let text = String::from_utf8_lossy(&count_out.stdout);
    assert!(text.contains("in-pack"), "count-objects -v output: {text}");

    let packs = std::fs::read_dir(root.join(".git/objects/pack"))
        .expect("pack dir")
        .filter_map(|e| e.ok())
        .filter(|e| e.path().extension().is_some_and(|x| x == "pack"))
        .count();
    assert_eq!(packs, 1, "expected a single pack after grit gc");
}

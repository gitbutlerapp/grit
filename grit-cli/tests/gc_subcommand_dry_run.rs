//! `grit gc <task> --dry-run` leaves `.git` unchanged and matches a follow-up real run.

use grit_test_support::CliFixture;
use serde_json::Value;

fn git_fingerprint(fixture: &CliFixture) -> String {
    let git_dir = fixture.path().join(".git");
    let mut paths = Vec::new();
    let mut stack = vec![git_dir.clone()];
    while let Some(dir) = stack.pop() {
        let Ok(read) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in read.flatten() {
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
            } else if let Ok(meta) = entry.metadata() {
                let rel = path.strip_prefix(&git_dir).unwrap_or(&path);
                paths.push(format!("{}:{}", rel.display(), meta.len()));
            }
        }
    }
    paths.sort();
    paths.join("\n")
}

fn assert_unchanged_git(before: &str, after: &str, label: &str) {
    assert_eq!(before, after, "{label}: .git changed during --dry-run");
}

fn task_report<'a>(doc: &'a Value, task: &str) -> &'a Value {
    doc.get("tasks")
        .and_then(Value::as_array)
        .and_then(|tasks| {
            tasks
                .iter()
                .find(|t| t.get("task").and_then(Value::as_str) == Some(task))
        })
        .unwrap_or_else(|| panic!("missing task {task} in {doc}"))
}

#[test]
fn gc_repack_dry_run_then_real() {
    let fixture = CliFixture::new();
    fixture.write_repo_file("extra.txt", "payload\n");
    fixture.run_ok(&["add", "extra.txt"]);
    fixture.run_ok(&["commit", "-m", "extra"]);

    let before = git_fingerprint(&fixture);
    let dry = fixture.run_ok(&["gc", "repack", "--dry-run", "--json"]);
    assert_unchanged_git(&before, &git_fingerprint(&fixture), "gc repack");
    let dry_doc: Value = serde_json::from_str(&dry.stdout).expect("json");
    assert_eq!(dry_doc["dry_run"], true);

    let real = fixture.run_ok(&["gc", "repack", "--json"]);
    let real_doc: Value = serde_json::from_str(&real.stdout).expect("json");
    let dry_repack = task_report(&dry_doc, "repack");
    let real_repack = task_report(&real_doc, "repack");
    assert_eq!(
        dry_repack["objects_packed"], real_repack["objects_packed"],
        "dry-run repack estimate should match real run"
    );
}

#[test]
fn gc_commit_graph_dry_run_then_real() {
    let fixture = CliFixture::new();
    let before = git_fingerprint(&fixture);
    let dry = fixture.run_ok(&["gc", "commit-graph", "--dry-run", "--json"]);
    assert_unchanged_git(&before, &git_fingerprint(&fixture), "gc commit-graph");
    let dry_doc: Value = serde_json::from_str(&dry.stdout).expect("json");
    assert_eq!(dry_doc["dry_run"], true);

    let real = fixture.run_ok(&["gc", "commit-graph", "--json"]);
    let real_doc: Value = serde_json::from_str(&real.stdout).expect("json");
    let dry_task = task_report(&dry_doc, "commit-graph");
    let real_task = task_report(&real_doc, "commit-graph");
    assert_eq!(dry_task["commit_count"], real_task["commit_count"]);
}

#[test]
fn gc_pack_refs_dry_run_unchanged() {
    let fixture = CliFixture::new();
    fixture.run_ok(&["branch", "side"]);
    let before = git_fingerprint(&fixture);
    let out = fixture.run_ok(&["gc", "pack-refs", "--dry-run", "--json"]);
    assert_unchanged_git(&before, &git_fingerprint(&fixture), "gc pack-refs");
    let doc: Value = serde_json::from_str(&out.stdout).expect("json");
    assert_eq!(doc["dry_run"], true);
}

#[test]
fn gc_prune_dry_run_unchanged() {
    let fixture = CliFixture::new();
    let before = git_fingerprint(&fixture);
    let out = fixture.run_ok(&["gc", "prune", "--dry-run", "--json"]);
    assert_unchanged_git(&before, &git_fingerprint(&fixture), "gc prune");
    let doc: Value = serde_json::from_str(&out.stdout).expect("json");
    assert_eq!(doc["dry_run"], true);
}

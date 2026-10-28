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
fn gc_in_shallow_repo_reports_skipped_commit_graph() {
    let fixture = CliFixture::new();
    fixture.run_ok(&["gc", "commit-graph"]);
    let graph = fixture.path().join(".git/objects/info/commit-graph");
    assert!(graph.exists());
    let head = grit_test_support::git(fixture.path(), &["rev-parse", "HEAD"]);
    std::fs::write(fixture.path().join(".git/shallow"), head).expect("write shallow");

    let dry = fixture.run_ok(&["gc", "commit-graph", "--dry-run", "--json"]);
    let dry_doc: Value = serde_json::from_str(&dry.stdout).expect("json");
    assert_eq!(task_report(&dry_doc, "commit-graph")["skipped"], "shallow");
    assert!(graph.exists(), "dry run keeps the graph");

    let real = fixture.run_ok(&["gc", "--json"]);
    let real_doc: Value = serde_json::from_str(&real.stdout).expect("json");
    let task = task_report(&real_doc, "commit-graph");
    assert_eq!(task["skipped"], "shallow");
    assert_eq!(task["commit_count"], 0);
    assert!(!graph.exists(), "gc removes the graph git would reject");

    let human = fixture.run_ok(&["gc", "commit-graph"]);
    assert!(human.stdout.contains("skipped"), "{}", human.stdout);
    let markdown = fixture.run_ok(&["gc", "commit-graph", "--markdown"]);
    assert!(
        markdown.stdout.contains("- **skipped:** shallow"),
        "{}",
        markdown.stdout
    );
}

#[test]
fn gc_pack_refs_dry_run_then_real() {
    let fixture = CliFixture::new();
    fixture.run_ok(&["branch", "side"]);
    let before = git_fingerprint(&fixture);
    let dry = fixture.run_ok(&["gc", "pack-refs", "--dry-run", "--json"]);
    assert_unchanged_git(&before, &git_fingerprint(&fixture), "gc pack-refs");
    let dry_doc: Value = serde_json::from_str(&dry.stdout).expect("json");
    assert_eq!(dry_doc["dry_run"], true);

    let real = fixture.run_ok(&["gc", "pack-refs", "--json"]);
    let real_doc: Value = serde_json::from_str(&real.stdout).expect("json");
    let dry_task = task_report(&dry_doc, "pack-refs");
    let real_task = task_report(&real_doc, "pack-refs");
    assert_eq!(dry_task["refs_packed"], real_task["refs_packed"]);
    assert_eq!(dry_task["refs_pruned"], real_task["refs_pruned"]);
}

#[test]
fn gc_prune_dry_run_then_real() {
    let fixture = CliFixture::new();
    let before = git_fingerprint(&fixture);
    let dry = fixture.run_ok(&["gc", "prune", "--dry-run", "--json"]);
    assert_unchanged_git(&before, &git_fingerprint(&fixture), "gc prune");
    let dry_doc: Value = serde_json::from_str(&dry.stdout).expect("json");
    assert_eq!(dry_doc["dry_run"], true);

    let real = fixture.run_ok(&["gc", "prune", "--json"]);
    let real_doc: Value = serde_json::from_str(&real.stdout).expect("json");
    let dry_task = task_report(&dry_doc, "prune");
    let real_task = task_report(&real_doc, "prune");
    assert_eq!(dry_task["pruned"], real_task["pruned"]);
    assert_eq!(dry_task["kept"], real_task["kept"]);
}

#[test]
fn gc_midx_dry_run_then_real() {
    let fixture = CliFixture::new();
    fixture.run_ok(&["gc", "repack"]);
    let before = git_fingerprint(&fixture);
    let dry = fixture.run_ok(&["gc", "midx", "--dry-run", "--json"]);
    assert_unchanged_git(&before, &git_fingerprint(&fixture), "gc midx");
    let dry_doc: Value = serde_json::from_str(&dry.stdout).expect("json");
    assert_eq!(dry_doc["dry_run"], true);

    let real = fixture.run_ok(&["gc", "midx", "--json"]);
    let real_doc: Value = serde_json::from_str(&real.stdout).expect("json");
    let dry_task = task_report(&dry_doc, "midx");
    let real_task = task_report(&real_doc, "midx");
    assert_eq!(dry_task["pack_count"], real_task["pack_count"]);
}

#[test]
fn gc_offload_dry_run_then_real() {
    let fixture = CliFixture::new();
    // Letters from a xorshift sequence: they do not compress away.
    let mut state = 0x2545_f491_4f6c_dd1d_u64;
    let big: String = (0..20_000)
        .map(|_| {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            char::from(b'a' + (state % 26) as u8)
        })
        .collect();
    fixture.write_repo_file("big.bin", &big);
    fixture.run_ok(&["add", "big.bin"]);
    fixture.run_ok(&["commit", "-m", "big"]);
    fixture.run_ok(&["rm", "big.bin"]);
    fixture.run_ok(&["commit", "-m", "drop big"]);
    let lop = fixture.home_root().join("lop.git");
    fixture.run_ok(&["init", "--bare", lop.to_str().expect("utf-8 path")]);
    fixture.run_ok(&["remote", "add", "lop", lop.to_str().expect("utf-8 path")]);

    let before = git_fingerprint(&fixture);
    let dry = fixture.run_ok(&[
        "gc",
        "offload",
        "--to",
        "lop",
        "--blobs",
        "10k",
        "--dry-run",
        "--json",
    ]);
    assert_unchanged_git(&before, &git_fingerprint(&fixture), "gc offload");
    let dry_doc: Value = serde_json::from_str(&dry.stdout).expect("json");
    assert_eq!(dry_doc["dry_run"], true);
    let dry_task = task_report(&dry_doc, "offload");
    assert_eq!(dry_task["candidates"], 1);
    assert_eq!(dry_task["dropped_from_main"], 0);
    assert!(!lop
        .join("objects/pack")
        .read_dir()
        .is_ok_and(|mut d| d.next().is_some()));

    // `--dry-run` on `gc` itself applies too.
    fixture.run_ok(&[
        "gc",
        "--dry-run",
        "offload",
        "--to",
        "lop",
        "--blobs",
        "10k",
    ]);
    assert_unchanged_git(&before, &git_fingerprint(&fixture), "gc --dry-run offload");

    let real = fixture.run_ok(&["gc", "offload", "--to", "lop", "--blobs", "10k", "--json"]);
    let real_doc: Value = serde_json::from_str(&real.stdout).expect("json");
    let real_task = task_report(&real_doc, "offload");
    assert_eq!(real_task["candidates"], dry_task["candidates"]);
    assert_eq!(real_task["dropped_from_main"], 1);
    assert_eq!(real_task["filter"], "blob:limit=10240");
    fixture.run_ok(&["fsck"]);
}

#[test]
fn gc_offload_reports_typed_errors() {
    let fixture = CliFixture::new();
    let out = fixture.run(&[
        "gc", "offload", "--to", "nowhere", "--blobs", "1k", "--json",
    ]);
    assert!(!out.ok());
    let doc: Value = serde_json::from_str(&out.stdout).expect("json error");
    assert!(
        doc["error"]
            .as_str()
            .is_some_and(|e| e.contains("'nowhere'")),
        "{}",
        out.dump("offload")
    );
    let bad = fixture.run(&["gc", "offload", "--to", "lop", "--blobs", "lots"]);
    assert!(!bad.ok());
    assert!(bad.stderr.contains("--blobs"), "{}", bad.dump("bad blobs"));
}

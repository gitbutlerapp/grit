//! `grit pick` multi-commit, range, and sequencer integration tests.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use grit_lib::objects::ObjectId;
use grit_test_support::{git, grit, unique_tmp, write_file};
use serde_json::Value;
use std::fs;
use std::path::Path;

fn json_stdout(args: &[&str], dir: &Path) -> Value {
    let out = grit(dir, args);
    assert!(out.ok(), "grit {:?} failed: {}", args, out.stderr);
    serde_json::from_str(out.stdout.trim()).expect("json stdout")
}

fn validate_pick_schema(instance: &Value) {
    let schema_path = Path::new(env!("CARGO_MANIFEST_DIR")).join("../content/schemas/pick.v2.json");
    let schema_text = fs::read_to_string(&schema_path).expect("read pick.v2.json");
    let schema: Value = serde_json::from_str(&schema_text).expect("parse schema");
    let compiled = jsonschema::validator_for(&schema).expect("compile pick schema");
    assert!(
        compiled.is_valid(instance),
        "pick JSON schema: {}",
        compiled
            .iter_errors(instance)
            .map(|e| e.to_string())
            .collect::<Vec<_>>()
            .join("\n")
    );
}

fn init_repo(dir: &Path) {
    git(dir, &["init", "-q", "-b", "main"]);
    git(dir, &["config", "user.name", "Test"]);
    git(dir, &["config", "user.email", "t@example.com"]);
    git(dir, &["config", "commit.gpgsign", "false"]);
}

fn commit_file(dir: &Path, path: &str, body: &str, msg: &str) -> String {
    write_file(dir, path, body);
    git(dir, &["add", path]);
    git(dir, &["commit", "-qm", msg]);
    git(dir, &["rev-parse", "HEAD"]).trim().to_owned()
}

#[test]
fn pick_multiple_and_range_json() {
    let dir = unique_tmp("pick-seq", "multi-range");
    init_repo(&dir);
    commit_file(&dir, "base.txt", "base\n", "base");
    git(&dir, &["switch", "-c", "topic"]);
    commit_file(&dir, "a.txt", "a\n", "one");
    commit_file(&dir, "b.txt", "b\n", "two");
    let c1 = git(&dir, &["rev-parse", "topic~1"]).trim().to_owned();
    let c2 = git(&dir, &["rev-parse", "topic"]).trim().to_owned();
    git(&dir, &["switch", "main"]);
    let main_tip = commit_file(&dir, "main.txt", "main\n", "main");

    let range = json_stdout(&["pick", &format!("{main_tip}..topic"), "--json"], &dir);
    validate_pick_schema(&range);
    assert_eq!(range["result"], "picked");
    assert_eq!(range["commits"].as_array().map(|a| a.len()), Some(2));
    let sources: Vec<_> = range["commits"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| c["source"].as_str().unwrap().to_owned())
        .collect();
    assert_eq!(sources, vec![c1.clone(), c2.clone()]);

    git(&dir, &["reset", "--hard", &main_tip]);
    let multi = json_stdout(&["pick", &c1, &c2, "--json"], &dir);
    validate_pick_schema(&multi);
    assert_eq!(multi["result"], "picked");
    assert_eq!(multi["commits"].as_array().map(|a| a.len()), Some(2));
}

#[test]
fn pick_conflict_then_continue() {
    let dir = unique_tmp("pick-seq", "continue");
    init_repo(&dir);
    commit_file(&dir, "f.txt", "base\n", "base");
    git(&dir, &["switch", "-c", "topic"]);
    fs::write(dir.join("f.txt"), "topic\n").unwrap();
    git(&dir, &["commit", "-qam", "topic change"]);
    let pick = git(&dir, &["rev-parse", "HEAD"]).trim().to_owned();
    git(&dir, &["switch", "main"]);
    fs::write(dir.join("f.txt"), "main\n").unwrap();
    git(&dir, &["commit", "-qam", "main change"]);

    let conflict = grit(&dir, &["pick", &pick, "--json"]);
    assert!(!conflict.ok());
    let conflict_json: Value = serde_json::from_str(conflict.stdout.trim()).unwrap();
    validate_pick_schema(&conflict_json);
    assert_eq!(conflict_json["result"], "conflicts");
    assert!(dir.join(".git/CHERRY_PICK_HEAD").is_file());

    fs::write(dir.join("f.txt"), "resolved\n").unwrap();
    git(&dir, &["add", "f.txt"]);
    let done = json_stdout(&["pick", "--continue", "--json"], &dir);
    validate_pick_schema(&done);
    assert_eq!(done["result"], "picked");
    assert_eq!(done["commits"].as_array().map(|a| a.len()), Some(1));
    assert!(done["commits"][0]["change_id"].as_str().is_some());
}

#[test]
fn pick_conflict_commit_clears_operation() {
    let dir = unique_tmp("pick-seq", "commit-finish");
    init_repo(&dir);
    commit_file(&dir, "f.txt", "base\n", "base");
    git(&dir, &["switch", "-c", "topic"]);
    fs::write(dir.join("f.txt"), "topic\n").unwrap();
    git(&dir, &["commit", "-qam", "topic change"]);
    let pick = git(&dir, &["rev-parse", "HEAD"]).trim().to_owned();
    git(&dir, &["switch", "main"]);
    fs::write(dir.join("f.txt"), "main\n").unwrap();
    git(&dir, &["commit", "-qam", "main change"]);

    let conflict = grit(&dir, &["pick", &pick]);
    assert!(!conflict.ok());
    fs::write(dir.join("f.txt"), "resolved\n").unwrap();
    let commit = grit(&dir, &["commit", "-m", "resolved via commit"]);
    assert!(commit.ok(), "commit after pick conflict: {}", commit.stderr);

    let status = grit(&dir, &["status", "--json"]);
    assert!(status.ok());
    let st: Value = serde_json::from_str(status.stdout.trim()).unwrap();
    assert!(
        st.get("operation").is_none() || st["operation"].is_null(),
        "status must not report pick in progress: {}",
        status.stdout
    );
    assert!(!dir.join(".git/CHERRY_PICK_HEAD").exists());
    let cont = grit(&dir, &["pick", "--continue"]);
    assert!(!cont.ok());
    assert!(cont.stderr.contains("no cherry-pick in progress"));
}

#[test]
fn pick_skip() {
    let dir = unique_tmp("pick-seq", "skip");
    init_repo(&dir);
    commit_file(&dir, "shared.txt", "base\n", "base");
    git(&dir, &["switch", "-c", "topic"]);
    commit_file(&dir, "only.txt", "one\n", "t1");
    fs::write(dir.join("shared.txt"), "topic\n").unwrap();
    git(&dir, &["add", "shared.txt"]);
    git(&dir, &["commit", "-qm", "t2 conflict"]);
    let c1 = git(&dir, &["rev-parse", "topic~1"]).trim().to_owned();
    let c2 = git(&dir, &["rev-parse", "topic"]).trim().to_owned();
    git(&dir, &["switch", "main"]);
    fs::write(dir.join("shared.txt"), "main\n").unwrap();
    git(&dir, &["add", "shared.txt"]);
    git(&dir, &["commit", "-qm", "main"]);

    let conflict = grit(&dir, &["pick", &c1, &c2]);
    assert!(!conflict.ok());

    let skipped = json_stdout(&["pick", "--skip", "--json"], &dir);
    validate_pick_schema(&skipped);
    assert_eq!(skipped["result"], "skipped");
    assert!(!dir.join(".git/CHERRY_PICK_HEAD").exists());
}

#[test]
fn pick_abort_restores() {
    let dir = unique_tmp("pick-seq", "abort");
    init_repo(&dir);
    commit_file(&dir, "f.txt", "base\n", "base");
    git(&dir, &["switch", "-c", "topic"]);
    fs::write(dir.join("f.txt"), "topic\n").unwrap();
    git(&dir, &["commit", "-qam", "topic"]);
    let pick = git(&dir, &["rev-parse", "HEAD"]).trim().to_owned();
    git(&dir, &["switch", "main"]);
    fs::write(dir.join("f.txt"), "main\n").unwrap();
    git(&dir, &["commit", "-qam", "main"]);
    let head_before = git(&dir, &["rev-parse", "HEAD"]).trim().to_owned();

    assert!(!grit(&dir, &["pick", &pick]).ok());
    let aborted = json_stdout(&["pick", "--abort", "--json"], &dir);
    validate_pick_schema(&aborted);
    assert_eq!(aborted["result"], "aborted");
    assert_eq!(git(&dir, &["rev-parse", "HEAD"]).trim(), head_before);
    assert!(!dir.join(".git/CHERRY_PICK_HEAD").exists());
}

#[test]
fn pick_x_trailer() {
    let dir = unique_tmp("pick-seq", "x-trailer");
    init_repo(&dir);
    commit_file(&dir, "a.txt", "a\n", "base");
    git(&dir, &["switch", "-c", "topic"]);
    commit_file(&dir, "b.txt", "b\n", "subject line");
    let pick = git(&dir, &["rev-parse", "HEAD"]).trim().to_owned();
    git(&dir, &["switch", "main"]);
    commit_file(&dir, "c.txt", "c\n", "main");

    json_stdout(&["pick", "-x", &pick, "--json"], &dir);
    let msg = git(&dir, &["log", "-1", "--format=%B"]);
    assert!(msg.contains("(cherry picked from commit"));
    let source = ObjectId::from_hex(&pick).expect("oid");
    assert!(msg.contains(&source.to_hex()[..7]));
}

#[test]
fn pick_no_commit_leaves_index() {
    let dir = unique_tmp("pick-seq", "no-commit");
    init_repo(&dir);
    commit_file(&dir, "a.txt", "a\n", "base");
    git(&dir, &["switch", "-c", "topic"]);
    commit_file(&dir, "b.txt", "b\n", "side");
    let pick = git(&dir, &["rev-parse", "HEAD"]).trim().to_owned();
    git(&dir, &["switch", "main"]);
    commit_file(&dir, "c.txt", "c\n", "main");
    let head_before = git(&dir, &["rev-parse", "HEAD"]).trim().to_owned();

    let out = json_stdout(&["pick", "--no-commit", &pick, "--json"], &dir);
    validate_pick_schema(&out);
    assert_eq!(out["result"], "picked");
    assert_eq!(git(&dir, &["rev-parse", "HEAD"]).trim(), head_before);
    assert!(git(&dir, &["diff", "--cached", "--name-only"]).contains("b.txt"));
}

#[test]
fn pick_empty_policies() {
    let dir = unique_tmp("pick-seq", "empty");
    init_repo(&dir);
    commit_file(&dir, "f.txt", "a\n", "base");
    git(&dir, &["switch", "-c", "topic"]);
    git(&dir, &["commit", "--allow-empty", "-qm", "empty"]);
    let empty = git(&dir, &["rev-parse", "HEAD"]).trim().to_owned();
    git(&dir, &["switch", "main"]);

    let stop = grit(&dir, &["pick", &empty, "--json"]);
    assert!(!stop.ok());
    let stop_json: Value = serde_json::from_str(stop.stdout.trim()).unwrap();
    validate_pick_schema(&stop_json);
    assert_eq!(stop_json["result"], "empty");
    json_stdout(&["pick", "--abort", "--json"], &dir);

    let drop = json_stdout(&["pick", "--empty", "drop", &empty, "--json"], &dir);
    validate_pick_schema(&drop);
    assert_eq!(drop["result"], "picked");
    assert_eq!(drop["dropped"].as_array().map(|a| a.len()), Some(1));

    git(&dir, &["reset", "--hard", "main"]);
    let keep = json_stdout(&["pick", "--empty", "keep", &empty, "--json"], &dir);
    validate_pick_schema(&keep);
    assert_eq!(keep["result"], "picked");
    assert_eq!(keep["commits"].as_array().map(|a| a.len()), Some(1));
}

#[test]
fn pick_abort_rejects_positional_commit() {
    let dir = unique_tmp("pick-seq", "abort-positional");
    init_repo(&dir);
    commit_file(&dir, "a.txt", "a\n", "base");
    let out = grit(&dir, &["pick", "HEAD", "--abort", "--json"]);
    assert!(!out.ok());
    assert_eq!(out.status, Some(2));
}

#[test]
fn pick_range_conflict_continue_json_pairs_all_commits() {
    let dir = unique_tmp("pick-seq", "range-continue-json");
    init_repo(&dir);
    commit_file(&dir, "shared.txt", "base\n", "base");
    git(&dir, &["switch", "-c", "topic"]);
    fs::write(dir.join("shared.txt"), "topic\n").unwrap();
    git(&dir, &["add", "shared.txt"]);
    git(&dir, &["commit", "-qm", "t1 conflict"]);
    commit_file(&dir, "clean.txt", "clean\n", "t2");
    let c1 = git(&dir, &["rev-parse", "topic~1"]).trim().to_owned();
    let c2 = git(&dir, &["rev-parse", "topic"]).trim().to_owned();
    git(&dir, &["switch", "main"]);
    fs::write(dir.join("shared.txt"), "main\n").unwrap();
    git(&dir, &["add", "shared.txt"]);
    git(&dir, &["commit", "-qm", "main"]);

    assert!(!grit(&dir, &["pick", &c1, &c2]).ok());
    fs::write(dir.join("shared.txt"), "resolved\n").unwrap();
    git(&dir, &["add", "shared.txt"]);
    let done = json_stdout(&["pick", "--continue", "--json"], &dir);
    validate_pick_schema(&done);
    assert_eq!(done["result"], "picked");
    let rows = done["commits"].as_array().unwrap();
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0]["source"].as_str().unwrap(), c1);
    assert_eq!(rows[1]["source"].as_str().unwrap(), c2);
    assert_ne!(
        rows[0]["oid"].as_str().unwrap(),
        rows[1]["oid"].as_str().unwrap()
    );
}

#[test]
fn pick_continue_json_reports_cherry_pick_source_without_x() {
    let dir = unique_tmp("pick-seq", "continue-source");
    init_repo(&dir);
    commit_file(&dir, "f.txt", "base\n", "base");
    git(&dir, &["switch", "-c", "topic"]);
    fs::write(dir.join("f.txt"), "topic\n").unwrap();
    git(&dir, &["commit", "-qam", "topic change"]);
    let pick = git(&dir, &["rev-parse", "HEAD"]).trim().to_owned();
    git(&dir, &["switch", "main"]);
    fs::write(dir.join("f.txt"), "main\n").unwrap();
    git(&dir, &["commit", "-qam", "main change"]);

    assert!(!grit(&dir, &["pick", &pick]).ok());
    fs::write(dir.join("f.txt"), "resolved\n").unwrap();
    git(&dir, &["add", "f.txt"]);
    let done = json_stdout(&["pick", "--continue", "--json"], &dir);
    validate_pick_schema(&done);
    assert_eq!(done["commits"][0]["source"].as_str().unwrap(), pick);
    let new_oid = done["commits"][0]["oid"].as_str().unwrap();
    assert_ne!(new_oid, pick);
}

#[test]
fn pick_empty_stop_next_steps_suggest_skip() {
    let dir = unique_tmp("pick-seq", "empty-skip-hint");
    init_repo(&dir);
    commit_file(&dir, "f.txt", "a\n", "base");
    git(&dir, &["switch", "-c", "topic"]);
    git(&dir, &["commit", "--allow-empty", "-qm", "empty"]);
    let empty = git(&dir, &["rev-parse", "HEAD"]).trim().to_owned();
    git(&dir, &["switch", "main"]);

    let stop = grit(&dir, &["pick", &empty, "--json"]);
    assert!(!stop.ok());
    let stop_json: Value = serde_json::from_str(stop.stdout.trim()).unwrap();
    validate_pick_schema(&stop_json);
    assert_eq!(stop_json["result"], "empty");
    let steps: Vec<_> = stop_json["next_steps"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_str().unwrap())
        .collect();
    assert!(steps.iter().any(|s| s.contains("pick --skip")));
    assert!(!steps.iter().any(|s| s.contains("pick --continue")));
}

#[test]
fn pick_first_clean_second_conflict_continue_json_lists_both_sources() {
    let dir = unique_tmp("pick-seq", "partial-created");
    init_repo(&dir);
    commit_file(&dir, "shared.txt", "base\n", "base");
    git(&dir, &["switch", "-c", "topic"]);
    commit_file(&dir, "only.txt", "one\n", "t1 clean");
    fs::write(dir.join("shared.txt"), "topic\n").unwrap();
    git(&dir, &["add", "shared.txt"]);
    git(&dir, &["commit", "-qm", "t2 conflict"]);
    let c1 = git(&dir, &["rev-parse", "topic~1"]).trim().to_owned();
    let c2 = git(&dir, &["rev-parse", "topic"]).trim().to_owned();
    git(&dir, &["switch", "main"]);
    fs::write(dir.join("shared.txt"), "main\n").unwrap();
    git(&dir, &["add", "shared.txt"]);
    git(&dir, &["commit", "-qm", "main"]);

    assert!(!grit(&dir, &["pick", &c1, &c2]).ok());
    fs::write(dir.join("shared.txt"), "resolved\n").unwrap();
    git(&dir, &["add", "shared.txt"]);
    let done = json_stdout(&["pick", "--continue", "--json"], &dir);
    let rows = done["commits"].as_array().unwrap();
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0]["source"].as_str().unwrap(), c1);
    assert_eq!(rows[1]["source"].as_str().unwrap(), c2);
}

#[test]
fn pick_status_suggests_pick_not_revert_during_sequence() {
    let dir = unique_tmp("pick-seq", "status-kind");
    init_repo(&dir);
    commit_file(&dir, "f.txt", "base\n", "base");
    git(&dir, &["switch", "-c", "topic"]);
    fs::write(dir.join("f.txt"), "topic\n").unwrap();
    git(&dir, &["commit", "-qam", "topic"]);
    let pick = git(&dir, &["rev-parse", "HEAD"]).trim().to_owned();
    git(&dir, &["switch", "main"]);
    fs::write(dir.join("f.txt"), "main\n").unwrap();
    git(&dir, &["commit", "-qam", "main"]);

    assert!(!grit(&dir, &["pick", &pick]).ok());
    let status = json_stdout(&["status", "--json"], &dir);
    let op = &status["operation"];
    assert_eq!(op["kind"], "cherry-pick");
    let steps: Vec<_> = op["next_steps"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_str().unwrap())
        .collect();
    assert!(steps.iter().any(|s| s.contains("pick --continue")));
    assert!(!steps.iter().any(|s| s.contains("revert --continue")));
}

#[test]
fn pick_assigns_change_id_and_repick_generates_new_id() {
    let dir = unique_tmp("pick-seq", "change-id");
    init_repo(&dir);
    commit_file(&dir, "f.txt", "a\n", "base");
    git(&dir, &["switch", "-c", "topic"]);
    commit_file(&dir, "side.txt", "b\n", "side");
    let pick = git(&dir, &["rev-parse", "HEAD"]).trim().to_owned();
    git(&dir, &["switch", "main"]);
    commit_file(&dir, "main.txt", "c\n", "main");

    let first = json_stdout(&["pick", &pick, "--json"], &dir);
    let id1 = first["commits"][0]["change_id"]
        .as_str()
        .unwrap()
        .to_owned();
    git(&dir, &["switch", "-c", "topic2"]);
    commit_file(&dir, "other.txt", "d\n", "other");
    let pick2 = git(&dir, &["rev-parse", "HEAD"]).trim().to_owned();
    git(&dir, &["switch", "main"]);
    let second = json_stdout(&["pick", &pick2, "--json"], &dir);
    let id2 = second["commits"][0]["change_id"].as_str().unwrap();
    assert_ne!(id1, id2);
}

#[test]
fn pick_continue_after_empty_stop_skip_completes() {
    let dir = unique_tmp("pick-seq", "empty-skip-flow");
    init_repo(&dir);
    commit_file(&dir, "f.txt", "a\n", "base");
    git(&dir, &["switch", "-c", "topic"]);
    git(&dir, &["commit", "--allow-empty", "-qm", "empty"]);
    let empty = git(&dir, &["rev-parse", "HEAD"]).trim().to_owned();
    git(&dir, &["switch", "main"]);

    assert!(!grit(&dir, &["pick", &empty]).ok());
    let skipped = json_stdout(&["pick", "--skip", "--json"], &dir);
    assert_eq!(skipped["result"], "skipped");
    assert!(!dir.join(".git/sequencer").exists());
}

#[test]
fn pick_message_requires_single_commit() {
    let dir = unique_tmp("pick-seq", "message");
    init_repo(&dir);
    commit_file(&dir, "a.txt", "a\n", "base");
    git(&dir, &["switch", "-c", "topic"]);
    commit_file(&dir, "b.txt", "b\n", "one");
    commit_file(&dir, "c.txt", "c\n", "two");
    let c1 = git(&dir, &["rev-parse", "topic~1"]).trim().to_owned();
    let c2 = git(&dir, &["rev-parse", "topic"]).trim().to_owned();
    git(&dir, &["switch", "main"]);
    commit_file(&dir, "d.txt", "d\n", "main");

    let err = grit(&dir, &["pick", "-m", "custom", &c1, &c2, "--json"]);
    assert!(!err.ok());
    assert!(err.stderr.contains("exactly one commit") || err.stdout.contains("exactly one commit"));

    let ok = json_stdout(&["pick", "-m", "custom", &c2, "--json"], &dir);
    validate_pick_schema(&ok);
    assert_eq!(git(&dir, &["log", "-1", "--format=%s"]).trim(), "custom");
}

#[test]
fn pick_json_subjects_skip_leading_blank_message_lines() {
    let dir = unique_tmp("pick-seq", "blank-led-subject");
    init_repo(&dir);
    commit_file(&dir, "f.txt", "base\n", "base");
    git(&dir, &["switch", "-q", "-c", "topic"]);
    let commit_verbatim = |path: &str, body: &str| {
        write_file(&dir, path, body);
        git(&dir, &["add", path]);
        write_file(
            &dir,
            ".git/blank-led-msg",
            "\n\n  blank-led subject\n\nbody\n",
        );
        git(
            &dir,
            &[
                "commit",
                "-q",
                "--cleanup=verbatim",
                "-F",
                ".git/blank-led-msg",
            ],
        );
        git(&dir, &["rev-parse", "HEAD"]).trim().to_owned()
    };
    let clean = commit_verbatim("g.txt", "g\n");
    let conflicting = commit_verbatim("f.txt", "topic\n");
    assert_eq!(
        git(&dir, &["log", "-1", "--format=%s", &conflicting]).trim(),
        "blank-led subject"
    );
    git(&dir, &["switch", "-q", "main"]);
    commit_file(&dir, "f.txt", "main\n", "main");

    let check = grit(&dir, &["--json", "pick", "--check", &conflicting]);
    let v: Value = serde_json::from_str(check.stdout.trim()).expect("json stdout");
    assert_eq!(v["result"], "conflicts", "{}", check.stderr);
    assert_eq!(v["commits"][0]["subject"], "blank-led subject");

    let v = json_stdout(&["--json", "pick", &clean], &dir);
    assert_eq!(v["result"], "picked");
    assert_eq!(v["commits"][0]["subject"], "blank-led subject");
    validate_pick_schema(&v);
    git(&dir, &["fsck", "--strict"]);
}

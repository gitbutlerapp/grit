//! CLI commits carry stable change ids checked with system git.

use std::fs;

use grit_test_support::{git, grit, unique_tmp};
use serde_json::Value;

fn change_id_from_cat(cat: &str) -> Option<String> {
    cat.lines()
        .find(|l| l.starts_with("change-id "))
        .map(|l| l.strip_prefix("change-id ").unwrap().to_owned())
}

fn json_stdout(args: &[&str], dir: &std::path::Path) -> Value {
    let out = grit(dir, args);
    assert!(out.ok(), "grit {:?} failed: {}", args, out.stderr);
    serde_json::from_str(&out.stdout).expect("json stdout")
}

fn assert_json_change_id_matches_head(dir: &std::path::Path, json: &Value) {
    let cat = git(dir, &["cat-file", "-p", "HEAD"]);
    let from_git = change_id_from_cat(&cat).expect("change-id in commit");
    assert_eq!(
        json.get("change_id").and_then(|v| v.as_str()),
        Some(from_git.as_str())
    );
}

#[test]
fn cli_commit_and_amend_keep_change_id() {
    let dir = unique_tmp("commit-change-id", "cli");
    git(&dir, &["init", "-q", "-b", "main"]);
    git(&dir, &["config", "user.name", "T"]);
    git(&dir, &["config", "user.email", "t@e.com"]);
    std::fs::write(dir.join("a.txt"), b"1\n").unwrap();

    assert!(grit(&dir, &["commit", "-m", "first"]).ok());

    std::fs::write(dir.join("a.txt"), b"2\n").unwrap();
    assert!(grit(&dir, &["commit", "-m", "second"]).ok());
    let tip_cat = git(&dir, &["cat-file", "-p", "HEAD"]);
    let tip_id = change_id_from_cat(&tip_cat).expect("tip change-id");

    assert!(grit(&dir, &["commit", "--amend", "-m", "second amended"]).ok());

    git(&dir, &["fsck", "--strict"]);
    let amended_cat = git(&dir, &["cat-file", "-p", "HEAD"]);
    let amended_id = change_id_from_cat(&amended_cat).expect("amended change-id");
    assert_eq!(amended_id, tip_id);
}

#[test]
fn pull_merge_commit_gets_change_id() {
    let scratch = unique_tmp("pull-merge-change-id", "cli");
    let seed = scratch.join("seed");
    let remote = scratch.join("remote.git");
    let clone = scratch.join("clone");
    fs::create_dir_all(&seed).unwrap();

    git(&seed, &["init", "-q", "-b", "main"]);
    git(&seed, &["config", "user.name", "T"]);
    git(&seed, &["config", "user.email", "t@e.com"]);
    fs::write(seed.join("README.md"), b"seed\n").unwrap();
    assert!(grit(&seed, &["commit", "-m", "seed"]).ok());

    git(&scratch, &["init", "--bare", "remote.git"]);
    git(
        &seed,
        &["remote", "add", "origin", remote.to_str().unwrap()],
    );
    assert!(grit(&seed, &["push"]).ok());

    git(&scratch, &["clone", remote.to_str().unwrap(), "clone"]);
    fs::write(clone.join("clone.txt"), b"clone\n").unwrap();
    assert!(grit(&clone, &["commit", "-m", "clone work"]).ok());
    assert!(grit(&clone, &["push"]).ok());

    fs::write(seed.join("seed.txt"), b"diverge\n").unwrap();
    assert!(grit(&seed, &["commit", "-m", "seed diverge"]).ok());
    assert!(grit(&seed, &["fetch"]).ok());
    assert!(grit(&seed, &["pull"]).ok());

    git(&seed, &["fsck", "--strict"]);
    let cat = git(&seed, &["cat-file", "-p", "HEAD"]);
    assert!(
        change_id_from_cat(&cat).is_some(),
        "merge pull commit should carry change-id:\n{cat}"
    );
}

#[test]
fn commit_json_reports_change_id() {
    let dir = unique_tmp("commit-change-id-json", "commit");
    git(&dir, &["init", "-q", "-b", "main"]);
    git(&dir, &["config", "user.name", "T"]);
    git(&dir, &["config", "user.email", "t@e.com"]);
    fs::write(dir.join("a.txt"), b"1\n").unwrap();

    let json = json_stdout(&["commit", "-m", "first", "--json"], &dir);
    assert_json_change_id_matches_head(&dir, &json);
    git(&dir, &["fsck", "--strict"]);
}

#[test]
fn amend_json_reports_same_change_id() {
    let dir = unique_tmp("commit-change-id-json", "amend");
    git(&dir, &["init", "-q", "-b", "main"]);
    git(&dir, &["config", "user.name", "T"]);
    git(&dir, &["config", "user.email", "t@e.com"]);
    fs::write(dir.join("a.txt"), b"1\n").unwrap();
    json_stdout(&["commit", "-m", "first", "--json"], &dir);
    fs::write(dir.join("a.txt"), b"2\n").unwrap();
    let before = json_stdout(&["commit", "-m", "second", "--json"], &dir);
    let id_before = before["change_id"].as_str().expect("change_id");

    let amended = json_stdout(&["commit", "--amend", "-m", "second v2", "--json"], &dir);
    assert_eq!(amended["change_id"].as_str(), Some(id_before));
    assert_json_change_id_matches_head(&dir, &amended);
    git(&dir, &["fsck", "--strict"]);
}

#[test]
fn pick_json_reports_new_change_id() {
    let dir = unique_tmp("commit-change-id-json", "pick");
    git(&dir, &["init", "-q", "-b", "main"]);
    git(&dir, &["config", "user.name", "T"]);
    git(&dir, &["config", "user.email", "t@e.com"]);
    fs::write(dir.join("a.txt"), b"1\n").unwrap();
    json_stdout(&["commit", "-m", "base", "--json"], &dir);
    assert!(grit(&dir, &["switch", "-c", "side"]).ok());
    fs::write(dir.join("side.txt"), b"side\n").unwrap();
    json_stdout(&["commit", "-m", "side work", "--json"], &dir);
    let source = git(&dir, &["rev-parse", "refs/heads/side"])
        .trim()
        .to_owned();
    let source_id =
        change_id_from_cat(&git(&dir, &["cat-file", "-p", &source])).expect("source change-id");

    assert!(grit(&dir, &["switch", "main"]).ok());
    fs::write(dir.join("main.txt"), b"main-only\n").unwrap();
    json_stdout(&["commit", "-m", "main diverge", "--json"], &dir);
    let json = json_stdout(&["pick", &source, "--json"], &dir);
    assert_ne!(json["oid"].as_str(), Some(source.as_str()));
    let pick_id = json["change_id"].as_str().expect("pick change_id");
    let head_id =
        change_id_from_cat(&git(&dir, &["cat-file", "-p", "HEAD"])).expect("head change-id");
    assert_eq!(pick_id, head_id.as_str());
    assert_ne!(pick_id, source_id.as_str());
    git(&dir, &["fsck", "--strict"]);
}

#[test]
fn revert_json_reports_new_change_id() {
    let dir = unique_tmp("commit-change-id-json", "revert");
    git(&dir, &["init", "-q", "-b", "main"]);
    git(&dir, &["config", "user.name", "T"]);
    git(&dir, &["config", "user.email", "t@e.com"]);
    fs::write(dir.join("a.txt"), b"1\n").unwrap();
    json_stdout(&["commit", "-m", "first", "--json"], &dir);
    fs::write(dir.join("b.txt"), b"2\n").unwrap();
    json_stdout(&["commit", "-m", "second", "--json"], &dir);
    let target = git(&dir, &["rev-parse", "HEAD~1"]).trim().to_owned();
    let target_id = change_id_from_cat(&git(&dir, &["cat-file", "-p", &target])).expect("id");

    let json = json_stdout(&["revert", &target, "--json"], &dir);
    let revert_id = json["change_id"].as_str().expect("revert change_id");
    assert_ne!(revert_id, target_id.as_str());
    assert_eq!(
        change_id_from_cat(&git(&dir, &["cat-file", "-p", "HEAD"])).as_deref(),
        Some(revert_id)
    );
    git(&dir, &["fsck", "--strict"]);
}

#[test]
fn merge_json_reports_change_id() {
    let dir = unique_tmp("commit-change-id-json", "merge");
    git(&dir, &["init", "-q", "-b", "main"]);
    git(&dir, &["config", "user.name", "T"]);
    git(&dir, &["config", "user.email", "t@e.com"]);
    fs::write(dir.join("base.txt"), b"base\n").unwrap();
    json_stdout(&["commit", "-m", "base", "--json"], &dir);

    git(&dir, &["checkout", "-q", "-b", "side"]);
    fs::write(dir.join("side.txt"), b"side\n").unwrap();
    json_stdout(&["commit", "-m", "side", "--json"], &dir);

    git(&dir, &["checkout", "-q", "main"]);
    fs::write(dir.join("main.txt"), b"main\n").unwrap();
    json_stdout(&["commit", "-m", "main", "--json"], &dir);

    let json = json_stdout(&["merge", "side", "--json"], &dir);
    assert_eq!(json["result"], "merged");
    let oid = json["oid"].as_str().expect("merge oid");
    let cat = git(&dir, &["cat-file", "-p", oid]);
    assert_eq!(
        json.get("change_id").and_then(|v| v.as_str()),
        change_id_from_cat(&cat).as_deref()
    );
    git(&dir, &["fsck", "--strict"]);
}

#[test]
fn history_reword_json_preserves_derived_change_id_for_headerless_git_commit() {
    let dir = unique_tmp("commit-change-id-json", "reword-derived");
    git(&dir, &["init", "-q", "-b", "main"]);
    git(&dir, &["config", "user.name", "T"]);
    git(&dir, &["config", "user.email", "t@e.com"]);
    fs::write(dir.join("a.txt"), b"a\n").unwrap();
    git(&dir, &["add", "a.txt"]);
    git(&dir, &["commit", "-m", "first"]);
    fs::write(dir.join("b.txt"), b"b\n").unwrap();
    git(&dir, &["add", "b.txt"]);
    git(&dir, &["commit", "-m", "second"]);
    let target = git(&dir, &["rev-parse", "HEAD~1"]).trim().to_owned();
    let derived = grit_lib::change_id::ChangeId::derived_from_commit_id(
        &grit_lib::objects::ObjectId::from_hex(&target).unwrap(),
    );
    assert!(change_id_from_cat(&git(&dir, &["cat-file", "-p", &target])).is_none());

    let json = json_stdout(
        &[
            "history",
            "reword",
            &target,
            "-m",
            "first reworded",
            "--json",
        ],
        &dir,
    );
    let row = json["rewritten"]
        .as_array()
        .and_then(|rows| {
            rows.iter()
                .find(|r| r["old"].as_str() == Some(target.as_str()))
        })
        .expect("rewritten row");
    assert_eq!(row["change_id"].as_str(), Some(derived.as_str()));
    if let Some(new) = row["new"].as_str() {
        assert_eq!(
            change_id_from_cat(&git(&dir, &["cat-file", "-p", new])).as_deref(),
            Some(derived.as_str())
        );
    }
    git(&dir, &["fsck", "--strict"]);
}

#[test]
fn history_reword_json_keeps_change_id() {
    let dir = unique_tmp("commit-change-id-json", "reword");
    git(&dir, &["init", "-q", "-b", "main"]);
    git(&dir, &["config", "user.name", "T"]);
    git(&dir, &["config", "user.email", "t@e.com"]);
    fs::write(dir.join("a.txt"), b"a\n").unwrap();
    json_stdout(&["commit", "-m", "first", "--json"], &dir);
    fs::write(dir.join("b.txt"), b"b\n").unwrap();
    json_stdout(&["commit", "-m", "second", "--json"], &dir);
    let target = git(&dir, &["rev-parse", "HEAD~1"]).trim().to_owned();
    let before_id = change_id_from_cat(&git(&dir, &["cat-file", "-p", &target])).expect("id");

    let json = json_stdout(
        &[
            "history",
            "reword",
            &target,
            "-m",
            "first reworded",
            "--json",
        ],
        &dir,
    );
    let row = json["rewritten"]
        .as_array()
        .and_then(|rows| {
            rows.iter()
                .find(|r| r["old"].as_str() == Some(target.as_str()))
        })
        .expect("rewritten row");
    assert_eq!(row["change_id"].as_str(), Some(before_id.as_str()));
    if let Some(new) = row["new"].as_str() {
        assert_eq!(
            change_id_from_cat(&git(&dir, &["cat-file", "-p", new])).as_deref(),
            Some(before_id.as_str())
        );
    }
    git(&dir, &["fsck", "--strict"]);
}

#[test]
fn history_squash_json_keeps_target_change_id() {
    let dir = unique_tmp("commit-change-id-json", "squash");
    git(&dir, &["init", "-q", "-b", "main"]);
    git(&dir, &["config", "user.name", "T"]);
    git(&dir, &["config", "user.email", "t@e.com"]);
    fs::write(dir.join("a.txt"), b"a\n").unwrap();
    json_stdout(&["commit", "-m", "first", "--json"], &dir);
    fs::write(dir.join("b.txt"), b"b\n").unwrap();
    json_stdout(&["commit", "-m", "second", "--json"], &dir);
    fs::write(dir.join("c.txt"), b"c\n").unwrap();
    json_stdout(&["commit", "-m", "third", "--json"], &dir);
    let tip = git(&dir, &["rev-parse", "HEAD"]).trim().to_owned();
    let range_base = git(&dir, &["rev-parse", "HEAD~2"]).trim().to_owned();
    // `base..tip` lists commits after `base`; the squashed template is the oldest in that list.
    let squash_template = git(&dir, &["rev-parse", "HEAD~1"]).trim().to_owned();
    let target_id =
        change_id_from_cat(&git(&dir, &["cat-file", "-p", &squash_template])).expect("id");
    let range = format!("{range_base}..{tip}");

    let json = json_stdout(
        &[
            "history",
            "squash",
            &range,
            "-m",
            "squashed",
            "--update-refs",
            "head",
            "--json",
        ],
        &dir,
    );
    let rewritten = json["rewritten"].as_array().expect("rewritten array");
    assert!(
        rewritten.iter().any(|r| r["new"].is_string()),
        "squash must produce a replacement commit: {json}"
    );
    for row in rewritten {
        if row["new"].is_string() {
            assert_eq!(row["change_id"].as_str(), Some(target_id.as_str()));
        } else {
            assert!(row["change_id"].is_null());
        }
    }
    git(&dir, &["fsck", "--strict"]);
}

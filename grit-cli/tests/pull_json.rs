//! `grit pull --json` result values and exit codes.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::error::Error;
use std::fs;
use std::path::Path;
use std::process::Command;

use grit_test_support::{git, unique_tmp, write_file};
use serde_json::Value;

type TestResult = Result<(), Box<dyn Error>>;

const GRIT: &str = env!("CARGO_BIN_EXE_grit");

struct CmdOutput {
    ok: bool,
    stdout: String,
    stderr: String,
}

fn grit_cmd(dir: &Path, args: &[&str]) -> CmdOutput {
    let out = Command::new(GRIT)
        .args(args)
        .current_dir(dir)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .output()
        .expect("spawn grit");
    CmdOutput {
        ok: out.status.success(),
        stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
    }
}

fn gs_json(dir: &Path, args: &[&str]) -> Value {
    let mut full = vec!["--json"];
    full.extend_from_slice(args);
    let out = grit_cmd(dir, &full);
    assert!(out.ok, "grit failed: {}", out.stderr);
    serde_json::from_str(&out.stdout).expect("json")
}

fn commit(dir: &Path, file: &str, msg: &str) {
    write_file(dir, file, msg);
    git(dir, &["add", file]);
    git(
        dir,
        &[
            "-c",
            "user.name=T",
            "-c",
            "user.email=t@example.com",
            "commit",
            "-q",
            "-m",
            msg,
        ],
    );
}

#[test]
fn pull_json_fast_forward_exit_zero() -> TestResult {
    let root = unique_tmp("pull-json", "ff");
    let upstream = root.join("upstream");
    fs::create_dir_all(&upstream)?;
    git(&upstream, &["init", "-q", "-b", "main"]);
    commit(&upstream, "a", "seed");
    git(
        &root,
        &["clone", "-q", upstream.to_str().unwrap(), "client"],
    );
    let client = root.join("client");
    commit(&upstream, "a", "remote");
    git(&client, &["fetch", "-q"]);
    let json = gs_json(&client, &["pull"]);
    assert_eq!(json["result"], "fast_forward");
    Ok(())
}

#[test]
fn pull_json_refused_not_ff_exit_one() -> TestResult {
    let root = unique_tmp("pull-json", "refused");
    let upstream = root.join("upstream");
    fs::create_dir_all(&upstream)?;
    git(&upstream, &["init", "-q", "-b", "main"]);
    commit(&upstream, "a", "a1");
    git(
        &root,
        &["clone", "-q", upstream.to_str().unwrap(), "client"],
    );
    let client = root.join("client");
    commit(&client, "b", "local");
    commit(&upstream, "a", "remote");
    git(&client, &["fetch", "-q"]);
    let out = grit_cmd(&client, &["--json", "pull", "--ff-only"]);
    assert!(!out.ok, "ff-only refused must exit 1");
    let json: Value = serde_json::from_str(&out.stdout)?;
    assert_eq!(json["result"], "refused_not_ff");
    assert!(json["ahead"].as_u64().is_some());
    assert!(json["behind"].as_u64().is_some());
    Ok(())
}

#[test]
fn pull_json_conflicts_exit_one() -> TestResult {
    let root = unique_tmp("pull-json", "conflicts");
    let upstream = root.join("upstream");
    fs::create_dir_all(&upstream)?;
    git(&upstream, &["init", "-q", "-b", "main"]);
    write_file(&upstream, "f", "base\n");
    git(&upstream, &["add", "f"]);
    git(
        &upstream,
        &[
            "-c",
            "user.name=T",
            "-c",
            "user.email=t@example.com",
            "commit",
            "-q",
            "-m",
            "base",
        ],
    );
    git(
        &root,
        &["clone", "-q", upstream.to_str().unwrap(), "client"],
    );
    let client = root.join("client");
    write_file(&client, "f", "local\n");
    git(&client, &["add", "f"]);
    git(
        &client,
        &[
            "-c",
            "user.name=T",
            "-c",
            "user.email=t@example.com",
            "commit",
            "-q",
            "-m",
            "local",
        ],
    );
    write_file(&upstream, "f", "remote\n");
    git(&upstream, &["add", "f"]);
    git(
        &upstream,
        &[
            "-c",
            "user.name=T",
            "-c",
            "user.email=t@example.com",
            "commit",
            "-q",
            "-m",
            "remote",
        ],
    );
    git(&client, &["fetch", "-q"]);
    let out = grit_cmd(&client, &["--json", "pull"]);
    assert!(!out.ok, "conflicts must exit 1");
    let json: Value = serde_json::from_str(&out.stdout)?;
    assert_eq!(json["result"], "conflicts");
    assert!(json["conflicts"].as_array().is_some());
    assert_eq!(json["operation"], "merge");
    assert!(json["next_steps"].as_array().is_some_and(|s| !s.is_empty()));
    assert!(json["alternatives"]
        .to_string()
        .contains("grit merge --abort"));
    Ok(())
}

#[test]
fn pull_json_up_to_date() -> TestResult {
    let root = unique_tmp("pull-json", "uptodate");
    let upstream = root.join("upstream");
    fs::create_dir_all(&upstream)?;
    git(&upstream, &["init", "-q", "-b", "main"]);
    commit(&upstream, "a", "seed");
    git(
        &root,
        &["clone", "-q", upstream.to_str().unwrap(), "client"],
    );
    let client = root.join("client");
    let json = gs_json(&client, &["pull"]);
    assert_eq!(json["result"], "up_to_date");
    Ok(())
}

#[test]
fn pull_json_merged() -> TestResult {
    let root = unique_tmp("pull-json", "merged");
    let upstream = root.join("upstream");
    fs::create_dir_all(&upstream)?;
    git(&upstream, &["init", "-q", "-b", "main"]);
    commit(&upstream, "a", "seed");
    git(
        &root,
        &["clone", "-q", upstream.to_str().unwrap(), "client"],
    );
    let client = root.join("client");
    commit(&client, "local", "local");
    commit(&upstream, "remote", "remote");
    git(&client, &["fetch", "-q"]);
    git(&client, &["config", "user.email", "t@example.com"]);
    git(&client, &["config", "user.name", "T"]);
    let json = gs_json(&client, &["pull", "--no-rebase"]);
    assert_eq!(json["result"], "merged");
    assert!(json["oid"].as_str().is_some());
    Ok(())
}

#[test]
fn pull_json_rebased() -> TestResult {
    let root = unique_tmp("pull-json", "rebased");
    let upstream = root.join("upstream");
    fs::create_dir_all(&upstream)?;
    git(&upstream, &["init", "-q", "-b", "main"]);
    commit(&upstream, "a", "seed");
    git(
        &root,
        &["clone", "-q", upstream.to_str().unwrap(), "client"],
    );
    let client = root.join("client");
    commit(&client, "local", "local");
    commit(&upstream, "remote", "remote");
    git(&client, &["fetch", "-q"]);
    let json = gs_json(&client, &["pull", "--rebase"]);
    assert_eq!(json["result"], "rebased");
    assert!(json["oid"].as_str().is_some());
    Ok(())
}

#[test]
fn pull_json_set_upstream_unborn() -> TestResult {
    let root = unique_tmp("pull-json", "set-upstream");
    let upstream = root.join("upstream");
    fs::create_dir_all(&upstream)?;
    git(&upstream, &["init", "-q", "-b", "main"]);
    commit(&upstream, "a", "seed");

    let client = root.join("client");
    fs::create_dir_all(&client)?;
    git(&client, &["init", "-q", "-b", "main"]);
    git(
        &client,
        &["remote", "add", "origin", upstream.to_str().unwrap()],
    );
    git(&client, &["config", "branch.main.remote", "origin"]);
    git(&client, &["config", "branch.main.merge", "refs/heads/main"]);
    let json = gs_json(&client, &["pull"]);
    assert_eq!(json["result"], "set_upstream");
    assert!(json["upstream"].as_str().is_some());
    Ok(())
}

#[test]
fn pull_json_check_shape_and_exit_codes() -> TestResult {
    let root = unique_tmp("pull-json", "check");
    let upstream = root.join("upstream");
    fs::create_dir_all(&upstream)?;
    git(&upstream, &["init", "-q", "-b", "main"]);
    commit(&upstream, "a", "seed");
    git(
        &root,
        &["clone", "-q", upstream.to_str().unwrap(), "client"],
    );
    let client = root.join("client");
    commit(&upstream, "a", "remote");
    git(&client, &["fetch", "-q"]);
    let head_before = git(&client, &["rev-parse", "HEAD"]);
    let ok = grit_cmd(&client, &["--json", "pull", "--check"]);
    assert!(ok.ok, "ff check must exit 0: {}", ok.stderr);
    let json: Value = serde_json::from_str(&ok.stdout)?;
    let check = &json["check"];
    assert_eq!(check["fast_forward_possible"], true);
    assert_eq!(check["would"], "fast_forward");
    assert_eq!(git(&client, &["rev-parse", "HEAD"]), head_before);

    commit(&client, "local", "local");
    commit(&upstream, "b", "remote2");
    git(&client, &["fetch", "-q"]);
    let bad = grit_cmd(&client, &["--json", "pull", "--check"]);
    assert!(!bad.ok, "diverged check must exit 1");
    let json: Value = serde_json::from_str(&bad.stdout)?;
    assert_eq!(json["check"]["fast_forward_possible"], false);
    assert!(json["check"]["ahead"].as_u64().is_some());
    assert!(json["check"]["behind"].as_u64().is_some());
    Ok(())
}

/// Client and upstream both change `f` after a shared base; the client has fetched.
fn diverged_conflict(tag: &str) -> std::path::PathBuf {
    let root = unique_tmp("pull-json", tag);
    let upstream = root.join("upstream");
    fs::create_dir_all(&upstream).unwrap();
    git(&upstream, &["init", "-q", "-b", "main"]);
    commit(&upstream, "f", "one\n");
    git(
        &root,
        &["clone", "-q", upstream.to_str().unwrap(), "client"],
    );
    let client = root.join("client");
    git(&client, &["config", "user.email", "t@example.com"]);
    git(&client, &["config", "user.name", "T"]);
    commit(&upstream, "f", "two\n");
    commit(&client, "f", "local\n");
    git(&client, &["fetch", "-q"]);
    client
}

#[test]
fn pull_rebase_conflict_stops_for_rebase_continue() -> TestResult {
    let client = diverged_conflict("rebase-stop");
    let local_tip = git(&client, &["rev-parse", "HEAD"]);

    let out = grit_cmd(&client, &["--json", "pull", "--rebase"]);
    assert!(!out.ok, "rebase conflicts must exit 1");
    let json: Value = serde_json::from_str(&out.stdout)?;
    assert_eq!(json["result"], "conflicts");
    assert_eq!(json["operation"], "rebase");
    assert_eq!(json["commit"], local_tip.trim());
    assert!(json.get("oid").is_none(), "nothing was integrated: {json}");
    assert_eq!(json["conflicts"], serde_json::json!(["f"]));
    assert_eq!(json["conflict_details"][0]["kind"], "content");
    let next: Vec<&str> = json["next_steps"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(Value::as_str)
        .collect();
    assert_eq!(next.last(), Some(&"grit rebase --continue"));
    let alternatives = json["alternatives"].to_string();
    assert!(
        alternatives.contains("grit rebase --abort"),
        "{alternatives}"
    );
    assert!(
        alternatives.contains("grit rebase --skip"),
        "{alternatives}"
    );

    let status = gs_json(&client, &["status"]);
    assert_eq!(status["in_progress"], serde_json::json!(["rebase"]));
    assert_eq!(status["conflicts"][0]["path"], "f");
    assert_eq!(git(&client, &["status", "--porcelain"]), "UU f\n");
    assert!(fs::read_to_string(client.join("f"))?.contains("<<<<<<<"));

    write_file(&client, "f", "resolved\n");
    git(&client, &["add", "f"]);
    let cont = grit_cmd(&client, &["--json", "rebase", "--continue"]);
    assert!(cont.ok, "rebase --continue: {}", cont.stderr);
    assert_eq!(git(&client, &["show", "HEAD:f"]), "resolved\n");
    assert_eq!(
        git(&client, &["rev-parse", "HEAD~1"]),
        git(&client, &["rev-parse", "origin/main"])
    );
    assert_eq!(git(&client, &["status", "--porcelain"]), "");
    Ok(())
}

#[test]
fn pull_rebase_conflict_human_and_markdown_name_the_rebase() -> TestResult {
    let client = diverged_conflict("rebase-stop-human");
    let human = grit_cmd(&client, &["pull", "--rebase"]);
    assert!(!human.ok);
    assert!(
        human.stderr.contains("CONFLICT (content): f"),
        "{}",
        human.stderr
    );
    assert!(
        human.stderr.contains("grit rebase --continue"),
        "{}",
        human.stderr
    );
    assert!(!human.stderr.contains("unchanged"), "{}", human.stderr);
    let abort = grit_cmd(&client, &["rebase", "--abort"]);
    assert!(abort.ok, "{}", abort.stderr);

    let md = grit_cmd(&client, &["--markdown", "pull", "--rebase"]);
    assert!(!md.ok);
    assert!(
        md.stdout.contains("Rebase stopped at commit"),
        "{}",
        md.stdout
    );
    assert!(
        md.stdout.contains("`grit rebase --continue`"),
        "{}",
        md.stdout
    );
    assert!(md.stdout.contains("`grit rebase --abort`"), "{}", md.stdout);
    Ok(())
}

#[test]
fn pull_rebase_conflict_reports_kept_autostash() -> TestResult {
    let client = diverged_conflict("rebase-stop-autostash");
    write_file(&client, "dirty.txt", "work in progress\n");
    git(&client, &["add", "dirty.txt"]);
    let out = grit_cmd(&client, &["--json", "pull", "--rebase", "--autostash"]);
    assert!(!out.ok);
    let json: Value = serde_json::from_str(&out.stdout)?;
    assert_eq!(json["operation"], "rebase");
    assert_eq!(
        json["autostash"],
        git(&client, &["rev-parse", "refs/stash"]).trim()
    );
    assert!(!client.join("dirty.txt").exists());
    Ok(())
}

#[test]
fn pull_rebase_check_predicts_conflicts() -> TestResult {
    let client = diverged_conflict("rebase-check");
    git(&client, &["config", "pull.rebase", "true"]);
    let head_before = git(&client, &["rev-parse", "HEAD"]);
    let out = grit_cmd(&client, &["--json", "pull", "--check"]);
    assert!(!out.ok, "a conflicting rebase is not a fast-forward");
    let json: Value = serde_json::from_str(&out.stdout)?;
    assert_eq!(json["check"]["would"], "conflicts");
    assert_eq!(git(&client, &["rev-parse", "HEAD"]), head_before);
    assert!(!client.join(".git/rebase-merge").exists());
    Ok(())
}

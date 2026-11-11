//! Pinned wording for errors whose library variants were reshaped (revision misses, a
//! missing tree member during commit, a push source missing locally), so a library change
//! cannot silently alter what users read.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::path::Path;
use std::process::Command;

struct Run {
    code: i32,
    stdout: String,
    stderr: String,
}

fn grit(cwd: &Path, args: &[&str]) -> Run {
    let out = Command::new(env!("CARGO_BIN_EXE_grit"))
        .current_dir(cwd)
        .args(args)
        .env("GIT_AUTHOR_NAME", "Pin Tester")
        .env("GIT_AUTHOR_EMAIL", "pin@example.com")
        .env("GIT_COMMITTER_NAME", "Pin Tester")
        .env("GIT_COMMITTER_EMAIL", "pin@example.com")
        .output()
        .expect("spawn grit");
    Run {
        code: out.status.code().unwrap_or(-1),
        stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
    }
}

fn grit_ok(cwd: &Path, args: &[&str]) -> String {
    let run = grit(cwd, args);
    assert_eq!(run.code, 0, "grit {args:?}: {}{}", run.stdout, run.stderr);
    run.stdout
}

/// A repository with one commit of `a.txt`.
fn repo_with_commit(dir: &Path) {
    grit_ok(dir, &["init", "."]);
    std::fs::write(dir.join("a.txt"), "a\n").expect("write a.txt");
    grit_ok(dir, &["add", "a.txt"]);
    grit_ok(dir, &["commit", "first"]);
}

/// Human stderr and `--json` stdout for `args`, both failing with exit 1.
fn failure(dir: &Path, args: &[&str]) -> (String, serde_json::Value) {
    let human = grit(dir, args);
    assert_eq!(
        human.code, 1,
        "grit {args:?}: {}{}",
        human.stdout, human.stderr
    );
    let mut json_args = vec!["--json"];
    json_args.extend_from_slice(args);
    let json = grit(dir, &json_args);
    assert_eq!(
        json.code, 1,
        "grit {json_args:?}: {}{}",
        json.stdout, json.stderr
    );
    let value = serde_json::from_str(json.stdout.trim()).expect("json error payload");
    (human.stderr, value)
}

#[test]
fn revision_miss_reads_revision_not_found() {
    let dir = tempfile::tempdir().expect("tempdir");
    repo_with_commit(dir.path());
    for args in [
        ["show", "deadbeef"].as_slice(),
        ["stash", "show", "deadbeef"].as_slice(),
    ] {
        let (stderr, json) = failure(dir.path(), args);
        assert_eq!(stderr, "error: revision 'deadbeef' not found\n", "{args:?}");
        assert_eq!(json["error"], "revision 'deadbeef' not found", "{args:?}");
    }
}

#[test]
fn switch_start_point_miss_reads_unknown_revision() {
    let dir = tempfile::tempdir().expect("tempdir");
    repo_with_commit(dir.path());
    for args in [
        ["switch", "--detach", "deadbeef"].as_slice(),
        ["switch", "--create", "topic", "deadbeef"].as_slice(),
    ] {
        let (stderr, json) = failure(dir.path(), args);
        assert_eq!(stderr, "error: unknown revision 'deadbeef'\n", "{args:?}");
        assert_eq!(json["error"], "unknown revision 'deadbeef'", "{args:?}");
    }
}

#[test]
fn merge_miss_reads_no_branch_tag_or_commit() {
    let dir = tempfile::tempdir().expect("tempdir");
    repo_with_commit(dir.path());
    let (stderr, json) = failure(dir.path(), &["merge", "deadbeef"]);
    assert_eq!(stderr, "error: no branch, tag or commit named 'deadbeef'\n");
    assert_eq!(json["error"], "no branch, tag or commit named 'deadbeef'");
    assert_eq!(json["kind"], "revision_not_found");
}

#[test]
fn commit_with_missing_blob_names_the_path() {
    let dir = tempfile::tempdir().expect("tempdir");
    repo_with_commit(dir.path());
    std::fs::write(dir.path().join("b.txt"), "b\n").expect("write b.txt");
    grit_ok(dir.path(), &["add", "b.txt"]);
    let resolved = grit_ok(dir.path(), &["resolve", ":b.txt"]);
    let oid = resolved
        .split_whitespace()
        .next()
        .expect("blob id")
        .to_owned();
    let loose = dir
        .path()
        .join(".git/objects")
        .join(&oid[..2])
        .join(&oid[2..]);
    std::fs::remove_file(&loose).expect("remove loose blob");

    let expected = format!("object not found: 100644 {oid} at b.txt");
    let (stderr, json) = failure(dir.path(), &["commit", "second"]);
    assert_eq!(stderr, format!("error: {expected}\n"));
    assert_eq!(json["error"], expected.as_str());
}

#[test]
fn push_with_missing_source_names_the_destination() {
    let dir = tempfile::tempdir().expect("tempdir");
    let work = dir.path().join("work");
    let remote = dir.path().join("remote.git");
    std::fs::create_dir_all(&work).expect("mkdir work");
    std::fs::create_dir_all(&remote).expect("mkdir remote");
    grit_ok(&remote, &["init", "--bare", "."]);
    repo_with_commit(&work);
    grit_ok(
        &work,
        &[
            "remote",
            "add",
            "origin",
            remote.to_str().expect("utf-8 path"),
        ],
    );
    let missing = "deadbeefdeadbeefdeadbeefdeadbeefdeadbeef";
    std::fs::write(work.join(".git/refs/heads/broken"), format!("{missing}\n"))
        .expect("write broken ref");

    let expected = format!(
        "object not found: source object {missing} for 'refs/heads/broken' is missing from the local object store"
    );
    let (stderr, json) = failure(&work, &["push", "origin", "broken"]);
    assert_eq!(stderr, format!("error: {expected}\n"));
    assert_eq!(json["error"], expected.as_str());
}

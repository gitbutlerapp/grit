//! `grit log -p`, `--stat`, `--name-only`, `--name-status` vs `git log` and per-commit `grit diff`.

use grit_test_support::{git, grit};
use serde_json::Value;

fn init_repo(dir: &std::path::Path) {
    assert!(grit(dir, &["init"]).ok(), "grit init");
}

fn commit_all(dir: &std::path::Path, msg: &str) {
    git(dir, &["add", "-A"]);
    git(dir, &["commit", "-m", msg]);
}

#[test]
fn log_patch_files_equal_diff_per_commit() {
    let dir = tempfile::tempdir().expect("tempdir");
    init_repo(dir.path());
    std::fs::write(dir.path().join("a.txt"), b"one\n").expect("write");
    commit_all(dir.path(), "c1");
    std::fs::write(dir.path().join("a.txt"), b"one\ntwo\n").expect("write");
    commit_all(dir.path(), "c2");
    std::fs::write(dir.path().join("b.txt"), b"x\n").expect("write");
    commit_all(dir.path(), "c3");

    let log_out = grit(dir.path(), &["log", "--json", "-p", "--limit", "3"]);
    assert!(log_out.ok(), "{}", log_out.dump("log -p json"));
    let log_json: Value = serde_json::from_str(log_out.stdout.trim()).expect("log json");
    let commits = log_json["commits"].as_array().expect("commits array");
    assert_eq!(commits.len(), 3);
    for entry in commits {
        let oid = entry["oid"].as_str().expect("oid");
        let log_files = entry["files"].as_array().expect("files");
        let diff_out = grit(dir.path(), &["diff", "--json", oid]);
        assert!(diff_out.ok(), "{}", diff_out.dump("diff json"));
        let diff_json: Value = serde_json::from_str(diff_out.stdout.trim()).expect("diff json");
        let diff_files = diff_json["files"].as_array().expect("diff files");
        assert_eq!(log_files, diff_files, "files differ for {oid}");
    }
}

#[test]
fn log_name_status_matches_git_log_name_status() {
    let dir = tempfile::tempdir().expect("tempdir");
    init_repo(dir.path());
    std::fs::write(dir.path().join("f.txt"), b"line\n").expect("write");
    commit_all(dir.path(), "init");
    git(dir.path(), &["mv", "f.txt", "g.txt"]);
    commit_all(dir.path(), "rename");

    let grit_out = grit(
        dir.path(),
        &["log", "--json", "--name-status", "--limit", "2"],
    );
    assert!(grit_out.ok(), "{}", grit_out.dump("log name-status"));
    let grit_json: Value = serde_json::from_str(grit_out.stdout.trim()).expect("json");
    for entry in grit_json["commits"].as_array().expect("commits") {
        let oid = entry["oid"].as_str().expect("oid");
        let files = entry["files"].as_array().expect("files");
        let mut grit_lines: Vec<String> = files
            .iter()
            .map(|f| {
                let status = f["status"].as_str().unwrap_or("");
                let path = f["path"].as_str().unwrap_or("");
                match status {
                    "renamed" => {
                        let old = f["old_path"].as_str().unwrap_or("");
                        let sim = f["similarity"].as_u64().unwrap_or(100);
                        format!("R{sim:03}\t{old}\t{path}")
                    }
                    "added" => format!("A\t{path}"),
                    "deleted" => format!("D\t{path}"),
                    "modified" => format!("M\t{path}"),
                    other => panic!("unexpected status {other}"),
                }
            })
            .collect();
        grit_lines.sort();
        let git_show = std::process::Command::new("git")
            .current_dir(dir.path())
            .args(["show", "--name-status", "-M50", "--format=", oid])
            .output()
            .expect("git show name-status");
        assert!(git_show.status.success());
        let mut git_lines: Vec<String> = String::from_utf8_lossy(&git_show.stdout)
            .lines()
            .filter(|l| !l.is_empty())
            .map(str::to_owned)
            .collect();
        git_lines.sort();
        assert_eq!(grit_lines, git_lines, "name-status for {oid}");
    }
}

#[test]
fn log_stat_numbers_match_git_log_numstat() {
    let dir = tempfile::tempdir().expect("tempdir");
    init_repo(dir.path());
    std::fs::write(dir.path().join("a.txt"), b"a\n").expect("write");
    commit_all(dir.path(), "one");
    std::fs::write(dir.path().join("a.txt"), b"a\nb\n").expect("write");
    commit_all(dir.path(), "two");

    let grit_out = grit(dir.path(), &["log", "--json", "--numstat", "--limit", "2"]);
    assert!(grit_out.ok(), "{}", grit_out.dump("log numstat"));
    let grit_json: Value = serde_json::from_str(grit_out.stdout.trim()).expect("json");
    for entry in grit_json["commits"].as_array().expect("commits") {
        let oid = entry["oid"].as_str().expect("oid");
        let files = entry["files"].as_array().expect("files");
        let grit_line = files
            .iter()
            .map(|f| {
                let ins = f["stat"]["insertions"].as_u64().unwrap_or(0);
                let del = f["stat"]["deletions"].as_u64().unwrap_or(0);
                let path = f["path"].as_str().unwrap_or("");
                format!("{ins}\t{del}\t{path}")
            })
            .collect::<Vec<_>>()
            .join("\n");
        let git_show = std::process::Command::new("git")
            .current_dir(dir.path())
            .args(["show", "--numstat", "--format=", oid])
            .output()
            .expect("git show numstat");
        assert!(git_show.status.success());
        let git_block = String::from_utf8_lossy(&git_show.stdout)
            .trim_end()
            .to_string();
        assert_eq!(grit_line, git_block, "numstat for {oid}");
    }
}

#[test]
fn log_p_with_paths_limits_patch_like_git() {
    let dir = tempfile::tempdir().expect("tempdir");
    init_repo(dir.path());
    std::fs::write(dir.path().join("keep.txt"), b"1\n").expect("write");
    std::fs::write(dir.path().join("skip.txt"), b"1\n").expect("write");
    commit_all(dir.path(), "init");
    std::fs::write(dir.path().join("keep.txt"), b"1\n2\n").expect("write");
    std::fs::write(dir.path().join("skip.txt"), b"1\n9\n").expect("write");
    commit_all(dir.path(), "change");

    let git_out = std::process::Command::new("git")
        .current_dir(dir.path())
        .args(["log", "-p", "-1", "--", "keep.txt"])
        .output()
        .expect("git log -p path");
    assert!(git_out.status.success());
    let git_patch = String::from_utf8_lossy(&git_out.stdout);

    let grit_out = grit(dir.path(), &["log", "-p", "--limit", "1", "--", "keep.txt"]);
    assert!(grit_out.ok(), "{}", grit_out.dump("log -p path"));
    assert!(
        grit_out.stdout.contains("keep.txt"),
        "expected keep.txt in patch: {}",
        grit_out.stdout
    );
    assert!(
        !grit_out.stdout.contains("skip.txt"),
        "skip.txt should not appear: {}",
        grit_out.stdout
    );
    assert!(
        git_patch.contains("keep.txt") && !git_patch.contains("skip.txt"),
        "git baseline"
    );
}

#[test]
fn log_p_skips_merge_patches_like_git() {
    let dir = tempfile::tempdir().expect("tempdir");
    init_repo(dir.path());
    std::fs::write(dir.path().join("f.txt"), b"base\n").expect("write");
    commit_all(dir.path(), "base");
    git(dir.path(), &["checkout", "-b", "side"]);
    std::fs::write(dir.path().join("f.txt"), b"side\n").expect("write");
    commit_all(dir.path(), "side");
    git(dir.path(), &["checkout", "main"]);
    git(
        dir.path(),
        &["merge", "--no-ff", "side", "-m", "merge side"],
    );

    let git_out = std::process::Command::new("git")
        .current_dir(dir.path())
        .args(["log", "-p", "-1", "--merges"])
        .output()
        .expect("git log merge");
    assert!(git_out.status.success());
    let git_text = String::from_utf8_lossy(&git_out.stdout);
    assert!(
        git_text.contains("merge side"),
        "expected merge commit in git log: {git_text}"
    );
    assert!(
        !git_text.contains("diff --git"),
        "git should skip merge patch by default: {git_text}"
    );

    let grit_json_out = grit(dir.path(), &["log", "--json", "-p", "--limit", "1"]);
    assert!(
        grit_json_out.ok(),
        "{}",
        grit_json_out.dump("log merge json -p")
    );
    let grit_json: Value = serde_json::from_str(grit_json_out.stdout.trim()).expect("json");
    let files = grit_json["commits"][0]["files"]
        .as_array()
        .expect("files array");
    assert!(
        files.is_empty(),
        "merge commit should have no patch files: {files:?}"
    );
}

//! `grit log` / `grit revs` revision selection vs system `git`.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::error::Error;
use std::fs;
use std::path::Path;
use std::process::Command;

use serde_json::Value;

type TestResult = Result<(), Box<dyn Error>>;

const GRIT: &str = env!("CARGO_BIN_EXE_grit");

fn null_device() -> &'static str {
    if cfg!(windows) {
        "NUL"
    } else {
        "/dev/null"
    }
}

fn grit_json(dir: &Path, args: &[&str]) -> Value {
    let out = Command::new(GRIT)
        .arg("--json")
        .args(args)
        .current_dir(dir)
        .env("GIT_CONFIG_GLOBAL", null_device())
        .env("GIT_CONFIG_SYSTEM", null_device())
        .output()
        .expect("spawn grit");
    assert_eq!(
        out.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    serde_json::from_slice(&out.stdout).expect("json")
}

fn grit_run(dir: &Path, args: &[&str]) -> std::process::Output {
    Command::new(GRIT)
        .args(args)
        .current_dir(dir)
        .env("GIT_CONFIG_GLOBAL", null_device())
        .env("GIT_CONFIG_SYSTEM", null_device())
        .output()
        .expect("spawn grit")
}

fn git_oids(dir: &Path, git_args: &[&str]) -> Vec<String> {
    let mut argv = vec!["-C", dir.to_str().expect("utf8"), "log", "--format=%H"];
    argv.extend(git_args);
    let out = Command::new("git")
        .args(&argv)
        .env("GIT_CONFIG_GLOBAL", null_device())
        .env("GIT_CONFIG_SYSTEM", null_device())
        .output()
        .expect("git log");
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter(|l| !l.is_empty())
        .map(str::to_owned)
        .collect()
}

fn git_branch(dir: &Path, name: &str, start: &str) {
    let out = Command::new("git")
        .args(["-C", dir.to_str().expect("utf8"), "branch", name, start])
        .env("GIT_CONFIG_GLOBAL", null_device())
        .env("GIT_CONFIG_SYSTEM", null_device())
        .output()
        .expect("git branch");
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
}

fn grit_log_oids(dir: &Path, args: &[&str]) -> Vec<String> {
    let v = grit_json(dir, args);
    v["commits"]
        .as_array()
        .expect("commits")
        .iter()
        .map(|c| c["oid"].as_str().expect("oid").to_owned())
        .collect()
}

fn seed_linear_repo(dir: &Path, n: usize) {
    fs::create_dir_all(dir).expect("mkdir");
    assert!(grit_run(dir, &["init", "."]).status.success());
    for i in 0..n {
        fs::write(dir.join("f.txt"), format!("v{i}\n")).expect("write");
        assert!(grit_run(dir, &["add", "f.txt"]).status.success());
        assert!(grit_run(dir, &["commit", &format!("c{i}")])
            .status
            .success());
    }
}

fn seed_diverged_repo(dir: &Path) {
    seed_linear_repo(dir, 3);
    assert!(grit_run(dir, &["branch", "side"]).status.success());
    assert!(grit_run(dir, &["switch", "side"]).status.success());
    fs::write(dir.join("side.txt"), "side\n").expect("write");
    assert!(grit_run(dir, &["add", "side.txt"]).status.success());
    assert!(grit_run(dir, &["commit", "side-tip"]).status.success());
    assert!(grit_run(dir, &["switch", "main"]).status.success());
    fs::write(dir.join("main.txt"), "main\n").expect("write");
    assert!(grit_run(dir, &["add", "main.txt"]).status.success());
    assert!(grit_run(dir, &["commit", "main-tip"]).status.success());
    assert!(grit_run(dir, &["merge", "side"]).status.success());
}

#[test]
fn log_range_matches_git() -> TestResult {
    let dir = std::env::temp_dir().join(format!("grit-rev-select-range-{}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    seed_diverged_repo(&dir);
    let git = git_oids(&dir, &["main..HEAD"]);
    let grit = grit_log_oids(&dir, &["log", "-n", "50", "main..HEAD"]);
    assert_eq!(grit, git);
    Ok(())
}

#[test]
fn log_one_shot_limit_emits_no_next_cursor() -> TestResult {
    let dir = std::env::temp_dir().join(format!("grit-log-one-shot-limit-{}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    seed_linear_repo(&dir, 20);
    let out = grit_json(&dir, &["log", "-n", "2"]);
    assert_eq!(out["commits"].as_array().expect("commits").len(), 2);
    assert_eq!(out["truncated"], Value::Bool(true));
    assert!(
        out["next"].is_null(),
        "one-shot -n must not emit next: {out}"
    );
    Ok(())
}

#[test]
fn log_since_until_matches_git() -> TestResult {
    let dir = std::env::temp_dir().join(format!("grit-rev-select-since-{}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    seed_linear_repo(&dir, 5);
    let git = git_oids(&dir, &["--since=1", "-n", "3"]);
    let grit = grit_log_oids(&dir, &["log", "--since=1", "-n", "3"]);
    assert_eq!(grit, git);
    Ok(())
}

#[test]
fn log_before_after_date_aliases() -> TestResult {
    let dir =
        std::env::temp_dir().join(format!("grit-rev-select-date-alias-{}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    seed_linear_repo(&dir, 4);
    let git = git_oids(&dir, &["--after=1", "--before=9999999999", "-n", "2"]);
    let grit = grit_log_oids(
        &dir,
        &["log", "--after=1", "--before=9999999999", "-n", "2"],
    );
    assert_eq!(grit, git);
    Ok(())
}

#[test]
fn log_before_cursor_rejected() -> TestResult {
    let dir = std::env::temp_dir().join(format!(
        "grit-rev-select-cursor-reject-{}",
        std::process::id()
    ));
    let _ = fs::remove_dir_all(&dir);
    seed_linear_repo(&dir, 22);
    let page1 = grit_json(&dir, &["log"]);
    let cursor = page1["next"].as_str().expect("next").to_owned();
    let out = grit_run(&dir, &["log", &format!("--before={cursor}")]);
    assert_eq!(
        out.status.code(),
        Some(2),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    Ok(())
}

#[test]
fn log_cursor_pages_through_range() -> TestResult {
    let dir = std::env::temp_dir().join(format!(
        "grit-rev-select-cursor-range-{}",
        std::process::id()
    ));
    let _ = fs::remove_dir_all(&dir);
    seed_linear_repo(&dir, 3);
    assert!(grit_run(&dir, &["branch", "base"]).status.success());
    for i in 0..22 {
        fs::write(dir.join("f.txt"), format!("main{i}\n")).expect("write");
        assert!(grit_run(&dir, &["add", "f.txt"]).status.success());
        assert!(grit_run(&dir, &["commit", &format!("m{i}")])
            .status
            .success());
    }
    let git = git_oids(&dir, &["base..HEAD"]);
    let page1 = grit_json(&dir, &["log", "base..HEAD"]);
    assert_eq!(page1["commits"].as_array().expect("commits").len(), 20);
    let cursor = page1["next"].as_str().expect("next cursor for range walk");
    let page2 = grit_json(&dir, &["log", &format!("--cursor={cursor}")]);
    assert!(page2["next"].is_null());
    let paged: Vec<String> = page1["commits"]
        .as_array()
        .expect("commits")
        .iter()
        .chain(page2["commits"].as_array().expect("commits").iter())
        .map(|c| c["oid"].as_str().expect("oid").to_owned())
        .collect();
    assert_eq!(paged, git);
    Ok(())
}

#[test]
fn log_cursor_continues_range_with_cursor_only() -> TestResult {
    let dir = std::env::temp_dir().join(format!(
        "grit-rev-select-cursor-only-{}",
        std::process::id()
    ));
    let _ = fs::remove_dir_all(&dir);
    seed_linear_repo(&dir, 23);
    git_branch(&dir, "base", "HEAD~22");
    let page1 = grit_json(&dir, &["log", "base..HEAD"]);
    let cursor = page1["next"].as_str().expect("next").to_owned();
    let page2 = grit_json(&dir, &["log", &format!("--cursor={cursor}")]);
    let paged: Vec<String> = page1["commits"]
        .as_array()
        .expect("commits")
        .iter()
        .chain(page2["commits"].as_array().expect("commits").iter())
        .map(|c| c["oid"].as_str().expect("oid").to_owned())
        .collect();
    assert_eq!(paged, git_oids(&dir, &["base..HEAD"]));
    Ok(())
}

#[test]
fn log_cursor_rejects_revision_selection() -> TestResult {
    let dir = std::env::temp_dir().join(format!(
        "grit-rev-select-cursor-reject-revs-{}",
        std::process::id()
    ));
    let _ = fs::remove_dir_all(&dir);
    seed_linear_repo(&dir, 23);
    git_branch(&dir, "base", "HEAD~22");
    let page1 = grit_json(&dir, &["log", "base..HEAD"]);
    let cursor = page1["next"].as_str().expect("next").to_owned();
    let out = grit_run(&dir, &["log", &format!("--cursor={cursor}"), "base..HEAD"]);
    assert!(!out.status.success());
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("cannot combine `--cursor`"),
        "stderr: {stderr}"
    );
    Ok(())
}

#[test]
fn log_cursor_rejects_date_selectors() -> TestResult {
    let dir = std::env::temp_dir().join(format!(
        "grit-rev-select-cursor-dates-{}",
        std::process::id()
    ));
    let _ = fs::remove_dir_all(&dir);
    seed_linear_repo(&dir, 23);
    git_branch(&dir, "base", "HEAD~22");
    let page1 = grit_json(&dir, &["log", "base..HEAD"]);
    let cursor = page1["next"].as_str().expect("next").to_owned();
    for extra in [
        "--since=9999999999",
        "--before=9999999999",
        "--until=1",
        "--after=1",
    ] {
        let out = grit_run(&dir, &["log", &format!("--cursor={cursor}"), extra]);
        assert!(!out.status.success(), "expected failure with {extra}");
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert!(
            stderr.contains("cannot combine `--cursor`"),
            "stderr for {extra}: {stderr}"
        );
    }
    Ok(())
}

#[test]
fn log_cursor_rejects_ref_set_selectors() -> TestResult {
    let dir = std::env::temp_dir().join(format!(
        "grit-rev-select-cursor-refs-{}",
        std::process::id()
    ));
    let _ = fs::remove_dir_all(&dir);
    seed_linear_repo(&dir, 22);
    let page1 = grit_json(&dir, &["log"]);
    let cursor = page1["next"].as_str().expect("next").to_owned();
    for extra in ["--all", "--branches", "--tags", "--remotes"] {
        let out = grit_run(&dir, &["log", &format!("--cursor={cursor}"), extra]);
        assert!(!out.status.success(), "expected failure with {extra}");
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert!(
            stderr.contains("cannot combine `--cursor`"),
            "stderr for {extra}: {stderr}"
        );
    }
    Ok(())
}

fn git_ok(dir: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .args(args)
        .current_dir(dir)
        .env("GIT_CONFIG_GLOBAL", null_device())
        .env("GIT_CONFIG_SYSTEM", null_device())
        .env("GIT_AUTHOR_NAME", "T")
        .env("GIT_AUTHOR_EMAIL", "t@e.x")
        .env("GIT_COMMITTER_NAME", "T")
        .env("GIT_COMMITTER_EMAIL", "t@e.x")
        .output()
        .expect("spawn git");
    assert!(
        out.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).into_owned()
}

/// Issue #2163: `feat` is rebased onto a new `main` commit and `c3` reworded; `old-feat` is
/// the reviewed version.
fn seed_rewritten_branch_repo(dir: &Path) {
    let _ = fs::remove_dir_all(dir);
    fs::create_dir_all(dir).expect("mkdir");
    git_ok(dir, &["init", "-q", "-b", "main", "."]);
    let commit = |path: &str, content: &str, msg: &str| {
        fs::write(dir.join(path), content).expect("write");
        git_ok(dir, &["add", path]);
        git_ok(dir, &["commit", "-q", "-m", msg]);
    };
    commit("f", "0\n", "base");
    git_ok(dir, &["switch", "-q", "-c", "feat"]);
    for i in 1..=3 {
        commit(&format!("f{i}"), &format!("{i}\n"), &format!("c{i}"));
    }
    git_ok(dir, &["branch", "old-feat"]);
    git_ok(dir, &["switch", "-q", "main"]);
    commit("m", "m\n", "m");
    git_ok(dir, &["switch", "-q", "feat"]);
    git_ok(dir, &["rebase", "-q", "main"]);
    git_ok(dir, &["commit", "-q", "--amend", "-m", "c3 reworded"]);
    commit("f1", "edited\n", "c4 edits f1");
}

#[test]
fn revs_cherry_mark_pairs_rewritten_branch_like_git() -> TestResult {
    let dir = std::env::temp_dir().join(format!("grit-rev-select-cherry-{}", std::process::id()));
    seed_rewritten_branch_repo(&dir);
    let range = "old-feat...feat";
    let git_lines: Vec<(char, String)> =
        git_ok(&dir, &["rev-list", "--left-right", "--cherry-mark", range])
            .lines()
            .map(|l| (l.chars().next().unwrap(), l[1..].trim().to_owned()))
            .collect();

    let v = grit_json(&dir, &["revs", "--cherry-mark", range]);
    let commits = v["commits"].as_array().expect("commits");
    let grit_lines: Vec<(char, String)> = commits
        .iter()
        .map(|c| {
            let marker = match (c["equivalent"].as_bool(), c["side"].as_str()) {
                (Some(true), _) => '=',
                (_, Some("left")) => '<',
                _ => '>',
            };
            (marker, c["oid"].as_str().unwrap().to_owned())
        })
        .collect();
    assert_eq!(grit_lines, git_lines);

    let rev = |spec: &str| git_ok(&dir, &["rev-parse", spec]).trim().to_owned();
    let by_oid = |oid: &str| commits.iter().find(|c| c["oid"] == oid).expect(oid);
    let old_c3 = by_oid(&rev("old-feat"));
    assert_eq!(old_c3["side"], "left");
    assert_eq!(old_c3["equivalent_to"], rev("feat~1"));
    assert_eq!(old_c3["message_changed"], true);
    let new_c1 = by_oid(&rev("feat~3"));
    assert_eq!(new_c1["side"], "right");
    assert_eq!(new_c1["equivalent_to"], rev("old-feat~2"));
    assert_eq!(new_c1["message_changed"], false);
    let c4 = by_oid(&rev("feat"));
    assert_eq!(c4["equivalent"], false);
    assert!(c4.get("equivalent_to").is_none());

    let human = grit_run(&dir, &["revs", "--cherry-mark", range]);
    assert!(human.status.success());
    let markers: Vec<char> = String::from_utf8_lossy(&human.stdout)
        .lines()
        .map(|l| l.chars().next().unwrap())
        .collect();
    assert_eq!(
        markers,
        git_lines.iter().map(|(m, _)| *m).collect::<Vec<_>>()
    );

    let lr = grit_json(&dir, &["revs", "--left-right", "-n", "2", range]);
    assert_eq!(lr["truncated"], true);
    assert_eq!(lr["commits"][0]["side"], "left");
    assert!(lr["commits"][0].get("equivalent").is_none());

    let plain = grit_json(&dir, &["revs", "-n", "1", "feat"]);
    assert!(
        plain["commits"][0].get("side").is_none(),
        "plain rows unchanged"
    );

    for args in [
        &["revs", "--cherry-mark", "feat"][..],
        &["revs", "--left-right", "main..feat"],
        &["revs", "--cherry-mark", "-n", "0", "feat"],
        &["revs", "--left-right", "-n", "0", "main..feat"],
        &["revs", "--count", "--left-right", "-n", "0", "feat"],
    ] {
        let out = grit_run(&dir, args);
        assert_eq!(out.status.code(), Some(1), "{args:?}");
        assert!(!out.stderr.is_empty(), "{args:?} explains the error");
    }
    let empty = grit_json(&dir, &["revs", "--cherry-mark", "-n", "0", range]);
    assert_eq!(empty["commits"].as_array().map(Vec::len), Some(0));
    let _ = fs::remove_dir_all(&dir);
    Ok(())
}

#[test]
fn patch_id_expands_ranges_like_git() -> TestResult {
    let dir = std::env::temp_dir().join(format!("grit-patch-id-range-{}", std::process::id()));
    seed_rewritten_branch_repo(&dir);
    let mut want = Vec::new();
    for commit in git_ok(&dir, &["rev-list", "main..old-feat"]).lines() {
        let show = git_ok(&dir, &["show", commit]);
        let mut child = Command::new("git")
            .args(["patch-id", "--stable"])
            .current_dir(&dir)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .spawn()?;
        std::io::Write::write_all(&mut child.stdin.take().expect("stdin"), show.as_bytes())?;
        want.push(
            String::from_utf8(child.wait_with_output()?.stdout)?
                .trim()
                .to_owned(),
        );
    }
    assert_eq!(want.len(), 3);

    let v = grit_json(&dir, &["patch-id", "main..old-feat"]);
    let got: Vec<String> = v["entries"]
        .as_array()
        .expect("entries")
        .iter()
        .map(|e| {
            format!(
                "{} {}",
                e["patch_id"].as_str().unwrap(),
                e["commit"].as_str().unwrap()
            )
        })
        .collect();
    assert_eq!(got, want);

    let human = grit_run(&dir, &["patch-id", "main..old-feat"]);
    assert!(human.status.success());
    assert_eq!(
        String::from_utf8(human.stdout)?.lines().collect::<Vec<_>>(),
        want
    );
    let _ = fs::remove_dir_all(&dir);
    Ok(())
}

#[test]
fn revs_left_right_count_json() -> TestResult {
    let dir = std::env::temp_dir().join(format!("grit-rev-select-lr-{}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    seed_diverged_repo(&dir);
    let out = Command::new("git")
        .args([
            "-C",
            dir.to_str().expect("utf8"),
            "rev-list",
            "--left-right",
            "--count",
            "main...side",
        ])
        .env("GIT_CONFIG_GLOBAL", null_device())
        .env("GIT_CONFIG_SYSTEM", null_device())
        .output()?;
    assert!(out.status.success());
    let stdout = String::from_utf8_lossy(&out.stdout);
    let mut parts = stdout.split_whitespace();
    let git_left: usize = parts.next().unwrap_or("0").parse()?;
    let git_right: usize = parts.next().unwrap_or("0").parse()?;
    let v = grit_json(&dir, &["revs", "--count", "--left-right", "main...side"]);
    assert_eq!(v["left_right"]["left"], git_left);
    assert_eq!(v["left_right"]["right"], git_right);
    assert!(v["count"].is_null());
    Ok(())
}

#[test]
fn revs_all_branches_tags_remotes_match_git() -> TestResult {
    let dir = std::env::temp_dir().join(format!("grit-rev-select-refs-{}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    seed_diverged_repo(&dir);
    assert!(grit_run(&dir, &["tag", "t1"]).status.success());
    for (flag, grit_flag) in [
        ("--all", "--all"),
        ("--branches", "--branches"),
        ("--tags", "--tags"),
    ] {
        let mut git = git_rev_list_oids(&dir, &[flag]);
        let mut grit = grit_revs_oids(&dir, &["revs", grit_flag]);
        if flag == "--all" {
            // Multiple ref tips can share committer time; Git and grit may tie-break differently.
            git.sort();
            grit.sort();
        }
        assert_eq!(grit, git, "{flag}");
    }
    Ok(())
}

#[test]
fn limit_hidden_alias_still_works() -> TestResult {
    let dir = std::env::temp_dir().join(format!(
        "grit-rev-select-limit-alias-{}",
        std::process::id()
    ));
    let _ = fs::remove_dir_all(&dir);
    seed_linear_repo(&dir, 6);
    let git = git_oids(&dir, &["-n", "3"]);
    let grit = grit_log_oids(&dir, &["log", "--limit", "3"]);
    assert_eq!(grit, git);
    Ok(())
}

#[test]
fn revs_n_zero_left_right_json_shape() -> TestResult {
    let dir = std::env::temp_dir().join(format!("grit-rev-select-lr-zero-{}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    seed_diverged_repo(&dir);
    let v = grit_json(
        &dir,
        &["revs", "--count", "--left-right", "-n", "0", "main...side"],
    );
    assert!(v["count"].is_null());
    assert_eq!(v["left_right"]["left"], 0);
    assert_eq!(v["left_right"]["right"], 0);
    Ok(())
}

#[test]
fn log_cursor_rejects_mismatched_revision_selection() -> TestResult {
    let dir = std::env::temp_dir().join(format!(
        "grit-rev-select-cursor-mismatch-{}",
        std::process::id()
    ));
    let _ = fs::remove_dir_all(&dir);
    seed_linear_repo(&dir, 22);
    let page1 = grit_json(&dir, &["log"]);
    let cursor = page1["next"].as_str().expect("next").to_owned();
    let out = grit_run(&dir, &["log", "HEAD~3", &format!("--cursor={cursor}")]);
    assert!(!out.status.success());
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("cannot combine `--cursor`"),
        "stderr: {stderr}"
    );
    Ok(())
}

#[test]
fn log_cursor_rejects_incompatible_flags() -> TestResult {
    let dir = std::env::temp_dir().join(format!(
        "grit-rev-select-cursor-flags-{}",
        std::process::id()
    ));
    let _ = fs::remove_dir_all(&dir);
    seed_linear_repo(&dir, 22);
    let page1 = grit_json(&dir, &["log"]);
    let cursor = page1["next"].as_str().expect("next").to_owned();
    for extra in [
        "--reverse",
        "--first-parent",
        "--merges",
        "--no-merges",
        "--skip=1",
    ] {
        let out = grit_run(&dir, &["log", &format!("--cursor={cursor}"), extra]);
        assert!(!out.status.success(), "expected failure with {extra}");
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert!(
            stderr.contains("cannot combine `--cursor`"),
            "stderr for {extra}: {stderr}"
        );
    }
    Ok(())
}

#[test]
fn log_cursor_continuation_hint_is_cursor_only_and_runs() -> TestResult {
    let dir = std::env::temp_dir().join(format!(
        "grit-rev-select-cursor-hint-{}",
        std::process::id()
    ));
    let _ = fs::remove_dir_all(&dir);
    seed_linear_repo(&dir, 3);
    assert!(grit_run(&dir, &["branch", "base"]).status.success());
    for i in 0..22 {
        fs::write(dir.join("f.txt"), format!("main{i}\n")).expect("write");
        assert!(grit_run(&dir, &["add", "f.txt"]).status.success());
        assert!(grit_run(&dir, &["commit", &format!("m{i}")])
            .status
            .success());
    }
    let out = grit_run(&dir, &["log", "base..HEAD", "--cursor"]);
    assert!(out.status.success());
    let stdout = String::from_utf8_lossy(&out.stdout);
    let prefix = "→ more: grit log --cursor=";
    let cursor_line = stdout
        .lines()
        .find(|l| l.starts_with(prefix))
        .unwrap_or_else(|| panic!("expected cursor-only hint in stdout: {stdout}"));
    assert!(
        !cursor_line.contains("base..HEAD"),
        "hint must not repeat revision args: {cursor_line}"
    );
    let token = cursor_line
        .strip_prefix(prefix)
        .expect("cursor token after prefix");
    assert!(token.starts_with("lw1.s."), "short cursor token: {token}");
    let page2 = grit_json(&dir, &["log", &format!("--cursor={token}")]);
    assert!(!page2["commits"].as_array().expect("commits").is_empty());
    Ok(())
}

#[test]
fn merges_conflicts_with_no_merges() -> TestResult {
    let dir = std::env::temp_dir().join(format!(
        "grit-rev-select-merge-conflict-{}",
        std::process::id()
    ));
    let _ = fs::remove_dir_all(&dir);
    seed_linear_repo(&dir, 2);
    let out = grit_run(&dir, &["revs", "HEAD", "--merges", "--no-merges"]);
    assert_eq!(out.status.code(), Some(2));
    Ok(())
}

fn log_walk_snapshot_count(git_dir: &Path) -> usize {
    let dir = git_dir.join("grit/log-walk");
    match fs::read_dir(&dir) {
        Ok(rd) => rd.filter_map(Result::ok).count(),
        Err(_) => 0,
    }
}

#[cfg(unix)]
fn chmod_readonly_recursive(path: &Path) {
    use std::os::unix::fs::PermissionsExt;
    if path.is_dir() {
        for entry in fs::read_dir(path).expect("read_dir") {
            chmod_readonly_recursive(&entry.expect("entry").path());
        }
    }
    let meta = fs::metadata(path).expect("metadata");
    let mut perms = meta.permissions();
    perms.set_mode(meta.permissions().mode() & !0o222);
    fs::set_permissions(path, perms).expect("chmod");
}

#[test]
fn log_piped_human_stores_one_cursor_per_page() -> TestResult {
    let dir = std::env::temp_dir().join(format!("grit-log-cursor-no-write-{}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    seed_linear_repo(&dir, 22);
    let git_dir = dir.join(".git");
    assert_eq!(log_walk_snapshot_count(&git_dir), 0);
    let out = grit_run(&dir, &["log"]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(
        log_walk_snapshot_count(&git_dir),
        1,
        "the next page's cursor is stored for the hint"
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("→ more: grit log --cursor=lw1.s."),
        "{stdout}"
    );
    let out2 = grit_run(&dir, &["log", "-n", "3"]);
    assert!(out2.status.success());
    assert_eq!(
        log_walk_snapshot_count(&git_dir),
        1,
        "a one-shot `-n` stores no cursor"
    );
    Ok(())
}

#[cfg(unix)]
#[test]
fn log_default_human_succeeds_with_read_only_git_dir() -> TestResult {
    let dir = std::env::temp_dir().join(format!("grit-log-readonly-git-{}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    seed_linear_repo(&dir, 22);
    chmod_readonly_recursive(&dir.join(".git"));
    let out = grit_run(&dir, &["log"]);
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    // The cursor cannot be stored, so the hint carries the long self-contained form.
    let stdout = String::from_utf8_lossy(&out.stdout);
    let token = stdout
        .lines()
        .find_map(|l| l.strip_prefix("→ more: grit log --cursor="))
        .expect("continuation with a cursor");
    assert!(!token.starts_with("lw1.s."), "long token: {token}");
    let next = grit_run(&dir, &["log", &format!("--cursor={token}")]);
    assert!(
        next.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&next.stderr)
    );
    Ok(())
}

#[test]
fn log_cursor_flag_persists_snapshot_for_hint() -> TestResult {
    let dir =
        std::env::temp_dir().join(format!("grit-log-cursor-hint-write-{}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    seed_linear_repo(&dir, 22);
    let git_dir = dir.join(".git");
    assert_eq!(log_walk_snapshot_count(&git_dir), 0);
    let out = grit_run(&dir, &["log", "--cursor"]);
    assert!(out.status.success());
    assert_eq!(
        log_walk_snapshot_count(&git_dir),
        1,
        "hint cursor should persist one snapshot"
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("→ more: grit log --cursor=lw1.s."));
    Ok(())
}

fn git_rev_list_oids(dir: &Path, extra: &[&str]) -> Vec<String> {
    let mut argv = vec!["-C", dir.to_str().expect("utf8"), "rev-list"];
    argv.extend(extra);
    let out = Command::new("git")
        .args(&argv)
        .env("GIT_CONFIG_GLOBAL", null_device())
        .env("GIT_CONFIG_SYSTEM", null_device())
        .output()
        .expect("git rev-list");
    assert!(out.status.success());
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter(|l| !l.is_empty())
        .map(str::to_owned)
        .collect()
}

fn grit_revs_oids(dir: &Path, args: &[&str]) -> Vec<String> {
    let v = grit_json(dir, args);
    v["commits"]
        .as_array()
        .expect("commits")
        .iter()
        .map(|c| c["oid"].as_str().expect("oid").to_owned())
        .collect()
}

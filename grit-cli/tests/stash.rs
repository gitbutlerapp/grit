use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

const GRIT: &str = env!("CARGO_BIN_EXE_grit");

fn git(dir: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .args(args)
        .current_dir(dir)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .env("GIT_AUTHOR_NAME", "Test User")
        .env("GIT_AUTHOR_EMAIL", "test@example.com")
        .env("GIT_COMMITTER_NAME", "Test User")
        .env("GIT_COMMITTER_EMAIL", "test@example.com")
        .output()
        .expect("git");
    assert!(
        out.status.success(),
        "git {:?} failed: {}",
        args,
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).into_owned()
}

fn grit(dir: &Path, args: &[&str]) -> (i32, String, String) {
    let out = Command::new(GRIT)
        .args(args)
        .current_dir(dir)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .env("GIT_AUTHOR_NAME", "Test User")
        .env("GIT_AUTHOR_EMAIL", "test@example.com")
        .env("GIT_COMMITTER_NAME", "Test User")
        .env("GIT_COMMITTER_EMAIL", "test@example.com")
        .output()
        .expect("grit");
    (
        out.status.code().unwrap_or(-1),
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

fn init_repo(dir: &Path) {
    git(dir, &["init", "-q", "-b", "main", "."]);
    git(dir, &["config", "user.email", "test@example.com"]);
    git(dir, &["config", "user.name", "Test User"]);
}

fn scratch(tag: &str) -> PathBuf {
    let mut path = std::env::temp_dir();
    path.push(format!("grit-stash-test-{}-{}", tag, std::process::id()));
    let _ = fs::remove_dir_all(&path);
    fs::create_dir_all(&path).expect("mkdir");
    path
}

#[test]
fn stash_push_pop_roundtrip() {
    let dir = scratch("roundtrip");
    init_repo(&dir);
    fs::write(dir.join("f.txt"), "base\n").expect("write");
    git(&dir, &["add", "f.txt"]);
    git(&dir, &["commit", "-qm", "init"]);

    fs::write(dir.join("f.txt"), "changed\n").expect("modify");
    let (code, stdout, stderr) = grit(&dir, &["stash", "push", "-m", "wip"]);
    assert_eq!(code, 0, "stderr={stderr}");
    assert!(
        stdout.contains("Saved working directory and index state On main: wip"),
        "stdout={stdout}"
    );
    assert!(
        !stdout.contains("On On"),
        "must not double the On prefix: {stdout}"
    );

    let porcelain = git(&dir, &["status", "--porcelain"]);
    assert!(porcelain.trim().is_empty(), "clean after push");

    let (code, _, stderr) = grit(&dir, &["stash", "pop"]);
    assert_eq!(code, 0, "pop failed: {stderr}");
    assert_eq!(
        fs::read_to_string(dir.join("f.txt")).expect("read"),
        "changed\n"
    );
    let list = git(&dir, &["stash", "list"]);
    assert!(list.trim().is_empty(), "stash should be empty after pop");
}

#[test]
fn stash_list_json() {
    let dir = scratch("list-json");
    init_repo(&dir);
    fs::write(dir.join("a"), "v\n").expect("write");
    git(&dir, &["add", "a"]);
    git(&dir, &["commit", "-qm", "init"]);
    fs::write(dir.join("a"), "one\n").expect("one");
    grit(&dir, &["stash", "-m", "first"]);
    fs::write(dir.join("a"), "two\n").expect("two");
    grit(&dir, &["stash", "-m", "second"]);

    let (code, stdout, _) = grit(&dir, &["stash", "list", "--json"]);
    assert_eq!(code, 0);
    let entries: Vec<serde_json::Value> = serde_json::from_str(stdout.trim()).expect("json array");
    assert_eq!(entries.len(), 2);
    assert_eq!(entries[0]["index"], 0);
    assert!(entries[0]["oid"].as_str().unwrap().len() >= 40);
    assert!(entries[0]["message"].as_str().unwrap().contains("second"));
    assert!(
        entries[0].get("stashed").is_none(),
        "list items must not include stashed"
    );
}

#[test]
fn stash_push_json_shapes() {
    let dir = scratch("push-json");
    init_repo(&dir);
    fs::write(dir.join("a"), "v\n").expect("write");
    git(&dir, &["add", "a"]);
    git(&dir, &["commit", "-qm", "init"]);

    let (code, stdout, _) = grit(&dir, &["stash", "--json"]);
    assert_eq!(code, 0);
    let nothing: serde_json::Value = serde_json::from_str(stdout.trim()).expect("json");
    assert_eq!(nothing, serde_json::json!({ "stashed": false }));
    assert!(nothing.get("oid").is_none());

    fs::write(dir.join("a"), "dirty\n").expect("dirty");
    let (code, stdout, _) = grit(&dir, &["stash", "-m", "wip", "--json"]);
    assert_eq!(code, 0);
    let saved: serde_json::Value = serde_json::from_str(stdout.trim()).expect("json");
    assert!(saved.get("stashed").is_none());
    assert!(saved["oid"].as_str().unwrap().len() >= 40);
    assert!(saved["message"].as_str().is_some());
}

#[test]
fn stash_pop_conflict_keeps_entry() {
    let dir = scratch("conflict");
    init_repo(&dir);
    fs::write(dir.join("f.txt"), "base\n").expect("write");
    git(&dir, &["add", "f.txt"]);
    git(&dir, &["commit", "-qm", "init"]);

    fs::write(dir.join("f.txt"), "from-stash\n").expect("stash me");
    grit(&dir, &["stash"]);
    fs::write(dir.join("f.txt"), "on-branch\n").expect("branch edit");
    git(&dir, &["add", "f.txt"]);
    git(&dir, &["commit", "-qm", "second"]);

    let (code, stdout, stderr) = grit(&dir, &["stash", "pop"]);
    assert_ne!(code, 0, "pop with conflicts must fail");
    let combined = format!("{stdout}{stderr}");
    assert!(
        combined.contains("conflict") || combined.contains("kept"),
        "output should mention conflict/kept: {combined}"
    );

    let list = git(&dir, &["stash", "list"]);
    assert!(
        !list.trim().is_empty(),
        "stash entry must remain after conflicted pop"
    );
}

#[test]
fn stash_pop_untracked_collision_keeps_stash_and_file() {
    let dir = scratch("untracked-collision");
    init_repo(&dir);
    fs::write(dir.join("a"), "a\n").expect("write");
    git(&dir, &["add", "a"]);
    git(&dir, &["commit", "-qm", "init"]);
    fs::write(dir.join("u.txt"), "stashed-untracked\n").expect("untracked");
    let (code, _, stderr) = grit(&dir, &["stash", "-u", "-m", "m"]);
    assert_eq!(code, 0, "stash push failed: {stderr}");
    fs::write(dir.join("u.txt"), "PRECIOUS new work\n").expect("replace");

    let (code, stdout, stderr) = grit(&dir, &["stash", "pop"]);
    assert_ne!(code, 0, "pop must fail when untracked path exists");
    let combined = format!("{stdout}{stderr}");
    assert!(
        combined.contains("stash untracked restore blocked") && combined.contains("u.txt"),
        "stderr should explain failure: {combined}"
    );
    assert_eq!(
        fs::read_to_string(dir.join("u.txt")).expect("read"),
        "PRECIOUS new work\n"
    );
    let list = git(&dir, &["stash", "list"]);
    assert!(
        !list.trim().is_empty(),
        "stash entry must remain after failed pop"
    );

    let (code, _, stderr) = grit(&dir, &["stash", "apply"]);
    assert_ne!(code, 0, "apply must fail: {stderr}");
    assert_eq!(
        fs::read_to_string(dir.join("u.txt")).expect("read"),
        "PRECIOUS new work\n"
    );
}

#[test]
fn stash_push_pathspec_json_and_keeps_other_changes() {
    let dir = scratch("pathspec-json");
    init_repo(&dir);
    fs::write(dir.join("a.txt"), "a\n").expect("write");
    fs::write(dir.join("b.txt"), "b\n").expect("write");
    git(&dir, &["add", "."]);
    git(&dir, &["commit", "-qm", "init"]);
    fs::write(dir.join("a.txt"), "A\n").expect("modify a");
    fs::write(dir.join("b.txt"), "B\n").expect("modify b");
    git(&dir, &["add", "b.txt"]);

    let (code, stdout, stderr) = grit(&dir, &["stash", "push", "--json", "--", "a.txt"]);
    assert_eq!(code, 0, "push failed: {stderr}");
    let v: serde_json::Value = serde_json::from_str(stdout.trim()).expect("json");
    assert_eq!(v["paths"], serde_json::json!(["a.txt"]));
    assert!(
        git(&dir, &["status", "--porcelain"]).contains("b.txt"),
        "b.txt should remain staged"
    );
}

#[test]
fn stash_push_pathspec_from_file_limits_paths() {
    let dir = scratch("pathspec-from-file");
    init_repo(&dir);
    fs::write(dir.join("a.txt"), "a\n").expect("write");
    fs::write(dir.join("b.txt"), "b\n").expect("write");
    git(&dir, &["add", "."]);
    git(&dir, &["commit", "-qm", "init"]);
    fs::write(dir.join("a.txt"), "A\n").expect("write");
    fs::write(dir.join("b.txt"), "B\n").expect("write");

    let list = dir.join("paths.txt");
    fs::write(&list, "a.txt\n").expect("write list");

    let (code, _, stderr) = grit(
        &dir,
        &[
            "stash",
            "push",
            "--pathspec-from-file",
            list.to_str().unwrap(),
        ],
    );
    assert_eq!(code, 0, "stash push from file failed: {stderr}");
    assert!(
        git(&dir, &["status", "--porcelain"]).contains("b.txt"),
        "b.txt should remain modified"
    );
    assert_eq!(
        fs::read_to_string(dir.join("a.txt")).expect("read a"),
        "a\n",
        "a.txt reset to HEAD"
    );
}

#[test]
fn stash_apply_index_json_reports_index_restored() {
    let dir = scratch("apply-index-json");
    init_repo(&dir);
    fs::write(dir.join("s.txt"), "base\n").expect("write");
    git(&dir, &["add", "s.txt"]);
    git(&dir, &["commit", "-qm", "init"]);
    fs::write(dir.join("s.txt"), "staged\n").expect("write");
    git(&dir, &["add", "s.txt"]);
    grit(&dir, &["stash", "push", "-m", "s"]);
    git(&dir, &["reset", "--hard", "HEAD"]);

    let (code, stdout, stderr) = grit(&dir, &["stash", "apply", "--index", "--json"]);
    assert_eq!(code, 0, "apply failed: {stderr}");
    let v: serde_json::Value = serde_json::from_str(stdout.trim()).expect("json");
    assert_eq!(v["index_restored"], true);
}

#[test]
fn stash_interop_with_git() {
    let dir = scratch("interop");
    init_repo(&dir);
    fs::write(dir.join("x"), "v1\n").expect("write");
    git(&dir, &["add", "x"]);
    git(&dir, &["commit", "-qm", "init"]);

    fs::write(dir.join("x"), "grit-stash\n").expect("modify");
    grit(&dir, &["stash", "-m", "from-grit"]);
    assert!(git(&dir, &["status", "--porcelain"]).trim().is_empty());

    git(&dir, &["stash", "pop"]);
    assert_eq!(
        fs::read_to_string(dir.join("x")).expect("read"),
        "grit-stash\n"
    );

    fs::write(dir.join("x"), "git-stash\n").expect("modify");
    git(&dir, &["stash", "push", "-m", "from-git"]);

    let (code, stdout, _) = grit(&dir, &["stash", "list", "--json"]);
    assert_eq!(code, 0);
    let entries: Vec<serde_json::Value> = serde_json::from_str(stdout.trim()).expect("json array");
    assert_eq!(entries.len(), 1);

    let (code, _, _) = grit(&dir, &["stash", "pop"]);
    assert_eq!(code, 0);
    assert_eq!(
        fs::read_to_string(dir.join("x")).expect("read"),
        "git-stash\n"
    );
}

#[test]
fn stash_show_untracked_json() {
    let dir = scratch("show-u-json");
    init_repo(&dir);
    fs::write(dir.join("t.txt"), "base\n").expect("write");
    git(&dir, &["add", "t.txt"]);
    git(&dir, &["commit", "-qm", "init"]);
    fs::write(dir.join("t.txt"), "dirty\n").expect("dirty");
    fs::write(dir.join("u.txt"), "new\n").expect("untracked");
    grit(&dir, &["stash", "-u"]);

    let (code, stdout, stderr) = grit(&dir, &["stash", "show", "-u", "--json"]);
    assert_eq!(code, 0, "show failed: {stderr}");
    let v: serde_json::Value = serde_json::from_str(stdout.trim()).expect("json");
    assert_eq!(v["include_untracked"], true);
    assert!(
        v["stat"]["files"]
            .as_array()
            .expect("files")
            .iter()
            .any(|f| f["path"] == "u.txt"),
        "untracked path must appear in stat: {v}"
    );
}

#[test]
fn stash_branch_json() {
    let dir = scratch("branch-json");
    init_repo(&dir);
    fs::write(dir.join("f.txt"), "base\n").expect("write");
    git(&dir, &["add", "f.txt"]);
    git(&dir, &["commit", "-qm", "init"]);
    fs::write(dir.join("f.txt"), "wip\n").expect("wip");
    grit(&dir, &["stash"]);
    let base = git(&dir, &["rev-parse", "HEAD"]).trim().to_owned();

    let (code, stdout, stderr) = grit(&dir, &["stash", "branch", "wip-branch", "--json"]);
    assert_eq!(code, 0, "branch failed: {stderr}");
    let v: serde_json::Value = serde_json::from_str(stdout.trim()).expect("json");
    assert_eq!(v["branch"], "wip-branch");
    assert_eq!(v["base"], base);
    assert_eq!(v["index"], 0);
    assert_eq!(v["conflicts"], false);
    assert!(git(&dir, &["stash", "list"]).trim().is_empty());
}

#[test]
fn stash_clear_without_force_exits_nonzero_and_json_dry_run() {
    let dir = scratch("clear-no-force");
    init_repo(&dir);
    fs::write(dir.join("a"), "v\n").expect("write");
    git(&dir, &["add", "a"]);
    git(&dir, &["commit", "-qm", "init"]);
    fs::write(dir.join("a"), "dirty\n").expect("dirty");
    grit(&dir, &["stash"]);

    let (code, stdout, stderr) = grit(&dir, &["stash", "clear", "--json"]);
    assert_ne!(code, 0, "clear without --force must fail: {stderr}");
    let v: serde_json::Value = serde_json::from_str(stdout.trim()).expect("json");
    assert_eq!(v["cleared"], 1);
    assert_eq!(v["dry_run"], true);
    assert!(!git(&dir, &["stash", "list"]).trim().is_empty());

    let (code, stdout, _) = grit(&dir, &["stash", "clear", "--force", "--dry-run", "--json"]);
    assert_eq!(code, 0);
    let v: serde_json::Value = serde_json::from_str(stdout.trim()).expect("json");
    assert_eq!(v["cleared"], 1);
    assert_eq!(v["dry_run"], true);
    assert!(!git(&dir, &["stash", "list"]).trim().is_empty());

    let (code, stdout, _) = grit(&dir, &["stash", "clear", "--force", "--json"]);
    assert_eq!(code, 0);
    let v: serde_json::Value = serde_json::from_str(stdout.trim()).expect("json");
    assert_eq!(v["cleared"], 1);
    assert_eq!(v["dry_run"], false);
    assert!(git(&dir, &["stash", "list"]).trim().is_empty());
}

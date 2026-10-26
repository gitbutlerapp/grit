//! `grit worktree lock` / `unlock` compatibility with system `git`.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU32, Ordering};

const GRIT: &str = env!("CARGO_BIN_EXE_grit");

#[derive(serde::Deserialize)]
struct WorktreeListJson {
    action: String,
    worktrees: Vec<WorktreeListRow>,
}

#[derive(serde::Deserialize)]
struct WorktreeListRow {
    path: String,
    locked: bool,
    lock_reason: Option<String>,
    prunable: bool,
    prunable_reason: Option<String>,
    is_current: bool,
}

fn unique_tmp(tag: &str) -> PathBuf {
    static COUNTER: AtomicU32 = AtomicU32::new(0);
    let n = COUNTER.fetch_add(1, Ordering::SeqCst);
    let mut p = std::env::temp_dir();
    p.push(format!(
        "grit-worktree-cli-{tag}-{}-{n}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&p);
    std::fs::create_dir_all(&p).expect("tempdir");
    p
}

fn git(dir: &Path, args: &[&str]) -> String {
    let output = Command::new("git")
        .env("GIT_AUTHOR_NAME", "Test")
        .env("GIT_AUTHOR_EMAIL", "test@example.com")
        .env("GIT_COMMITTER_NAME", "Test")
        .env("GIT_COMMITTER_EMAIL", "test@example.com")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .current_dir(dir)
        .args(args)
        .output()
        .expect("spawn git");
    assert!(
        output.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout).into_owned()
}

fn grit(dir: &Path, args: &[&str]) -> std::process::Output {
    Command::new(GRIT)
        .current_dir(dir)
        .args(args)
        .output()
        .expect("spawn grit")
}

#[test]
fn worktree_list_json_has_is_current_lock_reason_prunable() {
    let dir = unique_tmp("list-json");
    git(&dir, &["init", "-q", "-b", "main", "."]);
    std::fs::write(dir.join("f"), b"1\n").unwrap();
    git(&dir, &["add", "f"]);
    git(&dir, &["commit", "-qm", "base"]);

    let wt_lock = dir.join("linked-lock");
    git(
        &dir,
        &["worktree", "add", wt_lock.to_str().unwrap(), "-b", "side"],
    );
    git(
        &dir,
        &[
            "worktree",
            "lock",
            wt_lock.to_str().unwrap(),
            "--reason",
            "hold",
        ],
    );

    let wt_gone = dir.join("linked-gone");
    git(
        &dir,
        &["worktree", "add", wt_gone.to_str().unwrap(), "-b", "gone"],
    );
    std::fs::remove_dir_all(&wt_gone).unwrap();

    let out = grit(&wt_lock, &["worktree", "list", "--json"]);
    assert!(
        out.status.success(),
        "list json: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let parsed: WorktreeListJson =
        serde_json::from_slice(&out.stdout).expect("parse worktree list json");
    assert_eq!(parsed.action, "list");
    assert_eq!(parsed.worktrees.len(), 3);

    let current = parsed
        .worktrees
        .iter()
        .filter(|w| w.is_current)
        .collect::<Vec<_>>();
    assert_eq!(current.len(), 1);
    assert!(current[0].path.ends_with("linked-lock"));

    let locked = parsed
        .worktrees
        .iter()
        .find(|w| w.path.ends_with("linked-lock"))
        .expect("locked row");
    assert!(locked.locked);
    assert_eq!(locked.lock_reason.as_deref(), Some("hold"));

    let main_row = parsed
        .worktrees
        .iter()
        .find(|w| w.path.ends_with("list-json"))
        .or_else(|| parsed.worktrees.iter().find(|w| !w.locked && !w.prunable))
        .expect("main row");
    assert!(main_row.lock_reason.is_none());

    let gone = parsed
        .worktrees
        .iter()
        .find(|w| w.prunable)
        .expect("prunable row");
    assert_eq!(
        gone.prunable_reason.as_deref(),
        Some("gitdir file points to non-existent location")
    );
}

#[test]
fn worktree_lock_git_sees_locked() {
    let dir = unique_tmp("lock");
    git(&dir, &["init", "-q", "-b", "main", "."]);
    std::fs::write(dir.join("f"), b"1\n").unwrap();
    git(&dir, &["add", "f"]);
    git(&dir, &["commit", "-qm", "base"]);

    let wt = dir.join("linked");
    let out = grit(
        &dir,
        &["worktree", "add", wt.to_str().unwrap(), "-b", "side"],
    );
    assert!(
        out.status.success(),
        "add: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    let out = grit(
        &dir,
        &["worktree", "lock", wt.to_str().unwrap(), "--reason", "hold"],
    );
    assert!(
        out.status.success(),
        "lock: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let porcelain = git(&dir, &["worktree", "list", "--porcelain"]);
    assert!(
        porcelain.contains("locked") && porcelain.contains("hold"),
        "git must see grit lock:\n{porcelain}"
    );

    git(&dir, &["worktree", "unlock", wt.to_str().unwrap()]);
    git(
        &dir,
        &[
            "worktree",
            "lock",
            wt.to_str().unwrap(),
            "--reason",
            "from-git",
        ],
    );
    let porcelain = git(&dir, &["worktree", "list", "--porcelain"]);
    assert!(
        porcelain.contains("from-git"),
        "git lock must stick:\n{porcelain}"
    );
    let out = grit(&dir, &["worktree", "unlock", wt.to_str().unwrap()]);
    assert!(
        out.status.success(),
        "unlock: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let porcelain = git(&dir, &["worktree", "list", "--porcelain"]);
    assert!(
        !porcelain.contains("locked"),
        "git must see grit unlock:\n{porcelain}"
    );
}

fn write_system_autocrlf_config(dir: &Path) -> PathBuf {
    let path = dir.join("system.gitconfig");
    std::fs::write(&path, "[core]\n\tautocrlf = true\n").expect("system config");
    path
}

fn git_with_system(dir: &Path, system_cfg: &Path, args: &[&str]) -> String {
    let output = Command::new("git")
        .env("GIT_AUTHOR_NAME", "Test")
        .env("GIT_AUTHOR_EMAIL", "test@example.com")
        .env("GIT_COMMITTER_NAME", "Test")
        .env("GIT_COMMITTER_EMAIL", "test@example.com")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", system_cfg)
        .current_dir(dir)
        .args(args)
        .output()
        .expect("spawn git");
    assert!(
        output.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout).into_owned()
}

fn grit_with_system(dir: &Path, system_cfg: &Path, args: &[&str]) -> std::process::Output {
    Command::new(GRIT)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", system_cfg)
        .current_dir(dir)
        .args(args)
        .output()
        .expect("spawn grit")
}

/// Issue #1232: remove must honor inherited system `core.autocrlf` when checking cleanliness.
#[test]
fn worktree_remove_clean_crlf_with_system_autocrlf() {
    let cfg_dir = unique_tmp("sys-cfg");
    let system_cfg = write_system_autocrlf_config(&cfg_dir);

    let dir = unique_tmp("remove-autocrlf");
    git_with_system(&dir, &system_cfg, &["init", "-q", "-b", "main", "."]);
    git_with_system(&dir, &system_cfg, &["config", "user.email", "t@e.t"]);
    git_with_system(&dir, &system_cfg, &["config", "user.name", "T"]);
    std::fs::write(dir.join("a.txt"), b"a\n").unwrap();
    git_with_system(&dir, &system_cfg, &["add", "a.txt"]);
    git_with_system(&dir, &system_cfg, &["commit", "-qm", "one"]);

    let wt = dir.join("w");
    let out = grit_with_system(
        &dir,
        &system_cfg,
        &["worktree", "add", wt.to_str().unwrap(), "-b", "f"],
    );
    assert!(
        out.status.success(),
        "worktree add: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    std::fs::write(wt.join("new.txt"), b"new\r\n").unwrap();
    let out = grit_with_system(&wt, &system_cfg, &["commit", "-am", "two"]);
    assert!(
        out.status.success(),
        "commit in linked wt: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    let porcelain = git_with_system(&wt, &system_cfg, &["status", "--porcelain"]);
    assert!(
        porcelain.trim().is_empty(),
        "git status must be clean: {porcelain}"
    );

    let out = grit_with_system(
        &dir,
        &system_cfg,
        &["worktree", "remove", wt.to_str().unwrap()],
    );
    assert!(
        out.status.success(),
        "worktree remove must succeed on clean CRLF tree: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(!wt.exists(), "linked worktree directory should be gone");
}

#[derive(serde::Deserialize)]
struct WorktreeMoveJson {
    action: String,
    from: String,
    to: String,
    dry_run: bool,
}

#[derive(serde::Deserialize)]
struct WorktreeRepairJson {
    action: String,
    dry_run: bool,
    repairs: Vec<WorktreeRepairRowJson>,
}

#[derive(serde::Deserialize)]
struct WorktreeRepairRowJson {
    path: String,
    kind: String,
}

#[test]
fn worktree_move_and_repair_json_schema() {
    let dir = unique_tmp("move-repair-json");
    git(&dir, &["init", "-q", "-b", "main", "."]);
    std::fs::write(dir.join("f"), b"1\n").unwrap();
    git(&dir, &["add", "f"]);
    git(&dir, &["commit", "-qm", "base"]);

    let wt = dir.join("linked");
    git(
        &dir,
        &["worktree", "add", wt.to_str().unwrap(), "-b", "side"],
    );
    let dest = dir.join("moved");
    let out = grit(
        &dir,
        &[
            "worktree",
            "move",
            wt.to_str().unwrap(),
            dest.to_str().unwrap(),
            "--json",
        ],
    );
    assert!(
        out.status.success(),
        "move json: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let body: WorktreeMoveJson = serde_json::from_slice(&out.stdout).expect("move json parse");
    assert_eq!(body.action, "move");
    assert!(!body.dry_run);
    assert!(body.to.contains("moved"));

    std::fs::rename(&dest, dir.join("renamed")).unwrap();
    let out = grit(
        &dir,
        &[
            "worktree",
            "repair",
            dir.join("renamed").to_str().unwrap(),
            "--json",
        ],
    );
    assert!(
        out.status.success(),
        "repair json: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let body: WorktreeRepairJson = serde_json::from_slice(&out.stdout).expect("repair json parse");
    assert_eq!(body.action, "repair");
    assert!(!body.repairs.is_empty());
    assert!(body.repairs[0].kind.contains("git"));
}

#[test]
fn reftable_add_and_switch_json_match_git_visible_head() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let dir = tmp.path().join("repo");
    git(
        tmp.path(),
        &["init", "-q", "-b", "main", "--ref-format=reftable", "repo"],
    );
    std::fs::write(dir.join("a"), "base\n").unwrap();
    git(&dir, &["add", "a"]);
    git(&dir, &["commit", "-qm", "base"]);
    let base = git(&dir, &["rev-parse", "HEAD"]).trim().to_owned();
    git(&dir, &["branch", "topic"]);
    let wt = tmp.path().join("grit-wt");
    let out = grit(
        &dir,
        &["worktree", "add", wt.to_str().unwrap(), "topic", "--json"],
    );
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let added: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(added["checked_out"], true);
    assert_eq!(added["branch"], "topic");
    assert_eq!(added["head"], base);
    assert_eq!(git(&wt, &["rev-parse", "HEAD"]).trim(), base);
    assert_eq!(
        git(&wt, &["symbolic-ref", "HEAD"]).trim(),
        "refs/heads/topic"
    );
    assert_eq!(git(&wt, &["status", "--short"]), "");

    let git_wt = tmp.path().join("git-wt");
    git(
        &dir,
        &[
            "worktree",
            "add",
            "-b",
            "git-topic",
            git_wt.to_str().unwrap(),
        ],
    );
    std::fs::write(git_wt.join("a"), "topic edit\n").unwrap();
    git(&git_wt, &["commit", "-qam", "topic-edit"]);
    let topic = git(&git_wt, &["rev-parse", "HEAD"]).trim().to_owned();
    git(&dir, &["branch", "switch-target", "main"]);
    let out = grit(&git_wt, &["switch", "switch-target", "--json"]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let switched: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(switched["branch"], "switch-target");
    assert_eq!(
        git(&git_wt, &["symbolic-ref", "HEAD"]).trim(),
        "refs/heads/switch-target"
    );
    assert_eq!(git(&git_wt, &["rev-parse", "HEAD"]).trim(), base);
    assert_eq!(git(&git_wt, &["status", "--short"]), "");
    let out = grit(&git_wt, &["switch", "--detach", &topic, "--json"]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let detached: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(detached["commit"], topic);
    assert!(detached["branch"].is_null());
    assert_eq!(git(&git_wt, &["rev-parse", "HEAD"]).trim(), topic);
    assert_eq!(git(&git_wt, &["status", "--short"]), "");
    let out = grit(&git_wt, &["status", "--json"]);
    assert!(out.status.success());
    let status: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(status["head"], topic);
    assert_eq!(status["clean"], true);
    assert_eq!(
        git(&dir, &["symbolic-ref", "HEAD"]).trim(),
        "refs/heads/main"
    );
    assert_eq!(
        git(&wt, &["symbolic-ref", "HEAD"]).trim(),
        "refs/heads/topic"
    );
    git(&dir, &["fsck", "--strict"]);
}

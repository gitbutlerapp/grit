//! Runnable fsmonitor valid-bit regressions (issue #2378); not gated on `full-tests`.

#![cfg(unix)]

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;

use filetime::{set_file_mtime, FileTime};
use grit_lib::index::{Index, MODE_REGULAR};
use grit_lib::porcelain::add::{stage, StageOptions};
use grit_lib::progress::{NeverCancel, NullProgress};
use grit_lib::repo::{InitOptions, Repository};
use grit_lib::RefStorageFormat;
use grit_lib::PARALLEL_STAT_MIN_ENTRIES;

fn grit_bin() -> PathBuf {
    std::env::var("CARGO_BIN_EXE_grit")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../target/debug/grit"))
}

fn grit_in(root: &Path, args: &[&str]) {
    let out = Command::new(grit_bin())
        .current_dir(root)
        .args(args)
        .output()
        .expect("grit");
    assert!(
        out.status.success(),
        "grit {args:?} failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

fn git_config(repo_root: &Path, args: &[&str]) {
    let out = Command::new("git")
        .current_dir(repo_root)
        .args(args)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .output()
        .expect("git config");
    assert!(out.status.success());
}

fn write_stable_fsmonitor_hook(dir: &Path) -> PathBuf {
    let hook = dir.join("fsmonitor-stable-hook.sh");
    fs::write(
        &hook,
        "#!/bin/sh\nif [ \"$1\" = \"2\" ]; then printf 'tok\\0'; fi\n",
    )
    .expect("write hook");
    let mut perms = fs::metadata(&hook).expect("meta").permissions();
    perms.set_mode(0o755);
    fs::set_permissions(&hook, perms).expect("chmod");
    hook
}

fn write_report_once_hook(dir: &Path, report_path: &str) -> PathBuf {
    let hook = dir.join("fsmonitor-report-once-hook.sh");
    let state = dir.join("fsmonitor-report-once.stamp");
    let _ = fs::remove_file(&state);
    let script = format!(
        "#!/bin/sh\nSTATE={state}\nif [ \"$1\" = \"2\" ]; then\n  \
         if [ \"$2\" = \"tok\" ] && [ ! -f \"$STATE\" ]; then\n    touch \"$STATE\"\n    \
         printf 'tok\\0{report_path}\\0'\n    exit 0\n  fi\n  printf 'tok\\0'\nfi\n",
        state = state.display(),
        report_path = report_path
    );
    fs::write(&hook, script).expect("write hook");
    let mut perms = fs::metadata(&hook).expect("meta").permissions();
    perms.set_mode(0o755);
    fs::set_permissions(&hook, perms).expect("chmod");
    hook
}

fn seed_tracked_repo(root: &Path, files: usize) {
    Repository::init(
        root,
        &InitOptions::new()
            .with_initial_branch("main".to_owned())
            .with_ref_storage(RefStorageFormat::Files),
    )
    .expect("init");
    fs::create_dir_all(root.join("mix")).expect("mkdir");
    for i in 0..files {
        let rel = format!("mix/{i:05}.txt");
        fs::write(root.join(&rel), format!("payload {i}\n")).expect("write");
    }
    let repo = Repository::open(
        &root.join(".git"),
        &grit_lib::config::environment::RepositoryOptions::from_process().with_work_tree(root),
    )
    .expect("open");
    stage(
        &repo,
        &StageOptions::default(),
        &mut NullProgress,
        &NeverCancel,
    )
    .expect("stage");
    grit_in(root, &["commit", "-qm", "initial"]);
}

#[test]
fn parallel_fsmonitor_valid_after_clean_status() {
    const FILES: usize = PARALLEL_STAT_MIN_ENTRIES + 24;
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();
    seed_tracked_repo(root, FILES);
    let hook = write_stable_fsmonitor_hook(root);
    git_config(
        root,
        &["config", "core.fsmonitor", hook.to_str().expect("utf8")],
    );
    git_config(root, &["config", "index.threads", "4"]);
    for _ in 0..4 {
        grit_in(root, &["status"]);
    }
    let index = Index::load(&root.join(".git/index")).expect("index");
    assert_eq!(
        index.fsmonitor_last_update.as_deref(),
        Some("tok"),
        "hook token must be stored"
    );
    let stage0: Vec<_> = index
        .entries()
        .iter()
        .filter(|e| e.stage() == 0 && e.mode == MODE_REGULAR)
        .collect();
    assert_eq!(stage0.len(), FILES);
    let invalid = stage0.iter().filter(|e| !e.fsmonitor_valid()).count();
    assert_eq!(
        invalid, 0,
        "parallel clean scan must mark every stage-0 blob fsmonitor-valid \
         (invalid={invalid}, token={:?})",
        index.fsmonitor_last_update
    );
}

#[test]
fn parallel_fsmonitor_mode_change_stays_modified() {
    const FILES: usize = PARALLEL_STAT_MIN_ENTRIES + 24;
    const TARGET: &str = "mix/00002.txt";
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();
    seed_tracked_repo(root, FILES);
    git_config(
        root,
        &[
            "config",
            "core.fsmonitor",
            write_stable_fsmonitor_hook(root).to_str().expect("utf8"),
        ],
    );
    git_config(root, &["config", "index.threads", "4"]);
    for _ in 0..2 {
        grit_in(root, &["status"]);
    }

    let tracked = root.join(TARGET);
    let mut perms = fs::metadata(&tracked).expect("meta").permissions();
    perms.set_mode(0o755);
    fs::set_permissions(&tracked, perms).expect("chmod");
    set_file_mtime(&tracked, FileTime::now()).expect("touch");
    git_config(
        root,
        &[
            "config",
            "core.fsmonitor",
            write_report_once_hook(root, TARGET).to_str().expect("utf8"),
        ],
    );

    let first_json = Command::new(grit_bin())
        .current_dir(root)
        .args(["status", "--json"])
        .output()
        .expect("status json");
    assert!(first_json.status.success());
    let index_after_first = Index::load(&root.join(".git/index")).expect("index");
    let first: serde_json::Value = serde_json::from_slice(&first_json.stdout).expect("json");
    let unstaged = first["unstaged"].as_array().expect("unstaged array");
    assert!(
        unstaged.iter().any(|e| e["path"].as_str() == Some(TARGET)),
        "mode change must stay modified: {unstaged:?}"
    );
    assert!(
        !index_after_first
            .get(TARGET.as_bytes(), 0)
            .expect("target")
            .fsmonitor_valid(),
        "mode-changed path must not be fsmonitor-valid"
    );
    assert!(
        index_after_first
            .get(b"mix/00003.txt", 0)
            .expect("sibling")
            .fsmonitor_valid(),
        "untouched sibling stays fsmonitor-valid"
    );

    let second_json = Command::new(grit_bin())
        .current_dir(root)
        .args(["status", "--json"])
        .output()
        .expect("status json");
    assert!(second_json.status.success());
    let second: serde_json::Value = serde_json::from_slice(&second_json.stdout).expect("json");
    let unstaged2 = second["unstaged"].as_array().expect("unstaged");
    assert!(
        unstaged2.iter().any(|e| e["path"].as_str() == Some(TARGET)),
        "second status must still report mode change: {unstaged2:?}"
    );
}

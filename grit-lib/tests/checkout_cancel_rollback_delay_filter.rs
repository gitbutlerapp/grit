//! Checkout rollback must finish delayed process-filter smudges (#2330 review).

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use grit_lib::config::environment::{Environment, RepositoryOptions};
use grit_lib::error::Error;
use grit_lib::objects::ObjectId;
use grit_lib::porcelain::checkout::checkout_between_trees_with_progress;
use grit_lib::progress::{
    Cancel, FlagCancel, NeverCancel, NullProgress, ProgressPhase, ProgressSink,
};
use grit_lib::repo::Repository;

fn have_python3() -> bool {
    Command::new("python3")
        .arg("-c")
        .arg("pass")
        .status()
        .is_ok_and(|s| s.success())
}

fn git(root: &Path, args: &[&str]) {
    git_env(root, None, args);
}

fn git_env(root: &Path, filter_state: Option<&Path>, args: &[&str]) {
    let mut cmd = Command::new("git");
    cmd.args(args)
        .current_dir(root)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null");
    if let Some(state) = filter_state {
        cmd.env("GRIT_FILTER_STATE_FILE", state);
    }
    let out = cmd.output().expect("spawn git");
    assert!(
        out.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

fn filter_options(wt: &Path, state_file: &Path) -> RepositoryOptions {
    let vars = std::env::vars_os().chain([(
        "GRIT_FILTER_STATE_FILE".into(),
        state_file.as_os_str().to_owned(),
    )]);
    RepositoryOptions::with_environment(Environment::from_vars(vars, wt.to_path_buf()))
        .with_work_tree(wt)
}

fn open(wt: &Path, state_file: &Path) -> Repository {
    Repository::discover(Some(wt), &filter_options(wt, state_file)).expect("open repo")
}

fn tree_of(root: &Path, rev: &str) -> ObjectId {
    let hex = String::from_utf8(
        Command::new("git")
            .args(["rev-parse", &format!("{rev}^{{tree}}")])
            .current_dir(root)
            .output()
            .expect("rev-parse")
            .stdout,
    )
    .expect("utf8");
    ObjectId::from_hex(hex.trim()).expect("tree oid")
}

struct CancelAfter {
    cancel: FlagCancel,
    limit: u64,
    seen: u64,
    in_checkout: bool,
}

impl ProgressSink for CancelAfter {
    fn start(&mut self, phase: ProgressPhase, _total: Option<u64>) {
        self.in_checkout = phase == ProgressPhase::CheckingOutFiles;
    }

    fn inc(&mut self, units: u64) {
        if !self.in_checkout || self.cancel.is_cancelled() {
            return;
        }
        self.seen += units;
        if self.seen >= self.limit {
            self.cancel.cancel();
        }
    }
}

#[test]
fn cancelled_checkout_restores_process_filtered_file() {
    if !have_python3() {
        return;
    }
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path();
    let state_file = tempfile::Builder::new()
        .prefix("grit-filter-state-")
        .tempfile()
        .expect("state temp")
        .into_temp_path();
    fs::write(&state_file, "").expect("state");
    // Git and grit filter drivers inherit this for the fixture script.
    std::env::set_var("GRIT_FILTER_STATE_FILE", &state_file);

    git(root, &["init", "-q", "-b", "main"]);
    git(root, &["config", "user.email", "t@example.com"]);
    git(root, &["config", "user.name", "Test"]);
    let script = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/filter_process_delay_test_server.py");
    let process_cmd = format!("python3 -u {}", script.display());
    git(root, &["config", "filter.test.process", &process_cmd]);
    fs::write(root.join(".gitattributes"), "keep/f00.txt filter=test\n").expect("attributes");

    // Stay below bulk-checkout thresholds so the filtered path uses the general apply
    // (process-filter delay) rather than the bulk fast path.
    for i in 0..10 {
        let path = root.join(format!("keep/f{i:02}.txt"));
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).expect("mkdir keep");
        }
        fs::write(path, format!("keep {i}\n")).expect("write main");
    }
    git_env(root, Some(&state_file), &["add", "-A"]);
    git_env(root, Some(&state_file), &["commit", "-qm", "main"]);
    // .gitattributes must be in the tree so grit reads filter rules from the index.
    let expected_f00 = fs::read_to_string(root.join("keep/f00.txt")).expect("read f00 main");

    git(root, &["switch", "-qc", "next"]);
    for i in 0..10 {
        fs::write(
            root.join(format!("keep/f{i:02}.txt")),
            format!("keep {i} changed\n"),
        )
        .expect("write next");
    }
    git_env(root, Some(&state_file), &["add", "-A"]);
    git_env(root, Some(&state_file), &["commit", "-qm", "next"]);
    git(root, &["switch", "-q", "main"]);

    let from = tree_of(root, "main");
    let to = tree_of(root, "next");
    let cancel = FlagCancel::new();
    let mut progress = CancelAfter {
        cancel: cancel.clone(),
        limit: 4,
        seen: 0,
        in_checkout: false,
    };
    let err = checkout_between_trees_with_progress(
        &open(root, &state_file),
        Some(&from),
        &to,
        &mut progress,
        &cancel,
    )
    .expect_err("cancelled checkout");
    assert!(
        matches!(
            err,
            Error::Cancelled | Error::CheckoutRollbackIncomplete { .. }
        ),
        "unexpected error: {err:?}"
    );
    let restored = fs::read_to_string(root.join("keep/f00.txt")).expect("read f00 after cancel");
    assert_eq!(
        restored, expected_f00,
        "process-filtered path must be rolled back to pre-checkout bytes"
    );
}

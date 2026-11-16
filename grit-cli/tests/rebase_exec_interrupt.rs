//! Interrupting `grit rebase --exec` leaves a recoverable exec stop (issue #2235).

#![cfg(unix)]

use std::error::Error;
use std::fs;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use grit_test_support::{configure_git_env, grit_cmd};

type InterruptTestResult = Result<(), Box<dyn Error>>;

struct Scratch {
    path: PathBuf,
}

impl Scratch {
    fn new(tag: &str) -> Result<Self, Box<dyn Error>> {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let n = NEXT.fetch_add(1, Ordering::SeqCst);
        let mut path = std::env::temp_dir();
        path.push(format!(
            "grit-cli-rebase-exec-interrupt-{tag}-{}-{n}",
            std::process::id()
        ));
        if path.exists() {
            fs::remove_dir_all(&path)?;
        }
        fs::create_dir_all(&path)?;
        Ok(Self { path })
    }

    fn path(&self) -> &std::path::Path {
        &self.path
    }
}

const GRIT: &str = env!("CARGO_BIN_EXE_grit");

fn grit_exec_path(dir: &std::path::Path) -> std::path::PathBuf {
    dir.join(".git/rebase-merge/grit-exec")
}

/// Marker the exec command itself creates, so tests signal grit only once the command is
/// running. `grit-exec` alone is written before grit checks out the commit, so a signal sent as
/// soon as it appears can land while grit still holds `index.lock`.
fn exec_started_path(dir: &std::path::Path) -> std::path::PathBuf {
    dir.join(".git/exec-started")
}

fn wait_for_grit_exec(dir: &std::path::Path, child: &mut Child) -> bool {
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        if grit_exec_path(dir).is_file() && exec_started_path(dir).is_file() {
            return true;
        }
        if child.try_wait().expect("poll").is_some() {
            return grit_exec_path(dir).is_file() && exec_started_path(dir).is_file();
        }
        if Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

fn signal(child: &Child, sig: libc::c_int) {
    let pid = libc::pid_t::try_from(child.id()).expect("pid");
    // SAFETY: `pid` names our own child process.
    unsafe {
        libc::kill(pid, sig);
    }
}

fn init_exec_fixture(dir: &std::path::Path) -> String {
    grit_cmd(&["init"]).in_dir(dir).suc();
    fs::write(dir.join("f"), "0\n").expect("write");
    grit_cmd(&["commit", "-m", "base"]).in_dir(dir).suc();
    grit_cmd(&["switch", "-c", "topic"]).in_dir(dir).suc();
    for i in 1..=3 {
        fs::write(dir.join(format!("t{i}")), format!("{i}\n")).expect("write");
        grit_cmd(&["commit", "-m", &format!("t{i}")])
            .in_dir(dir)
            .suc();
    }
    grit_cmd(&["switch", "main"]).in_dir(dir).suc();
    fs::write(dir.join("m"), "m\n").expect("write");
    grit_cmd(&["commit", "-m", "main2"]).in_dir(dir).suc();
    grit_cmd(&["switch", "topic"]).in_dir(dir).suc();
    String::from_utf8(
        Command::new("git")
            .args(["rev-parse", "topic"])
            .current_dir(dir)
            .output()
            .expect("git")
            .stdout,
    )
    .expect("utf-8")
    .trim()
    .to_owned()
}

fn spawn_slow_rebase_exec(dir: &std::path::Path) -> Child {
    let mut cmd = Command::new(GRIT);
    configure_git_env(&mut cmd);
    cmd.args([
        "rebase",
        "main",
        "--exec",
        "touch .git/exec-started && sleep 300",
    ])
    .current_dir(dir)
    .stdout(Stdio::null())
    .stderr(Stdio::null());
    cmd.spawn().expect("spawn rebase --exec")
}

#[test]
fn sigint_during_rebase_exec_leaves_abortable_stop() -> InterruptTestResult {
    let scratch = Scratch::new("rebase-exec-sigint")?;
    let dir = scratch.path();
    let topic_tip = init_exec_fixture(dir);

    let mut child = spawn_slow_rebase_exec(dir);
    assert!(
        wait_for_grit_exec(dir, &mut child),
        "grit-exec should appear before the exec command finishes"
    );
    signal(&child, libc::SIGINT);
    let status = child.wait().expect("wait");
    assert!(
        !status.success(),
        "interrupted rebase should not succeed: {status:?}"
    );

    let status_out = grit_cmd(&["status"]).in_dir(dir).exec();
    let status_text = format!("{}{}", status_out.stdout, status_out.stderr);
    assert!(
        status_text.contains("Rebasing") || status_text.contains("rebase"),
        "status should mention the interrupted rebase: {status_text}"
    );
    assert!(grit_exec_path(dir).is_file());

    grit_cmd(&["rebase", "--abort"]).in_dir(dir).suc();
    assert!(!dir.join(".git/rebase-merge").exists());

    let head = String::from_utf8(
        Command::new("git")
            .args(["rev-parse", "HEAD"])
            .current_dir(dir)
            .output()
            .expect("git")
            .stdout,
    )
    .expect("utf-8")
    .trim()
    .to_owned();
    assert_eq!(head, topic_tip);
    grit_cmd(&["status"]).in_dir(dir).suc();
    Ok(())
}

#[test]
fn sigkill_during_rebase_exec_leaves_abortable_stop() -> InterruptTestResult {
    let scratch = Scratch::new("rebase-exec-sigkill")?;
    let dir = scratch.path();
    let topic_tip = init_exec_fixture(dir);

    let mut child = spawn_slow_rebase_exec(dir);
    assert!(wait_for_grit_exec(dir, &mut child));
    signal(&child, libc::SIGKILL);
    let status = child.wait().expect("wait");
    assert!(!status.success());

    assert!(grit_exec_path(dir).is_file());
    grit_cmd(&["rebase", "--abort"]).in_dir(dir).suc();
    assert!(!dir.join(".git/rebase-merge").exists());

    let head = String::from_utf8(
        Command::new("git")
            .args(["rev-parse", "HEAD"])
            .current_dir(dir)
            .output()
            .expect("git")
            .stdout,
    )
    .expect("utf-8")
    .trim()
    .to_owned();
    assert_eq!(head, topic_tip);
    Ok(())
}

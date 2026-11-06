//! Receive-pack hook cancellation on Windows (issue #2526).

#![cfg(windows)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
use std::process::Command;
use std::thread;
use std::time::Duration;

use grit_lib::command_runner::hooks::{run_hook_in_git_dir_with_cancel, HookError};
use grit_lib::command_runner::system_command_runner;
use grit_lib::config::environment::{Environment, RepositoryOptions};
use grit_lib::progress::FlagCancel;
use grit_lib::repo::Repository;

fn write_sh_hook(path: &std::path::Path, body: &str) {
    std::fs::write(path, body).expect("hook body");
}

#[test]
fn cancelled_pre_receive_hook_terminates_sleeping_child() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let bare = tmp.path().join("bare");
    std::fs::create_dir_all(&bare).expect("mkdir");
    Command::new("git")
        .current_dir(&bare)
        .args(["init", "-q", "--bare", "."])
        .status()
        .expect("git init");

    let child_pid_file = tmp.path().join("hook-child.pid");
    write_sh_hook(
        &bare.join("hooks/pre-receive"),
        &format!(
            "#!/bin/sh\ncat >/dev/null\nsleep 120 &\necho $! >\"{}\"\nwait\n",
            child_pid_file.display()
        ),
    );

    let mut env = Environment::empty();
    env.cwd = bare.clone();
    let repo = Repository::open(
        &bare,
        &RepositoryOptions::with_environment(env).with_command_runner(system_command_runner()),
    )
    .expect("open");

    let cancel = FlagCancel::new();
    let cancel_worker = cancel.clone();
    let kick = thread::spawn(move || {
        thread::sleep(Duration::from_millis(400));
        cancel_worker.cancel();
    });

    let stdin = b"0000000000000000000000000000000000000000 0000000000000000000000000000000000000000 refs/heads/x\n";
    let err = run_hook_in_git_dir_with_cancel(&repo, "pre-receive", &[], Some(stdin), &[], &cancel)
        .expect_err("cancelled");
    kick.join().expect("join");
    assert!(matches!(err, HookError::Cancelled));

    thread::sleep(Duration::from_millis(500));
    assert!(
        child_pid_file.is_file(),
        "pre-receive hook should record its sleeping child pid"
    );
    let child_pid: u32 = std::fs::read_to_string(&child_pid_file)
        .expect("child pid file")
        .trim()
        .parse()
        .expect("child pid");
    let still_running = Command::new("tasklist")
        .args(["/FI", &format!("PID eq {child_pid}")])
        .output()
        .expect("tasklist");
    let listing = String::from_utf8_lossy(&still_running.stdout);
    assert!(
        !listing.contains(&child_pid.to_string()),
        "hook child {child_pid} should not keep running after cancel: {listing}"
    );
}

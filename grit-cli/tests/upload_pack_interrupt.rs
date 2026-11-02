//! `grit upload-pack` must not install the cooperative cancel handler: the first
//! SIGTERM/SIGHUP should terminate the server while it waits on stdin (issue #2492 review).

#![cfg(unix)]

use std::process::{Command, Stdio};
use std::time::Duration;

use grit_test_support::{configure_git_env, grit_cmd};

const GRIT: &str = env!("CARGO_BIN_EXE_grit");

#[test]
fn sigterm_during_upload_pack_exits_on_first_signal() {
    let base = tempfile::tempdir().expect("tempdir");
    let bare = base.path().join("remote.git");
    grit_cmd(&["init", "--bare", bare.to_str().unwrap()])
        .in_dir(base.path())
        .suc();

    let mut cmd = Command::new(GRIT);
    configure_git_env(&mut cmd);
    cmd.env("HOME", base.path())
        .args(["upload-pack", bare.to_str().unwrap()])
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    let mut child = cmd.spawn().expect("spawn upload-pack");
    std::thread::sleep(Duration::from_millis(50));

    let pid = libc::pid_t::try_from(child.id()).expect("pid");
    unsafe {
        libc::kill(pid, libc::SIGTERM);
    }
    std::thread::sleep(Duration::from_millis(500));
    assert!(
        child.try_wait().expect("wait").is_some(),
        "upload-pack must exit on the first SIGTERM, not absorb it"
    );
}

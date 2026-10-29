//! Shared helpers for command interrupt integration tests.

#![cfg(any(unix, windows))]

use std::fs;
use std::path::Path;
use std::process::{Child, Command, Stdio};

pub const GRIT: &str = env!("CARGO_BIN_EXE_grit");

pub fn git_dir_locks(repo: &Path) -> Vec<String> {
    let git = repo.join(".git");
    fs::read_dir(&git)
        .expect("read .git")
        .flatten()
        .filter_map(|e| {
            let name = e.file_name().to_string_lossy().into_owned();
            name.ends_with(".lock").then_some(name)
        })
        .collect()
}

#[cfg(windows)]
pub fn own_process_group(cmd: &mut Command) {
    use std::os::windows::process::CommandExt as _;
    cmd.creation_flags(windows_sys::Win32::System::Threading::CREATE_NEW_PROCESS_GROUP);
}

#[cfg(unix)]
pub fn own_process_group(_cmd: &mut Command) {}

#[cfg(unix)]
pub fn signal(child: &Child, sig: libc::c_int) {
    let pid = libc::pid_t::try_from(child.id()).expect("pid");
    // SAFETY: `pid` is our own child, not yet reaped.
    unsafe {
        libc::kill(pid, sig);
    }
}

#[cfg(unix)]
pub fn sigint(child: &Child) {
    signal(child, libc::SIGINT);
}

#[cfg(unix)]
pub fn sigterm(child: &Child) {
    signal(child, libc::SIGTERM);
}

#[cfg(unix)]
pub fn sighup(child: &Child) {
    signal(child, libc::SIGHUP);
}

#[cfg(windows)]
pub fn sigint(child: &Child) {
    use windows_sys::Win32::System::Console::{GenerateConsoleCtrlEvent, CTRL_BREAK_EVENT};
    let sent = unsafe { GenerateConsoleCtrlEvent(CTRL_BREAK_EVENT, child.id()) };
    assert_ne!(sent, 0, "GenerateConsoleCtrlEvent failed");
}

pub fn spawn_grit(repo: &Path, args: &[&str]) -> Child {
    use grit_test_support::configure_git_env;
    let mut cmd = Command::new(GRIT);
    configure_git_env(&mut cmd);
    cmd.args(args)
        .current_dir(repo)
        .stdout(Stdio::null())
        .stderr(Stdio::piped());
    own_process_group(&mut cmd);
    cmd.spawn().expect("spawn grit")
}

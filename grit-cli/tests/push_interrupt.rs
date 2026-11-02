//! Interrupt signals during `grit push` remove in-progress receive quarantine dirs (issue #2492).

#![cfg(unix)]

use std::fs;
use std::io::Write;
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use grit_test_support::{configure_git_env, grit_cmd};

const GRIT: &str = env!("CARGO_BIN_EXE_grit");

fn init_local_with_big_blob(base: &Path) -> std::path::PathBuf {
    let src = base.join("src");
    fs::create_dir_all(&src).expect("mkdir src");
    grit_cmd(&["init"]).in_dir(&src).suc();
    let blob = src.join("blob.bin");
    let mut file = fs::File::create(&blob).expect("create blob");
    let chunk = vec![0xABu8; 1024 * 1024];
    for _ in 0..8 {
        file.write_all(&chunk).expect("write chunk");
    }
    grit_cmd(&["commit", "-m", "big blob"]).in_dir(&src).suc();
    src
}

fn bare_remote(base: &Path) -> std::path::PathBuf {
    let bare = base.join("remote.git");
    grit_cmd(&["init", "--bare", bare.to_str().unwrap()])
        .in_dir(base)
        .suc();
    bare
}

fn quarantine_started(remote: &Path) -> bool {
    let objects = remote.join("objects");
    fs::read_dir(&objects).is_ok_and(|entries| {
        entries.flatten().any(|e| {
            e.file_name()
                .to_string_lossy()
                .starts_with("tmp_objdir-incoming-")
        })
    })
}

fn quarantine_leftovers(remote: &Path) -> bool {
    let objects = remote.join("objects");
    let Ok(entries) = fs::read_dir(&objects) else {
        return false;
    };
    entries.flatten().any(|e| {
        let name = e.file_name();
        let n = name.to_string_lossy();
        n.starts_with("tmp_objdir-incoming-")
            || (e.path().join("pack").is_dir()
                && fs::read_dir(e.path().join("pack")).is_ok_and(|pack| {
                    pack.flatten()
                        .any(|p| p.file_name().to_string_lossy().starts_with("tmp_pack_"))
                }))
    })
}

fn spawn_push(base: &Path, local: &Path, remote: &Path) -> std::process::Child {
    let mut cmd = Command::new(GRIT);
    configure_git_env(&mut cmd);
    cmd.env("HOME", base)
        .args(["push", remote.to_str().unwrap(), "main"])
        .current_dir(local)
        .stdout(Stdio::null())
        .stderr(Stdio::piped());
    cmd.spawn().expect("spawn grit push")
}

fn interrupt_push(
    base: &Path,
    local: &Path,
    remote: &Path,
    deliver: fn(&std::process::Child),
) -> (i32, String) {
    let mut child = spawn_push(base, local, remote);
    let deadline = Instant::now() + Duration::from_secs(120);
    let mut saw_quarantine = false;
    while Instant::now() < deadline {
        if let Some(status) = child.try_wait().expect("wait") {
            panic!("push finished before quarantine appeared: {status:?}");
        }
        if quarantine_started(remote) {
            saw_quarantine = true;
            break;
        }
        std::thread::sleep(Duration::from_millis(2));
    }
    assert!(
        saw_quarantine,
        "timed out waiting for push to start writing the receive quarantine"
    );
    deliver(&child);
    let output = child.wait_with_output().expect("wait");
    let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
    (output.status.code().unwrap_or(-1), stderr)
}

fn assert_clean_after_interrupt(remote: &Path, code: i32, stderr: &str) {
    assert_eq!(code, 130, "expected exit 130 on interrupt, got {code}");
    assert!(
        stderr.contains("operation cancelled"),
        "expected cancellation message, got: {stderr}"
    );
    assert!(
        !quarantine_leftovers(remote),
        "interrupted push left quarantine or tmp_pack files under objects/"
    );
    grit_cmd(&["fsck"]).in_dir(remote).suc();
    assert!(
        grit_cmd(&["show-ref"])
            .in_dir(remote)
            .exec()
            .stdout
            .trim()
            .is_empty(),
        "remote refs should be unchanged"
    );
}

#[test]
fn sigterm_and_sighup_during_push_remove_quarantine_and_fsck_is_clean() {
    let base = tempfile::tempdir().expect("tempdir");
    let local = init_local_with_big_blob(base.path());
    let remote = bare_remote(base.path());

    for deliver in [
        |c: &std::process::Child| {
            let pid = libc::pid_t::try_from(c.id()).expect("pid");
            unsafe {
                libc::kill(pid, libc::SIGTERM);
            }
        },
        |c: &std::process::Child| {
            let pid = libc::pid_t::try_from(c.id()).expect("pid");
            unsafe {
                libc::kill(pid, libc::SIGHUP);
            }
        },
    ] {
        let (code, stderr) = interrupt_push(base.path(), &local, &remote, deliver);
        assert_clean_after_interrupt(&remote, code, &stderr);
    }
}

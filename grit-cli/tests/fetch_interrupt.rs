//! Interrupt signals during `grit fetch` remove in-progress pack temp files (issue #2362).

#![cfg(unix)]

use std::fs;
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use grit_test_support::{configure_git_env, grit_cmd};

const GRIT: &str = env!("CARGO_BIN_EXE_grit");
const FILES: usize = 4000;

fn source_bare(base: &Path) -> std::path::PathBuf {
    let src = base.join("src");
    fs::create_dir_all(&src).expect("mkdir src");
    grit_cmd(&["init"]).in_dir(&src).suc();
    for i in 0..FILES {
        let dir = src.join(format!("d{:02}", i % 40));
        fs::create_dir_all(&dir).expect("mkdir");
        let body = format!("file {i}\n").repeat(200);
        fs::write(dir.join(format!("f{i:04}.txt")), body).expect("write");
    }
    grit_cmd(&["commit", "-m", "many files"]).in_dir(&src).suc();
    let bare = base.join("remote.git");
    grit_cmd(&[
        "clone",
        "--bare",
        src.to_str().unwrap(),
        bare.to_str().unwrap(),
    ])
    .in_dir(base)
    .suc();
    let pack_dir = bare.join("objects/pack");
    if pack_dir.is_dir() {
        for entry in fs::read_dir(&pack_dir).expect("read pack") {
            let entry = entry.expect("entry");
            if entry.file_type().expect("type").is_file() {
                fs::remove_file(entry.path()).expect("remove pack file");
            }
        }
    }
    bare
}

fn pack_temp_leftovers(repo: &Path) -> bool {
    let pack = repo.join(".git/objects/pack");
    fs::read_dir(&pack).is_ok_and(|entries| {
        entries.flatten().any(|e| {
            let name = e.file_name();
            let n = name.to_string_lossy();
            n.starts_with("tmp_pack_")
                || n.starts_with("tmp_")
                || (n.starts_with('.') && n.contains("-install-"))
        })
    })
}

fn receiving_pack(dest: &Path) -> bool {
    if pack_temp_leftovers(dest) {
        return true;
    }
    let incoming = dest.join(".git/objects/incoming");
    fs::read_dir(&incoming).is_ok_and(|entries| {
        entries.flatten().any(|e| {
            e.file_name()
                .to_string_lossy()
                .starts_with("pack-receive-")
        })
    })
}

fn spawn_fetch(base: &Path, remote: &Path, dest: &Path) -> std::process::Child {
    grit_cmd(&["init"]).in_dir(dest).suc();
    grit_cmd(&["remote", "add", "origin", remote.to_str().unwrap()])
        .in_dir(dest)
        .suc();
    let mut cmd = Command::new(GRIT);
    configure_git_env(&mut cmd);
    cmd.env("HOME", base)
        .args(["fetch", "origin"])
        .current_dir(dest)
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    cmd.spawn().expect("spawn grit fetch")
}

fn interrupt_fetch(
    base: &Path,
    remote: &Path,
    dest: &Path,
    deliver: fn(&std::process::Child),
) -> i32 {
    let mut child = spawn_fetch(base, remote, dest);
    let started = Instant::now();
    let deadline = Instant::now() + Duration::from_secs(120);
    while started.elapsed() < Duration::from_millis(50) {
        if let Some(status) = child.try_wait().expect("wait") {
            panic!(
                "fetch finished in {:?} before interrupt window: {status:?}",
                started.elapsed()
            );
        }
        if receiving_pack(dest) {
            break;
        }
        assert!(Instant::now() < deadline, "timed out waiting to interrupt fetch");
        std::thread::sleep(Duration::from_micros(200));
    }
    if child.try_wait().expect("wait").is_none() {
        deliver(&child);
    }
    child.wait().expect("wait").code().unwrap_or(-1)
}

fn assert_clean_after_interrupt(dest: &Path, code: i32) {
    assert_eq!(code, 130, "expected exit 130 on interrupt, got {code}");
    assert!(
        !pack_temp_leftovers(dest),
        "interrupted fetch left temp pack files under objects/pack"
    );
    grit_cmd(&["fsck"]).in_dir(dest).suc();
}

#[test]
fn sigterm_and_sighup_during_fetch_remove_temp_packs_and_fsck_is_clean() {
    let base = tempfile::tempdir().expect("tempdir");
    let remote = source_bare(base.path());
    let dest = base.path().join("fx");

    for deliver in [
        |c: &std::process::Child| {
            let pid = libc::pid_t::try_from(c.id()).expect("pid");
            // SAFETY: child process we spawned.
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
        if dest.exists() {
            fs::remove_dir_all(&dest).expect("cleanup dest");
        }
        fs::create_dir(&dest).expect("mkdir fx");
        let code = interrupt_fetch(base.path(), &remote, &dest, deliver);
        assert_clean_after_interrupt(&dest, code);
    }
}

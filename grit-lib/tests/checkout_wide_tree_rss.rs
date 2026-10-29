//! Wide-tree checkout must stay within ~2× system git peak RSS (issue #2280).
//!
//! Peak RSS is measured on the process that execs `git` or `grit` via `fork`/`wait4`,
//! matching the review harness (not in-process library calls).
//!
//! Requires `GRIT_BIN` pointing at a release `grit` built from this workspace
//! (`make test-full` and CI test-full set it after `cargo build --release -p grit-cli`).

#![cfg(target_os = "linux")]

use std::io::Write;
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::path::PathBuf;
use std::process::Command;

const DIRS: u32 = 30;
const FILES_PER_DIR: u32 = 1500;
const DIRS_OTHER: u32 = 15;

fn git(root: &Path, args: &[&str], input: &[u8]) -> String {
    let mut child = Command::new("git")
        .args(args)
        .current_dir(root)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .env("GIT_AUTHOR_NAME", "Test")
        .env("GIT_AUTHOR_EMAIL", "t@example.com")
        .env("GIT_COMMITTER_NAME", "Test")
        .env("GIT_COMMITTER_EMAIL", "t@example.com")
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .expect("spawn git");
    child
        .stdin
        .take()
        .expect("stdin")
        .write_all(input)
        .expect("write stdin");
    let out = child.wait_with_output().expect("wait for git");
    assert!(
        out.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8(out.stdout).expect("UTF-8 output")
}

const GRIT_BIN_ENV: &str = "GRIT_BIN";

fn grit_bin() -> PathBuf {
    let path = std::env::var_os(GRIT_BIN_ENV).unwrap_or_else(|| {
        panic!(
            "{GRIT_BIN_ENV} must point at a release grit binary from this workspace \
             (build with `cargo build --release -p grit-cli` before running this test)"
        )
    });
    let path = PathBuf::from(path);
    assert!(
        path.is_file(),
        "{}={} is not an executable file",
        GRIT_BIN_ENV,
        path.display()
    );
    path
}

fn peak_rss_for_exec(mut cmd: Command) -> i64 {
    let pid = unsafe { libc::fork() };
    assert!(pid >= 0, "fork failed");
    if pid == 0 {
        let err = cmd.exec();
        eprintln!("exec failed: {err}");
        unsafe { libc::_exit(127) };
    }
    let mut status = 0;
    let mut usage = std::mem::MaybeUninit::<libc::rusage>::zeroed();
    let waited = unsafe { libc::wait4(pid, &mut status, 0, usage.as_mut_ptr()) };
    assert_eq!(waited, pid, "wait4 failed");
    assert!(
        libc::WIFEXITED(status) && libc::WEXITSTATUS(status) == 0,
        "child exited with status {status}"
    );
    // SAFETY: wait4 initialized usage on success.
    i64::from(unsafe { usage.assume_init() }.ru_maxrss)
}

fn build_wide_repo(root: &Path) {
    git(root, &["init", "-q", "-b", "main"], b"");
    git(root, &["config", "user.name", "Test"], b"");
    git(root, &["config", "user.email", "t@example.com"], b"");
    for d in 0..DIRS {
        std::fs::create_dir_all(root.join(format!("d{d}"))).expect("mkdir");
        for i in 0..FILES_PER_DIR {
            std::fs::write(
                root.join(format!("d{d}/f{i}.txt")),
                format!("base {d} {i}\n").repeat(5),
            )
            .expect("write");
        }
    }
    git(root, &["add", "-A"], b"");
    git(root, &["commit", "-qm", "base"], b"");

    git(root, &["checkout", "-q", "-b", "other"], b"");
    for d in 0..DIRS_OTHER {
        for i in 0..FILES_PER_DIR {
            let path = root.join(format!("d{d}/f{i}.txt"));
            std::fs::OpenOptions::new()
                .append(true)
                .open(&path)
                .expect("open")
                .write_all(b"other\n")
                .expect("append");
        }
    }
    git(root, &["commit", "-qam", "other"], b"");

    git(root, &["checkout", "-q", "main"], b"");
    for d in DIRS_OTHER..DIRS {
        for i in 0..FILES_PER_DIR {
            let path = root.join(format!("d{d}/f{i}.txt"));
            std::fs::OpenOptions::new()
                .append(true)
                .open(&path)
                .expect("open")
                .write_all(b"main\n")
                .expect("append");
        }
    }
    git(root, &["commit", "-qam", "main2"], b"");
}

fn exec_env(_root: &Path) -> [(&'static str, String); 2] {
    [
        ("GIT_CONFIG_GLOBAL", "/dev/null".to_owned()),
        ("GIT_CONFIG_SYSTEM", "/dev/null".to_owned()),
    ]
}

#[test]
fn checkout_between_wide_trees_peak_rss_within_twice_git() {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path();
    build_wide_repo(root);
    git(root, &["checkout", "-q", "main"], b"");

    let grit = grit_bin();
    assert!(
        grit.is_file(),
        "release grit binary missing at {}",
        grit.display()
    );

    let git_peak = peak_rss_for_exec({
        let mut cmd = Command::new("git");
        cmd.args(["reset", "--hard", "other"]).current_dir(root);
        for (k, v) in exec_env(root) {
            cmd.env(k, v);
        }
        cmd
    });
    git(root, &["checkout", "-q", "main"], b"");

    let grit_peak = peak_rss_for_exec({
        let mut cmd = Command::new(&grit);
        cmd.args(["reset", "--hard", "other"]).current_dir(root);
        for (k, v) in exec_env(root) {
            cmd.env(k, v);
        }
        cmd
    });

    let max_allowed = git_peak * 2 + 4096;
    eprintln!(
        "wide-tree peak RSS KiB: git reset={git_peak} grit reset={grit_peak} max={max_allowed}"
    );
    assert!(
        grit_peak <= max_allowed,
        "grit peaked at {grit_peak} KiB vs git {git_peak} KiB (limit {max_allowed} KiB)"
    );

    assert_eq!(
        git(root, &["write-tree"], b"").trim(),
        git(root, &["rev-parse", "other^{tree}"], b"").trim()
    );
    git(root, &["fsck", "--strict", "--no-progress"], b"");
}

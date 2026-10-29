//! `grit log` on a terminal: the whole history, one line per commit, through the pager,
//! with every row cut to the terminal width.
//!
//! stdout is the slave side of a pseudo-terminal so `grit` sees a real terminal; the pager
//! is a shell command that copies its input to a file.

#![cfg(unix)]
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::fs::File;
use std::io::Read;
use std::os::fd::{FromRawFd, OwnedFd};
use std::path::Path;
use std::process::{Command, Stdio};

const GRIT: &str = env!("CARGO_BIN_EXE_grit");

/// Pseudo-terminal width for these tests.
const COLUMNS: u16 = 80;

fn grit_ok(dir: &Path, args: &[&str]) {
    let out = Command::new(GRIT)
        .args(args)
        .current_dir(dir)
        .output()
        .expect("spawn grit");
    assert!(
        out.status.success(),
        "grit {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

/// A repository with `count` commits; each subject is long enough to need truncating.
fn repo_with_commits(count: usize) -> tempfile::TempDir {
    let dir = tempfile::tempdir().expect("tempdir");
    grit_ok(dir.path(), &["init", "."]);
    for i in 0..count {
        std::fs::write(dir.path().join("a.txt"), format!("v{i}\n")).unwrap();
        grit_ok(dir.path(), &["add", "a.txt"]);
        let subject = format!("commit {i} with a subject long enough to run past the edge");
        grit_ok(dir.path(), &["commit", &subject]);
    }
    dir
}

/// Run `grit log args…` with stdout on a `COLUMNS`-wide pseudo-terminal and `pager` as
/// `GRIT_PAGER`. Returns what reached the terminal directly.
fn log_on_terminal(dir: &Path, args: &[&str], pager: &str) -> String {
    let mut master = 0;
    let mut slave = 0;
    let mut size = libc::winsize {
        ws_row: 40,
        ws_col: COLUMNS,
        ws_xpixel: 0,
        ws_ypixel: 0,
    };
    // SAFETY: `openpty` writes two fds into the provided integers; the name and termios
    // arguments may be null.
    let rc = unsafe {
        libc::openpty(
            &mut master,
            &mut slave,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            &mut size,
        )
    };
    assert_eq!(rc, 0, "openpty failed");
    // SAFETY: both fds were just opened by `openpty` and are owned here.
    let (master, slave) = unsafe { (OwnedFd::from_raw_fd(master), OwnedFd::from_raw_fd(slave)) };
    let child = Command::new(GRIT)
        .arg("log")
        .args(args)
        .current_dir(dir)
        .env("GRIT_PAGER", pager)
        .env("NO_COLOR", "1")
        .stdin(Stdio::null())
        .stdout(Stdio::from(slave))
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn grit log");
    // Drain the terminal while grit runs so it never blocks on a full pty buffer.
    let reader = std::thread::spawn(move || {
        let mut out = Vec::new();
        let mut master = File::from(master);
        let mut buf = [0u8; 4096];
        // Reads fail with EIO once grit (the last holder of the slave side) exits.
        while let Ok(n) = master.read(&mut buf) {
            if n == 0 {
                break;
            }
            out.extend_from_slice(&buf[..n]);
        }
        out
    });
    let output = child.wait_with_output().expect("wait for grit log");
    assert!(
        output.status.success(),
        "grit log {args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let out = reader.join().expect("terminal reader");
    String::from_utf8_lossy(&out).replace("\r\n", "\n")
}

fn commit_lines(text: &str) -> Vec<&str> {
    text.lines().filter(|l| l.contains("commit ")).collect()
}

#[test]
fn terminal_log_pages_whole_history() {
    let dir = repo_with_commits(25);
    let paged = dir.path().join("paged.txt");
    let pager = format!("cat > '{}'", paged.display());
    let direct = log_on_terminal(dir.path(), &[], &pager);
    assert!(
        commit_lines(&direct).is_empty(),
        "output goes to the pager, not the terminal: {direct}"
    );
    let text = std::fs::read_to_string(&paged).expect("pager received output");
    let rows = commit_lines(&text);
    assert_eq!(rows.len(), 25, "the whole history, not a page: {text}");
    assert!(
        rows.iter().all(|row| !row.contains('●')),
        "no graph unless asked for: {text}"
    );
    assert!(!text.contains("→ more"), "nothing left to page to: {text}");
    for row in rows {
        assert!(
            row.chars().count() <= usize::from(COLUMNS),
            "row wider than the terminal ({} > {COLUMNS}): {row}",
            row.chars().count()
        );
        assert!(row.contains('…'), "long subject truncated: {row}");
        assert!(
            row.ends_with("+1 -1") || row.ends_with("+1 -0"),
            "line summary at the right edge: {row}"
        );
        assert_eq!(
            row.chars().count(),
            usize::from(COLUMNS),
            "the summary ends at the terminal edge: {row}"
        );
    }
}

#[test]
fn terminal_log_no_pager() {
    let dir = repo_with_commits(25);
    let paged = dir.path().join("paged.txt");
    let pager = format!("cat > '{}'", paged.display());
    let direct = log_on_terminal(dir.path(), &["--no-pager"], &pager);
    assert!(!paged.exists(), "--no-pager skips the pager");
    let rows = commit_lines(&direct);
    assert_eq!(rows.len(), 25, "the whole history: {direct}");
    assert!(
        rows.iter()
            .all(|row| row.chars().count() <= usize::from(COLUMNS)),
        "rows fit the terminal: {direct}"
    );
}

#[test]
fn terminal_log_graph_with_limit_and_cat_pager() {
    let dir = repo_with_commits(5);
    let direct = log_on_terminal(dir.path(), &["-n", "3", "--graph", "--decorate"], "cat");
    let rows = commit_lines(&direct);
    assert_eq!(rows.len(), 3, "`-n` still limits on a terminal: {direct}");
    assert!(
        rows.iter().all(|row| row.starts_with('●')),
        "graph drawn when asked for: {direct}"
    );
    assert!(
        rows[0].contains("HEAD -> main"),
        "decorated newest commit: {direct}"
    );
    assert!(
        rows.iter()
            .all(|row| row.chars().count() <= usize::from(COLUMNS)),
        "decorated graph rows fit the terminal: {direct}"
    );
}

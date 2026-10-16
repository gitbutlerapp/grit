//! Index lock PID sidecar: live vs stale process detection (native liveness, no `kill` CLI).

use grit_lib::index::format_index_lock_blocked_detail;
use std::fs;

#[test]
fn lock_detail_reports_live_pid_for_current_process() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let index_path = tmp.path().join("index");
    let lock_path = index_path.with_extension("lock");
    let pid_path = tmp.path().join("index~pid.lock");
    fs::write(&pid_path, format!("pid {}", std::process::id())).expect("write pid");
    fs::write(&lock_path, b"").expect("touch lock");

    let msg = format_index_lock_blocked_detail(&index_path);
    assert!(
        msg.contains(&format!("process {}", std::process::id())),
        "expected live pid in message: {msg}"
    );
    assert!(
        msg.contains("Lock is held by process"),
        "expected held lock wording: {msg}"
    );
}

#[test]
fn lock_detail_reports_stale_pid() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let index_path = tmp.path().join("index");
    let lock_path = index_path.with_extension("lock");
    let pid_path = tmp.path().join("index~pid.lock");
    fs::write(&pid_path, "pid 999999999").expect("write pid");
    fs::write(&lock_path, b"").expect("touch lock");

    let msg = format_index_lock_blocked_detail(&index_path);
    assert!(
        msg.contains("no longer running"),
        "expected stale lock wording: {msg}"
    );
}

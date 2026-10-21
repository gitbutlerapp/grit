//! Linux integration check for `measure-rss` byte scaling.

use std::path::Path;
use std::process::Command;

#[test]
fn measure_rss_scales_linux_kib() {
    if !cfg!(target_os = "linux") {
        return;
    }
    let exe = env!("CARGO_BIN_EXE_grit-bench");
    let cwd = Path::new("/tmp");
    // ~64 MiB resident set (order-of-magnitude check after KiB→bytes conversion).
    let script = "python3 -c 'bytearray(64 * 1024 * 1024)'";
    let out = Command::new(exe)
        .arg("measure-rss")
        .arg("--cwd")
        .arg(cwd)
        .env("GRIT_BENCH_MEASURE_CMD", script)
        .output()
        .expect("measure-rss");
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let rss: u64 = String::from_utf8_lossy(&out.stdout)
        .trim()
        .parse()
        .expect("rss integer");
    assert!(
        rss > 32 * 1024 * 1024,
        "expected tens of MiB RSS, got {rss} bytes"
    );
}

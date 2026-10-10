//! `grit init --markdown` includes ref_format in stdout.

use std::process::Command;

#[test]
fn init_markdown_lists_ref_format() {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path().join("repo");
    let grit = env!("CARGO_BIN_EXE_grit");
    let out = Command::new(grit)
        .args(["init", "--markdown", "--ref-format", "reftable"])
        .arg(&root)
        .output()
        .expect("grit init");
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("**ref_format**: reftable"),
        "stdout:\n{stdout}"
    );
}

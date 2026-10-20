//! `grit diff --patch-text` behavior.

use std::process::Command;

#[test]
fn diff_patch_text_rejected_with_json() {
    let grit = env!("CARGO_BIN_EXE_grit");
    let out = Command::new(grit)
        .args(["diff", "--patch-text", "--json"])
        .output()
        .expect("spawn grit");
    assert!(!out.status.success(), "expected failure");
    let msg = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        msg.contains("--patch-text") && msg.contains("--json"),
        "expected clear error, got: {msg}"
    );
}

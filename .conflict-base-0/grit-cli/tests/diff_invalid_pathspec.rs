//! `grit diff` with one argument must error when it is neither a revision nor a known path.

use std::process::Command;

fn grit(dir: &std::path::Path, args: &[&str]) -> (i32, String, String) {
    let out = Command::new(env!("CARGO_BIN_EXE_grit"))
        .current_dir(dir)
        .args(args)
        .output()
        .expect("run grit");
    (
        out.status.code().unwrap_or(-1),
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

#[test]
fn diff_unknown_revision_or_path_errors_instead_of_empty_success() {
    let dir = tempfile::tempdir().expect("tempdir");
    let init = grit(dir.path(), &["init"]);
    assert_eq!(init.0, 0, "grit init failed: {}", init.2);

    for spec in ["mainn", "does-not-exist"] {
        let (code, stdout, stderr) = grit(dir.path(), &["diff", spec]);
        assert_ne!(
            code, 0,
            "grit diff {spec} should fail (got exit {code}, stdout={stdout:?}, stderr={stderr:?})"
        );
        assert!(
            stderr.contains("did not match")
                || stderr.contains("not found")
                || stderr.contains("revision"),
            "expected revision/path error for {spec}, stderr={stderr}"
        );
    }
}

//! `grit gc`, `repack`, and `fsck` are registered subcommands.

use std::process::Command;

#[test]
fn maintenance_subcommands_help() {
    for cmd in ["gc", "repack", "fsck", "verify"] {
        let out = Command::new(env!("CARGO_BIN_EXE_grit"))
            .arg(cmd)
            .arg("--help")
            .output()
            .unwrap_or_else(|e| panic!("spawn grit {cmd}: {e}"));
        assert!(
            out.status.success(),
            "grit {cmd} --help: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }
}

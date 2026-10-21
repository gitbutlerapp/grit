//! CLI argv regression for `grit-bench fixtures prepare`.

use std::path::PathBuf;
use std::process::Command;

fn bench_exe() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_grit-bench"))
}

#[test]
fn prepare_only_parses_comma_separated_fixture_kinds() {
    let mut cmd = Command::new(bench_exe());
    cmd.args([
        "fixtures",
        "prepare",
        "--only",
        "synth-10k-smoke,many-refs-smoke,git-git-wt",
        "--offline",
    ]);
    let out = cmd.output().expect("spawn grit-bench");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        !stderr.contains("invalid value"),
        "comma-separated --only must parse as multiple kinds (CI daily smoke): {stderr}"
    );
}

//! A three-line blame range must not retain a growing historical object pack.

#![cfg(target_os = "linux")]
#![allow(clippy::expect_used)]

use std::fs;
use std::path::Path;
use std::process::{Command, Stdio};

use grit_test_support::configure_git_env;

fn peak_kib(root: &Path, program: &str, args: &[&str]) -> u64 {
    let report = tempfile::NamedTempFile::new().expect("rss report");
    let mut cmd = Command::new("/usr/bin/time");
    configure_git_env(&mut cmd);
    let out = cmd
        .current_dir(root)
        .args(["-f", "%M", "-o"])
        .arg(report.path())
        .arg(program)
        .args(args)
        .stdout(Stdio::null())
        .output()
        .expect("time");
    assert!(
        out.status.success(),
        "{program} {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    fs::read_to_string(report.path())
        .expect("rss")
        .trim()
        .parse()
        .expect("KiB")
}

#[test]
fn ranged_blame_pack_residency_is_bounded() {
    let grit = env!("CARGO_BIN_EXE_grit");
    let mut peaks = Vec::new();
    for padding in [8 * 1024, 64 * 1024] {
        let root = tempfile::tempdir().expect("repo");
        grit_test_support::blame::packed_history(root.path(), 1800, padding).expect("history");
        let args = [
            "blame",
            "--rev",
            "HEAD",
            "-L",
            "1,3",
            "--json",
            "target.txt",
        ];
        let git_args = [
            "blame",
            "--line-porcelain",
            "-L",
            "1,3",
            "HEAD",
            "--",
            "target.txt",
        ];
        peak_kib(root.path(), grit, &args);
        peak_kib(root.path(), "git", &git_args);
        let mut ours = Vec::new();
        let mut theirs = Vec::new();
        for _ in 0..3 {
            ours.push(peak_kib(root.path(), grit, &args));
            theirs.push(peak_kib(root.path(), "git", &git_args));
        }
        ours.sort_unstable();
        theirs.sort_unstable();
        let ours = ours[1];
        let theirs = theirs[1];
        eprintln!("padding={padding}: grit={ours} KiB git={theirs} KiB");
        assert!(ours <= 2 * theirs + 16 * 1024, "grit={ours}, git={theirs}");
        peaks.push(ours);
    }
    assert!(
        peaks[1] <= peaks[0] + 32 * 1024,
        "pack growth raised RSS: {peaks:?}"
    );
}

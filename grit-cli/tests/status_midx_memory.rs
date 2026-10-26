//! A large historical object set must not make a tiny clean checkout retain the MIDX.

#![cfg(target_os = "linux")]
#![allow(clippy::expect_used)]

use std::fs;
use std::io::{BufWriter, Write};
use std::path::Path;
use std::process::{Command, Stdio};

use grit_test_support::configure_git_env;

fn git(root: &Path, args: &[&str]) -> Vec<u8> {
    let mut cmd = Command::new("git");
    configure_git_env(&mut cmd);
    let out = cmd.current_dir(root).args(args).output().expect("git");
    assert!(
        out.status.success(),
        "{args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    out.stdout
}

fn historical_objects(root: &Path) {
    git(root, &["init", "-q", "-b", "main"]);
    // Fast-import writes real objects and a pack index without thousands of git processes.
    // HEAD has just one file; all the other blobs belong to its parent.
    let input = tempfile::tempfile().expect("import input");
    {
        let mut out = BufWriter::new(&input);
        writeln!(
            out,
            "commit refs/heads/main\ncommitter T <t@example.com> 1234567890 +0000\ndata 3\nold"
        )
        .expect("commit");
        for i in 0..420_000 {
            let data = format!("historical blob {i}\n");
            writeln!(
                out,
                "M 100644 inline old/{}/f{}\ndata {}\n{data}",
                i / 1000,
                i % 1000,
                data.len()
            )
            .expect("blob");
        }
        writeln!(out, "\ncommit refs/heads/main\ncommitter T <t@example.com> 1234567891 +0000\ndata 3\nnew\ndeleteall\nM 100644 inline current\ndata 2\nx\n\ndone").expect("tip");
        out.flush().expect("flush");
    }
    use std::io::{Seek, SeekFrom};
    let mut input = input;
    input.seek(SeekFrom::Start(0)).expect("rewind");
    let mut cmd = Command::new("git");
    configure_git_env(&mut cmd);
    let out = cmd
        .current_dir(root)
        .args(["fast-import", "--quiet", "--done"])
        .stdin(input)
        .output()
        .expect("fast-import");
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    git(root, &["reset", "--hard", "-q", "HEAD"]);
    git(root, &["pack-refs", "--all"]);
    git(
        root,
        &["commit-graph", "write", "--reachable", "--changed-paths"],
    );
    git(root, &["multi-pack-index", "write"]);
    git(root, &["fsck", "--strict"]);
    assert!(git(root, &["status", "--porcelain"]).is_empty());
}

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
fn clean_status_midx_peak_rss_is_bounded_in_every_format() {
    let root = tempfile::tempdir().expect("repo");
    historical_objects(root.path());
    let midx = root.path().join(".git/objects/pack/multi-pack-index");
    let saved = midx.with_extension("saved");
    assert!(fs::metadata(&midx).expect("midx").len() > 11 * 1024 * 1024);
    let grit = env!("CARGO_BIN_EXE_grit");
    for flags in [
        &[][..],
        &["--short"][..],
        &["--json"][..],
        &["--markdown"][..],
    ] {
        let mut args = vec!["status"];
        args.extend_from_slice(flags);
        fs::rename(&midx, &saved).expect("hide midx");
        let without = (0..3)
            .map(|_| peak_kib(root.path(), grit, &args))
            .min()
            .expect("runs");
        let git_without = peak_kib(root.path(), "git", &["status"]);
        fs::rename(&saved, &midx).expect("restore midx");
        let with = (0..3)
            .map(|_| peak_kib(root.path(), grit, &args))
            .min()
            .expect("runs");
        let git_with = peak_kib(root.path(), "git", &["status"]);
        eprintln!("{args:?}: grit {without} -> {with} KiB; git {git_without} -> {git_with} KiB");
        // A checksum needs a bounded window, not one resident copy per cache/view plus
        // a whole-file allocation for PNAM. Allow process/allocator variance in debug builds.
        assert!(
            with <= without + 8 * 1024,
            "{args:?}: MIDX added {} KiB (without={without}, with={with})",
            with.saturating_sub(without)
        );
    }
}

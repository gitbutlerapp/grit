//! Sparse tag peeling must not make a bounded log retain the historical pack.
#![cfg(target_os = "linux")]
#![allow(clippy::expect_used, clippy::unwrap_used)]

use std::fs;
use std::io::{BufWriter, Seek, SeekFrom, Write};
use std::path::Path;
use std::process::Command;

use grit_test_support::configure_git_env;

fn git(root: &Path, args: &[&str]) -> Vec<u8> {
    let mut cmd = Command::new("git");
    configure_git_env(&mut cmd);
    let out = cmd.current_dir(root).args(args).output().unwrap();
    assert!(
        out.status.success(),
        "{args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    out.stdout
}

fn scattered_tags(root: &Path) {
    git(root, &["init", "-q", "-b", "main"]);
    let mut input = tempfile::tempfile().unwrap();
    {
        let mut out = BufWriter::new(&input);
        let mut state = 4751u64;
        let mut blob = vec![0u8; 128 * 1024];
        for i in 0..768 {
            for byte in &mut blob {
                state ^= state << 13;
                state ^= state >> 7;
                state ^= state << 17;
                *byte = state as u8;
            }
            writeln!(out, "commit refs/heads/main\nmark :{}\ncommitter T <t@example.com> {} +0000\ndata 4\nlog\n\nM 100644 inline data\ndata {}", i + 1, 1234567890 + i, blob.len()).unwrap();
            out.write_all(&blob).unwrap();
            writeln!(
                out,
                "\n\ntag t{i}\nfrom :{}\ntagger T <t@example.com> 1234567890 +0000\ndata 4\ntag\n",
                i + 1
            )
            .unwrap();
        }
        writeln!(out, "done").unwrap();
        out.flush().unwrap();
    }
    input.seek(SeekFrom::Start(0)).unwrap();
    let mut cmd = Command::new("git");
    configure_git_env(&mut cmd);
    let out = cmd
        .current_dir(root)
        .args(["fast-import", "--quiet", "--done"])
        .stdin(input)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    // Fast-import may pack refs. Materialize all tips before clearing packed peel records,
    // so loose tags cannot accidentally use the matching packed tag shortcut.
    let refs = git(
        root,
        &["for-each-ref", "--format=%(objectname) %(refname)", "refs/"],
    );
    for line in std::str::from_utf8(&refs).unwrap().lines() {
        let (oid, name) = line.split_once(' ').unwrap();
        let path = root.join(".git").join(name);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, format!("{oid}\n")).unwrap();
    }
    let packed = root.join(".git/packed-refs");
    if packed.exists() {
        fs::remove_file(packed).unwrap();
    }
    git(root, &["fsck", "--strict"]);
}

fn measure(root: &Path, program: &str, args: &[&str]) -> (u64, Vec<u8>) {
    let report = tempfile::NamedTempFile::new().unwrap();
    let mut cmd = Command::new("/usr/bin/time");
    configure_git_env(&mut cmd);
    let out = cmd
        .current_dir(root)
        .args(["-f", "%M", "-o"])
        .arg(report.path())
        .arg(program)
        .args(args)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{program}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let rss = fs::read_to_string(report.path())
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    (rss, out.stdout)
}

#[test]
fn bounded_log_loose_and_packed_tags_have_equal_output_and_bounded_rss() {
    let root = tempfile::tempdir().unwrap();
    scattered_tags(root.path());
    let grit = env!("CARGO_BIN_EXE_grit");
    let grit_args = ["log", "-n", "10", "--json"];
    let git_args = [
        "log",
        "-n",
        "10",
        "--format=%H%x09%P%x09%an%x09%ae%x09%aI%x09%cn%x09%ce%x09%cI%x09%D%x09%B",
    ];
    let (loose, output) = measure(root.path(), grit, &grit_args);
    let (git_loose, git_output) = measure(root.path(), "git", &git_args);
    git(root.path(), &["pack-refs", "--all"]);
    let (packed, packed_output) = measure(root.path(), grit, &grit_args);
    let (git_packed, git_packed_output) = measure(root.path(), "git", &git_args);
    assert_eq!(output, packed_output);
    assert_eq!(git_output, git_packed_output);
    let json: serde_json::Value = serde_json::from_slice(&output).unwrap();
    assert_eq!(json["commits"].as_array().unwrap().len(), 10);
    eprintln!("grit loose={loose} packed={packed}; git loose={git_loose} packed={git_packed} KiB");
    // Allow process/allocator variation and index pages, but not fault-around windows for
    // hundreds of scattered tag and terminal-commit headers. Works with debug binaries too.
    assert!(
        loose <= packed + 12 * 1024,
        "loose tags added {} KiB (loose={loose}, packed={packed})",
        loose.saturating_sub(packed)
    );
}

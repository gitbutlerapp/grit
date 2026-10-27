//! Deterministic packed history for blame compatibility and residency checks.

use std::io::{self, BufWriter, Seek, SeekFrom, Write};
use std::path::Path;
use std::process::{Command, Stdio};

/// Create Git-written history with wide, changing root trees and a tiny blamed range.
///
/// `commits` controls history depth; `padding_bytes` controls pack size independently
/// of the requested lines. The target is renamed halfway through history, and a
/// side-parent merge carries an unrelated edit. Returns command or filesystem errors.
pub fn packed_history(root: &Path, commits: usize, padding_bytes: usize) -> io::Result<()> {
    git(root, &["init", "-q", "-b", "main"])?;
    let mut input = tempfile::tempfile()?;
    {
        let mut out = BufWriter::new(&input);
        let mut random = 17u64;
        for i in 0..commits {
            let target = if i < commits / 2 {
                "old.txt"
            } else {
                "target.txt"
            };
            writeln!(
                out,
                "commit refs/heads/main\nmark :{}\ncommitter T <t@example.com> {} +0000\ndata 1\nx",
                i + 1,
                1234567890 + i
            )?;
            if i == 0 {
                for n in 0..1000 {
                    writeln!(out, "M 100644 inline noise-{n:04}\ndata 1\nx")?;
                }
            }
            if i == commits / 2 {
                writeln!(out, "R old.txt target.txt")?;
            }
            let text = format!("first\nsecond\nthird\ntail {i}\n");
            writeln!(out, "M 100644 inline {target}\ndata {}\n{text}", text.len())?;
            writeln!(
                out,
                "M 100644 inline noise-{:04}\ndata {padding_bytes}",
                i % 1000
            )?;
            for _ in 0..padding_bytes {
                random ^= random << 13;
                random ^= random >> 7;
                random ^= random << 17;
                out.write_all(&[random as u8])?;
            }
            writeln!(out, "\n")?;
        }
        if commits >= 3 {
            writeln!(out, "commit refs/heads/topic\nmark :{}\ncommitter T <t@example.com> {} +0000\ndata 1\nx\nfrom :{}\nM 100644 inline side.txt\ndata 4\nside\n", commits + 1, 1234567890 + commits, commits / 3)?;
            writeln!(out, "commit refs/heads/main\ncommitter T <t@example.com> {} +0000\ndata 1\nx\nmerge :{}\nM 100644 inline side.txt\ndata 4\nside\n", 1234567891 + commits, commits + 1)?;
        }
        writeln!(out, "done")?;
        out.flush()?;
    }
    input.seek(SeekFrom::Start(0))?;
    let mut cmd = Command::new("git");
    crate::configure_git_env(&mut cmd);
    let output = cmd
        .current_dir(root)
        .args(["fast-import", "--quiet", "--done"])
        .stdin(input)
        .stdout(Stdio::null())
        .output()?;
    check(output)?;
    git(root, &["repack", "-adf", "--depth=50", "--window=10"])?;
    git(root, &["prune-packed"])?;
    git(root, &["fsck", "--strict"])?;
    git(root, &["reset", "--hard", "-q", "HEAD"])?;
    Ok(())
}

fn check(output: std::process::Output) -> io::Result<()> {
    if output.status.success() {
        Ok(())
    } else {
        Err(io::Error::other(
            String::from_utf8_lossy(&output.stderr).into_owned(),
        ))
    }
}

fn git(root: &Path, args: &[&str]) -> io::Result<()> {
    let mut cmd = Command::new("git");
    crate::configure_git_env(&mut cmd);
    check(cmd.current_dir(root).args(args).output()?)
}

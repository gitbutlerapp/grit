//! Git-written sparse-index fixtures with a fixed visible working set.

use std::fmt::Write as _;
use std::io::Write as _;
use std::path::Path;
use std::process::{Command, Stdio};

/// Build two tagged commits with `paths` tracked files, 300 visible in `d000`, and
/// 400 excluded directories. `unique` gives each excluded file its own blob;
/// `changed_excluded` changes that many excluded paths in the second commit.
///
/// `git` selects the system Git executable and `dir` is the repository directory.
/// Returns an I/O error for failed commands or invalid fixture sizes. The tags
/// `sparse-base` and `sparse-edited` are the reset/switch endpoints.
pub fn create(
    git: &Path,
    dir: &Path,
    paths: usize,
    unique: bool,
    changed_excluded: usize,
) -> std::io::Result<()> {
    if paths < 700 || changed_excluded > paths - 300 {
        return Err(std::io::Error::other("invalid sparse fixture size"));
    }
    std::fs::create_dir_all(dir)?;
    let run = |args: &[&str]| -> std::io::Result<()> {
        let out = Command::new(git).args(args).current_dir(dir).output()?;
        if !out.status.success() {
            return Err(std::io::Error::other(
                String::from_utf8_lossy(&out.stderr).into_owned(),
            ));
        }
        Ok(())
    };
    run(&["init", "-q", "-b", "main"])?;
    run(&["config", "gc.auto", "0"])?;
    let mut stream =
        String::from("blob\nmark :1\ndata 5\nsame\n\nblob\nmark :2\ndata 7\nedited\n\n");
    if unique {
        for i in 0..paths - 300 {
            let body = format!("file {i}\n");
            let _ = write!(
                stream,
                "blob\nmark :{}\ndata {}\n{}\n",
                i + 10,
                body.len(),
                body
            );
        }
    }
    stream.push_str("commit refs/heads/main\nmark :3\ncommitter Mono Dev <mono@example.com> 1700000000 +0000\ndata 5\nbase\n");
    for i in 0..paths - 300 {
        let mark = if unique { i + 10 } else { 1 };
        let _ = writeln!(stream, "M 100644 :{mark} d{:03}/f{i:06}.txt", i % 400 + 1);
    }
    for i in 0..300 {
        let _ = writeln!(stream, "M 100644 :1 d000/f{i:03}.txt");
    }
    stream.push_str("\ncommit refs/heads/main\ncommitter Mono Dev <mono@example.com> 1700000010 +0000\ndata 7\nedited\nfrom :3\n");
    for i in 0..300 {
        let _ = writeln!(stream, "M 100644 :2 d000/f{i:03}.txt");
    }
    for i in 0..changed_excluded {
        let _ = writeln!(stream, "M 100644 :2 d{:03}/f{i:06}.txt", i % 400 + 1);
    }
    stream.push_str("\ndone\n");
    let mut child = Command::new(git)
        .args(["fast-import", "--quiet"])
        .current_dir(dir)
        .stdin(Stdio::piped())
        .spawn()?;
    child
        .stdin
        .take()
        .ok_or_else(|| std::io::Error::other("missing fast-import stdin"))?
        .write_all(stream.as_bytes())?;
    if !child.wait()?.success() {
        return Err(std::io::Error::other("fast-import failed"));
    }
    run(&["tag", "sparse-base", "HEAD~1"])?;
    run(&["tag", "sparse-edited", "HEAD"])?;
    for key in [
        "core.sparseCheckout",
        "core.sparseCheckoutCone",
        "index.sparse",
    ] {
        run(&["config", key, "true"])?;
    }
    std::fs::write(dir.join(".git/info/sparse-checkout"), "/*\n!/*/\n/d000/\n")?;
    run(&["read-tree", "-mu", "HEAD"])
}

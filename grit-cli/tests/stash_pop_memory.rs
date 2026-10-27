//! Sparse stash edits must not retain several copies of a mostly unchanged tree.

#![cfg(target_os = "linux")]
#![allow(clippy::expect_used)]

use std::fs;
use std::io::{BufWriter, Seek, SeekFrom, Write};
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

fn fixture(root: &Path, files: usize) -> String {
    git(root, &["init", "-q", "-b", "main"]);
    git(root, &["config", "gc.auto", "0"]);
    let mut input = tempfile::tempfile().expect("import input");
    {
        let mut out = BufWriter::new(&input);
        writeln!(
            out,
            "commit refs/heads/main\ncommitter T <t@example.com> 1234567890 +0000\ndata 4\nbase"
        )
        .expect("commit");
        for i in 0..files {
            let data: String = (0..20).map(|j| format!("line {j} of {i}\n")).collect();
            writeln!(
                out,
                "M 100644 inline d{:03}/f{i:06}.txt\ndata {}\n{data}",
                i % 400,
                data.len()
            )
            .expect("blob");
        }
        writeln!(out, "\ndone").expect("done");
        out.flush().expect("flush");
    }
    input.seek(SeekFrom::Start(0)).expect("rewind");
    let mut cmd = Command::new("git");
    configure_git_env(&mut cmd);
    let out = cmd
        .current_dir(root)
        .args(["fast-import", "--quiet", "--done"])
        .stdin(input)
        .output()
        .expect("import");
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    git(root, &["reset", "--hard", "-q", "HEAD"]);
    let paths = git(root, &["ls-files"]);
    for path in String::from_utf8(paths).expect("paths").lines().take(3000) {
        writeln!(
            fs::OpenOptions::new()
                .append(true)
                .open(root.join(path))
                .expect("edit"),
            "stash repeat"
        )
        .expect("append");
    }
    git(root, &["stash", "push", "-qm", "repeat"]);
    String::from_utf8(git(root, &["rev-parse", "refs/stash"]))
        .expect("oid")
        .trim()
        .to_owned()
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

struct PopFixture {
    root: tempfile::TempDir,
    stash: String,
    git_rss: u64,
    diff: Vec<u8>,
    index: Vec<u8>,
    state: Vec<u8>,
}

impl PopFixture {
    fn new(files: usize) -> Self {
        let root = tempfile::tempdir().expect("repo");
        let stash = fixture(root.path(), files);
        let git_rss = peak_kib(root.path(), "git", &["stash", "pop"]);
        let diff = git(root.path(), &["diff", "--binary"]);
        let index = git(root.path(), &["ls-files", "--stage"]);
        let state = git(root.path(), &["status", "--porcelain"]);
        assert!(git(root.path(), &["stash", "list"]).is_empty());
        git(root.path(), &["fsck", "--strict"]);
        Self {
            root,
            stash,
            git_rss,
            diff,
            index,
            state,
        }
    }

    fn measure(&self, args: &[&str]) -> u64 {
        let root = self.root.path();
        let mut rss = u64::MAX;
        for _ in 0..2 {
            git(root, &["reset", "--hard", "-q", "HEAD"]);
            git(root, &["stash", "store", "-m", "repeat", &self.stash]);
            rss = rss.min(peak_kib(root, env!("CARGO_BIN_EXE_grit"), args));
            assert_eq!(git(root, &["diff", "--binary"]), self.diff);
            assert_eq!(git(root, &["ls-files", "--stage"]), self.index);
            assert_eq!(git(root, &["status", "--porcelain"]), self.state);
            assert!(git(root, &["stash", "list"]).is_empty());
            git(root, &["fsck", "--strict"]);
        }
        rss
    }
}

#[test]
fn sparse_stash_pop_peak_rss_is_bounded_in_every_format() {
    // Both trees apply exactly 3k edits. The small tree controls for the process,
    // allocator and checkout overhead, including that of unoptimized test binaries.
    let small = PopFixture::new(4_000);
    let wide = PopFixture::new(120_000);
    let git_growth = wide.git_rss.saturating_sub(small.git_rss);
    for flags in [&[][..], &["--json"][..], &["--markdown"][..]] {
        let mut args = vec!["stash", "pop"];
        args.extend_from_slice(flags);
        let small_rss = small.measure(&args);
        let wide_rss = wide.measure(&args);
        let growth = wide_rss.saturating_sub(small_rss);
        eprintln!(
            "{args:?}: grit {small_rss} -> {wide_rss} KiB; git {} -> {} KiB",
            small.git_rss, wide.git_rss
        );
        // Allow allocator variance while rejecting retained whole-tree/index copies.
        assert!(
            growth <= git_growth * 3 / 2 + 12 * 1024,
            "{args:?}: grit growth={growth} KiB, git growth={git_growth} KiB"
        );
        if !cfg!(debug_assertions) {
            assert!(
                wide_rss <= wide.git_rss * 3 / 2 + 12 * 1024,
                "{args:?}: grit={wide_rss} KiB, git={} KiB",
                wide.git_rss
            );
        }
    }
}

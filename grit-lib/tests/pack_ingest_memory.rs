//! Peak-memory gate for receiving a pack (the clone/fetch `index-pack` path).
//!
//! Builds a medium pack with system `git`, then measures the peak resident set size of
//! `git index-pack` and of grit's [`install_pack_path`] on the same file, each in its own
//! child process (the grit side re-runs this test binary on the ignored `ingest_child`
//! test). The gate fails when grit needs more than [`MAX_RATIO`] times git's peak.
//!
//! Peak RSS includes file-backed pages, so a pack that stays fully memory-mapped while it
//! is indexed counts its whole size; that is the regression this guards against.

#![cfg(target_os = "linux")]
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use grit_lib::objects::index_pack::{install_pack_path, IngestPackOptions};
use grit_lib::objects::odb::Odb;

/// Allowed grit / git peak-RSS ratio on the medium pack.
const MAX_RATIO: f64 = 1.5;
const CHILD_PACK_ENV: &str = "GRIT_INGEST_MEMORY_CHILD_PACK";
const PROBE_TOOL_ENV: &str = "GRIT_INGEST_MEMORY_PROBE_TOOL";
const PROBE_SCRATCH_ENV: &str = "GRIT_INGEST_MEMORY_PROBE_SCRATCH";

const GIT_ENV: &[(&str, &str)] = &[
    ("GIT_CONFIG_GLOBAL", "/dev/null"),
    ("GIT_CONFIG_SYSTEM", "/dev/null"),
    ("GIT_CONFIG_NOSYSTEM", "1"),
];

fn git(dir: &Path) -> Command {
    let mut cmd = Command::new("git");
    cmd.current_dir(dir).envs(GIT_ENV.iter().copied());
    cmd
}

fn run(cmd: &mut Command) {
    let out = cmd.output().expect("spawn");
    assert!(
        out.status.success(),
        "{cmd:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

/// Deterministic pseudo-random bytes (xorshift), so blobs do not compress or delta.
struct Noise(u64);

impl Noise {
    fn fill(&mut self, out: &mut Vec<u8>, len: usize) {
        out.reserve(len);
        for _ in 0..len {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 7;
            self.0 ^= self.0 << 17;
            out.push((self.0 >> 24) as u8);
        }
    }
}

/// A repository whose history mixes edited text files (long delta chains) with fresh
/// incompressible blobs (bulk pack bytes), repacked by `git` like a server would send it.
/// Returns the path of its single pack (about 140k objects in 50 MiB), large enough that
/// both tools' working memory outweighs their binaries' baseline.
fn build_medium_pack(dir: &Path) -> PathBuf {
    const COMMITS: usize = 10_000;
    const TEXT_FILES: usize = 40;
    const BINARY_SLOTS: usize = 200;
    const BINARY_LEN: usize = 2 * 1024;

    run(git(dir).args(["init", "-q", "--bare", "repo.git"]));
    let repo = dir.join("repo.git");
    let mut import = git(&repo)
        .args(["fast-import", "--quiet"])
        .stdin(Stdio::piped())
        .spawn()
        .expect("git fast-import");
    let mut stream = Vec::new();
    let mut texts: Vec<Vec<String>> = (0..TEXT_FILES)
        .map(|f| {
            (0..300)
                .map(|l| format!("file {f} line {l:04}: some ordinary source text\n"))
                .collect()
        })
        .collect();
    let mut noise = Noise(0x9e37_79b9_7f4a_7c15);
    let mut blob = Vec::new();
    for commit in 0..COMMITS {
        writeln!(stream, "commit refs/heads/main").expect("write");
        writeln!(
            stream,
            "committer T <t@example.com> {} +0000",
            1_000_000 + commit
        )
        .expect("w");
        writeln!(stream, "data <<EOF\nrevision {commit}\nEOF").expect("write");
        for f in [commit % TEXT_FILES, (commit * 7 + 3) % TEXT_FILES] {
            let lines = &mut texts[f];
            let at = (commit * 31) % lines.len();
            lines[at] = format!("file {f} line {at:04}: edited in revision {commit}\n");
            let body = lines.concat();
            writeln!(stream, "M 100644 inline src/f{f}.txt\ndata {}", body.len()).expect("w");
            stream.extend_from_slice(body.as_bytes());
            stream.push(b'\n');
        }
        blob.clear();
        noise.fill(&mut blob, BINARY_LEN);
        let slot = commit % BINARY_SLOTS;
        writeln!(
            stream,
            "M 100644 inline assets/b{slot}.bin\ndata {}",
            blob.len()
        )
        .expect("w");
        stream.extend_from_slice(&blob);
        stream.push(b'\n');
    }
    import
        .stdin
        .take()
        .expect("stdin")
        .write_all(&stream)
        .expect("feed fast-import");
    assert!(import.wait().expect("fast-import").success());
    run(git(&repo).args(["repack", "-adq"]));
    std::fs::read_dir(repo.join("objects/pack"))
        .expect("pack dir")
        .filter_map(|e| e.ok().map(|e| e.path()))
        .find(|p| p.extension().is_some_and(|x| x == "pack"))
        .expect("repacked pack")
}

/// Peak RSS in KiB of one ingest of `pack` by `tool` (`git` or `grit`).
///
/// A child's `ru_maxrss` starts at the RSS of the process it was forked from, so the
/// command runs under a fresh copy of this test binary ([`rss_probe`]) that reports
/// `getrusage(RUSAGE_CHILDREN)` for it.
fn ingest_peak_rss_kib(tool: &str, pack: &Path, scratch: &Path) -> u64 {
    let out = Command::new(std::env::current_exe().expect("test binary"))
        .args(["rss_probe", "--exact", "--ignored", "--nocapture"])
        .args(["--test-threads=1", "-q"])
        .env(PROBE_TOOL_ENV, tool)
        .env(CHILD_PACK_ENV, pack)
        .env(PROBE_SCRATCH_ENV, scratch)
        .output()
        .expect("spawn rss probe");
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        out.status.success(),
        "{tool} probe failed: {stdout}{}",
        String::from_utf8_lossy(&out.stderr)
    );
    stdout
        .lines()
        .find_map(|line| line.strip_prefix("peak_kib="))
        .and_then(|kib| kib.trim().parse().ok())
        .unwrap_or_else(|| panic!("{tool} probe printed no peak: {stdout}"))
}

#[test]
fn pack_ingest_peak_rss_stays_near_git_index_pack() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let pack = build_medium_pack(tmp.path());
    let pack_len = std::fs::metadata(&pack).expect("pack").len();

    let git_kib = ingest_peak_rss_kib("git", &pack, tmp.path());
    let grit_kib = ingest_peak_rss_kib("grit", &pack, tmp.path());

    let ratio = grit_kib as f64 / git_kib as f64;
    eprintln!(
        "pack {} KiB: git index-pack peak {git_kib} KiB, grit ingest peak {grit_kib} KiB ({ratio:.2}x)",
        pack_len / 1024
    );
    assert!(
        ratio <= MAX_RATIO,
        "grit pack ingest peaked at {grit_kib} KiB, {ratio:.2}x git index-pack's {git_kib} KiB \
         on a {} KiB pack (limit {MAX_RATIO}x)",
        pack_len / 1024
    );
}

/// Measuring side of the gate: run one ingest in a child and print its peak RSS.
#[test]
#[ignore = "run by pack_ingest_peak_rss_stays_near_git_index_pack in a child process"]
fn rss_probe() {
    let (Some(tool), Some(pack), Some(scratch)) = (
        std::env::var_os(PROBE_TOOL_ENV),
        std::env::var_os(CHILD_PACK_ENV),
        std::env::var_os(PROBE_SCRATCH_ENV),
    ) else {
        return;
    };
    let scratch = tempfile::tempdir_in(scratch).expect("probe scratch");
    let mut cmd = if tool == "git" {
        let mut cmd = git(scratch.path());
        cmd.arg("index-pack")
            .arg("-o")
            .arg(scratch.path().join("in.idx"))
            .arg(&pack);
        cmd
    } else {
        let mut cmd = Command::new(std::env::current_exe().expect("test binary"));
        cmd.args(["ingest_child", "--exact", "--ignored", "--test-threads=1"])
            .env(CHILD_PACK_ENV, &pack);
        cmd
    };
    run(cmd.stdout(Stdio::null()));
    // SAFETY: plain FFI call filling a zeroed, correctly sized `rusage`.
    let mut usage: libc::rusage = unsafe { std::mem::zeroed() };
    assert_eq!(
        unsafe { libc::getrusage(libc::RUSAGE_CHILDREN, &mut usage) },
        0
    );
    println!("peak_kib={}", usage.ru_maxrss);
}

/// Child side of the gate: ingest the pack named by `GRIT_INGEST_MEMORY_CHILD_PACK` the
/// way fetch and clone do.
#[test]
#[ignore = "run by pack_ingest_peak_rss_stays_near_git_index_pack in a child process"]
fn ingest_child() {
    let Some(pack) = std::env::var_os(CHILD_PACK_ENV) else {
        return;
    };
    let tmp = tempfile::tempdir().expect("tempdir");
    let objects = tmp.path().join("objects");
    std::fs::create_dir_all(objects.join("pack")).expect("objects");
    let staging = objects.join("pack/incoming.pack");
    std::fs::copy(&pack, &staging).expect("copy pack");
    let odb = Odb::new(&objects);
    let ingested = install_pack_path(
        &staging,
        &odb,
        &IngestPackOptions {
            fix_thin: true,
            skip_post_index_verify: true,
            ..Default::default()
        },
    )
    .expect("install pack");
    assert!(!ingested.object_ids.is_empty());
}

//! Issue #2373: peak RSS guard for parallel worktree stat scan on a 20k-file repo.
//!
//! Run after building the CLI:
//! `cargo build --release -p grit-cli -p grit-utils && GRIT_STATUS_MEMORY=1 cargo test -p grit-lib --test status_scan_memory --release`

use std::process::Command;

const FILE_COUNT: usize = 20_000;
/// Observed peak RSS ~42 MiB on factory Linux after sparse parallel preload (2026-10-20).
/// Cap leaves headroom for CI variance while catching reintroduction of full-index slot vectors.
const MAX_STATUS_PEAK_RSS_BYTES: u64 = 48 * 1024 * 1024;

fn grit_binary() -> std::path::PathBuf {
    if let Ok(p) = std::env::var("GRIT_BIN") {
        let path = std::path::PathBuf::from(p);
        if path.is_file() {
            return path;
        }
    }
    let manifest = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let workspace = manifest.parent().expect("workspace root");
    let target_dir = std::env::var("CARGO_TARGET_DIR")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|_| workspace.join("target"));
    let release = target_dir.join("release/grit");
    assert!(
        release.is_file(),
        "grit binary not found at {} — run `cargo build --release -p grit-cli` or set GRIT_BIN",
        release.display()
    );
    release
}

fn grit_bench_binary() -> std::path::PathBuf {
    let manifest = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let workspace = manifest.parent().expect("workspace root");
    let target_dir = std::env::var("CARGO_TARGET_DIR")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|_| workspace.join("target"));
    let release = target_dir.join("release/grit-bench");
    assert!(
        release.is_file(),
        "grit-bench not found at {} — run `cargo build --release -p grit-utils`",
        release.display()
    );
    release
}

fn build_fixture(root: &std::path::Path) {
    grit_test_support::git(root, &["init", "-q", "-b", "main", "."]);
    grit_test_support::git(root, &["config", "user.email", "t@example.com"]);
    grit_test_support::git(root, &["config", "user.name", "Test"]);

    let mut script = String::from("import os,random\nrandom.seed(2373)\nroot=r'''");
    script.push_str(&root.to_string_lossy());
    script.push_str("'''\n");
    script.push_str(&format!(
        "for i in range({FILE_COUNT}):\n depth=random.randint(3,8)\n p='/'.join([f'd{{random.randint(0,6)}}' for _ in range(depth)])+f'/f{{i}}.txt'\n path=os.path.join(root,p); os.makedirs(os.path.dirname(path),exist_ok=True); open(path,'w').write(f'c{{i}}\\n')\n"
    ));
    std::process::Command::new("python3")
        .arg("-c")
        .arg(&script)
        .status()
        .expect("python fixture");

    grit_test_support::git(root, &["add", "-A"]);
    grit_test_support::git(root, &["commit", "-qm", "base"]);
}

fn peak_rss_status(root: &std::path::Path, grit: &std::path::Path, grit_bench: &std::path::Path) -> u64 {
    let cmd = format!("{} status", grit.display());
    let out = Command::new(grit_bench)
        .arg("measure-rss")
        .arg("--cwd")
        .arg(root)
        .env("GRIT_BENCH_MEASURE_CMD", cmd)
        .output()
        .expect("measure-rss");
    assert!(
        out.status.success(),
        "measure-rss failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout)
        .trim()
        .parse::<u64>()
        .expect("peak RSS bytes")
}

#[test]
fn status_peak_rss_bounded_on_20k_file_repo() {
    if std::env::var("GRIT_STATUS_MEMORY").ok().as_deref() != Some("1") {
        eprintln!("skip status_scan_memory (set GRIT_STATUS_MEMORY=1)");
        return;
    }

    let tmp = tempfile::TempDir::new().expect("tempdir");
    build_fixture(tmp.path());
    let grit = grit_binary();
    let grit_bench = grit_bench_binary();
    let peak = peak_rss_status(tmp.path(), &grit, &grit_bench);
    eprintln!("grit status peak RSS on {FILE_COUNT} files: {peak} bytes");
    assert!(
        peak <= MAX_STATUS_PEAK_RSS_BYTES,
        "peak RSS {peak} exceeded bound {MAX_STATUS_PEAK_RSS_BYTES}"
    );
}

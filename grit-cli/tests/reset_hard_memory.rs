//! Hard reset at HEAD must not retain multiple copies of a wide index.

#![cfg(target_os = "linux")]
#![allow(clippy::expect_used)]

use std::fs;
use std::io::Write;
use std::path::Path;
use std::process::{Command, Stdio};

use grit_test_support::configure_git_env;

fn peak_kib(root: &Path, program: &str, args: &[&str], git_env: bool) -> u64 {
    let mut cmd = Command::new(program);
    if git_env {
        configure_git_env(&mut cmd);
    }
    let mut child = cmd
        .current_dir(root)
        .args(args)
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn");
    let stderr = child.stderr.take();
    let pid = child.id();
    let mut peak_kib = 0u64;
    loop {
        if let Ok(status) = fs::read_to_string(format!("/proc/{pid}/status")) {
            for line in status.lines() {
                if let Some(kib) = line.strip_prefix("VmHWM:") {
                    let kib = kib
                        .trim()
                        .trim_end_matches(" kB")
                        .parse::<u64>()
                        .unwrap_or(0);
                    peak_kib = peak_kib.max(kib);
                }
            }
        }
        match child.try_wait() {
            Ok(None) => std::thread::sleep(std::time::Duration::from_millis(5)),
            Ok(Some(status)) => {
                let mut err_text = String::new();
                if let Some(mut err) = stderr {
                    let _ = std::io::Read::read_to_string(&mut err, &mut err_text);
                }
                assert!(
                    status.success(),
                    "{program} {args:?} failed with {status}: {err_text}"
                );
                break;
            }
            Err(err) => panic!("wait for {program} {args:?}: {err}"),
        }
    }
    peak_kib
}

fn git_in(root: &Path) -> Command {
    let mut cmd = Command::new("git");
    configure_git_env(&mut cmd);
    cmd.current_dir(root);
    cmd
}

fn wide_repo(root: &Path, files: usize) {
    for d in 0..files / 100 {
        let dir = root.join(format!(
            "top{}/mid{}/pkg{}/leaf{}",
            d % 30,
            (d / 30) % 10,
            d / 300,
            d
        ));
        fs::create_dir_all(&dir).expect("mkdir");
        for f in 0..100 {
            let path = dir.join(format!("f{f}.txt"));
            let mut out = fs::File::create(&path).expect("create");
            for l in 0..3 + f % 20 {
                writeln!(out, "line {l} of {d}/{f}").expect("write");
            }
        }
    }
    git_in(root)
        .args(["init", "-q", "-b", "master"])
        .status()
        .expect("init");
    git_in(root)
        .args(["config", "user.name", "T"])
        .status()
        .expect("name");
    git_in(root)
        .args(["config", "user.email", "t@e.x"])
        .status()
        .expect("email");
    git_in(root).args(["add", "-A"]).status().expect("add");
    git_in(root)
        .args(["commit", "-qm", "base"])
        .status()
        .expect("commit");
    git_in(root)
        .args(["update-index", "-q", "--refresh"])
        .status()
        .expect("refresh");
    assert!(
        git_in(root)
            .args(["rev-parse", "HEAD"])
            .status()
            .expect("rev-parse")
            .success(),
        "fixture repo must have a commit before reset"
    );
}

#[test]
fn hard_reset_at_head_peak_rss_scales_like_git_on_wide_tree() {
    let small_git_root = tempfile::tempdir().expect("small git");
    wide_repo(small_git_root.path(), 2_000);
    let wide_git_root = tempfile::tempdir().expect("wide git");
    wide_repo(wide_git_root.path(), 10_000);
    let small_grit_root = tempfile::tempdir().expect("small grit");
    wide_repo(small_grit_root.path(), 2_000);
    let wide_grit_root = tempfile::tempdir().expect("wide grit");
    wide_repo(wide_grit_root.path(), 10_000);

    let grit = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../target/release/grit");
    assert!(
        grit.is_file(),
        "build release grit first: cargo build --release -p grit-cli"
    );
    let grit = grit.to_str().expect("utf-8 path");
    let small_git = peak_kib(
        small_git_root.path(),
        "git",
        &["reset", "--hard", "-q"],
        true,
    );
    let wide_git = peak_kib(
        wide_git_root.path(),
        "git",
        &["reset", "--hard", "-q"],
        true,
    );
    let small_grit = peak_kib(small_grit_root.path(), grit, &["reset", "--hard"], false);
    let wide_grit = peak_kib(wide_grit_root.path(), grit, &["reset", "--hard"], false);

    let git_growth = wide_git.saturating_sub(small_git);
    let grit_growth = wide_grit.saturating_sub(small_grit);
    eprintln!(
        "git {small_git} -> {wide_git} KiB (+{git_growth}); grit {small_grit} -> {wide_grit} KiB (+{grit_growth})"
    );
    assert!(
        grit_growth <= git_growth * 3 / 2 + 8 * 1024,
        "grit growth {grit_growth} KiB vs git {git_growth} KiB"
    );
    if !cfg!(debug_assertions) {
        assert!(
            wide_grit <= wide_git * 3 / 2 + 8 * 1024,
            "grit {wide_grit} KiB vs git {wide_git} KiB on 30k files"
        );
    }
}

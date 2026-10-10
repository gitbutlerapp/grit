//! Time budget for local-copy pack building with on-disk delta reuse.

use std::path::Path;
use std::process::Command;
use std::time::Instant;

use grit_lib::objects::ObjectId;
use grit_lib::odb::Odb;
use grit_lib::transfer::{build_pack, PackBuildOptions};

fn git(dir: &Path, args: &[&str]) {
    let out = Command::new("git")
        .current_dir(dir)
        .args(args)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .output()
        .expect("git");
    assert!(
        out.status.success(),
        "git {:?}: {}",
        args,
        String::from_utf8_lossy(&out.stderr)
    );
}

fn open_odb(git_dir: &Path) -> Odb {
    Odb::new(&git_dir.join("objects")).with_config_git_dir(git_dir.to_path_buf())
}

/// Repacked repo with on-disk deltas; `for_local_copy` must stay within a CI-friendly budget.
#[test]
fn for_local_copy_build_pack_reuse_bounded_on_repacked_fixture() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let dir = tmp.path();
    git(dir, &["init", "-q", "-b", "main", "."]);
    let mut body = String::from("shared prefix\n");
    for i in 0..80 {
        body.push_str(&format!("rev-{i}\n"));
        body.push_str(&"x".repeat(2048));
        std::fs::write(dir.join("blob.txt"), &body).unwrap();
        git(dir, &["add", "blob.txt"]);
        git(dir, &["commit", "-qm", &format!("c{i}")]);
    }
    git(
        dir,
        &[
            "repack",
            "-q",
            "-a",
            "-d",
            "-f",
            "--window=10",
            "--depth=50",
        ],
    );
    let git_dir = dir.join(".git");
    let rev_out = Command::new("git")
        .current_dir(dir)
        .args(["rev-parse", "HEAD"])
        .output()
        .expect("rev-parse");
    let tip_hex = String::from_utf8_lossy(&rev_out.stdout);
    let tip = ObjectId::from_hex(tip_hex.trim()).expect("tip");
    let odb = open_odb(&git_dir);
    let opts = PackBuildOptions::for_local_copy(None);

    let t0 = Instant::now();
    let pack = build_pack(&odb, &[tip], &[], &opts).expect("build_pack");
    let elapsed = t0.elapsed().as_secs_f64();

    assert!(
        pack.len() > 512,
        "expected a non-trivial delta pack, got {} bytes",
        pack.len()
    );
    assert!(
        elapsed < 30.0,
        "for_local_copy pack build took {elapsed:.2}s (budget 30s)"
    );
}

/// When `/tmp/cargo.mirror` exists, full local-copy pack build must stay near grit main, not 8× slower.
///
/// Budget is calibrated for release builds; debug builds can exceed it on large mirrors.
#[test]
#[cfg_attr(debug_assertions, ignore = "release-only: cargo mirror perf budget")]
fn cargo_mirror_for_local_copy_build_time_when_fixture_present() {
    let mirror = Path::new("/tmp/cargo.mirror");
    if !mirror.join("objects/pack").is_dir() {
        return;
    }
    let rev_out = Command::new("git")
        .current_dir(mirror)
        .args(["rev-parse", "refs/heads/master"])
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .output()
        .expect("rev-parse");
    let tip_hex = String::from_utf8_lossy(&rev_out.stdout);
    let tip = ObjectId::from_hex(tip_hex.trim()).expect("tip");
    let odb = open_odb(mirror);
    let opts = PackBuildOptions::for_local_copy(
        grit_lib::config::ConfigSet::load(
            &grit_lib::environment::Environment::empty(),
            Some(mirror),
            true,
        )
        .ok()
        .as_ref(),
    );

    let t0 = Instant::now();
    let pack = build_pack(&odb, &[tip], &[], &opts).expect("build_pack");
    let elapsed = t0.elapsed().as_secs_f64();

    assert!(
        pack.len() > 50_000_000,
        "expected compact cargo-mirror pack, got {} bytes",
        pack.len()
    );
    assert!(
        elapsed < 240.0,
        "cargo-mirror for_local_copy pack build took {elapsed:.2}s (budget 240s; origin/main ~77s, prior branch ~670s)"
    );
}

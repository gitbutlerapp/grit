//! Real-world pack size/delta regression for rust-lang/cargo mirror (manual/CI optional).
//!
//! Requires `/tmp/cargo.mirror` from:
//! `git clone -q --mirror https://github.com/rust-lang/cargo /tmp/cargo.mirror`

use std::path::Path;
use std::process::Command;
use std::time::Instant;

use grit_lib::objects::ObjectId;
use grit_lib::odb::Odb;
use grit_lib::transfer::{build_pack, PackBuildOptions};
use grit_lib::unpack_objects::pack_bytes_to_object_map;

fn open_odb(git_dir: &Path) -> Odb {
    Odb::new(&git_dir.join("objects")).with_config_git_dir(git_dir.to_path_buf())
}

fn git(dir: &Path, args: &[&str]) -> String {
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
    String::from_utf8_lossy(&out.stdout).into_owned()
}

fn verify_pack_delta_count(pack_bytes: &[u8]) -> usize {
    let scratch = tempfile::tempdir().expect("scratch");
    let pack_path = scratch.path().join("p.pack");
    std::fs::write(&pack_path, pack_bytes).unwrap();
    let idx_out = Command::new("git")
        .current_dir(scratch.path())
        .args(["index-pack", "p.pack"])
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .output()
        .expect("index-pack");
    assert!(
        idx_out.status.success(),
        "index-pack failed: {}",
        String::from_utf8_lossy(&idx_out.stderr)
    );
    let idx_path = std::fs::read_dir(scratch.path())
        .expect("read dir")
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .find(|p| p.extension().is_some_and(|x| x == "idx"))
        .expect("idx beside pack");
    let out = Command::new("git")
        .args(["verify-pack", "-v"])
        .arg(&idx_path)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .output()
        .expect("verify-pack");
    assert!(
        out.status.success(),
        "verify-pack failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter(|line| line.split_whitespace().count() >= 7)
        .count()
}

fn git_pack_objects_size(repo: &Path, tip: ObjectId) -> (usize, usize) {
    use std::io::Write;
    let mut child = Command::new("git")
        .current_dir(repo)
        .args(["pack-objects", "--stdout", "--revs", "--delta-base-offset"])
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .spawn()
        .expect("pack-objects");
    {
        let mut stdin = child.stdin.take().expect("stdin");
        writeln!(stdin, "{}", tip.to_hex()).unwrap();
    }
    let out = child.wait_with_output().expect("wait");
    assert!(out.status.success());
    let deltas = if out.stdout.len() >= 12 {
        verify_pack_delta_count(&out.stdout)
    } else {
        0
    };
    (out.stdout.len(), deltas)
}

#[test]
#[ignore = "manual: requires /tmp/cargo.mirror (see module docs)"]
fn cargo_mirror_for_local_copy_options_resolve() {
    let mirror = Path::new("/tmp/cargo.mirror");
    if !mirror.join("objects/pack").is_dir() {
        return;
    }
    let tip = ObjectId::from_hex(git(mirror, &["rev-parse", "refs/heads/master"]).trim()).unwrap();
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
    let pack = build_pack(&odb, &[tip], &[], &opts).expect("build_pack");
    let empty_git = tempfile::tempdir().expect("empty git dir");
    std::fs::create_dir_all(empty_git.path().join("objects")).unwrap();
    let empty_odb = Odb::new(&empty_git.path().join("objects"));
    pack_bytes_to_object_map(&pack, &empty_odb).expect("for_local_copy pack must resolve");
}

#[test]
#[ignore = "manual: requires /tmp/cargo.mirror (see module docs)"]
fn cargo_mirror_local_copy_pack_matches_git_delta_density() {
    let mirror = Path::new("/tmp/cargo.mirror");
    if !mirror.join("objects/pack").is_dir() {
        return;
    }
    let tip = ObjectId::from_hex(git(mirror, &["rev-parse", "refs/heads/master"]).trim()).unwrap();
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
    let grit_build_secs = t0.elapsed().as_secs_f64();

    let empty_git = tempfile::tempdir().expect("empty git dir");
    std::fs::create_dir_all(empty_git.path().join("objects")).unwrap();
    let empty_odb = Odb::new(&empty_git.path().join("objects"));
    pack_bytes_to_object_map(&pack, &empty_odb).expect("pack must resolve without external odb");

    let grit_deltas = verify_pack_delta_count(&pack);
    let grit_bytes = pack.len();

    let (git_bytes, git_deltas) = git_pack_objects_size(mirror, tip);

    assert!(
        grit_deltas > git_deltas / 2,
        "grit pack should reuse most on-disk deltas (grit={grit_deltas}, git={git_deltas})"
    );
    assert!(
        grit_bytes <= git_bytes + git_bytes / 10,
        "grit pack size should be within 10% of git pack-objects (grit={grit_bytes}, git={git_bytes}, build={grit_build_secs:.1}s)"
    );
}

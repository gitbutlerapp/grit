//! Issue #2251: packs installed by grit receive-pack are read-only like Git's.

use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::process::Command;

type TestResult = Result<(), Box<dyn std::error::Error>>;

const GRIT: &str = env!("CARGO_BIN_EXE_grit");

fn git(dir: &Path, args: &[&str]) -> TestResult {
    let out = Command::new("git")
        .args(args)
        .current_dir(dir)
        .env("GIT_AUTHOR_NAME", "T")
        .env("GIT_AUTHOR_EMAIL", "t@example.com")
        .env("GIT_COMMITTER_NAME", "T")
        .env("GIT_COMMITTER_EMAIL", "t@example.com")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .output()?;
    if !out.status.success() {
        return Err(format!("git {args:?}: {}", String::from_utf8_lossy(&out.stderr)).into());
    }
    Ok(())
}

fn sidecar_mode(path: &Path) -> u32 {
    std::fs::metadata(path)
        .expect("metadata")
        .permissions()
        .mode()
        & 0o777
}

#[test]
fn git_push_installs_read_only_pack_sidecars_in_shared_bare_repo() -> TestResult {
    let tmp = tempfile::tempdir()?;
    let src = tmp.path().join("src");
    std::fs::create_dir_all(&src)?;
    git(&src, &["init", "-q", "-b", "main"])?;
    for i in 1..=120 {
        std::fs::write(src.join(format!("f{i}")), format!("{i}\n"))?;
    }
    git(&src, &["add", "-A"])?;
    git(&src, &["commit", "-qm", "one"])?;

    let dst = tmp.path().join("dst.git");
    std::fs::create_dir_all(&dst)?;
    git(&dst, &["init", "-q", "--bare", "--shared=group", "."])?;
    let receive_pack = format!("--receive-pack={GRIT} receive-pack");
    git(
        &src,
        &[
            "push",
            "-q",
            receive_pack.as_str(),
            dst.to_str().expect("utf8"),
            "HEAD:refs/heads/main",
        ],
    )?;

    let pack_dir = dst.join("objects/pack");
    let pack_path = std::fs::read_dir(&pack_dir)?
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .find(|p| p.extension().is_some_and(|x| x == "pack"))
        .ok_or("missing .pack")?;
    let idx_path = pack_path.with_extension("idx");
    let rev_path = pack_path.with_extension("rev");
    assert_eq!(sidecar_mode(&pack_path), 0o444);
    assert_eq!(sidecar_mode(&idx_path), 0o444);
    assert!(rev_path.is_file(), "expected .rev sidecar");
    assert_eq!(sidecar_mode(&rev_path), 0o444);
    git(&dst, &["fsck", "--strict", "--no-progress"])?;
    Ok(())
}

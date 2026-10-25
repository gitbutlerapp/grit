//! Thin packs received over the network are completed with their missing bases before being
//! installed. The installed pack must still be readable by the system `git`: its `.idx` and
//! file name have to describe the completed pack, not the one that came off the wire.
//!
//! Covers `grit fetch` from `git daemon` (Git's upload-pack sends thin packs) and a thin
//! `git push` into `grit receive-pack` kept as a pack.

use std::net::{SocketAddr, TcpListener, TcpStream};
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

type TestResult = Result<(), Box<dyn std::error::Error>>;

const GRIT: &str = env!("CARGO_BIN_EXE_grit");

fn isolated<'a>(cmd: &'a mut Command, dir: &Path) -> &'a mut Command {
    cmd.current_dir(dir)
        .env("GIT_AUTHOR_NAME", "T")
        .env("GIT_AUTHOR_EMAIL", "t@example.com")
        .env("GIT_COMMITTER_NAME", "T")
        .env("GIT_COMMITTER_EMAIL", "t@example.com")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
}

fn run(program: &str, dir: &Path, args: &[&str]) -> Result<String, Box<dyn std::error::Error>> {
    let out = isolated(&mut Command::new(program), dir)
        .args(args)
        .output()?;
    if !out.status.success() {
        return Err(format!(
            "{program} {args:?} failed: {}",
            String::from_utf8_lossy(&out.stderr)
        )
        .into());
    }
    Ok(String::from_utf8(out.stdout)?.trim().to_owned())
}

fn git(dir: &Path, args: &[&str]) -> Result<String, Box<dyn std::error::Error>> {
    run("git", dir, args)
}

fn grit(dir: &Path, args: &[&str]) -> Result<String, Box<dyn std::error::Error>> {
    run(GRIT, dir, args)
}

/// Kills the daemon when the test ends, pass or fail.
struct Daemon(Child);

impl Drop for Daemon {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn start_daemon(base: &Path) -> Result<(Daemon, u16), Box<dyn std::error::Error>> {
    let port = TcpListener::bind(("127.0.0.1", 0))?.local_addr()?.port();
    let child = isolated(&mut Command::new("git"), base)
        .arg("daemon")
        .arg("--listen=127.0.0.1")
        .arg(format!("--port={port}"))
        .arg("--reuseaddr")
        .arg("--export-all")
        .arg(format!("--base-path={}", base.display()))
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()?;
    let daemon = Daemon(child);
    let addr = SocketAddr::from(([127, 0, 0, 1], port));
    let deadline = Instant::now() + Duration::from_secs(10);
    while TcpStream::connect_timeout(&addr, Duration::from_millis(200)).is_err() {
        if Instant::now() > deadline {
            return Err("git daemon did not start".into());
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    Ok((daemon, port))
}

/// A ~100-line text file plus a 2 MiB binary, as in the original report.
fn seed_commit(work: &Path) -> TestResult {
    std::fs::create_dir_all(work.join("f1"))?;
    let text: String = (0..100)
        .map(|i| format!("harbor line {i:03} with some text to delta against\n"))
        .collect();
    std::fs::write(work.join("f1/harbor.txt"), text)?;
    let binary: Vec<u8> = (0..2 * 1024 * 1024u32)
        .map(|i| (i.wrapping_mul(2_654_435_761) >> 24) as u8)
        .collect();
    std::fs::write(work.join("blob.bin"), binary)?;
    git(work, &["add", "-A"])?;
    git(work, &["commit", "-q", "-m", "seed"])?;
    Ok(())
}

fn append_line(work: &Path, msg: &str) -> TestResult {
    let path = work.join("f1/harbor.txt");
    let mut text = std::fs::read_to_string(&path)?;
    text.push_str(msg);
    text.push('\n');
    std::fs::write(&path, text)?;
    git(work, &["commit", "-q", "-am", msg])?;
    Ok(())
}

fn pack_count(git_dir: &Path) -> Result<usize, Box<dyn std::error::Error>> {
    Ok(std::fs::read_dir(git_dir.join("objects/pack"))?
        .filter_map(|e| e.ok())
        .filter(|e| e.path().extension().is_some_and(|x| x == "pack"))
        .count())
}

/// Issue #1844: `grit fetch` of a one-line edit over `git://` receives a thin pack; afterwards
/// `git fsck --strict`, `git verify-pack` and `git log` must accept the repository.
#[test]
fn grit_fetch_of_thin_update_from_git_daemon_is_fsck_clean() -> TestResult {
    let tmp = tempfile::tempdir()?;
    let srv = tmp.path().join("srv");
    let bare = srv.join("proj.git");
    std::fs::create_dir_all(&bare)?;
    git(&bare, &["init", "-q", "--bare", "-b", "main", "."])?;
    let seed = tmp.path().join("seed");
    std::fs::create_dir_all(&seed)?;
    git(&seed, &["init", "-q", "-b", "main", "."])?;
    seed_commit(&seed)?;
    git(&seed, &["push", "-q", bare.to_str().ok_or("path")?, "main"])?;

    let (_daemon, port) = start_daemon(&srv)?;
    let url = format!("git://127.0.0.1:{port}/proj.git");
    grit(tmp.path(), &["clone", &url, "gc2"])?;
    let clone = tmp.path().join("gc2");
    git(&clone, &["fsck", "--strict", "--no-progress"])?;
    let packs_after_clone = pack_count(&clone.join(".git"))?;

    for edit in ["edit1", "edit2"] {
        append_line(&seed, edit)?;
        git(&seed, &["push", "-q", bare.to_str().ok_or("path")?, "main"])?;
        grit(&clone, &["fetch", "origin"])?;
        git(&clone, &["fsck", "--strict", "--no-progress"])?;
        assert_eq!(
            git(&clone, &["log", "-1", "--format=%s", "origin/main"])?,
            edit
        );
        assert_eq!(
            git(&clone, &["rev-parse", "origin/main"])?,
            git(&seed, &["rev-parse", "HEAD"])?
        );
    }
    assert!(
        pack_count(&clone.join(".git"))? > packs_after_clone,
        "the fetches must have installed packs for this test to cover pack ingest"
    );
    for entry in std::fs::read_dir(clone.join(".git/objects/pack"))? {
        let path = entry?.path();
        if path.extension().is_some_and(|x| x == "idx") {
            git(
                &clone,
                &["verify-pack", path.to_str().ok_or("non-utf8 idx path")?],
            )?;
        }
    }
    Ok(())
}

/// The push side of #1844: a thin `git push` into `grit receive-pack`, kept as a pack
/// (`receive.unpackLimit=1`), must leave the remote readable by `git`.
#[test]
fn thin_git_push_into_grit_receive_pack_kept_as_pack_is_fsck_clean() -> TestResult {
    let tmp = tempfile::tempdir()?;
    let source = tmp.path().join("src");
    std::fs::create_dir_all(&source)?;
    git(&source, &["init", "-q", "-b", "main", "."])?;
    seed_commit(&source)?;
    let remote = tmp.path().join("remote.git");
    std::fs::create_dir_all(&remote)?;
    git(&remote, &["init", "-q", "--bare", "-b", "main", "."])?;
    git(&remote, &["config", "receive.unpackLimit", "1"])?;
    let receive_pack = format!("--receive-pack={GRIT} receive-pack");
    let remote_arg = remote.to_str().ok_or("non-utf8 path")?;
    git(
        &source,
        &["push", "-q", "--thin", &receive_pack, remote_arg, "main"],
    )?;
    let packs_before = pack_count(&remote)?;

    append_line(&source, "edit1")?;
    git(
        &source,
        &["push", "-q", "--thin", &receive_pack, remote_arg, "main"],
    )?;
    assert_eq!(
        pack_count(&remote)?,
        packs_before + 1,
        "receive.unpackLimit=1 keeps the thin push as a pack"
    );
    git(&remote, &["fsck", "--strict", "--no-progress"])?;
    assert_eq!(
        git(&remote, &["log", "-1", "--format=%s", "main"])?,
        "edit1"
    );
    Ok(())
}

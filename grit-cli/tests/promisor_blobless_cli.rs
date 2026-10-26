//! CLI commands in a blobless clone with the default promisor fetcher installed.

use std::fs;
use std::path::Path;
use std::process::Command;

type TestResult = Result<(), Box<dyn std::error::Error>>;

const GRIT: &str = env!("CARGO_BIN_EXE_grit");

fn git_in(dir: &Path, args: &[&str]) -> TestResult {
    let status = Command::new("git")
        .args(args)
        .current_dir(dir)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .env("GIT_AUTHOR_NAME", "T")
        .env("GIT_AUTHOR_EMAIL", "t@example.com")
        .env("GIT_COMMITTER_NAME", "T")
        .env("GIT_COMMITTER_EMAIL", "t@example.com")
        .status()?;
    assert!(status.success(), "git {:?} in {}", args, dir.display());
    Ok(())
}

fn grit_in(dir: &Path, args: &[&str]) -> TestResult {
    let out = Command::new(GRIT)
        .args(args)
        .current_dir(dir)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .output()?;
    assert!(
        out.status.success(),
        "grit {:?} in {}: status={} stderr={}",
        args,
        dir.display(),
        out.status,
        String::from_utf8_lossy(&out.stderr)
    );
    Ok(())
}

#[test]
fn blobless_clone_grit_commands_fetch_on_demand() -> TestResult {
    let layout = tempfile::tempdir()?;
    let upstream = layout.path().join("upstream");
    git_in(layout.path(), &["init", "-q", "-b", "main", "upstream"])?;
    git_in(&upstream, &["config", "user.email", "t@t"])?;
    fs::write(upstream.join("one.txt"), b"one\n")?;
    git_in(&upstream, &["add", "one.txt"])?;
    git_in(&upstream, &["commit", "-m", "one", "-q"])?;
    fs::write(upstream.join("two.txt"), b"two\n")?;
    git_in(&upstream, &["add", "two.txt"])?;
    git_in(&upstream, &["commit", "-m", "two", "-q"])?;
    git_in(&upstream, &["branch", "other"])?;
    fs::write(upstream.join("other.txt"), b"other\n")?;
    git_in(&upstream, &["add", "other.txt"])?;
    git_in(&upstream, &["commit", "-m", "other", "-q"])?;
    git_in(&upstream, &["switch", "main"])?;

    let bare = layout.path().join("remote.git");
    git_in(
        layout.path(),
        &[
            "clone",
            "-q",
            "--bare",
            upstream.to_str().unwrap(),
            "remote.git",
        ],
    )?;
    git_in(&bare, &["config", "uploadpack.allowFilter", "true"])?;

    let url = format!("file://{}", bare.canonicalize()?.display());
    let clone = layout.path().join("blobless");
    git_in(
        layout.path(),
        &[
            "clone",
            "-q",
            "--filter=blob:none",
            "--no-local",
            &url,
            clone.to_str().unwrap(),
        ],
    )?;

    grit_in(&clone, &["diff", "HEAD~1"])?;
    grit_in(&clone, &["show", "HEAD:one.txt"])?;
    grit_in(&clone, &["switch", "other"])?;
    Ok(())
}

fn pack_count(repo: &Path) -> usize {
    fs::read_dir(repo.join(".git/objects/pack"))
        .unwrap()
        .flatten()
        .filter(|entry| entry.path().extension().is_some_and(|ext| ext == "pack"))
        .count()
}

fn check_blobless_reset(root: &Path, url: &str, log: Option<&Path>) -> TestResult {
    let dest = root.join("client");
    grit_in(
        root,
        &[
            "clone",
            "--blobs",
            "none",
            "--no-checkout",
            url,
            dest.to_str().unwrap(),
        ],
    )?;
    assert_eq!(pack_count(&dest), 1);
    if let Some(log) = log {
        fs::write(log, "")?;
    }
    grit_in(&dest, &["reset", "--hard", "HEAD", "--json"])?;
    assert_eq!(pack_count(&dest), 2, "reset must fetch a single data pack");
    if let Some(log) = log {
        let headers = fs::read_to_string(log)?;
        assert_eq!(
            headers
                .lines()
                .filter(|line| line.starts_with("host: "))
                .count(),
            2,
            "one discovery and one data request: {headers}"
        );
        fs::write(log, "")?;
    }
    for i in 1..=200 {
        assert_eq!(
            fs::read(dest.join(format!("file-{i}.txt")))?,
            format!("file {i}\n{}", "build payload\n".repeat(100)).as_bytes()
        );
    }
    let out = Command::new("git")
        .args(["status", "--porcelain"])
        .current_dir(&dest)
        .output()?;
    assert!(out.status.success());
    assert!(
        out.stdout.is_empty(),
        "{}",
        String::from_utf8_lossy(&out.stdout)
    );
    git_in(&dest, &["fsck", "--strict"])?;
    grit_in(&dest, &["reset", "--hard", "HEAD", "--markdown"])?;
    assert_eq!(pack_count(&dest), 2);
    if let Some(log) = log {
        assert!(fs::read_to_string(log)?.is_empty());
    }
    Ok(())
}

fn reset_source(root: &Path) -> TestResult {
    git_in(root, &["init", "-q", "-b", "main", "source"])?;
    let source = root.join("source");
    for i in 1..=200 {
        fs::write(
            source.join(format!("file-{i}.txt")),
            format!("file {i}\n{}", "build payload\n".repeat(100)),
        )?;
    }
    git_in(&source, &["add", "."])?;
    git_in(&source, &["commit", "-qm", "inputs"])?;
    git_in(&source, &["config", "uploadpack.allowFilter", "true"])?;
    Ok(())
}

#[test]
fn no_checkout_blobless_clone_then_hard_reset_batches_fetch() -> TestResult {
    let root = tempfile::tempdir()?;
    reset_source(root.path())?;
    check_blobless_reset(
        root.path(),
        &format!("file://{}", root.path().join("source").display()),
        None,
    )
}

#[test]
fn no_checkout_blobless_http_reset_uses_two_requests() -> TestResult {
    use std::net::{TcpListener, TcpStream};
    use std::process::Stdio;
    use std::time::{Duration, Instant};
    let server = Path::new(GRIT)
        .parent()
        .unwrap()
        .join(format!("grit-http-server{}", std::env::consts::EXE_SUFFIX));
    if !server.exists() {
        eprintln!("SKIP: build grit-http-server first");
        return Ok(());
    }
    let root = tempfile::tempdir()?;
    reset_source(root.path())?;
    git_in(root.path(), &["clone", "--bare", "source", "source.git"])?;
    git_in(
        &root.path().join("source.git"),
        &["config", "uploadpack.allowFilter", "true"],
    )?;
    let listener = TcpListener::bind("127.0.0.1:0")?;
    let addr = listener.local_addr()?;
    drop(listener);
    let log = root.path().join("headers.log");
    struct Server(std::process::Child);
    impl Drop for Server {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }
    let _server = Server(
        Command::new(server)
            .arg("--root")
            .arg(root.path())
            .arg("--bind")
            .arg(addr.to_string())
            .arg("--log-headers")
            .arg(&log)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()?,
    );
    let deadline = Instant::now() + Duration::from_secs(10);
    while TcpStream::connect(addr).is_err() {
        assert!(Instant::now() < deadline, "HTTP server did not start");
        std::thread::sleep(Duration::from_millis(25));
    }
    check_blobless_reset(
        root.path(),
        &format!("http://{addr}/source.git"),
        Some(&log),
    )
}

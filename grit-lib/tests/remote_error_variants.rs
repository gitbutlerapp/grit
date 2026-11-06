//! Named interop checks for typed [`RemoteError`], [`BundleError`], and [`CloneError`]
//! (roadmap step 11). Assertions match on variants, not message text.

use std::process::Command;

use grit_lib::bundle::{Bundle, BundleError};
use grit_lib::config::environment::RepositoryOptions;
use grit_lib::error::{Error, RemoteUnreadableReason};
use grit_lib::progress::NeverCancel;
use grit_lib::remote::clone::{clone, CloneError, CloneOptions};
use grit_lib::remote::fetch::NoProgress;
use grit_lib::remote::transfer::{FetchDepth, FetchOptions};
use grit_lib::remote::{ListRefsOptions, Remote, RemoteError};
use grit_lib::repo::{InitOptions, Repository};

fn init_repo(dir: &std::path::Path) -> Repository {
    Repository::init(dir, &InitOptions::default())
        .expect("init")
        .0
}

fn system_git_available() -> bool {
    Command::new("git")
        .arg("--version")
        .status()
        .is_ok_and(|s| s.success())
}

#[test]
fn remote_error_variants() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let missing = tmp.path().join("nope.git");
    let url = format!("file://{}", missing.display());
    let remote = Remote::from_url(&url).expect("parse url");
    let err = remote
        .list_refs(None, &ListRefsOptions::default(), None)
        .expect_err("missing file:// repo");
    assert!(matches!(
        err,
        RemoteError::Unreadable {
            reason: RemoteUnreadableReason::RepositoryNotFound,
            ..
        }
    ));

    let work = init_repo(&tmp.path().join("work"));
    let remote = Remote::from_url(&url).expect("parse url");
    let fetch_err = remote
        .fetch(
            &work,
            FetchOptions::default(),
            &mut NoProgress,
            &NeverCancel,
            None,
        )
        .expect_err("fetch missing");
    assert!(matches!(
        fetch_err,
        RemoteError::Unreadable {
            reason: RemoteUnreadableReason::RepositoryNotFound,
            ..
        }
    ));

    if system_git_available() {
        let src = tmp.path().join("src.git");
        let status = Command::new("git")
            .current_dir(&src.parent().unwrap())
            .args(["init", "-q", "--bare", "-b", "main", src.to_str().unwrap()])
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_SYSTEM", "/dev/null")
            .status();
        if status.is_ok_and(|s| s.success()) {
            let wt = tmp.path().join("full");
            let clone_status = Command::new("git")
                .current_dir(tmp.path())
                .args([
                    "clone",
                    "-q",
                    &format!("file://{}", src.display()),
                    wt.to_str().unwrap(),
                ])
                .env("GIT_CONFIG_GLOBAL", "/dev/null")
                .env("GIT_CONFIG_SYSTEM", "/dev/null")
                .status();
            if clone_status.is_ok_and(|s| s.success()) {
                let wt_repo =
                    Repository::open(&wt.join(".git"), &RepositoryOptions::empty()).expect("open");
                let mut fetch_opts = FetchOptions::new();
                fetch_opts.fetch_depth = Some(FetchDepth::Unshallow);
                let src_url = format!("file://{}", src.display());
                let remote = Remote::from_url(&src_url).expect("url");
                let unshallow_err = remote
                    .fetch(&wt_repo, fetch_opts, &mut NoProgress, &NeverCancel, None)
                    .expect_err("unshallow on complete repo");
                assert!(matches!(
                    unshallow_err,
                    RemoteError::FetchUnshallowNotShallow
                ));
            }
        }
    }

    #[cfg(feature = "http-ureq")]
    {
        use std::net::TcpStream;
        use std::path::PathBuf;
        use std::process::{Child, Command, Stdio};
        use std::time::{Duration, Instant};

        use grit_lib::remote::transport::http::ureq_client::UreqHttpClient;
        use grit_lib::remote::transport::http::{http_client_arc, http_fetch};

        fn find_binary(name: &str) -> Option<PathBuf> {
            let exe = std::env::current_exe().ok()?;
            let deps = exe.parent()?;
            let profile = deps.parent()?;
            for cand in [profile.join(name), deps.join(name)] {
                if cand.is_file() {
                    return Some(cand);
                }
            }
            None
        }

        struct ServerGuard(Child);
        impl Drop for ServerGuard {
            fn drop(&mut self) {
                let _ = self.0.kill();
                let _ = self.0.wait();
            }
        }

        if let Some(server_bin) = find_binary("grit-http-server") {
            let srv_root = tmp.path().join("http-srv");
            let bare = srv_root.join("repo.git");
            std::fs::create_dir_all(&bare).expect("mkdir bare");
            let git_init_ok = Command::new("git")
                .current_dir(&bare)
                .args(["init", "-q", "--bare", "."])
                .env("GIT_CONFIG_GLOBAL", "/dev/null")
                .env("GIT_CONFIG_SYSTEM", "/dev/null")
                .status()
                .is_ok_and(|s| s.success());
            if git_init_ok {
                if let Some(port) = grit_test_support::free_port() {
                    if let Some(child) = Command::new(&server_bin)
                        .arg("--root")
                        .arg(&srv_root)
                        .arg("--bind")
                        .arg(format!("127.0.0.1:{port}"))
                        .arg("--require-auth")
                        .arg("user:pass")
                        .stdout(Stdio::null())
                        .stderr(Stdio::null())
                        .spawn()
                        .ok()
                    {
                        let _guard = ServerGuard(child);
                        let addr = std::net::SocketAddr::from(([127, 0, 0, 1], port));
                        let deadline = Instant::now() + Duration::from_secs(10);
                        while Instant::now() < deadline {
                            if TcpStream::connect_timeout(&addr, Duration::from_millis(200)).is_ok()
                            {
                                break;
                            }
                            std::thread::sleep(Duration::from_millis(50));
                        }
                        let local = init_repo(&tmp.path().join("auth-client"));
                        let url = format!("http://127.0.0.1:{port}/repo.git");
                        let client =
                            http_client_arc(UreqHttpClient::new().with_git_protocol("version=2"));
                        let err = http_fetch(
                            client,
                            local.git_dir(),
                            &url,
                            &mut FetchOptions::new(),
                            &mut NoProgress,
                        )
                        .expect_err("401 without credentials");
                        assert!(matches!(err, Error::Remote(RemoteError::Auth { .. })));
                    }
                }
            }
        } else {
            eprintln!("SKIP: grit-http-server binary not found (401 auth subtest)");
        }
    }
}

#[test]
fn bundle_missing_prerequisites() {
    if !system_git_available() {
        eprintln!("SKIP: system git unavailable");
        return;
    }
    let tmp = tempfile::tempdir().expect("tempdir");
    let src = tmp.path().join("src");
    std::fs::create_dir_all(&src).expect("mkdir");
    for args in [
        &["init", "-q", "-b", "main", "."][..],
        &["commit", "--allow-empty", "-m", "c1"][..],
        &["commit", "--allow-empty", "-m", "c2"][..],
    ] {
        let ok = Command::new("git")
            .current_dir(&src)
            .args(args)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_SYSTEM", "/dev/null")
            .env("GIT_AUTHOR_NAME", "t")
            .env("GIT_AUTHOR_EMAIL", "t@t")
            .env("GIT_COMMITTER_NAME", "t")
            .env("GIT_COMMITTER_EMAIL", "t@t")
            .status()
            .is_ok_and(|s| s.success());
        if !ok {
            eprintln!("SKIP: could not build fixture with git");
            return;
        }
    }
    let c1 = Command::new("git")
        .current_dir(&src)
        .args(["rev-parse", "HEAD~1"])
        .output()
        .expect("rev");
    let c2 = Command::new("git")
        .current_dir(&src)
        .args(["rev-parse", "HEAD"])
        .output()
        .expect("rev");
    let c1 = String::from_utf8_lossy(&c1.stdout).trim().to_owned();
    let c2 = String::from_utf8_lossy(&c2.stdout).trim().to_owned();
    let inc = tmp.path().join("inc.bundle");
    let ok = Command::new("git")
        .current_dir(&src)
        .args([
            "bundle",
            "create",
            inc.to_str().unwrap(),
            &format!("{c1}..{c2}"),
            "main",
        ])
        .status()
        .is_ok_and(|s| s.success());
    if !ok {
        eprintln!("SKIP: git bundle create failed");
        return;
    }
    let empty_git = tmp.path().join("empty.git");
    let ok = Command::new("git")
        .args([
            "init",
            "-q",
            "--bare",
            "-b",
            "main",
            empty_git.to_str().unwrap(),
        ])
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .status()
        .is_ok_and(|s| s.success());
    if !ok {
        eprintln!("SKIP: git init bare failed");
        return;
    }
    let empty = Repository::open(&empty_git, &RepositoryOptions::empty()).expect("open");
    let bundle = Bundle::open(&inc).expect("open bundle");
    let err = bundle.unbundle(&empty).expect_err("missing base");
    assert!(matches!(err, BundleError::MissingPrerequisites { .. }));
}

#[test]
fn clone_error_has_no_library_wrapper() {
    fn classify(e: CloneError) {
        match e {
            CloneError::DestNotEmpty(_)
            | CloneError::RemoteRefNotFound(_)
            | CloneError::InvalidCloneDepth
            | CloneError::Nested(_)
            | CloneError::Remote(_)
            | CloneError::CheckoutFailed { .. } => {}
        }
    }
    classify(CloneError::Remote(RemoteError::FetchUnshallowNotShallow));
    let tmp = tempfile::tempdir().expect("tempdir");
    let opts = CloneOptions::new("file:///nope.git", tmp.path().join("x"));
    let err = clone(opts, &mut NoProgress, &NeverCancel, None).expect_err("clone missing");
    classify(err);
}

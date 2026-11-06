//! Named interop checks for typed [`RemoteError`], [`BundleError`], and [`CloneError`]
//! (roadmap step 11). Assertions match on variants, not message text.

use std::path::Path;
use std::process::Command;

use grit_lib::bundle::{Bundle, BundleError};
use grit_lib::error::{AuthFailure, Error, RemoteUnreadableReason};
use grit_lib::remote::clone::{clone, CloneError, CloneOptions};
use grit_lib::remote::fetch::NoProgress;
use grit_lib::remote::transfer::{FetchDepth, FetchOptions, PushOptions, PushRefSpec};
use grit_lib::remote::{ListRefsOptions, Remote, RemoteError};
use grit_lib::repo::{InitOptions, Repository};

fn git_ok(dir: &Path, args: &[&str]) {
    let status = Command::new("git")
        .current_dir(dir)
        .args(args)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .status()
        .expect("spawn git");
    assert!(status.success(), "git {:?} in {}", args, dir.display());
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

    let src = tmp.path().join("src.git");
    git_ok(&src, &["init", "-q", "--bare", "-b", "main", "."]);
    let commit = Command::new("git")
        .current_dir(&src)
        .args(["commit", "--allow-empty", "-m", "c1"])
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .env("GIT_AUTHOR_NAME", "t")
        .env("GIT_AUTHOR_EMAIL", "t@t")
        .env("GIT_COMMITTER_NAME", "t")
        .env("GIT_COMMITTER_EMAIL", "t@t")
        .output()
        .expect("commit");
    if !commit.status.success() {
        eprintln!("SKIP: could not seed bare repo");
        return;
    }
    let dest = tmp.path().join("dest.git");
    git_ok(&dest, &["init", "-q", "--bare", "-b", "main", "."]);
    let dest_repo = Repository::open(&dest, &Default::default()).expect("open dest");
    let src_url = format!("file://{}", src.display());
    let push_err = dest_repo
        .remote()
        .push(
            &src_url,
            &[PushRefSpec::new("refs/heads/main:refs/heads/main")],
            &PushOptions::default(),
            &mut NoProgress,
            &grit_lib::progress::NeverCancel,
            None,
        )
        .expect_err("non-fast-forward push to populated branch");
    assert!(
        matches!(
            push_err,
            Error::Remote(RemoteError::Fatal { .. })
                | Error::Remote(RemoteError::Nested(_))
                | Error::NotFastForward { .. }
        ),
        "expected typed push rejection, got {push_err:?}"
    );

    let wt = tmp.path().join("full");
    git_ok(
        tmp.path(),
        &[
            "clone",
            "-q",
            &format!("file://{}", src.display()),
            wt.to_str().unwrap(),
        ],
    );
    let wt_repo = Repository::open(wt.join(".git"), &Default::default()).expect("open clone");
    let mut fetch_opts = FetchOptions::new();
    fetch_opts.fetch_depth = Some(FetchDepth::Unshallow);
    let unshallow_err = wt_repo
        .remote()
        .fetch(
            &format!("file://{}", src.display()),
            &fetch_opts,
            &mut NoProgress,
            &grit_lib::progress::NeverCancel,
            None,
        )
        .expect_err("unshallow on complete repo");
    assert!(matches!(
        unshallow_err,
        Error::Remote(RemoteError::FetchUnshallowNotShallow)
    ));

    #[cfg(feature = "http-ureq")]
    {
        use grit_lib::remote::credentials::{Credential, CredentialProvider};
        use grit_lib::remote::transport::http::HttpClient;
        use grit_lib::remote::DefaultHttpClientFactory;
        use std::sync::Arc;

        let port = grit_test_support::free_port().expect("port");
        let listener = std::net::TcpListener::bind(("127.0.0.1", port)).expect("bind");
        std::thread::spawn(move || {
            for stream in listener.incoming().flatten() {
                let mut stream = stream;
                let _ = std::io::Write::write_all(
                    &mut stream,
                    b"HTTP/1.1 401 Unauthorized\r\nWWW-Authenticate: Basic realm=\"git\"\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                );
            }
        });
        struct EmptyProvider;
        impl CredentialProvider for EmptyProvider {
            fn fill(&self, _: &Credential) -> grit_lib::error::Result<Credential> {
                Err(Error::Remote(RemoteError::CredentialsUnavailable {
                    helpers: Vec::new(),
                }))
            }
            fn approve(&self, _: &Credential) {}
            fn reject(&self, _: &Credential) {}
        }
        let client = Arc::new(
            grit_lib::remote::transport::http::UreqHttpClient::with_credentials(Box::new(
                EmptyProvider,
            ))
            .with_git_protocol("version=2"),
        ) as Arc<dyn HttpClient>;
        static FACTORY: DefaultHttpClientFactory = DefaultHttpClientFactory::new();
        let local = tmp.path().join("auth-client.git");
        Repository::init(&local, &InitOptions::default()).expect("init");
        let err = grit_lib::remote::fetch::http_fetch(
            client,
            local.join(".git"),
            &format!("http://127.0.0.1:{port}/r.git"),
            &mut FetchOptions::new(),
            &mut NoProgress,
        )
        .expect_err("401 must fail");
        assert!(matches!(
            err,
            Error::Remote(RemoteError::Auth {
                reason: AuthFailure::CredentialsUnavailable { .. },
                ..
            }) | Error::Remote(RemoteError::Auth {
                reason: AuthFailure::Rejected,
                ..
            })
        ));
        let _ = FACTORY;
    }
}

#[test]
fn bundle_missing_prerequisites() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let src = tmp.path().join("src");
    git_ok(&src, &["init", "-q", "-b", "main", "."]);
    git_ok(&src, &["commit", "--allow-empty", "-m", "c1"]);
    git_ok(&src, &["commit", "--allow-empty", "-m", "c2"]);
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
    if !c1.status.success() || !c2.status.success() {
        eprintln!("SKIP: git rev-parse failed");
        return;
    }
    let c1 = String::from_utf8_lossy(&c1.stdout).trim().to_owned();
    let c2 = String::from_utf8_lossy(&c2.stdout).trim().to_owned();
    let inc = tmp.path().join("inc.bundle");
    git_ok(
        &src,
        &[
            "bundle",
            "create",
            inc.to_str().unwrap(),
            &format!("{c1}..{c2}"),
            "main",
        ],
    );
    let empty_git = tmp.path().join("empty.git");
    git_ok(&empty_git, &["init", "-q", "--bare", "-b", "main", "."]);
    let empty = Repository::open(&empty_git, &Default::default()).expect("open");
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
    let dest = tempfile::tempdir().expect("tempdir");
    let err = clone(
        dest.path().join("x"),
        "file:///nope.git",
        &CloneOptions::default(),
        &mut NoProgress,
        &grit_lib::progress::NeverCancel,
        None,
    )
    .expect_err("clone missing");
    classify(err);
}

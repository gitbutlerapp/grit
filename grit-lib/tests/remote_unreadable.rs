//! A remote that cannot be reached or read fails with the typed
//! [`Error::RemoteUnreadable`] on every operation (list refs, fetch, push, clone) and every
//! transport, so callers never match on message text.

#![allow(clippy::unwrap_used, clippy::expect_used)]

#[cfg(feature = "http-ureq")]
use std::io::{Read, Write};
#[cfg(feature = "http-ureq")]
use std::net::TcpListener;
use std::path::Path;
use std::process::Command;

use grit_lib::error::{Error, RemoteUnreadableReason};
use grit_lib::porcelain::pull::PullError;
use grit_lib::remote::clone::{clone, CloneError, CloneOptions};
use grit_lib::remote::fetch::NoProgress;
use grit_lib::remote::transfer::{FetchOptions, PushOptions, PushRefSpec};
use grit_lib::remote::{HttpClientFactory, ListRefsOptions, Remote, RemoteError};
use grit_lib::repo::{InitOptions, Repository};

fn unreadable_reason(err: &RemoteError) -> &RemoteUnreadableReason {
    match err {
        RemoteError::Library(Error::RemoteUnreadable { reason, .. }) => reason,
        other => panic!("expected RemoteUnreadable, got {other:?}"),
    }
}

fn init_repo(dir: &Path) -> Repository {
    Repository::init(dir, &InitOptions::default()).expect("init")
}

/// Port with nothing listening (bound, then released).
#[cfg(feature = "http-ureq")]
fn closed_port() -> u16 {
    let listener = TcpListener::bind(("127.0.0.1", 0)).expect("bind");
    listener.local_addr().expect("addr").port()
}

/// Serve `status` with an empty body to every request until the test exits.
#[cfg(feature = "http-ureq")]
fn http_status_server(status: &'static str) -> u16 {
    let listener = TcpListener::bind(("127.0.0.1", 0)).expect("bind");
    let port = listener.local_addr().expect("addr").port();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { continue };
            let mut buf = [0u8; 4096];
            let _ = stream.read(&mut buf);
            let response =
                format!("HTTP/1.1 {status}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n");
            let _ = stream.write_all(response.as_bytes());
        }
    });
    port
}

fn http_factory() -> Option<&'static dyn HttpClientFactory> {
    #[cfg(feature = "http-ureq")]
    return Some(&grit_lib::remote::DefaultHttpClientFactory);
    #[cfg(not(feature = "http-ureq"))]
    None
}

fn list_refs(url: &str) -> Result<usize, RemoteError> {
    let remote = Remote::from_url(url).expect("parse url");
    remote
        .list_refs(None, &ListRefsOptions::default(), http_factory())
        .map(|refs| refs.len())
}

#[test]
fn list_refs_of_missing_local_path_is_repository_not_found() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let missing = tmp.path().join("missing.git");
    let err = list_refs(missing.to_str().expect("utf8")).expect_err("missing path must fail");
    assert!(err.is_remote_unreadable());
    assert_eq!(
        unreadable_reason(&err),
        &RemoteUnreadableReason::RepositoryNotFound
    );
    assert_eq!(
        err.to_string(),
        format!("could not find repository at '{}'", missing.display())
    );
}

#[test]
fn list_refs_of_missing_file_url_is_repository_not_found() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let missing = tmp.path().join("missing.git");
    let url = format!("file://{}", missing.display());
    let err = list_refs(&url).expect_err("missing file:// path must fail");
    assert_eq!(
        unreadable_reason(&err),
        &RemoteUnreadableReason::RepositoryNotFound
    );
}

#[test]
fn list_refs_of_plain_directory_is_repository_not_found() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let plain = tmp.path().join("plain");
    std::fs::create_dir_all(&plain).expect("mkdir");
    let err = list_refs(plain.to_str().expect("utf8")).expect_err("non-repo dir must fail");
    assert_eq!(
        unreadable_reason(&err),
        &RemoteUnreadableReason::RepositoryNotFound
    );
}

#[test]
fn list_refs_of_empty_git_repository_is_not_unreadable() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let bare = tmp.path().join("empty.git");
    let status = Command::new("git")
        .args(["init", "-q", "--bare"])
        .arg(&bare)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .status();
    if !status.is_ok_and(|s| s.success()) {
        eprintln!("SKIP: system git unavailable");
        return;
    }
    let refs = list_refs(bare.to_str().expect("utf8")).expect("empty repo lists fine");
    assert_eq!(refs, 0);
}

#[cfg(feature = "http-ureq")]
#[test]
fn list_refs_over_refused_http_port_is_transport_failure() {
    let url = format!("http://127.0.0.1:{}/repo.git", closed_port());
    let err = list_refs(&url).expect_err("refused connection must fail");
    assert!(err.is_remote_unreadable());
    assert!(
        matches!(
            unreadable_reason(&err),
            RemoteUnreadableReason::Transport { .. }
        ),
        "{err:?}"
    );
    match &err {
        RemoteError::Library(Error::RemoteUnreadable { url: Some(u), .. }) => {
            assert!(u.starts_with(&url), "url {u} should name {url}");
        }
        other => panic!("expected a url, got {other:?}"),
    }
}

#[cfg(feature = "http-ureq")]
#[test]
fn list_refs_with_http_404_is_http_status() {
    let port = http_status_server("404 Not Found");
    let err = list_refs(&format!("http://127.0.0.1:{port}/repo.git")).expect_err("404 must fail");
    assert_eq!(
        unreadable_reason(&err),
        &RemoteUnreadableReason::HttpStatus { status: 404 }
    );
    assert!(err
        .to_string()
        .starts_with("HTTP 404 from http://127.0.0.1:"));
}

#[cfg(feature = "http-ureq")]
#[test]
fn list_refs_with_http_500_is_http_status() {
    let port = http_status_server("500 Internal Server Error");
    let err = list_refs(&format!("http://127.0.0.1:{port}/repo.git")).expect_err("500 must fail");
    assert_eq!(
        unreadable_reason(&err),
        &RemoteUnreadableReason::HttpStatus { status: 500 }
    );
}

#[cfg(feature = "http-ureq")]
#[test]
fn http_url_credentials_are_redacted_from_the_error() {
    let url = format!("http://user:s3cret@127.0.0.1:{}/repo.git", closed_port());
    let err = list_refs(&url).expect_err("refused connection must fail");
    let msg = err.to_string();
    assert!(!msg.contains("s3cret"), "{msg}");
    assert!(!msg.contains("user:"), "{msg}");
}

#[test]
fn fetch_from_missing_local_path_is_remote_unreadable() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let repo = init_repo(&tmp.path().join("work"));
    let missing = tmp.path().join("missing.git");
    let remote = Remote::from_url(missing.to_str().expect("utf8")).expect("url");
    let err = remote
        .fetch(&repo, FetchOptions::default(), &mut NoProgress, None)
        .expect_err("fetch from missing path must fail");
    assert_eq!(
        unreadable_reason(&err),
        &RemoteUnreadableReason::RepositoryNotFound
    );
}

#[cfg(feature = "http-ureq")]
#[test]
fn fetch_over_refused_http_port_is_remote_unreadable() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let repo = init_repo(&tmp.path().join("work"));
    let url = format!("http://127.0.0.1:{}/repo.git", closed_port());
    let remote = Remote::from_url(&url).expect("url");
    let err = remote
        .fetch(
            &repo,
            FetchOptions::default(),
            &mut NoProgress,
            http_factory(),
        )
        .expect_err("refused connection must fail");
    assert!(matches!(
        unreadable_reason(&err),
        RemoteUnreadableReason::Transport { .. }
    ));
}

#[test]
fn push_to_missing_local_path_is_remote_unreadable() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let repo = init_repo(&tmp.path().join("work"));
    let missing = tmp.path().join("missing.git");
    let remote = Remote::from_url(missing.to_str().expect("utf8")).expect("url");
    let spec = PushRefSpec {
        dst: "refs/heads/main".to_owned(),
        delete: true,
        ..Default::default()
    };
    let err = remote
        .push(
            &repo,
            &[spec],
            PushOptions::default(),
            &mut NoProgress,
            None,
        )
        .expect_err("push to missing path must fail");
    assert_eq!(
        unreadable_reason(&err),
        &RemoteUnreadableReason::RepositoryNotFound
    );
    assert!(!missing.exists(), "push must not create the remote");
}

#[cfg(feature = "http-ureq")]
#[test]
fn push_over_refused_http_port_is_remote_unreadable() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let repo = init_repo(&tmp.path().join("work"));
    let url = format!("http://127.0.0.1:{}/repo.git", closed_port());
    let remote = Remote::from_url(&url).expect("url");
    let spec = PushRefSpec {
        dst: "refs/heads/main".to_owned(),
        delete: true,
        ..Default::default()
    };
    let err = remote
        .push(
            &repo,
            &[spec],
            PushOptions::default(),
            &mut NoProgress,
            http_factory(),
        )
        .expect_err("refused connection must fail");
    assert!(err.is_remote_unreadable(), "{err:?}");
}

#[test]
fn clone_of_missing_local_path_is_remote_unreadable() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let missing = tmp.path().join("missing.git");
    let err = clone(
        CloneOptions::new(
            missing.to_str().expect("utf8").to_owned(),
            tmp.path().join("dest"),
        ),
        &mut NoProgress,
        None,
    )
    .expect_err("clone of missing path must fail");
    assert!(err.is_remote_unreadable(), "{err:?}");
    assert!(err.should_cleanup_failed_clone());
}

#[cfg(feature = "http-ureq")]
#[test]
fn clone_over_refused_http_port_is_remote_unreadable() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let url = format!("http://127.0.0.1:{}/repo.git", closed_port());
    let err = clone(
        CloneOptions::new(url, tmp.path().join("dest")),
        &mut NoProgress,
        http_factory(),
    )
    .expect_err("refused connection must fail");
    assert!(err.is_remote_unreadable(), "{err:?}");
}

#[test]
fn git_reports_the_same_missing_path_as_unreadable() {
    // Cross-check the classification: system git fails `ls-remote` of a missing path with
    // its "could not read from remote" status (128), the code grit maps this error to.
    let tmp = tempfile::tempdir().expect("tempdir");
    let missing = tmp.path().join("missing.git");
    let Ok(out) = Command::new("git")
        .arg("ls-remote")
        .arg(&missing)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .output()
    else {
        eprintln!("SKIP: system git unavailable");
        return;
    };
    assert_eq!(out.status.code(), Some(128));
    assert!(list_refs(missing.to_str().expect("utf8"))
        .expect_err("grit must fail too")
        .is_remote_unreadable());
}

#[test]
fn wrapper_helpers_classify_only_remote_unreadable() {
    let unreadable = || {
        Error::remote_unreadable(
            "https://example.com/r.git",
            RemoteUnreadableReason::NoHandshake,
        )
    };
    assert!(unreadable().is_remote_unreadable());
    assert!(!Error::NothingToCommit.is_remote_unreadable());

    assert!(RemoteError::Library(unreadable()).is_remote_unreadable());
    assert!(!RemoteError::NoUrl.is_remote_unreadable());
    assert!(!RemoteError::Library(Error::Auth("denied".into())).is_remote_unreadable());

    assert!(CloneError::Library(unreadable()).is_remote_unreadable());
    assert!(CloneError::Remote(RemoteError::Library(unreadable())).is_remote_unreadable());
    assert!(!CloneError::InvalidCloneDepth.is_remote_unreadable());
    assert!(!CloneError::RemoteRefNotFound("x".into()).is_remote_unreadable());

    assert!(PullError::Remote(RemoteError::Library(unreadable())).is_remote_unreadable());
    assert!(PullError::Repository(unreadable()).is_remote_unreadable());
    assert!(!PullError::DetachedHead.is_remote_unreadable());
}

#[test]
fn messages_and_slugs_per_reason() {
    let cases = [
        (
            RemoteUnreadableReason::RepositoryNotFound,
            "repository_not_found",
            "could not find repository at '/srv/r.git'",
        ),
        (
            RemoteUnreadableReason::Transport {
                detail: "connection refused".into(),
            },
            "transport",
            "could not reach '/srv/r.git': connection refused",
        ),
        (
            RemoteUnreadableReason::HttpStatus { status: 503 },
            "http_status",
            "HTTP 503 from /srv/r.git",
        ),
        (
            RemoteUnreadableReason::NoHandshake,
            "no_handshake",
            "could not read from '/srv/r.git': no Git protocol response",
        ),
    ];
    for (reason, slug, message) in cases {
        assert_eq!(reason.slug(), slug);
        assert_eq!(
            Error::remote_unreadable("/srv/r.git", reason).to_string(),
            message
        );
    }
    let without_url = Error::RemoteUnreadable {
        url: None,
        reason: RemoteUnreadableReason::NoHandshake,
    };
    assert_eq!(
        without_url.to_string(),
        "could not read from the remote: no Git protocol response"
    );
}

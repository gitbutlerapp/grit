//! A remote that cannot be reached or read fails with the typed
//! [`Error::RemoteUnreadable`] on every operation (list refs, fetch, push, clone) and every
//! transport, so callers never match on message text.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::io::Read;
#[cfg(feature = "http-ureq")]
use std::io::Write;
use std::net::TcpListener;
use std::path::Path;
use std::process::Command;

use grit_lib::config::environment::RepositoryOptions;
use grit_lib::error::{Error, RemoteUnreadableReason};
use grit_lib::objects::ObjectId;
use grit_lib::porcelain::pull::PullError;
use grit_lib::progress::NeverCancel;
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
    {
        static FACTORY: grit_lib::remote::DefaultHttpClientFactory =
            grit_lib::remote::DefaultHttpClientFactory::new();
        return Some(&FACTORY);
    }
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
        .fetch(
            &repo,
            FetchOptions::default(),
            &mut NoProgress,
            &NeverCancel,
            None,
        )
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
            &NeverCancel,
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
            &NeverCancel,
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
            &NeverCancel,
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
        &NeverCancel,
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
        &NeverCancel,
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
    assert!(!Error::nothing_to_commit_clean().is_remote_unreadable());

    assert!(RemoteError::Library(unreadable()).is_remote_unreadable());
    assert!(!RemoteError::NoUrl { remote: None }.is_remote_unreadable());
    assert!(!RemoteError::Library(Error::Auth {
        url: "https://example.com/r.git".into(),
        reason: grit_lib::error::AuthFailure::Rejected,
    })
    .is_remote_unreadable());

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

/// Run system git in `dir`; `None` when git is unavailable.
fn git(dir: &Path, args: &[&str]) -> Option<String> {
    let out = Command::new("git")
        .current_dir(dir)
        .args(args)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .env("GIT_AUTHOR_NAME", "T")
        .env("GIT_AUTHOR_EMAIL", "t@example.com")
        .env("GIT_COMMITTER_NAME", "T")
        .env("GIT_COMMITTER_EMAIL", "t@example.com")
        .output()
        .ok()?;
    assert!(
        out.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    Some(String::from_utf8_lossy(&out.stdout).trim().to_owned())
}

/// A repository with one commit at `root/main` and a linked worktree on branch `wt` at
/// `root/wt`; `None` when system git is unavailable.
fn repo_with_linked_worktree(root: &Path) -> Option<ObjectId> {
    let main = root.join("main");
    std::fs::create_dir_all(&main).expect("mkdir");
    git(&main, &["init", "-q", "-b", "main", "."])?;
    std::fs::write(main.join("a.txt"), "a\n").expect("write");
    git(&main, &["add", "a.txt"])?;
    git(&main, &["commit", "-q", "-m", "c1"])?;
    git(&main, &["worktree", "add", "-q", "-b", "wt", "../wt"])?;
    ObjectId::from_hex(&git(&main, &["rev-parse", "HEAD"])?).ok()
}

/// Accept connections, read the request, and close without a Git advertisement.
fn silent_git_daemon() -> u16 {
    let listener = TcpListener::bind(("127.0.0.1", 0)).expect("bind");
    let port = listener.local_addr().expect("addr").port();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { continue };
            let mut buf = [0u8; 4096];
            let _ = stream.read(&mut buf);
        }
    });
    port
}

#[test]
fn list_refs_of_objects_only_directory_is_repository_not_found() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let fake = tmp.path().join("fake.git");
    std::fs::create_dir_all(fake.join("objects")).expect("mkdir");
    let err = list_refs(fake.to_str().expect("utf8")).expect_err("objects/ alone is not a repo");
    assert_eq!(
        unreadable_reason(&err),
        &RemoteUnreadableReason::RepositoryNotFound
    );
    let Ok(out) = Command::new("git")
        .arg("ls-remote")
        .arg(&fake)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .output()
    else {
        return;
    };
    assert_eq!(out.status.code(), Some(128), "git agrees it is not a repo");
}

#[test]
fn linked_worktree_checkout_is_a_readable_local_remote() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let Some(tip) = repo_with_linked_worktree(tmp.path()) else {
        eprintln!("SKIP: system git unavailable");
        return;
    };
    let wt = tmp.path().join("wt");
    let remote = Remote::from_url(wt.to_str().expect("utf8")).expect("url");
    let refs = remote
        .list_refs(None, &ListRefsOptions::default(), None)
        .expect("a linked worktree is a repository");
    let names: Vec<&str> = refs.iter().map(|r| r.name.as_str()).collect();
    assert!(names.contains(&"refs/heads/main"), "{names:?}");
    assert!(names.contains(&"refs/heads/wt"), "{names:?}");
    assert!(refs.iter().all(|r| r.oid == tip));

    let dest = tmp.path().join("clone");
    let outcome = clone(
        CloneOptions::new(wt.to_str().expect("utf8").to_owned(), dest.clone()),
        &mut NoProgress,
        &NeverCancel,
        None,
    )
    .expect("clone from a linked worktree");
    drop(outcome);
    git(&dest, &["fsck", "--strict"]).expect("git fsck of the clone");
    assert_eq!(
        git(&dest, &["rev-parse", "refs/remotes/origin/wt"]).as_deref(),
        Some(tip.to_hex().as_str())
    );

    std::fs::write(dest.join("b.txt"), "b\n").expect("write");
    git(&dest, &["add", "b.txt"]).expect("add");
    git(&dest, &["commit", "-q", "-m", "c2"]).expect("commit");
    let new_tip =
        ObjectId::from_hex(&git(&dest, &["rev-parse", "HEAD"]).expect("rev-parse")).expect("oid");
    let repo = Repository::open(
        &dest.join(".git"),
        &RepositoryOptions::empty().with_work_tree(&dest),
    )
    .expect("open clone");
    let spec = PushRefSpec {
        src: Some(new_tip),
        dst: "refs/heads/pushed".to_owned(),
        ..Default::default()
    };
    remote
        .push(
            &repo,
            &[spec],
            PushOptions::default(),
            &mut NoProgress,
            &NeverCancel,
            None,
        )
        .expect("push into a linked worktree's repository");
    let main = tmp.path().join("main");
    git(&main, &["fsck", "--strict"]).expect("git fsck of the remote");
    assert_eq!(
        git(&main, &["rev-parse", "refs/heads/pushed"]).as_deref(),
        Some(new_tip.to_hex().as_str())
    );

    git(&wt, &["commit", "-q", "--allow-empty", "-m", "wt2"]).expect("commit in worktree");
    let wt_tip = git(&wt, &["rev-parse", "HEAD"]).expect("rev-parse");
    let origin = repo.remotes().get("origin").expect("origin");
    origin
        .fetch(
            &repo,
            FetchOptions::default(),
            &mut NoProgress,
            &NeverCancel,
            None,
        )
        .expect("fetch from a linked worktree");
    assert_eq!(
        git(&dest, &["rev-parse", "refs/remotes/origin/wt"]).as_deref(),
        Some(wt_tip.as_str())
    );
    git(&dest, &["fsck", "--strict"]).expect("git fsck after fetch");
}

#[test]
fn separate_git_dir_checkout_is_a_readable_local_remote() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let work = tmp.path().join("work");
    let sep = tmp.path().join("sep.git");
    std::fs::create_dir_all(&work).expect("mkdir");
    let sep_arg = sep.to_str().expect("utf8");
    if git(
        &work,
        &[
            "init",
            "-q",
            "-b",
            "main",
            "--separate-git-dir",
            sep_arg,
            ".",
        ],
    )
    .is_none()
    {
        eprintln!("SKIP: system git unavailable");
        return;
    }
    std::fs::write(work.join("a.txt"), "a\n").expect("write");
    git(&work, &["add", "a.txt"]).expect("add");
    git(&work, &["commit", "-q", "-m", "c1"]).expect("commit");
    let refs = list_refs(work.to_str().expect("utf8")).expect("gitfile checkout is a repo");
    assert_eq!(refs, 2, "HEAD and refs/heads/main");
}

#[test]
fn git_daemon_without_advertisement_is_no_handshake() {
    let url = format!("git://127.0.0.1:{}/repo.git", silent_git_daemon());
    let err = list_refs(&url).expect_err("a silent git:// peer is not a remote");
    assert!(
        matches!(
            unreadable_reason(&err),
            RemoteUnreadableReason::NoHandshake | RemoteUnreadableReason::Transport { .. }
        ),
        "{err:?}"
    );

    let tmp = tempfile::tempdir().expect("tempdir");
    let repo = init_repo(&tmp.path().join("work"));
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
            &NeverCancel,
            None,
        )
        .expect_err("push to a silent git:// peer must fail");
    assert!(err.is_remote_unreadable(), "{err:?}");
    let err = remote
        .fetch(
            &repo,
            FetchOptions::default(),
            &mut NoProgress,
            &NeverCancel,
            None,
        )
        .expect_err("fetch from a silent git:// peer must fail");
    assert!(err.is_remote_unreadable(), "{err:?}");
}

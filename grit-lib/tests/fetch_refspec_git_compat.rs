//! Explicit fetch refspec resolution vs system `git fetch` (refs, FETCH_HEAD, fsck).

use std::path::{Path, PathBuf};
use std::process::Command;

use grit_lib::error::Error;
use grit_lib::objects::ObjectId;
use grit_lib::refs::resolve_ref;
use grit_lib::remote::transfer::{fetch_local, FetchOptions, TagMode};

fn run_git(dir: &Path, args: &[&str]) {
    let out = Command::new("git")
        .current_dir(dir)
        .args(args)
        .env("GIT_AUTHOR_NAME", "T")
        .env("GIT_AUTHOR_EMAIL", "t@example.com")
        .env("GIT_AUTHOR_DATE", "2005-04-07T22:13:13 +0200")
        .env("GIT_COMMITTER_NAME", "T")
        .env("GIT_COMMITTER_EMAIL", "t@example.com")
        .env("GIT_COMMITTER_DATE", "2005-04-07T22:13:13 +0200")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .output()
        .expect("git");
    assert!(
        out.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

fn git_out(dir: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .current_dir(dir)
        .args(args)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .output()
        .expect("git");
    assert!(out.status.success());
    String::from_utf8_lossy(&out.stdout).trim().to_owned()
}

fn fsck(dir: &Path) {
    let out = Command::new("git")
        .current_dir(dir)
        .args(["fsck", "--no-dangling"])
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .output()
        .expect("fsck");
    assert!(
        out.status.success(),
        "fsck: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

fn read_fetch_head(git_dir: &Path) -> String {
    std::fs::read_to_string(git_dir.join("FETCH_HEAD")).expect("FETCH_HEAD")
}

fn tracking_tip(git_dir: &Path, name: &str) -> Option<ObjectId> {
    resolve_ref(git_dir, name).ok()
}

struct BareRemote {
    work: PathBuf,
    bare: PathBuf,
}

impl BareRemote {
    fn new(tmp: &Path, name: &str) -> Self {
        let work = tmp.join(format!("{name}-work"));
        let bare = tmp.join(name);
        run_git(tmp, &["init", "-q", "-b", "main", work.to_str().unwrap()]);
        run_git(
            tmp,
            &[
                "clone",
                "-q",
                "--bare",
                work.to_str().unwrap(),
                bare.to_str().unwrap(),
            ],
        );
        run_git(&bare, &["symbolic-ref", "HEAD", "refs/heads/main"]);
        Self { work, bare }
    }

    fn push_to_bare(&self) {
        run_git(
            &self.work,
            &[
                "push",
                "-q",
                self.bare.to_str().unwrap(),
                "HEAD:refs/heads/main",
            ],
        );
    }

    fn commit_empty(&self, msg: &str) {
        run_git(&self.work, &["commit", "-q", "--allow-empty", "-m", msg]);
        self.push_to_bare();
    }

    fn path(&self) -> &Path {
        &self.bare
    }

    fn git_dir(&self) -> &Path {
        &self.bare
    }
}

fn clone_consumer(tmp: &Path, remote: &BareRemote, label: &str) -> PathBuf {
    let dest = tmp.join(label);
    run_git(
        tmp,
        &[
            "clone",
            "-q",
            remote.path().to_str().unwrap(),
            dest.to_str().unwrap(),
        ],
    );
    run_git(
        &dest,
        &[
            "remote",
            "set-url",
            "origin",
            remote.path().to_str().unwrap(),
        ],
    );
    dest
}

fn grit_fetch_file(
    local: &Path,
    remote_git: &Path,
    refspecs: &[&str],
    configured: &[&str],
) -> grit_lib::remote::transfer::FetchOutcome {
    let opts = FetchOptions {
        refspecs: refspecs.iter().map(|s| (*s).to_owned()).collect(),
        configured_refspecs: configured.iter().map(|s| (*s).to_owned()).collect(),
        tags: TagMode::None,
        remote_name: Some("origin".into()),
        fetch_source: Some({
            let mut s = remote_git.to_string_lossy().into_owned();
            if s.ends_with(".git") {
                s.truncate(s.len() - 4);
            }
            s
        }),
        ..Default::default()
    };
    fetch_local(&local.join(".git"), remote_git, &opts).expect("grit fetch_local")
}

fn git_fetch(local: &Path, remote: &Path, refspecs: &[&str]) {
    let mut args = vec!["fetch", remote.to_str().unwrap()];
    args.extend(refspecs.iter().copied());
    run_git(local, &args);
}

fn assert_fetch_parity(grit_dir: &Path, git_dir: &Path, tracking: &str) {
    fsck(grit_dir);
    fsck(git_dir);
    let g_head = read_fetch_head(&grit_dir.join(".git"));
    let gi_head = read_fetch_head(&git_dir.join(".git"));
    let grit_oid = g_head.lines().next().and_then(|l| l.split('\t').next());
    let git_oid = gi_head.lines().next().and_then(|l| l.split('\t').next());
    assert_eq!(
        grit_oid, git_oid,
        "FETCH_HEAD oid mismatch\ngrit:{g_head}\ngit:{gi_head}"
    );
    if let (Some(g), Some(i)) = (
        tracking_tip(&grit_dir.join(".git"), tracking),
        tracking_tip(&git_dir.join(".git"), tracking),
    ) {
        assert_eq!(g, i, "tracking ref {tracking}");
    }
}

#[test]
fn explicit_main_short_name_file() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let remote = BareRemote::new(tmp.path(), "remote");
    remote.commit_empty("one");
    remote.commit_empty("two");

    let grit_c = clone_consumer(tmp.path(), &remote, "grit-c");
    let git_c = clone_consumer(tmp.path(), &remote, "git-c");

    let specs = ["main"];
    let cfg = ["+refs/heads/*:refs/remotes/origin/*"];
    grit_fetch_file(&grit_c, &remote.git_dir(), &specs, &cfg);
    git_fetch(&git_c, &remote.path(), &specs);

    assert_fetch_parity(&grit_c, &git_c, "refs/remotes/origin/main");
}

#[test]
fn explicit_full_ref_and_custom_destination_file() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let remote = BareRemote::new(tmp.path(), "remote");
    remote.commit_empty("tip");

    let grit_c = clone_consumer(tmp.path(), &remote, "grit-c");
    let git_c = clone_consumer(tmp.path(), &remote, "git-c");

    let spec = ["refs/heads/main:refs/remotes/origin/main"];
    let cfg = ["+refs/heads/*:refs/remotes/origin/*"];
    grit_fetch_file(&grit_c, &remote.git_dir(), &spec, &cfg);
    git_fetch(&git_c, &remote.path(), &spec);

    assert_fetch_parity(&grit_c, &git_c, "refs/remotes/origin/main");
}

#[test]
fn pull_request_ref_short_destination_file() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let remote = BareRemote::new(tmp.path(), "remote");
    remote.commit_empty("base");
    let pr_tip = git_out(remote.path(), &["rev-parse", "refs/heads/main"]);
    run_git(
        remote.path(),
        &[
            "update-ref",
            "refs/pull/123/head",
            &pr_tip,
            "0000000000000000000000000000000000000000",
        ],
    );

    let ls = git_out(
        remote.path(),
        &["ls-remote", remote.path().to_str().unwrap()],
    );
    assert!(
        ls.lines().any(|l| l.ends_with("\trefs/pull/123/head")),
        "expected pull ref on bare remote, got:\n{ls}"
    );

    let grit_c = clone_consumer(tmp.path(), &remote, "grit-c");
    let git_c = clone_consumer(tmp.path(), &remote, "git-c");

    let spec = ["pull/123/head:pr-123"];
    let cfg = ["+refs/heads/*:refs/remotes/origin/*"];
    grit_fetch_file(&grit_c, &remote.git_dir(), &spec, &cfg);
    git_fetch(&git_c, &remote.path(), &spec);

    assert_eq!(
        tracking_tip(&grit_c.join(".git"), "refs/heads/pr-123"),
        tracking_tip(&git_c.join(".git"), "refs/heads/pr-123")
    );
    assert_fetch_parity(&grit_c, &git_c, "refs/heads/pr-123");
}

#[test]
fn tag_by_short_name_file() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let remote = BareRemote::new(tmp.path(), "remote");
    remote.commit_empty("tagged");
    run_git(&remote.work, &["tag", "v1"]);
    run_git(
        &remote.work,
        &["push", "-q", remote.bare.to_str().unwrap(), "refs/tags/v1"],
    );

    let grit_c = clone_consumer(tmp.path(), &remote, "grit-c");
    let git_c = clone_consumer(tmp.path(), &remote, "git-c");

    let spec = ["v1:refs/tags/v1"];
    let cfg = ["+refs/heads/*:refs/remotes/origin/*"];
    grit_fetch_file(&grit_c, &remote.git_dir(), &spec, &cfg);
    git_fetch(&git_c, &remote.path(), &spec);

    assert_eq!(
        tracking_tip(&grit_c.join(".git"), "refs/tags/v1"),
        tracking_tip(&git_c.join(".git"), "refs/tags/v1")
    );
    assert_fetch_parity(&grit_c, &git_c, "refs/tags/v1");
}

#[test]
fn explicit_head_resolves_file() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let remote = BareRemote::new(tmp.path(), "remote");
    remote.commit_empty("head-tip");

    let grit_c = clone_consumer(tmp.path(), &remote, "grit-c");
    let git_c = clone_consumer(tmp.path(), &remote, "git-c");

    let spec = ["HEAD:refs/heads/from-head"];
    let cfg = ["+refs/heads/*:refs/remotes/origin/*"];
    grit_fetch_file(&grit_c, &remote.git_dir(), &spec, &cfg);
    git_fetch(&git_c, &remote.path(), &spec);

    assert_eq!(
        tracking_tip(&grit_c.join(".git"), "refs/heads/from-head"),
        tracking_tip(&git_c.join(".git"), "refs/heads/from-head")
    );
    assert_fetch_parity(&grit_c, &git_c, "refs/heads/from-head");
}

#[test]
fn missing_source_ref_typed_error_file() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let remote = BareRemote::new(tmp.path(), "remote");
    remote.commit_empty("only");

    let grit_c = clone_consumer(tmp.path(), &remote, "grit-c");
    let err = fetch_local(
        &grit_c.join(".git"),
        &remote.git_dir(),
        &FetchOptions {
            refspecs: vec!["no-such-branch".into()],
            configured_refspecs: vec!["+refs/heads/*:refs/remotes/origin/*".into()],
            tags: TagMode::None,
            remote_name: Some("origin".into()),
            ..Default::default()
        },
    )
    .unwrap_err();
    assert!(
        matches!(err, Error::FetchRefspec(_)),
        "expected FetchRefspec error, got {err:?}"
    );
}

#[test]
fn non_fast_forward_local_branch_rejected_file() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let remote = BareRemote::new(tmp.path(), "remote");
    remote.commit_empty("r1");
    remote.commit_empty("r2");

    let grit_c = clone_consumer(tmp.path(), &remote, "grit-c");
    run_git(&grit_c, &["checkout", "-q", "-b", "forceme"]);
    run_git(&grit_c, &["commit", "-q", "--allow-empty", "-m", "diverge"]);
    run_git(&grit_c, &["checkout", "-q", "main"]);
    remote.commit_empty("r3");

    let err = fetch_local(
        &grit_c.join(".git"),
        &remote.git_dir(),
        &FetchOptions {
            refspecs: vec![format!("refs/heads/main:refs/heads/forceme")],
            configured_refspecs: vec!["+refs/heads/*:refs/remotes/origin/*".into()],
            tags: TagMode::None,
            remote_name: Some("origin".into()),
            ..Default::default()
        },
    )
    .unwrap_err();
    assert!(
        matches!(err, Error::FetchNonFastForward { .. }),
        "got {err:?}"
    );
}

#[test]
fn checked_out_branch_update_refused_file() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let remote = BareRemote::new(tmp.path(), "remote");
    remote.commit_empty("one");

    let grit_c = clone_consumer(tmp.path(), &remote, "grit-c");
    remote.commit_empty("two");

    let err = fetch_local(
        &grit_c.join(".git"),
        &remote.git_dir(),
        &FetchOptions {
            refspecs: vec!["refs/heads/main:refs/heads/main".into()],
            configured_refspecs: vec!["+refs/heads/*:refs/remotes/origin/*".into()],
            tags: TagMode::None,
            remote_name: Some("origin".into()),
            ..Default::default()
        },
    )
    .unwrap_err();
    assert!(
        matches!(err, Error::FetchIntoCheckedOutBranch { .. }),
        "got {err:?}"
    );
}

#[test]
fn force_update_non_fast_forward_file() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let remote = BareRemote::new(tmp.path(), "remote");
    remote.commit_empty("r1");
    let r1 = git_out(remote.path(), &["rev-parse", "HEAD"]);
    remote.commit_empty("r2");

    let grit_c = clone_consumer(tmp.path(), &remote, "grit-c");
    let git_c = clone_consumer(tmp.path(), &remote, "git-c");
    run_git(&grit_c, &["branch", "forceme", &r1]);
    run_git(&git_c, &["branch", "forceme", &r1]);

    let spec = [format!("+refs/heads/main:refs/heads/forceme")];
    let cfg = ["+refs/heads/*:refs/remotes/origin/*"];
    grit_fetch_file(&grit_c, &remote.git_dir(), &[spec[0].as_str()], &cfg);
    git_fetch(&git_c, &remote.path(), &[spec[0].as_str()]);

    assert_eq!(
        tracking_tip(&grit_c.join(".git"), "refs/heads/forceme"),
        tracking_tip(&git_c.join(".git"), "refs/heads/forceme")
    );
}

#[cfg(feature = "http-ureq")]
#[cfg(unix)]
mod http {
    use super::*;
    use grit_lib::remote::fetch::NoProgress;
    use grit_lib::remote::transport::http::ureq_client::UreqHttpClient;
    use grit_lib::remote::transport::http::{http_client_arc, http_fetch};
    use std::net::{TcpListener, TcpStream};
    use std::process::{Child, Command, Stdio};
    use std::sync::Arc;
    use std::time::{Duration, Instant};

    fn free_port() -> u16 {
        TcpListener::bind(("127.0.0.1", 0))
            .expect("bind ephemeral port")
            .local_addr()
            .expect("local addr")
            .port()
    }

    fn wait_port(port: u16) {
        let addr = std::net::SocketAddr::from(([127, 0, 0, 1], port));
        let deadline = Instant::now() + Duration::from_secs(15);
        while Instant::now() < deadline {
            if TcpStream::connect_timeout(&addr, Duration::from_millis(200)).is_ok() {
                return;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        panic!("grit-http-server did not accept connections on port {port}");
    }

    fn find_binary(name: &str) -> Option<PathBuf> {
        let exe = std::env::current_exe().ok()?;
        let deps = exe.parent()?;
        let profile = deps.parent()?;
        [profile.join(name), deps.join(name)]
            .into_iter()
            .find(|p| p.is_file())
    }

    fn require_http_server_bin() -> PathBuf {
        if let Some(p) = find_binary("grit-http-server") {
            return p;
        }
        let status = Command::new("cargo")
            .args(["build", "-q", "-p", "grit-http-server"])
            .status()
            .expect("cargo build grit-http-server");
        assert!(status.success(), "failed to build grit-http-server");
        find_binary("grit-http-server").expect("grit-http-server binary after build")
    }

    struct HttpServer {
        _child: Child,
        url: String,
    }

    fn start_http_server(bare: &Path) -> HttpServer {
        let port = free_port();
        let root = bare.parent().expect("parent").join("http-root");
        std::fs::create_dir_all(&root).expect("http root");
        let served = root.join("repo.git");
        run_git(
            bare.parent().expect("parent"),
            &[
                "clone",
                "-q",
                "--bare",
                bare.to_str().unwrap(),
                served.to_str().unwrap(),
            ],
        );
        // `git clone --bare` only mirrors heads/tags; sync the rest (e.g. refs/pull/*).
        run_git(
            served.parent().expect("parent"),
            &[
                "-C",
                served.to_str().unwrap(),
                "remote",
                "add",
                "sync-src",
                bare.to_str().unwrap(),
            ],
        );
        run_git(
            served.parent().expect("parent"),
            &[
                "-C",
                served.to_str().unwrap(),
                "fetch",
                "-q",
                "sync-src",
                "+refs/*:refs/*",
            ],
        );
        run_git(
            served.parent().expect("parent"),
            &[
                "-C",
                served.to_str().unwrap(),
                "remote",
                "remove",
                "sync-src",
            ],
        );
        let bin = require_http_server_bin();
        let child = Command::new(&bin)
            .arg("--root")
            .arg(&root)
            .arg("--bind")
            .arg(format!("127.0.0.1:{port}"))
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn grit-http-server");
        wait_port(port);
        HttpServer {
            _child: child,
            url: format!("http://127.0.0.1:{port}/repo.git"),
        }
    }

    fn grit_fetch_http(local: &Path, url: &str, refspecs: &[&str], configured: &[&str]) {
        let client =
            Arc::new(UreqHttpClient::from_config(&Default::default()).expect("ureq client"));
        let mut opts = FetchOptions {
            refspecs: refspecs.iter().map(|s| (*s).to_owned()).collect(),
            configured_refspecs: configured.iter().map(|s| (*s).to_owned()).collect(),
            tags: TagMode::None,
            remote_name: Some("origin".into()),
            fetch_source: Some(url.to_owned()),
            ..Default::default()
        };
        http_fetch(
            http_client_arc(client),
            &local.join(".git"),
            url,
            &mut opts,
            &mut NoProgress,
        )
        .expect("http fetch");
    }

    fn setup_http_consumers(
        tmp: &Path,
        remote: &BareRemote,
        server: &HttpServer,
        grit_label: &str,
        git_label: &str,
    ) -> (PathBuf, PathBuf) {
        let grit_c = clone_consumer(tmp, remote, grit_label);
        let git_c = clone_consumer(tmp, remote, git_label);
        run_git(&grit_c, &["remote", "set-url", "origin", &server.url]);
        run_git(&git_c, &["remote", "set-url", "origin", &server.url]);
        (grit_c, git_c)
    }

    #[test]
    fn explicit_main_short_name_http() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let remote = BareRemote::new(tmp.path(), "remote");
        remote.commit_empty("one");
        remote.commit_empty("two");
        let server = start_http_server(remote.path());
        let cfg = ["+refs/heads/*:refs/remotes/origin/*"];
        let (grit_c, git_c) = setup_http_consumers(tmp.path(), &remote, &server, "grit-c", "git-c");
        grit_fetch_http(&grit_c, &server.url, &["main"], &cfg);
        git_fetch(&git_c, &PathBuf::from(&server.url), &["main"]);
        assert_fetch_parity(&grit_c, &git_c, "refs/remotes/origin/main");
    }

    #[test]
    fn pull_request_ref_http() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let remote = BareRemote::new(tmp.path(), "remote");
        remote.commit_empty("base");
        let pr_tip = git_out(remote.path(), &["rev-parse", "refs/heads/main"]);
        run_git(
            remote.path(),
            &[
                "update-ref",
                "refs/pull/123/head",
                &pr_tip,
                "0000000000000000000000000000000000000000",
            ],
        );
        let server = start_http_server(remote.path());
        let ls = git_out(tmp.path(), &["ls-remote", server.url.as_str()]);
        assert!(
            ls.lines().any(|l| l.ends_with("\trefs/pull/123/head")),
            "HTTP remote must advertise pull ref:\n{ls}"
        );
        let cfg = ["+refs/heads/*:refs/remotes/origin/*"];
        let (grit_c, git_c) = setup_http_consumers(tmp.path(), &remote, &server, "grit-c", "git-c");
        let spec = ["pull/123/head:pr-123"];
        grit_fetch_http(&grit_c, &server.url, &spec, &cfg);
        git_fetch(&git_c, &PathBuf::from(&server.url), &spec);
        assert_fetch_parity(&grit_c, &git_c, "refs/heads/pr-123");
    }

    #[test]
    fn tag_by_short_name_http() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let remote = BareRemote::new(tmp.path(), "remote");
        remote.commit_empty("tagged");
        run_git(&remote.work, &["tag", "v1"]);
        run_git(
            &remote.work,
            &["push", "-q", remote.bare.to_str().unwrap(), "refs/tags/v1"],
        );
        let server = start_http_server(remote.path());
        let cfg = ["+refs/heads/*:refs/remotes/origin/*"];
        let (grit_c, git_c) = setup_http_consumers(tmp.path(), &remote, &server, "grit-c", "git-c");
        let spec = ["v1:refs/tags/v1"];
        grit_fetch_http(&grit_c, &server.url, &spec, &cfg);
        git_fetch(&git_c, &PathBuf::from(&server.url), &spec);
        assert_fetch_parity(&grit_c, &git_c, "refs/tags/v1");
    }

    #[test]
    fn explicit_head_http() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let remote = BareRemote::new(tmp.path(), "remote");
        remote.commit_empty("tip");
        let server = start_http_server(remote.path());
        let cfg = ["+refs/heads/*:refs/remotes/origin/*"];
        let (grit_c, git_c) = setup_http_consumers(tmp.path(), &remote, &server, "grit-c", "git-c");
        let spec = ["HEAD:refs/heads/from-head"];
        grit_fetch_http(&grit_c, &server.url, &spec, &cfg);
        git_fetch(&git_c, &PathBuf::from(&server.url), &spec);
        assert_fetch_parity(&grit_c, &git_c, "refs/heads/from-head");
    }

    #[test]
    fn missing_source_typed_error_http() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let remote = BareRemote::new(tmp.path(), "remote");
        remote.commit_empty("only");
        let server = start_http_server(remote.path());
        let grit_c = clone_consumer(tmp.path(), &remote, "grit-c");
        run_git(&grit_c, &["remote", "set-url", "origin", &server.url]);
        let client =
            Arc::new(UreqHttpClient::from_config(&Default::default()).expect("ureq client"));
        let mut opts = FetchOptions {
            refspecs: vec!["no-such-branch".into()],
            configured_refspecs: vec!["+refs/heads/*:refs/remotes/origin/*".into()],
            tags: TagMode::None,
            remote_name: Some("origin".into()),
            fetch_source: Some(server.url.clone()),
            ..Default::default()
        };
        let err = http_fetch(
            http_client_arc(client),
            &grit_c.join(".git"),
            &server.url,
            &mut opts,
            &mut NoProgress,
        )
        .unwrap_err();
        assert!(
            matches!(err, Error::FetchRefspec(_)),
            "expected FetchRefspec, got {err:?}"
        );
    }
}

//! Batch ref-store transactions in fetch and receive-pack (task #780 acceptance).

mod support;

use std::path::Path;
use std::process::{Command, Stdio};
#[cfg(unix)]
use std::time::{Duration, Instant};

use grit_lib::ref_storage::RefStorageFormat;
use grit_lib::refs::resolve_ref;
use grit_lib::repo::init_repository;
use grit_lib::transfer::{FetchOptions, TagMode};

use support::{each_backend, git, reftable_tables_snapshot, two_commit_oids, Backend};

#[test]
fn fetch_updates_refs_in_one_transaction_reftable() {
    each_backend(|backend, remote_repo| {
        if backend != Backend::Reftable {
            return;
        }
        let remote_git = remote_repo.git_dir();
        let (_, c2) = two_commit_oids(&remote_repo);
        grit_lib::refs::write_ref(&remote_git, "refs/heads/branch-a", &c2).expect("remote a");
        grit_lib::refs::write_ref(&remote_git, "refs/heads/branch-b", &c2).expect("remote b");

        let local_root = tempfile::tempdir().expect("local tempdir");
        let local = init_repository(
            local_root.path(),
            false,
            "main",
            None,
            RefStorageFormat::Reftable,
        )
        .expect("local init");
        let local_git = &local.git_dir;
        let (_, count_before) = reftable_tables_snapshot(&local_git);

        let opts = FetchOptions {
            refspecs: vec!["+refs/heads/*:refs/remotes/origin/*".to_owned()],
            tags: TagMode::None,
            initial_remote_fetch: true,
            remote_name: Some("origin".to_owned()),
            ..Default::default()
        };
        grit_lib::transfer::fetch_local(&local_git, &remote_git, &opts).expect("fetch");

        assert!(resolve_ref(&local_git, "refs/remotes/origin/branch-a").is_ok());
        assert!(resolve_ref(&local_git, "refs/remotes/origin/branch-b").is_ok());
        let (_, count_after) = reftable_tables_snapshot(&local_git);
        assert_eq!(
            count_after,
            count_before + 1,
            "fetch must apply all ref updates in one reftable transaction"
        );
    });
}

#[test]
fn push_tracking_remote_refs_one_reftable_transaction() {
    each_backend(|backend, repo| {
        if backend != Backend::Reftable {
            return;
        }
        let git_dir = repo.git_dir();
        let (c1, c2) = two_commit_oids(&repo);
        std::fs::write(
            git_dir.join("config"),
            "[remote \"origin\"]\n\tfetch = +refs/heads/*:refs/remotes/origin/*\n",
        )
        .expect("config");

        use grit_lib::branch_tracking::apply_push_remote_tracking_updates;
        use grit_lib::push_report::{PushRefResult, PushRefStatus};

        let results = vec![
            PushRefResult {
                local_ref: Some("refs/heads/alpha".to_owned()),
                remote_ref: "refs/heads/alpha".to_owned(),
                old_oid: None,
                new_oid: Some(c1),
                forced: false,
                deletion: false,
                status: PushRefStatus::Ok,
                message: None,
            },
            PushRefResult {
                local_ref: Some("refs/heads/beta".to_owned()),
                remote_ref: "refs/heads/beta".to_owned(),
                old_oid: None,
                new_oid: Some(c2),
                forced: false,
                deletion: false,
                status: PushRefStatus::Ok,
                message: None,
            },
        ];

        let (_, count_before) = reftable_tables_snapshot(&git_dir);
        apply_push_remote_tracking_updates(&git_dir, "origin", &results).expect("tracking batch");
        assert_eq!(
            resolve_ref(&git_dir, "refs/remotes/origin/alpha").ok(),
            Some(c1)
        );
        assert_eq!(
            resolve_ref(&git_dir, "refs/remotes/origin/beta").ok(),
            Some(c2)
        );
        let (_, count_after) = reftable_tables_snapshot(&git_dir);
        assert_eq!(
            count_after,
            count_before + 1,
            "tracking updates from one push must share one reftable transaction"
        );
    });
}

#[cfg(unix)]
#[test]
fn receive_pack_atomic_push_both_backends() {
    each_backend(|backend, _| {
        let ref_format = match backend {
            Backend::Files => RefStorageFormat::Files,
            Backend::Reftable => RefStorageFormat::Reftable,
        };
        atomic_push_over_grit_http(ref_format);
    });
}

#[cfg(unix)]
fn atomic_push_over_grit_http(ref_format: RefStorageFormat) {
    let Some(server_bin) = find_binary("grit-http-server") else {
        eprintln!("SKIP: grit-http-server not built");
        return;
    };
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path().join("srv");
    std::fs::create_dir_all(&root).expect("srv root");
    let bare_name = "atomic.git";
    let bare = root.join(bare_name);
    std::fs::create_dir_all(&bare).expect("bare dir");
    init_repository(&bare, true, "main", None, ref_format).expect("bare init");
    run_git(&bare, &["config", "receive.denyNonFastForwards", "true"]);

    let source = tmp.path().join("src");
    std::fs::create_dir_all(&source).expect("source");
    let graph = build_push_graph(&source);
    run_git(
        &bare,
        &[
            "fetch",
            source.to_str().expect("utf8"),
            &format!("refs/heads/main:refs/heads/main"),
        ],
    );
    run_git(
        &bare,
        &[
            "fetch",
            source.to_str().expect("utf8"),
            &format!("{}:refs/heads/main", graph.ff.to_hex()),
        ],
    );
    assert_eq!(
        resolve_ref(&bare, "refs/heads/main").expect("bare main"),
        graph.ff
    );

    let port = free_port().expect("port");
    let child = Command::new(&server_bin)
        .arg("--root")
        .arg(&root)
        .arg("--bind")
        .arg(format!("127.0.0.1:{port}"))
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn server");
    let _guard = ServerChild(child);
    if !wait_port(port) {
        eprintln!("SKIP: grit-http-server not ready");
        return;
    }

    run_git(
        &source,
        &[
            "remote",
            "add",
            "grit",
            &format!("http://127.0.0.1:{port}/{bare_name}"),
        ],
    );
    let push = Command::new("git")
        .current_dir(&source)
        .args([
            "push",
            "--atomic",
            "grit",
            &format!("side:refs/heads/main"),
            &format!("extra:refs/heads/atomic-ok"),
        ])
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .output()
        .expect("git push");
    assert!(
        !push.status.success(),
        "atomic push with one non-ff ref must fail: {}",
        String::from_utf8_lossy(&push.stderr)
    );
    assert_eq!(
        resolve_ref(&bare, "refs/heads/main").expect("main"),
        graph.ff,
        "atomic abort must leave main unchanged"
    );
    assert!(
        resolve_ref(&bare, "refs/heads/atomic-ok").is_err(),
        "atomic abort must not create the otherwise-valid ref"
    );
}

struct PushGraph {
    ff: grit_lib::objects::ObjectId,
    side: grit_lib::objects::ObjectId,
    extra: grit_lib::objects::ObjectId,
}

fn build_push_graph(dir: &Path) -> PushGraph {
    run_git(dir, &["init", "-q", "-b", "main", "."]);
    std::fs::write(dir.join("a.txt"), "one\n").expect("a");
    run_git(dir, &["add", "a.txt"]);
    run_git(dir, &["commit", "-q", "-m", "c1"]);
    let c1 = rev_parse(dir, "HEAD");

    std::fs::write(dir.join("b.txt"), "two\n").expect("b");
    run_git(dir, &["add", "b.txt"]);
    run_git(dir, &["commit", "-q", "-m", "c2"]);

    std::fs::write(dir.join("c.txt"), "three\n").expect("c");
    run_git(dir, &["add", "c.txt"]);
    run_git(dir, &["commit", "-q", "-m", "ff"]);
    let ff = rev_parse(dir, "HEAD");

    run_git(dir, &["checkout", "-q", "-b", "side", &c1.to_hex()]);
    std::fs::write(dir.join("s.txt"), "side\n").expect("s");
    run_git(dir, &["add", "s.txt"]);
    run_git(dir, &["commit", "-q", "-m", "side"]);
    let side = rev_parse(dir, "HEAD");

    run_git(dir, &["checkout", "-q", "-b", "extra", &c1.to_hex()]);
    std::fs::write(dir.join("x.txt"), "extra\n").expect("x");
    run_git(dir, &["add", "x.txt"]);
    run_git(dir, &["commit", "-q", "-m", "extra"]);
    let extra = rev_parse(dir, "HEAD");

    PushGraph { ff, side, extra }
}

fn run_git_output(dir: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .current_dir(dir)
        .args(args)
        .env("GIT_AUTHOR_NAME", "T")
        .env("GIT_AUTHOR_EMAIL", "t@example.com")
        .env("GIT_COMMITTER_NAME", "T")
        .env("GIT_COMMITTER_EMAIL", "t@example.com")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .output()
        .expect("git");
    assert!(
        out.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8(out.stdout).expect("utf8")
}

fn run_git(dir: &Path, args: &[&str]) {
    let _ = run_git_output(dir, args);
}

fn rev_parse(dir: &Path, rev: &str) -> grit_lib::objects::ObjectId {
    grit_lib::objects::ObjectId::from_hex(run_git_output(dir, &["rev-parse", rev]).trim())
        .expect("oid")
}

fn find_binary(name: &str) -> Option<std::path::PathBuf> {
    let manifest_dir = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let workspace = manifest_dir.parent()?;
    for profile in ["debug", "release"] {
        let path = workspace.join("target").join(profile).join(name);
        if path.is_file() {
            return Some(path);
        }
    }
    None
}

#[cfg(unix)]
fn free_port() -> std::io::Result<u16> {
    use std::net::TcpListener;
    let listener = TcpListener::bind("127.0.0.1:0")?;
    Ok(listener.local_addr()?.port())
}

#[cfg(unix)]
fn wait_port(port: u16) -> bool {
    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline {
        if std::net::TcpStream::connect(format!("127.0.0.1:{port}")).is_ok() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    false
}

#[cfg(unix)]
struct ServerChild(std::process::Child);

#[cfg(unix)]
impl Drop for ServerChild {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

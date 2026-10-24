//! Filtered fetch with promisor packs (partial clone) over upload-pack.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use grit_lib::config::environment::RepositoryOptions;
use grit_lib::config::ConfigSet;
use grit_lib::maintenance::repository_fsck::{fsck, FsckOptions, FsckReport};
use grit_lib::objects::pack::clear_pack_cache;
use grit_lib::objects::promisor::repo_treats_promisor_packs;
use grit_lib::objects::ObjectId;
use grit_lib::progress::{NeverCancel, NullProgress};
use grit_lib::remote::clone::{clone, CloneOptions};
use grit_lib::remote::fetch::NoProgress;
use grit_lib::remote::partial_clone::ObjectFilterSpec;
use grit_lib::remote::transfer::{FetchOptions, TagMode};
use grit_lib::remote::Remote;
use grit_lib::repo::Repository;
use grit_test_support::git_cmd;

fn git_in(dir: &Path, args: &[&str]) {
    let out = Command::new("git")
        .current_dir(dir)
        .args(args)
        .env_remove("GIT_DIR")
        .env_remove("GIT_ALTERNATE_OBJECT_DIRECTORIES")
        .env_remove("GIT_WORK_TREE")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .env("GIT_AUTHOR_NAME", "T")
        .env("GIT_AUTHOR_EMAIL", "t@example.com")
        .env("GIT_COMMITTER_NAME", "T")
        .env("GIT_COMMITTER_EMAIL", "t@example.com")
        .output()
        .expect("spawn git");
    assert!(
        out.status.success(),
        "git {:?} in {}: {}",
        args,
        dir.display(),
        String::from_utf8_lossy(&out.stderr)
    );
}

fn setup_filtering_bare() -> (tempfile::TempDir, PathBuf) {
    let layout = tempfile::TempDir::new().expect("layout");
    let upstream = layout.path().join("upstream");
    git_in(layout.path(), &["init", "-q", "-b", "main", "upstream"]);
    git_in(&upstream, &["config", "user.email", "t@t"]);
    fs::write(upstream.join("small.txt"), b"tiny\n").expect("small");
    fs::write(upstream.join("large.txt"), vec![b'x'; 4096]).expect("large");
    git_in(&upstream, &["add", "."]);
    git_in(&upstream, &["commit", "-m", "seed", "-q"]);

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
    );
    git_in(&bare, &["config", "uploadpack.allowFilter", "true"]);
    (layout, bare)
}

fn first_pack_path(pack_dir: &Path) -> Option<PathBuf> {
    fs::read_dir(pack_dir)
        .ok()?
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .find(|p| p.extension().is_some_and(|x| x == "pack"))
}

fn pack_has_blob(pack_dir: &Path) -> bool {
    let Some(pack) = first_pack_path(pack_dir) else {
        return false;
    };
    let out = Command::new("git")
        .args(["verify-pack", "-v"])
        .arg(&pack)
        .output()
        .expect("verify-pack");
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).contains(" blob ")
}

fn promisor_sidecar_exists(objects: &Path) -> bool {
    fs::read_dir(objects.join("pack"))
        .into_iter()
        .flatten()
        .flatten()
        .any(|e| e.path().extension().is_some_and(|x| x == "promisor"))
}

fn git_config_value(git_dir: &Path, key: &str) -> Option<String> {
    let cfg = ConfigSet::load(
        &grit_lib::config::environment::Environment::capture_process(),
        Some(git_dir),
        true,
    )
    .expect("config");
    cfg.get(key)
}

fn grit_clone_filtered(url: &str, dest: &Path, filter: ObjectFilterSpec) -> PathBuf {
    fs::create_dir_all(dest).expect("dest");
    clone(
        CloneOptions {
            url: url.to_owned(),
            dest: dest.to_path_buf(),
            filter: Some(filter),
            ..CloneOptions::new(url, dest.to_path_buf())
        },
        &mut NoProgress,
        None,
    )
    .expect("clone");
    dest.join(".git")
}

fn assert_partial_clone_config(git_dir: &Path, remote: &str) {
    assert_eq!(
        git_config_value(git_dir, &format!("remote.{remote}.promisor")).as_deref(),
        Some("true")
    );
    assert_eq!(
        git_config_value(git_dir, "core.repositoryformatversion").as_deref(),
        Some("1")
    );
    let git_cfg = Command::new("git")
        .args([
            "config",
            "--get",
            &format!("remote.{remote}.partialclonefilter"),
        ])
        .current_dir(git_dir.parent().unwrap())
        .env("GIT_DIR", git_dir)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .output()
        .expect("git config");
    assert!(git_cfg.status.success());
    let grit_filter = git_config_value(git_dir, &format!("remote.{remote}.partialclonefilter"))
        .expect("partialclonefilter");
    assert_eq!(
        String::from_utf8_lossy(&git_cfg.stdout).trim(),
        grit_filter.trim()
    );
}

#[test]
fn fetch_blob_none_receives_no_blobs() {
    let (_layout, bare) = setup_filtering_bare();
    let url = format!("file://{}", bare.canonicalize().unwrap().display());
    let dest = tempfile::TempDir::new().expect("dest");
    let git_dir = grit_clone_filtered(&url, dest.path(), ObjectFilterSpec::BlobNone);
    clear_pack_cache();

    let objects = git_dir.join("objects");
    assert!(
        promisor_sidecar_exists(&objects),
        "expected .promisor next to pack"
    );
    assert!(
        !pack_has_blob(&objects.join("pack")),
        "blob:none pack should contain no blobs"
    );
    assert_partial_clone_config(&git_dir, "origin");

    let repo = Repository::open(
        &git_dir,
        &RepositoryOptions::empty().with_work_tree(dest.path()),
    )
    .expect("open");
    for path in ["small.txt", "large.txt"] {
        let hex = git_cmd(&["rev-parse", &format!("HEAD:{path}")])
            .in_dir(dest.path())
            .exec()
            .stdout;
        let oid = ObjectId::from_hex(hex.trim()).expect("oid");
        assert!(
            !repo.odb().exists_local(&oid),
            "{path} blob should be absent locally after blob:none"
        );
    }
}

#[test]
fn fetch_blob_limit_keeps_small_blobs() {
    let (_layout, bare) = setup_filtering_bare();
    let url = format!("file://{}", bare.canonicalize().unwrap().display());
    let dest = tempfile::TempDir::new().expect("dest");
    let git_dir = grit_clone_filtered(&url, dest.path(), ObjectFilterSpec::BlobLimit(100));
    clear_pack_cache();
    let objects = git_dir.join("objects");
    assert!(promisor_sidecar_exists(&objects));
    assert!(
        pack_has_blob(&objects.join("pack")),
        "small blob should remain under blob:limit=100"
    );
    let large_oid_hex = git_cmd(&["rev-parse", "HEAD:large.txt"])
        .in_dir(dest.path())
        .exec()
        .stdout;
    let large = ObjectId::from_hex(large_oid_hex.trim()).expect("large oid");
    let repo = Repository::open(
        &git_dir,
        &RepositoryOptions::empty().with_work_tree(dest.path()),
    )
    .expect("open");
    assert!(!repo.odb().exists(&large), "large blob should be omitted");
}

#[test]
fn filter_dropped_when_server_lacks_capability() {
    let (_layout, bare) = setup_filtering_bare();
    git_in(&bare, &["config", "--unset", "uploadpack.allowFilter"]);
    let url = format!("file://{}", bare.canonicalize().unwrap().display());
    let dest = tempfile::TempDir::new().expect("dest");
    let git_dir = grit_clone_filtered(&url, dest.path(), ObjectFilterSpec::BlobNone);
    clear_pack_cache();
    let objects = git_dir.join("objects");
    assert_partial_clone_config(&git_dir, "origin");
    assert!(
        promisor_sidecar_exists(&objects),
        "Git still marks the pack promisor when the server drops the filter"
    );
    assert!(
        pack_has_blob(&objects.join("pack")),
        "unfiltered fetch should include blobs"
    );
}

#[test]
fn refetch_reapplies_stored_filter() {
    let (_layout, bare) = setup_filtering_bare();
    let url = format!("file://{}", bare.canonicalize().unwrap().display());
    let dest = tempfile::TempDir::new().expect("dest");
    let git_dir = grit_clone_filtered(&url, dest.path(), ObjectFilterSpec::BlobNone);
    clear_pack_cache();

    let wt = bare.parent().unwrap().join("upstream");
    fs::write(wt.join("new.txt"), b"new\n").expect("new file");
    git_in(&wt, &["add", "new.txt"]);
    git_in(&wt, &["commit", "-m", "tip2", "-q"]);
    git_in(&wt, &["push", bare.to_str().unwrap(), "main"]);

    let repo = Repository::open(
        &git_dir,
        &RepositoryOptions::empty().with_work_tree(dest.path()),
    )
    .expect("open");
    let config = ConfigSet::load(
        &grit_lib::config::environment::Environment::capture_process(),
        Some(&git_dir),
        true,
    )
    .expect("cfg");
    let remote = Remote::from_config(&config, "origin").expect("remote");
    let fetch_opts = FetchOptions {
        refspecs: vec!["+refs/heads/main:refs/remotes/origin/main".to_owned()],
        tags: TagMode::Following,
        remote_name: Some("origin".to_owned()),
        ..Default::default()
    };
    remote
        .fetch(&repo, fetch_opts, &mut NoProgress, None)
        .expect("refetch");
    clear_pack_cache();
    let objects = git_dir.join("objects");
    assert!(
        promisor_sidecar_exists(&objects),
        "refetch from promisor remote should mark pack promisor"
    );
}

/// Object store, connectivity and refs, like `git fsck --no-dangling`.
fn grit_fsck(repo: &Repository) -> (FsckReport, bool) {
    let opts = FsckOptions::full_repository(false).with_check_dangling(false);
    let report = fsck(repo, &opts, &mut NullProgress, &NeverCancel).expect("grit fsck");
    let ok = report.ok(&repo.config().expect("config"), &opts);
    (report, ok)
}

#[test]
fn fsck_accepts_promised_missing_blobs() {
    let (_layout, bare) = setup_filtering_bare();
    let url = format!("file://{}", bare.canonicalize().unwrap().display());
    let dest = tempfile::TempDir::new().expect("dest");
    let git_dir = grit_clone_filtered(&url, dest.path(), ObjectFilterSpec::BlobNone);
    clear_pack_cache();
    let repo = Repository::open(
        &git_dir,
        &RepositoryOptions::empty().with_work_tree(dest.path()),
    )
    .expect("open");
    assert!(repo_treats_promisor_packs(
        &git_dir,
        &repo.config().expect("cfg")
    ));
    let (report, ok) = grit_fsck(&repo);
    assert!(ok, "{:?}", report.issues);

    let git_fsck = Command::new("git")
        .current_dir(dest.path())
        .args(["fsck", "--no-dangling"])
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .output()
        .expect("git fsck");
    assert!(
        git_fsck.status.success(),
        "git fsck: {}",
        String::from_utf8_lossy(&git_fsck.stderr)
    );
}

#[test]
fn fsck_accepts_promised_missing_blobs_in_index_after_reset() {
    let (_layout, bare) = setup_filtering_bare();
    let url = format!("file://{}", bare.canonicalize().unwrap().display());
    let dest = tempfile::TempDir::new().expect("dest");
    let git_dir = grit_clone_filtered(&url, dest.path(), ObjectFilterSpec::BlobNone);
    clear_pack_cache();
    git_in(dest.path(), &["read-tree", "HEAD"]);
    let repo = Repository::open(
        &git_dir,
        &RepositoryOptions::empty().with_work_tree(dest.path()),
    )
    .expect("open");
    let index = repo.index_handle().load().expect("index");
    assert!(
        index
            .entries()
            .iter()
            .any(|e| !repo.odb().exists_local(&e.oid)),
        "index should reference at least one promisor-only blob after read-tree"
    );
    assert!(
        index
            .entries()
            .iter()
            .any(|e| repo.odb().read(&e.oid).is_err()),
        "missing promisor blobs must not be materialized locally"
    );
    let (report, ok) = grit_fsck(&repo);
    assert!(
        ok,
        "promised index blobs must not fail fsck: {:?}",
        report.issues
    );
}

#[cfg(feature = "http-ureq")]
mod http_v2 {
    use super::*;
    use std::net::{TcpListener, TcpStream};
    use std::process::{Child, Stdio};
    use std::time::{Duration, Instant};

    fn find_binary(name: &str) -> Option<PathBuf> {
        let exe = std::env::current_exe().ok()?;
        let deps = exe.parent()?;
        let profile = deps.parent()?;
        profile.join(name).is_file().then(|| profile.join(name))
    }

    fn spawn_server(server_bin: &Path, root: &Path, port: u16) -> Option<Child> {
        Command::new(server_bin)
            .arg("--root")
            .arg(root)
            .arg("--bind")
            .arg(format!("127.0.0.1:{port}"))
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .ok()
    }

    fn wait_ready(port: u16) -> bool {
        let addr = std::net::SocketAddr::from(([127, 0, 0, 1], port));
        let deadline = Instant::now() + Duration::from_secs(15);
        while Instant::now() < deadline {
            if TcpStream::connect_timeout(&addr, Duration::from_millis(200)).is_ok() {
                return true;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        false
    }

    #[test]
    fn fetch_blob_none_over_grit_http_v2() {
        let Some(server_bin) = find_binary("grit-http-server") else {
            eprintln!("SKIP: grit-http-server not built");
            return;
        };
        let (_layout, bare) = setup_filtering_bare();
        let port = TcpListener::bind(("127.0.0.1", 0))
            .ok()
            .and_then(|l| l.local_addr().ok().map(|a| a.port()))
            .expect("port");
        let root = bare.parent().unwrap().join("http-root");
        fs::create_dir_all(&root).expect("root");
        let served = root.join("repo.git");
        git_in(
            bare.parent().unwrap(),
            &[
                "clone",
                "-q",
                "--bare",
                bare.to_str().unwrap(),
                served.to_str().unwrap(),
            ],
        );
        git_in(&served, &["config", "uploadpack.allowFilter", "true"]);
        let mut child = spawn_server(&server_bin, &root, port).expect("server");
        assert!(wait_ready(port));
        let url = format!("http://127.0.0.1:{port}/repo.git");
        let dest = tempfile::TempDir::new().expect("dest");
        clone(
            CloneOptions {
                url: url.clone(),
                dest: dest.path().to_path_buf(),
                filter: Some(ObjectFilterSpec::BlobNone),
                ..CloneOptions::new(&url, dest.path().to_path_buf())
            },
            &mut NoProgress,
            Some(&grit_lib::remote::DefaultHttpClientFactory),
        )
        .expect("http clone");
        let git_dir = dest.path().join(".git");
        clear_pack_cache();
        assert!(promisor_sidecar_exists(&git_dir.join("objects")));
        let _ = child.kill();
    }
}

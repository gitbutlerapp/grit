//! A partial clone of a local path that accepted a large-object promisor fetches the
//! omitted blob from that LOP, all served in-process by grit's own upload-pack.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::path::Path;
use std::sync::Arc;

use grit_lib::config::environment::RepositoryOptions;
use grit_lib::config::ConfigSet;
use grit_lib::objects::pack::clear_pack_cache;
use grit_lib::objects::promisor::{DefaultPromisorFetcher, PromisorFetcher};
use grit_lib::objects::{ObjectId, ObjectKind};
use grit_lib::progress::NeverCancel;
use grit_lib::refs::ref_storage::RefStorageFormat;
use grit_lib::remote::clone::{clone, CloneOptions};
use grit_lib::remote::fetch::NoProgress;
use grit_lib::remote::partial_clone::ObjectFilterSpec;
use grit_lib::remote::transport::http::HttpClient;
use grit_lib::remote::{HttpClientFactory, RemoteError, RemoteResult};
use grit_lib::repo::{InitOptions, Repository};

/// Only local remotes here, so no HTTP client is ever made.
struct NoHttp;

impl HttpClientFactory for NoHttp {
    fn create(&self, _: &ConfigSet) -> RemoteResult<Box<dyn HttpClient>> {
        Err(RemoteError::UnsupportedScheme)
    }

    fn create_for_probe(&self, _: &ConfigSet) -> RemoteResult<Box<dyn HttpClient>> {
        Err(RemoteError::UnsupportedScheme)
    }
}

fn append_config(git_dir: &Path, text: &str) {
    let path = git_dir.join("config");
    let mut config = std::fs::read_to_string(&path).unwrap();
    config.push_str(text);
    std::fs::write(path, config).unwrap();
}

/// A bare repository whose `main` holds a small blob and a 20 KiB one.
fn bare_with_big_blob(git_dir: &Path) -> ObjectId {
    let repo = Repository::init(
        git_dir,
        &InitOptions::new()
            .with_bare(true)
            .with_initial_branch("main")
            .with_ref_storage(RefStorageFormat::Files),
    )
    .unwrap();
    let odb = repo.odb();
    let small = odb
        .write_loose_materialize(ObjectKind::Blob, b"small\n")
        .unwrap();
    let big = odb
        .write_loose_materialize(ObjectKind::Blob, &vec![b'x'; 20 * 1024])
        .unwrap();
    let mut tree = b"100644 big\0".to_vec();
    tree.extend_from_slice(big.as_bytes());
    tree.extend_from_slice(b"100644 small\0");
    tree.extend_from_slice(small.as_bytes());
    let tree = odb
        .write_loose_materialize(ObjectKind::Tree, &tree)
        .unwrap();
    let commit = format!(
        "tree {tree}\nauthor T <t@e.com> 100 +0000\ncommitter T <t@e.com> 100 +0000\n\nseed\n"
    );
    let tip = odb
        .write_loose_materialize(ObjectKind::Commit, commit.as_bytes())
        .unwrap();
    std::fs::write(git_dir.join("refs/heads/main"), format!("{tip}\n")).unwrap();
    big
}

#[test]
fn omitted_blob_comes_from_the_lop_when_the_server_is_gone() {
    let tmp = tempfile::tempdir().unwrap();
    let server = tmp.path().join("server.git");
    let lop = tmp.path().join("lop.git");
    let big = bare_with_big_blob(&server);
    assert_eq!(bare_with_big_blob(&lop), big);
    let lop_url = format!("file://{}", lop.display());
    append_config(
        &server,
        &format!(
            "[uploadpack]\n\tallowFilter = true\n\
             [promisor]\n\tadvertise = true\n\tsendFields = partialCloneFilter\n\
             [remote \"lop\"]\n\turl = {lop_url}\n\tpromisor = true\n\
             \tpartialCloneFilter = blob:limit=5k\n"
        ),
    );
    append_config(
        &lop,
        "[uploadpack]\n\tallowFilter = true\n\tallowAnySHA1InWant = true\n",
    );

    let dest = tmp.path().join("client");
    let mut opts = CloneOptions::new(server.display().to_string(), dest.clone());
    opts.auto_filter = true;
    opts.checkout = false;
    opts.config = vec![
        ("remote.lop.url".into(), lop_url.clone()),
        ("remote.lop.promisor".into(), "true".into()),
        ("promisor.acceptFromServer".into(), "knownUrl".into()),
    ];
    let outcome = clone(opts, &mut NoProgress, &NeverCancel, None).expect("clone");
    assert_eq!(outcome.filter, Some(ObjectFilterSpec::BlobLimit(5 * 1024)));
    assert_eq!(
        outcome.promisor.as_ref().unwrap().decision.accepted_names(),
        ["lop"]
    );
    let git_dir = outcome.repo.git_dir().to_path_buf();
    drop(outcome);
    clear_pack_cache();

    // Only the LOP is left to supply the omitted blob.
    std::fs::rename(&server, tmp.path().join("server-gone.git")).unwrap();
    let fetcher: Arc<dyn PromisorFetcher> = Arc::new(DefaultPromisorFetcher::new(Arc::new(NoHttp)));
    let repo = Repository::open(
        &git_dir,
        &RepositoryOptions::empty()
            .with_work_tree(&dest)
            .with_promisor_fetcher(Some(fetcher)),
    )
    .unwrap();
    let blob = repo.odb().read(&big).expect("lazy fetch from the LOP");
    assert_eq!(blob.kind, ObjectKind::Blob);
    assert_eq!(blob.data.len(), 20 * 1024);
}

#[test]
fn local_filtered_clone_is_served_in_process() {
    let tmp = tempfile::tempdir().unwrap();
    let server = tmp.path().join("server.git");
    let big = bare_with_big_blob(&server);
    append_config(&server, "[uploadpack]\n\tallowFilter = true\n");
    let dest = tmp.path().join("client");
    let mut opts = CloneOptions::new(format!("file://{}", server.display()), dest);
    opts.filter = Some(ObjectFilterSpec::BlobNone);
    opts.checkout = false;
    let outcome = clone(opts, &mut NoProgress, &NeverCancel, None).expect("clone");
    assert!(outcome.transfer.objects >= 2);
    assert!(!outcome.repo.odb().contains(&big).unwrap());
}

/// An environment whose only config outside the repositories is `global` (no system config).
fn env_with_global_config(home: &Path, global: &str) -> grit_lib::config::environment::Environment {
    std::fs::create_dir_all(home).unwrap();
    let global_path = home.join("global.gitconfig");
    std::fs::write(&global_path, global).unwrap();
    grit_lib::config::environment::Environment::from_vars(
        [
            ("HOME", home.as_os_str()),
            ("GIT_CONFIG_GLOBAL", global_path.as_os_str()),
            ("GIT_CONFIG_NOSYSTEM", std::ffi::OsStr::new("1")),
        ]
        .map(|(k, v)| (std::ffi::OsString::from(k), v.to_owned())),
        home.to_path_buf(),
    )
}

#[test]
fn local_filtered_clone_sees_the_global_upload_pack_config() {
    // Git's file:// transport runs upload-pack with the user's global config, so a
    // global uploadpack.allowFilter lets the served repository filter.
    let tmp = tempfile::tempdir().unwrap();
    let server = tmp.path().join("server.git");
    let big = bare_with_big_blob(&server);
    let env = env_with_global_config(
        &tmp.path().join("home"),
        "[uploadpack]\n\tallowFilter = true\n",
    );
    let dest = tmp.path().join("client");
    let mut opts = CloneOptions::new(format!("file://{}", server.display()), dest);
    opts.environment = env;
    opts.filter = Some(ObjectFilterSpec::BlobNone);
    opts.checkout = false;
    let outcome = clone(opts, &mut NoProgress, &NeverCancel, None).expect("clone");
    assert!(!outcome.repo.odb().contains(&big).unwrap());
}

#[test]
fn filter_the_server_does_not_support_is_reported_as_ignored() {
    let tmp = tempfile::tempdir().unwrap();
    let server = tmp.path().join("server.git");
    let big = bare_with_big_blob(&server);
    let env = env_with_global_config(&tmp.path().join("home"), "");
    let dest = tmp.path().join("client");
    let mut opts = CloneOptions::new(format!("file://{}", server.display()), dest);
    opts.environment = env;
    opts.filter = Some(ObjectFilterSpec::BlobNone);
    opts.checkout = false;
    let outcome = clone(opts, &mut NoProgress, &NeverCancel, None).expect("clone");
    assert!(outcome.filter_ignored());
    assert!(!outcome.filter_sent);
    assert!(outcome.repo.odb().contains(&big).unwrap());
}

#[test]
fn a_failing_local_server_reports_its_own_error() {
    // The served repository lost a blob it would need to size for the filter: the
    // clone must fail with the server's reason, not with a confusing client-side one.
    let tmp = tempfile::tempdir().unwrap();
    let server = tmp.path().join("server.git");
    let big = bare_with_big_blob(&server);
    append_config(&server, "[uploadpack]\n\tallowFilter = true\n");
    let hex = big.to_hex();
    std::fs::remove_file(server.join("objects").join(&hex[..2]).join(&hex[2..])).unwrap();
    let dest = tmp.path().join("client");
    let mut opts = CloneOptions::new(format!("file://{}", server.display()), dest);
    opts.environment = env_with_global_config(&tmp.path().join("home"), "");
    opts.filter = Some(ObjectFilterSpec::BlobLimit(10 * 1024));
    opts.checkout = false;
    let err = match clone(opts, &mut NoProgress, &NeverCancel, None) {
        Ok(_) => panic!("clone of a broken server succeeded"),
        Err(err) => err,
    };
    let shown = err.to_string();
    assert!(shown.contains(&hex), "{shown}");
    assert!(!shown.contains("refusing to point ref"), "{shown}");
}

#[test]
fn blobless_checkout_takes_each_blob_from_the_remote_that_has_it() {
    // The usual large-object layout: the LOP holds only the large blob, the server
    // everything else. The checkout's one lazy fetch names both blobs, which neither
    // remote can serve alone.
    let tmp = tempfile::tempdir().unwrap();
    let server = tmp.path().join("server.git");
    let big = bare_with_big_blob(&server);
    let lop = tmp.path().join("lop.git");
    let lop_repo = Repository::init(
        &lop,
        &InitOptions::new()
            .with_bare(true)
            .with_ref_storage(RefStorageFormat::Files),
    )
    .unwrap();
    assert_eq!(
        lop_repo
            .odb()
            .write_loose_materialize(ObjectKind::Blob, &vec![b'x'; 20 * 1024])
            .unwrap(),
        big
    );
    let hex = big.to_hex();
    std::fs::remove_file(server.join("objects").join(&hex[..2]).join(&hex[2..])).unwrap();
    let lop_url = format!("file://{}", lop.display());
    append_config(
        &server,
        &format!(
            "[uploadpack]\n\tallowFilter = true\n\tallowAnySHA1InWant = true\n\
             [promisor]\n\tadvertise = true\n\
             [remote \"lop\"]\n\turl = {lop_url}\n\tpromisor = true\n"
        ),
    );
    append_config(
        &lop,
        "[uploadpack]\n\tallowFilter = true\n\tallowAnySHA1InWant = true\n",
    );

    let dest = tmp.path().join("client");
    let mut opts = CloneOptions::new(format!("file://{}", server.display()), dest.clone());
    opts.environment = env_with_global_config(&tmp.path().join("home"), "");
    opts.filter = Some(ObjectFilterSpec::BlobNone);
    opts.config = vec![
        ("remote.lop.url".into(), lop_url),
        ("remote.lop.promisor".into(), "true".into()),
        ("promisor.acceptFromServer".into(), "knownUrl".into()),
    ];
    let fetcher: Arc<dyn PromisorFetcher> = Arc::new(DefaultPromisorFetcher::new(Arc::new(NoHttp)));
    opts.promisor_fetcher = Some(fetcher);
    let outcome = clone(opts, &mut NoProgress, &NeverCancel, None).expect("clone");
    assert_eq!(outcome.files, 2);
    assert_eq!(std::fs::read(dest.join("small")).unwrap(), b"small\n");
    assert_eq!(std::fs::read(dest.join("big")).unwrap().len(), 20 * 1024);
}

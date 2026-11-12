//! Coverage for the transport, bundle, serve and snapshot-checkout handles:
//! `repo.remotes().fetch_from` / `push_to` / `fetch_all` / push planning,
//! `repo.bundles()`, the `UploadPack` / `ReceivePack` sessions, `UploadPackStream`
//! and `repo.snapshot_checkouts()`. Everything runs in-process between grit
//! repositories; the git-compatible round trips live in the full-tests suite.

use std::io::{BufReader, PipeReader, PipeWriter, Read, Write};
use std::path::{Path, PathBuf};
use std::thread::JoinHandle;

use grit_lib::bundle::bundle_remote::BundleCreateOptions;
use grit_lib::bundle::{Bundle, BundleError, BundleSpec};
use grit_lib::config::environment::RepositoryOptions;
use grit_lib::objects::pack_objects::{PackBuildOptions, UploadPackStream};
use grit_lib::objects::{ObjectId, ObjectKind};
use grit_lib::progress::{CancelToken, NullProgress};
use grit_lib::remote::fetch::NoProgress;
use grit_lib::remote::push_plan::{ForceMode, PushRequest};
use grit_lib::remote::push_report::PushRefStatus;
use grit_lib::remote::serve::{
    ProtocolVersion, ReceivePack, ReceivePolicy, RefUpdateRefusal, ServeOptions, UploadPack,
};
use grit_lib::remote::transfer::{
    FetchAllOptions, FetchOptions, PushOptions, PushRefSpec, TagMode, TransferEndpoint,
};
use grit_lib::remote::transport::{read_advertisement, Advertisement, Connection};
use grit_lib::repo::{InitOptions, Repository};

fn init(path: &Path, bare: bool) -> Repository {
    let (repo, _) = Repository::init(
        path,
        &InitOptions::new()
            .with_bare(bare)
            .with_initial_branch("main"),
    )
    .expect("init");
    repo
}

fn open(path: &Path) -> Repository {
    Repository::open(path, &RepositoryOptions::empty()).expect("open")
}

/// Commit one file on top of `parent` and point `refs/heads/main` at it.
fn commit(repo: &Repository, name: &str, text: &str, parent: Option<ObjectId>) -> ObjectId {
    let blob = repo
        .objects()
        .write(ObjectKind::Blob, text.as_bytes())
        .expect("blob");
    let mut tree = Vec::new();
    tree.extend_from_slice(format!("100644 {name}\0").as_bytes());
    tree.extend_from_slice(blob.as_bytes());
    let tree = repo.objects().write(ObjectKind::Tree, &tree).expect("tree");
    let mut body = format!("tree {tree}\n");
    if let Some(parent) = parent {
        body.push_str(&format!("parent {parent}\n"));
    }
    body.push_str("author T <t@example.com> 1700000000 +0000\n");
    body.push_str("committer T <t@example.com> 1700000000 +0000\n\n");
    body.push_str(&format!("{name}\n"));
    let oid = repo
        .objects()
        .write(ObjectKind::Commit, body.as_bytes())
        .expect("commit");
    grit_test_support::refs::set_ref(&repo, "refs/heads/main", &oid).expect("main");
    oid
}

fn tracking_fetch() -> FetchOptions {
    let mut opts = FetchOptions::default();
    opts.refspecs = vec!["+refs/heads/*:refs/remotes/origin/*".to_owned()];
    opts.remote_name = Some("origin".to_owned());
    opts.tags = TagMode::None;
    opts
}

fn push_main(tip: ObjectId) -> Vec<PushRefSpec> {
    vec![PushRefSpec::default()
        .with_src(Some(tip))
        .with_src_ref(Some("refs/heads/main".to_owned()))
        .with_dst("refs/heads/main".to_owned())]
}

#[test]
fn bundles_write_verify_unbundle_and_fetch_between_repositories() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let src = init(&tmp.path().join("src"), true);
    let c1 = commit(&src, "a.txt", "one\n", None);
    let c2 = commit(&src, "b.txt", "two\n", Some(c1));
    let spec = BundleSpec::default().with_include(vec![(c2, "refs/heads/main".to_owned())]);

    let plan = src.bundles().plan_create(&spec).expect("plan");
    assert_eq!(plan.refs.len(), 1);
    assert!(plan.prerequisites.is_empty());
    assert_eq!(plan.object_count, 6);

    let path = tmp.path().join("all.bundle");
    let tips = src
        .bundles()
        .write(&spec, &path, &mut NullProgress, &CancelToken::never())
        .expect("write");
    assert_eq!(tips, 1);
    assert!(!path.with_extension("bundle.part").exists());

    let mut streamed = Vec::new();
    src.bundles()
        .write_to(
            &spec,
            &mut streamed,
            &mut NullProgress,
            &CancelToken::never(),
        )
        .expect("write_to");
    assert_eq!(streamed, std::fs::read(&path).expect("read bundle"));

    let heads = src.bundles().list_heads(&path, None).expect("heads");
    assert_eq!(heads.items[0].oid, c2);

    let dest = init(&tmp.path().join("dest"), true);
    assert!(dest.bundles().verify(&path).expect("verify").is_ok());
    let unbundle_plan = dest.bundles().plan_unbundle(&path).expect("plan unbundle");
    assert_eq!(unbundle_plan.object_count, 6);
    let outcome = dest
        .bundles()
        .unbundle(&path, &mut NullProgress, &CancelToken::never())
        .expect("unbundle");
    assert_eq!(outcome.refs[0].oid, c2);
    assert!(dest.objects().contains(&c1).expect("contains"));

    let fetcher = init(&tmp.path().join("fetcher"), true);
    let fetched = fetcher
        .bundles()
        .fetch(
            &path,
            &tracking_fetch(),
            &mut NullProgress,
            &CancelToken::never(),
        )
        .expect("fetch from bundle");
    assert_eq!(fetched.changed_ref_count(), 1);
    assert_eq!(
        fetcher
            .references()
            .resolve(
                grit_lib::refs::store::RefNameRef::new("refs/remotes/origin/main")
                    .expect("valid ref name")
            )
            .expect("tracking"),
        c2
    );
}

#[test]
fn bundles_write_from_revs_records_the_range_boundary() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let src = init(&tmp.path().join("src"), true);
    let c1 = commit(&src, "a.txt", "one\n", None);
    let c2 = commit(&src, "b.txt", "two\n", Some(c1));
    let options = BundleCreateOptions::from_rev_tokens(vec![format!("{c1}..main")]);

    let plan = src.bundles().plan_create_from_revs(&options).expect("plan");
    assert_eq!(plan.prerequisites.len(), 1);
    assert_eq!(plan.prerequisites[0].oid, c1);

    let path = tmp.path().join("range.bundle");
    src.bundles()
        .write_from_revs(&options, &path, &mut NullProgress, &CancelToken::never())
        .expect("write_from_revs");
    let bundle = Bundle::open(&path).expect("open");
    assert_eq!(bundle.header().prerequisites[0].0, c1);
    assert_eq!(bundle.header().refs[0].1, c2);

    let empty = init(&tmp.path().join("empty"), true);
    let err = bundle
        .unbundle(&empty, &mut NullProgress, &CancelToken::never())
        .expect_err("prerequisite missing");
    assert!(matches!(err, BundleError::MissingPrerequisites { .. }));
}

#[test]
fn bundles_write_leaves_the_destination_alone_when_cancelled() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let src = init(&tmp.path().join("src"), true);
    let tip = commit(&src, "a.txt", "one\n", None);
    let path = tmp.path().join("kept.bundle");
    std::fs::write(&path, b"previous").expect("seed");
    let cancel = CancelToken::new();
    cancel.cancel();
    let err = src
        .bundles()
        .write(
            &BundleSpec::default().with_include(vec![(tip, "refs/heads/main".to_owned())]),
            &path,
            &mut NullProgress,
            &cancel,
        )
        .expect_err("cancelled");
    assert!(err.is_cancelled(), "{err:?}");
    assert_eq!(std::fs::read(&path).expect("read"), b"previous");
    assert!(!path.with_extension("bundle.part").exists());
}

#[test]
fn remotes_fetch_from_and_push_to_a_local_endpoint() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let upstream = init(&tmp.path().join("upstream.git"), true);
    let base = commit(&upstream, "a.txt", "one\n", None);
    let local = init(&tmp.path().join("local"), false);

    let mut fetch = tracking_fetch();
    let outcome = local
        .remotes()
        .fetch_from(
            TransferEndpoint::Local(upstream.git_dir()),
            &mut fetch,
            &mut NoProgress,
            &CancelToken::never(),
        )
        .expect("fetch_from");
    assert_eq!(outcome.changed_ref_count(), 1);
    assert_eq!(
        local
            .references()
            .resolve(
                grit_lib::refs::store::RefNameRef::new("refs/remotes/origin/main")
                    .expect("valid ref name")
            )
            .expect("tracking"),
        base
    );

    let tip = commit(&local, "b.txt", "two\n", Some(base));
    let pushed = local
        .remotes()
        .push_to(
            TransferEndpoint::Local(upstream.git_dir()),
            &push_main(tip),
            &PushOptions::default(),
            &mut NoProgress,
            &CancelToken::never(),
        )
        .expect("push_to");
    assert!(pushed.has_updates());
    assert!(!pushed.has_rejections());
    assert_eq!(
        upstream
            .references()
            .resolve(
                grit_lib::refs::store::RefNameRef::new("refs/heads/main").expect("valid ref name")
            )
            .expect("main"),
        tip
    );

    let cancel = CancelToken::new();
    cancel.cancel();
    let err = local
        .remotes()
        .fetch_from(
            TransferEndpoint::Local(upstream.git_dir()),
            &mut tracking_fetch(),
            &mut NoProgress,
            &cancel,
        )
        .expect_err("cancelled");
    assert!(matches!(err, grit_lib::error::Error::Cancelled), "{err:?}");
}

/// A [`Connection`] to an upload-pack or receive-pack session running on a thread.
struct SessionConnection {
    reader: BufReader<PipeReader>,
    writer: Option<PipeWriter>,
    advertisement: Advertisement,
    server: Option<JoinHandle<()>>,
}

impl SessionConnection {
    fn spawn(serve: impl FnOnce(&mut dyn Read, &mut dyn Write) + Send + 'static) -> Self {
        let (mut server_in, client_out) = std::io::pipe().expect("pipe");
        let (client_in, mut server_out) = std::io::pipe().expect("pipe");
        let server = std::thread::spawn(move || serve(&mut server_in, &mut server_out));
        let mut reader = BufReader::new(client_in);
        let advertisement = read_advertisement(&mut reader).expect("advertisement");
        Self {
            reader,
            writer: Some(client_out),
            advertisement,
            server: Some(server),
        }
    }

    fn finish(mut self) {
        self.writer = None;
        let mut rest = Vec::new();
        let _ = self.reader.read_to_end(&mut rest);
        if let Some(server) = self.server.take() {
            server.join().expect("server thread");
        }
    }
}

impl Connection for SessionConnection {
    fn reader(&mut self) -> &mut dyn Read {
        &mut self.reader
    }

    fn writer(&mut self) -> &mut dyn Write {
        self.writer.as_mut().expect("writer open")
    }

    fn advertised_refs(&self) -> &[(String, ObjectId)] {
        &self.advertisement.refs
    }

    fn capabilities(&self) -> &[String] {
        &self.advertisement.capabilities
    }

    fn head_symref(&self) -> Option<&str> {
        self.advertisement.head_symref.as_deref()
    }

    fn protocol_version(&self) -> u8 {
        self.advertisement.protocol_version
    }

    fn finish_send(&mut self) {
        self.writer = None;
    }
}

fn serve_options() -> ServeOptions {
    ServeOptions::default()
        .with_protocol(ProtocolVersion::V0)
        .with_agent("grit/test".to_owned())
}

#[test]
fn remotes_fetch_from_and_push_to_a_connection_served_in_process() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let upstream_dir = tmp.path().join("upstream.git");
    let upstream = init(&upstream_dir, true);
    let base = commit(&upstream, "a.txt", "one\n", None);
    let local = init(&tmp.path().join("local"), false);

    let served: PathBuf = upstream_dir.clone();
    let mut conn = SessionConnection::spawn(move |input, output| {
        let repo = open(&served);
        let mut options = serve_options();
        UploadPack::new(&repo, &mut options)
            .run(input, output)
            .expect("upload-pack");
    });
    let outcome = local
        .remotes()
        .fetch_from(
            TransferEndpoint::Connection(&mut conn),
            &mut tracking_fetch(),
            &mut NoProgress,
            &CancelToken::never(),
        )
        .expect("fetch over connection");
    conn.finish();
    assert_eq!(outcome.changed_ref_count(), 1);
    assert!(outcome.transfer.objects >= 3);
    assert!(local.objects().contains(&base).expect("contains"));

    let tip = commit(&local, "b.txt", "two\n", Some(base));
    let served: PathBuf = upstream_dir.clone();
    let mut conn = SessionConnection::spawn(move |input, output| {
        let repo = open(&served);
        let refuse_protected = |command: &grit_lib::remote::serve::RefCommand,
                                _: &grit_lib::objects::odb::Odb| {
            if command.refname.starts_with("refs/heads/protected/") {
                Err(RefUpdateRefusal::Policy("protected branch".to_owned()))
            } else {
                Ok(())
            }
        };
        ReceivePack::new(&repo, &serve_options(), &ReceivePolicy::default())
            .with_hook(&refuse_protected)
            .with_cancel(&CancelToken::never())
            .run(input, output)
            .expect("receive-pack");
    });
    let mut specs = push_main(tip);
    specs.push(
        PushRefSpec::default()
            .with_src(Some(tip))
            .with_src_ref(Some("refs/heads/main".to_owned()))
            .with_dst("refs/heads/protected/x".to_owned()),
    );
    let pushed = local
        .remotes()
        .push_to(
            TransferEndpoint::Connection(&mut conn),
            &specs,
            &PushOptions::default(),
            &mut NoProgress,
            &CancelToken::never(),
        )
        .expect("push over connection");
    conn.finish();
    assert_eq!(pushed.results[0].status, PushRefStatus::Ok);
    assert_eq!(pushed.results[1].status, PushRefStatus::RemoteRejected);
    assert_eq!(
        grit_test_support::refs::at(&upstream_dir)
            .references()
            .resolve(
                grit_lib::refs::store::RefNameRef::new("refs/heads/main").expect("valid ref name")
            )
            .expect("main"),
        tip
    );
    assert!(grit_test_support::refs::at(&upstream_dir)
        .references()
        .resolve(
            grit_lib::refs::store::RefNameRef::new("refs/heads/protected/x")
                .expect("valid ref name")
        )
        .is_err());
}

#[test]
fn receive_pack_session_stops_before_moving_refs_when_cancelled() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let repo = init(&tmp.path().join("r.git"), true);
    commit(&repo, "a.txt", "one\n", None);
    let cancel = CancelToken::new();
    cancel.cancel();
    let err = ReceivePack::new(&repo, &serve_options(), &ReceivePolicy::default())
        .with_cancel(&cancel)
        .run(&mut std::io::empty(), &mut Vec::new())
        .expect_err("cancelled");
    assert!(err.is_cancelled(), "{err:?}");

    let mut options = serve_options();
    let err = UploadPack::new(&repo, &mut options)
        .with_cancel(&cancel)
        .run(&mut std::io::empty(), &mut Vec::new())
        .expect_err("cancelled");
    assert!(err.is_cancelled(), "{err:?}");
}

#[test]
fn remotes_fetch_all_applies_the_prune_override_to_every_remote() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let one = init(&tmp.path().join("one.git"), true);
    let one_tip = commit(&one, "a.txt", "one\n", None);
    let two = init(&tmp.path().join("two.git"), true);
    let two_tip = commit(&two, "b.txt", "two\n", None);
    let local = init(&tmp.path().join("local"), false);
    local
        .remotes()
        .add("one", &one.git_dir().to_string_lossy())
        .expect("add one");
    local
        .remotes()
        .add("two", &two.git_dir().to_string_lossy())
        .expect("add two");
    let local_tip = commit(&local, "c.txt", "local\n", None);
    grit_test_support::refs::set_ref(&local, "refs/remotes/one/gone", &local_tip).expect("stale");

    let results = local
        .remotes()
        .fetch_all(
            &FetchAllOptions::new(FetchOptions::default())
                .with_prune(Some(true))
                .with_tags(Some(false)),
            &mut NoProgress,
            &CancelToken::never(),
            None,
        )
        .expect("fetch_all");
    let names: Vec<&str> = results.iter().map(|(name, _)| name.as_str()).collect();
    assert_eq!(names, ["one", "two"]);
    for (name, result) in &results {
        assert!(result.is_ok(), "{name}: {result:?}");
    }
    assert_eq!(
        local
            .references()
            .resolve(
                grit_lib::refs::store::RefNameRef::new("refs/remotes/one/main")
                    .expect("valid ref name")
            )
            .expect("one"),
        one_tip
    );
    assert_eq!(
        local
            .references()
            .resolve(
                grit_lib::refs::store::RefNameRef::new("refs/remotes/two/main")
                    .expect("valid ref name")
            )
            .expect("two"),
        two_tip
    );
    assert!(local
        .references()
        .resolve(
            grit_lib::refs::store::RefNameRef::new("refs/remotes/one/gone")
                .expect("valid ref name")
        )
        .is_err());
}

#[test]
fn remotes_plan_a_push_and_record_its_upstream() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let upstream = init(&tmp.path().join("upstream.git"), true);
    let base = commit(&upstream, "a.txt", "one\n", None);
    let local = init(&tmp.path().join("local"), false);
    local
        .remotes()
        .add("origin", &upstream.git_dir().to_string_lossy())
        .expect("add origin");
    local
        .remotes()
        .fetch_from(
            TransferEndpoint::Local(upstream.git_dir()),
            &mut tracking_fetch(),
            &mut NoProgress,
            &CancelToken::never(),
        )
        .expect("fetch");
    let tip = commit(&local, "b.txt", "two\n", Some(base));

    assert_eq!(
        local.remotes().force_mode(false, None).expect("none"),
        ForceMode::None
    );
    assert_eq!(
        local.remotes().force_mode(true, None).expect("force"),
        ForceMode::Force
    );

    let ctx = local
        .remotes()
        .push_plan_context(Some("origin"), None)
        .expect("context");
    assert!(ctx.remote_ref_names.contains("refs/heads/main"));
    let request = PushRequest::default()
        .with_remote(Some("origin".to_owned()))
        .with_refspecs(vec!["main".to_owned()])
        .with_set_upstream(true);
    let plan = local
        .remotes()
        .resolve_push(&request, &ctx)
        .expect("resolve");
    assert_eq!(plan.remote, "origin");
    assert_eq!(plan.specs[0].dst, "refs/heads/main");
    assert_eq!(plan.specs[0].src, Some(tip));
    assert!(local
        .remotes()
        .follow_tag_push_specs(&plan.specs, &ctx)
        .expect("follow tags")
        .is_empty());

    let mut leased = plan.specs[0].clone();
    local.remotes().apply_tracking_lease("origin", &mut leased);
    assert!(leased.force);
    assert_eq!(leased.expected_old, Some(base));

    let pushed = local
        .remotes()
        .push_to(
            TransferEndpoint::Local(upstream.git_dir()),
            &plan.specs,
            &PushOptions::default(),
            &mut NoProgress,
            &CancelToken::never(),
        )
        .expect("push");
    let upstream_set = local
        .remotes()
        .set_upstream_after_push(&plan.remote, &plan.specs, &pushed.results)
        .expect("set upstream");
    assert_eq!(upstream_set, ["main"]);
    let config = local.config().snapshot().expect("config");
    assert_eq!(
        config.get("branch.main.remote").expect("remote").as_deref(),
        Some("origin")
    );
    assert_eq!(
        config.get("branch.main.merge").expect("merge").as_deref(),
        Some("refs/heads/main")
    );
}

#[test]
fn upload_pack_stream_writes_the_objects_a_client_lacks() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let repo = init(&tmp.path().join("r.git"), true);
    let c1 = commit(&repo, "a.txt", "one\n", None);
    let c2 = commit(&repo, "b.txt", "two\n", Some(c1));
    let options = PackBuildOptions::for_serve(None);

    let mut full = Vec::new();
    let stats = UploadPackStream::new(&[c2], options)
        .write(&repo, &mut NullProgress, &mut full)
        .expect("full pack");
    assert_eq!(stats.object_count, 6);
    assert!(full.starts_with(b"PACK"));

    let haves = [c1];
    let mut thin = Vec::new();
    let stats = UploadPackStream::new(&[c2], options)
        .with_haves(&haves)
        .write(&repo, &mut NullProgress, &mut thin)
        .expect("incremental pack");
    assert_eq!(stats.object_count, 3);
}

#[cfg(feature = "unstable-hosting")]
#[test]
fn snapshot_checkouts_make_a_snapshot_of_the_target_branch() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let repo = init(&tmp.path().join("served.git"), true);
    let tip = commit(&repo, "a.txt", "one\n", None);
    let checkouts = repo.snapshot_checkouts();
    assert_eq!(checkouts.target_branch(), "main");
    assert!(checkouts.read_slices(tip).expect("slices").is_empty());
    let snapshot = checkouts
        .make_snapshot(tip, &[], None)
        .expect("make snapshot");
    assert_eq!(snapshot.files, 1);
    assert_eq!(
        checkouts
            .make_snapshot(tip, &[], None)
            .expect("again")
            .commit,
        snapshot.commit
    );
    assert_eq!(
        checkouts
            .fetch_promised_blobs(snapshot.commit)
            .expect("blobs"),
        0
    );
    assert!(checkouts.ensure_refreshable().is_ok());
}

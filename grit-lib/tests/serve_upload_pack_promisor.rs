//! `upload_pack` serving a repository whose large blobs were offloaded to a
//! large-object promisor (LOP): promised blobs are left out for clients that
//! accepted the LOP, fetched first when lazy fetch is on, and otherwise refused
//! with a clear error before any pack data is sent.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::collections::HashSet;
use std::io::Cursor;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use grit_lib::config::environment::RepositoryOptions;
use grit_lib::config::ConfigSet;
use grit_lib::maintenance::{offload_blobs, OffloadOptions};
use grit_lib::objects::index_pack::{ingest_received_pack, IngestPackOptions};
use grit_lib::objects::promisor::{DefaultPromisorFetcher, PromisorFetcher};
use grit_lib::objects::{ObjectId, ObjectKind};
use grit_lib::refs::ref_storage::RefStorageFormat;
use grit_lib::remote::partial_clone::ObjectFilterSpec;
use grit_lib::remote::pkt_line::{read_packet, Packet};
use grit_lib::remote::serve::{
    receive_pack, upload_pack, MissingObjectReason, ProtocolVersion, ReceiveOutcome, ReceivePolicy,
    ServeError, ServeOptions,
};
use grit_lib::remote::transport::http::HttpClient;
use grit_lib::remote::{HttpClientFactory, RemoteError, RemoteResult};
use grit_lib::repo::{InitOptions, Repository};

const BIG: usize = 20 * 1024;

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

fn pkt(payload: &str) -> Vec<u8> {
    format!("{:04x}{payload}", payload.len() + 4).into_bytes()
}

fn append_config(git_dir: &Path, text: &str) {
    let path = git_dir.join("config");
    let mut config = std::fs::read_to_string(&path).unwrap();
    config.push_str(text);
    std::fs::write(path, config).unwrap();
}

fn init_bare(dir: &Path) -> Repository {
    Repository::init(
        dir,
        &InitOptions::new()
            .with_bare(true)
            .with_initial_branch("main")
            .with_ref_storage(RefStorageFormat::Files),
    )
    .unwrap()
}

/// A main repository (`main` holds a small and a 20 KiB blob) whose big blob was
/// offloaded to a LOP next to it, advertised to clients.
struct Offloaded {
    _dir: tempfile::TempDir,
    main: PathBuf,
    lop: PathBuf,
    tip: ObjectId,
    tree: ObjectId,
    small: ObjectId,
    big: ObjectId,
}

impl Offloaded {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let main = dir.path().join("main.git");
        let lop = dir.path().join("lop.git");
        let repo = init_bare(&main);
        init_bare(&lop);
        let odb = repo.odb();
        let small = odb.write(ObjectKind::Blob, b"small\n").unwrap();
        let big_data: Vec<u8> = (0..BIG).map(|i| (i * 7 % 251) as u8).collect();
        let big = odb.write(ObjectKind::Blob, &big_data).unwrap();
        let mut tree = b"100644 big\0".to_vec();
        tree.extend_from_slice(big.as_bytes());
        tree.extend_from_slice(b"100644 small\0");
        tree.extend_from_slice(small.as_bytes());
        let tree = odb.write(ObjectKind::Tree, &tree).unwrap();
        let commit = format!(
            "tree {tree}\nauthor T <t@e.com> 100 +0000\ncommitter T <t@e.com> 100 +0000\n\nseed\n"
        );
        let tip = odb.write(ObjectKind::Commit, commit.as_bytes()).unwrap();
        std::fs::write(main.join("refs/heads/main"), format!("{tip}\n")).unwrap();
        append_config(
            &main,
            &format!(
                "[uploadpack]\n\tallowFilter = true\n\
                 [promisor]\n\tadvertise = true\n\
                 [remote \"lop\"]\n\turl = file://{}\n",
                lop.display()
            ),
        );
        let mut opts = OffloadOptions::new("lop", 1_700_000_000);
        opts.filter = Some(ObjectFilterSpec::BlobLimit(5 * 1024));
        opts.configure_lop = true;
        let report = offload_blobs(&repo, &opts).expect("offload");
        assert_eq!(report.dropped_from_main, 1);
        Self {
            _dir: dir,
            main,
            lop,
            tip,
            tree,
            small,
            big,
        }
    }

    fn open(&self, fetcher: Option<Arc<dyn PromisorFetcher>>) -> Repository {
        Repository::open(
            &self.main,
            &RepositoryOptions::empty()
                .with_promisor_fetcher(fetcher)
                .for_serving(),
        )
        .unwrap()
    }
}

fn fetch_request(caps: &[&str], args: &[String]) -> Vec<u8> {
    let mut body = pkt("command=fetch\n");
    for c in caps {
        body.extend(pkt(&format!("{c}\n")));
    }
    body.extend(b"0001");
    for a in args {
        body.extend(pkt(&format!("{a}\n")));
    }
    body.extend(pkt("done\n"));
    body.extend(b"0000");
    body
}

fn want(oid: ObjectId) -> String {
    format!("want {oid}")
}

fn v2(stateless_rpc: bool) -> ServeOptions {
    ServeOptions {
        protocol: ProtocolVersion::V2,
        stateless_rpc,
        agent: "grit-test".to_owned(),
        ..ServeOptions::default()
    }
}

/// What a v2 fetch response held: its section lines, the pack (when one came) and
/// any `ERR` message.
#[derive(Default)]
struct Response {
    lines: Vec<String>,
    pack: Option<Vec<u8>>,
    err: Option<String>,
}

fn parse_v2(out: &[u8]) -> Response {
    let mut r = Cursor::new(out.to_vec());
    let mut resp = Response::default();
    let mut in_pack = false;
    loop {
        match read_packet(&mut r) {
            Ok(Some(Packet::Data(line))) if !in_pack => {
                let line = line.trim_end().to_owned();
                if let Some(msg) = line.strip_prefix("ERR ") {
                    resp.err = Some(msg.to_owned());
                }
                in_pack = line == "packfile";
                resp.lines.push(line);
            }
            Ok(Some(Packet::Data(_))) => unreachable!("pack data is read below"),
            Ok(Some(_)) if !in_pack => {}
            Ok(None) | Err(_) => break,
            Ok(Some(_)) => break,
        }
        if in_pack {
            resp.pack = Some(read_sideband_pack(&mut r));
            break;
        }
    }
    resp
}

fn read_sideband_pack(r: &mut Cursor<Vec<u8>>) -> Vec<u8> {
    use std::io::Read;
    let mut pack = Vec::new();
    loop {
        let mut len = [0u8; 4];
        if r.read_exact(&mut len).is_err() {
            break;
        }
        let len = usize::from_str_radix(std::str::from_utf8(&len).unwrap(), 16).unwrap();
        if len == 0 {
            break;
        }
        let mut payload = vec![0u8; len - 4];
        r.read_exact(&mut payload).unwrap();
        if payload[0] == 1 {
            pack.extend_from_slice(&payload[1..]);
        }
    }
    pack
}

/// The object ids in `pack`, read by indexing it into a scratch repository.
fn pack_objects(pack: Vec<u8>) -> HashSet<ObjectId> {
    let dir = tempfile::tempdir().unwrap();
    let scratch = init_bare(dir.path());
    ingest_received_pack(pack, scratch.odb(), &IngestPackOptions::default())
        .unwrap()
        .object_ids
}

fn serve(
    repo: &Repository,
    request: Vec<u8>,
    opts: &mut ServeOptions,
) -> (Vec<u8>, Result<(), ServeError>) {
    let mut out = Vec::new();
    let result = upload_pack(repo, &mut Cursor::new(request), &mut out, opts).map(|_| ());
    (out, result)
}

#[test]
fn accepting_client_gets_the_pack_without_the_promised_blob() {
    let fx = Offloaded::new();
    let repo = fx.open(None);
    for filter in [Some("filter blob:limit=5120"), None] {
        let mut args = vec![want(fx.tip)];
        args.extend(filter.map(str::to_owned));
        let (out, result) = serve(
            &repo,
            fetch_request(&["promisor-remote=lop"], &args),
            &mut v2(true),
        );
        result.unwrap();
        let resp = parse_v2(&out);
        assert_eq!(resp.err, None, "{filter:?}");
        let objects = pack_objects(resp.pack.expect("pack"));
        let expected: HashSet<ObjectId> = [fx.tip, fx.tree, fx.small].into_iter().collect();
        assert_eq!(objects, expected, "{filter:?}");
    }
    // The server still lacks the blob: nothing was fetched.
    assert!(!repo.odb().exists_stored(&fx.big));
}

#[test]
fn client_that_accepts_nothing_gets_an_err_line_naming_the_lop() {
    let fx = Offloaded::new();
    let repo = fx.open(None);
    let (out, result) = serve(&repo, fetch_request(&[], &[want(fx.tip)]), &mut v2(true));
    result.expect("stateless errors are reported in-band");
    let resp = parse_v2(&out);
    let err = resp.err.expect("ERR line");
    assert!(err.contains(&fx.big.to_hex()), "{err}");
    assert!(err.contains("promisor.acceptFromServer"), "{err}");
    assert!(err.contains("lop"), "{err}");
    assert!(resp.pack.is_none(), "no pack data before the error");

    // A stateful session reports the error both ways.
    let (out, result) = serve(&repo, fetch_request(&[], &[want(fx.tip)]), &mut v2(false));
    assert!(matches!(
        result,
        Err(ServeError::ObjectMissing {
            oid,
            reason: MissingObjectReason::Promised
        }) if oid == fx.big
    ));
    // Skip the advertisement; the response ends with the ERR line.
    let text = String::from_utf8_lossy(&out);
    assert!(text.contains("ERR object"), "{text}");
}

#[test]
fn a_client_lazily_asking_for_the_blob_itself() {
    let fx = Offloaded::new();
    let repo = fx.open(None);
    // Accepted: the want is promised, so it is left out (the client then asks the LOP).
    let (out, result) = serve(
        &repo,
        fetch_request(
            &["promisor-remote=lop"],
            &[want(fx.big), "filter blob:none".into()],
        ),
        &mut v2(true),
    );
    result.unwrap();
    let resp = parse_v2(&out);
    assert!(pack_objects(resp.pack.expect("pack")).is_empty());

    // Not accepted: refused as promised.
    let (out, _) = serve(
        &repo,
        fetch_request(&[], &[want(fx.big), "filter blob:none".into()]),
        &mut v2(true),
    );
    let err = parse_v2(&out).err.expect("ERR");
    assert!(err.contains("did not accept"), "{err}");
}

#[test]
fn lazy_fetch_brings_the_blob_in_from_the_lop_first() {
    let fx = Offloaded::new();
    let fetcher: Arc<dyn PromisorFetcher> = Arc::new(DefaultPromisorFetcher::new(Arc::new(NoHttp)));
    let repo = fx.open(Some(fetcher));
    let mut opts = v2(true);
    opts.lazy_fetch = Some(true);
    let (out, result) = serve(&repo, fetch_request(&[], &[want(fx.tip)]), &mut opts);
    result.unwrap();
    let resp = parse_v2(&out);
    assert_eq!(resp.err, None);
    let objects = pack_objects(resp.pack.expect("pack"));
    assert!(objects.contains(&fx.big));
    assert_eq!(objects.len(), 4);
    // The main repository now has the blob, in a promisor pack.
    let repo = fx.open(None);
    assert!(repo.odb().exists_stored(&fx.big));
    let promisor_packs = std::fs::read_dir(fx.main.join("objects/pack"))
        .unwrap()
        .flatten()
        .filter(|e| e.path().extension().is_some_and(|x| x == "promisor"))
        .count();
    assert_eq!(
        promisor_packs, 2,
        "offload's pack plus the lazily fetched one"
    );
}

#[test]
fn lazy_fetch_still_applies_the_clients_filter() {
    let fx = Offloaded::new();
    let fetcher: Arc<dyn PromisorFetcher> = Arc::new(DefaultPromisorFetcher::new(Arc::new(NoHttp)));
    let repo = fx.open(Some(fetcher));
    let mut opts = v2(true);
    opts.lazy_fetch = Some(true);
    let (out, result) = serve(
        &repo,
        fetch_request(&[], &[want(fx.tip), "filter blob:limit=5120".into()]),
        &mut opts,
    );
    result.unwrap();
    let resp = parse_v2(&out);
    assert_eq!(resp.err, None);
    let objects = pack_objects(resp.pack.expect("pack"));
    let expected: HashSet<ObjectId> = [fx.tip, fx.tree, fx.small].into_iter().collect();
    assert_eq!(
        objects, expected,
        "the 20 KiB blob is over the client's limit"
    );
}

#[test]
fn a_session_without_lazy_fetch_drops_the_repositorys_fetcher() {
    let fx = Offloaded::new();
    let fetcher: Arc<dyn PromisorFetcher> = Arc::new(DefaultPromisorFetcher::new(Arc::new(NoHttp)));
    // Opened with a fetcher, as a client's options would carry one.
    let repo = fx.open(Some(fetcher.clone()));
    let config = ConfigSet::new();
    let off = v2(true);
    assert!(!off.attach_promisor_fetcher(&repo, &config, Some(fetcher.clone())));
    // Forcing lazy fetch afterwards finds no fetcher: the session's choice stuck.
    let mut forced = v2(false);
    forced.lazy_fetch = Some(true);
    let (_, result) = serve(&repo, fetch_request(&[], &[want(fx.tip)]), &mut forced);
    assert!(matches!(
        result,
        Err(ServeError::ObjectMissing {
            reason: MissingObjectReason::NoFetcher,
            ..
        })
    ));
    assert!(!repo.odb().exists_stored(&fx.big));

    let mut on = v2(true);
    on.lazy_fetch = Some(true);
    assert!(on.attach_promisor_fetcher(&repo, &config, Some(fetcher)));
    let (out, result) = serve(&repo, fetch_request(&[], &[want(fx.tip)]), &mut on);
    result.unwrap();
    assert!(pack_objects(parse_v2(&out).pack.expect("pack")).contains(&fx.big));
}

#[test]
fn uploadpack_lazy_fetch_config_turns_it_on() {
    let fx = Offloaded::new();
    append_config(&fx.main, "[uploadpack]\n\tlazyFetch = true\n");
    let fetcher: Arc<dyn PromisorFetcher> = Arc::new(DefaultPromisorFetcher::new(Arc::new(NoHttp)));
    let repo = fx.open(Some(fetcher));
    let (out, result) = serve(&repo, fetch_request(&[], &[want(fx.tip)]), &mut v2(true));
    result.unwrap();
    assert!(pack_objects(parse_v2(&out).pack.expect("pack")).contains(&fx.big));
}

#[test]
fn lazy_fetch_without_a_fetcher_says_so() {
    let fx = Offloaded::new();
    let repo = fx.open(None);
    let mut opts = v2(false);
    opts.lazy_fetch = Some(true);
    let (_, result) = serve(&repo, fetch_request(&[], &[want(fx.tip)]), &mut opts);
    assert!(matches!(
        result,
        Err(ServeError::ObjectMissing {
            reason: MissingObjectReason::NoFetcher,
            ..
        })
    ));
}

#[test]
fn lazy_fetch_that_finds_nothing_fails_cleanly() {
    let fx = Offloaded::new();
    // The LOP lost the blob.
    std::fs::remove_dir_all(fx.lop.join("objects/pack")).unwrap();
    std::fs::create_dir_all(fx.lop.join("objects/pack")).unwrap();
    let fetcher: Arc<dyn PromisorFetcher> = Arc::new(DefaultPromisorFetcher::new(Arc::new(NoHttp)));
    let repo = fx.open(Some(fetcher));
    let mut opts = v2(true);
    opts.lazy_fetch = Some(true);
    let (out, _) = serve(&repo, fetch_request(&[], &[want(fx.tip)]), &mut opts);
    let resp = parse_v2(&out);
    assert!(resp.err.is_some(), "{:?}", resp.lines);
    assert!(resp.pack.is_none());
}

#[test]
fn v0_client_is_told_on_the_error_band() {
    let fx = Offloaded::new();
    let repo = fx.open(None);
    let mut request = pkt(&format!("want {} side-band-64k ofs-delta\n", fx.tip));
    request.extend(b"0000");
    request.extend(pkt("done\n"));
    let mut opts = ServeOptions {
        protocol: ProtocolVersion::V0,
        stateless_rpc: true,
        agent: "grit-test".to_owned(),
        ..ServeOptions::default()
    };
    let (out, result) = serve(&repo, request, &mut opts);
    assert!(matches!(
        result,
        Err(ServeError::ObjectMissing {
            reason: MissingObjectReason::Promised,
            ..
        })
    ));
    let text = String::from_utf8_lossy(&out);
    assert!(text.contains("\u{3}object"), "{text}");
}

#[test]
fn v0_want_of_a_promised_blob_gets_an_err_line() {
    let fx = Offloaded::new();
    let repo = fx.open(None);
    let mut request = pkt(&format!("want {} side-band-64k ofs-delta\n", fx.big));
    request.extend(b"0000");
    request.extend(pkt("done\n"));
    let mut opts = ServeOptions {
        protocol: ProtocolVersion::V0,
        stateless_rpc: true,
        agent: "grit-test".to_owned(),
        ..ServeOptions::default()
    };
    let (out, result) = serve(&repo, request, &mut opts);
    assert!(matches!(
        result,
        Err(ServeError::ObjectMissing {
            reason: MissingObjectReason::Promised,
            ..
        })
    ));
    let mut reader = Cursor::new(out);
    match read_packet(&mut reader).unwrap() {
        Some(Packet::Data(line)) => {
            assert!(line.starts_with("ERR object"), "{line}");
            assert!(line.contains("promisor.acceptFromServer"), "{line}");
        }
        other => panic!("expected an ERR line, got {other:?}"),
    }
}

#[test]
fn a_truly_missing_blob_is_not_promised() {
    let fx = Offloaded::new();
    // Add a commit whose tree names a blob nobody has, outside any promisor pack.
    let repo = fx.open(None);
    let ghost = ObjectId::from_hex("0123456789abcdef0123456789abcdef01234567").unwrap();
    let mut tree = b"100644 ghost\0".to_vec();
    tree.extend_from_slice(ghost.as_bytes());
    let tree = repo.odb().write(ObjectKind::Tree, &tree).unwrap();
    let commit = format!(
        "tree {tree}\nparent {}\nauthor T <t@e.com> 200 +0000\ncommitter T <t@e.com> 200 +0000\n\nghost\n",
        fx.tip
    );
    let tip = repo
        .odb()
        .write(ObjectKind::Commit, commit.as_bytes())
        .unwrap();
    std::fs::write(fx.main.join("refs/heads/main"), format!("{tip}\n")).unwrap();
    let repo = fx.open(None);
    let mut opts = v2(false);
    let (_, result) = serve(
        &repo,
        fetch_request(&["promisor-remote=lop"], &[want(tip)]),
        &mut opts,
    );
    assert!(matches!(
        result,
        Err(ServeError::ObjectMissing {
            oid,
            reason: MissingObjectReason::NotPromised
        }) if oid == ghost
    ));
}

#[test]
fn repositories_without_promisor_remotes_keep_the_old_path() {
    // A missing blob in an ordinary repository is still a repository error from the
    // pack writer, not a planned ObjectMissing: the planner never runs for it.
    let dir = tempfile::tempdir().unwrap();
    let repo = init_bare(dir.path());
    let ghost = ObjectId::from_hex("0123456789abcdef0123456789abcdef01234567").unwrap();
    let mut tree = b"100644 ghost\0".to_vec();
    tree.extend_from_slice(ghost.as_bytes());
    let tree = repo.odb().write(ObjectKind::Tree, &tree).unwrap();
    let commit =
        format!("tree {tree}\nauthor T <t@e.com> 1 +0000\ncommitter T <t@e.com> 1 +0000\n\nc\n");
    let tip = repo
        .odb()
        .write(ObjectKind::Commit, commit.as_bytes())
        .unwrap();
    std::fs::write(dir.path().join("refs/heads/main"), format!("{tip}\n")).unwrap();
    let repo = Repository::open(dir.path(), &RepositoryOptions::empty().for_serving()).unwrap();
    let (_, result) = serve(&repo, fetch_request(&[], &[want(tip)]), &mut v2(false));
    match result {
        Err(ServeError::ObjectMissing { .. }) => panic!("planner ran for an ordinary repository"),
        Err(_) => {}
        Ok(()) => panic!("a missing blob cannot be served"),
    }
}

#[test]
fn environment_maps_git_no_lazy_fetch() {
    use grit_lib::config::environment::Environment;
    let env = |vars: &[(&str, &str)]| {
        Environment::from_vars(
            vars.iter()
                .map(|(k, v)| (std::ffi::OsString::from(k), std::ffi::OsString::from(v))),
            PathBuf::from("/"),
        )
    };
    assert_eq!(ServeOptions::lazy_fetch_from_environment(&env(&[])), None);
    assert_eq!(
        ServeOptions::lazy_fetch_from_environment(&env(&[("GIT_NO_LAZY_FETCH", "0")])),
        Some(true)
    );
    assert_eq!(
        ServeOptions::lazy_fetch_from_environment(&env(&[("GIT_NO_LAZY_FETCH", "true")])),
        Some(false)
    );
    assert_eq!(
        ServeOptions::lazy_fetch_from_environment(&env(&[
            ("GIT_NO_LAZY_FETCH", "1"),
            ("GRIT_NO_LAZY_FETCH", "no")
        ])),
        Some(true),
        "grit's variable wins"
    );
    assert_eq!(
        ServeOptions::lazy_fetch_from_environment(&env(&[("GIT_NO_LAZY_FETCH", "maybe")])),
        None
    );

    let mut config = ConfigSet::new();
    assert!(!v2(true).lazy_fetch_enabled(&config));
    config
        .add_command_override("uploadpack.lazyfetch", "true")
        .unwrap();
    assert!(v2(true).lazy_fetch_enabled(&config));
    let mut opts = v2(true);
    opts.lazy_fetch = Some(false);
    assert!(!opts.lazy_fetch_enabled(&config));
}

#[test]
fn fsck_of_an_offloaded_repository_never_fetches() {
    use grit_lib::maintenance::repository_fsck::{fsck, FsckOptions};
    let fx = Offloaded::new();
    // A fetcher is installed, as the CLI does for every command; fsck must not use it.
    let fetcher: Arc<dyn PromisorFetcher> = Arc::new(DefaultPromisorFetcher::new(Arc::new(NoHttp)));
    let repo = fx.open(Some(fetcher));
    let report = fsck(
        &repo,
        &FsckOptions::default(),
        &mut grit_lib::progress::NullProgress,
        &grit_lib::progress::NeverCancel,
    )
    .unwrap();
    assert!(report.issues.is_empty(), "{:?}", report.issues);
    assert!(
        !repo.odb().exists_stored(&fx.big),
        "the promised blob stayed on the LOP"
    );
}

/// Push a commit on top of `fx.tip` whose tree keeps the offloaded blob and names
/// `extra` next to a new small file, as a client editing a file next to a large
/// one sends it: a pack of the new commit, tree and blob only (fewer objects than
/// `receive.unpackLimit`, so they are unpacked loose).
fn push_edit(fx: &Offloaded, extra: Option<(&str, ObjectId)>) -> (ObjectId, ReceiveOutcome) {
    let client_dir = tempfile::tempdir().unwrap();
    let client = init_bare(client_dir.path());
    let odb = client.odb();
    let small = odb.write(ObjectKind::Blob, b"edited\n").unwrap();
    let mut entries = vec![("big", fx.big), ("small", small)];
    entries.extend(extra);
    entries.sort_by_key(|(name, _)| *name);
    let mut tree = Vec::new();
    for (name, oid) in &entries {
        tree.extend_from_slice(format!("100644 {name}\0").as_bytes());
        tree.extend_from_slice(oid.as_bytes());
    }
    let tree = odb.write(ObjectKind::Tree, &tree).unwrap();
    let commit = format!(
        "tree {tree}\nparent {}\nauthor T <t@e.com> 200 +0000\ncommitter T <t@e.com> 200 +0000\n\nedit\n",
        fx.tip
    );
    let commit = odb.write(ObjectKind::Commit, commit.as_bytes()).unwrap();
    let pack = grit_lib::objects::pack_objects::build_pack_from_send_list(
        odb,
        &[commit, tree, small],
        &HashSet::new(),
        &grit_lib::objects::pack_objects::PackBuildOptions::default(),
        &std::collections::HashMap::new(),
    )
    .unwrap();
    let mut body = pkt(&format!(
        "{} {commit} refs/heads/main\0report-status\n",
        fx.tip
    ));
    body.extend_from_slice(b"0000");
    body.extend_from_slice(&pack);
    let opts = ServeOptions {
        protocol: ProtocolVersion::V0,
        stateless_rpc: true,
        agent: "grit-test".to_owned(),
        ..ServeOptions::default()
    };
    let repo = fx.open(None);
    let mut out = Vec::new();
    let outcome = receive_pack(
        &repo,
        &mut Cursor::new(body),
        &mut out,
        &opts,
        &ReceivePolicy::default(),
    )
    .unwrap();
    (commit, outcome)
}

#[test]
fn small_push_naming_an_offloaded_blob_is_accepted() {
    let fx = Offloaded::new();
    let (commit, outcome) = push_edit(&fx, None);
    assert!(outcome.unpack_error.is_none(), "{:?}", outcome.unpack_error);
    assert!(
        outcome.updates.iter().all(|u| u.error.is_none()),
        "{outcome:?}"
    );
    let repo = fx.open(None);
    assert_eq!(
        grit_lib::refs::resolve_ref(repo.git_dir(), "refs/heads/main").unwrap(),
        commit
    );
    assert!(!repo.odb().exists_stored(&fx.big), "nothing was fetched");

    // fsck accepts the pushed tree's promised blob (Git's is_promisor_object).
    let report = grit_lib::maintenance::repository_fsck::fsck(
        &repo,
        &grit_lib::maintenance::repository_fsck::FsckOptions::default(),
        &mut grit_lib::progress::NullProgress,
        &grit_lib::progress::NeverCancel,
    )
    .unwrap();
    assert!(report.issues.is_empty(), "{:?}", report.issues);

    // An accepting client still gets the new tip without the blob.
    let (out, result) = serve(
        &repo,
        fetch_request(&["promisor-remote=lop"], &[want(commit)]),
        &mut v2(true),
    );
    result.unwrap();
    let objects = pack_objects(parse_v2(&out).pack.expect("pack"));
    assert!(objects.contains(&commit));
    assert!(!objects.contains(&fx.big));
}

#[test]
fn small_push_naming_a_blob_nobody_promises_is_still_refused() {
    let fx = Offloaded::new();
    let ghost = ObjectId::from_hex("0123456789abcdef0123456789abcdef01234567").unwrap();
    let (_, outcome) = push_edit(&fx, Some(("ghost", ghost)));
    let err = outcome.unpack_error.expect("unpack error");
    assert!(err.to_string().contains(&ghost.to_hex()), "{err}");
}

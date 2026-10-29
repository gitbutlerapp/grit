//! Client side of the `promisor-remote` exchange, against grit's own `upload_pack` run
//! in-process: what the fetch sends, what it accepts, and what it records.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::io::{BufReader, Read, Write};
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;

use grit_lib::config::environment::RepositoryOptions;
use grit_lib::config::ConfigSet;
use grit_lib::objects::promisor_remote::{PromisorField, PromisorRejection};
use grit_lib::objects::{ObjectId, ObjectKind};
use grit_lib::progress::NeverCancel;
use grit_lib::refs::ref_storage::RefStorageFormat;
use grit_lib::remote::fetch::{fetch_remote, NoProgress};
use grit_lib::remote::partial_clone::ObjectFilterSpec;
use grit_lib::remote::serve::{upload_pack, ProtocolVersion, ServeOptions};
use grit_lib::remote::transfer::{FetchOptions, FetchOutcome};
use grit_lib::remote::transport::{read_advertisement, Advertisement, Connection};
use grit_lib::repo::{InitOptions, Repository};

/// A v2 connection to `upload_pack` running on a thread, recording every request byte.
struct InProcessConn {
    writer: Option<TeeWriter>,
    reader: BufReader<std::io::PipeReader>,
    adv: Advertisement,
    server: Option<JoinHandle<ServeOptions>>,
}

struct TeeWriter {
    pipe: std::io::PipeWriter,
    sent: Arc<Mutex<Vec<u8>>>,
}

impl Write for TeeWriter {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        let n = self.pipe.write(buf)?;
        self.sent.lock().unwrap().extend_from_slice(&buf[..n]);
        Ok(n)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.pipe.flush()
    }
}

impl InProcessConn {
    fn connect(server_git_dir: &Path) -> (Self, Arc<Mutex<Vec<u8>>>) {
        let (to_server_r, to_server_w) = std::io::pipe().unwrap();
        let (to_client_r, to_client_w) = std::io::pipe().unwrap();
        let dir = server_git_dir.to_path_buf();
        let server = std::thread::spawn(move || {
            let repo = Repository::open(&dir, &RepositoryOptions::empty()).unwrap();
            let mut opts = ServeOptions {
                protocol: ProtocolVersion::V2,
                agent: "grit-test".to_owned(),
                ..ServeOptions::default()
            };
            let mut input = to_server_r;
            let mut output = to_client_w;
            upload_pack(&repo, &mut input, &mut output, &mut opts).unwrap();
            opts
        });
        let mut reader = BufReader::new(to_client_r);
        let adv = read_advertisement(&mut reader).unwrap();
        let sent = Arc::new(Mutex::new(Vec::new()));
        (
            Self {
                writer: Some(TeeWriter {
                    pipe: to_server_w,
                    sent: Arc::clone(&sent),
                }),
                reader,
                adv,
                server: Some(server),
            },
            sent,
        )
    }

    /// Close the request side and return the server's options after the session.
    fn finish(mut self) -> ServeOptions {
        self.writer = None;
        self.server.take().unwrap().join().unwrap()
    }
}

impl Connection for InProcessConn {
    fn reader(&mut self) -> &mut dyn Read {
        &mut self.reader
    }

    fn writer(&mut self) -> &mut dyn Write {
        self.writer.as_mut().unwrap()
    }

    fn advertised_refs(&self) -> &[(String, ObjectId)] {
        &self.adv.refs
    }

    fn capabilities(&self) -> &[String] {
        &self.adv.capabilities
    }

    fn head_symref(&self) -> Option<&str> {
        self.adv.head_symref.as_deref()
    }

    fn protocol_version(&self) -> u8 {
        self.adv.protocol_version
    }

    fn finish_send(&mut self) {
        self.writer = None;
    }
}

fn init(dir: &Path, bare: bool) -> Repository {
    Repository::init(
        dir,
        &InitOptions::new()
            .with_bare(bare)
            .with_initial_branch("main")
            .with_ref_storage(RefStorageFormat::Files),
    )
    .unwrap()
}

fn append_config(git_dir: &Path, text: &str) {
    let path = git_dir.join("config");
    let mut config = std::fs::read_to_string(&path).unwrap();
    config.push_str(text);
    std::fs::write(path, config).unwrap();
}

struct Server {
    _tmp: tempfile::TempDir,
    git_dir: std::path::PathBuf,
    big: ObjectId,
    small: ObjectId,
}

/// A bare server with `main` holding a small and a 20 KiB blob, advertising promisor
/// remote `lop` with `partialCloneFilter=blob:limit=5k`.
fn server(advertise: bool) -> Server {
    let tmp = tempfile::tempdir().unwrap();
    let git_dir = tmp.path().join("server.git");
    let repo = init(&git_dir, true);
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
    append_config(
        &git_dir,
        &format!(
            "[uploadpack]\n\tallowFilter = true\n\
             [promisor]\n\tadvertise = {advertise}\n\tsendFields = partialCloneFilter token\n\
             [remote \"lop\"]\n\turl = file:///srv/lop\n\tpromisor = true\n\
             \tpartialCloneFilter = blob:limit=5k\n\ttoken = s3cret-token\n"
        ),
    );
    Server {
        _tmp: tmp,
        git_dir,
        big,
        small,
    }
}

/// An empty client repository with `extra` appended to its config.
fn client(extra: &str) -> tempfile::TempDir {
    let tmp = tempfile::tempdir().unwrap();
    init(tmp.path(), true);
    append_config(
        tmp.path(),
        &format!("[remote \"origin\"]\n\turl = file:///srv/server.git\n{extra}"),
    );
    tmp
}

const KNOWN_LOP: &str = "[remote \"lop\"]\n\turl = file:///srv/lop\n\tpromisor = true\n\
    [promisor]\n\tacceptFromServer = knownUrl\n";

fn fetch(
    server: &Server,
    client: &Path,
    filter: Option<ObjectFilterSpec>,
    auto_filter: bool,
) -> (FetchOutcome, String, ServeOptions) {
    fetch_from("origin", server, client, filter, auto_filter)
}

/// [`fetch`] as remote `remote`.
fn fetch_from(
    remote: &str,
    server: &Server,
    client: &Path,
    filter: Option<ObjectFilterSpec>,
    auto_filter: bool,
) -> (FetchOutcome, String, ServeOptions) {
    let (mut conn, sent) = InProcessConn::connect(&server.git_dir);
    let mut opts = FetchOptions::new();
    opts.refspecs = vec![format!("+refs/heads/*:refs/remotes/{remote}/*")];
    opts.remote_name = Some(remote.to_owned());
    opts.filter = filter;
    opts.auto_filter = auto_filter;
    let outcome =
        fetch_remote(client, &mut conn, &mut opts, &mut NoProgress, &NeverCancel).expect("fetch");
    let served = conn.finish();
    let sent = String::from_utf8_lossy(&sent.lock().unwrap()).into_owned();
    (outcome, sent, served)
}

fn has_object(git_dir: &Path, oid: &ObjectId) -> bool {
    let repo = Repository::open(git_dir, &RepositoryOptions::empty()).unwrap();
    repo.odb().contains(oid).unwrap()
}

fn promisor_marked_packs(git_dir: &Path) -> usize {
    std::fs::read_dir(git_dir.join("objects/pack"))
        .unwrap()
        .filter_map(Result::ok)
        .filter(|e| e.path().extension().is_some_and(|x| x == "promisor"))
        .count()
}

#[test]
fn accepted_lop_is_sent_with_ls_refs_and_fetch() {
    let server = server(true);
    let client = client(KNOWN_LOP);
    let (outcome, sent, served) = fetch(
        &server,
        client.path(),
        Some(ObjectFilterSpec::BlobLimit(5 * 1024)),
        false,
    );
    assert_eq!(
        sent.matches("promisor-remote=lop\n").count(),
        2,
        "reply on both commands:\n{sent}"
    );
    assert_eq!(served.accepted_promisor_remotes, ["lop"]);
    let handshake = outcome.promisor.expect("handshake");
    assert_eq!(handshake.decision.accepted_names(), ["lop"]);
    assert!(handshake.stored.is_empty());
    assert!(has_object(client.path(), &server.small));
    assert!(!has_object(client.path(), &server.big));
    assert_eq!(promisor_marked_packs(client.path()), 1);
}

#[test]
fn no_reply_when_policy_is_none() {
    let server = server(true);
    let client = client("[remote \"lop\"]\n\turl = file:///srv/lop\n\tpromisor = true\n");
    let (outcome, sent, served) = fetch(
        &server,
        client.path(),
        Some(ObjectFilterSpec::BlobNone),
        false,
    );
    assert!(!sent.contains("promisor-remote"), "{sent}");
    assert!(served.accepted_promisor_remotes.is_empty());
    let handshake = outcome.promisor.expect("handshake");
    assert_eq!(
        handshake.decision.candidates[0].rejection,
        Some(PromisorRejection::PolicyNone)
    );
}

#[test]
fn unfiltered_fetch_declines_the_lop() {
    let server = server(true);
    let client = client(KNOWN_LOP);
    let (outcome, sent, _) = fetch(&server, client.path(), None, false);
    assert!(!sent.contains("promisor-remote"), "{sent}");
    assert_eq!(
        outcome.promisor.unwrap().decision.candidates[0].rejection,
        Some(PromisorRejection::NotPartialFetch)
    );
    assert!(has_object(client.path(), &server.big));
    assert_eq!(promisor_marked_packs(client.path()), 0);
}

#[test]
fn unfiltered_fetch_from_a_second_remote_of_a_partial_clone_declines_the_lop() {
    // A partial clone of origin fetching everything from another, non-promisor
    // remote: accepting would mark that pack promisor although no promisor remote
    // can give its objects back.
    let server = server(true);
    let client = client(&format!(
        "\tpromisor = true\n{KNOWN_LOP}[extensions]\n\tpartialClone = origin\n\
         [remote \"upstream\"]\n\turl = file:///srv/upstream.git\n"
    ));
    let (outcome, sent, served) = fetch_from("upstream", &server, client.path(), None, false);
    assert!(!sent.contains("promisor-remote"), "{sent}");
    assert!(served.accepted_promisor_remotes.is_empty());
    assert_eq!(
        outcome.promisor.unwrap().decision.candidates[0].rejection,
        Some(PromisorRejection::NotPartialFetch)
    );
    assert!(has_object(client.path(), &server.big));
    assert_eq!(promisor_marked_packs(client.path()), 0);
}

#[test]
fn unfiltered_fetch_from_a_promisor_remote_accepts_the_lop() {
    let server = server(true);
    let client = client(&format!(
        "\tpromisor = true\n{KNOWN_LOP}[extensions]\n\tpartialClone = origin\n"
    ));
    let (outcome, sent, served) = fetch(&server, client.path(), None, false);
    assert_eq!(sent.matches("promisor-remote=lop\n").count(), 2, "{sent}");
    assert_eq!(served.accepted_promisor_remotes, ["lop"]);
    assert_eq!(outcome.promisor.unwrap().decision.accepted_names(), ["lop"]);
    assert_eq!(promisor_marked_packs(client.path()), 1);
}

#[test]
fn no_handshake_when_server_does_not_advertise() {
    let server = server(false);
    let client = client(KNOWN_LOP);
    let (outcome, sent, _) = fetch(
        &server,
        client.path(),
        Some(ObjectFilterSpec::BlobNone),
        false,
    );
    assert!(outcome.promisor.is_none());
    assert!(!sent.contains("promisor-remote"), "{sent}");
}

#[test]
fn auto_filter_uses_the_accepted_lop_filter() {
    let server = server(true);
    let client = client(KNOWN_LOP);
    let (outcome, sent, _) = fetch(&server, client.path(), None, true);
    assert!(sent.contains("filter blob:limit=5120\n"), "{sent}");
    let handshake = outcome.promisor.unwrap();
    assert_eq!(
        handshake.resolved_filter,
        Some(ObjectFilterSpec::BlobLimit(5120))
    );
    assert!(has_object(client.path(), &server.small));
    assert!(!has_object(client.path(), &server.big));
    assert_eq!(promisor_marked_packs(client.path()), 1);
}

#[test]
fn stored_auto_filter_turns_on_auto_filter() {
    let server = server(true);
    let client = client(&format!(
        "{KNOWN_LOP}[remote \"origin\"]\n\tpromisor = true\n\tpartialCloneFilter = auto\n"
    ));
    let (outcome, sent, _) = fetch(&server, client.path(), None, false);
    assert!(sent.contains("filter blob:limit=5120\n"), "{sent}");
    assert!(outcome.promisor.unwrap().decision.any_accepted());
}

#[test]
fn store_fields_updates_the_known_lop() {
    let server = server(true);
    let client = client(&format!(
        "{KNOWN_LOP}\tstoreFields = partialCloneFilter,token\n"
    ));
    let (outcome, sent, _) = fetch(
        &server,
        client.path(),
        Some(ObjectFilterSpec::BlobNone),
        false,
    );
    let stored: Vec<PromisorField> = outcome
        .promisor
        .unwrap()
        .stored
        .iter()
        .map(|s| s.field)
        .collect();
    assert_eq!(
        stored,
        [PromisorField::PartialCloneFilter, PromisorField::Token]
    );
    let cfg = ConfigSet::load_repo_local_only(client.path()).unwrap();
    assert_eq!(
        cfg.get("remote.lop.partialclonefilter").as_deref(),
        Some("blob:limit=5k")
    );
    assert_eq!(cfg.get("remote.lop.token").as_deref(), Some("s3cret-token"));
    assert_eq!(
        cfg.get("remote.lop.url").as_deref(),
        Some("file:///srv/lop")
    );
    // The reply names the remote, never its token.
    assert!(!sent.contains("s3cret-token"), "{sent}");
}

#[test]
fn object_want_fetch_sends_no_reply() {
    let server = server(true);
    let client = client(KNOWN_LOP);
    fetch(
        &server,
        client.path(),
        Some(ObjectFilterSpec::BlobNone),
        false,
    );
    assert!(!has_object(client.path(), &server.big));

    let (mut conn, sent) = InProcessConn::connect(&server.git_dir);
    let mut opts = FetchOptions::new().promisor_object_wants(vec![server.big]);
    opts.remote_name = Some("origin".to_owned());
    opts.filter = Some(ObjectFilterSpec::BlobNone);
    let outcome = fetch_remote(
        client.path(),
        &mut conn,
        &mut opts,
        &mut NoProgress,
        &NeverCancel,
    )
    .expect("object fetch");
    conn.finish();
    let sent = String::from_utf8_lossy(&sent.lock().unwrap()).into_owned();
    assert!(!sent.contains("promisor-remote"), "{sent}");
    assert!(outcome.promisor.is_none());
    assert!(has_object(client.path(), &server.big));
}

#[test]
fn handshake_debug_output_hides_the_token() {
    let server = server(true);
    let client = client("[promisor]\n\tacceptFromServer = all\n");
    let (outcome, _, _) = fetch(
        &server,
        client.path(),
        Some(ObjectFilterSpec::BlobNone),
        false,
    );
    let handshake = outcome.promisor.unwrap();
    assert_eq!(
        handshake.decision.candidates[0].advertised.token.as_deref(),
        Some("s3cret-token")
    );
    assert!(!format!("{handshake:?}").contains("s3cret-token"));
}

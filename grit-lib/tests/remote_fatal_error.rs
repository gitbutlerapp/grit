//! A server that aborts with its own text (an `ERR` pkt-line or a side-band channel-3
//! packet) surfaces as [`Error::RemoteFatal`] carrying exactly that text.
//!
//! Driven by in-memory connections, so every case runs without a network or system `git`.

use std::io::{Cursor, Read, Write};
use std::path::{Path, PathBuf};

use grit_lib::error::Error;
use grit_lib::objects::ObjectId;
use grit_lib::progress::NeverCancel;
use grit_lib::refs::resolve_ref;
use grit_lib::remote::fetch::{fetch_remote, NoProgress};
use grit_lib::remote::pkt_line;
use grit_lib::remote::transfer::{FetchOptions, TagMode};
use grit_lib::remote::transport::http::{
    discover_upload_pack_for_list_refs_from_body, discover_upload_pack_from_body,
};
use grit_lib::remote::transport::{read_advertisement, Connection};
use grit_lib::repo::{InitOptions, Repository};

struct ScriptedConn {
    reader: Cursor<Vec<u8>>,
    sink: Vec<u8>,
    refs: Vec<(String, ObjectId)>,
    caps: Vec<String>,
}

impl ScriptedConn {
    fn new(server_script: Vec<u8>, tip: ObjectId) -> Self {
        Self {
            reader: Cursor::new(server_script),
            sink: Vec::new(),
            refs: vec![("refs/heads/main".to_owned(), tip)],
            caps: ["side-band-64k", "multi_ack_detailed", "ofs-delta"]
                .map(str::to_owned)
                .to_vec(),
        }
    }
}

impl Connection for ScriptedConn {
    fn reader(&mut self) -> &mut dyn Read {
        &mut self.reader
    }
    fn writer(&mut self) -> &mut dyn Write {
        &mut self.sink
    }
    fn advertised_refs(&self) -> &[(String, ObjectId)] {
        &self.refs
    }
    fn capabilities(&self) -> &[String] {
        &self.caps
    }
    fn head_symref(&self) -> Option<&str> {
        None
    }
    fn protocol_version(&self) -> u8 {
        0
    }
}

fn empty_local(root: &Path) -> PathBuf {
    let repo =
        Repository::init(root, &InitOptions::new().with_initial_branch("main")).expect("init");
    repo.git_dir().to_path_buf()
}

fn absent_oid() -> ObjectId {
    ObjectId::from_hex(&"b".repeat(40)).expect("oid")
}

fn line(text: &str) -> Vec<u8> {
    let mut buf = Vec::new();
    pkt_line::write_line_to_vec(&mut buf, text).expect("pkt-line");
    buf
}

fn remote_fatal_message(err: Error) -> String {
    match err {
        Error::RemoteFatal { message } => message,
        other => panic!("expected Error::RemoteFatal, got {other:?}"),
    }
}

/// Fetch `refs/heads/*` from a v0 server whose post-`done` reply is `server_script`.
fn fetch_err(server_script: Vec<u8>) -> (Error, PathBuf, tempfile::TempDir) {
    let tmp = tempfile::tempdir().expect("tempdir");
    let local_git = empty_local(&tmp.path().join("local"));
    let mut conn = ScriptedConn::new(server_script, absent_oid());
    let mut opts = FetchOptions::new();
    opts.refspecs = vec!["+refs/heads/*:refs/remotes/origin/*".to_owned()];
    opts.tags = TagMode::None;
    let err = fetch_remote(
        &local_git,
        &mut conn,
        &mut opts,
        &mut NoProgress,
        &NeverCancel,
    )
    .expect_err("the server aborted the fetch");
    (err, local_git, tmp)
}

#[test]
fn advertisement_err_line_is_remote_fatal() {
    let mut wire = line("ERR access denied or repository not exported: /srv/x.git");
    wire.extend_from_slice(b"0000");
    let err = read_advertisement(&mut Cursor::new(wire)).expect_err("ERR advertisement");
    assert_eq!(err.kind_name(), "remote_fatal");
    assert_eq!(
        err.to_string(),
        "remote error: access denied or repository not exported: /srv/x.git"
    );
    assert_eq!(
        remote_fatal_message(err),
        "access denied or repository not exported: /srv/x.git"
    );
}

#[test]
fn v2_capability_block_err_is_remote_fatal() {
    let mut wire = line("version 2");
    wire.extend(line("agent=git/2.99"));
    wire.extend(line("ERR service not enabled"));
    wire.extend_from_slice(b"0000");
    let err = read_advertisement(&mut Cursor::new(wire)).expect_err("v2 ERR");
    assert_eq!(remote_fatal_message(err), "service not enabled");
}

/// A smart-HTTP `info/refs` body: the optional `# service=` preamble, then `lines`.
fn http_info_refs(preamble: bool, lines: &[&str]) -> Vec<u8> {
    let mut body = Vec::new();
    if preamble {
        body.extend(line("# service=git-upload-pack"));
        body.extend_from_slice(b"0000");
    }
    for text in lines {
        body.extend(line(text));
    }
    body.extend_from_slice(b"0000");
    body
}

fn assert_http_discovery_fatal(body: &[u8], expected: &str) {
    for (entry, result) in [
        ("discover", discover_upload_pack_from_body(body).map(|_| ())),
        (
            "list_refs",
            discover_upload_pack_for_list_refs_from_body(body).map(|_| ()),
        ),
    ] {
        let err = result.expect_err("an ERR info/refs body must fail");
        assert_eq!(remote_fatal_message(err), expected, "{entry}");
    }
}

#[test]
fn smart_http_v0_err_is_remote_fatal() {
    for preamble in [true, false] {
        let body = http_info_refs(preamble, &["ERR access denied"]);
        assert_http_discovery_fatal(&body, "access denied");
    }
}

#[test]
fn smart_http_v0_err_after_first_ref_is_remote_fatal() {
    let oid = "a".repeat(40);
    let first = format!("{oid} refs/heads/main\0multi_ack side-band-64k");
    let body = http_info_refs(true, &[&first, "ERR repository is being moved"]);
    assert_http_discovery_fatal(&body, "repository is being moved");
}

#[test]
fn smart_http_v2_err_is_remote_fatal() {
    for preamble in [true, false] {
        let body = http_info_refs(
            preamble,
            &["version 2", "agent=git/2.99", "ERR access denied"],
        );
        assert_http_discovery_fatal(&body, "access denied");
    }
}

#[test]
fn fetch_err_after_done_is_remote_fatal() {
    let (err, local_git, _tmp) = fetch_err(line("ERR upload-pack: not our ref bbbb"));
    assert_eq!(remote_fatal_message(err), "upload-pack: not our ref bbbb");
    assert!(resolve_ref(&local_git, "refs/remotes/origin/main").is_err());
}

#[test]
fn fetch_sideband_channel3_is_remote_fatal() {
    let mut script = line("NAK");
    pkt_line::write_packet_raw(&mut script, b"\x03fatal: the requested object is corrupt\n")
        .expect("band 3");
    let (err, local_git, _tmp) = fetch_err(script);
    assert_eq!(
        remote_fatal_message(err),
        "fatal: the requested object is corrupt"
    );
    assert!(resolve_ref(&local_git, "refs/remotes/origin/main").is_err());
}

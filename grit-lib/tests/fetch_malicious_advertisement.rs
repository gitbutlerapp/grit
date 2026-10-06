//! Security regression: malformed network ref advertisements must not corrupt
//! repository metadata (e.g. `.git/config`) during fetch.

use std::fs;
use std::io::{Cursor, Read, Write};
use std::path::{Path, PathBuf};
use std::process::Command;

use grit_lib::fetch::{fetch_remote, NoProgress};
use grit_lib::objects::ObjectId;
use grit_lib::pkt_line;
use grit_lib::refs::resolve_ref;
use grit_lib::transfer::{FetchOptions, TagMode};
use grit_lib::transport::{read_advertisement, Connection};

const MALICIOUS: &str = "refs/heads/../../../config";

struct ScriptedConn {
    reader: Cursor<Vec<u8>>,
    sink: Vec<u8>,
    refs: Vec<(String, ObjectId)>,
    caps: Vec<String>,
    head_symref: Option<String>,
    version: u8,
}

impl ScriptedConn {
    fn new(server_script: Vec<u8>) -> Self {
        Self {
            reader: Cursor::new(server_script),
            sink: Vec::new(),
            refs: Vec::new(),
            caps: Vec::new(),
            head_symref: None,
            version: 0,
        }
    }
    fn with_ref(mut self, name: &str, oid: ObjectId) -> Self {
        self.refs.push((name.to_owned(), oid));
        self
    }
    fn with_caps(mut self, caps: &[&str]) -> Self {
        self.caps = caps.iter().map(|c| (*c).to_owned()).collect();
        self
    }
    fn with_version(mut self, v: u8) -> Self {
        self.version = v;
        self
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
        self.head_symref.as_deref()
    }
    fn protocol_version(&self) -> u8 {
        self.version
    }
}

fn init_repo_with_commit(root: &Path) -> (PathBuf, ObjectId) {
    fs::create_dir_all(root).unwrap();
    Command::new("git")
        .args(["init", "-q", "-b", "main"])
        .current_dir(root)
        .status()
        .expect("git init");
    fs::write(root.join("payload"), b"fetch-fixture\n").unwrap();
    Command::new("git")
        .args(["add", "payload"])
        .current_dir(root)
        .status()
        .unwrap();
    Command::new("git")
        .args([
            "-c",
            "user.email=fixture@example.invalid",
            "-c",
            "user.name=Fixture",
            "commit",
            "-qm",
            "initial",
        ])
        .current_dir(root)
        .status()
        .unwrap();
    let out = Command::new("git")
        .args(["rev-parse", "HEAD"])
        .current_dir(root)
        .output()
        .unwrap();
    let hex = String::from_utf8(out.stdout).unwrap();
    let hex = hex.trim();
    (
        root.join(".git"),
        ObjectId::from_hex(hex).expect("HEAD oid"),
    )
}

fn config_bytes(git_dir: &Path) -> Vec<u8> {
    fs::read(git_dir.join("config")).expect("config exists")
}

fn assert_config_unchanged(git_dir: &Path, before: &[u8]) {
    let after = config_bytes(git_dir);
    assert_eq!(before, after, ".git/config must be byte-for-byte unchanged");
}

fn assert_no_tracking_ref_for_traversal(git_dir: &Path) {
    assert!(
        resolve_ref(git_dir, "refs/remotes/origin/../../../config").is_err(),
        "must not create a tracking ref for traversal-shaped names"
    );
}

#[test]
fn fetch_v0_malicious_advertisement_preserves_config() {
    let tmp = tempfile::tempdir().unwrap();
    let (git_dir, oid) = init_repo_with_commit(&tmp.path().join("client"));
    let config_before = config_bytes(&git_dir);

    let conn = ScriptedConn::new(Vec::new())
        .with_ref(MALICIOUS, oid)
        .with_caps(&["multi_ack_detailed", "side-band-64k", "ofs-delta"])
        .with_version(0);

    let opts = FetchOptions {
        refspecs: vec!["+refs/heads/*:refs/remotes/origin/*".to_owned()],
        tags: TagMode::None,
        ..Default::default()
    };

    let mut conn = conn;
    fetch_remote(&git_dir, &mut conn, &opts, &mut NoProgress).expect("fetch completes");
    assert_config_unchanged(&git_dir, &config_before);
    assert_no_tracking_ref_for_traversal(&git_dir);
}

#[test]
fn fetch_v2_malicious_ls_refs_preserves_config() {
    let tmp = tempfile::tempdir().unwrap();
    let (git_dir, oid) = init_repo_with_commit(&tmp.path().join("client"));
    let config_before = config_bytes(&git_dir);

    let mut script = Vec::new();
    pkt_line::write_line(&mut script, &format!("{oid} {MALICIOUS}")).unwrap();
    pkt_line::write_flush(&mut script).unwrap();

    let conn = ScriptedConn::new(script)
        .with_caps(&[
            "version 2",
            "ls-refs",
            "fetch=shallow",
            "object-format=sha1",
        ])
        .with_version(2);

    let opts = FetchOptions {
        refspecs: vec!["+refs/heads/*:refs/remotes/origin/*".to_owned()],
        tags: TagMode::None,
        ..Default::default()
    };

    let mut conn = conn;
    fetch_remote(&git_dir, &mut conn, &opts, &mut NoProgress).expect("v2 fetch completes");
    assert_config_unchanged(&git_dir, &config_before);
    assert_no_tracking_ref_for_traversal(&git_dir);
}

#[test]
fn read_advertisement_v0_filters_malicious_ref_and_symref() {
    let oid = "aabbccddeeff00112233445566778899aabbccdd";
    let mut buf = Vec::new();
    let head = format!("{oid} HEAD\0symref=HEAD:{MALICIOUS} multi_ack");
    pkt_line::write_line(&mut buf, &head).unwrap();
    pkt_line::write_line(&mut buf, &format!("{oid} {MALICIOUS}")).unwrap();
    pkt_line::write_flush(&mut buf).unwrap();

    let adv = read_advertisement(&mut Cursor::new(buf)).unwrap();
    assert!(
        adv.head_symref.is_none(),
        "invalid symref target must be dropped"
    );
    assert!(
        adv.refs.is_empty(),
        "malicious advertised ref must not be recorded"
    );
}

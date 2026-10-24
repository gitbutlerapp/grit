//! In-process `upload_pack` coverage (HTTP compat tests spawn a separate binary).

#![allow(clippy::unwrap_used, clippy::expect_used)]

use grit_lib::config::environment::RepositoryOptions;
use grit_lib::objects::{ObjectId, ObjectKind};
use grit_lib::refs::ref_storage::RefStorageFormat;
use grit_lib::remote::serve::{upload_pack, ProtocolVersion, ServeOptions};
use grit_lib::repo::{InitOptions, Repository};
use std::io::Cursor;

fn pkt_line(payload: &str) -> Vec<u8> {
    let bytes = payload.as_bytes();
    format!("{:04x}{}", bytes.len() + 4, payload).into_bytes()
}

fn seed_bare_main(repo: &Repository) -> ObjectId {
    let blob = repo
        .odb()
        .write_loose_materialize(ObjectKind::Blob, b"payload\n")
        .expect("blob");
    let tree_body = format!("100644 file.txt\0").into_bytes();
    let mut tree_bytes = tree_body;
    tree_bytes.extend_from_slice(blob.as_bytes());
    let tree = repo
        .odb()
        .write_loose_materialize(ObjectKind::Tree, &tree_bytes)
        .expect("tree");
    let commit_body = format!(
        "tree {tree}\nauthor T <t@e.com> 100 +0000\ncommitter T <t@e.com> 100 +0000\n\nseed\n"
    );
    let tip = repo
        .odb()
        .write_loose_materialize(ObjectKind::Commit, commit_body.as_bytes())
        .expect("commit");
    std::fs::write(repo.git_dir().join("refs/heads/main"), format!("{tip}\n")).expect("ref");
    tip
}

fn v0_stateless_fetch_body(want: ObjectId) -> Vec<u8> {
    let caps = "multi_ack_detailed side-band-64k thin-pack no-progress include-tag ofs-delta";
    let mut body = pkt_line(&format!("want {} {caps}\n", want.to_hex()));
    body.extend_from_slice(b"0000");
    body.extend_from_slice(&pkt_line("done"));
    body.extend_from_slice(b"0000");
    body
}

#[test]
fn upload_pack_v0_advertise_and_stateless_fetch() {
    let dir = tempfile::tempdir().expect("tempdir");
    Repository::init(
        dir.path(),
        &InitOptions::new()
            .with_bare(true)
            .with_initial_branch("main")
            .with_ref_storage(RefStorageFormat::Files),
    )
    .expect("init");
    let repo = Repository::open(dir.path(), &RepositoryOptions::empty()).expect("open");
    let tip = seed_bare_main(&repo);

    let mut ad_out = Vec::new();
    upload_pack(
        &repo,
        &mut Cursor::new([] as [u8; 0]),
        &mut ad_out,
        &ServeOptions {
            protocol: ProtocolVersion::V0,
            advertise_refs: true,
            stateless_rpc: false,
            agent: "grit-cov".to_owned(),
            hidden_refs: Vec::new(),
        },
    )
    .expect("advertise");
    assert!(ad_out.len() > 20);

    let mut pack_out = Vec::new();
    upload_pack(
        &repo,
        &mut Cursor::new(v0_stateless_fetch_body(tip)),
        &mut pack_out,
        &ServeOptions {
            protocol: ProtocolVersion::V0,
            advertise_refs: false,
            stateless_rpc: true,
            agent: "grit-cov".to_owned(),
            hidden_refs: Vec::new(),
        },
    )
    .expect("fetch");
    assert!(pack_out.len() > 50);
}

#[test]
fn upload_pack_v1_and_v2_entry_points() {
    let dir = tempfile::tempdir().expect("tempdir");
    Repository::init(
        dir.path(),
        &InitOptions::new()
            .with_bare(true)
            .with_initial_branch("main")
            .with_ref_storage(RefStorageFormat::Files),
    )
    .expect("init");
    let repo = Repository::open(dir.path(), &RepositoryOptions::empty()).expect("open");
    let tip = seed_bare_main(&repo);

    let mut v1_out = Vec::new();
    upload_pack(
        &repo,
        &mut Cursor::new([] as [u8; 0]),
        &mut v1_out,
        &ServeOptions {
            protocol: ProtocolVersion::V1,
            advertise_refs: true,
            stateless_rpc: false,
            agent: "grit-cov".to_owned(),
            hidden_refs: Vec::new(),
        },
    )
    .expect("v1 ad");
    assert!(v1_out.windows(9).any(|w| w == b"version 1"));

    let v2_ls = pkt_line("command=ls-refs\n");
    let mut v2_out = Vec::new();
    upload_pack(
        &repo,
        &mut Cursor::new(v2_ls),
        &mut v2_out,
        &ServeOptions {
            protocol: ProtocolVersion::V2,
            advertise_refs: false,
            stateless_rpc: true,
            agent: "grit-cov".to_owned(),
            hidden_refs: Vec::new(),
        },
    )
    .expect("v2 ls-refs");
    assert!(!v2_out.is_empty());

    let mut v2_ad = Vec::new();
    upload_pack(
        &repo,
        &mut Cursor::new([] as [u8; 0]),
        &mut v2_ad,
        &ServeOptions {
            protocol: ProtocolVersion::V2,
            advertise_refs: true,
            stateless_rpc: false,
            agent: "grit-cov".to_owned(),
            hidden_refs: Vec::new(),
        },
    )
    .expect("v2 capability ad");
    assert!(v2_ad.windows(9).any(|w| w == b"version 2"));

    let mut v2_fetch = Vec::new();
    let mut fetch_body = pkt_line("command=fetch\n");
    fetch_body.extend_from_slice(&pkt_line(&format!("agent=grit-cov\n")));
    fetch_body.extend_from_slice(&pkt_line(&format!("want {0}\n", tip.to_hex())));
    fetch_body.extend_from_slice(b"0000");
    fetch_body.extend_from_slice(&pkt_line("done"));
    fetch_body.extend_from_slice(b"0000");
    upload_pack(
        &repo,
        &mut Cursor::new(fetch_body),
        &mut v2_fetch,
        &ServeOptions {
            protocol: ProtocolVersion::V2,
            advertise_refs: false,
            stateless_rpc: true,
            agent: "grit-cov".to_owned(),
            hidden_refs: Vec::new(),
        },
    )
    .expect("v2 fetch");
    assert!(v2_fetch.len() > 20);
}

/// Serving a full clone of a repository packed by system git copies its deltas,
/// streams a pack git accepts (`index-pack --strict`, `fsck --full`), and stays
/// close to the source pack's size.
#[test]
fn upload_pack_serves_git_packed_history_with_its_deltas() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let src = grit_test_support::git_packed_history(tmp.path(), 120);
    let tip = grit_test_support::git(&src, &["rev-parse", "main"]);
    let tip = ObjectId::from_hex(tip.trim()).expect("tip");
    let source_pack_len: u64 = std::fs::read_dir(src.join("objects/pack"))
        .expect("pack dir")
        .map(|e| e.expect("entry").path())
        .filter(|p| p.extension().is_some_and(|e| e == "pack"))
        .map(|p| std::fs::metadata(p).expect("pack metadata").len())
        .sum();

    let repo = Repository::open(&src, &RepositoryOptions::empty()).expect("open");
    let caps = "multi_ack_detailed side-band-64k no-progress ofs-delta";
    let mut body = pkt_line(&format!("want {} {caps}\n", tip.to_hex()));
    body.extend_from_slice(b"0000");
    body.extend_from_slice(&pkt_line("done\n"));
    let mut response = Vec::new();
    upload_pack(
        &repo,
        &mut Cursor::new(body),
        &mut response,
        &ServeOptions {
            protocol: ProtocolVersion::V0,
            advertise_refs: false,
            stateless_rpc: true,
            agent: "grit-cov".to_owned(),
            hidden_refs: Vec::new(),
        },
    )
    .expect("fetch");
    assert!(response.starts_with(b"0008NAK\n"));
    let pack = grit_lib::remote::pkt_line::decode_sideband_primary(&response).expect("sideband");
    assert!(pack.starts_with(b"PACK"));
    assert!(
        (pack.len() as u64) < source_pack_len * 11 / 10,
        "served pack ({} bytes) reuses the source deltas ({source_pack_len} bytes)",
        pack.len()
    );

    let dst = tmp.path().join("clone.git");
    let dst_arg = dst.to_string_lossy().into_owned();
    grit_test_support::git(tmp.path(), &["init", "-q", "--bare", &dst_arg]);
    grit_test_support::git_cmd(&["index-pack", "--strict", "--stdin"])
        .in_dir(&dst)
        .stdin(pack)
        .suc();
    grit_test_support::git(&dst, &["update-ref", "refs/heads/main", &tip.to_hex()]);
    grit_test_support::git(&dst, &["fsck", "--full", "--strict"]);
    let count = |dir: &std::path::Path| {
        grit_test_support::git(dir, &["rev-list", "--objects", "main"])
            .lines()
            .count()
    };
    assert_eq!(count(&dst), count(&src));
}

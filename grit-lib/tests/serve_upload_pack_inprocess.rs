//! In-process `upload_pack` coverage (HTTP compat tests spawn a separate binary).

#![allow(clippy::unwrap_used, clippy::expect_used)]

use grit_lib::config::environment::RepositoryOptions;
use grit_lib::objects::{ObjectId, ObjectKind};
use grit_lib::progress::FlagCancel;
use grit_lib::refs::ref_storage::RefStorageFormat;
use grit_lib::remote::serve::{
    upload_pack, upload_pack_cancellable, ProtocolVersion, ServeError, ServeOptions,
};
use grit_lib::repo::{InitOptions, Repository};
use std::io::Cursor;

fn pkt_line(payload: &str) -> Vec<u8> {
    let bytes = payload.as_bytes();
    format!("{:04x}{}", bytes.len() + 4, payload).into_bytes()
}

fn test_serve_opts(
    protocol: ProtocolVersion,
    advertise_refs: bool,
    stateless_rpc: bool,
) -> ServeOptions {
    ServeOptions {
        protocol,
        advertise_refs,
        stateless_rpc,
        agent: "grit-cov".to_owned(),
        hidden_refs: Vec::new(),
        ..ServeOptions::default()
    }
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

fn v0_stateless_fetch_with_have_flush(want: ObjectId, have: ObjectId) -> Vec<u8> {
    let caps = "multi_ack_detailed side-band-64k thin-pack no-progress include-tag ofs-delta";
    let mut body = pkt_line(&format!("want {} {caps}\n", want.to_hex()));
    body.extend_from_slice(b"0000");
    body.extend_from_slice(&pkt_line(&format!("have {}\n", have.to_hex())));
    body.extend_from_slice(b"0000");
    body.extend_from_slice(&pkt_line("done"));
    body.extend_from_slice(b"0000");
    body
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
    let mut serve_opts = test_serve_opts(ProtocolVersion::V0, true, false);
    upload_pack(
        &repo,
        &mut Cursor::new([] as [u8; 0]),
        &mut ad_out,
        &mut serve_opts,
    )
    .expect("advertise");
    assert!(ad_out.len() > 20);

    let mut pack_out = Vec::new();
    let mut serve_opts = test_serve_opts(ProtocolVersion::V0, false, true);
    upload_pack(
        &repo,
        &mut Cursor::new(v0_stateless_fetch_body(tip)),
        &mut pack_out,
        &mut serve_opts,
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
    let mut serve_opts = test_serve_opts(ProtocolVersion::V1, true, false);
    upload_pack(
        &repo,
        &mut Cursor::new([] as [u8; 0]),
        &mut v1_out,
        &mut serve_opts,
    )
    .expect("v1 ad");
    assert!(v1_out.windows(9).any(|w| w == b"version 1"));

    let v2_ls = pkt_line("command=ls-refs\n");
    let mut v2_out = Vec::new();
    let mut serve_opts = test_serve_opts(ProtocolVersion::V2, false, true);
    upload_pack(&repo, &mut Cursor::new(v2_ls), &mut v2_out, &mut serve_opts).expect("v2 ls-refs");
    assert!(!v2_out.is_empty());

    let mut v2_ad = Vec::new();
    let mut serve_opts = test_serve_opts(ProtocolVersion::V2, true, false);
    upload_pack(
        &repo,
        &mut Cursor::new([] as [u8; 0]),
        &mut v2_ad,
        &mut serve_opts,
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
    let mut serve_opts = test_serve_opts(ProtocolVersion::V2, false, true);
    upload_pack(
        &repo,
        &mut Cursor::new(fetch_body),
        &mut v2_fetch,
        &mut serve_opts,
    )
    .expect("v2 fetch");
    assert!(v2_fetch.len() > 20);
}

/// Concatenate the band-1 payloads of a side-band pkt-line stream, up to its flush.
fn sideband_band1(mut input: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    loop {
        let len = grit_lib::remote::pkt_line::parse_hex_len(&input[..4]).expect("pkt length");
        if len == 0 {
            return out;
        }
        let payload = &input[4..len];
        if payload[0] == 1 {
            out.extend_from_slice(&payload[1..]);
        }
        input = &input[len..];
    }
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
    let mut serve_opts = test_serve_opts(ProtocolVersion::V0, false, true);
    upload_pack(
        &repo,
        &mut Cursor::new(body),
        &mut response,
        &mut serve_opts,
    )
    .expect("fetch");
    assert!(response.starts_with(b"0008NAK\n"));
    let pack = sideband_band1(&response["0008NAK\n".len()..]);
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

fn seed_three_linear_commits(repo: &Repository) -> (ObjectId, ObjectId) {
    let empty_tree =
        ObjectId::from_hex("4b825dc642cb6eb9a060e54bf8d69288fbee4904").expect("empty tree");
    let _ = repo
        .odb()
        .write_loose_materialize(ObjectKind::Tree, &[] as &[u8]);
    let mut parent: Option<ObjectId> = None;
    let mut chain = Vec::new();
    for i in 1..=3 {
        let mut body = format!("tree {}\n", empty_tree.to_hex());
        if let Some(p) = parent {
            body.push_str(&format!("parent {}\n", p.to_hex()));
        }
        body.push_str("author T <t@e.com> 100 +0000\ncommitter T <t@e.com> 100 +0000\n\n");
        body.push_str(&format!("c{i}\n"));
        let oid = repo
            .odb()
            .write_loose_materialize(ObjectKind::Commit, body.as_bytes())
            .expect("commit");
        chain.push(oid);
        parent = Some(oid);
    }
    let tip = chain[2];
    let middle = chain[1];
    std::fs::write(repo.git_dir().join("refs/heads/main"), format!("{tip}\n")).expect("ref");
    (tip, middle)
}

/// Issue #1939: stateless RPC must accept a want for a reachable non-tip commit.
#[test]
fn upload_pack_stateless_v0_want_reachable_non_tip() {
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
    let (_tip, middle) = seed_three_linear_commits(&repo);

    let mut pack_out = Vec::new();
    upload_pack(
        &repo,
        &mut Cursor::new(v0_stateless_fetch_body(middle)),
        &mut pack_out,
        &mut ServeOptions {
            protocol: ProtocolVersion::V0,
            advertise_refs: false,
            stateless_rpc: true,
            agent: "grit-cov".to_owned(),
            hidden_refs: Vec::new(),
            ..ServeOptions::default()
        },
    )
    .expect("stateless want for ancestor must succeed");
    assert!(pack_out.starts_with(b"0008NAK\n"));
    let pack = sideband_band1(&pack_out["0008NAK\n".len()..]);
    assert!(pack.starts_with(b"PACK"));

    let wt = dir.path().join("git-check");
    grit_test_support::git(dir.path(), &["init", "-q", &wt.to_string_lossy()]);
    for i in 1..=3 {
        grit_test_support::git(
            &wt,
            &["commit", "-q", "--allow-empty", "-m", &format!("c{i}")],
        );
    }
    let git_middle =
        ObjectId::from_hex(grit_test_support::git(&wt, &["rev-parse", "HEAD~1"]).trim())
            .expect("git middle");
    let mut git_body = pkt_line(&format!("want {} ofs-delta\n", git_middle.to_hex()));
    git_body.extend_from_slice(b"0000");
    git_body.extend_from_slice(&pkt_line("done\n"));
    git_body.extend_from_slice(b"0000");
    let git_out = grit_test_support::git_cmd(&["upload-pack", "--stateless-rpc", "."])
        .in_dir(&wt.join(".git"))
        .stdin(git_body)
        .suc()
        .stdout;
    assert!(git_out.as_bytes().starts_with(b"0008NAK\n"));
}

#[test]
fn upload_pack_stateful_v0_rejects_non_advertised_ancestor_want() {
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
    let (_tip, middle) = seed_three_linear_commits(&repo);

    let caps = "multi_ack_detailed side-band-64k thin-pack no-progress include-tag ofs-delta";
    let mut body = pkt_line(&format!("want {} {caps}\n", middle.to_hex()));
    body.extend_from_slice(b"0000");
    body.extend_from_slice(&pkt_line("done\n"));

    let err = upload_pack(
        &repo,
        &mut Cursor::new(body),
        &mut Vec::new(),
        &mut ServeOptions {
            protocol: ProtocolVersion::V0,
            advertise_refs: false,
            stateless_rpc: false,
            agent: "grit-cov".to_owned(),
            hidden_refs: Vec::new(),
            ..ServeOptions::default()
        },
    )
    .expect_err("stateful session should reject ancestor not in advertisement");
    assert!(
        err.to_string().contains("not our ref"),
        "unexpected error: {err}"
    );
}

#[test]
fn upload_pack_stateless_v2_want_reachable_non_tip() {
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
    let (_tip, middle) = seed_three_linear_commits(&repo);

    let mut fetch_body = pkt_line("command=fetch\n");
    fetch_body.extend_from_slice(&pkt_line("agent=grit-cov\n"));
    fetch_body.extend_from_slice(&pkt_line(&format!("want {0}\n", middle.to_hex())));
    fetch_body.extend_from_slice(b"0000");
    fetch_body.extend_from_slice(&pkt_line("done"));
    fetch_body.extend_from_slice(b"0000");
    let mut v2_fetch = Vec::new();
    upload_pack(
        &repo,
        &mut Cursor::new(fetch_body),
        &mut v2_fetch,
        &mut ServeOptions {
            protocol: ProtocolVersion::V2,
            advertise_refs: false,
            stateless_rpc: true,
            agent: "grit-cov".to_owned(),
            hidden_refs: Vec::new(),
            ..ServeOptions::default()
        },
    )
    .expect("v2 stateless fetch for ancestor");
    assert!(v2_fetch.len() > 20);
}

/// With `uploadpack.allowTipSha1InWant`, stateless RPC must still accept reachable
/// non-tip commits (Git does not narrow stateless reachability to tips only).
#[test]
fn upload_pack_stateless_allow_tip_sha1_in_want_accepts_reachable_ancestor() {
    let dir = tempfile::tempdir().expect("tempdir");
    Repository::init(
        dir.path(),
        &InitOptions::new()
            .with_bare(true)
            .with_initial_branch("main")
            .with_ref_storage(RefStorageFormat::Files),
    )
    .expect("init");
    grit_test_support::git(
        dir.path(),
        &["config", "uploadpack.allowTipSha1InWant", "true"],
    );
    let repo = Repository::open(dir.path(), &RepositoryOptions::empty()).expect("open");
    let (_tip, middle) = seed_three_linear_commits(&repo);

    upload_pack(
        &repo,
        &mut Cursor::new(v0_stateless_fetch_body(middle)),
        &mut Vec::new(),
        &mut ServeOptions {
            protocol: ProtocolVersion::V0,
            advertise_refs: false,
            stateless_rpc: true,
            agent: "grit-cov".to_owned(),
            hidden_refs: Vec::new(),
            ..ServeOptions::default()
        },
    )
    .expect("allowTipSha1InWant must not block stateless reachable ancestor wants");
}

#[test]
fn upload_pack_stateless_v0_want_current_annotated_tag_object() {
    let dir = tempfile::tempdir().expect("tempdir");
    let wt = dir.path().join("wt");
    grit_test_support::git(dir.path(), &["init", "-q", &wt.to_string_lossy()]);
    grit_test_support::git(&wt, &["commit", "-q", "--allow-empty", "-m", "c1"]);
    grit_test_support::git(&wt, &["tag", "-a", "v1", "-m", "annotated"]);
    let tag_obj =
        ObjectId::from_hex(grit_test_support::git(&wt, &["rev-parse", "refs/tags/v1"]).trim())
            .expect("tag oid");
    let repo = Repository::open(&wt.join(".git"), &RepositoryOptions::empty()).expect("open");

    let mut pack_out = Vec::new();
    upload_pack(
        &repo,
        &mut Cursor::new(v0_stateless_fetch_body(tag_obj)),
        &mut pack_out,
        &mut ServeOptions {
            protocol: ProtocolVersion::V0,
            advertise_refs: false,
            stateless_rpc: true,
            agent: "grit-cov".to_owned(),
            hidden_refs: Vec::new(),
            ..ServeOptions::default()
        },
    )
    .expect("stateless want for annotated tag object on current ref");
    assert!(pack_out.starts_with(b"0008NAK\n"));
    let pack = sideband_band1(&pack_out["0008NAK\n".len()..]);
    assert!(pack.starts_with(b"PACK"));
}

#[test]
fn upload_pack_stateless_v0_want_advertised_tip_skips_reachability_walk() {
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
    let (tip, _middle) = seed_three_linear_commits(&repo);

    upload_pack(
        &repo,
        &mut Cursor::new(v0_stateless_fetch_body(tip)),
        &mut Vec::new(),
        &mut ServeOptions {
            protocol: ProtocolVersion::V0,
            advertise_refs: false,
            stateless_rpc: true,
            agent: "grit-cov".to_owned(),
            hidden_refs: Vec::new(),
            ..ServeOptions::default()
        },
    )
    .expect("current tip want uses advertisement fast path");
}

fn grit_bin_for_subprocess() -> Option<std::path::PathBuf> {
    if let Ok(p) = std::env::var("CARGO_BIN_EXE_grit") {
        return Some(p.into());
    }
    let candidate =
        std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../target/debug/grit");
    candidate.is_file().then_some(candidate)
}

fn v0_multi_ack_caps() -> &'static str {
    "multi_ack_detailed side-band-64k thin-pack no-progress include-tag ofs-delta"
}

fn seed_linear_commits(repo: &Repository, n: usize) -> Vec<ObjectId> {
    let empty_tree =
        ObjectId::from_hex("4b825dc642cb6eb9a060e54bf8d69288fbee4904").expect("empty tree");
    let _ = repo
        .odb()
        .write_loose_materialize(ObjectKind::Tree, &[] as &[u8]);
    let mut parent: Option<ObjectId> = None;
    let mut chain = Vec::new();
    for i in 1..=n {
        let mut body = format!("tree {}\n", empty_tree.to_hex());
        if let Some(p) = parent {
            body.push_str(&format!("parent {}\n", p.to_hex()));
        }
        body.push_str("author T <t@e.com> 100 +0000\ncommitter T <t@e.com> 100 +0000\n\n");
        body.push_str(&format!("c{i}\n"));
        let oid = repo
            .odb()
            .write_loose_materialize(ObjectKind::Commit, body.as_bytes())
            .expect("commit");
        chain.push(oid);
        parent = Some(oid);
    }
    if let Some(tip) = chain.last().copied() {
        std::fs::write(repo.git_dir().join("refs/heads/main"), format!("{tip}\n")).expect("ref");
    }
    chain
}

/// Fork: root -> a1 -> tip (main), root -> side.
fn seed_fork(repo: &Repository) -> (ObjectId, ObjectId, ObjectId) {
    let empty_tree =
        ObjectId::from_hex("4b825dc642cb6eb9a060e54bf8d69288fbee4904").expect("empty tree");
    let root_body = format!(
        "tree {}\nauthor T <t@e.com> 100 +0000\ncommitter T <t@e.com> 100 +0000\n\nroot\n",
        empty_tree.to_hex()
    );
    let root = repo
        .odb()
        .write_loose_materialize(ObjectKind::Commit, root_body.as_bytes())
        .expect("root");
    let a1_body = format!(
        "tree {}\nparent {}\nauthor T <t@e.com> 101 +0000\ncommitter T <t@e.com> 101 +0000\n\na1\n",
        empty_tree.to_hex(),
        root.to_hex()
    );
    let a1 = repo
        .odb()
        .write_loose_materialize(ObjectKind::Commit, a1_body.as_bytes())
        .expect("a1");
    let tip_body = format!(
        "tree {}\nparent {}\nauthor T <t@e.com> 102 +0000\ncommitter T <t@e.com> 102 +0000\n\ntip\n",
        empty_tree.to_hex(),
        a1.to_hex()
    );
    let tip = repo
        .odb()
        .write_loose_materialize(ObjectKind::Commit, tip_body.as_bytes())
        .expect("tip");
    let b1_body = format!(
        "tree {}\nparent {}\nauthor T <t@e.com> 103 +0000\ncommitter T <t@e.com> 103 +0000\n\nb1\n",
        empty_tree.to_hex(),
        root.to_hex()
    );
    let side = repo
        .odb()
        .write_loose_materialize(ObjectKind::Commit, b1_body.as_bytes())
        .expect("b1");
    std::fs::write(repo.git_dir().join("refs/heads/main"), format!("{tip}\n")).expect("ref");
    (tip, a1, side)
}

fn count_substr(haystack: &[u8], needle: &str) -> usize {
    haystack
        .windows(needle.len())
        .filter(|w| *w == needle.as_bytes())
        .count()
}

#[test]
fn upload_pack_v0_multi_ack_detailed_emits_ready_on_flush_when_haves_cover() {
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
    let chain = seed_linear_commits(&repo, 5);
    let tip = chain[4];
    let ancestor = chain[2];

    let caps = v0_multi_ack_caps();
    let mut body = pkt_line(&format!("want {} {caps}\n", tip.to_hex()));
    body.extend_from_slice(b"0000");
    body.extend_from_slice(&pkt_line(&format!("have {}", ancestor.to_hex())));
    body.extend_from_slice(b"0000");
    body.extend_from_slice(&pkt_line("done\n"));

    let mut out = Vec::new();
    upload_pack(
        &repo,
        &mut Cursor::new(body),
        &mut out,
        &mut test_serve_opts(ProtocolVersion::V0, false, false),
    )
    .expect("negotiation");
    let text = String::from_utf8_lossy(&out);
    assert!(
        text.contains(&format!("ACK {} common", ancestor.to_hex())),
        "expected common ACK: {text}"
    );
    assert!(
        text.contains(&format!("ACK {} ready", ancestor.to_hex())),
        "expected ready when haves cover wants: {text}"
    );
    assert_eq!(
        count_substr(&out, " ready\n"),
        1,
        "single ready before done: {text}"
    );
    assert!(text.contains("NAK"), "expected NAK end of round: {text}");
    assert!(out.windows(4).any(|w| w == b"PACK"), "pack after done");
}

#[test]
fn upload_pack_v0_multi_ack_detailed_no_ready_when_haves_do_not_cover() {
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
    let (tip, _a1, side) = seed_fork(&repo);

    let caps = v0_multi_ack_caps();
    let mut body = pkt_line(&format!("want {} {caps}\n", tip.to_hex()));
    body.extend_from_slice(b"0000");
    body.extend_from_slice(&pkt_line(&format!("have {}", side.to_hex())));
    body.extend_from_slice(b"0000");
    body.extend_from_slice(&pkt_line("done\n"));

    let mut out = Vec::new();
    upload_pack(
        &repo,
        &mut Cursor::new(body),
        &mut out,
        &mut test_serve_opts(ProtocolVersion::V0, false, false),
    )
    .expect("negotiation");
    let text = String::from_utf8_lossy(&out);
    assert!(text.contains(&format!("ACK {} common", side.to_hex())));
    assert!(
        !text.contains(" ready\n"),
        "must not emit ready when haves do not cover wants: {text}"
    );
}

#[test]
fn upload_pack_v0_multi_ack_detailed_ready_on_absent_have_not_on_flush() {
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
    let chain = seed_linear_commits(&repo, 4);
    let tip = chain[3];
    let ancestor = chain[1];
    let absent =
        ObjectId::from_hex("1234567890123456789012345678901234567890").expect("absent oid");

    let caps = v0_multi_ack_caps();
    let mut body = pkt_line(&format!("want {} {caps}\n", tip.to_hex()));
    body.extend_from_slice(b"0000");
    body.extend_from_slice(&pkt_line(&format!("have {}", ancestor.to_hex())));
    body.extend_from_slice(&pkt_line(&format!("have {}", absent.to_hex())));
    body.extend_from_slice(b"0000");
    body.extend_from_slice(&pkt_line("done\n"));

    let mut out = Vec::new();
    upload_pack(
        &repo,
        &mut Cursor::new(body),
        &mut out,
        &mut test_serve_opts(ProtocolVersion::V0, false, false),
    )
    .expect("negotiation");
    let text = String::from_utf8_lossy(&out);
    assert!(
        text.contains(&format!("ACK {} ready", absent.to_hex())),
        "absent have should get ready on that oid: {text}"
    );
    assert_eq!(
        count_substr(&out, " ready\n"),
        1,
        "must not emit a second ready on flush: {text}"
    );
}

/// Absent have before a covering common have: Git suppresses flush `ready` (common+NAK only).
#[test]
fn upload_pack_v0_multi_ack_detailed_absent_have_before_common_suppresses_flush_ready() {
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
    let chain = seed_linear_commits(&repo, 4);
    let tip = chain[3];
    let ancestor = chain[1];
    let absent =
        ObjectId::from_hex("1234567890123456789012345678901234567890").expect("absent oid");

    let caps = v0_multi_ack_caps();
    let mut body = pkt_line(&format!("want {} {caps}\n", tip.to_hex()));
    body.extend_from_slice(b"0000");
    body.extend_from_slice(&pkt_line(&format!("have {}", absent.to_hex())));
    body.extend_from_slice(&pkt_line(&format!("have {}", ancestor.to_hex())));
    body.extend_from_slice(b"0000");
    body.extend_from_slice(&pkt_line("done\n"));

    let mut out = Vec::new();
    upload_pack(
        &repo,
        &mut Cursor::new(body),
        &mut out,
        &mut test_serve_opts(ProtocolVersion::V0, false, false),
    )
    .expect("negotiation");
    let text = String::from_utf8_lossy(&out);
    assert!(
        text.contains(&format!("ACK {} common", ancestor.to_hex())),
        "expected common ACK: {text}"
    );
    assert!(
        !text.contains(" ready\n"),
        "must not emit ready on flush after absent-then-common round: {text}"
    );
    assert!(text.contains("NAK"), "expected NAK: {text}");
}

#[test]
fn upload_pack_stateless_v0_multi_ack_detailed_ready_without_done() {
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
    let chain = seed_linear_commits(&repo, 4);
    let tip = chain[3];
    let ancestor = chain[1];

    let caps = v0_multi_ack_caps();
    let mut body = pkt_line(&format!("want {} {caps}\n", tip.to_hex()));
    body.extend_from_slice(b"0000");
    body.extend_from_slice(&pkt_line(&format!("have {}", ancestor.to_hex())));
    body.extend_from_slice(b"0000");

    let mut out = Vec::new();
    upload_pack(
        &repo,
        &mut Cursor::new(body),
        &mut out,
        &mut test_serve_opts(ProtocolVersion::V0, false, true),
    )
    .expect("stateless negotiation");
    let text = String::from_utf8_lossy(&out);
    assert!(text.contains(&format!("ACK {} ready", ancestor.to_hex())));
    assert!(text.contains("NAK"));
    assert!(!out.windows(4).any(|w| w == b"PACK"));
}

#[test]
fn upload_pack_v0_git_client_sees_ready_with_few_haves() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let src = grit_test_support::git_packed_history(tmp.path(), 80);
    let behind = ObjectId::from_hex(grit_test_support::git(&src, &["rev-parse", "main~25"]).trim())
        .expect("behind");

    let client = tmp.path().join("client.git");
    grit_test_support::git(
        tmp.path(),
        &["init", "-q", "--bare", client.to_str().unwrap()],
    );
    grit_test_support::git(
        &client,
        &[
            "fetch",
            "-q",
            src.to_str().unwrap(),
            &format!("{}:refs/heads/old", behind.to_hex()),
        ],
    );

    let grit_upload = grit_bin_for_subprocess()
        .expect("build grit-cli for subprocess test")
        .canonicalize()
        .expect("grit binary path");
    let trace = grit_test_support::git_cmd(&[
        "-c",
        "protocol.version=0",
        "fetch",
        "--upload-pack",
        &format!("{} upload-pack", grit_upload.display()),
        src.to_str().unwrap(),
        "main:refs/remotes/o/main",
    ])
    .in_dir(&client)
    .env("GIT_TRACE_PACKET", "1")
    .suc()
    .stderr;

    assert!(
        trace.contains("ready"),
        "git client should receive ACK ready: {trace}"
    );
    let have_count = trace.matches("fetch> have").count();
    assert!(
        have_count <= 64,
        "expected ~first window of haves, not full history walk (got {have_count}): tail {}",
        &trace[trace.len().saturating_sub(2000)..]
    );

    grit_test_support::git(&client, &["fsck", "--full"]);
}

#[test]
fn upload_pack_v0_have_flush_counts_negotiation_round() {
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
    let body = v0_stateless_fetch_with_have_flush(tip, tip);
    let mut out = Vec::new();
    let mut opts = test_serve_opts(ProtocolVersion::V0, false, true);
    let summary = upload_pack(&repo, &mut Cursor::new(body), &mut out, &mut opts).expect("fetch");
    assert_eq!(summary.negotiation_rounds, 1);
    assert!(summary.bytes_written > 0);
}

#[test]
fn upload_pack_cancelled_before_start_writes_no_pack() {
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
    let cancel = FlagCancel::new();
    cancel.cancel();
    let mut out = Vec::new();
    let mut opts = test_serve_opts(ProtocolVersion::V0, false, true);
    let err = upload_pack_cancellable(
        &repo,
        &mut Cursor::new(v0_stateless_fetch_body(tip)),
        &mut out,
        &mut opts,
        &cancel,
    )
    .expect_err("cancelled");
    assert!(matches!(err, ServeError::Cancelled));
    assert!(!out.windows(4).any(|w| w == b"PACK"));
}

/// Annotated tag on a blob reachable from `main`, with the tag ref removed (smart HTTP race).
#[test]
fn upload_pack_stateless_v0_want_moved_annotated_blob_tag() {
    let dir = tempfile::tempdir().expect("tempdir");
    let wt = dir.path().join("wt");
    grit_test_support::git(dir.path(), &["init", "-q", &wt.to_string_lossy()]);
    std::fs::write(wt.join("payload.txt"), b"tagged blob\n").expect("file");
    grit_test_support::git(&wt, &["add", "payload.txt"]);
    grit_test_support::git(&wt, &["commit", "-q", "-m", "seed"]);
    let blob = grit_test_support::git(&wt, &["rev-parse", "HEAD:payload.txt"])
        .trim()
        .to_owned();
    grit_test_support::git(&wt, &["tag", "-a", "blobtag", "-m", "on blob", &blob]);
    let tag_obj =
        ObjectId::from_hex(grit_test_support::git(&wt, &["rev-parse", "refs/tags/blobtag"]).trim())
            .expect("tag oid");
    std::fs::remove_file(wt.join(".git/refs/tags/blobtag")).expect("drop tag ref");

    let repo = Repository::open(&wt.join(".git"), &RepositoryOptions::empty()).expect("open");
    let mut pack_out = Vec::new();
    upload_pack(
        &repo,
        &mut Cursor::new(v0_stateless_fetch_body(tag_obj)),
        &mut pack_out,
        &mut ServeOptions {
            protocol: ProtocolVersion::V0,
            advertise_refs: false,
            stateless_rpc: true,
            agent: "grit-cov".to_owned(),
            hidden_refs: Vec::new(),
            ..ServeOptions::default()
        },
    )
    .expect("stateless want for tag on reachable blob after tag ref moved away");
    assert!(pack_out.starts_with(b"0008NAK\n"));
    let pack = sideband_band1(&pack_out["0008NAK\n".len()..]);
    assert!(pack.starts_with(b"PACK"));

    let mut git_body = pkt_line(&format!("want {} ofs-delta\n", tag_obj.to_hex()));
    git_body.extend_from_slice(b"0000");
    git_body.extend_from_slice(&pkt_line("done\n"));
    git_body.extend_from_slice(b"0000");
    let git_out = grit_test_support::git_cmd(&["upload-pack", "--stateless-rpc", "."])
        .in_dir(&wt.join(".git"))
        .stdin(git_body)
        .suc()
        .stdout;
    assert!(git_out.as_bytes().starts_with(b"0008NAK\n"));
}

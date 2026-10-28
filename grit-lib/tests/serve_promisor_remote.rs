//! `upload_pack` advertises the `promisor-remote` capability, records which promisor
//! remotes the client accepted, and honors `uploadpack.allowAnySHA1InWant`.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::io::Cursor;
use std::path::Path;

use grit_lib::config::environment::RepositoryOptions;
use grit_lib::objects::{ObjectId, ObjectKind};
use grit_lib::refs::ref_storage::RefStorageFormat;
use grit_lib::remote::serve::{upload_pack, ProtocolVersion, ServeError, ServeOptions};
use grit_lib::repo::{InitOptions, Repository};

fn pkt(payload: &str) -> Vec<u8> {
    format!("{:04x}{payload}", payload.len() + 4).into_bytes()
}

/// A bare repository with one commit on `main` plus an unreachable blob, and `extra` appended
/// to its config.
fn bare_repo(dir: &Path, extra: &str) -> (Repository, ObjectId, ObjectId) {
    Repository::init(
        dir,
        &InitOptions::new()
            .with_bare(true)
            .with_initial_branch("main")
            .with_ref_storage(RefStorageFormat::Files),
    )
    .unwrap();
    let mut config = std::fs::read_to_string(dir.join("config")).unwrap();
    config.push_str(extra);
    std::fs::write(dir.join("config"), config).unwrap();
    let repo = Repository::open(dir, &RepositoryOptions::empty()).unwrap();
    let odb = repo.odb();
    let blob = odb
        .write_loose_materialize(ObjectKind::Blob, b"tracked\n")
        .unwrap();
    let mut tree = b"100644 file.txt\0".to_vec();
    tree.extend_from_slice(blob.as_bytes());
    let tree = odb
        .write_loose_materialize(ObjectKind::Tree, &tree)
        .unwrap();
    let commit = format!(
        "tree {tree}\nauthor T <t@e.com> 100 +0000\ncommitter T <t@e.com> 100 +0000\n\nseed\n"
    );
    let tip = odb
        .write_loose_materialize(ObjectKind::Commit, commit.as_bytes())
        .unwrap();
    std::fs::write(dir.join("refs/heads/main"), format!("{tip}\n")).unwrap();
    let loose = odb
        .write_loose_materialize(ObjectKind::Blob, b"nobody points at me\n")
        .unwrap();
    (repo, tip, loose)
}

const ADVERTISE_LOP: &str = "[promisor]\n\tadvertise = true\n\
    [remote \"lop\"]\n\turl = file:///srv/lop\n\tpromisor = true\n";

fn v2_opts(stateless_rpc: bool, advertise_refs: bool) -> ServeOptions {
    ServeOptions {
        protocol: ProtocolVersion::V2,
        stateless_rpc,
        advertise_refs,
        agent: "grit-test".to_owned(),
        ..ServeOptions::default()
    }
}

/// The capability lines of a v2 advertisement.
fn v2_capabilities(repo: &Repository) -> Vec<String> {
    let mut out = Vec::new();
    upload_pack(
        repo,
        &mut Cursor::new(Vec::new()),
        &mut out,
        &mut v2_opts(false, true),
    )
    .unwrap();
    let mut caps = Vec::new();
    let mut r = Cursor::new(out);
    while let Ok(Some(grit_lib::remote::pkt_line::Packet::Data(line))) =
        grit_lib::remote::pkt_line::read_packet(&mut r)
    {
        caps.push(line.trim_end().to_owned());
    }
    caps
}

fn ls_refs_request(extra_caps: &[&str]) -> Vec<u8> {
    let mut body = pkt("command=ls-refs\n");
    for c in extra_caps {
        body.extend(pkt(&format!("{c}\n")));
    }
    body.extend(b"0001");
    body.extend(pkt("ref-prefix refs/heads/\n"));
    body.extend(b"0000");
    body
}

fn fetch_request(extra_caps: &[&str], want: ObjectId) -> Vec<u8> {
    let mut body = pkt("command=fetch\n");
    for c in extra_caps {
        body.extend(pkt(&format!("{c}\n")));
    }
    body.extend(b"0001");
    body.extend(pkt(&format!("want {want}\n")));
    body.extend(pkt("done\n"));
    body.extend(b"0000");
    body
}

#[test]
fn v2_advertisement_ends_with_promisor_remote() {
    let tmp = tempfile::tempdir().unwrap();
    let (repo, _, _) = bare_repo(tmp.path(), ADVERTISE_LOP);
    let caps = v2_capabilities(&repo);
    assert_eq!(
        caps.last().map(String::as_str),
        Some("promisor-remote=name=lop,url=file:///srv/lop"),
        "{caps:?}"
    );
}

#[test]
fn no_promisor_remote_capability_unless_configured() {
    for extra in [
        "",
        "[promisor]\n\tadvertise = false\n[remote \"lop\"]\n\turl = file:///l\n\tpromisor = true\n",
        "[promisor]\n\tadvertise = true\n[remote \"origin\"]\n\turl = file:///o\n",
    ] {
        let tmp = tempfile::tempdir().unwrap();
        let (repo, _, _) = bare_repo(tmp.path(), extra);
        let caps = v2_capabilities(&repo);
        assert!(
            !caps.iter().any(|c| c.starts_with("promisor-remote")),
            "{extra:?}: {caps:?}"
        );
    }
}

#[test]
fn v0_advertisement_never_carries_promisor_remote() {
    let tmp = tempfile::tempdir().unwrap();
    let (repo, _, _) = bare_repo(tmp.path(), ADVERTISE_LOP);
    let mut out = Vec::new();
    let mut opts = ServeOptions {
        advertise_refs: true,
        ..ServeOptions::default()
    };
    upload_pack(&repo, &mut Cursor::new(Vec::new()), &mut out, &mut opts).unwrap();
    assert!(!String::from_utf8_lossy(&out).contains("promisor-remote"));
}

#[test]
fn stateful_session_keeps_acceptance_from_ls_refs() {
    let tmp = tempfile::tempdir().unwrap();
    let (repo, tip, _) = bare_repo(tmp.path(), ADVERTISE_LOP);
    let mut input = ls_refs_request(&["agent=x", "promisor-remote=lop;unknown"]);
    input.extend(fetch_request(&["agent=x"], tip));
    let mut out = Vec::new();
    let mut opts = v2_opts(false, false);
    upload_pack(&repo, &mut Cursor::new(input), &mut out, &mut opts).unwrap();
    assert_eq!(opts.accepted_promisor_remotes, ["lop"]);
    assert!(String::from_utf8_lossy(&out).contains("packfile"));
}

#[test]
fn stateless_request_records_acceptance_from_fetch() {
    let tmp = tempfile::tempdir().unwrap();
    let (repo, tip, _) = bare_repo(tmp.path(), ADVERTISE_LOP);
    let mut out = Vec::new();
    let mut opts = v2_opts(true, false);
    upload_pack(
        &repo,
        &mut Cursor::new(fetch_request(&["promisor-remote=lop"], tip)),
        &mut out,
        &mut opts,
    )
    .unwrap();
    assert_eq!(opts.accepted_promisor_remotes, ["lop"]);
}

#[test]
fn acceptance_is_ignored_when_nothing_was_advertised() {
    let tmp = tempfile::tempdir().unwrap();
    let (repo, _, _) = bare_repo(tmp.path(), "");
    let mut out = Vec::new();
    let mut opts = v2_opts(true, false);
    upload_pack(
        &repo,
        &mut Cursor::new(ls_refs_request(&["promisor-remote=lop"])),
        &mut out,
        &mut opts,
    )
    .unwrap();
    assert!(opts.accepted_promisor_remotes.is_empty());
}

#[test]
fn promisor_remote_without_names_is_refused() {
    for bad in ["promisor-remote", "promisor-remote="] {
        let tmp = tempfile::tempdir().unwrap();
        let (repo, _, _) = bare_repo(tmp.path(), ADVERTISE_LOP);
        let mut out = Vec::new();
        let err = upload_pack(
            &repo,
            &mut Cursor::new(ls_refs_request(&[bad])),
            &mut out,
            &mut v2_opts(false, false),
        )
        .expect_err(bad);
        assert!(matches!(err, ServeError::Protocol(_)), "{bad}: {err:?}");

        // Smart HTTP gets the error as an ERR packet.
        let mut out = Vec::new();
        upload_pack(
            &repo,
            &mut Cursor::new(ls_refs_request(&[bad])),
            &mut out,
            &mut v2_opts(true, false),
        )
        .unwrap();
        assert!(String::from_utf8_lossy(&out).contains("ERR "), "{bad}");
    }
}

#[test]
fn allow_any_sha1_in_want_serves_unreachable_objects() {
    let tmp = tempfile::tempdir().unwrap();
    let (repo, _, loose) = bare_repo(tmp.path(), "");
    let mut out = Vec::new();
    let err = upload_pack(
        &repo,
        &mut Cursor::new(fetch_request(&[], loose)),
        &mut out,
        &mut v2_opts(false, false),
    )
    .expect_err("unreachable want refused by default");
    assert!(matches!(err, ServeError::NotOurRef(oid) if oid == loose));

    let tmp = tempfile::tempdir().unwrap();
    let (repo, _, loose) = bare_repo(tmp.path(), "[uploadpack]\n\tallowAnySHA1InWant = true\n");
    let mut out = Vec::new();
    upload_pack(
        &repo,
        &mut Cursor::new(fetch_request(&[], loose)),
        &mut out,
        &mut v2_opts(false, false),
    )
    .unwrap();
    assert!(String::from_utf8_lossy(&out).contains("packfile"));

    // A want for an object the repository does not have is still refused.
    let missing = ObjectId::from_hex("1234567890123456789012345678901234567890").unwrap();
    let err = upload_pack(
        &repo,
        &mut Cursor::new(fetch_request(&[], missing)),
        &mut Vec::new(),
        &mut v2_opts(false, false),
    )
    .expect_err("missing object");
    assert!(matches!(err, ServeError::NotOurRef(_)));
}

#[test]
fn allow_any_sha1_in_want_advertises_v0_capabilities() {
    let tmp = tempfile::tempdir().unwrap();
    let (repo, _, _) = bare_repo(tmp.path(), "[uploadpack]\n\tallowAnySHA1InWant = true\n");
    let mut out = Vec::new();
    let mut opts = ServeOptions {
        advertise_refs: true,
        ..ServeOptions::default()
    };
    upload_pack(&repo, &mut Cursor::new(Vec::new()), &mut out, &mut opts).unwrap();
    let text = String::from_utf8_lossy(&out);
    assert!(text.contains("allow-tip-sha1-in-want"), "{text}");
    assert!(text.contains("allow-reachable-sha1-in-want"), "{text}");
}

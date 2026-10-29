//! Protocol v2 `fetch` served in process: `deepen-not` ref resolution, the
//! `wanted-refs` section for `want-ref`, and the `fetch=` capability line,
//! each checked against the system `git upload-pack` answering the same request.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::io::{Cursor, Write};
use std::path::Path;
use std::process::{Command, Stdio};

use grit_lib::config::environment::RepositoryOptions;
use grit_lib::objects::ObjectId;
use grit_lib::remote::serve::{
    upload_pack, DeepenNotError, ProtocolVersion, ServeError, ServeOptions,
};
use grit_lib::repo::Repository;

/// System `git` output with surrounding whitespace trimmed, ready to compare
/// with pkt-line payloads.
fn git(dir: &Path, args: &[&str]) -> String {
    grit_test_support::git(dir, args).trim().to_owned()
}

/// A bare repository with six linear commits on `main`, tags `v3` (lightweight)
/// and `av3` (annotated) on the third, and a branch and tag both named `dup`.
fn fixture(tmp: &Path) -> std::path::PathBuf {
    let work = tmp.join("work");
    std::fs::create_dir_all(&work).unwrap();
    git(&work, &["init", "-q", "-b", "main", "."]);
    for i in 1..=6 {
        std::fs::write(work.join(format!("f{i}")), format!("{i}\n")).unwrap();
        git(&work, &["add", "."]);
        git(&work, &["commit", "-q", "-m", &format!("c{i}")]);
        if i == 3 {
            git(&work, &["tag", "v3"]);
            git(&work, &["tag", "-a", "-m", "annotated", "av3"]);
        }
    }
    git(&work, &["branch", "dup", "HEAD~1"]);
    git(&work, &["tag", "dup", "HEAD~2"]);
    git(&work, &["tag", "treetag", "HEAD^{tree}"]);
    let bare = tmp.join("srv.git");
    git(
        tmp,
        &[
            "clone",
            "-q",
            "--bare",
            work.to_str().unwrap(),
            bare.to_str().unwrap(),
        ],
    );
    // Git advertises and serves `object-info` only with this set.
    git(&bare, &["config", "transfer.advertiseObjectInfo", "true"]);
    bare
}

fn pkt(buf: &mut Vec<u8>, line: &str) {
    let payload = format!("{line}\n");
    buf.extend(format!("{:04x}", payload.len() + 4).as_bytes());
    buf.extend(payload.as_bytes());
}

/// A stateless v2 `command=fetch` request carrying `args` and `done`.
fn fetch_request(args: &[&str]) -> Vec<u8> {
    let mut body = Vec::new();
    pkt(&mut body, "command=fetch");
    pkt(&mut body, "object-format=sha1");
    body.extend(b"0001");
    for arg in args {
        pkt(&mut body, arg);
    }
    pkt(&mut body, "done");
    body.extend(b"0000");
    body
}

/// A stateless v2 `command=fetch` negotiation round: `args` without `done`.
fn negotiation_request(args: &[&str]) -> Vec<u8> {
    let mut body = Vec::new();
    pkt(&mut body, "command=fetch");
    pkt(&mut body, "object-format=sha1");
    body.extend(b"0001");
    for arg in args {
        pkt(&mut body, arg);
    }
    body.extend(b"0000");
    body
}

/// Decode a v2 response into readable tokens up to and including `packfile`.
///
/// Side-band channel 1 framing (from `sideband-all`) is stripped, so the same
/// token list comes out with and without it.
fn response_sections(resp: &[u8]) -> Vec<String> {
    let mut out = Vec::new();
    let mut i = 0;
    while i + 4 <= resp.len() {
        let len = usize::from_str_radix(std::str::from_utf8(&resp[i..i + 4]).unwrap(), 16)
            .expect("pkt-line length");
        if len < 4 {
            out.push(if len == 1 { "<delim>" } else { "<flush>" }.to_owned());
            i += 4;
            continue;
        }
        let mut payload = &resp[i + 4..i + len];
        i += len;
        if payload.first() == Some(&1) {
            payload = &payload[1..];
        }
        let line = String::from_utf8_lossy(payload).trim_end().to_owned();
        let done = line == "packfile";
        out.push(line);
        if done {
            break;
        }
    }
    out
}

fn serve_grit(bare: &Path, body: &[u8]) -> Vec<u8> {
    let repo = Repository::open(bare, &RepositoryOptions::empty()).expect("open");
    let mut out = Vec::new();
    let mut opts = ServeOptions {
        protocol: ProtocolVersion::V2,
        stateless_rpc: true,
        ..ServeOptions::default()
    };
    upload_pack(&repo, &mut Cursor::new(body.to_vec()), &mut out, &mut opts).expect("upload_pack");
    out
}

fn serve_git(bare: &Path, body: &[u8]) -> Vec<u8> {
    let mut child = Command::new("git")
        .args(["upload-pack", "--stateless-rpc"])
        .arg(bare)
        .env("GIT_PROTOCOL", "version=2")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn git upload-pack");
    child.stdin.take().unwrap().write_all(body).unwrap();
    child.wait_with_output().expect("git upload-pack").stdout
}

/// The `fetch=` line of the v2 capability advertisement.
fn advertised_fetch_features(bare: &Path) -> String {
    let repo = Repository::open(bare, &RepositoryOptions::empty()).expect("open");
    let mut out = Vec::new();
    let mut opts = ServeOptions {
        protocol: ProtocolVersion::V2,
        advertise_refs: true,
        ..ServeOptions::default()
    };
    upload_pack(&repo, &mut Cursor::new(Vec::new()), &mut out, &mut opts).expect("advertise");
    response_sections(&out)
        .into_iter()
        .find_map(|l| l.strip_prefix("fetch=").map(str::to_owned))
        .expect("fetch= capability")
}

/// Run a non-stateless v2 session so the typed error comes back to the caller.
fn serve_grit_err(bare: &Path, body: &[u8]) -> ServeError {
    let repo = Repository::open(bare, &RepositoryOptions::empty()).expect("open");
    let mut out = Vec::new();
    let mut opts = ServeOptions {
        protocol: ProtocolVersion::V2,
        ..ServeOptions::default()
    };
    upload_pack(&repo, &mut Cursor::new(body.to_vec()), &mut out, &mut opts)
        .expect_err("request should be refused")
}

fn shallow_lines(sections: &[String]) -> Vec<&str> {
    sections
        .iter()
        .filter_map(|l| l.strip_prefix("shallow "))
        .collect()
}

#[test]
fn deepen_not_resolves_ref_names_like_git() {
    let tmp = tempfile::tempdir().unwrap();
    let bare = fixture(tmp.path());
    let tip = git(&bare, &["rev-parse", "main"]);
    let boundary = git(&bare, &["rev-parse", "main~2"]);
    let tagged = git(&bare, &["rev-parse", "v3"]);

    for spec in ["v3", "refs/tags/v3", "tags/v3", "av3", "refs/tags/av3"] {
        let body = fetch_request(&[&format!("want {tip}"), &format!("deepen-not {spec}")]);
        let grit = response_sections(&serve_grit(&bare, &body));
        let git = response_sections(&serve_git(&bare, &body));
        assert_eq!(grit, git, "deepen-not {spec}: response sections differ");
        assert_eq!(
            shallow_lines(&grit),
            [boundary.as_str()],
            "deepen-not {spec}"
        );
    }

    let body = fetch_request(&[&format!("want {tip}"), &format!("deepen-not {tagged}")]);
    let grit = response_sections(&serve_grit(&bare, &body));
    assert_eq!(
        shallow_lines(&grit),
        [boundary.as_str()],
        "deepen-not <oid>"
    );
}

#[test]
fn deepen_not_with_sideband_all_matches_git() {
    let tmp = tempfile::tempdir().unwrap();
    let bare = fixture(tmp.path());
    git(&bare, &["config", "uploadpack.allowSidebandAll", "true"]);
    let tip = git(&bare, &["rev-parse", "main"]);
    let body = fetch_request(&["sideband-all", &format!("want {tip}"), "deepen-not v3"]);
    let grit = response_sections(&serve_grit(&bare, &body));
    let git = response_sections(&serve_git(&bare, &body));
    assert_eq!(grit, git);
}

#[test]
fn deepen_not_rejects_unknown_ambiguous_and_non_commit_names() {
    let tmp = tempfile::tempdir().unwrap();
    let bare = fixture(tmp.path());
    let tip = git(&bare, &["rev-parse", "main"]);
    let missing = "0123456789012345678901234567890123456789";
    for (spec, expected) in [
        ("nope", DeepenNotError::UnknownRef),
        (missing, DeepenNotError::UnknownRef),
        ("dup", DeepenNotError::AmbiguousRef),
        ("treetag", DeepenNotError::NotACommit),
    ] {
        let body = fetch_request(&[&format!("want {tip}"), &format!("deepen-not {spec}")]);
        match serve_grit_err(&bare, &body) {
            ServeError::InvalidDeepenNot { spec: got, reason } => {
                assert_eq!(got, spec);
                assert_eq!(reason, expected, "deepen-not {spec}");
            }
            other => panic!("deepen-not {spec}: unexpected error {other:?}"),
        }
        let stateless = String::from_utf8_lossy(&serve_grit(&bare, &body)).into_owned();
        assert!(
            stateless.contains("ERR deepen-not"),
            "stateless response for {spec}: {stateless}"
        );
    }
}

#[test]
fn want_ref_response_lists_wanted_refs_before_packfile() {
    let tmp = tempfile::tempdir().unwrap();
    let bare = fixture(tmp.path());
    git(&bare, &["config", "uploadpack.allowRefInWant", "true"]);
    git(&bare, &["config", "uploadpack.allowSidebandAll", "true"]);
    let tip = git(&bare, &["rev-parse", "main"]);
    let dup_branch = git(&bare, &["rev-parse", "refs/heads/dup"]);

    for sideband_all in [false, true] {
        let mut args = vec!["no-progress", "want-ref refs/heads/main"];
        if sideband_all {
            args.push("sideband-all");
        }
        let body = fetch_request(&args);
        let grit = response_sections(&serve_grit(&bare, &body));
        assert_eq!(
            grit,
            [
                "wanted-refs".to_owned(),
                format!("{tip} refs/heads/main"),
                "<delim>".to_owned(),
                "packfile".to_owned(),
            ],
            "sideband-all={sideband_all}"
        );
        assert_eq!(grit, response_sections(&serve_git(&bare, &body)));
    }

    // gitprotocol-v2 orders shallow-info before wanted-refs, which is what Git's
    // fetch client reads. Some older `git upload-pack` releases send them the
    // other way round, so this case is checked against the spec, not the binary.
    let body = fetch_request(&["want-ref refs/heads/dup", "deepen 1"]);
    let grit = response_sections(&serve_grit(&bare, &body));
    assert_eq!(
        grit,
        [
            "shallow-info".to_owned(),
            format!("shallow {dup_branch}"),
            "<delim>".to_owned(),
            "wanted-refs".to_owned(),
            format!("{dup_branch} refs/heads/dup"),
            "<delim>".to_owned(),
            "packfile".to_owned(),
        ]
    );
}

#[test]
fn want_ref_is_refused_unless_allowed() {
    let tmp = tempfile::tempdir().unwrap();
    let bare = fixture(tmp.path());
    let body = fetch_request(&["want-ref refs/heads/main"]);
    assert!(matches!(
        serve_grit_err(&bare, &body),
        ServeError::Protocol(_)
    ));
    let resp = String::from_utf8_lossy(&serve_grit(&bare, &body)).into_owned();
    assert!(resp.contains("ERR "), "expected ERR, got {resp}");
    assert!(!resp.contains("packfile"), "no pack without allowRefInWant");
}

#[test]
fn duplicate_want_ref_is_a_protocol_error() {
    let tmp = tempfile::tempdir().unwrap();
    let bare = fixture(tmp.path());
    git(&bare, &["config", "uploadpack.allowRefInWant", "true"]);
    let body = fetch_request(&["want-ref refs/heads/main", "want-ref refs/heads/main"]);
    match serve_grit_err(&bare, &body) {
        ServeError::Protocol(msg) => assert!(msg.contains("duplicate want-ref"), "{msg}"),
        other => panic!("unexpected error {other:?}"),
    }
    let resp = String::from_utf8_lossy(&serve_grit(&bare, &body)).into_owned();
    assert!(resp.contains("ERR duplicate want-ref"), "got {resp}");
    assert!(
        !resp.contains("packfile"),
        "no pack for a duplicate want-ref"
    );
}

#[test]
fn sideband_all_is_refused_unless_allowed() {
    let tmp = tempfile::tempdir().unwrap();
    let bare = fixture(tmp.path());
    let tip = git(&bare, &["rev-parse", "main"]);
    let body = fetch_request(&["sideband-all", &format!("want {tip}")]);
    assert!(matches!(
        serve_grit_err(&bare, &body),
        ServeError::Protocol(_)
    ));
    let resp = String::from_utf8_lossy(&serve_grit(&bare, &body)).into_owned();
    assert!(resp.contains("ERR "), "expected ERR, got {resp}");
    assert!(
        !resp.contains("packfile"),
        "no pack without allowSidebandAll"
    );
    assert!(
        serve_git(&bare, &body).is_empty(),
        "system git upload-pack also refuses sideband-all when it is not allowed"
    );
}

#[test]
fn v2_fetch_capability_lists_only_v2_features() {
    let tmp = tempfile::tempdir().unwrap();
    let bare = fixture(tmp.path());
    assert_eq!(advertised_fetch_features(&bare), "shallow wait-for-done");

    git(&bare, &["config", "uploadpack.allowFilter", "true"]);
    git(&bare, &["config", "uploadpack.allowRefInWant", "true"]);
    git(&bare, &["config", "uploadpack.allowSidebandAll", "true"]);
    let features = advertised_fetch_features(&bare);
    let features: Vec<&str> = features.split(' ').collect();
    assert_eq!(
        features,
        [
            "shallow",
            "wait-for-done",
            "filter",
            "ref-in-want",
            "sideband-all"
        ]
    );
}

fn v2_capability_lines(bare: &Path) -> Vec<String> {
    let repo = Repository::open(bare, &RepositoryOptions::empty()).expect("open");
    let mut out = Vec::new();
    let mut opts = ServeOptions {
        protocol: ProtocolVersion::V2,
        advertise_refs: true,
        ..ServeOptions::default()
    };
    upload_pack(&repo, &mut Cursor::new(Vec::new()), &mut out, &mut opts).expect("advertise");
    response_sections(&out)
        .into_iter()
        .filter(|l| !matches!(l.as_str(), "<flush>" | "<delim>"))
        .collect()
}

#[test]
fn v2_advertisement_includes_server_option_object_info_and_wait_for_done() {
    let tmp = tempfile::tempdir().unwrap();
    let bare = fixture(tmp.path());
    let grit_caps = v2_capability_lines(&bare);
    let git_out = Command::new("git")
        .args(["upload-pack", "--advertise-refs", "--stateless-rpc"])
        .arg(&bare)
        .env("GIT_PROTOCOL", "version=2")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .output()
        .expect("git advertise");
    assert!(git_out.status.success());
    let git_caps: Vec<String> = response_sections(&git_out.stdout)
        .into_iter()
        .filter(|l| !matches!(l.as_str(), "<flush>" | "<delim>" | "version 2"))
        .collect();
    for required in ["server-option", "fetch=shallow wait-for-done"] {
        assert!(
            grit_caps.iter().any(|l| l == required),
            "grit missing {required:?}; caps={grit_caps:?}"
        );
        assert!(
            git_caps.iter().any(|l| l == required),
            "git missing {required:?}; caps={git_caps:?}"
        );
    }
    assert!(
        grit_caps.iter().any(|l| l == "object-info=size type"),
        "grit must advertise the object-info attributes it answers; caps={grit_caps:?}"
    );
    // Git before 2.56 advertises a bare `object-info`; newer Git lists the
    // attributes it answers. Every attribute Git lists must be one grit lists.
    let git_object_info = git_caps
        .iter()
        .find_map(|l| object_info_attributes(l))
        .unwrap_or_else(|| panic!("git missing object-info; caps={git_caps:?}"));
    for attr in git_object_info {
        assert!(
            ["size", "type"].contains(&attr),
            "git object-info attribute {attr:?} not advertised by grit; caps={git_caps:?}"
        );
    }
}

/// Attributes of an `object-info` capability line (empty for a bare
/// `object-info`), or `None` when `line` is another capability.
fn object_info_attributes(line: &str) -> Option<Vec<&str>> {
    match line.strip_prefix("object-info")? {
        "" => Some(Vec::new()),
        rest => rest.strip_prefix('=').map(|v| v.split(' ').collect()),
    }
}

#[test]
fn object_info_is_not_advertised_without_transfer_advertise_object_info() {
    let tmp = tempfile::tempdir().unwrap();
    let bare = fixture(tmp.path());
    git(
        &bare,
        &["config", "--unset", "transfer.advertiseObjectInfo"],
    );
    let git_out = Command::new("git")
        .args(["upload-pack", "--advertise-refs", "--stateless-rpc"])
        .arg(&bare)
        .env("GIT_PROTOCOL", "version=2")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .output()
        .expect("git advertise");
    assert!(git_out.status.success());
    let git_caps = response_sections(&git_out.stdout);
    let grit_caps = v2_capability_lines(&bare);
    assert!(
        !git_caps.iter().any(|l| object_info_attributes(l).is_some()),
        "{git_caps:?}"
    );
    assert!(
        !grit_caps
            .iter()
            .any(|l| object_info_attributes(l).is_some()),
        "{grit_caps:?}"
    );

    let mut body = Vec::new();
    pkt(&mut body, "command=object-info");
    pkt(&mut body, "object-format=sha1");
    body.extend(b"0001");
    pkt(&mut body, "size");
    body.extend(b"0000");
    let repo = Repository::open(&bare, &RepositoryOptions::empty()).expect("open");
    let mut out = Vec::new();
    let mut opts = ServeOptions {
        protocol: ProtocolVersion::V2,
        stateless_rpc: true,
        ..ServeOptions::default()
    };
    upload_pack(&repo, &mut Cursor::new(body), &mut out, &mut opts).expect("stateless v2");
    let sections = response_sections(&out);
    assert!(
        sections.first().is_some_and(|l| l.starts_with("ERR ")),
        "object-info must be refused unless advertised: {sections:?}"
    );
}

#[test]
fn v2_fetch_records_server_option_from_client() {
    let tmp = tempfile::tempdir().unwrap();
    let bare = fixture(tmp.path());
    let tip = git(&bare, &["rev-parse", "main"]);
    let mut body = Vec::new();
    pkt(&mut body, "command=fetch");
    pkt(&mut body, "server-option=trace-id=42");
    pkt(&mut body, "object-format=sha1");
    body.extend(b"0001");
    pkt(&mut body, &format!("want {tip}"));
    pkt(&mut body, "done");
    body.extend(b"0000");
    let repo = Repository::open(&bare, &RepositoryOptions::empty()).expect("open");
    let mut out = Vec::new();
    let mut opts = ServeOptions {
        protocol: ProtocolVersion::V2,
        stateless_rpc: true,
        ..ServeOptions::default()
    };
    upload_pack(&repo, &mut Cursor::new(body), &mut out, &mut opts)
        .expect("fetch with server-option");
    assert_eq!(opts.server_options, ["trace-id=42"]);
}

#[test]
fn object_info_missing_oid_is_not_an_error() {
    let tmp = tempfile::tempdir().unwrap();
    let bare = fixture(tmp.path());
    let missing = "1111111111111111111111111111111111111111";
    let mut body = Vec::new();
    pkt(&mut body, "command=object-info");
    pkt(&mut body, "object-format=sha1");
    body.extend(b"0001");
    pkt(&mut body, "size");
    pkt(&mut body, &format!("oid {missing}"));
    body.extend(b"0000");
    let grit_sections = response_sections(&serve_grit(&bare, &body));
    let git_sections = response_sections(&serve_git(&bare, &body));
    assert_eq!(
        grit_sections, git_sections,
        "missing oid should not fail the command"
    );
}

#[test]
fn object_info_matches_git_for_tip_commit() {
    let tmp = tempfile::tempdir().unwrap();
    let bare = fixture(tmp.path());
    let tip = git(&bare, &["rev-parse", "main"]);
    let mut body = Vec::new();
    pkt(&mut body, "command=object-info");
    pkt(&mut body, "object-format=sha1");
    body.extend(b"0001");
    pkt(&mut body, "size");
    pkt(&mut body, &format!("oid {tip}"));
    body.extend(b"0000");
    let grit_resp = serve_grit(&bare, &body);
    let git_resp = serve_git(&bare, &body);
    assert_eq!(
        response_sections(&grit_resp),
        response_sections(&git_resp),
        "object-info response should match git"
    );
}

fn grit_bin_for_subprocess() -> Option<std::path::PathBuf> {
    if let Ok(p) = std::env::var("CARGO_BIN_EXE_grit") {
        return Some(p.into());
    }
    let candidate =
        std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../target/debug/grit");
    candidate.is_file().then_some(candidate)
}

/// Add `refs/heads/side`, a commit forked from `main~3`, to the fixture.
fn add_side_branch(bare: &Path) -> String {
    let base = git(bare, &["rev-parse", "main~3"]);
    let tree = git(bare, &["rev-parse", "main~3^{tree}"]);
    let side = git(bare, &["commit-tree", &tree, "-p", &base, "-m", "side"]);
    git(bare, &["update-ref", "refs/heads/side", &side]);
    side
}

/// Both servers answer the same negotiation round with the same sections.
fn assert_round_matches_git(bare: &Path, args: &[&str]) -> Vec<String> {
    let body = negotiation_request(args);
    let grit = response_sections(&serve_grit(bare, &body));
    let git = response_sections(&serve_git(bare, &body));
    assert_eq!(grit, git, "{args:?}: response sections differ from git");
    grit
}

fn acks(oids: &[&str]) -> Vec<String> {
    std::iter::once("acknowledgments".to_owned())
        .chain(oids.iter().map(|oid| format!("ACK {oid}")))
        .collect()
}

#[test]
fn v2_round_is_ready_when_haves_cover_the_want() {
    let tmp = tempfile::tempdir().unwrap();
    let bare = fixture(tmp.path());
    let tip = git(&bare, &["rev-parse", "main"]);
    let have = git(&bare, &["rev-parse", "main~2"]);
    let sections = assert_round_matches_git(
        &bare,
        &[
            "no-progress",
            &format!("want {tip}"),
            &format!("have {have}"),
        ],
    );
    let mut expected = acks(&[&have]);
    expected.extend(["ready", "<delim>", "packfile"].map(str::to_owned));
    assert_eq!(sections, expected);
}

#[test]
fn v2_round_is_ready_for_tag_and_tree_wants() {
    let tmp = tempfile::tempdir().unwrap();
    let bare = fixture(tmp.path());
    let tagged = git(&bare, &["rev-parse", "main~3"]);
    let annotated = git(&bare, &["rev-parse", "refs/tags/av3"]);
    let tree = git(&bare, &["rev-parse", "refs/tags/treetag"]);
    for want in [&annotated, &tree] {
        let sections = assert_round_matches_git(
            &bare,
            &[
                "no-progress",
                &format!("want {want}"),
                &format!("have {tagged}"),
            ],
        );
        assert!(sections.iter().any(|l| l == "ready"), "want {want}");
    }
}

#[test]
fn v2_round_is_not_ready_when_a_want_is_not_covered() {
    let tmp = tempfile::tempdir().unwrap();
    let bare = fixture(tmp.path());
    let side = add_side_branch(&bare);
    let tip = git(&bare, &["rev-parse", "main"]);
    let older = git(&bare, &["rev-parse", "main~3"]);
    let near_tip = git(&bare, &["rev-parse", "main~1"]);
    let missing = "1111111111111111111111111111111111111111";
    let mut not_ready = acks(&[&tip]);
    not_ready.push("<flush>".to_owned());

    // The have is a descendant of the want, not an ancestor.
    let sections =
        assert_round_matches_git(&bare, &[&format!("want {older}"), &format!("have {tip}")]);
    assert_eq!(sections, not_ready);

    // The have is on a different line of history.
    let sections =
        assert_round_matches_git(&bare, &[&format!("want {side}"), &format!("have {tip}")]);
    assert_eq!(sections, not_ready);

    // Two tips, and the have covers only `main`.
    let sections = assert_round_matches_git(
        &bare,
        &[
            &format!("want {tip}"),
            &format!("want {side}"),
            &format!("have {near_tip}"),
        ],
    );
    let mut expected = acks(&[&near_tip]);
    expected.push("<flush>".to_owned());
    assert_eq!(sections, expected);

    // Nothing in common yet.
    let sections =
        assert_round_matches_git(&bare, &[&format!("want {tip}"), &format!("have {missing}")]);
    assert_eq!(sections, ["acknowledgments", "NAK", "<flush>"]);
}

#[test]
fn v2_round_with_wait_for_done_is_never_ready() {
    let tmp = tempfile::tempdir().unwrap();
    let bare = fixture(tmp.path());
    let tip = git(&bare, &["rev-parse", "main"]);
    let have = git(&bare, &["rev-parse", "main~2"]);
    let sections = assert_round_matches_git(
        &bare,
        &[
            "wait-for-done",
            &format!("want {tip}"),
            &format!("have {have}"),
        ],
    );
    let mut expected = acks(&[&have]);
    expected.push("<flush>".to_owned());
    assert_eq!(sections, expected);
}

#[test]
fn v2_ready_with_sideband_all_is_followed_by_a_raw_delimiter() {
    let tmp = tempfile::tempdir().unwrap();
    let bare = fixture(tmp.path());
    git(&bare, &["config", "uploadpack.allowSidebandAll", "true"]);
    let tip = git(&bare, &["rev-parse", "main"]);
    let have = git(&bare, &["rev-parse", "main~2"]);
    let args = [
        "sideband-all",
        "no-progress",
        &format!("want {tip}"),
        &format!("have {have}"),
    ];
    let sections = assert_round_matches_git(&bare, &args);
    assert!(sections.ends_with(&["ready", "<delim>", "packfile"].map(str::to_owned)));
    let raw = serve_grit(&bare, &negotiation_request(&args));
    let ready = b"000b\x01ready\n0001";
    assert!(
        raw.windows(ready.len()).any(|w| w == ready),
        "ready on band 1, then a bare delimiter"
    );
}

#[test]
fn git_fetch_from_grit_upload_pack_stops_after_one_round_when_ready() {
    let Some(grit_bin) = grit_bin_for_subprocess() else {
        eprintln!("skip git_fetch_from_grit_upload_pack_stops_after_one_round_when_ready");
        return;
    };
    let tmp = tempfile::tempdir().unwrap();
    let bare = fixture(tmp.path());
    let behind = git(&bare, &["rev-parse", "main~2"]);
    let client = tmp.path().join("client.git");
    git(
        tmp.path(),
        &["init", "-q", "--bare", client.to_str().unwrap()],
    );
    git(
        &client,
        &[
            "fetch",
            "-q",
            "--no-tags",
            bare.to_str().unwrap(),
            &format!("{behind}:refs/heads/main"),
        ],
    );
    let trace = tmp.path().join("trace");
    let out = Command::new("git")
        .current_dir(&client)
        .args([
            "fetch",
            "--no-tags",
            &format!("--upload-pack={} upload-pack", grit_bin.display()),
            bare.to_str().unwrap(),
            "main:refs/remotes/origin/main",
        ])
        .env("GIT_TRACE_PACKET", &trace)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .output()
        .expect("git fetch");
    assert!(
        out.status.success(),
        "git fetch failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let trace = std::fs::read_to_string(&trace).unwrap();
    let rounds = trace.matches("fetch> command=fetch").count();
    assert_eq!(rounds, 1, "one negotiation round, trace:\n{trace}");
    assert!(trace.contains("fetch< ready"), "trace:\n{trace}");
    assert!(!trace.contains("fetch> done"), "trace:\n{trace}");
    assert_eq!(
        git(&client, &["rev-parse", "refs/remotes/origin/main"]),
        git(&bare, &["rev-parse", "main"])
    );
    git(&client, &["fsck", "--no-progress"]);
}

#[test]
fn git_fetch_accepts_grit_upload_pack_with_server_option() {
    let Some(grit_bin) = grit_bin_for_subprocess() else {
        eprintln!(
            "skip git_fetch_accepts_grit_upload_pack_with_server_option: grit binary not built"
        );
        return;
    };
    let tmp = tempfile::tempdir().unwrap();
    let bare = fixture(tmp.path());
    let client = tmp.path().join("client");
    git(tmp.path(), &["init", "-q", client.to_str().unwrap()]);
    let out = Command::new("git")
        .current_dir(&client)
        .args([
            "fetch",
            "--server-option=trace-id=42",
            &format!("--upload-pack={} upload-pack", grit_bin.display()),
            bare.to_str().unwrap(),
            "main:fetched-main",
        ])
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .output()
        .expect("git fetch");
    assert!(
        out.status.success(),
        "git fetch failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(
        git(&client, &["rev-parse", "fetched-main"]),
        git(&bare, &["rev-parse", "main"])
    );
}

#[test]
fn stateful_v2_session_returns_upload_outcome_for_fetch() {
    let tmp = tempfile::tempdir().unwrap();
    let bare = fixture(tmp.path());
    let tip =
        ObjectId::from_hex(git(&bare, &["rev-parse", "refs/heads/main"]).trim()).expect("main tip");
    let body = fetch_request(&[&format!("want {}", tip.to_hex())]);
    let repo = Repository::open(&bare, &RepositoryOptions::empty()).expect("open");
    let mut out = Vec::new();
    let mut opts = ServeOptions {
        protocol: ProtocolVersion::V2,
        stateless_rpc: false,
        agent: "grit-test".to_owned(),
        ..ServeOptions::default()
    };
    let summary =
        upload_pack(&repo, &mut Cursor::new(body), &mut out, &mut opts).expect("stateful v2");
    assert!(
        summary.objects_sent > 0,
        "expected pack objects, got {summary:?}"
    );
    assert!(summary.bytes_written > 100);
    assert!(summary.pack.is_some());

    let mut two_step = negotiation_request(&[&format!("want {}", tip.to_hex())]);
    two_step.extend(fetch_request(&[&format!("want {}", tip.to_hex())]));
    let summary2 = upload_pack(
        &repo,
        &mut Cursor::new(two_step),
        &mut Vec::new(),
        &mut ServeOptions {
            protocol: ProtocolVersion::V2,
            stateless_rpc: false,
            agent: "grit-test".to_owned(),
            ..ServeOptions::default()
        },
    )
    .expect("stateful v2 two-step");
    assert_eq!(summary2.negotiation_rounds, 1);
    assert!(summary2.objects_sent > 0);
}

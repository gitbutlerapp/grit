//! Protocol v2 `fetch` served in process: `deepen-not` ref resolution, the
//! `wanted-refs` section for `want-ref`, and the `fetch=` capability line,
//! each checked against the system `git upload-pack` answering the same request.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::io::{Cursor, Write};
use std::path::Path;
use std::process::{Command, Stdio};

use grit_lib::config::environment::RepositoryOptions;
use grit_lib::remote::serve::{
    upload_pack, DeepenNotError, ProtocolVersion, ServeError, ServeOptions,
};
use grit_lib::repo::Repository;

fn git(dir: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .current_dir(dir)
        .args(args)
        .env("GIT_AUTHOR_NAME", "T")
        .env("GIT_AUTHOR_EMAIL", "t@example.com")
        .env("GIT_AUTHOR_DATE", "2005-04-07T22:13:13 +0200")
        .env("GIT_COMMITTER_NAME", "T")
        .env("GIT_COMMITTER_EMAIL", "t@example.com")
        .env("GIT_COMMITTER_DATE", "2005-04-07T22:13:13 +0200")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .output()
        .expect("run git");
    assert!(
        out.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8(out.stdout)
        .expect("utf8")
        .trim()
        .to_owned()
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
    for required in [
        "server-option",
        "object-info",
        "fetch=shallow wait-for-done",
    ] {
        assert!(
            grit_caps.iter().any(|l| l == required),
            "grit missing {required:?}; caps={grit_caps:?}"
        );
        assert!(
            git_caps.iter().any(|l| l == required),
            "git missing {required:?}; caps={git_caps:?}"
        );
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
    assert!(!git_caps.iter().any(|l| l == "object-info"), "{git_caps:?}");
    assert!(
        !grit_caps.iter().any(|l| l == "object-info"),
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

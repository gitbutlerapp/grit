//! Unit tests for smart-HTTP discovery helpers (included from [`super::http`]).
#![cfg(test)]

use super::*;
use crate::error::{Error, RemoteUnreadableReason};
use crate::remote::RemoteError;

#[test]
fn rebase_redirect_strips_info_refs_and_query() {
    let base = "https://tangled.org/me/repo";
    assert_eq!(
        rebased_base_from_redirect(
            base,
            Some("https://knot.example/did:plc:xyz/info/refs?service=git-upload-pack")
        )
        .as_deref(),
        Some("https://knot.example/did:plc:xyz")
    );
    assert_eq!(
        rebased_base_from_redirect(base, Some("https://host/smart/repo/info/refs")).as_deref(),
        Some("https://host/smart/repo")
    );
}

#[test]
fn effective_info_refs_url_uses_redirect_target() {
    let requested = smart_info_refs_discovery_url("http://127.0.0.1:9/smart-redir-auth/repo.git");
    let final_url = "http://127.0.0.1:9/auth/smart/repo.git/info/refs?service=git-upload-pack";
    let effective = effective_info_refs_url_after_redirect(&requested, Some(final_url));
    assert_eq!(
        effective,
        smart_info_refs_discovery_url("http://127.0.0.1:9/auth/smart/repo.git")
    );
}

#[test]
fn http_origin_distinguishes_localhost_from_loopback() {
    assert!(!http_origins_match(
        "http://127.0.0.1:8080/repo",
        "http://localhost:8080/repo"
    ));
    assert!(http_origins_match(
        "http://127.0.0.1:8080/x",
        "http://127.0.0.1:8080/y"
    ));
    assert_eq!(
        http_origin_key("https://Example.com/repo").as_deref(),
        Some("https://example.com:443")
    );
}

#[test]
fn rebase_redirect_none_when_no_change_or_unknown() {
    let base = "https://host/smart/repo";
    assert_eq!(rebased_base_from_redirect(base, None), None);
    assert_eq!(
        rebased_base_from_redirect(
            base,
            Some("https://host/smart/repo/info/refs?service=git-upload-pack")
        ),
        None
    );
    assert_eq!(
        rebased_base_from_redirect(base, Some("https://host/elsewhere")),
        None
    );
}

#[test]
fn html_info_refs_body_is_not_git_server() {
    let body = b"<html><body>not git</body></html>";
    let err = discover_upload_pack_from_body(body).expect_err("html must fail");
    assert!(matches!(
        err,
        Error::Remote(RemoteError::Unreadable {
            url: None,
            reason: RemoteUnreadableReason::NotGitServer { status: 200, .. },
        })
    ));
    assert_eq!(
        err.to_string(),
        "the remote did not answer like a Git server (got 200)"
    );
}

#[test]
fn http_content_type_media_type_strips_parameters() {
    assert_eq!(
        http_content_type_media_type("text/html; charset=utf-8"),
        "text/html"
    );
}

#[test]
fn strips_smart_service_preamble() {
    let mut body = Vec::new();
    pkt_line::write_line_to_vec(&mut body, "# service=git-upload-pack\n").unwrap();
    body.extend_from_slice(b"0000");
    let oid = "1".repeat(40);
    let line = format!("{oid} refs/heads/main\0multi_ack_detailed side-band-64k");
    pkt_line::write_line_to_vec(&mut body, &line).unwrap();
    body.extend_from_slice(b"0000");

    let stripped = strip_service_advertisement(&body).unwrap();
    let disc = parse_advertisement(stripped).unwrap();
    assert_eq!(disc.protocol_version, 0);
    assert_eq!(disc.refs.len(), 1);
    assert_eq!(disc.refs[0].name, "refs/heads/main");
    assert!(disc.caps.contains("side-band-64k"));
}

#[test]
fn parse_advertisement_drops_malicious_ref_and_symref() {
    let oid = "aabbccddeeff00112233445566778899aabbccdd";
    let malicious = "refs/heads/../../../config";
    let mut body = Vec::new();
    let head = format!("{oid} HEAD\0symref=HEAD:{malicious}");
    pkt_line::write_line_to_vec(&mut body, &head).unwrap();
    pkt_line::write_line_to_vec(&mut body, &format!("{oid} {malicious}")).unwrap();
    body.extend_from_slice(b"0000");

    let disc = parse_advertisement(&body).unwrap();
    assert!(disc.head_symref.is_none());
    assert!(
        !disc.refs.iter().any(|r| r.name.contains("../")),
        "malicious ref must not appear in HTTP discovery"
    );
}

#[test]
fn parses_symref_and_caps() {
    let mut body = Vec::new();
    let main = "2".repeat(40);
    let head =
        format!("{main} HEAD\0multi_ack_detailed symref=HEAD:refs/heads/main object-format=sha1");
    pkt_line::write_line_to_vec(&mut body, &head).unwrap();
    let r = format!("{main} refs/heads/main");
    pkt_line::write_line_to_vec(&mut body, &r).unwrap();
    body.extend_from_slice(b"0000");

    let disc = parse_advertisement(&body).unwrap();
    assert_eq!(disc.head_symref.as_deref(), Some("refs/heads/main"));
    assert_eq!(disc.object_format, "sha1");
    assert!(disc.refs.iter().any(|r| r.name == "HEAD"));
    assert!(disc.refs.iter().any(|r| r.name == "refs/heads/main"));
}

#[test]
fn detects_v2_preamble() {
    let mut body = Vec::new();
    pkt_line::write_line_to_vec(&mut body, "version 2").unwrap();
    pkt_line::write_line_to_vec(&mut body, "ls-refs").unwrap();
    pkt_line::write_line_to_vec(&mut body, "object-format=sha256").unwrap();
    body.extend_from_slice(b"0000");
    let disc = parse_advertisement(&body).unwrap();
    assert_eq!(disc.protocol_version, 2);
    assert_eq!(disc.object_format, "sha256");
}

#[test]
fn url_helpers() {
    assert_eq!(
        info_refs_url("http://h/r.git"),
        "http://h/r.git/info/refs?service=git-upload-pack"
    );
    assert_eq!(
        info_refs_url("http://h/r.git/"),
        "http://h/r.git/info/refs?service=git-upload-pack"
    );
    assert_eq!(
        format!("http://h/r.git/{UPLOAD_PACK}"),
        "http://h/r.git/git-upload-pack"
    );
}

struct MockHttp {
    body: Vec<u8>,
}

impl HttpClient for MockHttp {
    fn get(&self, _url: &str, _git_protocol: Option<&str>) -> crate::remote::RemoteResult<Vec<u8>> {
        Ok(self.body.clone())
    }

    fn post(
        &self,
        _url: &str,
        _content_type: &str,
        _accept: &str,
        _body: &[u8],
        _git_protocol: Option<&str>,
    ) -> crate::remote::RemoteResult<Vec<u8>> {
        Ok(Vec::new())
    }

    fn get_with_final_url_exact(
        &self,
        _url: &str,
        _git_protocol: Option<&str>,
    ) -> crate::remote::RemoteResult<(Vec<u8>, Option<String>)> {
        Ok((self.body.clone(), None))
    }
}

fn sample_smart_advertisement() -> Vec<u8> {
    let oid = "0000000000000000000000000000000000000000";
    let mut body = String::new();
    body.push_str("001e# service=git-upload-pack\n");
    body.push_str("0000");
    body.push_str(&format!("0032{oid} capabilities^{}\n", "multi_ack"));
    body.push_str("0000");
    body.into_bytes()
}

fn remote_fatal_message(err: crate::error::Error) -> String {
    match err {
        crate::error::Error::Remote(crate::remote::RemoteError::Fatal { message }) => message,
        other => panic!("expected RemoteError::Fatal, got {other:?}"),
    }
}

fn pkt_line(text: &str) -> Vec<u8> {
    let mut buf = Vec::new();
    pkt_line::write_line_to_vec(&mut buf, text).expect("pkt-line");
    buf
}

fn http_info_refs(preamble: bool, lines: &[&str]) -> Vec<u8> {
    let mut body = Vec::new();
    if preamble {
        body.extend(pkt_line("# service=git-upload-pack"));
        body.extend_from_slice(b"0000");
    }
    for text in lines {
        body.extend(pkt_line(text));
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
fn discover_upload_pack_client_helpers_and_git_advertisement() {
    let body = sample_smart_advertisement();
    let _ = discover_upload_pack_from_body(&body);
    let _ = discover_upload_pack_for_list_refs_from_body(&body);
    let client = MockHttp { body: body.clone() };
    let _ = discover_upload_pack(&client, "https://example.com/r.git", None);
    let _ = discover_upload_pack_exact(&client, "https://example.com/r.git", Some("version=2"));
    let _ = discover_upload_pack_for_list_refs(&client, "https://example.com/r.git", None);

    if std::process::Command::new("git")
        .arg("--version")
        .output()
        .is_err()
    {
        eprintln!("SKIP: no system git for live advertisement round trip");
        return;
    }

    use crate::repo::{InitOptions, Repository};
    use std::process::Command;

    fn hermetic_git(cmd: &mut Command) {
        cmd.env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_SYSTEM", "/dev/null");
    }

    let dir = tempfile::tempdir().expect("tempdir");
    Repository::init(
        dir.path(),
        &InitOptions::new()
            .with_initial_branch("main".to_owned())
            .with_ref_storage(crate::RefStorageFormat::Files),
    )
    .expect("init");
    std::fs::write(dir.path().join("f.txt"), b"x\n").expect("write");
    {
        let mut add = Command::new("git");
        add.current_dir(dir.path()).args(["add", "f.txt"]);
        hermetic_git(&mut add);
        let out = add.output().expect("git add");
        assert!(
            out.status.success(),
            "git add: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }
    {
        let mut commit = Command::new("git");
        commit.current_dir(dir.path()).args(["commit", "-qm", "c"]);
        commit
            .env("GIT_AUTHOR_NAME", "T")
            .env("GIT_AUTHOR_EMAIL", "t@e.com")
            .env("GIT_COMMITTER_NAME", "T")
            .env("GIT_COMMITTER_EMAIL", "t@e.com");
        hermetic_git(&mut commit);
        let out = commit.output().expect("git commit");
        assert!(
            out.status.success(),
            "git commit: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }
    let git_advert = {
        let mut cmd = Command::new("git");
        cmd.current_dir(dir.path())
            .args(["upload-pack", "--advertise-refs", "."]);
        hermetic_git(&mut cmd);
        cmd.output().expect("git upload-pack")
    };
    assert!(
        git_advert.status.success(),
        "git upload-pack: {}",
        String::from_utf8_lossy(&git_advert.stderr)
    );
    let (_version, refs, _caps, _agent) =
        discover_upload_pack_from_body(&git_advert.stdout).expect("parse git advertisement");
    assert!(
        refs.iter().any(|(name, _)| name == "refs/heads/main"),
        "parsed advertisement must list main: {refs:?}"
    );
    let (_version2, list_refs, _caps2, _agent2) =
        discover_upload_pack_for_list_refs_from_body(&git_advert.stdout)
            .expect("list refs from git");
    assert!(
        list_refs.iter().any(|(name, _)| name == "refs/heads/main"),
        "list-refs parse must include main: {list_refs:?}"
    );
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

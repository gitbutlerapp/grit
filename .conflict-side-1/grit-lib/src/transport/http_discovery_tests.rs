//! Unit tests for smart-HTTP discovery helpers (included from [`super::http`]).
use super::*;

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

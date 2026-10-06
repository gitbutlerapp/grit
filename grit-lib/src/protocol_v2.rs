//! Pure protocol-v2 capability parsing and request-fragment building.
//!
//! These helpers operate purely on the textual capability/feature lines exchanged during a
//! Git wire-protocol-v2 conversation (the lines between `version 2` and the first flush, plus
//! the space-separated feature list inside a `fetch=` capability). They have no I/O, no
//! environment access, and no transport-backend coupling, so they are shared by every v2 client
//! transport (file://, git://, ssh, smart-HTTP) rather than duplicated per backend.
//!
//! See `Documentation/gitprotocol-v2.txt` and `serve.c` / `connect.c` / `fetch-pack.c` in Git.

use std::collections::HashSet;

/// A section header in a protocol-v2 `command=fetch` response.
///
/// See `Documentation/gitprotocol-v2.txt` ("The response of fetch is broken into a number of
/// sections…").
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum FetchResponseSection {
    /// Negotiation round-trips (`ACK`/`NAK`/`ready`); omitted when the server jumps straight to the pack.
    Acknowledgments,
    /// Shallow boundary updates when the client sent `deepen` / related lines.
    ShallowInfo,
    /// OID resolution for client `want-ref` lines when ref-in-want is enabled.
    WantedRefs,
    /// Alternate pack URIs when the client requested `packfile-uris`.
    PackfileUris,
    /// Side-band-64k framed pack data.
    Packfile,
}

impl FetchResponseSection {
    /// PKT-LINE section header written/read on the wire.
    #[must_use]
    pub const fn header(self) -> &'static str {
        match self {
            Self::Acknowledgments => "acknowledgments",
            Self::ShallowInfo => "shallow-info",
            Self::WantedRefs => "wanted-refs",
            Self::PackfileUris => "packfile-uris",
            Self::Packfile => "packfile",
        }
    }
}

/// Section order after negotiation completes: optional prelude sections, then `packfile`.
///
/// Matches gitprotocol-v2 (and upstream `upload-pack.c` after shallow/wanted-ref order was fixed).
/// Returns true when `shallow-info` appears before `wanted-refs` in a fetch response prelude.
#[must_use]
pub fn fetch_prelude_shallow_before_wanted_refs(section_headers: &[&str]) -> bool {
    let shallow = section_headers
        .iter()
        .position(|h| *h == FetchResponseSection::ShallowInfo.header());
    let wanted = section_headers
        .iter()
        .position(|h| *h == FetchResponseSection::WantedRefs.header());
    match (shallow, wanted) {
        (Some(s), Some(w)) => s < w,
        _ => true,
    }
}

pub const FETCH_RESPONSE_SECTION_ORDER: &[FetchResponseSection] = &[
    FetchResponseSection::Acknowledgments,
    FetchResponseSection::ShallowInfo,
    FetchResponseSection::WantedRefs,
    FetchResponseSection::PackfileUris,
    FetchResponseSection::Packfile,
];

/// True when the server's v2 capability advertisement offers the `bundle-uri` command.
///
/// Advertised either bare (`bundle-uri`) or with a value (`bundle-uri=...`).
#[must_use]
pub fn server_advertises_bundle_uri(caps: &[String]) -> bool {
    caps.iter()
        .any(|c| c == "bundle-uri" || c.starts_with("bundle-uri="))
}

/// Build the capability lines a client echoes back when issuing a follow-up v2 command
/// (e.g. `command=bundle-uri` or `command=fetch`).
///
/// Mirrors the client capability handling in `connect.c`: the `agent=` line is forwarded
/// verbatim and `object-format=<hash>` is re-emitted. Other advertised capabilities are not
/// echoed in the per-command capability list.
#[must_use]
pub fn cap_lines_for_command_request(caps: &[String]) -> Vec<String> {
    let mut out = Vec::new();
    for line in caps {
        if line.starts_with("agent=") {
            out.push(line.clone());
        } else if let Some(fmt) = line.strip_prefix("object-format=") {
            out.push(format!("object-format={fmt}"));
        }
    }
    out
}

/// Collect the space-separated feature tokens advertised by the server's v2 `fetch=` capability.
///
/// Returns an empty set when the server advertises no `fetch=` capability.
#[must_use]
pub fn fetch_features(caps: &[String]) -> HashSet<String> {
    let mut features = HashSet::new();
    for line in caps {
        if let Some(rest) = line.strip_prefix("fetch=") {
            for feature in rest.split_whitespace() {
                features.insert(feature.to_string());
            }
        }
    }
    features
}

/// True when the server's v2 `fetch=` capability lists `<feature>`.
#[must_use]
pub fn fetch_supports_feature(caps: &[String], feature: &str) -> bool {
    caps.iter().any(|c| {
        c.strip_prefix("fetch=")
            .is_some_and(|rest| rest.split_whitespace().any(|w| w == feature))
    })
}

/// True when the server's `fetch=` capability advertises `sideband-all`.
#[must_use]
pub fn fetch_supports_sideband_all(caps: &[String]) -> bool {
    fetch_supports_feature(caps, "sideband-all")
}

/// True when the server's v2 `fetch=` capability advertises `ref-in-want` (so the client may send
/// `want-ref <name>` lines instead of resolving named refspecs to OIDs itself).
#[must_use]
pub fn fetch_supports_ref_in_want(caps: &[String]) -> bool {
    fetch_supports_feature(caps, "ref-in-want")
}

/// True when the server's v2 `fetch=` capability advertises `filter` (so the client may send a
/// `filter <spec>` line).
///
/// Mirrors `fetch-pack.c` `send_filter`, which only writes the `filter` request line when the
/// server advertised filtering support. A promisor remote without `uploadpack.allowFilter` does
/// not advertise it, so the client must silently drop the filter and fetch unfiltered rather than
/// send a line the server rejects with "unexpected line".
#[must_use]
pub fn fetch_supports_filter(caps: &[String]) -> bool {
    fetch_supports_feature(caps, "filter")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s(v: &[&str]) -> Vec<String> {
        v.iter().map(|x| (*x).to_owned()).collect()
    }

    #[test]
    fn fetch_prelude_order_helper() {
        assert!(fetch_prelude_shallow_before_wanted_refs(&[
            "shallow-info",
            "wanted-refs",
            "packfile"
        ]));
        assert!(!fetch_prelude_shallow_before_wanted_refs(&[
            "wanted-refs",
            "shallow-info"
        ]));
    }

    #[test]
    fn fetch_response_section_order_matches_protocol_v2() {
        assert_eq!(
            FETCH_RESPONSE_SECTION_ORDER
                .iter()
                .map(|s| s.header())
                .collect::<Vec<_>>(),
            [
                "acknowledgments",
                "shallow-info",
                "wanted-refs",
                "packfile-uris",
                "packfile"
            ]
        );
    }

    #[test]
    fn bundle_uri_bare_and_valued() {
        assert!(server_advertises_bundle_uri(&s(&[
            "agent=git/2",
            "bundle-uri"
        ])));
        assert!(server_advertises_bundle_uri(&s(&["bundle-uri=foo"])));
        assert!(!server_advertises_bundle_uri(&s(&[
            "agent=git/2",
            "ls-refs"
        ])));
        assert!(!server_advertises_bundle_uri(&s(&[])));
    }

    #[test]
    fn cap_lines_forwards_agent_and_object_format() {
        let caps = s(&[
            "version 2",
            "agent=git/2.43",
            "ls-refs=unborn",
            "object-format=sha256",
            "fetch=shallow filter",
        ]);
        assert_eq!(
            cap_lines_for_command_request(&caps),
            s(&["agent=git/2.43", "object-format=sha256"])
        );
    }

    #[test]
    fn cap_lines_empty_when_no_agent_or_format() {
        assert_eq!(
            cap_lines_for_command_request(&s(&["version 2", "ls-refs"])),
            Vec::<String>::new()
        );
    }

    #[test]
    fn fetch_features_splits_on_whitespace() {
        let caps = s(&["fetch=shallow filter ref-in-want sideband-all"]);
        let f = fetch_features(&caps);
        assert!(f.contains("shallow"));
        assert!(f.contains("filter"));
        assert!(f.contains("ref-in-want"));
        assert!(f.contains("sideband-all"));
        assert_eq!(f.len(), 4);
    }

    #[test]
    fn fetch_features_empty_without_fetch_cap() {
        assert!(fetch_features(&s(&["ls-refs", "agent=x"])).is_empty());
    }

    #[test]
    fn per_feature_helpers() {
        let caps = s(&["fetch=ref-in-want filter sideband-all"]);
        assert!(fetch_supports_sideband_all(&caps));
        assert!(fetch_supports_ref_in_want(&caps));
        assert!(fetch_supports_filter(&caps));
        assert!(fetch_supports_feature(&caps, "ref-in-want"));
        assert!(!fetch_supports_feature(&caps, "shallow"));

        let none = s(&["fetch=shallow", "ls-refs"]);
        assert!(!fetch_supports_sideband_all(&none));
        assert!(!fetch_supports_ref_in_want(&none));
        assert!(!fetch_supports_filter(&none));
    }
}

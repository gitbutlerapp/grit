//! Bundle URIs: seeding a clone from a prebuilt bundle the server points at.
//!
//! A server that keeps a bundle of its repository advertises the protocol v2
//! `bundle-uri` command (see Git's `gitprotocol-v2` and the bundle-URI design
//! notes). A client sends `command=bundle-uri` and gets back a bundle list:
//! `key=value` lines such as
//!
//! ```text
//! bundle.version=1
//! bundle.mode=all
//! bundle.clone.uri=https://example.com/repo/bundles/1700000000-ab12cd.bundle
//! ```
//!
//! [`BundleList`] parses and writes those lines. [`fetch_bundle_list`] asks a
//! smart-HTTP server for its list, and [`seed_from_bundle_uri`] is the clone
//! step: it downloads the advertised bundle with
//! [`crate::resumable::download_resumable`] (so an interrupted download picks
//! up where it stopped on the next clone), unpacks it, and records its tips as
//! `refs/bundles/*`. The clone's fetch then offers those tips as `have`s, so
//! the server only sends what is newer than the bundle.
//!
//! Git clients use the same list when `transfer.bundleURI` is enabled.

use std::io::Cursor;
use std::path::{Path, PathBuf};

use crate::error::{Error, Result};
use crate::fetch::Progress;
use crate::objects::ObjectId;
use crate::pkt_line::{self, Packet};
use crate::progress::{Cancel, ProgressPhase, ProgressSink, ProgressUnit};
use crate::protocol_v2::{cap_lines_for_command_request, server_advertises_bundle_uri};
use crate::repo::Repository;
use crate::resumable::{discard_download, download_resumable, resolve_url};
use crate::transfer::TransferStats;
use crate::transport::http::{discover_upload_pack_exact, HttpClient};

/// Ref namespace a clone records bundle tips under while it fetches.
pub const BUNDLE_REFS_PREFIX: &str = "refs/bundles/";

/// How a client should use the bundles in a [`BundleList`] (`bundle.mode`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum BundleListMode {
    /// Every bundle is needed (`all`).
    #[default]
    All,
    /// Any one bundle is enough (`any`).
    Any,
}

impl BundleListMode {
    fn as_str(self) -> &'static str {
        match self {
            Self::All => "all",
            Self::Any => "any",
        }
    }
}

/// One bundle in a [`BundleList`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BundleListEntry {
    /// The bundle's id in the list (`bundle.<id>.*`).
    pub id: String,
    /// Where to download it; relative URIs resolve against the repository URL.
    pub uri: String,
    /// Orders bundles from oldest to newest (`bundle.<id>.creationToken`).
    pub creation_token: Option<u64>,
}

/// A bundle list as the `bundle-uri` command returns it.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct BundleList {
    /// `bundle.mode`.
    pub mode: BundleListMode,
    /// The bundles, in the order the server listed them.
    pub bundles: Vec<BundleListEntry>,
}

impl BundleList {
    /// Parse `key=value` lines. Unknown keys are ignored; bundles without a
    /// `uri` are dropped.
    ///
    /// # Errors
    ///
    /// [`Error::Message`] when `bundle.version` is present and is not `1`.
    pub fn parse<'a>(lines: impl IntoIterator<Item = &'a str>) -> Result<Self> {
        let mut list = Self::default();
        let mut entries: Vec<(String, Option<String>, Option<u64>)> = Vec::new();
        for line in lines {
            let Some((key, value)) = line.trim_end().split_once('=') else {
                continue;
            };
            let Some(key) = key.strip_prefix("bundle.") else {
                continue;
            };
            match key.to_ascii_lowercase().as_str() {
                "version" if value != "1" => {
                    return Err(Error::Message(format!(
                        "unsupported bundle list version {value}"
                    )))
                }
                "mode" => {
                    list.mode = if value == "any" {
                        BundleListMode::Any
                    } else {
                        BundleListMode::All
                    };
                }
                _ => {}
            }
            let Some((id, field)) = key.rsplit_once('.') else {
                continue;
            };
            let index = match entries.iter().position(|(i, _, _)| i == id) {
                Some(i) => i,
                None => {
                    entries.push((id.to_owned(), None, None));
                    entries.len() - 1
                }
            };
            match field.to_ascii_lowercase().as_str() {
                "uri" => entries[index].1 = Some(value.to_owned()),
                "creationtoken" => entries[index].2 = value.parse().ok(),
                _ => {}
            }
        }
        list.bundles = entries
            .into_iter()
            .filter_map(|(id, uri, creation_token)| {
                Some(BundleListEntry {
                    id,
                    uri: uri?,
                    creation_token,
                })
            })
            .collect();
        Ok(list)
    }

    /// The `key=value` lines of this list, ready to send as pkt-lines.
    #[must_use]
    pub fn to_lines(&self) -> Vec<String> {
        let mut lines = vec![
            "bundle.version=1".to_owned(),
            format!("bundle.mode={}", self.mode.as_str()),
        ];
        if self.bundles.iter().any(|b| b.creation_token.is_some()) {
            lines.push("bundle.heuristic=creationToken".to_owned());
        }
        for bundle in &self.bundles {
            lines.push(format!("bundle.{}.uri={}", bundle.id, bundle.uri));
            if let Some(token) = bundle.creation_token {
                lines.push(format!("bundle.{}.creationToken={token}", bundle.id));
            }
        }
        lines
    }

    /// The bundle a fresh clone should start from: the newest by creation
    /// token, else the first listed.
    #[must_use]
    pub fn clone_bundle(&self) -> Option<&BundleListEntry> {
        self.bundles
            .iter()
            .filter(|b| b.creation_token.is_some())
            .max_by_key(|b| b.creation_token)
            .or_else(|| self.bundles.first())
    }
}

/// Ask the smart-HTTP server at `repo_url` for its bundle list.
///
/// Discovers the server's protocol v2 capabilities; when it offers
/// `bundle-uri`, sends `command=bundle-uri` and parses the answer.
///
/// # Returns
///
/// `None` when the server does not speak protocol v2 or offers no bundles.
///
/// # Errors
///
/// HTTP failures and malformed answers.
pub fn fetch_bundle_list(client: &dyn HttpClient, repo_url: &str) -> Result<Option<BundleList>> {
    let (version, _refs, caps, _head) =
        discover_upload_pack_exact(client, repo_url, Some("version=2"))?;
    if version != 2 || !server_advertises_bundle_uri(&caps) {
        return Ok(None);
    }
    let mut request = Vec::new();
    pkt_line::write_line_to_vec(&mut request, "command=bundle-uri")?;
    for line in cap_lines_for_command_request(&caps) {
        pkt_line::write_line_to_vec(&mut request, &line)?;
    }
    request.extend_from_slice(b"0001");
    request.extend_from_slice(b"0000");
    let url = format!("{}/git-upload-pack", repo_url.trim_end_matches('/'));
    let body = client.post(
        &url,
        "application/x-git-upload-pack-request",
        "application/x-git-upload-pack-result",
        &request,
        Some("version=2"),
    )?;
    let mut lines = Vec::new();
    let mut cursor = Cursor::new(body);
    while let Some(packet) = pkt_line::read_packet(&mut cursor)? {
        match packet {
            Packet::Data(line) => {
                if let Some(msg) = line.strip_prefix("ERR ") {
                    return Err(Error::Message(format!("bundle-uri: {msg}")));
                }
                lines.push(line);
            }
            _ => break,
        }
    }
    let list = BundleList::parse(lines.iter().map(String::as_str))?;
    Ok((!list.bundles.is_empty()).then_some(list))
}

/// What [`seed_from_bundle_uri`] did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BundleSeed {
    /// The server offers no bundle; the clone fetches everything.
    NotOffered,
    /// The bundle was downloaded and unpacked.
    Seeded(SeededBundle),
    /// The bundle could not be used; the clone fetches everything. A partial
    /// download is kept so the next clone resumes it.
    Failed {
        /// The bundle URL, when the failure came after the list was read.
        uri: Option<String>,
        /// Why.
        reason: String,
    },
}

/// A bundle that seeded a clone.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SeededBundle {
    /// Where it was downloaded from.
    pub uri: String,
    /// Size of the bundle file.
    pub bytes: u64,
    /// Bytes an earlier interrupted clone had already downloaded.
    pub resumed_from: u64,
    /// Objects and pack bytes unpacked from it.
    pub transfer: TransferStats,
    /// Tips recorded under [`BUNDLE_REFS_PREFIX`].
    pub refs: Vec<(String, ObjectId)>,
}

/// Where a download of `uri` is kept in `cache_dir`, so an interrupted clone
/// and the next one agree on it.
#[must_use]
pub fn bundle_cache_path(cache_dir: &Path, uri: &str) -> PathBuf {
    let digest = crate::objects::HashAlgo::Sha256.checksum_digest(uri.as_bytes());
    cache_dir.join(format!("{}.bundle", hex::encode(&digest[..12])))
}

/// Seed the empty repository `repo` from the bundle the server at `repo_url`
/// advertises.
///
/// Downloads the bundle into `cache_dir` (resuming a download an earlier clone
/// left there), unpacks its pack into `repo`, and points
/// `refs/bundles/<name>` at each of its tips (`refs/heads/main` becomes
/// `refs/bundles/heads/main`). The download is deleted once unpacked. Call
/// [`remove_bundle_refs`] after the clone's fetch.
///
/// Failures never stop a clone: they come back as [`BundleSeed::Failed`].
///
/// # Parameters
///
/// - `repo`: the new, empty repository.
/// - `client`: the HTTP client for the remote.
/// - `repo_url`: the remote repository URL.
/// - `cache_dir`: where bundle downloads are kept; created when missing.
/// - `progress`: receives the download's
///   [`crate::resumable::ResumableEvent`]s and the unpacking phases.
/// - `cancel`: checked while the bundle is unpacked.
#[must_use]
pub fn seed_from_bundle_uri(
    repo: &Repository,
    client: &dyn HttpClient,
    repo_url: &str,
    cache_dir: &Path,
    progress: &mut dyn Progress,
    cancel: &dyn Cancel,
) -> BundleSeed {
    let list = match fetch_bundle_list(client, repo_url) {
        Ok(Some(list)) => list,
        Ok(None) => return BundleSeed::NotOffered,
        Err(e) => {
            return BundleSeed::Failed {
                uri: None,
                reason: e.to_string(),
            }
        }
    };
    let Some(entry) = list.clone_bundle() else {
        return BundleSeed::NotOffered;
    };
    let base = format!("{}/", repo_url.trim_end_matches('/'));
    let Some(uri) = resolve_url(&base, &entry.uri) else {
        return BundleSeed::Failed {
            uri: Some(entry.uri.clone()),
            reason: "the bundle URI is not a valid URL".to_owned(),
        };
    };
    match seed_from(repo, client, &uri, cache_dir, progress, cancel) {
        Ok(seeded) => BundleSeed::Seeded(seeded),
        Err(e) => BundleSeed::Failed {
            uri: Some(uri),
            reason: e.to_string(),
        },
    }
}

fn seed_from(
    repo: &Repository,
    client: &dyn HttpClient,
    uri: &str,
    cache_dir: &Path,
    progress: &mut dyn Progress,
    cancel: &dyn Cancel,
) -> Result<SeededBundle> {
    std::fs::create_dir_all(cache_dir)?;
    let dest = bundle_cache_path(cache_dir, uri);
    let downloaded =
        download_resumable(client, uri, &dest, progress).map_err(|e| Error::from(Box::new(e)))?;
    let bundle = repo.bundles().open(&dest).map_err(bundle_error)?;
    if !bundle.header().prerequisites.is_empty() {
        // An incremental bundle cannot seed an empty repository.
        discard_download(&dest)?;
        return Err(Error::Message(
            "the advertised bundle needs history this clone does not have".to_owned(),
        ));
    }
    // Unpacking copies the bundle's pack locally; only indexing it is news.
    let mut indexing = WithoutReceiving {
        inner: progress,
        muted: false,
    };
    let unbundled = match bundle.unbundle_with_progress(repo, &mut indexing, cancel) {
        Ok(u) => u,
        Err(e) => {
            // A corrupt download would fail the same way next time.
            discard_download(&dest)?;
            return Err(bundle_error(e));
        }
    };
    let mut refs = Vec::new();
    for tip in &unbundled.refs {
        let Some(rest) = tip.name.strip_prefix("refs/") else {
            continue;
        };
        let name = format!("{BUNDLE_REFS_PREFIX}{rest}");
        if crate::check_ref_format::check_refname_format(
            &name,
            &crate::check_ref_format::RefNameOptions::default(),
        )
        .is_err()
        {
            continue;
        }
        crate::refs::write_ref(repo.git_dir(), &name, &tip.oid)?;
        refs.push((name, tip.oid));
    }
    discard_download(&dest)?;
    Ok(SeededBundle {
        uri: uri.to_owned(),
        bytes: downloaded.bytes,
        resumed_from: downloaded.resumed_from,
        transfer: unbundled.transfer,
        refs,
    })
}

/// Forwards progress except the [`ProgressPhase::ReceivingObjects`] phase.
struct WithoutReceiving<'a> {
    inner: &'a mut dyn Progress,
    muted: bool,
}

impl ProgressSink for WithoutReceiving<'_> {
    fn start_with_unit(&mut self, phase: ProgressPhase, unit: ProgressUnit, total: Option<u64>) {
        self.muted = phase == ProgressPhase::ReceivingObjects;
        if !self.muted {
            self.inner.start_with_unit(phase, unit, total);
        }
    }

    fn start(&mut self, phase: ProgressPhase, total: Option<u64>) {
        self.start_with_unit(phase, phase.default_unit(), total);
    }

    fn inc(&mut self, units: u64) {
        if !self.muted {
            self.inner.inc(units);
        }
    }

    fn set(&mut self, current: u64) {
        if !self.muted {
            self.inner.set(current);
        }
    }

    fn finish(&mut self) {
        if !self.muted {
            self.inner.finish();
        }
        self.muted = false;
    }
}

fn bundle_error(e: crate::bundle::BundleError) -> Error {
    Error::Message(format!("bundle: {e}"))
}

/// Delete the `refs/bundles/*` refs [`seed_from_bundle_uri`] wrote, once the
/// clone's fetch no longer needs them as `have`s.
///
/// # Errors
///
/// Ref listing or deletion failures.
pub fn remove_bundle_refs(repo: &Repository) -> Result<()> {
    for (name, _) in crate::refs::list_refs(repo.git_dir(), BUNDLE_REFS_PREFIX)? {
        crate::refs::delete_ref(repo.git_dir(), &name)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bundle_lists_round_trip() {
        let list = BundleList {
            mode: BundleListMode::All,
            bundles: vec![BundleListEntry {
                id: "clone".to_owned(),
                uri: "https://h/r/bundles/1-ab.bundle".to_owned(),
                creation_token: Some(1),
            }],
        };
        let lines = list.to_lines();
        assert_eq!(
            lines,
            [
                "bundle.version=1",
                "bundle.mode=all",
                "bundle.heuristic=creationToken",
                "bundle.clone.uri=https://h/r/bundles/1-ab.bundle",
                "bundle.clone.creationToken=1",
            ]
        );
        assert_eq!(
            BundleList::parse(lines.iter().map(String::as_str)).ok(),
            Some(list)
        );
    }

    #[test]
    fn bundle_lists_pick_the_newest_bundle_and_skip_junk() {
        let list = BundleList::parse([
            "bundle.version=1",
            "bundle.mode=any",
            "bundle.old.uri=old.bundle",
            "bundle.old.creationToken=5",
            "bundle.new.uri=new.bundle",
            "bundle.new.creationToken=9",
            "bundle.nouri.creationToken=10",
            "not a key",
            "other.key=1",
        ]);
        let list = match list {
            Ok(l) => l,
            Err(e) => panic!("parse: {e}"),
        };
        assert_eq!(list.mode, BundleListMode::Any);
        assert_eq!(list.bundles.len(), 2);
        assert_eq!(list.clone_bundle().map(|b| b.id.as_str()), Some("new"));
        assert!(BundleList::parse(["bundle.version=2"]).is_err());
        assert_eq!(BundleList::default().clone_bundle(), None);
    }

    #[test]
    fn cache_paths_depend_only_on_the_uri() {
        let dir = Path::new("/cache");
        assert_eq!(
            bundle_cache_path(dir, "http://h/a"),
            bundle_cache_path(dir, "http://h/a")
        );
        assert_ne!(
            bundle_cache_path(dir, "http://h/a"),
            bundle_cache_path(dir, "http://h/b")
        );
        assert!(bundle_cache_path(dir, "x").starts_with(dir));
    }
}

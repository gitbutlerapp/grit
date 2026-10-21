//! Change identifiers stored in commit headers.
//!
//! Grit records a stable [`ChangeId`] in the `change-id` header (and reads the
//! legacy `gitbutler-change-id` name). Identifiers can be randomly generated,
//! derived from a commit id (Jujutsu-compatible), or controlled via
//! [`ChangeIdPolicy`] and repository config.

use std::fmt;

use thiserror::Error;

use crate::config::ConfigSet;
use crate::error::{Error, Result};
use crate::objects::{
    parse_commit, read_commit_subject_line, serialize_commit, CommitData, HashAlgo, ObjectId,
    ObjectKind,
};
use crate::repo::Repository;
use crate::rev_list::{all_ref_commit_tips_for_walk, walk_closure_ordered, CommitGraph};
use crate::rev_parse_error::{AmbiguousChangeIdHint, RevParseError};

const REVERSE_HEX_LEN: usize = 32;
const CHANGE_ID_KEY: &[u8] = b"change-id";
const LEGACY_CHANGE_ID_KEY: &[u8] = b"gitbutler-change-id";

/// A change identifier as stored in a commit header (reverse-hex or opaque text).
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ChangeId(String);

/// Failure modes for change-id parsing and generation.
#[derive(Debug, Error, PartialEq, Eq)]
pub enum ChangeIdError {
    /// Reverse-hex form must be exactly 32 characters.
    #[error("change id reverse-hex must be 32 characters")]
    WrongLength,
    /// A character is outside the reverse-hex alphabet (`k`–`z`).
    #[error("invalid character in change id reverse-hex")]
    InvalidCharacter,
    /// OS randomness failed.
    #[error("failed to read random bytes for change id: {0}")]
    RandomFailed(getrandom::Error),
}

/// Injectable source of random change-id bytes.
pub trait ChangeIdGenerator: Send + Sync {
    /// Return 16 fresh random bytes for [`ChangeId::from_bytes`].
    ///
    /// # Errors
    ///
    /// Returns [`ChangeIdError::RandomFailed`] when entropy is unavailable.
    fn random_bytes(&self) -> std::result::Result<[u8; 16], ChangeIdError>;
}

/// Default generator backed by the OS CSPRNG (`getrandom`).
#[derive(Debug, Clone, Copy, Default)]
pub struct OsChangeIdGenerator;

impl ChangeIdGenerator for OsChangeIdGenerator {
    fn random_bytes(&self) -> std::result::Result<[u8; 16], ChangeIdError> {
        let mut buf = [0u8; 16];
        getrandom::getrandom(&mut buf).map_err(ChangeIdError::RandomFailed)?;
        Ok(buf)
    }
}

/// A commit whose resolved change id matched a prefix lookup.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChangeIdMatch {
    /// Commit object id.
    pub oid: ObjectId,
    /// Stored or derived change id for that commit.
    pub change_id: ChangeId,
}

/// Resolved change id plus whether it came from a stored header.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedChangeId {
    /// The effective change id.
    pub id: ChangeId,
    /// `true` when read from `change-id` or `gitbutler-change-id`.
    pub stored: bool,
}

/// Repository and testing knobs for assigning change ids to new commits.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChangeIdPolicy {
    enabled: bool,
    testing: ChangeIdTestingMode,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum ChangeIdTestingMode {
    Normal,
    ContentHash,
    Verbatim(String),
}

impl ChangeId {
    /// Encode 16 raw bytes as GitButler reverse-hex (32 characters).
    #[must_use]
    pub fn from_bytes(bytes: &[u8; 16]) -> Self {
        let mut s = String::with_capacity(REVERSE_HEX_LEN);
        for byte in bytes {
            push_reverse_hex_nibble(&mut s, byte >> 4);
            push_reverse_hex_nibble(&mut s, byte & 0x0f);
        }
        Self(s)
    }

    /// Generate a random id using `generator`.
    ///
    /// # Errors
    ///
    /// Propagates [`ChangeIdError::RandomFailed`] from the generator.
    pub fn generate(generator: &dyn ChangeIdGenerator) -> std::result::Result<Self, ChangeIdError> {
        let bytes = generator.random_bytes()?;
        Ok(Self::from_bytes(&bytes))
    }

    /// Parse strict reverse-hex form (32 chars, alphabet `k`–`z`).
    ///
    /// # Errors
    ///
    /// Returns [`ChangeIdError::WrongLength`] or [`ChangeIdError::InvalidCharacter`].
    pub fn parse_reverse_hex(s: &str) -> std::result::Result<Self, ChangeIdError> {
        if s.len() != REVERSE_HEX_LEN {
            return Err(ChangeIdError::WrongLength);
        }
        if !s.bytes().all(is_reverse_hex_byte) {
            return Err(ChangeIdError::InvalidCharacter);
        }
        Ok(Self(s.to_owned()))
    }

    /// Whether the stored value is valid reverse-hex.
    #[must_use]
    pub fn is_reverse_hex(&self) -> bool {
        self.0.len() == REVERSE_HEX_LEN && self.0.bytes().all(is_reverse_hex_byte)
    }

    /// Full header value.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Prefix of at most `n` Unicode scalar values for display.
    #[must_use]
    pub fn short(&self, n: usize) -> &str {
        if n == 0 {
            return "";
        }
        let end = self
            .0
            .char_indices()
            .nth(n)
            .map_or(self.0.len(), |(byte_idx, _)| byte_idx);
        &self.0[..end]
    }

    /// Jujutsu/GitButler deterministic id from a commit object id.
    #[must_use]
    pub fn derived_from_commit_id(oid: &ObjectId) -> Self {
        let raw = oid.as_bytes();
        let slice = &raw[4..20];
        let mut transformed = [0u8; 16];
        for (i, &b) in slice.iter().rev().enumerate() {
            transformed[i] = b.reverse_bits();
        }
        Self::from_bytes(&transformed)
    }

    #[must_use]
    fn from_opaque(value: String) -> Self {
        Self(value)
    }
}

impl fmt::Display for ChangeId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl ChangeIdPolicy {
    /// Load `grit.changeIds` (default true) and `grit.testing.changeId`.
    #[must_use]
    pub fn from_config(config: &ConfigSet) -> Self {
        let enabled = config
            .get_bool("grit.changeIds")
            .and_then(|r| r.ok())
            .unwrap_or(true);
        let testing = config
            .get("grit.testing.changeId")
            .map(|s| s.trim().to_owned())
            .filter(|s| !s.is_empty())
            .map(|value| {
                if value.eq_ignore_ascii_case("content-hash") {
                    ChangeIdTestingMode::ContentHash
                } else {
                    ChangeIdTestingMode::Verbatim(value)
                }
            })
            .unwrap_or(ChangeIdTestingMode::Normal);
        Self { enabled, testing }
    }

    /// Whether this policy assigns an id to newly written commits.
    #[must_use]
    pub fn enabled(&self) -> bool {
        self.enabled
    }

    /// Compute the change id for a new commit, if policy allows.
    ///
    /// # Errors
    ///
    /// Propagates random generation failures.
    pub fn id_for_new_commit(
        &self,
        data: &CommitData,
        algo: HashAlgo,
        generator: &dyn ChangeIdGenerator,
    ) -> std::result::Result<Option<ChangeId>, ChangeIdError> {
        if !self.enabled {
            return Ok(None);
        }
        match &self.testing {
            ChangeIdTestingMode::Normal => Ok(Some(ChangeId::generate(generator)?)),
            ChangeIdTestingMode::ContentHash => {
                let mut stripped = data.clone();
                strip_change_id_headers(&mut stripped.extra_headers);
                let bytes = serialize_commit(&stripped);
                let digest = algo.digest(&bytes);
                let raw = digest.as_bytes();
                let mut chunk = [0u8; 16];
                chunk.copy_from_slice(&raw[4..20]);
                Ok(Some(ChangeId::from_bytes(&chunk)))
            }
            ChangeIdTestingMode::Verbatim(value) => Ok(Some(ChangeId::from_opaque(value.clone()))),
        }
    }
}

/// Read `change-id`, falling back to `gitbutler-change-id`.
#[must_use]
pub fn change_id(data: &CommitData) -> Option<ChangeId> {
    let mut legacy = None;
    for_each_extra_header(&data.extra_headers, |key, value| {
        if key == CHANGE_ID_KEY {
            return Some(ChangeId::from_opaque(
                String::from_utf8_lossy(value).into_owned(),
            ));
        }
        if key == LEGACY_CHANGE_ID_KEY && legacy.is_none() {
            legacy = Some(ChangeId::from_opaque(
                String::from_utf8_lossy(value).into_owned(),
            ));
        }
        None
    })
    .or(legacy)
}

/// Set or replace the `change-id` header (before any signature block).
pub fn set_change_id(data: &mut CommitData, id: &ChangeId) {
    let line = format!("change-id {}\n", id.as_str());
    data.extra_headers = insert_change_id_line(&data.extra_headers, line.as_bytes());
}

/// Stored header when present, otherwise [`ChangeId::derived_from_commit_id`].
#[must_use]
pub fn resolved_change_id(oid: &ObjectId, data: &CommitData) -> ResolvedChangeId {
    if let Some(id) = change_id(data) {
        ResolvedChangeId { id, stored: true }
    } else {
        ResolvedChangeId {
            id: ChangeId::derived_from_commit_id(oid),
            stored: false,
        }
    }
}

/// Whether `spec` can name a change id (4–32 chars, alphabet `k`–`z`).
#[must_use]
pub fn is_change_id_spec_token(spec: &str) -> bool {
    let len = spec.len();
    (4..=32).contains(&len) && spec.bytes().all(is_reverse_hex_byte)
}

/// Walk commits reachable from all refs and `HEAD`, returning those whose resolved
/// change id starts with `prefix`.
///
/// # Errors
///
/// Propagates ref listing, graph walk, and object read failures.
pub fn find_commits_by_change_id_prefix(
    repo: &Repository,
    prefix: &str,
) -> Result<Vec<ChangeIdMatch>> {
    debug_assert!(is_change_id_spec_token(prefix));
    let tips = all_ref_commit_tips_for_walk(repo)?;
    let mut graph = CommitGraph::with_commit_graph(repo, false, true);
    let (_, order) = walk_closure_ordered(&mut graph, &tips)?;
    let mut matches = Vec::new();
    for oid in order {
        let Ok(obj) = repo.read_replaced(&oid) else {
            continue;
        };
        if obj.kind != ObjectKind::Commit {
            continue;
        }
        let resolved = if commit_bytes_contain_change_id_header(&obj.data) {
            let Ok(data) = parse_commit(&obj.data) else {
                continue;
            };
            resolved_change_id(&oid, &data)
        } else {
            ResolvedChangeId {
                id: ChangeId::derived_from_commit_id(&oid),
                stored: false,
            }
        };
        if resolved.id.as_str().starts_with(prefix) {
            matches.push(ChangeIdMatch {
                oid,
                change_id: resolved.id,
            });
        }
    }
    matches.sort_by_key(|m| m.oid.to_hex());
    Ok(matches)
}

/// Resolve a change-id token (full id or unique prefix) to a commit oid.
///
/// # Errors
///
/// Returns [`Error::ObjectNotFound`] when nothing matches,
/// [`Error::RevParse`] with [`RevParseError::AmbiguousChangeId`] when several commits match.
pub fn resolve_commit_by_change_id_prefix(repo: &Repository, prefix: &str) -> Result<ObjectId> {
    let matches = find_commits_by_change_id_prefix(repo, prefix)?;
    match matches.len() {
        0 => Err(Error::ObjectNotFound(prefix.to_owned())),
        1 => Ok(matches[0].oid),
        _ => Err(RevParseError::AmbiguousChangeId {
            prefix: prefix.to_owned(),
            candidates: ambiguous_change_id_hints(repo, &matches)?,
        }
        .into()),
    }
}

fn ambiguous_change_id_hints(
    repo: &Repository,
    matches: &[ChangeIdMatch],
) -> Result<Vec<AmbiguousChangeIdHint>> {
    let mut out = Vec::with_capacity(matches.len());
    for m in matches {
        let subject = repo
            .read_replaced(&m.oid)
            .ok()
            .and_then(|obj| read_commit_subject_line(&obj.data).ok())
            .unwrap_or_else(|| "(no subject)".to_owned());
        out.push(AmbiguousChangeIdHint {
            oid_hex: m.oid.to_hex(),
            change_id: m.change_id.as_str().to_owned(),
            subject,
        });
    }
    Ok(out)
}

fn commit_bytes_contain_change_id_header(data: &[u8]) -> bool {
    header_key_present(data, CHANGE_ID_KEY) || header_key_present(data, LEGACY_CHANGE_ID_KEY)
}

fn header_key_present(data: &[u8], key: &[u8]) -> bool {
    if data.starts_with(key) {
        return true;
    }
    data.windows(key.len() + 1)
        .any(|w| w[0] == b'\n' && &w[1..] == key)
}

fn push_reverse_hex_nibble(out: &mut String, nibble: u8) {
    debug_assert!(nibble < 16);
    out.push(char::from(b'z' - nibble));
}

fn is_reverse_hex_byte(b: u8) -> bool {
    (b'k'..=b'z').contains(&b)
}

fn strip_change_id_headers(extra: &mut Vec<u8>) {
    *extra = remove_header_lines(extra, |key| {
        key == CHANGE_ID_KEY || key == LEGACY_CHANGE_ID_KEY
    });
}

fn insert_change_id_line(extra: &[u8], line: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    let mut pos = 0usize;
    let mut inserted = false;
    while pos < extra.len() {
        let (key, block_end, is_continued) = next_header_block(extra, pos);
        if !inserted && (key == b"gpgsig" || key == b"gpgsig-sha256") {
            out.extend_from_slice(line);
            inserted = true;
        }
        if key == CHANGE_ID_KEY || key == LEGACY_CHANGE_ID_KEY {
            pos = block_end;
            continue;
        }
        out.extend_from_slice(&extra[pos..block_end]);
        pos = block_end;
        let _ = is_continued;
    }
    if !inserted {
        out.extend_from_slice(line);
    }
    out
}

fn remove_header_lines(extra: &[u8], drop_key: impl Fn(&[u8]) -> bool) -> Vec<u8> {
    let mut out = Vec::new();
    let mut pos = 0usize;
    while pos < extra.len() {
        let (key, block_end, _) = next_header_block(extra, pos);
        if drop_key(key) {
            pos = block_end;
            continue;
        }
        out.extend_from_slice(&extra[pos..block_end]);
        pos = block_end;
    }
    out
}

fn next_header_block(extra: &[u8], start: usize) -> (&[u8], usize, bool) {
    let pos = start;
    let line_start = pos;
    let mut line_end = pos;
    while line_end < extra.len() && extra[line_end] != b'\n' {
        line_end += 1;
    }
    let after_nl = (line_end + 1).min(extra.len());
    let line = &extra[line_start..line_end];
    if line.first() == Some(&b' ') {
        return (b"", after_nl, true);
    }
    let key_end = line.iter().position(|&b| b == b' ').unwrap_or(line.len());
    let key = &line[..key_end];
    let mut end = after_nl;
    while end < extra.len() {
        let ls = end;
        let mut le = end;
        while le < extra.len() && extra[le] != b'\n' {
            le += 1;
        }
        let an = (le + 1).min(extra.len());
        if extra.get(ls) == Some(&b' ') {
            end = an;
        } else {
            break;
        }
    }
    (key, end, false)
}

fn for_each_extra_header(
    extra: &[u8],
    mut f: impl FnMut(&[u8], &[u8]) -> Option<ChangeId>,
) -> Option<ChangeId> {
    let mut pos = 0usize;
    while pos < extra.len() {
        let line_start = pos;
        let mut line_end = pos;
        while line_end < extra.len() && extra[line_end] != b'\n' {
            line_end += 1;
        }
        let after_nl = (line_end + 1).min(extra.len());
        let line = &extra[line_start..line_end];
        if line.first() == Some(&b' ') {
            pos = after_nl;
            continue;
        }
        let key_end = line.iter().position(|&b| b == b' ').unwrap_or(line.len());
        let key = &line[..key_end];
        let value = line.get(key_end + 1..).unwrap_or_default();
        if let Some(id) = f(key, value) {
            return Some(id);
        }
        pos = after_nl;
        while pos < extra.len() {
            let ls = pos;
            let mut le = pos;
            while le < extra.len() && extra[le] != b'\n' {
                le += 1;
            }
            let an = (le + 1).min(extra.len());
            if extra.get(ls) == Some(&b' ') {
                pos = an;
            } else {
                break;
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;
    use crate::config::environment::RepositoryOptions;
    use crate::repo::{InitOptions, Repository};

    struct FixedGenerator([u8; 16]);

    impl ChangeIdGenerator for FixedGenerator {
        fn random_bytes(&self) -> std::result::Result<[u8; 16], ChangeIdError> {
            Ok(self.0)
        }
    }

    #[test]
    fn reverse_hex_roundtrip_and_alphabet() {
        assert_eq!(
            ChangeId::from_bytes(&[0x00; 16]).as_str(),
            "zzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzz"
        );
        assert_eq!(
            ChangeId::from_bytes(&[0xff; 16]).as_str(),
            "kkkkkkkkkkkkkkkkkkkkkkkkkkkkkkkk"
        );
        let mut one = [0u8; 16];
        one[0] = 0xc0;
        assert_eq!(&ChangeId::from_bytes(&one).as_str()[..2], "nz");
        let bytes = [
            0x12, 0x34, 0x12, 0x34, 0x12, 0x34, 0x12, 0x34, 0x12, 0x34, 0x12, 0x34, 0x12, 0x34,
            0x12, 0x34,
        ];
        let id = ChangeId::from_bytes(&bytes);
        assert!(id.is_reverse_hex());
        assert_eq!(ChangeId::parse_reverse_hex(id.as_str()).expect("parse"), id);
    }

    #[test]
    fn parse_rejects_bad_len_and_chars() {
        assert_eq!(
            ChangeId::parse_reverse_hex("tooshort"),
            Err(ChangeIdError::WrongLength)
        );
        let mut s = "k".repeat(32);
        s.replace_range(0..1, "j");
        assert_eq!(
            ChangeId::parse_reverse_hex(&s),
            Err(ChangeIdError::InvalidCharacter)
        );
    }

    #[test]
    fn derived_matches_jujutsu_vectors() {
        let cases = [
            (
                "0123456789abcdef0123456789abcdef01234567",
                "ltpxnvrzksowmuqyltpxnvrzksowmuqy",
            ),
            (
                "4b825dc642cb6eb9a060e54bf8d69288fbee4904",
                "xzqxssmkyyvqtoykmxpsztzuqmstmwvx",
            ),
            (
                "e69de29bb2d1d6434b8b29ae775ad8c2e48c5391",
                "rqnpwyxsvwyoupllsuqvmymxnxtorovm",
            ),
        ];
        for (hex, want) in cases {
            let oid = ObjectId::from_hex(hex).expect("oid");
            assert_eq!(ChangeId::derived_from_commit_id(&oid).as_str(), want);
        }
    }

    #[test]
    fn reads_change_id_before_legacy_header() {
        let mut data = CommitData {
            tree: ObjectId::from_hex("4b825dc642cb6eb9a060e54bf8d69288fbee4904").unwrap(),
            parents: vec![],
            author: String::new(),
            committer: String::new(),
            author_raw: Vec::new(),
            committer_raw: Vec::new(),
            encoding: None,
            message: String::new(),
            raw_message: None,
            extra_headers: b"gitbutler-change-id legacy\nchange-id preferred\n".to_vec(),
        };
        assert_eq!(
            change_id(&data).map(|id| id.as_str().to_owned()),
            Some("preferred".to_owned())
        );
        data.extra_headers = b"gitbutler-change-id only-legacy\n".to_vec();
        assert_eq!(
            change_id(&data).map(|id| id.as_str().to_owned()),
            Some("only-legacy".to_owned())
        );
    }

    #[test]
    fn set_replaces_existing_and_inserts_before_gpgsig() {
        let mut data = CommitData {
            tree: ObjectId::from_hex("4b825dc642cb6eb9a060e54bf8d69288fbee4904").unwrap(),
            parents: vec![],
            author: String::new(),
            committer: String::new(),
            author_raw: Vec::new(),
            committer_raw: Vec::new(),
            encoding: None,
            message: String::new(),
            raw_message: None,
            extra_headers: b"change-id old\nencoding UTF-8\n".to_vec(),
        };
        let new_id = ChangeId::from_opaque("new-id".to_owned());
        set_change_id(&mut data, &new_id);
        let raw = String::from_utf8_lossy(&data.extra_headers);
        assert!(!raw.contains("change-id old"));
        assert!(raw.contains("change-id new-id"));

        data.extra_headers = b"encoding UTF-8\n".to_vec();
        set_change_id(&mut data, &new_id);
        assert!(String::from_utf8_lossy(&data.extra_headers).contains("change-id new-id"));

        data.extra_headers =
            b"change-id old\ngpgsig -----BEGIN SSH SIGNATURE-----\n sig\n -----END SSH SIGNATURE-----\n"
                .to_vec();
        set_change_id(&mut data, &new_id);
        let raw = String::from_utf8_lossy(&data.extra_headers);
        assert!(raw.find("change-id new-id").unwrap() < raw.find("gpgsig").unwrap());

        data.extra_headers = b"gitbutler-change-id legacy-only\nencoding UTF-8\n".to_vec();
        set_change_id(&mut data, &new_id);
        let raw = String::from_utf8_lossy(&data.extra_headers);
        assert!(!raw.contains("gitbutler-change-id"));
        assert!(!raw.contains("legacy-only"));
        assert_eq!(raw.matches("change-id new-id").count(), 1);
    }

    #[test]
    fn short_respects_utf8_char_boundaries_for_verbatim_ids() {
        let mut cfg = ConfigSet::new();
        cfg.add_command_override("grit.testing.changeId", "é")
            .expect("override");
        let policy = ChangeIdPolicy::from_config(&cfg);
        let data = CommitData {
            tree: ObjectId::from_hex("4b825dc642cb6eb9a060e54bf8d69288fbee4904").unwrap(),
            parents: vec![],
            author: String::new(),
            committer: String::new(),
            author_raw: Vec::new(),
            committer_raw: Vec::new(),
            encoding: None,
            message: String::new(),
            raw_message: None,
            extra_headers: Vec::new(),
        };
        let id = policy
            .id_for_new_commit(&data, HashAlgo::Sha1, &FixedGenerator([0; 16]))
            .expect("ok")
            .expect("id");
        assert_eq!(id.as_str(), "é");
        assert_eq!(id.short(1), "é");
        assert_eq!(id.short(0), "");
    }

    #[test]
    fn policy_disabled_and_testing_modes() {
        let mut cfg = ConfigSet::new();
        cfg.add_command_override("grit.changeIds", "false")
            .expect("override");
        let policy = ChangeIdPolicy::from_config(&cfg);
        assert!(!policy.enabled());
        let data = CommitData {
            tree: ObjectId::from_hex("4b825dc642cb6eb9a060e54bf8d69288fbee4904").unwrap(),
            parents: vec![],
            author: "a".into(),
            committer: "c".into(),
            author_raw: Vec::new(),
            committer_raw: Vec::new(),
            encoding: None,
            message: "m".into(),
            raw_message: None,
            extra_headers: Vec::new(),
        };
        let gen = FixedGenerator([1; 16]);
        assert!(policy
            .id_for_new_commit(&data, HashAlgo::Sha1, &gen)
            .expect("ok")
            .is_none());

        cfg.add_command_override("grit.changeIds", "true")
            .expect("override");
        cfg.add_command_override("grit.testing.changeId", "content-hash")
            .expect("override");
        let policy = ChangeIdPolicy::from_config(&cfg);
        let id1 = policy
            .id_for_new_commit(&data, HashAlgo::Sha1, &gen)
            .expect("ok")
            .expect("some");
        let id2 = policy
            .id_for_new_commit(&data, HashAlgo::Sha1, &gen)
            .expect("ok")
            .expect("some");
        assert_eq!(id1, id2);

        cfg.add_command_override("grit.testing.changeId", "fixed-test-value")
            .expect("override");
        let policy = ChangeIdPolicy::from_config(&cfg);
        let id = policy
            .id_for_new_commit(&data, HashAlgo::Sha1, &gen)
            .expect("ok")
            .expect("some");
        assert_eq!(id.as_str(), "fixed-test-value");
    }

    #[test]
    fn generator_is_injectable() {
        let gen = FixedGenerator([0xab; 16]);
        let id = ChangeId::generate(&gen).expect("generate");
        assert_eq!(id, ChangeId::from_bytes(&[0xab; 16]));
    }

    #[test]
    fn repository_exposes_injected_change_id_generator() -> crate::error::Result<()> {
        let gen: Arc<dyn ChangeIdGenerator> = Arc::new(FixedGenerator([0xcd; 16]));
        let dir = tempfile::tempdir().map_err(crate::error::Error::Io)?;
        let init = Repository::init(dir.path(), &InitOptions::default())?;
        let opts = RepositoryOptions::empty()
            .with_change_id_generator(Arc::clone(&gen))
            .with_work_tree(dir.path());
        let repo = Repository::open(init.git_dir(), &opts)?;
        let id = ChangeId::generate(repo.change_id_generator().as_ref()).expect("generate");
        assert_eq!(id, ChangeId::from_bytes(&[0xcd; 16]));
        Ok(())
    }

    #[test]
    fn resolved_change_id_stored_vs_derived() {
        let oid = ObjectId::from_hex("0123456789abcdef0123456789abcdef01234567").unwrap();
        let mut data = CommitData {
            tree: ObjectId::from_hex("4b825dc642cb6eb9a060e54bf8d69288fbee4904").unwrap(),
            parents: vec![],
            author: String::new(),
            committer: String::new(),
            author_raw: Vec::new(),
            committer_raw: Vec::new(),
            encoding: None,
            message: String::new(),
            raw_message: None,
            extra_headers: b"change-id stored\n".to_vec(),
        };
        let resolved = resolved_change_id(&oid, &data);
        assert!(resolved.stored);
        assert_eq!(resolved.id.as_str(), "stored");
        data.extra_headers.clear();
        let resolved = resolved_change_id(&oid, &data);
        assert!(!resolved.stored);
        assert_eq!(
            resolved.id.as_str(),
            ChangeId::derived_from_commit_id(&oid).as_str()
        );
    }

    #[test]
    fn public_api_surface_smoke() {
        let _ = CHANGE_ID_KEY;
        let _ = LEGACY_CHANGE_ID_KEY;
        let id = ChangeId::from_bytes(&[0; 16]);
        let _ = id.short(4);
        let _ = format!("{id}");
        let policy = ChangeIdPolicy::from_config(&ConfigSet::new());
        let _ = policy.enabled();
        let _ = OsChangeIdGenerator;
        let _ = ResolvedChangeId {
            id: id.clone(),
            stored: false,
        };
        let _ = ChangeIdError::WrongLength;
        let data = CommitData {
            tree: ObjectId::from_hex("4b825dc642cb6eb9a060e54bf8d69288fbee4904").unwrap(),
            parents: vec![],
            author: String::new(),
            committer: String::new(),
            author_raw: Vec::new(),
            committer_raw: Vec::new(),
            encoding: None,
            message: String::new(),
            raw_message: None,
            extra_headers: Vec::new(),
        };
        let _ = change_id(&data);
        set_change_id(&mut { data.clone() }, &id);
        let oid = ObjectId::from_hex("0123456789abcdef0123456789abcdef01234567").unwrap();
        let _ = resolved_change_id(&oid, &data);
    }
}

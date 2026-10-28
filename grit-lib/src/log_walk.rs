//! Resumable HEAD log walks for bounded `grit log` paging.
//!
//! [`LogWalkCursor`] encodes the heap and visited sets of the date-order reachable walk
//! so `--cursor` continues the same traversal instead of restarting from a single commit.

use std::io::{Cursor, Read, Write};
use std::ops::Range;
use std::path::{Path, PathBuf};

use crate::error::{Error, Result};
use crate::merge_base::{ReachableWalkCommitterCutoffs, ReachableWalkSnapshot, ReachableWalkState};
use crate::objects::{parse_commit, CommitData, ObjectId, OidHashMap};
use crate::porcelain::log::CommitReuse;
use crate::repo::Repository;
use crate::revwalk::queries::{resolve_log_walk_tips_and_hide, MergeFilter, RevWalkListOptions};

const CURSOR_PREFIX: &str = "lw1.";
/// Repository-local short continuation token (`lw1.s.` + eight id chars).
const SHORT_CURSOR_PREFIX: &str = "lw1.s.";
const SHORT_CURSOR_ID_LEN: usize = 8;
const MAGIC_V2: &[u8; 4] = b"GLW2";
const MAGIC_V3: &[u8; 4] = b"GLW3";
const NO_CUTOFF: i64 = i64::MIN;

/// Tips, exclusions, and date cutoffs for a resumable date-order log walk.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LogWalkQuery {
    /// Positive walk tips (resolved commit oids).
    pub tips: Vec<ObjectId>,
    /// Hide tips whose ancestors are omitted from output.
    pub hide: Vec<ObjectId>,
    /// Committer-date filters applied during the walk.
    pub cutoffs: ReachableWalkCommitterCutoffs,
}

impl LogWalkQuery {
    /// Build a walk query when revision selection maps to default-order reachable walks.
    ///
    /// Returns [`None`] when `walk` uses options that require full `rev-list` (paths,
    /// `--reverse`, `--first-parent`, parent-count filters, or `--skip`).
    ///
    /// # Errors
    ///
    /// Propagates rev-parse and ref errors from [`resolve_log_walk_tips_and_hide`].
    pub fn from_rev_walk(
        repo: &Repository,
        tokens: &[&str],
        walk: &RevWalkListOptions,
    ) -> Result<Option<Self>> {
        if walk.graph
            || !walk.paths.is_empty()
            || walk.reverse
            || walk.first_parent
            || walk.merges != MergeFilter::Any
            || walk.skip > 0
            || walk.commit_filter.as_ref().is_some_and(|f| f.is_active())
            || walk.pickaxe.is_some()
        {
            return Ok(None);
        }
        let (tips, hide) = resolve_log_walk_tips_and_hide(repo, tokens, walk)?;
        Ok(Some(Self {
            tips,
            hide,
            cutoffs: ReachableWalkCommitterCutoffs {
                since: walk.since,
                until: walk.until,
            },
        }))
    }
}

/// Opaque continuation token for paged `grit log` history walks.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LogWalkCursor {
    query: LogWalkQuery,
    walk: ReachableWalkSnapshot,
}

/// One page of commits plus an optional continuation cursor.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LogWalkPage {
    /// Emitted commits, newest first.
    pub commits: Vec<ObjectId>,
    /// Cursor for the next page, or [`None`] when the walk is exhausted.
    pub next: Option<LogWalkCursor>,
    /// Commits from [`Self::commits`] that the walk already read from the object database.
    /// Commits whose parents and dates came from the commit-graph are absent.
    ///
    /// Pass this to
    /// [`load_commits_with_decorations_reusing`](crate::porcelain::log::load_commits_with_decorations_reusing)
    /// to build output rows without reading those commits again.
    pub commit_data: WalkedCommits,
}

/// Commit objects a log walk already read, kept as raw object bytes and parsed when taken.
///
/// The payloads share one buffer instead of a parsed [`CommitData`] (or allocation) per
/// commit: a long page then costs one large block that goes back to the operating system
/// when dropped, rather than thousands of small ones that raise the heap's high-water mark.
#[derive(Clone, Debug, Default)]
pub struct WalkedCommits {
    bytes: Vec<u8>,
    spans: OidHashMap<Range<usize>>,
}

impl WalkedCommits {
    /// Empty set with room for the ids of `commits` commits.
    pub(crate) fn with_capacity(commits: usize) -> Self {
        Self {
            bytes: Vec::new(),
            spans: crate::objects::oid_hash_map_with_capacity(commits),
        }
    }

    /// Keep a copy of the commit object payload `data` for `oid`.
    pub(crate) fn insert(&mut self, oid: ObjectId, data: &[u8]) {
        let start = self.bytes.len();
        self.bytes.extend_from_slice(data);
        self.spans.insert(oid, start..self.bytes.len());
    }

    /// Forget every commit not in `keep`.
    pub(crate) fn retain(&mut self, keep: impl Fn(&ObjectId) -> bool) {
        self.spans.retain(|oid, _| keep(oid));
    }

    /// Number of commits held.
    #[must_use]
    pub fn len(&self) -> usize {
        self.spans.len()
    }

    /// Whether no commits are held.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.spans.is_empty()
    }

    /// Whether the commit `oid` is held.
    #[must_use]
    pub fn contains(&self, oid: &ObjectId) -> bool {
        self.spans.contains_key(oid)
    }

    /// Ids of the commits held, in no particular order.
    pub fn oids(&self) -> impl Iterator<Item = &ObjectId> + '_ {
        self.spans.keys()
    }

    fn payload(&self, span: &Range<usize>) -> &[u8] {
        self.bytes.get(span.clone()).unwrap_or_default()
    }
}

/// Equal when both hold the same commits with the same payloads, however they are laid out.
impl PartialEq for WalkedCommits {
    fn eq(&self, other: &Self) -> bool {
        self.spans.len() == other.spans.len()
            && self.spans.iter().all(|(oid, span)| {
                other
                    .spans
                    .get(oid)
                    .is_some_and(|theirs| self.payload(span) == other.payload(theirs))
            })
    }
}

impl Eq for WalkedCommits {}

impl CommitReuse for WalkedCommits {
    /// Remove `oid` and parse it, exactly as [`parse_commit`] parses the object payload.
    fn take_commit(&mut self, oid: &ObjectId) -> Option<Result<CommitData>> {
        let span = self.spans.remove(oid)?;
        Some(parse_commit(self.payload(&span)))
    }

    fn cached_subject_line(&self, oid: &ObjectId) -> Option<Result<String>> {
        let span = self.spans.get(oid)?;
        Some(crate::objects::read_commit_subject_line(self.payload(span)))
    }
}

impl LogWalkCursor {
    /// Revision tips, exclusions, and date cutoffs bound to this continuation token.
    #[must_use]
    pub fn query(&self) -> &LogWalkQuery {
        &self.query
    }

    /// Start or resume a date-order walk for `query`, emitting up to `limit` commits.
    ///
    /// Pass `cursor` from a previous page's [`LogWalkPage::next`]. When `cursor` is
    /// [`None`], a fresh walk begins from `query`.
    ///
    /// # Errors
    ///
    /// Returns [`Error::CorruptObject`] for an invalid cursor, when the query changed since
    /// the cursor was issued, or when commit objects cannot be read.
    pub fn step(
        repo: &Repository,
        query: &LogWalkQuery,
        cursor: Option<&LogWalkCursor>,
        limit: usize,
    ) -> Result<LogWalkPage> {
        if limit == 0 {
            return Ok(LogWalkPage {
                commits: Vec::new(),
                next: cursor.cloned(),
                commit_data: WalkedCommits::default(),
            });
        }

        let resume = match cursor {
            None => None,
            Some(c) => {
                if c.query != *query {
                    return Err(Error::CorruptObject(
                        "log walk cursor does not match the current revision selection".to_owned(),
                    ));
                }
                Some(c.walk.clone())
            }
        };

        let (state, commits, commit_data) = ReachableWalkState::page_retaining_commits(
            repo,
            resume,
            &query.tips,
            &query.hide,
            limit,
            query.cutoffs,
        )?;
        let next = state
            .has_emittable_pending(query.cutoffs)
            .then(|| LogWalkCursor {
                query: query.clone(),
                walk: state.snapshot(),
            });
        Ok(LogWalkPage {
            commits,
            next,
            commit_data,
        })
    }

    /// Encode this cursor for `--cursor` (URL-safe, opaque to callers).
    ///
    /// # Errors
    ///
    /// Returns [`Error::CorruptObject`] if internal serialization fails.
    pub fn encode(&self) -> Result<String> {
        let mut buf = Vec::new();
        write_snapshot_v3(&mut buf, &self.query, &self.walk)?;
        Ok(format!("{CURSOR_PREFIX}{}", base64_encode(&buf)))
    }

    /// Store this cursor under `.git/grit/log-walk/` and return a short `--cursor` token.
    ///
    /// # Errors
    ///
    /// Returns [`Error::CorruptObject`] if internal serialization fails, or [`Error::Io`] when
    /// the snapshot cannot be written.
    pub fn encode_short(&self, repo: &Repository) -> Result<String> {
        let mut buf = Vec::new();
        write_snapshot_v3(&mut buf, &self.query, &self.walk)?;
        let id = random_short_cursor_id()?;
        write_short_cursor_file(repo.git_dir(), &id, &buf)?;
        Ok(format!("{SHORT_CURSOR_PREFIX}{id}"))
    }

    /// Decode a `--cursor` token produced by [`Self::encode`] or [`Self::encode_short`].
    ///
    /// Short tokens (`lw1.s.…`) require `git_dir` for the repository that issued them.
    ///
    /// # Errors
    ///
    /// Returns [`Error::CorruptObject`] when the token is malformed.
    pub fn decode(token: &str) -> Result<Self> {
        Self::decode_with_git_dir(token, None)
    }

    /// Decode a `--cursor` token, resolving short tokens from `git_dir` when needed.
    ///
    /// # Errors
    ///
    /// Returns [`Error::CorruptObject`] when the token is malformed.
    pub fn decode_with_git_dir(token: &str, git_dir: Option<&Path>) -> Result<Self> {
        if let Some(id) = token.strip_prefix(SHORT_CURSOR_PREFIX) {
            if id.len() != SHORT_CURSOR_ID_LEN || !id.bytes().all(is_short_cursor_id_byte) {
                return Err(Error::CorruptObject(
                    "malformed short log walk cursor".to_owned(),
                ));
            }
            let git_dir = git_dir.ok_or_else(|| {
                Error::CorruptObject("short log walk cursor requires an open repository".to_owned())
            })?;
            let path = log_walk_store_dir(git_dir).join(id);
            let bytes = std::fs::read(&path).map_err(|e| {
                Error::CorruptObject(format!("short log walk cursor not found: {e}"))
            })?;
            return read_snapshot(&bytes);
        }
        let payload = token
            .strip_prefix(CURSOR_PREFIX)
            .ok_or_else(|| Error::CorruptObject("unrecognized log walk cursor".to_owned()))?;
        let bytes = base64_decode(payload.as_bytes())
            .map_err(|_| Error::CorruptObject("log walk cursor base64 decode failed".to_owned()))?;
        read_snapshot(&bytes)
    }
}

fn is_short_cursor_id_byte(b: u8) -> bool {
    b.is_ascii_lowercase() || b.is_ascii_digit()
}

fn log_walk_store_dir(git_dir: &Path) -> PathBuf {
    git_dir.join("grit/log-walk")
}

fn random_short_cursor_id() -> Result<String> {
    const ALPHABET: &[u8; 36] = b"0123456789abcdefghijklmnopqrstuvwxyz";
    let mut buf = [0u8; SHORT_CURSOR_ID_LEN];
    getrandom::getrandom(&mut buf).map_err(|e| Error::Io(std::io::Error::other(e.to_string())))?;
    Ok(buf
        .iter()
        .map(|&b| ALPHABET[(b as usize) % ALPHABET.len()] as char)
        .collect())
}

fn write_short_cursor_file(git_dir: &Path, id: &str, snapshot: &[u8]) -> Result<()> {
    let dir = log_walk_store_dir(git_dir);
    std::fs::create_dir_all(&dir).map_err(Error::Io)?;
    let path = dir.join(id);
    let tmp = dir.join(format!("{id}.tmp"));
    std::fs::write(&tmp, snapshot).map_err(Error::Io)?;
    std::fs::rename(&tmp, &path).map_err(Error::Io)
}

fn write_snapshot_v3(
    out: &mut Vec<u8>,
    query: &LogWalkQuery,
    walk: &ReachableWalkSnapshot,
) -> Result<()> {
    out.write_all(MAGIC_V3).map_err(Error::Io)?;
    write_oid_vec(out, &query.tips)?;
    write_oid_vec(out, &query.hide)?;
    write_i64(out, query.cutoffs.since.unwrap_or(NO_CUTOFF))?;
    write_i64(out, query.cutoffs.until.unwrap_or(NO_CUTOFF))?;
    write_walk_body(out, walk)
}

fn write_walk_body(out: &mut Vec<u8>, walk: &ReachableWalkSnapshot) -> Result<()> {
    write_u64(out, walk.seq)?;
    write_oid_vec(out, &walk.hide)?;
    write_oid_vec(out, &walk.done)?;
    write_oid_vec(out, &walk.queued)?;
    // Reserved `(oid, u32)` list, always written empty; older v3 cursors may fill it.
    write_u32(out, 0)?;
    write_oid_vec(out, &walk.seeded)?;
    write_u32(out, walk.heap.len() as u32)?;
    for &(time, seq, oid) in &walk.heap {
        write_i64(out, time)?;
        write_u64(out, seq)?;
        out.write_all(oid.as_bytes()).map_err(Error::Io)?;
    }
    Ok(())
}

fn read_snapshot(bytes: &[u8]) -> Result<LogWalkCursor> {
    let mut cur = Cursor::new(bytes);
    let mut magic = [0u8; 4];
    cur.read_exact(&mut magic)
        .map_err(|_| Error::CorruptObject("truncated log walk cursor".to_owned()))?;
    let query = if &magic == MAGIC_V3 {
        let tips = read_oid_vec(&mut cur)?;
        let hide = read_oid_vec(&mut cur)?;
        let since_raw = read_i64(&mut cur)?;
        let until_raw = read_i64(&mut cur)?;
        LogWalkQuery {
            tips,
            hide,
            cutoffs: ReachableWalkCommitterCutoffs {
                since: (since_raw != NO_CUTOFF).then_some(since_raw),
                until: (until_raw != NO_CUTOFF).then_some(until_raw),
            },
        }
    } else if &magic == MAGIC_V2 {
        let anchor = read_oid(&mut cur)?;
        LogWalkQuery {
            tips: vec![anchor],
            hide: Vec::new(),
            cutoffs: ReachableWalkCommitterCutoffs::default(),
        }
    } else {
        return Err(Error::CorruptObject(
            "log walk cursor version mismatch".to_owned(),
        ));
    };
    let walk = read_walk_body(&mut cur)?;
    if cur.position() != bytes.len() as u64 {
        return Err(Error::CorruptObject(
            "log walk cursor trailing bytes".to_owned(),
        ));
    }
    Ok(LogWalkCursor { query, walk })
}

fn read_walk_body(cur: &mut Cursor<&[u8]>) -> Result<ReachableWalkSnapshot> {
    let seq = read_u64(cur)?;
    let hide = read_oid_vec(cur)?;
    let done = read_oid_vec(cur)?;
    let queued = read_oid_vec(cur)?;
    for _ in 0..read_u32(cur)? {
        read_oid(cur)?;
        read_u32(cur)?;
    }
    let seeded = read_oid_vec(cur)?;
    let heap_len = read_u32(cur)? as usize;
    let mut heap = Vec::with_capacity(heap_len);
    for _ in 0..heap_len {
        let time = read_i64(cur)?;
        let seq_entry = read_u64(cur)?;
        let oid = read_oid(cur)?;
        heap.push((time, seq_entry, oid));
    }
    Ok(ReachableWalkSnapshot {
        heap,
        done,
        queued,
        seeded,
        seq,
        hide,
    })
}

fn write_u32(out: &mut Vec<u8>, v: u32) -> Result<()> {
    out.write_all(&v.to_le_bytes()).map_err(Error::Io)
}

fn write_u64(out: &mut Vec<u8>, v: u64) -> Result<()> {
    out.write_all(&v.to_le_bytes()).map_err(Error::Io)
}

fn write_i64(out: &mut Vec<u8>, v: i64) -> Result<()> {
    out.write_all(&v.to_le_bytes()).map_err(Error::Io)
}

fn read_u32(cur: &mut Cursor<&[u8]>) -> Result<u32> {
    let mut buf = [0u8; 4];
    cur.read_exact(&mut buf)
        .map_err(|_| Error::CorruptObject("truncated log walk cursor".to_owned()))?;
    Ok(u32::from_le_bytes(buf))
}

fn read_u64(cur: &mut Cursor<&[u8]>) -> Result<u64> {
    let mut buf = [0u8; 8];
    cur.read_exact(&mut buf)
        .map_err(|_| Error::CorruptObject("truncated log walk cursor".to_owned()))?;
    Ok(u64::from_le_bytes(buf))
}

fn read_i64(cur: &mut Cursor<&[u8]>) -> Result<i64> {
    let mut buf = [0u8; 8];
    cur.read_exact(&mut buf)
        .map_err(|_| Error::CorruptObject("truncated log walk cursor".to_owned()))?;
    Ok(i64::from_le_bytes(buf))
}

fn write_oid_vec(out: &mut Vec<u8>, oids: &[ObjectId]) -> Result<()> {
    write_u32(out, oids.len() as u32)?;
    for oid in oids {
        out.write_all(oid.as_bytes()).map_err(Error::Io)?;
    }
    Ok(())
}

fn read_oid_vec(cur: &mut Cursor<&[u8]>) -> Result<Vec<ObjectId>> {
    let len = read_u32(cur)? as usize;
    let mut out = Vec::with_capacity(len);
    for _ in 0..len {
        out.push(read_oid(cur)?);
    }
    Ok(out)
}

fn read_oid(cur: &mut Cursor<&[u8]>) -> Result<ObjectId> {
    let mut buf = [0u8; 20];
    cur.read_exact(&mut buf)
        .map_err(|_| Error::CorruptObject("truncated log walk cursor".to_owned()))?;
    ObjectId::from_bytes(&buf)
}

fn base64_encode(input: &[u8]) -> String {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::new();
    let mut i = 0;
    while i + 3 <= input.len() {
        let n = ((input[i] as u32) << 16) | ((input[i + 1] as u32) << 8) | input[i + 2] as u32;
        out.push(TABLE[((n >> 18) & 63) as usize] as char);
        out.push(TABLE[((n >> 12) & 63) as usize] as char);
        out.push(TABLE[((n >> 6) & 63) as usize] as char);
        out.push(TABLE[(n & 63) as usize] as char);
        i += 3;
    }
    let rem = input.len() - i;
    if rem == 1 {
        let n = (input[i] as u32) << 16;
        out.push(TABLE[((n >> 18) & 63) as usize] as char);
        out.push(TABLE[((n >> 12) & 63) as usize] as char);
        out.push('=');
        out.push('=');
    } else if rem == 2 {
        let n = ((input[i] as u32) << 16) | ((input[i + 1] as u32) << 8);
        out.push(TABLE[((n >> 18) & 63) as usize] as char);
        out.push(TABLE[((n >> 12) & 63) as usize] as char);
        out.push(TABLE[((n >> 6) & 63) as usize] as char);
        out.push('=');
    }
    out
}

fn base64_decode(input: &[u8]) -> std::result::Result<Vec<u8>, ()> {
    fn val(b: u8) -> std::result::Result<u8, ()> {
        match b {
            b'A'..=b'Z' => Ok(b - b'A'),
            b'a'..=b'z' => Ok(b - b'a' + 26),
            b'0'..=b'9' => Ok(b - b'0' + 52),
            b'+' => Ok(62),
            b'/' => Ok(63),
            b'=' => Ok(0),
            _ => Err(()),
        }
    }
    let mut out = Vec::new();
    let mut buf = [0u8; 4];
    let mut n = 0usize;
    for &b in input {
        if b.is_ascii_whitespace() {
            continue;
        }
        buf[n] = b;
        n += 1;
        if n == 4 {
            if buf[0] == b'=' {
                return Err(());
            }
            let a = val(buf[0])?;
            let b = val(buf[1])?;
            let c = val(buf[2])?;
            let d = val(buf[3])?;
            let triple = ((a as u32) << 18) | ((b as u32) << 12) | ((c as u32) << 6) | d as u32;
            out.push((triple >> 16) as u8);
            if buf[2] != b'=' {
                out.push((triple >> 8) as u8);
            }
            if buf[3] != b'=' {
                out.push(triple as u8);
            }
            n = 0;
        }
    }
    if n != 0 {
        return Err(());
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::environment::RepositoryOptions;
    use crate::objects::ObjectKind;
    use crate::odb::Odb;
    use crate::repo::{InitOptions, Repository};
    use std::collections::HashSet;
    use std::process::Command;
    use tempfile::tempdir;

    fn empty_tree(odb: &Odb) -> Result<ObjectId> {
        odb.write_loose_materialize(ObjectKind::Tree, b"")
    }

    fn write_commit(odb: &Odb, parents: &[ObjectId], msg: &str, time: i64) -> Result<ObjectId> {
        let tree = empty_tree(odb)?;
        let mut body = format!("tree {tree}\n");
        for p in parents {
            body.push_str(&format!("parent {p}\n"));
        }
        body.push_str(&format!(
            "author T <t@e.com> {time} +0000\ncommitter T <t@e.com> {time} +0000\n\n{msg}\n"
        ));
        odb.write_loose_materialize(ObjectKind::Commit, body.as_bytes())
    }

    fn open_repo(dir: &tempfile::TempDir) -> Result<Repository> {
        Repository::init(
            dir.path(),
            &InitOptions {
                bare: true,

                ref_storage: crate::RefStorageFormat::Files,
                ..InitOptions::default()
            },
        )?;
        Repository::open(dir.path(), &RepositoryOptions::empty())
    }

    fn git_log_oids(repo: &Repository, max: usize) -> Vec<ObjectId> {
        let out = Command::new("git")
            .args(["-C", repo.git_dir().to_str().expect("utf8"), "log", "-n"])
            .arg(max.to_string())
            .args(["--format=%H"])
            .output()
            .expect("git log");
        assert!(out.status.success());
        String::from_utf8_lossy(&out.stdout)
            .lines()
            .filter_map(|line| line.parse().ok())
            .collect()
    }

    #[cfg_attr(
        not(feature = "full-tests"),
        ignore = "starts system git or is heavy; run with --features full-tests"
    )]
    #[test]
    fn cursor_roundtrip_and_resume_matches_git_log() -> Result<()> {
        let dir = tempdir().map_err(Error::Io)?;
        let repo = open_repo(&dir)?;
        let base = write_commit(&repo.odb(), &[], "base", 100)?;
        let side = write_commit(&repo.odb(), &[base], "side", 150)?;
        let main1 = write_commit(&repo.odb(), &[base], "main1", 200)?;
        let merge = write_commit(&repo.odb(), &[main1, side], "merge", 300)?;
        let tip = write_commit(&repo.odb(), &[merge], "tip", 400)?;
        crate::refs::write_ref(repo.git_dir(), "refs/heads/main", &tip)?;
        crate::refs::write_symbolic_ref(repo.git_dir(), "HEAD", "refs/heads/main")?;

        const PAGE: usize = 3;
        let query = LogWalkQuery {
            tips: vec![tip],
            hide: Vec::new(),
            cutoffs: ReachableWalkCommitterCutoffs::default(),
        };
        let mut cursor: Option<LogWalkCursor> = None;
        let mut paged = Vec::new();
        loop {
            let page = LogWalkCursor::step(&repo, &query, cursor.as_ref(), PAGE)?;
            paged.extend(page.commits.clone());
            match page.next {
                Some(next) => {
                    let encoded = next.encode()?;
                    cursor = Some(LogWalkCursor::decode(&encoded)?);
                }
                None => break,
            }
        }

        let git_order = git_log_oids(&repo, 20);
        assert_eq!(paged, git_order);
        Ok(())
    }

    #[test]
    fn short_cursor_roundtrip_via_repo_store() -> Result<()> {
        let dir = tempdir().map_err(Error::Io)?;
        let repo = open_repo(&dir)?;
        let base = write_commit(&repo.odb(), &[], "base", 100)?;
        let tip = write_commit(&repo.odb(), &[base], "tip", 200)?;
        let query = LogWalkQuery {
            tips: vec![tip],
            hide: Vec::new(),
            cutoffs: ReachableWalkCommitterCutoffs::default(),
        };
        let page = LogWalkCursor::step(&repo, &query, None, 1)?;
        let next = page.next.expect("continuation");
        let short = next.encode_short(&repo)?;
        assert!(short.starts_with(SHORT_CURSOR_PREFIX));
        assert_eq!(short.len(), SHORT_CURSOR_PREFIX.len() + SHORT_CURSOR_ID_LEN);
        let decoded = LogWalkCursor::decode_with_git_dir(&short, Some(repo.git_dir()))?;
        assert_eq!(decoded, next);
        Ok(())
    }

    #[test]
    fn page_commit_data_holds_the_commits_the_walk_read() -> Result<()> {
        let dir = tempdir().map_err(Error::Io)?;
        let repo = open_repo(&dir)?;
        let base = write_commit(&repo.odb(), &[], "base", 100)?;
        let side = write_commit(&repo.odb(), &[base], "side", 150)?;
        let main1 = write_commit(&repo.odb(), &[base], "main1", 200)?;
        let merge = write_commit(&repo.odb(), &[main1, side], "merge", 300)?;
        let tip = write_commit(&repo.odb(), &[merge], "tip", 400)?;
        let query = LogWalkQuery {
            tips: vec![tip],
            hide: Vec::new(),
            cutoffs: ReachableWalkCommitterCutoffs::default(),
        };
        let page = LogWalkCursor::step(&repo, &query, None, 10)?;
        assert_eq!(page.commits.len(), 5);
        // Seeding the walk reads the tip linkage first, so only the tip may be missing.
        assert!(page.commit_data.len() >= page.commits.len() - 1);
        let retained: Vec<ObjectId> = page.commit_data.oids().copied().collect();
        let mut commit_data = page.commit_data.clone();
        for oid in retained {
            assert!(page.commits.contains(&oid));
            assert_eq!(
                commit_data.take_commit(&oid).transpose()?,
                Some(parse_commit(&repo.odb().read(&oid)?.data)?)
            );
        }

        let reopened = Repository::open(dir.path(), &RepositoryOptions::empty())?;
        let limited = LogWalkCursor::step(&reopened, &query, None, 2)?;
        assert!(limited
            .commit_data
            .oids()
            .all(|oid| limited.commits.contains(oid)));
        Ok(())
    }

    #[test]
    fn page_commit_data_skips_commits_behind_a_replace_ref() -> Result<()> {
        let dir = tempdir().map_err(Error::Io)?;
        let repo = open_repo(&dir)?;
        let base = write_commit(&repo.odb(), &[], "base", 100)?;
        let mid = write_commit(&repo.odb(), &[base], "mid", 200)?;
        let tip = write_commit(&repo.odb(), &[mid], "tip", 300)?;
        let replacement = write_commit(&repo.odb(), &[base], "replacement", 200)?;
        crate::refs::write_ref(
            repo.git_dir(),
            &format!("refs/replace/{}", mid.to_hex()),
            &replacement,
        )?;
        let repo = Repository::open(dir.path(), &RepositoryOptions::empty())?;
        let query = LogWalkQuery {
            tips: vec![tip],
            hide: Vec::new(),
            cutoffs: ReachableWalkCommitterCutoffs::default(),
        };
        let page = LogWalkCursor::step(&repo, &query, None, 10)?;
        assert!(page.commits.contains(&mid));
        assert!(!page.commit_data.contains(&mid));
        assert!(page.commit_data.contains(&base));
        Ok(())
    }

    #[cfg_attr(
        not(feature = "full-tests"),
        ignore = "starts system git or is heavy; run with --features full-tests"
    )]
    #[test]
    fn equal_committer_dates_match_git_log_order() -> Result<()> {
        let dir = tempdir().map_err(Error::Io)?;
        let repo = open_repo(&dir)?;
        let same = 1_735_689_600i64; // 2026-01-01T00:00:00Z
        let base = write_commit(&repo.odb(), &[], "base", same)?;
        let side = write_commit(&repo.odb(), &[base], "side", same)?;
        let mainedit = write_commit(&repo.odb(), &[base], "mainedit", same)?;
        let merge = write_commit(&repo.odb(), &[mainedit, side], "merge", same)?;
        crate::refs::write_ref(repo.git_dir(), "refs/heads/main", &merge)?;
        crate::refs::write_symbolic_ref(repo.git_dir(), "HEAD", "refs/heads/main")?;

        let git_order = git_log_oids(&repo, 10);
        let query = LogWalkQuery {
            tips: vec![merge],
            hide: Vec::new(),
            cutoffs: ReachableWalkCommitterCutoffs::default(),
        };
        let page = LogWalkCursor::step(&repo, &query, None, 10)?;
        assert_eq!(page.commits, git_order);
        assert_no_ancestor_before_descendant(&repo, &page.commits);
        Ok(())
    }

    fn assert_no_ancestor_before_descendant(repo: &Repository, order: &[ObjectId]) {
        for (i, &ancestor) in order.iter().enumerate() {
            for &maybe_descendant in &order[i + 1..] {
                assert!(
                    !crate::merge_base::is_ancestor(repo, ancestor, maybe_descendant)
                        .expect("ancestry"),
                    "ancestor {ancestor} listed before descendant {maybe_descendant}"
                );
            }
        }
    }

    #[test]
    fn since_cutoff_does_not_emit_stale_continuation_cursor() -> Result<()> {
        let dir = tempdir().map_err(Error::Io)?;
        let repo = open_repo(&dir)?;
        let c1 = write_commit(&repo.odb(), &[], "c1", 100)?;
        let c2 = write_commit(&repo.odb(), &[c1], "c2", 200)?;
        let c3 = write_commit(&repo.odb(), &[c2], "c3", 300)?;
        let tip = write_commit(&repo.odb(), &[c3], "tip", 400)?;
        crate::refs::write_ref(repo.git_dir(), "refs/heads/main", &tip)?;
        crate::refs::write_symbolic_ref(repo.git_dir(), "HEAD", "refs/heads/main")?;

        let query = LogWalkQuery {
            tips: vec![tip],
            hide: Vec::new(),
            cutoffs: ReachableWalkCommitterCutoffs {
                since: Some(250),
                until: None,
            },
        };
        let page = LogWalkCursor::step(&repo, &query, None, 10)?;
        assert_eq!(page.commits, vec![tip, c3]);
        assert!(
            page.next.is_none(),
            "since cutoff must not leave a continuation cursor with pruned heap state"
        );
        Ok(())
    }

    #[test]
    fn since_cutoff_at_limit_does_not_false_continue() -> Result<()> {
        let dir = tempdir().map_err(Error::Io)?;
        let repo = open_repo(&dir)?;
        let c1 = write_commit(&repo.odb(), &[], "c1", 100)?;
        let c2 = write_commit(&repo.odb(), &[c1], "c2", 200)?;
        let c3 = write_commit(&repo.odb(), &[c2], "c3", 300)?;
        let tip = write_commit(&repo.odb(), &[c3], "tip", 400)?;
        crate::refs::write_ref(repo.git_dir(), "refs/heads/main", &tip)?;
        crate::refs::write_symbolic_ref(repo.git_dir(), "HEAD", "refs/heads/main")?;

        let query = LogWalkQuery {
            tips: vec![tip],
            hide: Vec::new(),
            cutoffs: ReachableWalkCommitterCutoffs {
                since: Some(250),
                until: None,
            },
        };
        let page = LogWalkCursor::step(&repo, &query, None, 2)?;
        assert_eq!(page.commits, vec![tip, c3]);
        assert!(
            page.next.is_none(),
            "limit matching since-filtered count must not imply more pages"
        );
        Ok(())
    }

    #[test]
    fn decodes_v3_cursor_with_filled_reserved_list() -> Result<()> {
        let tip: ObjectId = "1111111111111111111111111111111111111111"
            .parse()
            .map_err(|_| Error::CorruptObject("oid".to_owned()))?;
        let parent: ObjectId = "2222222222222222222222222222222222222222"
            .parse()
            .map_err(|_| Error::CorruptObject("oid".to_owned()))?;
        let mut buf = Vec::new();
        buf.extend_from_slice(MAGIC_V3);
        write_oid_vec(&mut buf, &[tip])?;
        write_oid_vec(&mut buf, &[])?;
        write_i64(&mut buf, NO_CUTOFF)?;
        write_i64(&mut buf, NO_CUTOFF)?;
        write_u64(&mut buf, 2)?;
        write_oid_vec(&mut buf, &[])?;
        write_oid_vec(&mut buf, &[tip])?;
        write_oid_vec(&mut buf, &[tip, parent])?;
        write_u32(&mut buf, 1)?;
        buf.extend_from_slice(parent.as_bytes());
        write_u32(&mut buf, 0)?;
        write_oid_vec(&mut buf, &[tip])?;
        write_u32(&mut buf, 1)?;
        write_i64(&mut buf, 100)?;
        write_u64(&mut buf, 1)?;
        buf.extend_from_slice(parent.as_bytes());

        let token = format!("{CURSOR_PREFIX}{}", base64_encode(&buf));
        let cursor = LogWalkCursor::decode(&token)?;
        assert_eq!(cursor.query().tips, vec![tip]);
        assert_eq!(cursor.walk.heap, vec![(100, 1, parent)]);
        assert_eq!(cursor.walk.queued, vec![tip, parent]);

        let reencoded = LogWalkCursor::decode(&cursor.encode()?)?;
        assert_eq!(reencoded, cursor);
        Ok(())
    }

    #[cfg_attr(
        not(feature = "full-tests"),
        ignore = "starts system git or is heavy; run with --features full-tests"
    )]
    #[test]
    fn range_pages_match_git_log_under_clock_skew() -> Result<()> {
        let dir = tempdir().map_err(Error::Io)?;
        let repo = open_repo(&dir)?;
        let base = write_commit(&repo.odb(), &[], "base", 50)?;
        let shared = write_commit(&repo.odb(), &[base], "shared", 120)?;
        let newer = write_commit(&repo.odb(), &[shared], "newer", 350)?;
        // Older than its parent: Git prints `shared` before this child.
        let skewed = write_commit(&repo.odb(), &[shared], "skewed", 100)?;
        let merge = write_commit(&repo.odb(), &[newer, skewed], "merge", 400)?;
        let tip = write_commit(&repo.odb(), &[merge], "tip", 500)?;
        crate::refs::write_ref(repo.git_dir(), "refs/heads/main", &tip)?;
        crate::refs::write_symbolic_ref(repo.git_dir(), "HEAD", "refs/heads/main")?;

        let out = Command::new("git")
            .args(["-C", repo.git_dir().to_str().expect("utf8"), "rev-list"])
            .arg(tip.to_hex())
            .arg(format!("^{base}"))
            .output()
            .expect("git rev-list");
        assert!(out.status.success());
        let git_order: Vec<ObjectId> = String::from_utf8_lossy(&out.stdout)
            .lines()
            .filter_map(|line| line.parse().ok())
            .collect();
        assert_eq!(git_order, vec![tip, merge, newer, shared, skewed]);

        let query = LogWalkQuery {
            tips: vec![tip],
            hide: vec![base],
            cutoffs: ReachableWalkCommitterCutoffs::default(),
        };
        let mut cursor: Option<LogWalkCursor> = None;
        let mut paged = Vec::new();
        loop {
            let page = LogWalkCursor::step(&repo, &query, cursor.as_ref(), 2)?;
            paged.extend(page.commits);
            match page.next {
                Some(next) => cursor = Some(LogWalkCursor::decode(&next.encode()?)?),
                None => break,
            }
        }
        assert_eq!(paged, git_order);
        Ok(())
    }

    fn assert_commit_data_matches_odb(repo: &Repository, page: &LogWalkPage) -> Result<()> {
        let emitted: HashSet<ObjectId> = page.commits.iter().copied().collect();
        let mut walked = page.commit_data.clone();
        let held: Vec<ObjectId> = walked.oids().copied().collect();
        assert_eq!(held.len(), walked.len());
        for oid in held {
            assert!(emitted.contains(&oid), "{oid} retained but not emitted");
            assert!(walked.contains(&oid));
            let data = walked.take_commit(&oid).expect("held commit")?;
            let fresh = crate::objects::parse_commit(&repo.odb().read(&oid)?.data)?;
            assert_eq!(
                data, fresh,
                "retained data for {oid} differs from the object"
            );
            assert!(!walked.contains(&oid));
            assert!(walked.take_commit(&oid).is_none(), "taken twice");
        }
        assert!(walked.is_empty());
        Ok(())
    }

    #[cfg_attr(
        not(feature = "full-tests"),
        ignore = "starts system git or is heavy; run with --features full-tests"
    )]
    #[test]
    fn pages_carry_parsed_commits_for_every_emitted_commit_read() -> Result<()> {
        let dir = tempdir().map_err(Error::Io)?;
        let repo = open_repo(&dir)?;
        let base = write_commit(&repo.odb(), &[], "base", 100)?;
        let hidden_side = write_commit(&repo.odb(), &[base], "hidden side", 150)?;
        let side = write_commit(&repo.odb(), &[base], "side\n\nbody line\n", 160)?;
        let main = write_commit(&repo.odb(), &[base], "main", 200)?;
        let merge = write_commit(&repo.odb(), &[main, side], "merge", 300)?;
        let tip = write_commit(&repo.odb(), &[merge], "tip", 400)?;
        crate::refs::write_ref(repo.git_dir(), "refs/heads/main", &tip)?;
        crate::refs::write_symbolic_ref(repo.git_dir(), "HEAD", "refs/heads/main")?;

        let query = LogWalkQuery {
            tips: vec![tip],
            hide: Vec::new(),
            cutoffs: ReachableWalkCommitterCutoffs::default(),
        };
        let first = LogWalkCursor::step(&repo, &query, None, 3)?;
        assert_eq!(first.commits.len(), 3);
        assert_eq!(first.commit_data.len(), 3, "loose commits are all read");
        assert_commit_data_matches_odb(&repo, &first)?;
        let second = LogWalkCursor::step(&repo, &query, first.next.as_ref(), 10)?;
        assert_commit_data_matches_odb(&repo, &second)?;
        let mut all = first.commits.clone();
        all.extend(second.commits.iter().copied());
        assert_eq!(all, git_log_oids(&repo, 20));

        // Commits read only to paint the hidden side are not handed back.
        let reopened = Repository::open(dir.path(), &RepositoryOptions::empty())?;
        let range = LogWalkQuery {
            tips: vec![tip],
            hide: vec![hidden_side],
            cutoffs: ReachableWalkCommitterCutoffs::default(),
        };
        let page = LogWalkCursor::step(&reopened, &range, None, 10)?;
        assert_eq!(page.commits, vec![tip, merge, main, side]);
        assert!(!page.commit_data.contains(&hidden_side));
        assert!(!page.commit_data.contains(&base));
        assert_commit_data_matches_odb(&reopened, &page)?;
        Ok(())
    }

    #[cfg_attr(
        not(feature = "full-tests"),
        ignore = "starts system git or is heavy; run with --features full-tests"
    )]
    #[test]
    fn commit_graph_walks_match_git_log_without_object_reads() -> Result<()> {
        let dir = tempdir().map_err(Error::Io)?;
        let repo = open_repo(&dir)?;
        let mut parent = write_commit(&repo.odb(), &[], "c0", 100)?;
        for i in 1..8 {
            parent = write_commit(&repo.odb(), &[parent], &format!("c{i}"), 100 + i * 10)?;
        }
        crate::refs::write_ref(repo.git_dir(), "refs/heads/main", &parent)?;
        crate::refs::write_symbolic_ref(repo.git_dir(), "HEAD", "refs/heads/main")?;
        let status = Command::new("git")
            .args(["-C", repo.git_dir().to_str().expect("utf8")])
            .args(["commit-graph", "write", "--reachable"])
            .status()
            .expect("git commit-graph write");
        assert!(status.success());

        let repo = Repository::open(dir.path(), &RepositoryOptions::empty())?;
        let query = LogWalkQuery {
            tips: vec![parent],
            hide: Vec::new(),
            cutoffs: ReachableWalkCommitterCutoffs::default(),
        };
        let page = LogWalkCursor::step(&repo, &query, None, 20)?;
        assert_eq!(page.commits, git_log_oids(&repo, 20));
        assert_commit_data_matches_odb(&repo, &page)?;
        Ok(())
    }

    #[test]
    fn paging_includes_second_parent_after_merge_boundary() -> Result<()> {
        let dir = tempdir().map_err(Error::Io)?;
        let repo = open_repo(&dir)?;
        let base = write_commit(&repo.odb(), &[], "base", 100)?;
        let side = write_commit(&repo.odb(), &[base], "side-only", 120)?;
        let main = write_commit(&repo.odb(), &[base], "main", 200)?;
        let merge = write_commit(&repo.odb(), &[main, side], "merge", 250)?;
        crate::refs::write_ref(repo.git_dir(), "refs/heads/main", &merge)?;
        crate::refs::write_symbolic_ref(repo.git_dir(), "HEAD", "refs/heads/main")?;

        let query = LogWalkQuery {
            tips: vec![merge],
            hide: Vec::new(),
            cutoffs: ReachableWalkCommitterCutoffs::default(),
        };
        let page1 = LogWalkCursor::step(&repo, &query, None, 1)?;
        assert_eq!(page1.commits, vec![merge]);
        assert!(page1.next.is_some());

        let page2 = LogWalkCursor::step(&repo, &query, page1.next.as_ref(), 10)?;
        let seen: HashSet<_> = page1
            .commits
            .iter()
            .chain(page2.commits.iter())
            .copied()
            .collect();
        assert!(
            seen.contains(&side),
            "second parent must appear after merge page"
        );
        Ok(())
    }
}

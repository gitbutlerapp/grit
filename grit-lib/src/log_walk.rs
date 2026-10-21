//! Resumable HEAD log walks for bounded `grit log` paging.
//!
//! [`LogWalkCursor`] encodes the heap and visited sets of the date-order reachable walk
//! so `--cursor` continues the same traversal instead of restarting from a single commit.

use std::io::{Cursor, Read, Write};

use crate::error::{Error, Result};
use crate::merge_base::{ReachableWalkCommitterCutoffs, ReachableWalkSnapshot, ReachableWalkState};
use crate::objects::ObjectId;
use crate::repo::Repository;
use crate::revwalk::queries::{resolve_log_walk_tips_and_hide, MergeFilter, RevWalkListOptions};

const CURSOR_PREFIX: &str = "lw1.";
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
        if !walk.paths.is_empty()
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
    /// `commits` field.
    pub commits: Vec<ObjectId>,
    /// `next` field.
    pub next: Option<LogWalkCursor>,
}

impl LogWalkCursor {
    /// Revision tips, exclusions, and date cutoffs bound to this continuation token.
    #[must_use]
    pub fn query(&self) -> &LogWalkQuery {
        &self.query
    }

    /// Query embedded in this cursor (same as [`Self::query`]).
    #[must_use]
    pub fn resume_query(&self) -> &LogWalkQuery {
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
            });
        }

        let mut state = match cursor {
            None => ReachableWalkState::new(repo, &query.tips, &query.hide)?,
            Some(c) => {
                if c.query != *query {
                    return Err(Error::CorruptObject(
                        "log walk cursor does not match the current revision selection".to_owned(),
                    ));
                }
                ReachableWalkState::from_snapshot(c.walk.clone())
            }
        };

        let commits = state.emit_until(repo, Some(limit), query.cutoffs)?;
        let next = state.has_pending().then(|| LogWalkCursor {
            query: query.clone(),
            walk: state.snapshot(),
        });
        Ok(LogWalkPage { commits, next })
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

    /// Decode a `--cursor` token produced by [`Self::encode`].
    ///
    /// # Errors
    ///
    /// Returns [`Error::CorruptObject`] when the token is malformed.
    pub fn decode(token: &str) -> Result<Self> {
        let payload = token
            .strip_prefix(CURSOR_PREFIX)
            .ok_or_else(|| Error::CorruptObject("unrecognized log walk cursor".to_owned()))?;
        let bytes = base64_decode(payload.as_bytes())
            .map_err(|_| Error::CorruptObject("log walk cursor base64 decode failed".to_owned()))?;
        read_snapshot(&bytes)
    }
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
    write_u32(out, walk.unfinished_children.len() as u32)?;
    for (oid, count) in &walk.unfinished_children {
        out.write_all(oid.as_bytes()).map_err(Error::Io)?;
        write_u32(out, *count)?;
    }
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
    let unfinished_len = read_u32(cur)? as usize;
    let mut unfinished_children = Vec::with_capacity(unfinished_len);
    for _ in 0..unfinished_len {
        let oid = read_oid(cur)?;
        let count = read_u32(cur)?;
        unfinished_children.push((oid, count));
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
        unfinished_children,
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
                initial_branch: "main".to_owned(),
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

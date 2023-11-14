//! Reflog listing, deletion, and expiry for one [`Repository`](crate::repo::Repository).

use std::fs;
use std::io;
use std::ops::ControlFlow;
use std::path::Path;

use crate::error::{Error, Result};
use crate::reflog::{
    delete_reflog_entries, delete_reflog_entries_rechain, expire_reflog_git, list_reflog_refs,
    load_gc_reflog_expire_config, parse_reflog_expire_spec, parse_reflog_line_for_store,
    reflog_exists, reflog_identity_timestamp, reflog_path, MessageMatcher, ReflogEntry,
    ReflogExpireParams, ReflogExpireResult,
};
use crate::refs::resolve_ref_dwim;
use crate::refs::store::RefName;
use crate::repo::Repository;

/// Options for [`RepositoryReflogs::expire`].
#[derive(Debug, Clone)]
pub struct ExpireOptions {
    /// Current time as Unix seconds (never read from the system clock inside the library).
    pub now: i64,
    /// Explicit reachable expiry (`--expire`); `None` uses merged `gc.reflogExpire` config.
    pub expire: Option<String>,
    /// Explicit unreachable expiry (`--expire-unreachable`); `None` uses gc config.
    pub expire_unreachable: Option<String>,
    /// When true, process every ref under `refs/` that exists (Git `--all`); otherwise only refs with reflog files.
    pub all: bool,
}

/// Summary returned by [`RepositoryReflogs::expire`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReflogExpireOutcome {
    /// Total reflog entries removed across all processed refs.
    pub entries_pruned: usize,
}

/// One reflog line with its newest-first index (`0` is the latest entry).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IndexedReflogEntry {
    /// Index in newest-first order (`ref@{{index}}`).
    pub index: usize,
    /// Parsed reflog fields.
    pub entry: ReflogEntry,
}

/// Filters for [`RepositoryReflogs::query`].
#[derive(Debug, Clone, Default)]
pub struct ReflogQuery {
    /// Maximum matching entries to return (newest first).
    pub limit: Option<usize>,
    /// Exclude entries with identity timestamp strictly before this Unix second.
    pub since: Option<i64>,
    /// Exclude entries with identity timestamp strictly after this Unix second.
    pub until: Option<i64>,
    /// Optional message substring filter.
    pub grep: Option<MessageMatcher>,
    /// Wall-clock anchor passed by the caller (not read from the system clock here).
    pub now: i64,
}

/// One page of reflog query results.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReflogPage {
    /// Matching entries in newest-first order with global reflog indices.
    pub entries: Vec<IndexedReflogEntry>,
    /// `true` when scanning stopped before the oldest entry (typically because `limit` was reached).
    pub truncated: bool,
}

/// Reflog read/write helpers scoped to an open repository.
#[derive(Debug, Copy, Clone)]
pub struct RepositoryReflogs<'repo> {
    repo: &'repo Repository,
}

impl<'repo> RepositoryReflogs<'repo> {
    pub(crate) fn new(repo: &'repo Repository) -> Self {
        Self { repo }
    }

    /// Borrow the underlying repository.
    #[must_use]
    pub fn repository(&self) -> &'repo Repository {
        self.repo
    }

    /// List refs that currently have a reflog file or reftable log.
    ///
    /// # Errors
    ///
    /// Propagates ref-store and I/O errors while scanning logs.
    pub fn refs(&self) -> Result<Vec<RefName>> {
        let git_dir = self.repo.git_dir();
        let mut out = Vec::new();
        for name in list_reflog_refs(git_dir)? {
            if name == "HEAD" {
                out.push(
                    RefName::try_from("HEAD")
                        .map_err(|e| Error::Message(format!("invalid HEAD ref name: {e}")))?,
                );
                continue;
            }
            if let Ok(parsed) = RefName::try_from(name.as_str()) {
                out.push(parsed);
            }
        }
        out.sort_by(|a, b| a.as_str().cmp(b.as_str()));
        Ok(out)
    }

    /// Reflog entries for `ref_name`, newest first with indices starting at `0`.
    ///
    /// Returns an empty vector when the ref exists but its reflog is empty.
    ///
    /// # Errors
    ///
    /// Returns [`Error::ReflogUnknownRef`] when the ref cannot be resolved and has no reflog.
    pub fn entries(&self, ref_name: &RefName) -> Result<Vec<IndexedReflogEntry>> {
        let page = self.query(ref_name, &ReflogQuery::default())?;
        Ok(page.entries)
    }

    /// Reflog entries for `ref_name` matching `query`, newest first.
    ///
    /// Indices match Git's `ref@{{n}}` numbering on the full reflog (not renumbered after
    /// filtering). Scanning stops once `limit` matches are collected.
    ///
    /// # Errors
    ///
    /// Returns [`Error::ReflogUnknownRef`] when the ref cannot be resolved and has no reflog.
    pub fn query(&self, ref_name: &RefName, query: &ReflogQuery) -> Result<ReflogPage> {
        let canonical = self.resolve_reflog_canonical(ref_name.as_str())?;
        if query.limit == Some(0) {
            return Ok(ReflogPage {
                entries: Vec::new(),
                truncated: false,
            });
        }
        let git_dir = self.repo.git_dir();
        let mut state = QueryScanState {
            query,
            entries: Vec::new(),
            stopped_at_limit: false,
            non_empty_seen_from_newest: 0,
            total_non_empty: None,
        };
        let storage_format = crate::refs::store::open_ref_store(git_dir)?.format();
        if storage_format == crate::refs::store::RefStorageFormat::Files {
            scan_loose_reflog_newest_first(git_dir, &canonical, &mut state)?;
        } else {
            scan_store_reflog_newest_first(self.repo, &canonical, &mut state)?;
        }
        let truncated = state.truncated();
        Ok(ReflogPage {
            entries: state.entries,
            truncated,
        })
    }

    fn resolve_reflog_canonical(&self, user_ref: &str) -> Result<String> {
        let git_dir = self.repo.git_dir();
        for candidate in dwim_reflog_candidates(user_ref) {
            if crate::reflog::reflog_entry_count(git_dir, &candidate)? > 0 {
                return Ok(candidate);
            }
        }
        if !reflog_ref_known(git_dir, user_ref)? {
            return Err(Error::ReflogUnknownRef {
                refname: user_ref.to_owned(),
            });
        }
        canonical_reflog_ref(git_dir, user_ref)
    }

    /// Delete reflog entries by newest-first `indices`.
    ///
    /// When `rewrite` is true, surviving entries are re-chained (Git `EXPIRE_REFLOGS_REWRITE`).
    ///
    /// # Errors
    ///
    /// Returns [`Error::ReflogIndexOutOfRange`] when any index is out of range, or
    /// [`Error::ReflogUnknownRef`] when the ref does not exist.
    pub fn delete(&self, ref_name: &RefName, indices: &[usize], rewrite: bool) -> Result<()> {
        if indices.is_empty() {
            return Ok(());
        }
        let (canonical, entries) = self.load_reflog(ref_name.as_str())?;
        let len = entries.len();
        for &idx in indices {
            if idx >= len {
                return Err(Error::ReflogIndexOutOfRange {
                    refname: canonical,
                    index: idx,
                });
            }
        }
        let git_dir = self.repo.git_dir();
        if rewrite {
            delete_reflog_entries_rechain(git_dir, &canonical, indices)?;
        } else {
            delete_reflog_entries(git_dir, &canonical, indices)?;
        }
        Ok(())
    }

    /// Expire reflog entries using Git-compatible `gc.reflogExpire*` rules.
    ///
    /// # Errors
    ///
    /// Propagates config load failures, invalid expiry strings, and reflog rewrite errors.
    pub fn expire(&self, options: &ExpireOptions) -> Result<ReflogExpireOutcome> {
        let cfg = self.repo.config()?;
        let gc = load_gc_reflog_expire_config(&cfg, options.now);
        let explicit_total = options
            .expire
            .as_deref()
            .map(|s| parse_reflog_expire_spec(s, options.now))
            .transpose()?;
        let explicit_unreachable = options
            .expire_unreachable
            .as_deref()
            .map(|s| parse_reflog_expire_spec(s, options.now))
            .transpose()?;
        let params = ReflogExpireParams {
            stale_fix: false,
            dry_run: false,
            verbose: false,
        };
        let git_dir = self.repo.git_dir();
        let refnames = expire_target_refs(git_dir, options.all)?;
        let mut total = 0usize;
        for refname in refnames {
            let ReflogExpireResult { pruned, .. } = expire_reflog_git(
                self.repo,
                git_dir,
                &refname,
                &params,
                explicit_total,
                explicit_unreachable,
                &gc.patterns,
                gc.global_total,
                gc.global_unreachable,
                options.now,
            )?;
            total += pruned;
        }
        Ok(ReflogExpireOutcome {
            entries_pruned: total,
        })
    }

    fn load_reflog(&self, user_ref: &str) -> Result<(String, Vec<ReflogEntry>)> {
        let git_dir = self.repo.git_dir();
        for candidate in dwim_reflog_candidates(user_ref) {
            let entries = read_reflog_via_store(self.repo, &candidate)?;
            if !entries.is_empty() {
                return Ok((candidate, entries));
            }
        }
        if !reflog_ref_known(git_dir, user_ref)? {
            return Err(Error::ReflogUnknownRef {
                refname: user_ref.to_owned(),
            });
        }
        let canonical = canonical_reflog_ref(git_dir, user_ref)?;
        Ok((canonical, Vec::new()))
    }
}

fn read_reflog_via_store(repo: &Repository, refname: &str) -> Result<Vec<ReflogEntry>> {
    let mut entries = Vec::new();
    repo.refs()
        .for_each_reflog_entry(refname, false, &mut |entry| {
            entries.push(entry.clone());
            ControlFlow::Continue(())
        })?;
    Ok(entries)
}

struct QueryScanState<'a> {
    query: &'a ReflogQuery,
    entries: Vec<IndexedReflogEntry>,
    stopped_at_limit: bool,
    non_empty_seen_from_newest: usize,
    total_non_empty: Option<usize>,
}

impl QueryScanState<'_> {
    fn truncated(&self) -> bool {
        if !self.stopped_at_limit {
            return false;
        }
        let Some(total) = self.total_non_empty else {
            return true;
        };
        self.non_empty_seen_from_newest < total
    }

    fn visit(&mut self, index: usize, entry: &ReflogEntry) -> ControlFlow<()> {
        if !entry_matches_query(entry, self.query) {
            return ControlFlow::Continue(());
        }
        if self.query.limit == Some(0) {
            self.stopped_at_limit = true;
            return ControlFlow::Break(());
        }
        self.entries.push(IndexedReflogEntry {
            index,
            entry: entry.clone(),
        });
        if self
            .query
            .limit
            .is_some_and(|limit| limit > 0 && self.entries.len() >= limit)
        {
            self.stopped_at_limit = true;
            return ControlFlow::Break(());
        }
        ControlFlow::Continue(())
    }
}

fn entry_matches_query(entry: &ReflogEntry, query: &ReflogQuery) -> bool {
    if let Some(since) = query.since {
        if reflog_identity_timestamp(&entry.identity).is_some_and(|ts| ts < since) {
            return false;
        }
    }
    if let Some(until) = query.until {
        if reflog_identity_timestamp(&entry.identity).is_some_and(|ts| ts > until) {
            return false;
        }
    }
    if let Some(grep) = &query.grep {
        if !grep.matches(&entry.message) {
            return false;
        }
    }
    let _ = query.now;
    true
}

fn scan_loose_reflog_newest_first(
    git_dir: &Path,
    refname: &str,
    state: &mut QueryScanState<'_>,
) -> Result<()> {
    let path = reflog_path(git_dir, refname);
    let content = match fs::read(&path) {
        Ok(c) => c,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(Error::Io(e)),
    };
    scan_reflog_bytes_newest_first(&content, state);
    Ok(())
}

fn scan_store_reflog_newest_first(
    repo: &Repository,
    refname: &str,
    state: &mut QueryScanState<'_>,
) -> Result<()> {
    let mut index = 0usize;
    repo.refs()
        .for_each_reflog_entry(refname, true, &mut |entry| {
            state.non_empty_seen_from_newest += 1;
            let flow = state.visit(index, entry);
            index += 1;
            flow
        })?;
    state.total_non_empty = Some(index);
    Ok(())
}

/// Walk non-empty reflog lines newest-first without building a line index vector.
fn scan_reflog_bytes_newest_first(content: &[u8], state: &mut QueryScanState<'_>) {
    let mut index_from_newest = 0usize;
    let mut pos = content.len();
    while pos > 0 {
        let start = match content[..pos].iter().rposition(|&b| b == b'\n') {
            Some(at) => at + 1,
            None => 0,
        };
        let line = std::str::from_utf8(&content[start..pos])
            .unwrap_or("")
            .trim_end_matches('\r');
        pos = start.saturating_sub(1);
        if line.is_empty() {
            continue;
        }
        state.non_empty_seen_from_newest += 1;
        let index = index_from_newest;
        index_from_newest += 1;
        let Some(entry) = parse_reflog_line_for_store(line) else {
            continue;
        };
        if state.visit(index, &entry).is_break() {
            break;
        }
    }
    if state.stopped_at_limit {
        state.total_non_empty = Some(count_nonempty_reflog_lines(content));
    }
}

fn count_nonempty_reflog_lines(content: &[u8]) -> usize {
    content
        .split(|&b| b == b'\n')
        .filter(|line| !line.is_empty() && *line != b"\r")
        .count()
}

fn dwim_reflog_candidates(user_ref: &str) -> Vec<String> {
    let mut out = vec![user_ref.to_owned()];
    if user_ref != "HEAD" && !user_ref.starts_with("refs/") {
        out.push(format!("refs/{user_ref}"));
        out.push(format!("refs/heads/{user_ref}"));
    }
    out
}

fn canonical_reflog_ref(git_dir: &Path, user_ref: &str) -> Result<String> {
    for candidate in dwim_reflog_candidates(user_ref) {
        if reflog_exists(git_dir, &candidate) {
            return Ok(candidate);
        }
    }
    if reflog_ref_known(git_dir, user_ref)? {
        if user_ref.starts_with("refs/") || user_ref == "HEAD" {
            return Ok(user_ref.to_owned());
        }
        return Ok(format!("refs/heads/{user_ref}"));
    }
    Err(Error::ReflogUnknownRef {
        refname: user_ref.to_owned(),
    })
}

fn reflog_ref_known(git_dir: &Path, user_ref: &str) -> Result<bool> {
    if user_ref == "HEAD" {
        return Ok(true);
    }
    for candidate in dwim_reflog_candidates(user_ref) {
        if reflog_exists(git_dir, &candidate) {
            return Ok(true);
        }
        let (_, oid) = resolve_ref_dwim(git_dir, &candidate);
        if oid.is_some() {
            return Ok(true);
        }
    }
    let (_, oid) = resolve_ref_dwim(git_dir, user_ref);
    Ok(oid.is_some())
}

fn expire_target_refs(git_dir: &Path, all: bool) -> Result<Vec<String>> {
    if !all {
        return list_reflog_refs(git_dir);
    }
    let mut names: Vec<String> = crate::refs::list_refs(git_dir, "refs/")?
        .into_iter()
        .map(|(name, _)| name)
        .collect();
    if reflog_exists(git_dir, "HEAD") {
        names.push("HEAD".to_owned());
    }
    names.sort();
    names.dedup();
    Ok(names)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::refs::append_reflog;
    use crate::repo::{InitOptions, Repository};

    fn init_repo() -> (tempfile::TempDir, Repository) {
        let dir = tempfile::tempdir().expect("tempdir");
        let repo = Repository::init(dir.path(), &InitOptions::default()).expect("init");
        (dir, repo)
    }

    #[test]
    fn query_limit_zero_returns_empty_without_reading_entries() {
        let (_dir, repo) = init_repo();
        let git_dir = repo.git_dir().to_path_buf();
        let refname = "refs/heads/main";
        append_reflog(
            &git_dir,
            refname,
            &crate::objects::ObjectId::zero(),
            &crate::objects::ObjectId::from_hex(
                "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
            )
            .expect("oid"),
            "T <t@example.com> 1000000000 +0000",
            "commit: one",
            true,
        )
        .expect("append");
        let name = RefName::try_from(refname).expect("valid");
        let page = repo
            .reflogs()
            .query(
                &name,
                &ReflogQuery {
                    limit: Some(0),
                    ..ReflogQuery::default()
                },
            )
            .expect("query");
        assert!(page.entries.is_empty());
        assert!(!page.truncated);
    }

    #[test]
    fn loose_scan_index_skips_unparseable_lines_without_renumbering() {
        let (_dir, repo) = init_repo();
        let git_dir = repo.git_dir().to_path_buf();
        let refname = "refs/heads/x";
        let oid = crate::objects::ObjectId::from_hex("bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb")
            .expect("oid");
        let body = format!(
            "not-a-valid-reflog-line\n0000000000000000000000000000000000000000 {oid} T <t@e.com> 2000 +0000\tcommit: newest\n"
        );
        // Newest line is valid (index 0); the unparseable line is older and does not consume an index.
        let path = crate::reflog::reflog_path(&git_dir, refname);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).expect("mkdir");
        }
        fs::write(&path, body).expect("write");
        let name = RefName::try_from(refname).expect("valid");
        let page = repo
            .reflogs()
            .query(&name, &ReflogQuery::default())
            .expect("query");
        assert_eq!(page.entries.len(), 1);
        assert_eq!(page.entries[0].index, 0);
    }

    #[test]
    fn delete_out_of_range_returns_typed_error() {
        let (_dir, repo) = init_repo();
        let git_dir = repo.git_dir().to_path_buf();
        let refname = "refs/heads/main";
        append_reflog(
            &git_dir,
            refname,
            &crate::objects::ObjectId::zero(),
            &crate::objects::ObjectId::zero(),
            "T <t@example.com> 1000000000 +0000",
            "only",
            true,
        )
        .expect("append");
        let name = RefName::try_from(refname).expect("valid");
        let err = repo
            .reflogs()
            .delete(&name, &[1], false)
            .expect_err("out of range");
        match err {
            Error::ReflogIndexOutOfRange { refname, index } => {
                assert_eq!(refname, "refs/heads/main");
                assert_eq!(index, 1);
            }
            other => panic!("expected ReflogIndexOutOfRange, got {other:?}"),
        }
    }
}

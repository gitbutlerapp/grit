//! Reflog listing, deletion, and expiry for one [`Repository`](crate::repo::Repository).

use std::ops::ControlFlow;

use crate::error::{Error, Result};
use crate::reflog::{
    delete_reflog_entries_on_store, delete_reflog_entries_rechain_on_store,
    expire_reflog_git_with_cache, load_gc_reflog_expire_config, parse_reflog_expire_spec,
    read_reflog_on_store, reflog_identity_timestamp, reflog_refs_on_store, MessageMatcher,
    ReflogEntry, ReflogExpireParams, ReflogExpireResult, ReflogExpiryCache,
};
use crate::refs::store::{RefName, RefNameRef, RefStore};
use crate::repo::Repository;
use crate::worktree_ref::resolve_ref_dwim;

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
    /// When true, report what would be pruned without rewriting reflog files.
    pub dry_run: bool,
}

/// Per-ref prune count from [`RepositoryReflogs::expire`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RefExpireResult {
    /// Full ref name (for example `refs/heads/main` or `HEAD`).
    pub ref_name: String,
    /// Entries removed (or that would be removed when [`ExpireOptions::dry_run`] is set).
    pub pruned: usize,
}

/// Summary returned by [`RepositoryReflogs::expire`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReflogExpireOutcome {
    /// Total reflog entries removed across all processed refs.
    pub entries_pruned: usize,
    /// Mirrors [`ExpireOptions::dry_run`].
    pub dry_run: bool,
    /// One row per processed ref, in sorted ref-name order.
    pub refs: Vec<RefExpireResult>,
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
///
/// Every operation reads and rewrites reflogs through the repository's ref store
/// ([`Repository::ref_store`]), so a repository opened with an injected
/// [`RefStore`](crate::refs::store::RefStore) lists, deletes and expires the reflog that
/// store holds, exactly like the files and reftable backends.
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

    /// List refs that currently have a reflog in the repository's ref store, sorted by name.
    ///
    /// # Errors
    ///
    /// Propagates ref-store and I/O errors while scanning logs.
    pub fn list_reflog_refs(&self) -> Result<Vec<RefName>> {
        Ok(reflog_refs_on_store(self.store())?
            .iter()
            .map(RefName::from_trusted)
            .collect())
    }

    /// Legacy alias for [`Self::list_reflog_refs`].
    #[deprecated(note = "use `list_reflog_refs`")]
    pub fn refs(&self) -> Result<Vec<RefName>> {
        self.list_reflog_refs()
    }

    fn store(&self) -> &'repo dyn RefStore {
        self.repo.ref_store()
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
        let mut state = QueryScanState {
            query,
            entries: Vec::new(),
            limit_reached: false,
            truncated: false,
        };
        let mut index = 0usize;
        self.store().for_each_reflog_entry_str(
            RefNameRef::from_trusted(&canonical),
            true,
            &mut |entry| {
                let flow = state.visit(index, entry);
                index += 1;
                flow
            },
        )?;
        Ok(ReflogPage {
            entries: state.entries,
            truncated: state.truncated,
        })
    }

    fn resolve_reflog_canonical(&self, user_ref: &str) -> Result<String> {
        let store = self.store();
        for candidate in dwim_reflog_candidates(user_ref) {
            if reflog_has_entries(store, &candidate)? {
                return Ok(candidate);
            }
        }
        canonical_reflog_ref(store, user_ref)
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
        if rewrite {
            delete_reflog_entries_rechain_on_store(self.store(), &canonical, indices)
        } else {
            delete_reflog_entries_on_store(self.store(), &canonical, indices)
        }
    }

    /// Expire reflog entries using Git-compatible `gc.reflogExpire*` rules.
    ///
    /// # Errors
    ///
    /// Propagates config load failures, invalid expiry strings, and reflog rewrite errors.
    pub fn expire(&self, options: &ExpireOptions) -> Result<ReflogExpireOutcome> {
        let cfg = self.repo.config().snapshot()?;
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
            dry_run: options.dry_run,
            verbose: false,
        };
        let refnames = expire_target_refs(self.store(), options.all)?;
        let mut total = 0usize;
        let mut refs = Vec::with_capacity(refnames.len());
        let cache = ReflogExpiryCache::default();
        for refname in refnames {
            let ReflogExpireResult { pruned, .. } = expire_reflog_git_with_cache(
                self.repo,
                &refname,
                &params,
                explicit_total,
                explicit_unreachable,
                &gc.patterns,
                gc.global_total,
                gc.global_unreachable,
                options.now,
                &cache,
            )?;
            total += pruned;
            refs.push(RefExpireResult {
                ref_name: refname,
                pruned,
            });
        }
        Ok(ReflogExpireOutcome {
            entries_pruned: total,
            dry_run: options.dry_run,
            refs,
        })
    }

    fn load_reflog(&self, user_ref: &str) -> Result<(String, Vec<ReflogEntry>)> {
        let store = self.store();
        for candidate in dwim_reflog_candidates(user_ref) {
            let entries = read_reflog_on_store(store, &candidate)?;
            if !entries.is_empty() {
                return Ok((candidate, entries));
            }
        }
        Ok((canonical_reflog_ref(store, user_ref)?, Vec::new()))
    }
}

struct QueryScanState<'a> {
    query: &'a ReflogQuery,
    entries: Vec<IndexedReflogEntry>,
    limit_reached: bool,
    truncated: bool,
}

impl QueryScanState<'_> {
    /// Collect `entry` when it matches; once `limit` matches are in, the next entry only
    /// marks the page truncated.
    fn visit(&mut self, index: usize, entry: &ReflogEntry) -> ControlFlow<()> {
        if self.limit_reached {
            self.truncated = true;
            return ControlFlow::Break(());
        }
        if !entry_matches_query(entry, self.query) {
            return ControlFlow::Continue(());
        }
        self.entries.push(IndexedReflogEntry {
            index,
            entry: entry.clone(),
        });
        self.limit_reached = self
            .query
            .limit
            .is_some_and(|limit| self.entries.len() >= limit);
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

/// Whether `refname`'s reflog in `store` has at least one entry (reads only the newest one).
fn reflog_has_entries(store: &dyn RefStore, refname: &str) -> Result<bool> {
    let mut found = false;
    store.for_each_reflog_entry_str(RefNameRef::from_trusted(refname), true, &mut |_| {
        found = true;
        ControlFlow::Break(())
    })?;
    Ok(found)
}

fn dwim_reflog_candidates(user_ref: &str) -> Vec<String> {
    let mut out = vec![user_ref.to_owned()];
    if user_ref != "HEAD" && !user_ref.starts_with("refs/") {
        out.push(format!("refs/{user_ref}"));
        out.push(format!("refs/heads/{user_ref}"));
    }
    out
}

/// The ref whose (possibly empty) reflog `user_ref` names: the first DWIM candidate with a
/// reflog, else the full name of a ref that exists without one.
fn canonical_reflog_ref(store: &dyn RefStore, user_ref: &str) -> Result<String> {
    for candidate in dwim_reflog_candidates(user_ref) {
        if store.reflog_exists(RefNameRef::from_trusted(&candidate))? {
            return Ok(candidate);
        }
    }
    if !ref_exists_dwim(store, user_ref)? {
        return Err(Error::ReflogUnknownRef {
            refname: user_ref.to_owned(),
        });
    }
    if user_ref.starts_with("refs/") || user_ref == "HEAD" {
        return Ok(user_ref.to_owned());
    }
    Ok(format!("refs/heads/{user_ref}"))
}

/// Whether `user_ref` names a ref in `store` under Git's DWIM rules (`HEAD` counts while it
/// is stored, even unborn).
fn ref_exists_dwim(store: &dyn RefStore, user_ref: &str) -> Result<bool> {
    if user_ref == "HEAD" {
        return Ok(store.read_raw(RefNameRef::from_trusted("HEAD"))?.is_some());
    }
    let (_, oid) = resolve_ref_dwim(
        |candidate| store.resolve(RefNameRef::from_trusted(candidate)).ok(),
        user_ref,
    );
    Ok(oid.is_some())
}

/// Refs [`RepositoryReflogs::expire`] processes, sorted: every ref with a reflog, or with
/// `all`, every resolvable ref under `refs/` plus `HEAD` when it has a reflog.
fn expire_target_refs(store: &dyn RefStore, all: bool) -> Result<Vec<String>> {
    if !all {
        return reflog_refs_on_store(store);
    }
    let mut names: Vec<String> = crate::refs::collect_resolved_refs(store, "refs/")?
        .into_iter()
        .map(|(name, _)| name)
        .collect();
    if store.reflog_exists(RefNameRef::from_trusted("HEAD"))? {
        names.push("HEAD".to_owned());
    }
    names.sort();
    names.dedup();
    Ok(names)
}

#[cfg(test)]
mod tests {
    use std::fs;

    use super::*;
    use crate::refs::append_reflog;
    use crate::repo::{InitOptions, Repository};

    fn init_repo() -> (tempfile::TempDir, Repository) {
        let dir = tempfile::tempdir().expect("tempdir");
        let (repo, _) = Repository::init(dir.path(), &InitOptions::default()).expect("init");
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
            &crate::objects::ObjectId::from_hex("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa")
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
    fn loose_scan_index_skips_unparseable_newer_line_without_renumbering() {
        let (_dir, repo) = init_repo();
        let git_dir = repo.git_dir().to_path_buf();
        let refname = "refs/heads/y";
        let oid = crate::objects::ObjectId::from_hex("cccccccccccccccccccccccccccccccccccccccc")
            .expect("oid");
        // Newest line is unparseable; the valid entry is older and must stay at index 0.
        let body = format!(
            "0000000000000000000000000000000000000000 {oid} T <t@e.com> 2000 +0000\tcommit: older\nnot-a-valid-reflog-line\n"
        );
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

    #[test]
    fn expire_outcome_refs_are_sorted() {
        let (_dir, repo) = init_repo();
        let git_dir = repo.git_dir();
        let zero = crate::objects::ObjectId::zero();
        let identity = "T <t@example.com> 1700000000 +0000";
        for refname in ["refs/heads/zebra", "refs/heads/alpha"] {
            append_reflog(git_dir, refname, &zero, &zero, identity, "seed", true).expect("append");
        }
        let outcome = repo
            .reflogs()
            .expire(&ExpireOptions {
                now: 1_700_000_000,
                expire: Some("never".into()),
                expire_unreachable: Some("never".into()),
                all: false,
                dry_run: true,
            })
            .expect("expire");
        let names: Vec<_> = outcome.refs.iter().map(|r| r.ref_name.as_str()).collect();
        let mut sorted = names.clone();
        sorted.sort();
        assert_eq!(names, sorted);
        assert!(names.iter().any(|n| *n == "refs/heads/alpha"));
        assert!(names.iter().any(|n| *n == "refs/heads/zebra"));
    }
}

//! Reflog listing, deletion, and expiry for one [`Repository`](crate::repo::Repository).

use std::ops::ControlFlow;
use std::path::Path;

use crate::error::{Error, Result};
use crate::reflog::{
    delete_reflog_entries, delete_reflog_entries_rechain, expire_reflog_git, list_reflog_refs,
    load_gc_reflog_expire_config, parse_reflog_expire_spec, reflog_exists, ReflogEntry,
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
        let (canonical, raw) = self.load_reflog(ref_name.as_str())?;
        let _ = canonical;
        Ok(index_entries_newest_first(&raw))
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
            dry_run: options.dry_run,
            verbose: false,
        };
        let git_dir = self.repo.git_dir();
        let refnames = expire_target_refs(git_dir, options.all)?;
        let mut total = 0usize;
        let mut refs = Vec::with_capacity(refnames.len());
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

fn index_entries_newest_first(entries: &[ReflogEntry]) -> Vec<IndexedReflogEntry> {
    entries
        .iter()
        .rev()
        .enumerate()
        .map(|(index, entry)| IndexedReflogEntry {
            index,
            entry: entry.clone(),
        })
        .collect()
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

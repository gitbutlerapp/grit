//! Library-backed ref workloads matched to `git for-each-ref` and `git update-ref --stdin`.

use std::collections::HashMap;
use std::io::{self, BufRead, Write};

use anyhow::{bail, Context, Result};
use grit_lib::error::Error;
use grit_lib::objects::ObjectId;
use grit_lib::refs::gc::{update_refs, RefTransactionItem};
use grit_lib::refs::{list_refs_for_repository, list_refs_glob_for_repository};
use grit_lib::repo::Repository;

use crate::odb_driver::open_repo;

/// Print refs matching optional `pattern` in default `git for-each-ref` format:
/// `{oid} {objecttype}\t{refname}\n`.
///
/// When `pattern` contains `*`, `?`, or `[`, uses the same glob rules as
/// `git for-each-ref`; otherwise uses prefix listing like `list_refs`.
pub fn for_each_ref(repo: &Repository, pattern: Option<&str>) -> Result<()> {
    let pattern = pattern.unwrap_or("");
    let entries = if pattern.contains('*') || pattern.contains('?') || pattern.contains('[') {
        list_refs_glob_for_repository(repo, pattern).context("list_refs_glob")?
    } else {
        list_refs_for_repository(repo, pattern).context("list_refs")?
    };
    let mut stdout = io::stdout().lock();
    let mut kind_by_oid: HashMap<ObjectId, &str> = HashMap::new();
    repo.odb().with_pack_read_context(|| {
        for (name, oid) in entries {
            let kind = match kind_by_oid.get(&oid) {
                Some(&kind) => kind,
                None => {
                    let kind = match repo.odb().read_info(&oid) {
                        Ok(info) => info.kind.as_str(),
                        Err(Error::ObjectNotFound(_)) => "missing",
                        Err(e) => return Err(e.into()),
                    };
                    kind_by_oid.insert(oid, kind);
                    kind
                }
            };
            writeln!(stdout, "{oid} {kind}\t{name}")?;
        }
        Ok::<(), anyhow::Error>(())
    })
}

/// Read `create` / `update` / `delete` lines from stdin and apply one batch via [`update_refs`].
pub fn update_ref_stdin(repo: &Repository) -> Result<()> {
    update_ref_stdin_at(repo.git_dir())
}

/// Like [`update_ref_stdin`] but takes a git directory path directly (avoids extra work on open).
pub fn update_ref_stdin_at(git_dir: &std::path::Path) -> Result<()> {
    let items = parse_update_ref_stdin()?;
    update_refs(git_dir, &items).context("update_refs batch")?;
    Ok(())
}

fn parse_update_ref_stdin() -> Result<Vec<RefTransactionItem>> {
    let stdin = io::stdin();
    let mut items = Vec::with_capacity(1024);
    for line in stdin.lock().lines() {
        let line = line.context("read stdin")?;
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let mut parts = line.split_whitespace();
        let Some(cmd) = parts.next() else {
            continue;
        };
        match cmd {
            "create" => {
                let name = parts
                    .next()
                    .ok_or_else(|| anyhow::anyhow!("create line missing refname: {line}"))?;
                let oid_hex = parts
                    .next()
                    .ok_or_else(|| anyhow::anyhow!("create line missing oid: {line}"))?;
                if parts.next().is_some() {
                    bail!("unexpected extra fields on create line: {line}");
                }
                let oid = ObjectId::from_hex(oid_hex)
                    .with_context(|| format!("parse oid on create line: {line}"))?;
                items.push(RefTransactionItem {
                    name: name.to_owned(),
                    new_oid: Some(oid),
                    expected_old: None,
                });
            }
            "update" => {
                let name = parts
                    .next()
                    .ok_or_else(|| anyhow::anyhow!("update line missing refname: {line}"))?;
                let new_hex = parts
                    .next()
                    .ok_or_else(|| anyhow::anyhow!("update line missing new oid: {line}"))?;
                let new_oid = ObjectId::from_hex(new_hex)
                    .with_context(|| format!("parse new oid on update line: {line}"))?;
                let old = parts
                    .next()
                    .map(ObjectId::from_hex)
                    .transpose()
                    .with_context(|| format!("parse old oid on update line: {line}"))?;
                if parts.next().is_some() {
                    bail!("unexpected extra fields on update line: {line}");
                }
                items.push(RefTransactionItem {
                    name: name.to_owned(),
                    new_oid: Some(new_oid),
                    expected_old: old,
                });
            }
            "delete" => {
                let name = parts
                    .next()
                    .ok_or_else(|| anyhow::anyhow!("delete line missing refname: {line}"))?;
                let old = parts
                    .next()
                    .map(ObjectId::from_hex)
                    .transpose()
                    .with_context(|| format!("parse old oid on delete line: {line}"))?;
                if parts.next().is_some() {
                    bail!("unexpected extra fields on delete line: {line}");
                }
                items.push(RefTransactionItem {
                    name: name.to_owned(),
                    new_oid: None,
                    expected_old: old,
                });
            }
            other => bail!("unknown update-ref stdin command '{other}'"),
        }
    }
    Ok(items)
}

/// Open a repository at `path` for refs driver subcommands.
pub fn open_repo_for_refs(path: &std::path::Path) -> Result<Repository> {
    open_repo(path)
}

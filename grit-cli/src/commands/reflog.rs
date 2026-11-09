//! `grit reflog` — inspect and maintain reference logs.

use anyhow::{Context, Result};
use grit_lib::error::Error;
use grit_lib::error::RefError;
use grit_lib::ident::Signature;
use grit_lib::refs::reflog::{parse_reflog_ref, reflog_identity_timestamp, ReflogSelector};
use grit_lib::repo::Repository;
use grit_lib::repo_handles::{ExpireOptions, IndexedReflogEntry, MessageMatcher, ReflogQuery};
use grit_lib::revwalk::AbbrevLength;

use crate::context;
use crate::output::Person;
pub use crate::render::reflog::{
    ReflogDeleteOutcome, ReflogExpireOutcome, ReflogExpireRefOutcome, ReflogListEntry,
    ReflogListOutcome, ReflogRefRow, ReflogRefsListOutcome,
};

/// List options for `grit reflog` (shared between clap and the command handler).
#[derive(Debug, Clone, Default)]
pub struct ReflogListOptions {
    pub max_count: Option<usize>,
    pub since: Option<String>,
    pub after: Option<String>,
    pub until: Option<String>,
    pub before: Option<String>,
    pub grep: Option<String>,
}

pub fn run_show(r#ref: Option<String>, options: ReflogListOptions) -> Result<ReflogListOutcome> {
    let repo = context::discover()?;
    let env = repo.environment();
    let now = context::wall_clock_now(env).unix_timestamp();
    let spec = r#ref.as_deref().unwrap_or("HEAD");
    let ref_name = parse_reflog_ref(spec)?;
    let since = parse_since(options.since.as_deref().or(options.after.as_deref()), now)?;
    let until = parse_until(options.until.as_deref().or(options.before.as_deref()), now)?;
    let grep = options.grep.as_deref().map(MessageMatcher::new);
    let page = repo
        .reflogs()
        .query(
            &ref_name,
            &ReflogQuery {
                limit: options.max_count,
                since,
                until,
                grep,
                now,
            },
        )
        .map_err(map_reflog_error)?;
    let entries = page
        .entries
        .into_iter()
        .map(|e| indexed_to_json(&repo, &e, now))
        .collect();
    Ok(ReflogListOutcome {
        r#ref: spec.to_owned(),
        entries,
        truncated: page.truncated,
    })
}

fn parse_since(raw: Option<&str>, now: i64) -> Result<Option<i64>> {
    match raw {
        Some(s) => Ok(Some(
            grit_lib::revwalk::committer_cutoff_from_date(s, now)
                .map_err(|_| anyhow::anyhow!("invalid --since/--after date"))?,
        )),
        None => Ok(None),
    }
}

fn parse_until(raw: Option<&str>, now: i64) -> Result<Option<i64>> {
    match raw {
        Some(s) => Ok(Some(
            grit_lib::revwalk::committer_cutoff_from_date(s, now)
                .map_err(|_| anyhow::anyhow!("invalid --until/--before date"))?,
        )),
        None => Ok(None),
    }
}

pub fn run_delete(specs: Vec<String>) -> Result<ReflogDeleteOutcome> {
    let repo = context::discover()?;
    let mut deleted = Vec::new();
    for spec in specs {
        let selector: ReflogSelector = spec.parse().map_err(map_selector_error)?;
        repo.reflogs()
            .delete(&selector.ref_name, &[selector.index], false)
            .map_err(map_reflog_error)?;
        deleted.push(spec);
    }
    Ok(ReflogDeleteOutcome { deleted })
}

pub fn run_list_refs(limit: Option<usize>) -> Result<ReflogRefsListOutcome> {
    let repo = context::discover()?;
    let mut refs: Vec<ReflogRefRow> = repo
        .reflogs()
        .list_reflog_refs()
        .context("reflog list failed")?
        .into_iter()
        .map(|name| ReflogRefRow {
            r#ref: name.as_str().to_owned(),
        })
        .collect();
    if let Some(n) = limit {
        refs.truncate(n);
    }
    Ok(ReflogRefsListOutcome { refs })
}

pub fn run_expire(
    all: bool,
    expire: Option<String>,
    expire_unreachable: Option<String>,
    dry_run: bool,
) -> Result<ReflogExpireOutcome> {
    let repo = context::discover()?;
    let env = repo.environment();
    let now = context::wall_clock_now(env).unix_timestamp();
    let outcome = repo
        .reflogs()
        .expire(&ExpireOptions {
            now,
            expire,
            expire_unreachable,
            all,
            dry_run,
        })
        .context("reflog expire failed")?;
    Ok(ReflogExpireOutcome {
        entries_pruned: outcome.entries_pruned,
        dry_run: outcome.dry_run,
        refs: outcome
            .refs
            .into_iter()
            .map(|r| ReflogExpireRefOutcome {
                r#ref: r.ref_name,
                pruned: r.pruned,
            })
            .collect(),
    })
}

fn indexed_to_json(repo: &Repository, entry: &IndexedReflogEntry, now: i64) -> ReflogListEntry {
    let ts = reflog_identity_timestamp(&entry.entry.identity).unwrap_or(0);
    let signature = entry.entry.signature();
    let date = signature
        .as_ref()
        .and_then(Signature::format_rfc3339)
        .unwrap_or_default();
    let committer = signature.as_ref().map(person_from_signature);
    let new_hex = entry.entry.new_oid.to_hex();
    let abbreviated_oid = repo
        .revwalk()
        .abbreviate(entry.entry.new_oid, AbbrevLength::Configured)
        .unwrap_or_else(|_| new_hex.clone());
    ReflogListEntry {
        index: entry.index,
        oid: new_hex.clone(),
        old_oid: entry.entry.old_oid.to_hex(),
        new_oid: new_hex,
        date,
        relative_date: context::relative_date_from(ts, now),
        committer,
        message: entry.entry.message.clone(),
        action: entry.entry.action().map(str::to_owned),
        abbreviated_oid,
    }
}

fn person_from_signature(sig: &Signature) -> Person {
    Person {
        name: sig.name.clone(),
        email: sig.email.clone(),
        date: sig.format_rfc3339().unwrap_or_default(),
    }
}

// Include the integer parser's explanation in CLI output as well as the error chain.
fn map_selector_error(err: Error) -> anyhow::Error {
    match err {
        Error::Ref(RefError::ReflogInvalidIndex { spec, source }) => {
            anyhow::Error::new(source).context(format!("invalid reflog index in '{spec}'"))
        }
        other => anyhow::Error::new(other),
    }
}

fn map_reflog_error(err: Error) -> anyhow::Error {
    match err {
        Error::Ref(RefError::ReflogUnknownRef { refname }) => {
            anyhow::anyhow!("unknown ref '{refname}'")
        }
        Error::Ref(RefError::ReflogIndexOutOfRange { refname, index }) => {
            anyhow::anyhow!("reflog entry {refname}@{{{index}}} not found")
        }
        other => anyhow::Error::new(other),
    }
}

/// Dispatch parsed CLI args for `Reflog`.
pub fn dispatch(
    args: crate::args::ReflogArgs,
    env: crate::dispatch::CommandEnv<'_>,
) -> anyhow::Result<crate::exit_status::ExitStatus> {
    use crate::args::*;
    use crate::commands;

    use crate::dispatch::{emit_success, CommandEnv};

    let CommandEnv {
        opts,
        ctx,
        process_env: _,
    } = env;
    let ReflogArgs {
        reference,
        max_count,
        since,
        after,
        until,
        before,
        grep,
        action,
    } = args;
    match action {
        None => emit_success(
            commands::reflog::run_show(
                reference,
                commands::reflog::ReflogListOptions {
                    max_count,
                    since,
                    after,
                    until,
                    before,
                    grep,
                },
            )?,
            opts,
            ctx,
        ),
        Some(ReflogAction::Delete { specs }) => {
            emit_success(commands::reflog::run_delete(specs)?, opts, ctx)
        }
        Some(ReflogAction::Expire {
            all,
            expire,
            expire_unreachable,
            dry_run,
        }) => emit_success(
            commands::reflog::run_expire(all, expire, expire_unreachable, dry_run)?,
            opts,
            ctx,
        ),
        Some(ReflogAction::List { limit }) => {
            emit_success(commands::reflog::run_list_refs(limit)?, opts, ctx)
        }
    }
}

//! Incremental commit listing for `log` and `revs` on plain date-order walks.

use anyhow::{Context, Result};
use grit_lib::objects::ObjectId;
use grit_lib::repo::Repository;
use grit_lib::revwalk::log_walk::WalkedCommits;
use grit_lib::revwalk::{
    rev_walk_list_supports_stream, RevWalkCommitStream, RevWalkCommitStreamItem, RevWalkListOptions,
};
use std::io::Write;

use crate::commits_json::CommitJsonLoader;
use crate::output::json_write;
use crate::stdio;

/// First batch size so `| head` returns quickly.
pub const FIRST_BATCH: usize = 100;
/// Later batch size for long listings.
pub const BATCH: usize = 1000;

/// Whether `walk` can use [`RevWalk::commits_stream`] for listing.
#[must_use]
pub fn plain_walk_streamable(walk: &RevWalkListOptions) -> bool {
    rev_walk_list_supports_stream(walk)
}

pub struct CommitListBatch {
    pub oids: Vec<ObjectId>,
    pub walked: WalkedCommits,
}

impl CommitListBatch {
    pub fn from_stream_items(items: Vec<RevWalkCommitStreamItem>) -> Self {
        let mut walked = WalkedCommits::with_capacity(items.len());
        let mut oids = Vec::with_capacity(items.len());
        for item in items {
            oids.push(item.oid);
            if let Some(bytes) = item.commit_bytes {
                walked.insert(item.oid, &bytes);
            }
        }
        Self { oids, walked }
    }
}

/// Pull up to `cap` commits from `stream`.
pub fn pull_batch(
    stream: &mut RevWalkCommitStream<'_>,
    cap: usize,
) -> Result<Option<CommitListBatch>> {
    let mut items = Vec::new();
    for _ in 0..cap {
        match stream.next() {
            Some(Ok(item)) => items.push(item),
            Some(Err(err)) => return Err(err.into()),
            None => break,
        }
    }
    if items.is_empty() {
        return Ok(None);
    }
    Ok(Some(CommitListBatch::from_stream_items(items)))
}

/// Write a JSON array prefix for outcomes shaped like `{"commits":[...], "truncated": bool, "next": null}`.
pub fn begin_commits_json_document(w: &mut dyn Write) -> Result<()> {
    stdio::io_result(write!(w, "{{\"commits\":["))
        .map_err(|e| anyhow::anyhow!("writing output: {e}"))
}

/// Finish a commits JSON document; `extra` is inserted after `truncated` (for example `,"groups":null`).
pub fn finish_commits_json_document(
    w: &mut dyn Write,
    truncated: bool,
    extra_fields: &str,
) -> Result<()> {
    stdio::io_result(write!(w, "],\"truncated\":{}", truncated))
        .map_err(|e| anyhow::anyhow!("writing output: {e}"))?;
    stdio::io_result(writeln!(
        w,
        "{extra_fields},\"next\":null}}",
        extra_fields = extra_fields
    ))
    .map_err(|e| anyhow::anyhow!("writing output: {e}"))
}

/// Serialize one commit value into a JSON array, with a leading comma when `first` is false.
pub fn write_json_array_element<T: serde::Serialize>(
    w: &mut dyn Write,
    first: &mut bool,
    value: &T,
) -> Result<()> {
    if !*first {
        stdio::io_result(write!(w, ",")).map_err(|e| anyhow::anyhow!("writing output: {e}"))?;
    } else {
        *first = false;
    }
    json_write(serde_json::to_writer(&mut *w, value))
}

/// Load human summaries for one batch, reusing walk bytes when present.
pub fn load_batch_summaries(
    repo: &Repository,
    batch: &mut CommitListBatch,
) -> Result<Vec<crate::context::CommitSummary>> {
    repo.odb().with_pack_read_context(|| {
        CommitJsonLoader::load_summaries_only(repo, &batch.oids, &mut batch.walked)
    })
}

/// Flush `w`, treating broken pipe as success.
pub fn flush_or_broken_pipe(w: &mut dyn Write) -> Result<bool> {
    match stdio::io_result(w.flush()) {
        Ok(()) => Ok(false),
        Err(e) if e.kind() == std::io::ErrorKind::BrokenPipe => Ok(true),
        Err(e) => Err(anyhow::anyhow!("writing output: {e}")),
    }
}

/// Write `buf` to `w`, returning true when the reader hung up.
pub fn write_all_or_broken_pipe(w: &mut dyn Write, buf: &[u8]) -> Result<bool> {
    match stdio::io_result(w.write_all(buf)) {
        Ok(()) => Ok(false),
        Err(e) if e.kind() == std::io::ErrorKind::BrokenPipe => Ok(true),
        Err(e) => Err(anyhow::anyhow!("writing output: {e}")),
    }
}

/// Open a commit stream for `tokens`/`walk`, applying `limit_plus_one` when set.
pub fn open_plain_stream<'repo>(
    repo: &'repo Repository,
    tokens: &[&str],
    walk: &RevWalkListOptions,
    limit_plus_one: Option<usize>,
) -> Result<RevWalkCommitStream<'repo>> {
    let mut stream_opts = walk.clone();
    stream_opts.limit = limit_plus_one;
    stream_opts.retain_commit_data = true;
    repo.rev_walk()
        .commits_stream(tokens, &stream_opts)?
        .context("plain walk should stream")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finish_json_matches_revs_outcome_keys() {
        let mut buf = Vec::new();
        begin_commits_json_document(&mut buf).unwrap();
        finish_commits_json_document(&mut buf, true, ",\"count\":null").unwrap();
        let v: serde_json::Value = serde_json::from_slice(&buf).unwrap();
        assert_eq!(v["truncated"], true);
        assert!(v["commits"].as_array().unwrap().is_empty());
        assert!(v["next"].is_null());
    }
}

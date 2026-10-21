//! Parsing [`FETCH_HEAD`](https://git-scm.com/docs/git-fetch#_discussion) lines.

use crate::error::{Error, Result};
use crate::objects::ObjectId;
use std::path::Path;

/// Overwrite `.git/FETCH_HEAD` with a single for-merge line for an anonymous URL/path fetch.
///
/// Matches `git fetch <url>` when no remote-tracking refspec applies: one line
/// `<oid>` + two tab characters + `<source>`.
///
/// # Errors
///
/// Returns [`Error::Io`] if the file cannot be written.
pub fn write_anonymous_fetch_head(git_dir: &Path, tip: ObjectId, source: &str) -> Result<()> {
    let line = format!("{}\t\t{source}\n", tip.to_hex());
    std::fs::write(git_dir.join("FETCH_HEAD"), line).map_err(Error::Io)
}

/// Collect 40-character hex object IDs from `FETCH_HEAD` content for lines that are **for merge**.
///
/// Git marks merge candidates with either:
/// - `<oid>` + tab + tab + description (empty middle field; typical `branch '…' of …` lines), or
/// - `<oid>` + tab + description when there is no `not-for-merge` marker.
///
/// Lines containing `not-for-merge` after the first tab are skipped.
///
/// This matches the rules used by merge-message formatting and fixes incorrect handling where
/// splitting on tab treated the empty middle field as a separate token.
#[must_use]
pub fn merge_object_ids_hex(input: &str) -> Vec<String> {
    let mut out = Vec::new();
    for line in input.lines() {
        if line.trim().is_empty() {
            continue;
        }
        let Some(first_tab) = line.find('\t') else {
            continue;
        };
        let oid = &line[..first_tab];
        if !ObjectId::is_full_hex(oid) {
            continue;
        }
        let rest = &line[first_tab + 1..];
        if rest.starts_with("not-for-merge") {
            continue;
        }
        let desc = rest.strip_prefix('\t').unwrap_or(rest);
        if desc.is_empty() {
            continue;
        }
        out.push(oid.to_owned());
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn double_tab_for_merge_branch() {
        let input = "a".repeat(40);
        let line = format!("{input}\t\tbranch 'main' of ../repo2\n");
        let oids = merge_object_ids_hex(&line);
        assert_eq!(oids, vec![input]);
    }

    #[test]
    fn not_for_merge_skipped() {
        let input = format!(
            "{}\tnot-for-merge\tbranch 'other' of ../x\n",
            "b".repeat(40)
        );
        assert!(merge_object_ids_hex(&input).is_empty());
    }

    #[test]
    fn bare_url_line_for_merge() {
        let oid = "c".repeat(40);
        let line = format!("{oid}\t\t../repo2\n");
        assert_eq!(merge_object_ids_hex(&line), vec![oid]);
    }
}

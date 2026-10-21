//! Parsing and writing [`FETCH_HEAD`](https://git-scm.com/docs/git-fetch#_discussion) lines.

use crate::error::{Error, Result};
use crate::objects::ObjectId;
use std::path::Path;

/// One line to append to or replace in `.git/FETCH_HEAD`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct FetchHeadLine {
    /// Object id at the tip of the fetched ref.
    pub oid: ObjectId,
    /// When false, the line is written with a `not-for-merge` marker.
    pub for_merge: bool,
    /// Text after the flag field (branch description or source path).
    pub description: String,
}

/// Replace `.git/FETCH_HEAD` with `lines` (Git overwrites on a normal fetch).
///
/// # Errors
///
/// Returns [`Error::Io`] if the file cannot be written.
pub(crate) fn write_fetch_head(git_dir: &Path, lines: &[FetchHeadLine]) -> Result<()> {
    let mut body = String::new();
    for line in lines {
        if line.for_merge {
            body.push_str(&format!("{}\t\t{}\n", line.oid.to_hex(), line.description));
        } else {
            body.push_str(&format!(
                "{}\tnot-for-merge\t{}\n",
                line.oid.to_hex(),
                line.description
            ));
        }
    }
    std::fs::write(git_dir.join("FETCH_HEAD"), body).map_err(Error::Io)
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
    fn write_fetch_head_for_merge_uses_double_tab() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let git_dir = tmp.path().join(".git");
        std::fs::create_dir_all(&git_dir).unwrap();
        let oid = ObjectId::from_hex(&"a".repeat(40)).unwrap();
        write_fetch_head(
            &git_dir,
            &[FetchHeadLine {
                oid,
                for_merge: true,
                description: "../repo".to_owned(),
            }],
        )
        .unwrap();
        let text = std::fs::read_to_string(git_dir.join("FETCH_HEAD")).unwrap();
        assert_eq!(text, format!("{}\t\t../repo\n", oid.to_hex()));
    }

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

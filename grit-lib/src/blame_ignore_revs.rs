//! Load revision ignore lists for [`crate::blame::blame_file`].

use std::collections::HashSet;
use std::fs;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};

use crate::config::parse_path;
use crate::error::{Error, Result};
use crate::objects::ObjectId;
use crate::repo::Repository;
use crate::rev_parse::resolve_revision_as_commit;

/// Where to load ignored revisions from (config, CLI files, CLI rev specs).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct IgnoreRevsSources {
    /// `--ignore-revs-file` paths in argv order. An empty string clears entries loaded so far.
    pub files: Vec<String>,
    /// `--ignore-rev` revision expressions in argv order.
    pub revs: Vec<String>,
    /// When true, prepend every `blame.ignoreRevsFile` config value before `files`.
    pub use_config: bool,
}

/// Commits skipped during blame (Git `--ignore-rev` / `--ignore-revs-file`).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct IgnoreRevs {
    revs: HashSet<ObjectId>,
}

impl IgnoreRevs {
    /// Load ignored commits from config and CLI sources.
    ///
    /// Config `blame.ignoreRevsFile` values are expanded with [`parse_path`]; `:(optional)`
    /// skips missing files (Git `git_config_pathname` semantics, checked on the
    /// work-tree-resolved path).
    /// processed first, then each entry in [`IgnoreRevsSources::files`]. An empty
    /// file path clears the set accumulated so far (Git `--ignore-revs-file ""`).
    /// Finally each `--ignore-rev` spec is resolved to a commit and inserted.
    ///
    /// # Errors
    ///
    /// Returns an error when an ignore-revs file is missing, contains an invalid
    /// object name, or a `--ignore-rev` spec does not resolve to a commit.
    pub fn load(repo: &Repository, sources: &IgnoreRevsSources) -> Result<Self> {
        let mut revs = HashSet::new();
        let config = repo.config().map(|c| (*c).clone()).unwrap_or_default();

        if sources.use_config {
            for path in config.get_all("blame.ignorerevsfile") {
                if path.is_empty() {
                    revs.clear();
                } else {
                    load_file_into(repo, &path, &mut revs)?;
                }
            }
        }

        for path in &sources.files {
            if path.is_empty() {
                revs.clear();
            } else {
                load_file_into(repo, path, &mut revs)?;
            }
        }

        for spec in &sources.revs {
            let oid = resolve_revision_as_commit(repo, spec)
                .map_err(|_| Error::BlameIgnoreRevUnknown { rev: spec.clone() })?;
            revs.insert(oid);
        }

        Ok(Self { revs })
    }

    /// Borrow the loaded commit ids.
    #[must_use]
    pub fn as_set(&self) -> &HashSet<ObjectId> {
        &self.revs
    }
}

fn resolve_ignore_revs_file_path(repo: &Repository, path: &str) -> Option<PathBuf> {
    let (optional, body) = match path.strip_prefix(":(optional)") {
        Some(rest) => (true, rest),
        None => (false, path),
    };
    let expanded = parse_path(repo.environment(), body);
    let p = Path::new(&expanded);
    let resolved = if p.is_absolute() {
        p.to_path_buf()
    } else if let Some(wt) = repo.work_tree() {
        wt.join(p)
    } else {
        repo.git_dir().join(p)
    };
    if optional && !resolved.is_file() {
        return None;
    }
    Some(resolved)
}

fn load_file_into(repo: &Repository, path: &str, revs: &mut HashSet<ObjectId>) -> Result<()> {
    let Some(resolved) = resolve_ignore_revs_file_path(repo, path) else {
        return Ok(());
    };
    let file = fs::File::open(&resolved).map_err(|e| Error::BlameIgnoreRevsFileOpen {
        path: path.to_owned(),
        source: e,
    })?;
    let reader = BufReader::new(file);
    for line in reader.lines() {
        let line = line?;
        parse_ignore_revs_line(repo, path, &line, revs)?;
    }
    Ok(())
}

fn parse_ignore_revs_line(
    repo: &Repository,
    file_path: &str,
    line: &str,
    revs: &mut HashSet<ObjectId>,
) -> Result<()> {
    let trimmed = line
        .split('#')
        .next()
        .unwrap_or(line)
        .trim()
        .trim_end_matches('\r');
    if trimmed.is_empty() {
        return Ok(());
    }
    let oid = ObjectId::from_hex(trimmed).map_err(|_| Error::BlameIgnoreRevsFileInvalidRev {
        path: file_path.to_owned(),
        rev: trimmed.to_owned(),
    })?;
    let expected_len = repo.odb().hash_algo().hex_len();
    if trimmed.len() != expected_len {
        return Err(Error::BlameIgnoreRevsFileInvalidRev {
            path: file_path.to_owned(),
            rev: trimmed.to_owned(),
        });
    }
    match repo.odb().peel_to_commit(oid) {
        Ok(Some(commit)) => {
            revs.insert(commit);
        }
        Ok(None) => {}
        Err(Error::ObjectNotFound(_)) => {}
        Err(e) => return Err(e),
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::objects::HashAlgo;
    use crate::repo::{InitOptions, Repository};
    use grit_test_support::objects::git_supports_sha256;

    fn init_repo(dir: &Path) -> Repository {
        Repository::init(dir, &InitOptions::default()).expect("init")
    }

    #[test]
    fn parse_line_skips_comments_and_blank_lines() {
        let dir = tempfile::tempdir().unwrap();
        let repo = init_repo(dir.path());
        let mut revs = HashSet::new();
        parse_ignore_revs_line(&repo, "revs.txt", "# only comment", &mut revs).unwrap();
        parse_ignore_revs_line(&repo, "revs.txt", "   ", &mut revs).unwrap();
        assert!(revs.is_empty());
    }

    #[test]
    fn parse_line_rejects_abbreviated_oid() {
        let dir = tempfile::tempdir().unwrap();
        let repo = init_repo(dir.path());
        let mut revs = HashSet::new();
        let err = parse_ignore_revs_line(&repo, "f", "deadbeef", &mut revs).unwrap_err();
        assert!(matches!(err, Error::BlameIgnoreRevsFileInvalidRev { .. }));
    }

    #[test]
    fn parse_line_rejects_invalid_hex() {
        let dir = tempfile::tempdir().unwrap();
        let repo = init_repo(dir.path());
        let mut revs = HashSet::new();
        let hex = "g".repeat(40);
        let err = parse_ignore_revs_line(&repo, "f", &hex, &mut revs).unwrap_err();
        assert!(matches!(err, Error::BlameIgnoreRevsFileInvalidRev { .. }));
    }

    #[cfg_attr(
        not(feature = "full-tests"),
        ignore = "starts system git or is heavy; run with --features full-tests"
    )]
    #[test]
    fn parse_line_accepts_crlf_suffix() {
        let dir = tempfile::tempdir().unwrap();
        let repo = init_repo(dir.path());
        std::process::Command::new("git")
            .current_dir(dir.path())
            .args(["init"])
            .status()
            .unwrap();
        std::fs::write(dir.path().join("f"), "x").unwrap();
        std::process::Command::new("git")
            .current_dir(dir.path())
            .args(["add", "f"])
            .status()
            .unwrap();
        std::process::Command::new("git")
            .current_dir(dir.path())
            .env("GIT_AUTHOR_NAME", "t")
            .env("GIT_AUTHOR_EMAIL", "t@e.com")
            .env("GIT_COMMITTER_NAME", "t")
            .env("GIT_COMMITTER_EMAIL", "t@e.com")
            .args(["commit", "-m", "c"])
            .status()
            .unwrap();
        let hex = std::process::Command::new("git")
            .current_dir(dir.path())
            .args(["rev-parse", "HEAD"])
            .output()
            .unwrap();
        let hex = String::from_utf8_lossy(&hex.stdout).trim().to_string();
        let mut revs = HashSet::new();
        parse_ignore_revs_line(&repo, "f", &format!("{hex}\r"), &mut revs).unwrap();
        assert_eq!(revs.len(), 1);
    }

    #[cfg_attr(
        not(feature = "full-tests"),
        ignore = "starts system git or is heavy; run with --features full-tests"
    )]
    #[test]
    fn parse_line_rejects_wrong_length_in_sha256_repo() {
        if !git_supports_sha256() {
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        std::process::Command::new("git")
            .current_dir(dir.path())
            .args(["init", "--object-format=sha256"])
            .status()
            .unwrap();
        let repo = Repository::open(
            &dir.path().join(".git"),
            &crate::config::environment::RepositoryOptions::empty().with_work_tree(dir.path()),
        )
        .unwrap();
        assert_eq!(repo.odb().hash_algo(), HashAlgo::Sha256);
        let mut revs = HashSet::new();
        let forty = "a".repeat(40);
        let err = parse_ignore_revs_line(&repo, "f", &forty, &mut revs).unwrap_err();
        assert!(matches!(err, Error::BlameIgnoreRevsFileInvalidRev { .. }));
    }

    #[test]
    fn parse_line_skips_missing_full_length_oid() {
        let dir = tempfile::tempdir().unwrap();
        let repo = init_repo(dir.path());
        let mut revs = HashSet::new();
        parse_ignore_revs_line(
            &repo,
            "f",
            "0000000000000000000000000000000000000001",
            &mut revs,
        )
        .unwrap();
        assert!(revs.is_empty());
    }

    #[test]
    fn optional_ignore_revs_file_path_is_skipped_when_missing() {
        let dir = tempfile::tempdir().unwrap();
        let repo = init_repo(dir.path());
        let loaded = IgnoreRevs::load(
            &repo,
            &IgnoreRevsSources {
                files: vec![":(optional)no-such-ignore-revs-file".into()],
                ..Default::default()
            },
        )
        .unwrap();
        assert!(loaded.revs.is_empty());
    }

    #[test]
    fn empty_file_entry_clears_prior_revs() {
        let dir = tempfile::tempdir().unwrap();
        let repo = init_repo(dir.path());
        let loaded = IgnoreRevs::load(
            &repo,
            &IgnoreRevsSources {
                files: vec![String::new()],
                ..Default::default()
            },
        )
        .unwrap();
        assert!(loaded.revs.is_empty());
    }
}

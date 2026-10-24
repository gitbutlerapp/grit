//! Shared pathspec matching against index, HEAD tree, and worktree paths.

use crate::error::{Error, Result};
use crate::git_path::pathspec_traverses_symlink_dir;
use crate::index::MODE_TREE;
use crate::objects::{parse_commit, parse_tree, ObjectId};
use crate::pathspec::{has_glob_chars, pathspec_is_exclude, sorted_paths_match_pathspec};
use crate::repo::Repository;
use crate::state::resolve_head_for_repository;
use crate::unicode_normalization::resolve_worktree_path_for_staging;
use std::collections::HashSet;

/// Returns whether `resolved` matches at least one path in the index (stage 0), `HEAD` tree,
/// or an on-disk worktree file (literal pathspecs only).
///
/// # Errors
///
/// Propagates object-store failures while reading `HEAD`'s tree.
pub fn pathspec_matches_known_in_repository(repo: &Repository, resolved: &str) -> Result<bool> {
    KnownPathspecChecker::new(repo).matches(resolved)
}

/// Like [`pathspec_matches_known_in_repository`], but returns [`Error::PathspecNoMatch`] when
/// nothing matches.
pub fn ensure_pathspec_matches_known_in_repository(
    repo: &Repository,
    display_spec: &str,
    resolved: &str,
) -> Result<()> {
    KnownPathspecChecker::new(repo).ensure(display_spec, resolved)
}

/// Checks pathspecs against the paths a repository knows about: stage-0 index entries, the
/// `HEAD` tree, and (for literal pathspecs) files on disk.
///
/// The index and `HEAD` tree are read at most once per checker, and only when some pathspec is
/// not already satisfied by an on-disk worktree file, so validating many literal paths costs one
/// `stat` per path instead of one full index load and tree walk per path.
pub(crate) struct KnownPathspecChecker<'r> {
    repo: &'r Repository,
    ignorecase: bool,
    precompose: bool,
    known: Option<Vec<String>>,
}

impl<'r> KnownPathspecChecker<'r> {
    /// Creates a checker for `repo`; nothing is read until the first query.
    pub(crate) fn new(repo: &'r Repository) -> Self {
        Self {
            repo,
            ignorecase: repo_pathspec_ignorecase(repo),
            precompose: repo.pathspec_precompose_enabled(),
            known: None,
        }
    }

    /// Returns whether `resolved` matches a known path (see
    /// [`pathspec_matches_known_in_repository`]).
    ///
    /// # Errors
    ///
    /// Propagates failures loading the index or reading `HEAD`'s tree.
    pub(crate) fn matches(&mut self, resolved: &str) -> Result<bool> {
        if is_literal_worktree_pathspec(resolved)
            && !self.traverses_symlink_dir(resolved)
            && self.exists_in_worktree(resolved)
        {
            return Ok(true);
        }
        self.matches_known_paths(resolved)
    }

    /// Like [`Self::matches`], but maps a miss to [`Error::PathspecNoMatch`] and a literal path
    /// through a symlinked directory to [`Error::PathspecBeyondSymlink`].
    ///
    /// # Errors
    ///
    /// The errors above, plus failures loading the index or reading `HEAD`'s tree.
    pub(crate) fn ensure(&mut self, display_spec: &str, resolved: &str) -> Result<()> {
        if self.traverses_symlink_dir(resolved) {
            return Err(Error::PathspecBeyondSymlink {
                spec: display_spec.to_owned(),
            });
        }
        let matched = (is_literal_worktree_pathspec(resolved) && self.exists_in_worktree(resolved))
            || self.matches_known_paths(resolved)?;
        if matched {
            Ok(())
        } else {
            Err(Error::PathspecNoMatch {
                spec: display_spec.to_owned(),
            })
        }
    }

    fn matches_known_paths(&mut self, resolved: &str) -> Result<bool> {
        if self.known.is_none() {
            self.known = Some(known_paths(self.repo)?);
        }
        let known = self.known.as_deref().unwrap_or_default();
        Ok(sorted_paths_match_pathspec(
            resolved,
            known,
            self.repo.pathspec_match_context(),
        ))
    }

    fn traverses_symlink_dir(&self, resolved: &str) -> bool {
        self.repo.work_tree().is_some_and(|work_tree| {
            pathspec_traverses_symlink_dir(work_tree, resolved, self.precompose, self.ignorecase)
        })
    }

    fn exists_in_worktree(&self, resolved: &str) -> bool {
        let Some(work_tree) = self.repo.work_tree() else {
            return false;
        };
        if self.precompose {
            resolve_worktree_path_for_staging(work_tree, resolved, true)
                .abs
                .exists()
        } else {
            work_tree.join(resolved).exists()
        }
    }
}

/// A pathspec without glob characters or `:` magic, which may name a worktree file directly.
fn is_literal_worktree_pathspec(resolved: &str) -> bool {
    !has_glob_chars(resolved) && !resolved.starts_with(':') && !pathspec_is_exclude(resolved)
}

fn repo_pathspec_ignorecase(repo: &Repository) -> bool {
    repo.config()
        .ok()
        .and_then(|cfg| cfg.get_bool("core.ignorecase").and_then(|r| r.ok()))
        .unwrap_or(false)
}

fn known_paths(repo: &Repository) -> Result<Vec<String>> {
    let index = repo.load_index()?;
    let mut set = HashSet::<String>::new();
    for entry in &index.entries {
        if entry.stage() == 0 {
            set.insert(String::from_utf8_lossy(&entry.path).into_owned());
        }
    }
    set.extend(sorted_head_tree_paths(repo)?);
    let mut known: Vec<String> = set.into_iter().collect();
    known.sort_unstable();
    Ok(known)
}

/// Every non-tree path in `HEAD`'s tree, sorted in byte order (empty on an unborn branch).
///
/// # Errors
///
/// Propagates failures resolving `HEAD` or reading its commit and trees.
pub(crate) fn sorted_head_tree_paths(repo: &Repository) -> Result<Vec<String>> {
    let head = resolve_head_for_repository(repo)?;
    let Some(head_oid) = head.oid() else {
        return Ok(Vec::new());
    };
    let obj = repo.odb().read(head_oid)?;
    let commit = parse_commit(&obj.data)?;
    let mut paths = Vec::new();
    collect_tree_paths(repo, commit.tree, "", &mut paths)?;
    paths.sort_unstable();
    Ok(paths)
}

fn collect_tree_paths(
    repo: &Repository,
    tree_oid: ObjectId,
    prefix: &str,
    out: &mut Vec<String>,
) -> Result<()> {
    let obj = repo.odb().read(&tree_oid)?;
    for entry in parse_tree(&obj.data)? {
        let name = String::from_utf8_lossy(&entry.name);
        let path = if prefix.is_empty() {
            name.into_owned()
        } else {
            format!("{prefix}/{name}")
        };
        if entry.mode == MODE_TREE {
            collect_tree_paths(repo, entry.oid, &path, out)?;
        } else {
            out.push(path);
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::environment::RepositoryOptions;
    use crate::repo::Repository;
    use std::fs;
    use tempfile::TempDir;

    #[test]
    fn unknown_literal_pathspec_does_not_match_empty_repo() {
        let dir = TempDir::new().expect("tempdir");
        crate::repo::init_repository(
            dir.path(),
            false,
            "main",
            None,
            crate::RefStorageFormat::Files,
        )
        .expect("init");
        let repo =
            Repository::discover(Some(dir.path()), &RepositoryOptions::empty()).expect("discover");
        assert!(!pathspec_matches_known_in_repository(&repo, "does-not-exist").expect("check"));
    }

    #[test]
    #[cfg(unix)]
    fn literal_pathspec_through_symlink_dir_is_not_known() {
        use std::os::unix::fs::symlink;

        let dir = TempDir::new().expect("tempdir");
        crate::repo::init_repository(
            dir.path(),
            false,
            "main",
            None,
            crate::RefStorageFormat::Files,
        )
        .expect("init");
        std::fs::create_dir_all(dir.path().join("real")).expect("mkdir");
        std::fs::write(dir.path().join("real/f"), b"x").expect("write");
        symlink("real", dir.path().join("dl")).expect("symlink");
        let repo =
            Repository::discover(Some(dir.path()), &RepositoryOptions::empty()).expect("discover");
        assert!(
            !pathspec_matches_known_in_repository(&repo, "dl/f").expect("check"),
            "must not follow symlink directory for pathspec matching"
        );
    }

    fn repo_with_commit(dir: &TempDir, files: &[&str]) -> Repository {
        use crate::porcelain::add::{stage, StageOptions};
        use crate::porcelain::commit::{create_commit, CommitRequest};
        use crate::progress::NullProgress;

        crate::repo::init_repository(
            dir.path(),
            false,
            "main",
            None,
            crate::RefStorageFormat::Files,
        )
        .expect("init");
        for rel in files {
            let abs = dir.path().join(rel);
            fs::create_dir_all(abs.parent().expect("parent")).expect("mkdir");
            fs::write(abs, b"x\n").expect("write");
        }
        let repo =
            Repository::discover(Some(dir.path()), &RepositoryOptions::empty()).expect("discover");
        stage(&repo, &StageOptions::default(), &mut NullProgress).expect("stage");
        let ident = "T <t@example.com> 1 +0000".to_owned();
        create_commit(
            &repo,
            &CommitRequest {
                message: "init".into(),
                author: ident.clone(),
                committer: ident,
                ..Default::default()
            },
            &mut NullProgress,
        )
        .expect("commit");
        repo
    }

    #[test]
    fn checker_does_not_load_known_paths_for_existing_literal_files() {
        let dir = TempDir::new().expect("tempdir");
        let repo = repo_with_commit(&dir, &["a.txt", "dir/b.txt"]);
        let mut checker = KnownPathspecChecker::new(&repo);
        checker.ensure("a.txt", "a.txt").expect("a.txt");
        checker.ensure("dir/b.txt", "dir/b.txt").expect("dir/b.txt");
        assert!(checker.matches("dir").expect("dir"));
        assert!(checker.known.is_none(), "index and HEAD must not be read");
    }

    #[test]
    fn checker_matches_tracked_paths_missing_from_worktree() {
        let dir = TempDir::new().expect("tempdir");
        let repo = repo_with_commit(&dir, &["a.txt", "dir/b.txt", "dir/c.txt"]);
        fs::remove_file(dir.path().join("a.txt")).expect("rm");
        fs::remove_dir_all(dir.path().join("dir")).expect("rm dir");
        let mut checker = KnownPathspecChecker::new(&repo);
        checker
            .ensure("a.txt", "a.txt")
            .expect("deleted tracked file");
        checker.ensure("dir", "dir").expect("deleted tracked dir");
        assert!(checker.matches("dir/*.txt").expect("glob"));
        assert!(checker.known.is_some());
        let err = checker.ensure("nope", "nope").expect_err("unknown path");
        assert!(matches!(err, Error::PathspecNoMatch { spec } if spec == "nope"));
        assert!(!checker.matches("*.rs").expect("glob miss"));
    }

    #[test]
    #[cfg(unix)]
    fn checker_rejects_literal_path_through_symlink_dir() {
        use std::os::unix::fs::symlink;

        let dir = TempDir::new().expect("tempdir");
        let repo = repo_with_commit(&dir, &["real/f"]);
        symlink("real", dir.path().join("dl")).expect("symlink");
        let mut checker = KnownPathspecChecker::new(&repo);
        let err = checker.ensure("dl/f", "dl/f").expect_err("beyond symlink");
        assert!(matches!(err, Error::PathspecBeyondSymlink { spec } if spec == "dl/f"));
        assert!(!checker.matches("dl/f").expect("check"));
        checker.ensure("real/f", "real/f").expect("real path");
    }

    #[test]
    fn literal_pathspec_matches_worktree_file() {
        let dir = TempDir::new().expect("tempdir");
        crate::repo::init_repository(
            dir.path(),
            false,
            "main",
            None,
            crate::RefStorageFormat::Files,
        )
        .expect("init");
        fs::write(dir.path().join("tracked.txt"), b"x").expect("write");
        let repo =
            Repository::discover(Some(dir.path()), &RepositoryOptions::empty()).expect("discover");
        assert!(pathspec_matches_known_in_repository(&repo, "tracked.txt").expect("check"));
    }
}

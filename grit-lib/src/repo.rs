//! Repository discovery and the primary `Repository` handle.
//!
//! # Discovery
//!
//! [`Repository::discover`] walks up from a starting directory to find the
//! nearest `.git` directory (or bare repository), honouring `GIT_DIR` and
//! `GIT_WORK_TREE` environment variables and the `.git` gitfile indirection.
//!
//! # Structure
//!
//! A [`Repository`] owns:
//!
//! - `git_dir` — absolute path to the `.git` directory (or the repo root for
//!   bare repos).
//! - `work_tree` — `Some(path)` for non-bare repos, `None` for bare.
//! - [`Odb`] — the loose object database.

use std::collections::{BTreeSet, HashSet};
use std::env;
use std::fs;
use std::fs::OpenOptions;
use std::io::Write;
use std::path::{Component, Path, PathBuf};
use std::sync::Mutex;

use crate::config::{ConfigFile, ConfigScope, ConfigSet};
use crate::error::{Error, Result};
use crate::hooks::run_hook;
use crate::index::Index;
use crate::objects::parse_commit;
use crate::odb::Odb;
use crate::rev_parse::is_inside_work_tree;
use crate::sparse_checkout::effective_cone_mode_for_sparse_file;
use crate::split_index::{write_index_file_split, WriteSplitIndexRequest};
use crate::state::resolve_head;
use crate::worktree_cwd::cwd_relative_under_work_tree;

const GIT_PREFIX_ENV: &str = "GIT_PREFIX";

/// Set `GIT_PREFIX` to the repository-relative path of the process cwd (POSIX, no trailing `/`).
///
/// Git's `git-sh-setup` / `cd_to_toplevel` moves the process to the work tree root but preserves
/// the original subdirectory in `GIT_PREFIX` (`setup.c`). Helpers such as `git-merge-one-file`
/// rely on this for correct cwd-sensitive behavior.
fn export_git_prefix_env(repo: &Repository) {
    let Some(wt) = repo.work_tree.as_ref() else {
        return;
    };
    let Ok(cwd) = env::current_dir() else {
        return;
    };
    let new_s = cwd_relative_under_work_tree(wt, &cwd).unwrap_or_default();
    if new_s.is_empty() {
        if let Ok(existing) = env::var(GIT_PREFIX_ENV) {
            if !existing.trim().is_empty() {
                return;
            }
        }
    }
    env::set_var(GIT_PREFIX_ENV, new_s);
}

fn read_sparse_checkout_patterns(git_dir: &Path) -> Vec<String> {
    let path = git_dir.join("info").join("sparse-checkout");
    let Ok(content) = fs::read_to_string(&path) else {
        return Vec::new();
    };
    content
        .lines()
        .map(|l| l.trim())
        .filter(|l| !l.is_empty() && !l.starts_with('#'))
        .map(String::from)
        .collect()
}

/// A handle to an open Git repository.
#[derive(Debug)]
pub struct Repository {
    /// Absolute path to the git directory (`.git/` or bare repo root).
    pub git_dir: PathBuf,
    /// Absolute path to the working tree, or `None` for bare repos.
    pub work_tree: Option<PathBuf>,
    /// Loose object database.
    pub odb: Odb,
    /// Discovery provenance: true when opened via `GIT_DIR` env or explicit API.
    ///
    /// This suppresses safe.bareRepository implicit checks.
    pub explicit_git_dir: bool,
    /// When the repo was found by walking from a directory containing `.git` / a gitfile,
    /// that directory (matches Git's setup trace using `.git` for the default git-dir).
    pub discovery_root: Option<PathBuf>,
    /// `GIT_WORK_TREE` was set without `GIT_DIR` and applied after discovery (t1510 #1, #5, …).
    pub work_tree_from_env: bool,
    /// `.git` was a gitfile (not a directory) when the repo was discovered.
    pub discovery_via_gitfile: bool,
    /// Cached settings derived from config that are stable for the process lifetime.
    ///
    /// Cached the first time they are needed; recreated on each `Repository` open. Used to
    /// avoid re-loading the system/global/local config cascade on every object read in hot
    /// paths like `Repository::read_replaced`.
    cached_settings: std::sync::Arc<std::sync::OnceLock<RepoCachedSettings>>,
}

/// Repository-level settings derived from config that are read on hot paths.
#[derive(Debug, Clone)]
struct RepoCachedSettings {
    /// `core.useReplaceRefs` (default `true`).
    use_replace_refs: bool,
    /// Effective `refs/replace/` base path (always slash-terminated).
    replace_ref_base: String,
}

impl Repository {
    fn from_canonical_git_dir(git_dir: PathBuf, work_tree: Option<&Path>) -> Result<Self> {
        // Check HEAD exists or is a symlink (linked worktrees have a symlink HEAD)
        let head_path = git_dir.join("HEAD");
        if !head_path.exists() && !head_path.is_symlink() {
            return Err(Error::NotARepository(git_dir.display().to_string()));
        }

        // For git worktrees the `objects/` directory lives in the common git
        // directory pointed to by the `commondir` file.
        let objects_dir = if git_dir.join("objects").exists() {
            git_dir.join("objects")
        } else if let Some(common_dir) = resolve_common_dir(&git_dir) {
            common_dir.join("objects")
        } else {
            return Err(Error::NotARepository(git_dir.display().to_string()));
        };

        if !objects_dir.exists() {
            return Err(Error::NotARepository(git_dir.display().to_string()));
        }

        let work_tree = match work_tree {
            Some(p) => {
                let cwd = env::current_dir().map_err(Error::Io)?;
                let mut resolved = if p.is_absolute() {
                    p.to_path_buf()
                } else {
                    cwd.join(p)
                };
                if resolved.exists() {
                    resolved = resolved
                        .canonicalize()
                        .map_err(|_| Error::PathError(p.display().to_string()))?;
                }
                Some(resolved)
            }
            None => None,
        };

        let odb = if let Some(ref wt) = work_tree {
            Odb::with_work_tree(&objects_dir, wt).with_config_git_dir(git_dir.clone())
        } else {
            Odb::new(&objects_dir).with_config_git_dir(git_dir.clone())
        };

        Ok(Self {
            git_dir,
            work_tree,
            odb,
            explicit_git_dir: false,
            discovery_root: None,
            work_tree_from_env: false,
            discovery_via_gitfile: false,
            cached_settings: std::sync::Arc::new(std::sync::OnceLock::new()),
        })
    }

    /// Lazily compute and return the cached repo-level settings used on hot paths.
    ///
    /// The settings are computed once per `Repository` instance: they read the system / global
    /// / local config cascade and may stat env vars. Because `Repository` is reopened per
    /// command invocation, this matches Git's process-lifetime caching of the same values.
    fn cached_settings(&self) -> &RepoCachedSettings {
        self.cached_settings.get_or_init(|| {
            let cfg = ConfigSet::load(Some(&self.git_dir), true).unwrap_or_default();
            let use_replace_refs = cfg
                .get_bool("core.useReplaceRefs")
                .and_then(|r| r.ok())
                .unwrap_or(true);
            let replace_ref_base = std::env::var("GIT_REPLACE_REF_BASE")
                .ok()
                .filter(|s| !s.is_empty())
                .unwrap_or_else(|| "refs/replace/".to_owned());
            let replace_ref_base = if replace_ref_base.ends_with('/') {
                replace_ref_base
            } else {
                format!("{replace_ref_base}/")
            };
            RepoCachedSettings {
                use_replace_refs,
                replace_ref_base,
            }
        })
    }

    /// Open a repository from an explicit git-dir and optional work-tree.
    ///
    /// # Errors
    ///
    /// Returns [`Error::NotARepository`] if `git_dir` does not look like a
    /// valid git directory (missing `objects/`, `HEAD`, etc.).
    pub fn open(git_dir: &Path, work_tree: Option<&Path>) -> Result<Self> {
        let git_dir = git_dir
            .canonicalize()
            .map_err(|_| Error::NotARepository(git_dir.display().to_string()))?;

        validate_repository_format(&git_dir)?;

        Self::from_canonical_git_dir(git_dir, work_tree)
    }

    /// Like [`Self::open`] but skips [`validate_repository_format`].
    ///
    /// Used after repository discovery when the format is unsupported so callers still learn
    /// the git directory (Git `GIT_DIR_INVALID_FORMAT` still records gitdir for `read_early_config`).
    pub fn open_skipping_format_validation(
        git_dir: &Path,
        work_tree: Option<&Path>,
    ) -> Result<Self> {
        let git_dir = git_dir
            .canonicalize()
            .map_err(|_| Error::NotARepository(git_dir.display().to_string()))?;
        Self::from_canonical_git_dir(git_dir, work_tree)
    }

    /// Discover the repository starting from `start` (defaults to cwd if `None`).
    ///
    /// Checks `GIT_DIR` first; if set, uses it directly.  Otherwise walks up
    /// the directory tree looking for `.git` (regular directory or gitfile).
    ///
    /// # Errors
    ///
    /// Returns [`Error::NotARepository`] if no repository can be found.
    pub fn discover(start: Option<&Path>) -> Result<Self> {
        // GIT_DIR override
        if let Ok(dir) = env::var("GIT_DIR") {
            let cwd = env::current_dir()?;
            let mut git_dir = PathBuf::from(&dir);
            if git_dir.is_relative() {
                git_dir = cwd.join(git_dir);
            }
            // `GIT_DIR` may name a gitfile (`.git` as a file); resolve like Git's `read_gitfile`.
            git_dir = resolve_git_dir_env_path(&git_dir)?;
            let work_tree = env::var("GIT_WORK_TREE").ok().map(|wt| {
                let p = PathBuf::from(wt);
                if p.is_absolute() {
                    p
                } else {
                    cwd.join(p)
                }
            });
            if let Some(ref wt_path) = work_tree {
                if env::var("GIT_WORK_TREE")
                    .ok()
                    .is_some_and(|raw| Path::new(&raw).is_absolute())
                {
                    validate_git_work_tree_path(wt_path)?;
                }
            }
            if work_tree.is_some() {
                let mut repo = Self::open(&git_dir, work_tree.as_deref())?;
                repo.explicit_git_dir = true;
                repo.discovery_root = None;
                repo.work_tree_from_env = false;
                repo.discovery_via_gitfile = false;
                export_git_prefix_env(&repo);
                return Ok(repo);
            }
            // `GIT_DIR` without `GIT_WORK_TREE`: honour `core.bare` / `core.worktree` like Git.
            let (is_bare, core_wt) = read_core_bare_and_worktree(&git_dir)?;
            if is_bare && core_wt.is_some() {
                warn_core_bare_worktree_conflict(&git_dir);
            }
            let resolved_wt = if is_bare {
                None
            } else if let Some(raw) = core_wt {
                Some(resolve_core_worktree_path(&git_dir, &raw)?)
            } else {
                // Without `GIT_WORK_TREE`, Git uses the current working directory as the work
                // tree root (see git-config(1) / `git help repository-layout`), not the parent
                // of `$GIT_DIR`. This matches upstream tests that run
                // `GIT_DIR=other/.git git …` from the top-level repo while manipulating paths
                // under `$PWD` (e.g. t5402-post-merge-hook).
                Some(cwd.canonicalize().unwrap_or_else(|_| cwd.clone()))
            };
            let mut repo = Self::open(&git_dir, resolved_wt.as_deref())?;
            repo.explicit_git_dir = true;
            repo.discovery_root = None;
            repo.work_tree_from_env = false;
            repo.discovery_via_gitfile = false;
            export_git_prefix_env(&repo);
            return Ok(repo);
        }

        let cwd = env::current_dir()?;

        // If GIT_WORK_TREE is set without GIT_DIR, we still need to honor it
        // after discovery (path is relative to cwd, like Git).
        let env_work_tree = env::var("GIT_WORK_TREE").ok().map(|wt| {
            let p = PathBuf::from(wt);
            if p.is_absolute() {
                p
            } else {
                cwd.join(p)
            }
        });
        if let Some(ref p) = env_work_tree {
            if env::var("GIT_WORK_TREE")
                .ok()
                .is_some_and(|raw| Path::new(&raw).is_absolute())
            {
                validate_git_work_tree_path(p)?;
            }
        }
        let start = start.unwrap_or(&cwd);
        let start = if start.is_absolute() {
            start.to_path_buf()
        } else {
            cwd.join(start)
        };

        // Parse GIT_CEILING_DIRECTORIES — mirror Git `setup_git_directory_gently_1` +
        // `longest_ancestor_length` on the canonical cwd path.
        // A leading colon disables symlink resolution for both ceiling paths and cwd.
        let (ceiling_paths, no_resolve_ceilings) = parse_ceiling_directories();
        let ceiling_dirs: Vec<String> = ceiling_paths
            .into_iter()
            .map(|p| path_for_ceiling_compare(&p))
            .collect();

        let start_canon = start.canonicalize().unwrap_or_else(|_| start.clone());
        // For ceiling comparison, use non-canonical path when leading colon disables resolution.
        let ceil_cmp_buf = if no_resolve_ceilings {
            path_for_ceiling_compare(&start)
        } else {
            path_for_ceiling_compare(&start_canon)
        };
        let mut dir_buf = path_for_ceiling_compare(&start_canon);
        let min_offset = offset_1st_component(&dir_buf);
        let mut ceil_offset: isize = longest_ancestor_length(&ceil_cmp_buf, &ceiling_dirs)
            .map(|n| n as isize)
            .unwrap_or(-1);
        if ceil_offset < 0 {
            ceil_offset = min_offset as isize - 2;
        }

        loop {
            let current = Path::new(&dir_buf);
            if let Some(DiscoveredAt { mut repo, gitfile }) = try_open_at(current)? {
                // git/setup.c `setup_git_directory` runs `check_repository_format` on the resolved
                // git dir and dies on a bad format (e.g. a v1-only `extensions.*` in a
                // `repositoryformatversion = 0` repo; t0001 #60). Discovery itself opens with
                // validation skipped so an empty `.git/` is walked past, but a *found* repository
                // must satisfy the format check.
                validate_repository_format(&repo.git_dir)?;
                if let Some(ref wt) = env_work_tree {
                    repo.work_tree = Some(wt.canonicalize().unwrap_or_else(|_| wt.clone()));
                    repo.work_tree_from_env = true;
                } else {
                    repo.work_tree_from_env = false;
                    // Linked worktree (gitfile → admin dir with `commondir`): `Repository::open`
                    // already set `work_tree` to the directory that contains the `.git` file.
                    // Do not replace it with `core.worktree` from the common config — it may be
                    // stale (t1501 multi-worktree) or point at another linked checkout.
                    let linked_gitfile =
                        repo.discovery_via_gitfile && resolve_common_dir(&repo.git_dir).is_some();
                    if !linked_gitfile {
                        let (is_bare, core_wt) = read_core_bare_and_worktree(&repo.git_dir)?;
                        if is_bare {
                            repo.work_tree = None;
                        } else if let Some(raw) = core_wt {
                            repo.work_tree = Some(resolve_core_worktree_path(&repo.git_dir, &raw)?);
                        }
                    }
                }
                let assume_different = env::var("GIT_TEST_ASSUME_DIFFERENT_OWNER")
                    .ok()
                    .map(|v| {
                        let lower = v.to_ascii_lowercase();
                        v == "1" || lower == "true" || lower == "yes" || lower == "on"
                    })
                    .unwrap_or(false);
                if assume_different {
                    repo.enforce_safe_directory()?;
                } else {
                    ensure_valid_ownership(
                        gitfile.as_deref(),
                        repo.work_tree.as_deref(),
                        &repo.git_dir,
                    )?;
                }
                export_git_prefix_env(&repo);
                return Ok(repo);
            }

            let mut offset: isize = dir_buf.len() as isize;
            if offset <= min_offset as isize {
                break;
            }
            loop {
                offset -= 1;
                if offset <= ceil_offset {
                    break;
                }
                if dir_buf
                    .as_bytes()
                    .get(offset as usize)
                    .is_some_and(|b| *b == b'/')
                {
                    break;
                }
            }
            if offset <= ceil_offset {
                break;
            }
            let off_u = offset as usize;
            let new_len = if off_u > min_offset {
                off_u
            } else {
                min_offset
            };
            dir_buf.truncate(new_len);
        }

        Err(Error::NotARepository(start.display().to_string()))
    }

    /// Current directory to use for pathspec / cwd-prefix logic.
    ///
    /// When `GIT_WORK_TREE` points at a directory that does not contain the process cwd
    /// (alternate work tree + index from the main repo directory), Git treats pathspecs as
    /// relative to the work tree root — use that root as the effective cwd.
    #[must_use]
    pub fn effective_pathspec_cwd(&self) -> PathBuf {
        let cwd = env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
        let Some(wt) = self.work_tree.as_ref() else {
            return cwd;
        };
        let inside_lexical = cwd.strip_prefix(wt).is_ok();
        let inside_canon = cwd
            .canonicalize()
            .ok()
            .zip(wt.canonicalize().ok())
            .is_some_and(|(c, w)| c.starts_with(&w));
        if inside_lexical || inside_canon {
            cwd
        } else {
            wt.clone()
        }
    }

    /// Path to the index file.
    #[must_use]
    pub fn index_path(&self) -> PathBuf {
        self.git_dir.join("index")
    }

    /// Resolve which index file to use, honouring `GIT_INDEX_FILE` like Git plumbing.
    ///
    /// Relative paths are resolved from the process current directory.
    pub fn index_path_for_env(&self) -> Result<PathBuf> {
        if let Ok(raw) = env::var("GIT_INDEX_FILE") {
            if !raw.is_empty() {
                let p = PathBuf::from(raw);
                return Ok(if p.is_absolute() {
                    p
                } else {
                    env::current_dir().map_err(Error::Io)?.join(p)
                });
            }
        }
        Ok(self.index_path())
    }

    /// Load the index, expanding sparse-directory placeholders from the object database.
    ///
    /// Commands that operate on individual paths should use this instead of [`Index::load`].
    pub fn load_index(&self) -> Result<Index> {
        let path = self.index_path_for_env()?;
        self.load_index_at(&path)
    }

    /// Like [`Repository::load_index`], but reads from an explicit index file path
    /// (e.g. `GIT_INDEX_FILE` or a worktree-specific index).
    pub fn load_index_at(&self, path: &std::path::Path) -> Result<Index> {
        let cfg = ConfigSet::load(Some(&self.git_dir), true).unwrap_or_default();
        if let Some(res) = cfg.get_bool("index.sparse") {
            res.map_err(Error::ConfigError)?;
        }
        let mut idx = Index::load_expand_sparse_optional(path, &self.odb)?;
        crate::split_index::resolve_split_index_if_needed(&mut idx, &self.git_dir, path)?;
        if let Some(ref wt) = self.work_tree {
            crate::sparse_checkout::clear_skip_worktree_from_present_files(
                &self.git_dir,
                wt,
                &mut idx,
            );
        }
        Ok(idx)
    }

    /// Write the index to the default path after optionally collapsing skip-worktree
    /// subtrees into sparse-directory placeholders (when sparse index is enabled).
    pub fn write_index(&self, index: &mut Index) -> Result<()> {
        self.write_index_at(&self.index_path(), index)
    }

    /// Write the index to the default path and pass explicit `post-index-change` hook flags.
    ///
    /// Parameters:
    /// - `index` is the in-memory index to serialize.
    /// - `updated_workdir` reports that the write is paired with a working-tree update.
    /// - `updated_skipworktree` reports that skip-worktree related index state changed.
    ///
    /// Returns `Ok(())` after the index is written and the hook has been attempted.
    ///
    /// Errors when the index cannot be finalized or written.
    pub fn write_index_with_post_index_change(
        &self,
        index: &mut Index,
        updated_workdir: bool,
        updated_skipworktree: bool,
    ) -> Result<()> {
        self.write_index_at_with_post_index_change(
            &self.index_path(),
            index,
            updated_workdir,
            updated_skipworktree,
        )
    }

    /// Like [`Repository::write_index`], but writes to an explicit index file path.
    pub fn write_index_at(&self, path: &std::path::Path, index: &mut Index) -> Result<()> {
        self.write_index_at_split(path, index, WriteSplitIndexRequest::default())
    }

    /// Whether reading `index` under this repository's config would mark the cache changed solely to
    /// materialize a split index.
    ///
    /// Git's `tweak_split_index` runs after every index read: when `core.splitIndex` is `true` and the
    /// index is not yet a split index, `add_split_index` sets `SPLIT_INDEX_ORDERED` on `cache_changed`,
    /// which makes opportunistic writers such as `status` (`repo_update_index_if_able`) rewrite the
    /// index in split form, creating `.git/sharedindex.<oid>`. This returns `true` exactly when that
    /// would happen: split index is requested (config `true` or `GIT_TEST_SPLIT_INDEX`) and the
    /// supplied `index` does not already carry a `link` extension.
    ///
    /// Returns `false` when split index is disabled/unset or the index is already split.
    #[must_use]
    pub fn split_index_would_force_write(&self, index: &Index) -> bool {
        if index.split_index_base_oid().is_some() {
            return false;
        }
        let cfg = ConfigSet::load(Some(&self.git_dir), true).unwrap_or_default();
        matches!(
            crate::split_index::split_index_config(&cfg),
            crate::split_index::SplitIndexConfig::Enabled
        ) || crate::split_index::git_test_split_index_env()
    }

    /// Like [`Repository::write_index_at`], but passes explicit `post-index-change` hook flags.
    ///
    /// Parameters:
    /// - `path` is the destination index file.
    /// - `index` is the in-memory index to serialize.
    /// - `updated_workdir` reports that the write is paired with a working-tree update.
    /// - `updated_skipworktree` reports that skip-worktree related index state changed.
    ///
    /// Returns `Ok(())` after the index is written and the hook has been attempted.
    ///
    /// Errors when the index cannot be finalized or written.
    pub fn write_index_at_with_post_index_change(
        &self,
        path: &std::path::Path,
        index: &mut Index,
        updated_workdir: bool,
        updated_skipworktree: bool,
    ) -> Result<()> {
        self.write_index_at_split_with_post_index_change(
            path,
            index,
            WriteSplitIndexRequest::default(),
            updated_workdir,
            updated_skipworktree,
        )
    }

    /// Write the index to `path`, optionally emitting a split index (shared base + `link` extension).
    pub fn write_index_at_split(
        &self,
        path: &std::path::Path,
        index: &mut Index,
        split: WriteSplitIndexRequest,
    ) -> Result<()> {
        self.write_index_at_split_with_post_index_change(path, index, split, false, false)
    }

    /// Write the index to `path`, optionally emitting a split index, with explicit hook flags.
    ///
    /// Parameters:
    /// - `path` is the destination index file.
    /// - `index` is the in-memory index to serialize.
    /// - `split` controls whether a split index should be written.
    /// - `updated_workdir` reports that the write is paired with a working-tree update.
    /// - `updated_skipworktree` reports that skip-worktree related index state changed.
    ///
    /// Returns `Ok(())` after the index is written and the hook has been attempted.
    ///
    /// Errors when the index cannot be finalized or written.
    pub fn write_index_at_split_with_post_index_change(
        &self,
        path: &std::path::Path,
        index: &mut Index,
        split: WriteSplitIndexRequest,
        updated_workdir: bool,
        updated_skipworktree: bool,
    ) -> Result<()> {
        // The on-disk index format (entry OID width and trailing checksum) is
        // fixed by the repository's hash algorithm. Stamp it here so every
        // index written through the repository is consistent, regardless of how
        // the in-memory `Index` was constructed (e.g. a fresh `Index::new`).
        index.hash_algo = self.odb.hash_algo();
        // Racy-git protection (Git `ce_smudge_racily_clean_entry`): before overwriting the index,
        // zero the cached size of entries whose worktree file changed in the same timestamp tick
        // as the previous index write, so the rewritten (newer-mtime) index cannot make them look
        // clean. The pre-overwrite mtime of `path` is the racy reference point.
        if let Some(work_tree) = self.work_tree.as_deref() {
            let prev_index_mtime = {
                use std::os::unix::fs::MetadataExt;
                std::fs::symlink_metadata(path)
                    .ok()
                    .map(|m| (m.mtime() as u32, m.mtime_nsec() as u32))
            };
            crate::diff::smudge_racily_clean_entries(index, work_tree, prev_index_mtime);
        }
        self.finalize_sparse_index_if_needed(index)?;
        let cfg = ConfigSet::load(Some(&self.git_dir), true).unwrap_or_default();
        let skip_hash = crate::index::index_skip_hash_for_write(Some(&cfg));
        write_index_file_split(path, &self.git_dir, index, &cfg, split, skip_hash)?;
        // Git `write_locked_index`: `post-index-change` after a successful index write (t1800).
        let updated_workdir_arg = if updated_workdir { "1" } else { "0" };
        let updated_skipworktree_arg = if updated_skipworktree { "1" } else { "0" };
        let _ = run_hook(
            self,
            "post-index-change",
            &[updated_workdir_arg, updated_skipworktree_arg],
            None,
        );
        Ok(())
    }

    fn finalize_sparse_index_if_needed(&self, index: &mut Index) -> Result<()> {
        let cfg = ConfigSet::load(Some(&self.git_dir), true).unwrap_or_default();
        let sparse_enabled = cfg
            .get("core.sparseCheckout")
            .map(|v| v == "true")
            .unwrap_or(false);
        if !sparse_enabled {
            index.sparse_directories = false;
            return Ok(());
        }
        let cone_cfg = cfg
            .get("core.sparseCheckoutCone")
            .and_then(|v| v.parse::<bool>().ok())
            .unwrap_or(true);
        let sparse_ix = cfg
            .get("index.sparse")
            .map(|v| v == "true")
            .unwrap_or(false);
        let patterns = read_sparse_checkout_patterns(&self.git_dir);
        let cone = effective_cone_mode_for_sparse_file(cone_cfg, &patterns);
        let head = resolve_head(&self.git_dir)?;
        let tree_oid = if let Some(oid) = head.oid() {
            let obj = self.odb.read(oid)?;
            let commit = parse_commit(&obj.data)?;
            Some(commit.tree)
        } else {
            None
        };
        if let Some(t) = tree_oid {
            index.try_collapse_sparse_directories(&self.odb, &t, &patterns, cone, sparse_ix)?;
        } else {
            index.sparse_directories = false;
        }
        Ok(())
    }

    /// Path to the `refs/` directory.
    #[must_use]
    pub fn refs_dir(&self) -> PathBuf {
        self.git_dir.join("refs")
    }

    /// Path to `HEAD`.
    #[must_use]
    pub fn head_path(&self) -> PathBuf {
        self.git_dir.join("HEAD")
    }

    /// Relative path from the work tree root to the process current directory, `/`-separated.
    ///
    /// Used for `:(top)` / `:/` pathspec Bloom lookups. Returns `None` for bare repositories or
    /// when paths cannot be resolved; callers should treat `None` like an empty prefix.
    #[must_use]
    pub fn bloom_pathspec_cwd(&self) -> Option<String> {
        let wt = self.work_tree.as_ref()?;
        let cwd = env::current_dir().ok()?;
        let wt = wt.canonicalize().ok()?;
        let cwd = cwd.canonicalize().ok()?;
        let rel = cwd.strip_prefix(&wt).ok()?;
        let s = rel.to_string_lossy().replace('\\', "/");
        let s = s.trim_start_matches('/').to_string();
        Some(s)
    }

    /// Whether this is a bare repository (no working tree).
    #[must_use]
    pub fn is_bare(&self) -> bool {
        if let Ok(cfg) = ConfigSet::load(Some(&self.git_dir), true) {
            if let Some(Ok(bare)) = cfg.get_bool("core.bare") {
                return bare;
            }
        }
        self.work_tree.is_none()
    }

    /// Read an object, transparently following replace refs.
    ///
    /// If `refs/replace/<hex>` exists for the requested OID and
    /// `GIT_NO_REPLACE_OBJECTS` is **not** set, this reads the
    /// replacement object instead.  Otherwise it behaves identically
    /// to `self.odb.read(oid)`.
    pub fn read_replaced(&self, oid: &crate::objects::ObjectId) -> Result<crate::objects::Object> {
        if std::env::var_os("GIT_NO_REPLACE_OBJECTS").is_some() {
            return self.odb.read(oid);
        }
        let settings = self.cached_settings();
        if !settings.use_replace_refs {
            return self.odb.read(oid);
        }
        let replace_ref =
            self.git_dir
                .join(format!("{}{}", settings.replace_ref_base, oid.to_hex()));
        if replace_ref.is_file() {
            if let Ok(content) = std::fs::read_to_string(&replace_ref) {
                let hex = content.trim();
                if let Ok(replacement_oid) = hex.parse::<crate::objects::ObjectId>() {
                    if let Ok(obj) = self.odb.read(&replacement_oid) {
                        return Ok(obj);
                    }
                }
            }
        }
        self.odb.read(oid)
    }
}

/// If `GIT_TRACE_SETUP` is an absolute path, append `setup:` lines (Git test format).
///
/// Upstream tests grep `^setup: ` from the trace file; they do not use the timestamped
/// `trace.c:` prefix that full Git tracing adds.
pub fn trace_repo_setup_if_requested(repo: &Repository) -> std::io::Result<()> {
    let Ok(path) = env::var("GIT_TRACE_SETUP") else {
        return Ok(());
    };
    if path.is_empty() || path == "0" {
        return Ok(());
    }
    let trace_path = Path::new(&path);
    if !trace_path.is_absolute() {
        return Ok(());
    }

    let actual_cwd = env::current_dir()?;
    let actual_cwd = actual_cwd
        .canonicalize()
        .unwrap_or_else(|_| actual_cwd.clone());

    // After setup, Git's traced `cwd` is the worktree root when the process cwd started inside
    // the worktree, but stays at the real cwd when outside (t1510 nephew cases).
    let (trace_cwd, prefix) = if let Some(ref wt) = repo.work_tree {
        let wt_canon = wt.canonicalize().unwrap_or_else(|_| wt.clone());
        if actual_cwd.starts_with(&wt_canon) {
            let rel = actual_cwd
                .strip_prefix(&wt_canon)
                .map(|p| p.to_path_buf())
                .unwrap_or_default();
            let prefix = if rel.as_os_str().is_empty() {
                "(null)".to_owned()
            } else {
                let mut s = rel.to_string_lossy().replace('\\', "/");
                if !s.ends_with('/') {
                    s.push('/');
                }
                s
            };
            (wt_canon, prefix)
        } else {
            (actual_cwd.clone(), "(null)".to_owned())
        }
    } else {
        (actual_cwd.clone(), "(null)".to_owned())
    };

    let git_dir_display =
        display_git_dir_for_setup_trace(repo, &trace_cwd, &actual_cwd, prefix.as_str());
    let common_display = display_common_dir_for_setup_trace(
        repo,
        &trace_cwd,
        &actual_cwd,
        prefix.as_str(),
        &git_dir_display,
    );
    let worktree_display = repo
        .work_tree
        .as_ref()
        .map(|p| {
            p.canonicalize()
                .unwrap_or_else(|_| lexical_normalize_path(p))
                .display()
                .to_string()
        })
        .unwrap_or_else(|| "(null)".to_owned());

    let mut f = OpenOptions::new()
        .create(true)
        .append(true)
        .open(trace_path)?;
    writeln!(f, "setup: git_dir: {git_dir_display}")?;
    writeln!(f, "setup: git_common_dir: {common_display}")?;
    writeln!(f, "setup: worktree: {worktree_display}")?;
    writeln!(f, "setup: cwd: {}", trace_cwd.display())?;
    writeln!(f, "setup: prefix: {prefix}")?;
    Ok(())
}

/// Collapse `.` / `..` in a path for display when `canonicalize()` fails (e.g. non-existent `..` segments).
fn lexical_normalize_path(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    let mut absolute = false;
    for c in path.components() {
        match c {
            Component::Prefix(p) => {
                out.push(p.as_os_str());
            }
            Component::RootDir => {
                absolute = true;
                out.push(c.as_os_str());
            }
            Component::CurDir => {}
            Component::ParentDir => {
                if absolute {
                    let _ = out.pop();
                } else if !out.pop() {
                    out.push("..");
                }
            }
            Component::Normal(s) => out.push(s),
        }
    }
    if out.as_os_str().is_empty() {
        PathBuf::from(".")
    } else {
        out
    }
}

/// Path from `base` to `target` using `..` segments when needed (matches Git setup traces).
fn path_relative_to(target: &Path, base: &Path) -> Option<PathBuf> {
    let t = target.canonicalize().ok()?;
    let b = base.canonicalize().ok()?;
    let tc: Vec<_> = t.components().collect();
    let bc: Vec<_> = b.components().collect();
    let mut i = 0usize;
    while i < tc.len() && i < bc.len() && tc[i] == bc[i] {
        i += 1;
    }
    let up = bc.len().saturating_sub(i);
    let mut out = PathBuf::new();
    for _ in 0..up {
        out.push("..");
    }
    for comp in &tc[i..] {
        out.push(comp.as_os_str());
    }
    Some(out)
}

fn rel_path_for_setup_trace(target: &Path, trace_cwd: &Path) -> String {
    let t = target
        .canonicalize()
        .unwrap_or_else(|_| target.to_path_buf());
    let tc = trace_cwd
        .canonicalize()
        .unwrap_or_else(|_| trace_cwd.to_path_buf());
    if let Some(rel) = path_relative_to(&t, &tc) {
        let s = rel.to_string_lossy().replace('\\', "/");
        return if s.is_empty() || s == "." {
            ".".to_owned()
        } else {
            s
        };
    }
    t.display().to_string()
}

fn trace_cwd_strictly_inside_git_parent(trace_cwd: &Path, git_dir: &Path) -> bool {
    let tc = trace_cwd
        .canonicalize()
        .unwrap_or_else(|_| trace_cwd.to_path_buf());
    let gd = git_dir
        .canonicalize()
        .unwrap_or_else(|_| git_dir.to_path_buf());
    let Some(parent) = gd.parent() else {
        return false;
    };
    let parent = parent.to_path_buf();
    if tc == parent {
        return false;
    }
    tc.starts_with(&parent) && tc != parent
}

fn display_git_dir_for_setup_trace(
    repo: &Repository,
    trace_cwd: &Path,
    actual_cwd: &Path,
    setup_prefix: &str,
) -> String {
    let gd = repo
        .git_dir
        .canonicalize()
        .unwrap_or_else(|_| repo.git_dir.clone());
    let tc = trace_cwd
        .canonicalize()
        .unwrap_or_else(|_| trace_cwd.to_path_buf());
    let ac = actual_cwd
        .canonicalize()
        .unwrap_or_else(|_| actual_cwd.to_path_buf());

    // Bare repo discovered without `GIT_DIR`: cwd inside the git directory (t1510 #16).
    // Trace uses `.` at the git-dir root and the absolute git-dir path from subdirectories.
    if repo.work_tree.is_none() && !repo.explicit_git_dir {
        if ac == gd {
            return ".".to_owned();
        }
        if ac.starts_with(&gd) && ac != gd {
            return gd.display().to_string();
        }
    }

    // Non-bare repo with `core.worktree` while cwd is inside the git-dir (t1510 #20a).
    if !repo.explicit_git_dir {
        if let Some(wt) = &repo.work_tree {
            let wt = wt.canonicalize().unwrap_or_else(|_| wt.clone());
            if ac.starts_with(&gd) && ac != wt {
                return gd.display().to_string();
            }
        }
    }

    // `GIT_DIR` set: Git's `set_git_dir(gitdirenv, make_realpath)` keeps a relative
    // `gitdirenv` only when cwd is at the worktree root or outside the worktree; from a
    // subdirectory it realpath()s to an absolute path (see `setup.c` / t1510).
    if repo.explicit_git_dir {
        if repo.work_tree.is_none() {
            if let Ok(raw) = env::var("GIT_DIR") {
                let p = Path::new(raw.trim());
                if p.is_absolute() {
                    return gd.display().to_string();
                }
                let joined = ac.join(p);
                if joined.is_file() {
                    return gd.display().to_string();
                }
                if let Some(rel) = path_relative_to(&gd, &tc) {
                    let s = rel.to_string_lossy().replace('\\', "/");
                    return if s.is_empty() || s == "." {
                        ".".to_owned()
                    } else {
                        s
                    };
                }
            }
            return gd.display().to_string();
        }
        if let Some(wt) = &repo.work_tree {
            let wt = wt.canonicalize().unwrap_or_else(|_| wt.clone());
            let strictly_inside_wt = ac.starts_with(&wt) && ac != wt;
            if strictly_inside_wt {
                return gd.display().to_string();
            }
            if let Ok(raw) = env::var("GIT_DIR") {
                let p = Path::new(raw.trim());
                if p.is_relative() {
                    let joined = ac.join(p);
                    if joined.is_file() {
                        // `GIT_DIR` points at a gitfile; trace shows the resolved git dir.
                        return gd.display().to_string();
                    }
                    if let Some(rel) = path_relative_to(&gd, &tc) {
                        let s = rel.to_string_lossy().replace('\\', "/");
                        return if s.is_empty() || s == "." {
                            ".".to_owned()
                        } else {
                            s
                        };
                    }
                }
                return gd.display().to_string();
            }
        }
        if trace_cwd_strictly_inside_git_parent(trace_cwd, &gd) {
            return rel_path_for_setup_trace(&gd, trace_cwd);
        }
        return gd.display().to_string();
    }

    let work_relocated = match (&repo.discovery_root, &repo.work_tree) {
        (Some(root), Some(wt)) if !repo.work_tree_from_env => {
            let r = root.canonicalize().unwrap_or_else(|_| root.clone());
            let w = wt.canonicalize().unwrap_or_else(|_| wt.clone());
            r != w
        }
        _ => false,
    };

    if repo.work_tree_from_env {
        if !repo.discovery_via_gitfile {
            if setup_prefix == "(null)" {
                if let (Some(root), Some(wt)) = (&repo.discovery_root, &repo.work_tree) {
                    let r = root.canonicalize().unwrap_or_else(|_| root.clone());
                    let w = wt.canonicalize().unwrap_or_else(|_| wt.clone());
                    if r == w {
                        let dot_git = r.join(".git");
                        let dot_git = dot_git.canonicalize().unwrap_or(dot_git);
                        if gd == dot_git {
                            return ".git".to_owned();
                        }
                    }
                }
            }
            if trace_cwd_strictly_inside_git_parent(trace_cwd, &gd) {
                return rel_path_for_setup_trace(&gd, trace_cwd);
            }
        }
        return gd.display().to_string();
    }

    if work_relocated {
        if let Some(wt) = &repo.work_tree {
            let wt = wt.canonicalize().unwrap_or_else(|_| wt.clone());
            if ac == wt {
                return gd.display().to_string();
            }
            let inside_wt = ac.starts_with(&wt) && ac != wt;
            if inside_wt {
                if let Some(rel) = path_relative_to(&gd, &ac) {
                    let s = rel.to_string_lossy().replace('\\', "/");
                    return if s.is_empty() || s == "." {
                        ".".to_owned()
                    } else {
                        s
                    };
                }
            }
        }
    }
    if repo.work_tree.is_some() {
        if let Some(root) = &repo.discovery_root {
            let r = root.canonicalize().unwrap_or_else(|_| root.clone());
            let dot_git = r.join(".git");
            let dot_git = dot_git.canonicalize().unwrap_or(dot_git);
            if gd == dot_git {
                return ".git".to_owned();
            }
        } else if let Some(wt) = &repo.work_tree {
            let wt = wt.canonicalize().unwrap_or_else(|_| wt.clone());
            let dot_git = wt.join(".git");
            let dot_git = dot_git.canonicalize().unwrap_or(dot_git);
            if gd == dot_git {
                return ".git".to_owned();
            }
        }
    }

    if repo.discovery_via_gitfile && !repo.explicit_git_dir {
        return gd.display().to_string();
    }

    // Bare repo whose git-dir is `parent/.git`: at `parent` the trace shows `.git`; from a
    // subdirectory of `parent` that is still outside `.git`, Git uses the absolute git-dir (t1510
    // #16c sub/ case — not `../.git`).
    if repo.work_tree.is_none() && !repo.explicit_git_dir {
        if let Some(gp) = gd.parent() {
            let gp = gp.canonicalize().unwrap_or_else(|_| gp.to_path_buf());
            let gdc = gd.canonicalize().unwrap_or_else(|_| gd.clone());
            if tc.starts_with(&gp) && tc != gp && !tc.starts_with(&gdc) {
                return gdc.display().to_string();
            }
            if tc == gp {
                return rel_path_for_setup_trace(&gd, trace_cwd);
            }
        }
    }

    if trace_cwd_strictly_inside_git_parent(trace_cwd, &gd) {
        rel_path_for_setup_trace(&gd, trace_cwd)
    } else {
        gd.display().to_string()
    }
}

fn display_common_dir_for_setup_trace(
    repo: &Repository,
    trace_cwd: &Path,
    actual_cwd: &Path,
    _setup_prefix: &str,
    git_dir_display: &str,
) -> String {
    let gd = repo
        .git_dir
        .canonicalize()
        .unwrap_or_else(|_| repo.git_dir.clone());
    let Some(common) = resolve_common_dir(&gd) else {
        return git_dir_display.to_owned();
    };
    let common = common.canonicalize().unwrap_or(common);
    if common == gd {
        return git_dir_display.to_owned();
    }

    let ac = actual_cwd
        .canonicalize()
        .unwrap_or_else(|_| actual_cwd.to_path_buf());
    if repo.work_tree.is_none() && !repo.explicit_git_dir {
        if ac == common {
            return ".".to_owned();
        }
        if ac.starts_with(&common) && ac != common {
            return common.display().to_string();
        }
    }

    let work_relocated = match (&repo.discovery_root, &repo.work_tree) {
        (Some(root), Some(wt)) if !repo.work_tree_from_env => {
            let r = root.canonicalize().unwrap_or_else(|_| root.clone());
            let w = wt.canonicalize().unwrap_or_else(|_| wt.clone());
            r != w
        }
        _ => false,
    };
    if work_relocated {
        if let Some(wt) = &repo.work_tree {
            let wt = wt.canonicalize().unwrap_or_else(|_| wt.clone());
            if ac == wt {
                return common.display().to_string();
            }
            let inside_wt = ac.starts_with(&wt) && ac != wt;
            if inside_wt {
                if let Some(rel) = path_relative_to(&common, &ac) {
                    let s = rel.to_string_lossy().replace('\\', "/");
                    return if s.is_empty() || s == "." {
                        ".".to_owned()
                    } else {
                        s
                    };
                }
            }
        }
    }

    if repo.discovery_via_gitfile && !repo.explicit_git_dir {
        return common.display().to_string();
    }

    if repo.work_tree.is_none() && !repo.explicit_git_dir {
        let tc = trace_cwd
            .canonicalize()
            .unwrap_or_else(|_| trace_cwd.to_path_buf());
        if let Some(cp) = common.parent() {
            let cp = cp.canonicalize().unwrap_or_else(|_| cp.to_path_buf());
            let comc = common.canonicalize().unwrap_or_else(|_| common.clone());
            if tc.starts_with(&cp) && tc != cp && !tc.starts_with(&comc) {
                return comc.display().to_string();
            }
            if tc == cp {
                return rel_path_for_setup_trace(&common, trace_cwd);
            }
        }
    }

    if trace_cwd_strictly_inside_git_parent(trace_cwd, &common) {
        rel_path_for_setup_trace(&common, trace_cwd)
    } else {
        common.display().to_string()
    }
}

/// Resolve the common git directory for linked worktrees.
fn resolve_common_dir(git_dir: &Path) -> Option<PathBuf> {
    let common_raw = fs::read_to_string(git_dir.join("commondir")).ok()?;
    let common_rel = common_raw.trim();
    if common_rel.is_empty() {
        return None;
    }
    let common_dir = if Path::new(common_rel).is_absolute() {
        PathBuf::from(common_rel)
    } else {
        git_dir.join(common_rel)
    };
    Some(common_dir.canonicalize().unwrap_or(common_dir))
}

/// Directory holding `config` for early-config reads (`commondir` when present).
#[must_use]
pub fn common_git_dir_for_config(git_dir: &Path) -> PathBuf {
    resolve_common_dir(git_dir).unwrap_or_else(|| git_dir.to_path_buf())
}

/// True when `extensions.worktreeConfig` is enabled in the common `config`.
pub fn worktree_config_enabled(common_dir: &Path) -> bool {
    let path = common_dir.join("config");
    let Ok(content) = fs::read_to_string(&path) else {
        return false;
    };
    let mut in_extensions = false;
    for raw_line in content.lines() {
        let mut line = raw_line.trim();
        if line.is_empty() || line.starts_with('#') || line.starts_with(';') {
            continue;
        }
        if line.starts_with('[') {
            let Some(end_idx) = line.find(']') else {
                continue;
            };
            let section = line[1..end_idx].trim();
            let section_name = section
                .split_whitespace()
                .next()
                .unwrap_or_default()
                .to_ascii_lowercase();
            in_extensions = section_name == "extensions";
            let remainder = line[end_idx + 1..].trim();
            if remainder.is_empty() || remainder.starts_with('#') || remainder.starts_with(';') {
                continue;
            }
            line = remainder;
        }
        if in_extensions {
            let Some((key, value)) = line.split_once('=') else {
                continue;
            };
            if key.trim().eq_ignore_ascii_case("worktreeconfig") {
                let v = value.trim();
                return v.eq_ignore_ascii_case("true")
                    || v.eq_ignore_ascii_case("yes")
                    || v.eq_ignore_ascii_case("on")
                    || v == "1";
            }
        }
    }
    false
}

fn open_or_create_config_file(path: &Path, scope: ConfigScope) -> Result<ConfigFile> {
    match ConfigFile::from_path(path, scope)? {
        Some(f) => Ok(f),
        None => {
            if let Some(parent) = path.parent() {
                fs::create_dir_all(parent).map_err(Error::Io)?;
            }
            ConfigFile::parse(path, "", scope)
        }
    }
}

fn config_file_bool_true(cfg: &ConfigFile, key: &str) -> bool {
    cfg.get(key).is_some_and(|v| {
        matches!(
            v.trim().to_ascii_lowercase().as_str(),
            "true" | "yes" | "on" | "1"
        )
    })
}

/// Enable per-worktree configuration (`extensions.worktreeConfig`) and create
/// `config.worktree`, matching Git's `init_worktree_config` in `worktree.c`.
///
/// When `core.bare` is true or `core.worktree` is set in the common config,
/// those keys are moved into `config.worktree` so linked worktrees keep working.
///
/// # Errors
///
/// Returns [`Error::Io`] or [`Error::ConfigError`] if config files cannot be read or written.
pub fn init_worktree_config(git_dir: &Path) -> Result<()> {
    let common_dir = common_git_dir_for_config(git_dir);
    let common_config_path = common_dir.join("config");
    let worktree_config_path = git_dir.join("config.worktree");

    if worktree_config_enabled(&common_dir) {
        if !worktree_config_path.exists() {
            if let Some(parent) = worktree_config_path.parent() {
                fs::create_dir_all(parent).map_err(Error::Io)?;
            }
            fs::write(&worktree_config_path, "").map_err(Error::Io)?;
        }
        return Ok(());
    }

    let mut common_cfg = open_or_create_config_file(&common_config_path, ConfigScope::Local)?;
    common_cfg.set("extensions.worktreeConfig", "true")?;

    let mut wt_cfg = open_or_create_config_file(&worktree_config_path, ConfigScope::Worktree)?;

    if config_file_bool_true(&common_cfg, "core.bare") {
        wt_cfg.set("core.bare", "true")?;
        common_cfg.unset("core.bare")?;
    }
    if let Some(worktree) = common_cfg.get("core.worktree") {
        wt_cfg.set("core.worktree", &worktree)?;
        common_cfg.unset("core.worktree")?;
    }

    common_cfg.write()?;
    wt_cfg.write()?;
    Ok(())
}

/// If the common `config` declares a repository format newer than Git's
/// `GIT_REPO_VERSION_READ`, return the human message Git prints for
/// `discover_git_directory_reason` / t1309.
pub fn early_config_ignore_repo_reason(common_dir: &Path) -> Option<String> {
    const GIT_REPO_VERSION_READ: u32 = 1;
    let path = common_dir.join("config");
    let content = fs::read_to_string(&path).ok()?;
    let mut version = 0u32;
    let mut in_core = false;
    for raw_line in content.lines() {
        let mut line = raw_line.trim();
        if line.is_empty() || line.starts_with('#') || line.starts_with(';') {
            continue;
        }
        if line.starts_with('[') {
            let Some(end_idx) = line.find(']') else {
                continue;
            };
            let section = line[1..end_idx].trim();
            let section_name = section
                .split_whitespace()
                .next()
                .unwrap_or_default()
                .to_ascii_lowercase();
            in_core = section_name == "core";
            let remainder = line[end_idx + 1..].trim();
            if remainder.is_empty() || remainder.starts_with('#') || remainder.starts_with(';') {
                continue;
            }
            line = remainder;
        }
        if in_core {
            if let Some((key, value)) = line.split_once('=') {
                if key.trim().eq_ignore_ascii_case("repositoryformatversion") {
                    if let Ok(v) = value.trim().parse::<u32>() {
                        version = v;
                    }
                }
            }
        }
    }
    if version > GIT_REPO_VERSION_READ {
        Some(format!(
            "Expected git repo version <= {GIT_REPO_VERSION_READ}, found {version}"
        ))
    } else {
        None
    }
}

fn path_for_ceiling_compare(path: &Path) -> String {
    let path = path.to_string_lossy();
    #[cfg(windows)]
    {
        path.replace('\\', "/")
    }
    #[cfg(not(windows))]
    {
        path.into_owned()
    }
}

fn offset_1st_component(path: &str) -> usize {
    if path.starts_with('/') {
        1
    } else {
        0
    }
}

/// Git `longest_ancestor_length`: longest strict ancestor prefix among ceilings.
fn longest_ancestor_length(path: &str, ceilings: &[String]) -> Option<usize> {
    if path == "/" {
        return None;
    }
    let mut max_len: Option<usize> = None;
    for ceil in ceilings {
        let mut len = ceil.len();
        while len > 0 && ceil.as_bytes().get(len - 1) == Some(&b'/') {
            len -= 1;
        }
        if len == 0 {
            continue;
        }
        if path.len() <= len + 1 {
            continue;
        }
        if !path.starts_with(&ceil[..len]) {
            continue;
        }
        if path.as_bytes().get(len) != Some(&b'/') {
            continue;
        }
        if path.as_bytes().get(len + 1).is_none() {
            continue;
        }
        max_len = Some(max_len.map_or(len, |m| m.max(len)));
    }
    max_len
}

/// Determine the config file path for a repository or linked worktree.
fn repository_config_path(git_dir: &Path) -> Option<PathBuf> {
    let local = git_dir.join("config");
    if local.exists() {
        return Some(local);
    }
    let common = resolve_common_dir(git_dir)?;
    let shared = common.join("config");
    if shared.exists() {
        Some(shared)
    } else {
        None
    }
}

/// Validate core repository format/version compatibility.
///
/// Supports repository format versions 0 and 1, with extension handling that
/// matches Git's compatibility expectations in upstream repo-version tests.
/// Public wrapper for validate_repository_format.
pub fn validate_repo_format(git_dir: &Path) -> Result<()> {
    validate_repository_format(git_dir)
}

fn validate_repository_format(git_dir: &Path) -> Result<()> {
    let Some(config_path) = repository_config_path(git_dir) else {
        return Ok(());
    };

    let content = fs::read_to_string(&config_path).map_err(Error::Io)?;
    let parsed = parse_repository_format(&content, &config_path)?;

    if parsed.repo_version > 1 {
        return Err(Error::UnsupportedRepositoryFormatVersion(
            parsed.repo_version,
        ));
    }

    if let Some(raw) = parsed.ref_storage.as_deref() {
        let lower = raw.to_ascii_lowercase();
        let name = lower
            .split_once(':')
            .map(|(prefix, _)| prefix)
            .unwrap_or(lower.as_str());
        if !matches!(name, "files" | "reftable") {
            return Err(Error::Message(format!(
                "error: invalid value for 'extensions.refstorage': '{raw}'"
            )));
        }
    }

    if let Some(msg) = parsed.format_error_message() {
        return Err(Error::Message(msg));
    }

    Ok(())
}

/// The result of parsing `core.repositoryformatversion` and `extensions.*` from a
/// repository's `config` file, using Git-compatible format parsing.
struct RepositoryFormat {
    /// Declared `core.repositoryformatversion` (defaults to 0; invalid values ignored).
    repo_version: u32,
    /// All extension keys (lowercased) declared under `[extensions]`.
    extensions: BTreeSet<String>,
    /// Raw value of `extensions.refstorage`, if present.
    ref_storage: Option<String>,
}

impl RepositoryFormat {
    /// Build git's `verify_repository_format` warning/error message for unsupported
    /// extension declarations, or `None` if the extensions are all acceptable.
    ///
    /// This does not cover the `core.repositoryformatversion > 1` case; callers that
    /// need the version message handle it separately via
    /// [`repository_format_warning`].
    fn format_error_message(&self) -> Option<String> {
        // Mirror git/setup.c `check_repo_format` / `verify_repository_format`. Extensions
        // split into:
        //   * v0-compatible (`handle_extension_v0`): respected even in a v0 repository.
        //   * v1-only (`handle_extension`): legal only when `core.repositoryformatversion >= 1`.
        // A v0 repository that declares any v1-only extension is rejected (t0001 #60, #62);
        // an unknown extension is rejected only in a v1 repository.
        let mut v1_only_found: Vec<&str> = Vec::new();
        let mut unknown_found: Vec<&str> = Vec::new();
        for extension in &self.extensions {
            match extension.as_str() {
                // v0-compatible extensions — always allowed.
                "noop" | "preciousobjects" | "partialclone" | "worktreeconfig" => {}
                // v1-only extensions — only valid with repository format version >= 1.
                "noop-v1"
                | "objectformat"
                | "compatobjectformat"
                | "refstorage"
                | "relativeworktrees"
                | "submodulepathconfig" => {
                    if self.repo_version == 0 {
                        v1_only_found.push(extension);
                    }
                }
                // Unknown extension — rejected only in a v1 repository.
                _ => {
                    if self.repo_version >= 1 {
                        unknown_found.push(extension);
                    }
                }
            }
        }

        if !unknown_found.is_empty() {
            let mut msg = if unknown_found.len() == 1 {
                "unknown repository extension found:".to_owned()
            } else {
                "unknown repository extensions found:".to_owned()
            };
            for ext in &unknown_found {
                msg.push_str(&format!("\n\t{ext}"));
            }
            return Some(msg);
        }

        if !v1_only_found.is_empty() {
            let mut msg = if v1_only_found.len() == 1 {
                "repo version is 0, but v1-only extension found:".to_owned()
            } else {
                "repo version is 0, but v1-only extensions found:".to_owned()
            };
            for ext in &v1_only_found {
                msg.push_str(&format!("\n\t{ext}"));
            }
            return Some(msg);
        }

        None
    }
}

/// Parse `core.repositoryformatversion` and `[extensions]` entries out of raw
/// `config` file contents.
///
/// # Errors
///
/// Returns [`Error::ConfigError`] if a section header is malformed (no closing `]`).
fn parse_repository_format(content: &str, config_path: &Path) -> Result<RepositoryFormat> {
    let mut in_core = false;
    let mut in_extensions = false;
    let mut repo_version = 0u32;
    let mut extensions = BTreeSet::new();
    let mut ref_storage: Option<String> = None;

    for raw_line in content.lines() {
        let mut line = raw_line.trim();
        if line.is_empty() || line.starts_with('#') || line.starts_with(';') {
            continue;
        }

        if line.starts_with('[') {
            let Some(end_idx) = line.find(']') else {
                return Err(Error::ConfigError(format!(
                    "invalid config in {}",
                    config_path.display()
                )));
            };

            let section = line[1..end_idx].trim();
            let section_name = section
                .split_whitespace()
                .next()
                .unwrap_or_default()
                .to_ascii_lowercase();
            in_core = section_name == "core";
            in_extensions = section_name == "extensions";

            let remainder = line[end_idx + 1..].trim();
            if remainder.is_empty() || remainder.starts_with('#') || remainder.starts_with(';') {
                continue;
            }
            line = remainder;
        }

        if in_core {
            if let Some((key, value)) = line.split_once('=') {
                if key.trim().eq_ignore_ascii_case("repositoryformatversion") {
                    // Match Git's `read_repository_format`: bad values are ignored (version stays 0).
                    if let Ok(v) = value.trim().parse::<u32>() {
                        repo_version = v;
                    }
                }
            }
        }

        if in_extensions {
            let (key, value) = if let Some((key, value)) = line.split_once('=') {
                (key.trim(), Some(value.trim()))
            } else {
                (line, None)
            };
            if key.eq_ignore_ascii_case("refstorage") {
                ref_storage = value.map(str::to_owned);
            }
            if !key.is_empty() {
                extensions.insert(key.to_ascii_lowercase());
            }
        }
    }

    Ok(RepositoryFormat {
        repo_version,
        extensions,
        ref_storage,
    })
}

/// Return the warning message git would print for a repository whose `config`
/// declares an unsupported format, or `None` if the format is acceptable.
///
/// This matches Git's repository-format verification behavior:
/// commands that run with `RUN_SETUP_GENTLY` (e.g. `git config`) emit this text as a
/// `warning:` and then behave as if no repository were present, which makes the command
/// fail with a non-zero exit. The returned string is the bare message without the
/// `warning: ` prefix.
///
/// `git_dir` is the resolved git directory; its `config` (or the common-dir `config` for
/// linked worktrees) is parsed. A missing config yields `None` (git treats it as ok).
///
/// # Errors
///
/// Returns [`Error::Io`] if the config file exists but cannot be read, or
/// [`Error::ConfigError`] if a section header is malformed.
pub fn repository_format_warning(git_dir: &Path) -> Result<Option<String>> {
    const GIT_REPO_VERSION_READ: u32 = 1;
    let Some(config_path) = repository_config_path(git_dir) else {
        return Ok(None);
    };
    let content = fs::read_to_string(&config_path).map_err(Error::Io)?;
    let parsed = parse_repository_format(&content, &config_path)?;

    if parsed.repo_version > GIT_REPO_VERSION_READ {
        return Ok(Some(format!(
            "Expected git repo version <= {GIT_REPO_VERSION_READ}, found {}",
            parsed.repo_version
        )));
    }

    Ok(parsed.format_error_message())
}

/// Try to open a repository rooted exactly at `dir`.
///
/// Returns `Ok(None)` when `dir` is not a repository root (the caller should
/// walk up); returns `Err` on a structural problem.
/// Result of probing a single directory during [`Repository::discover`].
struct DiscoveredAt {
    repo: Repository,
    /// When discovery used a `.git` gitfile, the path to that file (for ownership checks).
    gitfile: Option<PathBuf>,
}

fn try_open_at(dir: &Path) -> Result<Option<DiscoveredAt>> {
    let dot_git = dir.join(".git");

    // Check for special file types (FIFO, socket, etc.) — reject them
    // instead of walking up to a parent repository.
    #[cfg(unix)]
    {
        use std::os::unix::fs::FileTypeExt;
        if let Ok(meta) = fs::symlink_metadata(&dot_git) {
            let ft = meta.file_type();
            if ft.is_fifo() || ft.is_socket() || ft.is_block_device() || ft.is_char_device() {
                return Err(Error::NotARepository(format!(
                    "invalid gitfile format: {} is not a regular file",
                    dot_git.display()
                )));
            }
            if ft.is_symlink() {
                if let Ok(target_meta) = fs::metadata(&dot_git) {
                    let tft = target_meta.file_type();
                    if tft.is_fifo()
                        || tft.is_socket()
                        || tft.is_block_device()
                        || tft.is_char_device()
                    {
                        return Err(Error::NotARepository(format!(
                            "invalid gitfile format: {} is not a regular file",
                            dot_git.display()
                        )));
                    }
                }
            }
        }
    }

    if dot_git.is_file() {
        // gitfile indirection: file contains "gitdir: <path>"
        let content =
            fs::read_to_string(&dot_git).map_err(|e| Error::NotARepository(e.to_string()))?;
        let git_dir = parse_gitfile(&content, dir)?;
        let mut repo = Repository::open_skipping_format_validation(&git_dir, Some(dir))?;
        // Linked worktree: `core.worktree` in the common config may point at another directory
        // (t1501). When the process cwd is not inside that configured tree, Git uses the
        // discovery directory as the work tree (commondir overrides for ops under the real tree).
        if resolve_common_dir(&git_dir).is_some() {
            let cwd = env::current_dir().map_err(Error::Io)?;
            if repo.work_tree.is_some() && !is_inside_work_tree(&repo, &cwd) {
                let root = if dir.is_absolute() {
                    dir.to_path_buf()
                } else {
                    cwd.join(dir)
                };
                repo.work_tree = Some(root.canonicalize().unwrap_or(root));
            }
        }
        let root = if dir.is_absolute() {
            dir.to_path_buf()
        } else {
            env::current_dir().map_err(Error::Io)?.join(dir)
        };
        repo.discovery_root = Some(root.canonicalize().unwrap_or(root));
        repo.discovery_via_gitfile = true;
        warn_core_bare_worktree_conflict(&git_dir);
        return Ok(Some(DiscoveredAt {
            repo,
            gitfile: Some(dot_git.clone()),
        }));
    }

    if dot_git.is_dir() {
        // If .git is a symlink to a directory, resolve the symlink target
        // for validation but keep the original .git path for user-facing output
        // (matches real git behavior: `rev-parse --git-dir` shows `.git`).
        let open_path = if dot_git.is_symlink() {
            // Resolve the symlink target for validation
            dot_git.read_link().unwrap_or_else(|_| dot_git.clone())
        } else {
            dot_git.clone()
        };
        // Try to open; if the directory is empty or invalid, continue
        // walking up (e.g. an empty .git/ directory should be ignored).
        match Repository::open_skipping_format_validation(&open_path, Some(dir)) {
            Ok(mut repo) => {
                // Restore the original path so rev-parse shows .git not the
                // resolved symlink target.
                if dot_git.is_symlink() {
                    let abs_dot_git = if dot_git.is_absolute() {
                        dot_git
                    } else {
                        dir.join(".git")
                    };
                    repo.git_dir = abs_dot_git;
                }
                let root = if dir.is_absolute() {
                    dir.to_path_buf()
                } else {
                    env::current_dir().map_err(Error::Io)?.join(dir)
                };
                repo.discovery_root = Some(root.canonicalize().unwrap_or(root));
                repo.discovery_via_gitfile = false;
                return Ok(Some(DiscoveredAt {
                    repo,
                    gitfile: None,
                }));
            }
            Err(Error::NotARepository(_)) | Err(Error::ConfigError(_)) => return Ok(None),
            Err(Error::Message(ref msg)) if msg.contains("bad config") => return Ok(None),
            Err(e) => return Err(e),
        }
    }

    // Linked-worktree gitdir/admin directories contain HEAD and commondir,
    // and can be opened as repositories even without a local objects/ dir.
    if dir.join("HEAD").is_file() && dir.join("commondir").is_file() {
        maybe_trace_implicit_bare_repository(dir);
        let repo = Repository::open(dir, None)?;
        warn_core_bare_worktree_conflict(dir);
        return Ok(Some(DiscoveredAt {
            repo,
            gitfile: None,
        }));
    }

    // Check if `dir` itself is a bare repo (has objects/ and HEAD directly)
    if dir.join("objects").is_dir() && dir.join("HEAD").is_file() {
        maybe_trace_implicit_bare_repository(dir);
        // Check safe.bareRepository policy before opening bare repos.
        // When set to "explicit", implicit bare repo discovery is forbidden
        // unless GIT_DIR was set (handled earlier in discover()).
        if !is_inside_dot_git(dir) {
            if let Ok(cfg) = crate::config::ConfigSet::load(None, true) {
                if let Some(val) = cfg.get("safe.bareRepository") {
                    if val.eq_ignore_ascii_case("explicit") {
                        return Err(Error::ForbiddenBareRepository(dir.display().to_string()));
                    }
                }
            }
        }
        let repo = Repository::open(dir, None)?;
        warn_core_bare_worktree_conflict(dir);
        return Ok(Some(DiscoveredAt {
            repo,
            gitfile: None,
        }));
    }

    Ok(None)
}

fn is_inside_dot_git(path: &Path) -> bool {
    path.components().any(|c| c.as_os_str() == ".git")
}

fn maybe_trace_implicit_bare_repository(dir: &Path) {
    let path = match std::env::var("GIT_TRACE2_PERF") {
        Ok(p) if !p.is_empty() => p,
        _ => return,
    };

    if let Ok(mut file) = OpenOptions::new().create(true).append(true).open(path) {
        let _ = writeln!(file, "setup: implicit-bare-repository:{}", dir.display());
    }
}

/// Collect effective `safe.directory` values from protected config (system/global/command),
/// applying empty-value resets like Git.
fn safe_directory_effective_values(git_dir: &Path) -> Vec<String> {
    let cfg = crate::config::ConfigSet::load(Some(git_dir), true)
        .unwrap_or_else(|_| crate::config::ConfigSet::new());
    let mut values: Vec<String> = Vec::new();
    for e in cfg.entries() {
        if e.key == "safe.directory"
            && e.scope != crate::config::ConfigScope::Local
            && e.scope != crate::config::ConfigScope::Worktree
        {
            values.push(e.value.clone().unwrap_or_else(|| "true".to_owned()));
        }
    }
    let mut effective: Vec<String> = Vec::new();
    for v in values {
        if v.is_empty() {
            effective.clear();
        } else {
            effective.push(v);
        }
    }
    effective
}

fn ensure_safe_directory_allows(git_dir: &Path, checked: &Path) -> Result<()> {
    let effective = safe_directory_effective_values(git_dir);
    let checked_s = checked.to_string_lossy().to_string();
    if std::env::var("GRIT_DEBUG_SAFE_DIR").is_ok() {
        eprintln!("debug-safe-directory values={:?}", effective);
    }
    if effective
        .iter()
        .any(|v| safe_directory_matches(v, &checked_s))
    {
        return Ok(());
    }
    Err(Error::DubiousOwnership(checked_s))
}

#[cfg(unix)]
fn path_lstat_uid(path: &Path) -> std::io::Result<u32> {
    use std::os::unix::fs::MetadataExt;
    let meta = fs::symlink_metadata(path)?;
    Ok(meta.uid())
}

#[cfg(unix)]
fn extract_uid_from_env(name: &str) -> Option<u32> {
    let raw = std::env::var(name).ok()?;
    if raw.is_empty() {
        return None;
    }
    raw.parse::<u32>().ok()
}

/// Match Git's `ensure_valid_ownership`: check gitfile, worktree, and gitdir ownership,
/// then `safe.directory` when any path is not owned by the effective user.
#[cfg(unix)]
fn ensure_valid_ownership(
    gitfile: Option<&Path>,
    worktree: Option<&Path>,
    gitdir: &Path,
) -> Result<()> {
    const ROOT_UID: u32 = 0;

    fn owned_by_effective_user(path: &Path) -> std::io::Result<bool> {
        let st_uid = path_lstat_uid(path)?;
        let mut euid = nix::unistd::geteuid().as_raw();
        if euid == ROOT_UID {
            if st_uid == ROOT_UID {
                return Ok(true);
            }
            if let Some(sudo_uid) = extract_uid_from_env("SUDO_UID") {
                euid = sudo_uid;
            }
        }
        Ok(st_uid == euid)
    }

    let assume_different = std::env::var("GIT_TEST_ASSUME_DIFFERENT_OWNER")
        .ok()
        .map(|v| {
            let lower = v.to_ascii_lowercase();
            v == "1" || lower == "true" || lower == "yes" || lower == "on"
        })
        .unwrap_or(false);
    if !assume_different {
        let gitfile_ok = gitfile
            .map(owned_by_effective_user)
            .transpose()?
            .unwrap_or(true);
        // Git may use a `GIT_WORK_TREE` that does not exist yet (t1510); skip ownership when
        // the path is absent instead of failing discovery with ENOENT.
        let wt_ok = match worktree {
            None => true,
            Some(wt) => match owned_by_effective_user(wt) {
                Ok(ok) => ok,
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => true,
                Err(e) => return Err(Error::Io(e)),
            },
        };
        let gd_ok = owned_by_effective_user(gitdir)?;
        if gitfile_ok && wt_ok && gd_ok {
            return Ok(());
        }
    }

    let data_path = if let Some(wt) = worktree {
        wt.canonicalize().unwrap_or_else(|_| wt.to_path_buf())
    } else {
        gitdir
            .canonicalize()
            .unwrap_or_else(|_| gitdir.to_path_buf())
    };
    ensure_safe_directory_allows(gitdir, &data_path)
}

#[cfg(not(unix))]
fn ensure_valid_ownership(
    _gitfile: Option<&Path>,
    _worktree: Option<&Path>,
    _gitdir: &Path,
) -> Result<()> {
    Ok(())
}

impl Repository {
    /// Enforce `safe.directory` ownership checks, matching upstream behavior.
    ///
    /// When `GIT_TEST_ASSUME_DIFFERENT_OWNER=1`, ownership is considered unsafe
    /// unless a matching `safe.directory` value is configured in system/global/
    /// command scopes (repository-local config is ignored).
    pub fn enforce_safe_directory(&self) -> Result<()> {
        let assume_different = std::env::var("GIT_TEST_ASSUME_DIFFERENT_OWNER")
            .ok()
            .map(|v| {
                let lower = v.to_ascii_lowercase();
                v == "1" || lower == "true" || lower == "yes" || lower == "on"
            })
            .unwrap_or(false);
        if !assume_different {
            return Ok(());
        }

        if self.explicit_git_dir {
            return Ok(());
        }

        // In normal discovery, ownership is checked against worktree paths
        // unless invocation starts inside the gitdir, in which case gitdir is
        // checked.
        let checked = if let Some(wt) = &self.work_tree {
            let cwd = std::env::current_dir().ok();
            if let Some(cwd) = cwd {
                if cwd
                    .canonicalize()
                    .ok()
                    .is_some_and(|c| c.starts_with(&self.git_dir))
                {
                    self.git_dir
                        .canonicalize()
                        .unwrap_or_else(|_| self.git_dir.clone())
                } else {
                    wt.canonicalize().unwrap_or_else(|_| wt.clone())
                }
            } else {
                wt.canonicalize().unwrap_or_else(|_| wt.clone())
            }
        } else {
            self.git_dir
                .canonicalize()
                .unwrap_or_else(|_| self.git_dir.clone())
        };

        if std::env::var("GRIT_DEBUG_SAFE_DIR").is_ok() {
            eprintln!(
                "debug-safe-directory checked={} git_dir={} work_tree={:?} cwd={:?}",
                checked.display(),
                self.git_dir.display(),
                self.work_tree,
                std::env::current_dir().ok()
            );
        }
        self.enforce_safe_directory_checked(&checked)
    }

    /// Enforce safe.directory checks using the repository git-dir path.
    ///
    /// Used by operations that explicitly open another repository by path
    /// (e.g. local clone source).
    pub fn enforce_safe_directory_git_dir(&self) -> Result<()> {
        let assume_different = std::env::var("GIT_TEST_ASSUME_DIFFERENT_OWNER")
            .ok()
            .map(|v| {
                let lower = v.to_ascii_lowercase();
                v == "1" || lower == "true" || lower == "yes" || lower == "on"
            })
            .unwrap_or(false);
        if !assume_different {
            return Ok(());
        }
        let checked = self
            .git_dir
            .canonicalize()
            .unwrap_or_else(|_| self.git_dir.clone());
        if std::env::var("GRIT_DEBUG_SAFE_DIR").is_ok() {
            eprintln!(
                "debug-safe-directory(gitdir) checked={} git_dir={} work_tree={:?}",
                checked.display(),
                self.git_dir.display(),
                self.work_tree
            );
        }
        self.enforce_safe_directory_checked(&checked)
    }

    /// Enforce safe.directory checks against an explicit checked path.
    pub fn enforce_safe_directory_git_dir_with_path(&self, checked: &Path) -> Result<()> {
        let assume_different = std::env::var("GIT_TEST_ASSUME_DIFFERENT_OWNER")
            .ok()
            .map(|v| {
                let lower = v.to_ascii_lowercase();
                v == "1" || lower == "true" || lower == "yes" || lower == "on"
            })
            .unwrap_or(false);
        if !assume_different {
            return Ok(());
        }
        self.enforce_safe_directory_checked(checked)
    }

    fn enforce_safe_directory_checked(&self, checked: &Path) -> Result<()> {
        ensure_safe_directory_allows(&self.git_dir, checked)
    }

    /// Verify the repository is safe to use as a `git clone` source (local clone).
    ///
    /// When `GIT_TEST_ASSUME_DIFFERENT_OWNER` is set, applies the same `safe.directory`
    /// rules as discovery. Otherwise checks filesystem ownership of the git directory
    /// only (matching Git's `die_upon_dubious_ownership` for clone).
    pub fn verify_safe_for_clone_source(&self) -> Result<()> {
        let assume_different = std::env::var("GIT_TEST_ASSUME_DIFFERENT_OWNER")
            .ok()
            .map(|v| {
                let lower = v.to_ascii_lowercase();
                v == "1" || lower == "true" || lower == "yes" || lower == "on"
            })
            .unwrap_or(false);
        if assume_different {
            self.enforce_safe_directory_git_dir()
        } else {
            #[cfg(unix)]
            {
                ensure_valid_ownership(None, None, &self.git_dir)
            }
            #[cfg(not(unix))]
            {
                Ok(())
            }
        }
    }
}

fn normalize_fs_path(raw: &str) -> String {
    use std::path::Component;
    let p = std::path::Path::new(raw);
    let mut parts: Vec<String> = Vec::new();
    let mut absolute = false;
    for c in p.components() {
        match c {
            Component::RootDir => {
                absolute = true;
                parts.clear();
            }
            Component::CurDir => {}
            Component::ParentDir => {
                if !parts.is_empty() {
                    parts.pop();
                }
            }
            Component::Normal(s) => parts.push(s.to_string_lossy().to_string()),
            Component::Prefix(_) => {}
        }
    }
    let mut out = if absolute {
        String::from("/")
    } else {
        String::new()
    };
    out.push_str(&parts.join("/"));
    out
}

fn safe_directory_matches(config_value: &str, checked: &str) -> bool {
    if config_value == "*" {
        return true;
    }
    if config_value == "." {
        // CWD only.
        if let Ok(cwd) = std::env::current_dir() {
            let cwd_s = normalize_fs_path(&cwd.to_string_lossy());
            let checked_s = normalize_fs_path(checked);
            return cwd_s == checked_s;
        }
        return false;
    }

    let canonicalize_or_normalize = |raw: &str| -> String {
        let p = std::path::Path::new(raw);
        if p.exists() {
            p.canonicalize()
                .map(|c| c.to_string_lossy().to_string())
                .map(|s| normalize_fs_path(&s))
                .unwrap_or_else(|_| normalize_fs_path(raw))
        } else {
            normalize_fs_path(raw)
        }
    };

    let config_norm = canonicalize_or_normalize(config_value);
    let checked_norm = normalize_fs_path(checked);

    if config_norm.ends_with("/*") {
        let prefix_raw = &config_norm[..config_norm.len() - 2];
        let prefix_norm = canonicalize_or_normalize(prefix_raw);
        let mut prefix = prefix_norm;
        if !prefix.ends_with('/') {
            prefix.push('/');
        }
        return checked_norm.starts_with(&prefix);
    }

    config_norm == checked_norm
}

fn warn_core_bare_worktree_conflict(git_dir: &Path) {
    if env::var("GIT_WORK_TREE")
        .ok()
        .filter(|s| !s.trim().is_empty())
        .is_some()
    {
        return;
    }
    static WARNED_DIRS: Mutex<Option<HashSet<String>>> = Mutex::new(None);
    if let Ok((bare, wt)) = read_core_bare_and_worktree(git_dir) {
        if bare && wt.is_some() {
            let key = git_dir
                .canonicalize()
                .unwrap_or_else(|_| git_dir.to_path_buf())
                .to_string_lossy()
                .to_string();
            let mut guard = WARNED_DIRS.lock().unwrap_or_else(|e| e.into_inner());
            let set = guard.get_or_insert_with(HashSet::new);
            if set.insert(key) {
                eprintln!("warning: core.bare and core.worktree do not make sense");
            }
        }
    }
}

fn read_core_bare_and_worktree(git_dir: &Path) -> Result<(bool, Option<String>)> {
    let Some(config_path) = repository_config_path(git_dir) else {
        return Ok((false, None));
    };
    let content = fs::read_to_string(&config_path).map_err(Error::Io)?;
    let mut in_core = false;
    let mut bare = false;
    let mut worktree: Option<String> = None;
    for raw_line in content.lines() {
        let line = raw_line.trim();
        if line.is_empty() || line.starts_with('#') || line.starts_with(';') {
            continue;
        }
        if line.starts_with('[') {
            in_core = line.eq_ignore_ascii_case("[core]");
            continue;
        }
        if !in_core {
            continue;
        }
        if let Some((k, v)) = line.split_once('=') {
            let key = k.trim();
            let val = v.trim();
            if key.eq_ignore_ascii_case("bare") {
                bare = val.eq_ignore_ascii_case("true");
            } else if key.eq_ignore_ascii_case("worktree") {
                worktree = Some(val.to_owned());
            }
        }
    }
    Ok((bare, worktree))
}

/// Reject impossible `GIT_WORK_TREE` values before repository setup (matches Git's
/// `validate_worktree` / `die` on bogus absolute paths, e.g. t1501).
fn validate_git_work_tree_path(path: &Path) -> Result<()> {
    if !path.is_absolute() {
        return Ok(());
    }
    let comps: Vec<Component<'_>> = path.components().collect();
    let Some(last_normal_idx) = comps
        .iter()
        .enumerate()
        .rev()
        .find_map(|(i, c)| matches!(c, Component::Normal(_)).then_some(i))
    else {
        return Ok(());
    };
    let mut cur = PathBuf::new();
    for (i, comp) in comps.iter().enumerate() {
        match comp {
            Component::Prefix(p) => cur.push(p.as_os_str()),
            Component::RootDir => cur.push(comp.as_os_str()),
            Component::CurDir => {}
            Component::ParentDir => {
                let _ = cur.pop();
            }
            Component::Normal(seg) => {
                cur.push(seg);
                if i != last_normal_idx && !cur.exists() {
                    return Err(Error::PathError(format!(
                        "Invalid path '{}': No such file or directory",
                        cur.display()
                    )));
                }
            }
        }
    }
    Ok(())
}

fn resolve_core_worktree_path(git_dir: &Path, raw: &str) -> Result<PathBuf> {
    let p = Path::new(raw);
    if p.is_absolute() {
        return Ok(p.canonicalize().unwrap_or_else(|_| p.to_path_buf()));
    }
    let old = env::current_dir().map_err(Error::Io)?;
    env::set_current_dir(git_dir).map_err(Error::Io)?;
    env::set_current_dir(raw).map_err(Error::Io)?;
    let resolved = env::current_dir().map_err(Error::Io)?;
    env::set_current_dir(&old).map_err(Error::Io)?;
    Ok(resolved.canonicalize().unwrap_or(resolved))
}

/// When `GIT_DIR` names a gitfile, resolve to the real git directory.
fn resolve_git_dir_env_path(git_dir: &Path) -> Result<PathBuf> {
    if git_dir.is_file() {
        let content =
            fs::read_to_string(git_dir).map_err(|e| Error::NotARepository(e.to_string()))?;
        let base = git_dir
            .parent()
            .ok_or_else(|| Error::NotARepository(git_dir.display().to_string()))?;
        return parse_gitfile(&content, base);
    }
    Ok(git_dir.to_path_buf())
}

/// Resolve an explicit git directory path the same way as `GIT_DIR` (including gitfile indirection).
///
/// # Errors
///
/// Returns [`Error::NotARepository`] for invalid gitfile content.
pub fn resolve_git_directory_arg(git_dir: &Path) -> Result<PathBuf> {
    resolve_git_dir_env_path(git_dir)
}

/// Resolves a work tree's `.git` path (directory or gitfile) to the real git directory.
///
/// # Errors
///
/// Returns [`Error::NotARepository`] when `.git` is missing, invalid, or the gitfile target is absent.
pub fn resolve_dot_git(dot_git: &Path) -> Result<PathBuf> {
    if dot_git.is_dir() {
        return dot_git
            .canonicalize()
            .map_err(|_| Error::NotARepository(dot_git.display().to_string()));
    }
    if dot_git.is_file() {
        let content =
            fs::read_to_string(dot_git).map_err(|e| Error::NotARepository(e.to_string()))?;
        let base = dot_git
            .parent()
            .ok_or_else(|| Error::NotARepository(dot_git.display().to_string()))?;
        return parse_gitfile(&content, base);
    }
    Err(Error::NotARepository(dot_git.display().to_string()))
}

/// Parse a gitfile's `"gitdir: <path>"` line.
fn parse_gitfile(content: &str, base: &Path) -> Result<PathBuf> {
    for line in content.lines() {
        if let Some(rest) = line.strip_prefix("gitdir:") {
            let rel = rest.trim();
            let path = if Path::new(rel).is_absolute() {
                PathBuf::from(rel)
            } else {
                base.join(rel)
            };
            if !path.exists() {
                return Err(Error::NotARepository(path.display().to_string()));
            }
            return Ok(path);
        }
    }
    Err(Error::NotARepository("invalid gitfile format".to_owned()))
}

/// Initialise a new Git repository at the given path.
///
/// Creates the standard directory skeleton (objects/, refs/heads/, refs/tags/,
/// info/, hooks/) and a default `HEAD` pointing to `refs/heads/<initial_branch>`.
///
/// # Parameters
///
/// - `path` — root directory to initialise (created if absent).
/// - `bare` — if true, `path` itself becomes the git-dir; otherwise `path/.git`.
/// - `initial_branch` — branch name for `HEAD` (e.g. `"main"`).
/// - `template_dir` — optional template directory; if `None`, a minimal skeleton
///   is created.
///
/// # Errors
///
/// Returns [`Error::Io`] on filesystem failures.
fn write_fresh_git_directory(
    git_dir: &Path,
    bare: bool,
    initial_branch: &str,
    template_dir: Option<&Path>,
    ref_storage: &str,
    skip_hooks_and_info: bool,
) -> Result<()> {
    let mut subs = vec![
        "objects",
        "objects/info",
        "objects/pack",
        "refs",
        "refs/heads",
        "refs/tags",
    ];
    if !bare && !skip_hooks_and_info {
        subs.push("info");
        subs.push("hooks");
    }
    for sub in subs {
        fs::create_dir_all(git_dir.join(sub))?;
    }

    if ref_storage == "reftable" {
        let reftable_dir = git_dir.join("reftable");
        fs::create_dir_all(&reftable_dir)?;
        let tables_list = reftable_dir.join("tables.list");
        if !tables_list.exists() {
            fs::write(&tables_list, "")?;
        }
    }

    if let Some(tmpl) = template_dir {
        if tmpl.is_dir() {
            copy_template(tmpl, git_dir)?;
        }
    }

    let head_content = format!("ref: refs/heads/{initial_branch}\n");
    fs::write(git_dir.join("HEAD"), head_content)?;

    let needs_extensions = ref_storage == "reftable";
    let repo_version = if needs_extensions { 1 } else { 0 };

    let mut config_content = String::from("[core]\n");
    config_content.push_str(&format!("\trepositoryformatversion = {repo_version}\n"));
    config_content.push_str("\tfilemode = true\n");
    if bare {
        config_content.push_str("\tbare = true\n");
    } else {
        config_content.push_str("\tbare = false\n");
        config_content.push_str("\tlogallrefupdates = true\n");
    }
    if needs_extensions {
        config_content.push_str("[extensions]\n");
        config_content.push_str("\trefStorage = reftable\n");
    }
    fs::write(git_dir.join("config"), config_content)?;

    // Merge `config` from the template on top of the default (matches `git clone --template`).
    if let Some(tmpl) = template_dir {
        if tmpl.is_dir() {
            let tmpl_config = tmpl.join("config");
            if tmpl_config.is_file() {
                let tmpl_text = fs::read_to_string(&tmpl_config)?;
                let tmpl_parsed = ConfigFile::parse(&tmpl_config, &tmpl_text, ConfigScope::Local)?;
                let dest_path = git_dir.join("config");
                let dest_text = fs::read_to_string(&dest_path)?;
                let mut dest_parsed =
                    ConfigFile::parse(&dest_path, &dest_text, ConfigScope::Local)?;
                for e in &tmpl_parsed.entries {
                    // Git clone ignores `core.bare` from templates (non-bare clone must stay non-bare).
                    if e.key == "core.bare" {
                        continue;
                    }
                    if let Some(v) = &e.value {
                        let _ = dest_parsed.set(&e.key, v);
                    } else {
                        let _ = dest_parsed.set(&e.key, "true");
                    }
                }
                dest_parsed.write()?;
            }
        }
    }

    fs::write(
        git_dir.join("description"),
        "Unnamed repository; edit this file 'description' to name the repository.\n",
    )?;
    Ok(())
}

/// Initialise a non-bare repository with the git directory at `git_dir` and the work tree at `work_tree`.
///
/// Creates `work_tree/.git` as a gitfile pointing at `git_dir` (absolute path). Matches `git clone
/// --separate-git-dir` layout.
///
/// # Errors
///
/// Returns [`Error::Io`] on filesystem failures.
pub fn init_repository_separate_git_dir(
    work_tree: &Path,
    git_dir: &Path,
    initial_branch: &str,
    template_dir: Option<&Path>,
    ref_storage: &str,
) -> Result<Repository> {
    let skip_hooks_info = template_dir.is_some_and(|p| p.as_os_str().is_empty());
    fs::create_dir_all(work_tree)?;
    fs::create_dir_all(git_dir)?;
    write_fresh_git_directory(
        git_dir,
        false,
        initial_branch,
        template_dir,
        ref_storage,
        skip_hooks_info,
    )?;

    // Write an absolute `gitdir:` path, matching C Git's `init_db` →
    // `set_git_dir(real_git_dir, make_realpath=1)` → `separate_git_dir`, which records the
    // realpath of the separate git directory (`t5601` "clone separate gitdir: output"). This
    // path is only used by `git clone --separate-git-dir`; submodule layouts use a different
    // code path and are unaffected.
    let gitfile = work_tree.join(".git");
    let abs_git_dir = fs::canonicalize(git_dir).unwrap_or_else(|_| git_dir.to_path_buf());
    let abs_git_dir = abs_git_dir.to_string_lossy().replace('\\', "/");
    fs::write(gitfile, format!("gitdir: {abs_git_dir}\n"))?;

    Repository::open(git_dir, Some(work_tree))
}

/// Initialise a **minimal** bare repository directory layout matching `git clone --template= --bare`.
///
/// Git's clone-with-empty-template omits `hooks/`, `info/`, `description`, and `branches/` until
/// something needs them; tests rely on `mkdir <repo>/info` succeeding afterward.
///
/// # Parameters
///
/// - `git_dir` — bare repository root (the destination `.git` directory for a bare clone).
/// - `initial_branch` — used only for the initial `HEAD` symref text before clone rewires it.
///
/// # Errors
///
/// Returns [`Error::Io`] on filesystem failures.
/// Ensure `core.bare = true` in the repository `config` (used after `git clone --bare`).
pub fn ensure_core_bare(git_dir: &Path) -> Result<()> {
    let path = git_dir.join("config");
    let text = fs::read_to_string(&path).unwrap_or_default();
    if text.lines().any(|l| {
        let t = l.trim();
        t == "bare = true" || t == "bare=true"
    }) {
        return Ok(());
    }
    let mut out = text;
    if !out.ends_with('\n') && !out.is_empty() {
        out.push('\n');
    }
    if !out.contains("[core]") {
        out.push_str("[core]\n");
    }
    out.push_str("\tbare = true\n");
    fs::write(path, out).map_err(Error::Io)
}

pub fn init_bare_clone_minimal(
    git_dir: &Path,
    initial_branch: &str,
    ref_storage: &str,
) -> Result<()> {
    for sub in &[
        "objects",
        "objects/info",
        "objects/pack",
        "refs",
        "refs/heads",
        "refs/tags",
    ] {
        fs::create_dir_all(git_dir.join(sub))?;
    }

    if ref_storage == "reftable" {
        let reftable_dir = git_dir.join("reftable");
        fs::create_dir_all(&reftable_dir)?;
        let tables_list = reftable_dir.join("tables.list");
        if !tables_list.exists() {
            fs::write(&tables_list, "")?;
        }
    }

    let head_content = format!("ref: refs/heads/{initial_branch}\n");
    fs::write(git_dir.join("HEAD"), head_content)?;

    let needs_extensions = ref_storage == "reftable";
    let repo_version = if needs_extensions { 1 } else { 0 };
    let mut config_content = String::from("[core]\n");
    config_content.push_str(&format!("\trepositoryformatversion = {repo_version}\n"));
    config_content.push_str("\tfilemode = true\n");
    config_content.push_str("\tbare = true\n");
    if needs_extensions {
        config_content.push_str("[extensions]\n");
        config_content.push_str("\trefStorage = reftable\n");
    }
    fs::write(git_dir.join("config"), config_content)?;

    fs::write(
        git_dir.join("packed-refs"),
        "# pack-refs with: peeled fully-peeled sorted\n",
    )?;
    Ok(())
}

pub fn init_repository(
    path: &Path,
    bare: bool,
    initial_branch: &str,
    template_dir: Option<&Path>,
    ref_storage: &str,
) -> Result<Repository> {
    let skip_hooks_info = !bare && template_dir.is_some_and(|p| p.as_os_str().is_empty());
    let git_dir = if bare {
        path.to_path_buf()
    } else {
        path.join(".git")
    };

    if !bare {
        fs::create_dir_all(path)?;
    }
    fs::create_dir_all(&git_dir)?;
    write_fresh_git_directory(
        &git_dir,
        bare,
        initial_branch,
        template_dir,
        ref_storage,
        skip_hooks_info,
    )?;

    let work_tree = if bare { None } else { Some(path) };
    Repository::open(&git_dir, work_tree)
}

/// Initialise a **bare** repository at `git_dir` with `core.worktree` set to `work_tree`.
///
/// Used when `GIT_WORK_TREE` is set during `git clone`: the clone destination is the bare
/// git directory and checked-out files go under the environment work tree (matches upstream Git).
///
/// # Errors
///
/// Returns [`Error::Io`] on filesystem failures.
pub fn init_bare_with_env_worktree(
    git_dir: &Path,
    work_tree: &Path,
    initial_branch: &str,
    template_dir: Option<&Path>,
    ref_storage: &str,
) -> Result<Repository> {
    fs::create_dir_all(git_dir)?;
    fs::create_dir_all(work_tree)?;
    write_fresh_git_directory(
        git_dir,
        true,
        initial_branch,
        template_dir,
        ref_storage,
        false,
    )?;
    let work_tree_abs = fs::canonicalize(work_tree).unwrap_or_else(|_| work_tree.to_path_buf());
    let config_path = git_dir.join("config");
    let mut config = match ConfigFile::from_path(&config_path, ConfigScope::Local)? {
        Some(c) => c,
        None => ConfigFile::parse(&config_path, "", ConfigScope::Local)?,
    };
    config.set("core.worktree", &work_tree_abs.to_string_lossy())?;
    config.write()?;
    Repository::open(git_dir, Some(work_tree))
}

/// Initialise a repository whose git directory is separate from the work tree.
///
/// Creates `git_dir` with the usual layout, writes `work_tree/.git` as a gitfile
/// pointing at `git_dir`, and sets `core.worktree` in `git_dir/config`.
pub fn init_repository_separate(
    work_tree: &Path,
    git_dir: &Path,
    initial_branch: &str,
    template_dir: Option<&Path>,
) -> Result<Repository> {
    fs::create_dir_all(work_tree)?;
    if git_dir.exists() {
        return Err(Error::PathError(format!(
            "git directory '{}' already exists",
            git_dir.display()
        )));
    }

    for sub in &[
        "objects",
        "objects/info",
        "objects/pack",
        "refs",
        "refs/heads",
        "refs/tags",
        "info",
        "hooks",
    ] {
        fs::create_dir_all(git_dir.join(sub))?;
    }

    if let Some(tmpl) = template_dir {
        if tmpl.is_dir() {
            copy_template(tmpl, git_dir)?;
        }
    }

    fs::write(
        git_dir.join("HEAD"),
        format!("ref: refs/heads/{initial_branch}\n"),
    )?;

    let work_tree_abs = fs::canonicalize(work_tree).unwrap_or_else(|_| work_tree.to_path_buf());
    let git_dir_abs = fs::canonicalize(git_dir).unwrap_or_else(|_| git_dir.to_path_buf());
    let config_content = format!(
        "[core]\n\trepositoryformatversion = 0\n\tfilemode = true\n\tbare = false\n\tlogallrefupdates = true\n\tworktree = {}\n",
        work_tree_abs.display()
    );
    fs::write(git_dir.join("config"), config_content)?;
    fs::write(
        git_dir.join("description"),
        "Unnamed repository; edit this file 'description' to name the repository.\n",
    )?;

    let gitfile = work_tree.join(".git");
    fs::write(&gitfile, format!("gitdir: {}\n", git_dir_abs.display()))?;

    Repository::open(git_dir, Some(work_tree))
}

/// Recursively copy template files from `src` to `dst`.
fn copy_template(src: &Path, dst: &Path) -> Result<()> {
    for entry in fs::read_dir(src)? {
        let entry = entry?;
        let src_path = entry.path();
        let dst_path = dst.join(entry.file_name());
        if src_path.is_dir() {
            fs::create_dir_all(&dst_path)?;
            copy_template(&src_path, &dst_path)?;
        } else {
            fs::copy(&src_path, &dst_path)?;
        }
    }
    Ok(())
}

/// Parse `GIT_CEILING_DIRECTORIES` into a list of absolute paths and whether
/// symlink resolution should be skipped.
///
/// The variable is colon-separated (`:`) on Unix.  Empty entries and
/// non-absolute paths are silently skipped, matching Git's behaviour.
///
/// A leading colon (`:path1:path2`) disables symlink resolution for all
/// ceiling paths AND the cwd used for comparison (Git `resolve_symlinks` flag).
fn parse_ceiling_directories() -> (Vec<PathBuf>, bool) {
    let raw = match env::var("GIT_CEILING_DIRECTORIES") {
        Ok(val) => val,
        Err(_) => return (Vec::new(), false),
    };
    if raw.is_empty() {
        return (Vec::new(), false);
    }
    // A leading colon means "don't resolve symlinks".
    let (no_resolve, effective) = if raw.starts_with(':') {
        (true, &raw[1..])
    } else {
        (false, raw.as_str())
    };
    let paths = effective
        .split(':')
        .filter(|s| !s.is_empty())
        .filter_map(|s| {
            let p = PathBuf::from(s);
            if !p.is_absolute() {
                return None;
            }
            if no_resolve {
                // Strip trailing slashes for consistent comparison but don't resolve symlinks.
                let s = s.trim_end_matches('/');
                Some(PathBuf::from(s))
            } else {
                // Canonicalize to resolve symlinks; fall back to the raw path
                // (with trailing slashes stripped) when the directory doesn't exist.
                Some(p.canonicalize().unwrap_or_else(|_| {
                    let s = s.trim_end_matches('/');
                    PathBuf::from(s)
                }))
            }
        })
        .collect();
    (paths, no_resolve)
}

/// Validate the repository format version from config text.
/// Returns Ok if the format is supported, Err with message if not.
pub fn validate_repo_config(config_text: &str) -> std::result::Result<(), String> {
    let mut version: u32 = 0;
    let mut in_core = false;
    for line in config_text.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with('[') {
            in_core = trimmed.to_lowercase().starts_with("[core");
            continue;
        }
        if in_core {
            if let Some(rest) = trimmed.strip_prefix("repositoryformatversion") {
                let val = rest.trim_start_matches([' ', '=']).trim();
                if let Ok(v) = val.parse::<u32>() {
                    version = v;
                }
            }
        }
    }
    if version >= 2 {
        return Err(format!("unknown repository format version: {version}"));
    }
    Ok(())
}

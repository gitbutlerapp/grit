//! Per-operation worktree attribute and ignore context.
//!
//! Hot porcelain paths (`status`, staging, index/worktree diffs, checkout stat checks)
//! previously loaded `.gitattributes` and exclude sources independently, often repeating
//! the same disk reads within one operation. [`WorktreeRules`] is built once from a
//! [`Repository`] config snapshot and index view, then threaded through those paths.
//! Attribute files along parent directories and per-directory `.gitignore` files are
//! loaded at most once per [`WorktreeRules`] instance (memoized inside the value, not
//! in process globals). Merged attribute stacks are kept only for the directory chain of
//! the latest lookup, so memory follows path depth rather than the number of directories.

use std::cell::RefCell;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::attributes::{
    parse_gitattributes_file_content, parse_gitattributes_file_content_with_base,
    ParsedGitAttributes, MAX_ATTR_FILE_BYTES,
};
use crate::config::parse_path;
use crate::config::ConfigSet;
use crate::crlf::{self, AttrRule, ConversionConfig, FileAttrs};
use crate::environment::Environment;
use crate::error::{Error, Result};
use crate::filter_process::FilterProcessState;
use crate::ignore::IgnoreMatcher;
use crate::index::Index;
use crate::objects::ObjectId;
use crate::odb::Odb;
use crate::repo::Repository;
use crate::repo_caches::RepoCaches;

/// Counts disk reads of attribute/ignore pattern files (tests only).
///
/// Counts are per thread so tests running concurrently in one process do not see each other's
/// reads. Attribute files are always read on the calling thread ([`WorktreeRules`] is not
/// `Sync`), so an operation's reads land in its caller's counts.
#[cfg(test)]
pub mod file_load_counters {
    use crate::git_path::CanonicalizeExt;
    use std::cell::RefCell;
    use std::collections::HashMap;
    use std::path::{Path, PathBuf};

    thread_local! {
        static MAP: RefCell<HashMap<PathBuf, u32>> = RefCell::new(HashMap::new());
    }

    /// Reset this thread's counters before an operation under test.
    pub fn reset() {
        MAP.with(|m| m.borrow_mut().clear());
    }

    /// Record one read of `path` (canonicalized when possible).
    pub fn record(path: &Path) {
        let key = path
            .canonicalize_simplified()
            .unwrap_or_else(|_| path.to_path_buf());
        MAP.with(|m| *m.borrow_mut().entry(key).or_insert(0) += 1);
    }

    /// Maximum read count for any single file since the last [`reset`].
    #[must_use]
    pub fn max_reads_per_file() -> u32 {
        MAP.with(|m| m.borrow().values().copied().max().unwrap_or(0))
    }

    /// Read count for `path` since the last [`reset`] (canonicalized when possible).
    #[must_use]
    pub fn reads_for(path: &Path) -> u32 {
        let key = path
            .canonicalize_simplified()
            .unwrap_or_else(|_| path.to_path_buf());
        MAP.with(|m| m.borrow().get(&key).copied().unwrap_or(0))
    }
}

fn record_attr_read(path: &Path) {
    #[cfg(test)]
    file_load_counters::record(path);
    let _ = path;
}

fn record_ignore_read(path: &Path) {
    #[cfg(test)]
    file_load_counters::record(path);
    let _ = path;
}

/// Attribute/ignore/conversion state shared across one porcelain operation.
pub struct WorktreeRules {
    config: Arc<ConfigSet>,
    conversion: ConversionConfig,
    ignore: RefCell<IgnoreMatcher>,
    attrs: AttributeState,
    caches: Arc<RepoCaches>,
}

struct AttributeState {
    work_tree: PathBuf,
    _git_dir: PathBuf,
    odb: Odb,
    /// Stage-0 `.gitattributes` paths recorded in the index (path → blob oid).
    gitattributes_index: HashMap<Vec<u8>, ObjectId>,
    /// When true, nested and root `.gitattributes` come only from [`Index`] blobs (checkout smudge).
    gitattributes_index_only: bool,
    /// Global + root `.gitattributes` (lowest precedence among file sources).
    stack_prefix: Arc<ParsedGitAttributes>,
    /// `.git/info/attributes` — applied after all worktree `.gitattributes` along the path.
    info_rules: ParsedGitAttributes,
    /// Parsed `.gitattributes` of each directory that has one (in the worktree or the index).
    /// Directories without one are not recorded, so this stays as small as the set of files.
    dir_fragment: RefCell<HashMap<String, Arc<ParsedGitAttributes>>>,
    /// Attribute stacks for the directory chain of the latest lookup, outermost first.
    /// Callers look paths up in index order, so consecutive lookups share most of the chain.
    dir_stack: RefCell<Vec<DirFrame>>,
    /// [`Self::stack_prefix`] with `info_rules` applied, for paths at the top level.
    root_merged: RefCell<Option<Arc<ParsedGitAttributes>>>,
}

/// One directory on [`AttributeState::dir_stack`].
struct DirFrame {
    dir: String,
    /// Rules from the global file down to this directory's `.gitattributes`. Shared with the
    /// parent frame when this directory adds none.
    stack: Arc<ParsedGitAttributes>,
    /// `stack` with `info/attributes` applied, built on first use.
    merged: Option<Arc<ParsedGitAttributes>>,
}

impl WorktreeRules {
    /// Build rules context from `repo`, using `index` for in-index `.gitattributes` fallbacks.
    ///
    /// # Errors
    ///
    /// Returns I/O or config errors from loading global exclude/attribute sources.
    pub fn from_repository(repo: &Repository, index: &Index) -> Result<Self> {
        let config = repo.config()?.clone();
        Self::from_parts(repo, index, config)
    }

    /// Build rules when the caller already holds a config snapshot.
    ///
    /// # Errors
    ///
    /// Returns I/O errors from loading exclude/attribute files.
    pub fn from_parts(repo: &Repository, index: &Index, config: Arc<ConfigSet>) -> Result<Self> {
        Self::from_parts_with_attribute_source(repo, index, config, false)
    }

    /// Like [`Self::from_parts`], but reads nested `.gitattributes` only from the index snapshot.
    pub(crate) fn from_parts_index_attributes_only(
        repo: &Repository,
        index: &Index,
        config: Arc<ConfigSet>,
    ) -> Result<Self> {
        Self::from_parts_with_attribute_source(repo, index, config, true)
    }

    fn from_parts_with_attribute_source(
        repo: &Repository,
        index: &Index,
        config: Arc<ConfigSet>,
        gitattributes_index_only: bool,
    ) -> Result<Self> {
        let work_tree = repo
            .work_tree()
            .ok_or(Error::RequiresWorkTree)?
            .to_path_buf();
        let conversion = ConversionConfig::from_config_with_runner(&config, repo.command_runner());
        let ignore = RefCell::new(IgnoreMatcher::from_repository(repo)?);
        let (stack_prefix, info_rules) = load_attribute_stack_prefix_and_info(
            repo,
            &work_tree,
            &config,
            index,
            repo.odb(),
            gitattributes_index_only,
        )?;
        let attrs = AttributeState {
            work_tree,
            _git_dir: repo.git_dir().to_path_buf(),
            odb: repo.odb().clone(),
            gitattributes_index: gitattributes_index_map(index),
            gitattributes_index_only,
            stack_prefix: Arc::new(stack_prefix),
            info_rules,
            dir_fragment: RefCell::new(HashMap::new()),
            dir_stack: RefCell::new(Vec::new()),
            root_merged: RefCell::new(None),
        };
        Ok(Self {
            config,
            conversion,
            ignore,
            attrs,
            caches: Arc::clone(repo.caches()),
        })
    }

    /// Build attribute/conversion context for tree checkout using the **destination** index view.
    ///
    /// `.gitattributes` along each path are read from `prospective_index` (and global/info sources),
    /// not from the working tree, so stale on-disk attribute files cannot override the tree being
    /// checked out.
    ///
    /// # Errors
    ///
    /// Returns I/O errors from loading global attribute sources.
    pub fn from_checkout_prospective_index(
        repo: &Repository,
        prospective_index: &Index,
        config: Arc<ConfigSet>,
    ) -> Result<Self> {
        Self::from_parts_index_attributes_only(repo, prospective_index, config)
    }

    /// Long-running filter-process registry for this repository handle.
    #[must_use]
    pub fn filter_process(&self) -> &FilterProcessState {
        self.caches.filters()
    }

    /// Repository config snapshot used to build this context.
    #[must_use]
    pub fn config(&self) -> &ConfigSet {
        &self.config
    }

    /// Config snapshot as `Arc` (for diff options that clone options structs).
    #[must_use]
    pub fn config_arc(&self) -> Arc<ConfigSet> {
        Arc::clone(&self.config)
    }

    /// Line-ending / filter conversion derived from config.
    #[must_use]
    pub fn conversion(&self) -> &ConversionConfig {
        &self.conversion
    }

    /// Ignore engine for untracked/ignored walks (`check-ignore` semantics).
    pub fn ignore(&self) -> std::cell::Ref<'_, IgnoreMatcher> {
        self.ignore.borrow()
    }

    /// Mutable ignore engine (per-directory `.gitignore` loads are memoized inside).
    pub fn ignore_mut(&self) -> std::cell::RefMut<'_, IgnoreMatcher> {
        self.ignore.borrow_mut()
    }

    /// Merged `.gitattributes` rules applying to `rel_path` (parent directories included).
    #[must_use]
    pub fn attribute_rules_for_path(&self, rel_path: &str) -> Vec<AttrRule> {
        let parent = parent_dir_key(rel_path);
        self.attrs.merged_for_dir(&parent).rules.clone()
    }

    /// Resolved attributes for `rel_path` using this context's config snapshot.
    #[must_use]
    pub fn file_attrs(&self, rel_path: &str, is_dir: bool) -> FileAttrs {
        let parent = parent_dir_key(rel_path);
        let parsed = self.attrs.merged_for_dir(&parent);
        crlf::file_attrs_from_gitattributes(
            &parsed.rules,
            &parsed.macros,
            rel_path,
            is_dir,
            &self.config,
        )
    }
}

impl AttributeState {
    /// Every attribute source that applies to paths directly inside `dir` (`""` for the top
    /// level), with `info/attributes` last.
    fn merged_for_dir(&self, dir: &str) -> Arc<ParsedGitAttributes> {
        if dir.is_empty() {
            let mut root = self.root_merged.borrow_mut();
            return Arc::clone(root.get_or_insert_with(|| self.with_info(&self.stack_prefix)));
        }
        self.enter_dir(dir);
        let mut frames = self.dir_stack.borrow_mut();
        let Some(frame) = frames.last_mut() else {
            return self.with_info(&self.stack_prefix);
        };
        Arc::clone(
            frame
                .merged
                .get_or_insert_with(|| self.with_info(&frame.stack)),
        )
    }

    fn with_info(&self, stack: &Arc<ParsedGitAttributes>) -> Arc<ParsedGitAttributes> {
        let info = &self.info_rules;
        if info.rules.is_empty() && info.macros.defs.is_empty() && info.warnings.is_empty() {
            return Arc::clone(stack);
        }
        let mut merged = ParsedGitAttributes::clone(stack);
        append_parsed(&mut merged, info.clone(), true);
        Arc::new(merged)
    }

    /// Make [`Self::dir_stack`] the chain of directories from the top level down to `dir`
    /// (non-empty, `/`-separated), keeping the frames it shares with the previous chain.
    fn enter_dir(&self, dir: &str) {
        let ends: Vec<usize> = dir
            .match_indices('/')
            .map(|(at, _)| at)
            .chain(std::iter::once(dir.len()))
            .collect();
        let mut frames = self.dir_stack.borrow_mut();
        // The frames form one chain, so the deepest frame that matches its prefix of `dir`
        // proves every shallower one matches too.
        let shared = (1..=frames.len().min(ends.len()))
            .rev()
            .find(|&depth| frames[depth - 1].dir == dir[..ends[depth - 1]])
            .unwrap_or(0);
        frames.truncate(shared);
        for &end in &ends[shared..] {
            let prefix = &dir[..end];
            let parent = frames
                .last()
                .map_or(&self.stack_prefix, |frame| &frame.stack);
            let stack = match self.load_dir_fragment(prefix) {
                Some(fragment) if !fragment.rules.is_empty() || !fragment.warnings.is_empty() => {
                    let mut stack = ParsedGitAttributes::clone(parent);
                    append_parsed(&mut stack, ParsedGitAttributes::clone(&fragment), false);
                    Arc::new(stack)
                }
                _ => Arc::clone(parent),
            };
            frames.push(DirFrame {
                dir: prefix.to_owned(),
                stack,
                merged: None,
            });
        }
    }

    /// The parsed `.gitattributes` of `dir`, or `None` when it has none in the worktree or
    /// (as a fallback) the index.
    fn load_dir_fragment(&self, dir: &str) -> Option<Arc<ParsedGitAttributes>> {
        if let Some(hit) = self.dir_fragment.borrow().get(dir) {
            return Some(Arc::clone(hit));
        }
        let mut parsed = ParsedGitAttributes::default();
        let ga_stored = if dir.is_empty() {
            ".gitattributes".to_owned()
        } else {
            format!("{dir}/.gitattributes")
        };
        let key = crate::repo_relpath::storage_bytes(&ga_stored).into_owned();
        if !self.gitattributes_index_only {
            let wt_ga = if dir.is_empty() {
                self.work_tree.join(".gitattributes")
            } else {
                self.work_tree.join(dir).join(".gitattributes")
            };
            if let Some(content) = read_worktree_gitattributes(&wt_ga) {
                if content.len() <= MAX_ATTR_FILE_BYTES {
                    let mut p =
                        parse_gitattributes_file_content_with_base(&content, &ga_stored, dir);
                    parsed.rules.append(&mut p.rules);
                    parsed.warnings.append(&mut p.warnings);
                }
                return Some(self.remember_fragment(dir, parsed));
            }
        }
        let oid = self.gitattributes_index.get(&key)?;
        if let Ok(obj) = self.odb.read(oid) {
            if let Ok(content) = String::from_utf8(obj.data) {
                if content.len() <= MAX_ATTR_FILE_BYTES {
                    let mut p =
                        parse_gitattributes_file_content_with_base(&content, &ga_stored, dir);
                    parsed.rules.append(&mut p.rules);
                    parsed.warnings.append(&mut p.warnings);
                }
            }
        }
        Some(self.remember_fragment(dir, parsed))
    }

    fn remember_fragment(
        &self,
        dir: &str,
        parsed: ParsedGitAttributes,
    ) -> Arc<ParsedGitAttributes> {
        let parsed = Arc::new(parsed);
        self.dir_fragment
            .borrow_mut()
            .insert(dir.to_owned(), Arc::clone(&parsed));
        parsed
    }
}

fn append_parsed(
    merged: &mut ParsedGitAttributes,
    mut parsed: ParsedGitAttributes,
    take_macros: bool,
) {
    merged.rules.append(&mut parsed.rules);
    if take_macros {
        merged.macros.defs.extend(parsed.macros.defs.drain());
    }
    merged.warnings.append(&mut parsed.warnings);
}

fn parent_dir_key(rel_path: &str) -> String {
    Path::new(rel_path)
        .parent()
        .map(|p| p.to_string_lossy().replace('\\', "/"))
        .filter(|s| !s.is_empty())
        .unwrap_or_default()
}

fn load_attribute_stack_prefix_and_info(
    repo: &Repository,
    work_tree: &Path,
    config: &ConfigSet,
    index: &Index,
    odb: &Odb,
    gitattributes_index_only: bool,
) -> Result<(ParsedGitAttributes, ParsedGitAttributes)> {
    let mut stack_prefix = ParsedGitAttributes::default();
    if let Some(g) = global_attributes_path(repo.environment(), config)? {
        if let Some(content) = read_worktree_gitattributes(&g) {
            if content.len() <= MAX_ATTR_FILE_BYTES {
                let p = parse_gitattributes_file_content(&content, g.to_string_lossy().as_ref());
                append_parsed(&mut stack_prefix, p, true);
            }
        }
    }
    if gitattributes_index_only {
        if let Some(entry) = index.get(b".gitattributes", 0) {
            if let Ok(obj) = odb.read(&entry.oid) {
                if let Ok(content) = String::from_utf8(obj.data) {
                    if content.len() <= MAX_ATTR_FILE_BYTES {
                        let p = parse_gitattributes_file_content(&content, ".gitattributes");
                        append_parsed(&mut stack_prefix, p, true);
                    }
                }
            }
        }
    } else {
        let root_ga = work_tree.join(".gitattributes");
        if let Some(content) = read_worktree_gitattributes(&root_ga) {
            if content.len() <= MAX_ATTR_FILE_BYTES {
                let p = parse_gitattributes_file_content(&content, ".gitattributes");
                append_parsed(&mut stack_prefix, p, true);
            }
        } else if let Some(entry) = index.get(b".gitattributes", 0) {
            if let Ok(obj) = odb.read(&entry.oid) {
                if let Ok(content) = String::from_utf8(obj.data) {
                    if content.len() <= MAX_ATTR_FILE_BYTES {
                        let p = parse_gitattributes_file_content(&content, ".gitattributes");
                        append_parsed(&mut stack_prefix, p, true);
                    }
                }
            }
        }
    }
    let mut info_rules = ParsedGitAttributes::default();
    let info = repo.git_dir().join("info/attributes");
    if let Some(content) = read_worktree_gitattributes(&info) {
        if content.len() <= MAX_ATTR_FILE_BYTES {
            let p = parse_gitattributes_file_content(&content, "info/attributes");
            append_parsed(&mut info_rules, p, true);
        }
    }
    Ok((stack_prefix, info_rules))
}

fn global_attributes_path(env: &Environment, config: &ConfigSet) -> Result<Option<PathBuf>> {
    if let Some(path) = config.get("core.attributesfile") {
        return Ok(Some(PathBuf::from(parse_path(env, &path))));
    }
    Ok(env.default_xdg_config_git_file("attributes"))
}

fn read_worktree_gitattributes(path: &Path) -> Option<String> {
    let meta = std::fs::symlink_metadata(path).ok()?;
    if meta.file_type().is_symlink() {
        return None;
    }
    record_attr_read(path);
    std::fs::read_to_string(path).ok()
}

fn gitattributes_index_map(index: &Index) -> HashMap<Vec<u8>, ObjectId> {
    index
        .entries()
        .iter()
        .filter(|e| e.stage() == 0 && is_gitattributes_index_path(&e.path))
        .map(|e| (e.path.clone(), e.oid))
        .collect()
}

fn is_gitattributes_index_path(path: &[u8]) -> bool {
    path == b".gitattributes" || path.ends_with(b"/.gitattributes")
}

/// Hook for [`crate::ignore`] to record pattern-file reads (memoization tests).
pub(crate) fn record_ignore_pattern_file_read(path: &Path) {
    record_ignore_read(path);
}

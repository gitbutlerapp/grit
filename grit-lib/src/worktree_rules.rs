//! Per-operation worktree attribute and ignore context.
//!
//! Hot porcelain paths (`status`, staging, index/worktree diffs, checkout stat checks)
//! previously loaded `.gitattributes` and exclude sources independently, often repeating
//! the same disk reads within one operation. [`WorktreeRules`] is built once from a
//! [`Repository`] config snapshot and index view, then threaded through those paths.
//! Attribute files along parent directories and per-directory `.gitignore` files are
//! loaded at most once per [`WorktreeRules`] instance (memoized inside the value, not
//! in process globals).

use std::cell::RefCell;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::config::parse_path;
use crate::config::ConfigSet;
use crate::crlf::{self, AttrRule, ConversionConfig, FileAttrs};
use crate::error::{Error, Result};
use crate::ignore::IgnoreMatcher;
use crate::index::Index;
use crate::odb::Odb;
use crate::repo::Repository;

/// Counts disk reads of attribute/ignore pattern files (tests only).
pub mod file_load_counters {
    use std::collections::HashMap;
    use std::path::{Path, PathBuf};
    use std::sync::{Mutex, OnceLock};

    fn map() -> &'static Mutex<HashMap<PathBuf, u32>> {
        static MAP: OnceLock<Mutex<HashMap<PathBuf, u32>>> = OnceLock::new();
        MAP.get_or_init(|| Mutex::new(HashMap::new()))
    }

    /// Reset counters before an operation under test.
    pub fn reset() {
        if let Ok(mut m) = map().lock() {
            m.clear();
        }
    }

    /// Record one read of `path` (canonicalized when possible).
    pub fn record(path: &Path) {
        let key = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
        if let Ok(mut m) = map().lock() {
            *m.entry(key).or_insert(0) += 1;
        }
    }

    /// Maximum read count for any single file since the last [`reset`].
    #[must_use]
    pub fn max_reads_per_file() -> u32 {
        map()
            .lock()
            .map(|m| m.values().copied().max().unwrap_or(0))
            .unwrap_or(0)
    }

    /// Total distinct files read since the last [`reset`].
    #[must_use]
    pub fn distinct_files_read() -> usize {
        map().lock().map(|m| m.len()).unwrap_or(0)
    }

    /// Read count for `path` since the last [`reset`] (canonicalized when possible).
    #[must_use]
    pub fn reads_for(path: &Path) -> u32 {
        let key = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
        map()
            .lock()
            .map(|m| m.get(&key).copied().unwrap_or(0))
            .unwrap_or(0)
    }
}

fn record_attr_read(path: &Path) {
    file_load_counters::record(path);
}

fn record_ignore_read(path: &Path) {
    file_load_counters::record(path);
}

/// Attribute/ignore/conversion state shared across one porcelain operation.
pub struct WorktreeRules {
    config: Arc<ConfigSet>,
    conversion: ConversionConfig,
    ignore: RefCell<IgnoreMatcher>,
    attrs: AttributeState,
}

struct AttributeState {
    work_tree: PathBuf,
    _git_dir: PathBuf,
    odb: Odb,
    index: Index,
    /// Global + root `.gitattributes` (lowest precedence among file sources).
    stack_prefix: Vec<AttrRule>,
    /// `.git/info/attributes` — applied after all worktree `.gitattributes` along the path.
    info_rules: Vec<AttrRule>,
    dir_fragment: RefCell<HashMap<String, Vec<AttrRule>>>,
    stack_without_info_by_dir: RefCell<HashMap<String, Vec<AttrRule>>>,
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
        let work_tree = repo
            .work_tree
            .clone()
            .ok_or_else(|| Error::Message("this operation must be run in a work tree".into()))?;
        let conversion = ConversionConfig::from_config(&config);
        let ignore = RefCell::new(IgnoreMatcher::from_repository(repo)?);
        let (stack_prefix, info_rules) =
            load_attribute_stack_prefix_and_info(repo, &work_tree, &config, index, &repo.odb)?;
        let attrs = AttributeState {
            work_tree,
            _git_dir: repo.git_dir.clone(),
            odb: repo.odb.clone(),
            index: index.clone(),
            stack_prefix,
            info_rules,
            dir_fragment: RefCell::new(HashMap::new()),
            stack_without_info_by_dir: RefCell::new(HashMap::new()),
        };
        Ok(Self {
            config,
            conversion,
            ignore,
            attrs,
        })
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
        self.attrs.merged_for_dir(&parent)
    }

    /// Resolved attributes for `rel_path` using this context's config snapshot.
    #[must_use]
    pub fn file_attrs(&self, rel_path: &str, is_dir: bool) -> FileAttrs {
        let rules = self.attribute_rules_for_path(rel_path);
        crlf::get_file_attrs(&rules, rel_path, is_dir, &self.config)
    }

    /// Global, root, and info attribute rules (no nested directories).
    #[must_use]
    pub fn root_attribute_rules(&self) -> Vec<AttrRule> {
        let mut rules = self.attrs.stack_prefix.clone();
        rules.extend_from_slice(&self.attrs.info_rules);
        rules
    }
}

impl AttributeState {
    fn merged_for_dir(&self, dir: &str) -> Vec<AttrRule> {
        let mut merged = self.stack_without_info_for_dir(dir);
        merged.extend_from_slice(&self.info_rules);
        merged
    }

    fn stack_without_info_for_dir(&self, dir: &str) -> Vec<AttrRule> {
        if let Some(hit) = self.stack_without_info_by_dir.borrow().get(dir).cloned() {
            return hit;
        }
        let stack = if dir.is_empty() {
            self.stack_prefix.clone()
        } else {
            let parent = parent_of_dir(dir);
            let mut rules = self.stack_without_info_for_dir(&parent);
            rules.extend(self.load_dir_fragment(dir));
            rules
        };
        self.stack_without_info_by_dir
            .borrow_mut()
            .insert(dir.to_owned(), stack.clone());
        stack
    }

    fn load_dir_fragment(&self, dir: &str) -> Vec<AttrRule> {
        if let Some(hit) = self.dir_fragment.borrow().get(dir).cloned() {
            return hit;
        }
        let mut rules = Vec::new();
        let ga_rel = if dir.is_empty() {
            PathBuf::from(".gitattributes")
        } else {
            Path::new(dir).join(".gitattributes")
        };
        let wt_ga = self.work_tree.join(&ga_rel);
        if let Some(content) = read_worktree_gitattributes(&wt_ga) {
            rules.extend(crlf::parse_gitattributes_content(&content));
        } else {
            let key = path_to_index_bytes(&ga_rel);
            if let Some(entry) = self.index.get(&key, 0) {
                if let Ok(obj) = self.odb.read(&entry.oid) {
                    if let Ok(content) = String::from_utf8(obj.data) {
                        rules.extend(crlf::parse_gitattributes_content(&content));
                    }
                }
            }
        }
        self.dir_fragment
            .borrow_mut()
            .insert(dir.to_owned(), rules.clone());
        rules
    }
}

fn parent_dir_key(rel_path: &str) -> String {
    Path::new(rel_path)
        .parent()
        .map(|p| p.to_string_lossy().replace('\\', "/"))
        .filter(|s| !s.is_empty())
        .unwrap_or_default()
}

fn parent_of_dir(dir: &str) -> String {
    Path::new(dir)
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
) -> Result<(Vec<AttrRule>, Vec<AttrRule>)> {
    let mut stack_prefix = Vec::new();
    if let Some(g) = global_attributes_path(config)? {
        if let Some(content) = read_worktree_gitattributes(&g) {
            stack_prefix.extend(crlf::parse_gitattributes_content(&content));
        }
    }
    let root_ga = work_tree.join(".gitattributes");
    if let Some(content) = read_worktree_gitattributes(&root_ga) {
        stack_prefix.extend(crlf::parse_gitattributes_content(&content));
    } else if let Some(entry) = index.get(b".gitattributes", 0) {
        if let Ok(obj) = odb.read(&entry.oid) {
            if let Ok(content) = String::from_utf8(obj.data) {
                stack_prefix.extend(crlf::parse_gitattributes_content(&content));
            }
        }
    }
    let mut info_rules = Vec::new();
    let info = repo.git_dir.join("info/attributes");
    if let Some(content) = read_worktree_gitattributes(&info) {
        info_rules.extend(crlf::parse_gitattributes_content(&content));
    }
    Ok((stack_prefix, info_rules))
}

fn global_attributes_path(config: &ConfigSet) -> Result<Option<PathBuf>> {
    if let Some(path) = config.get("core.attributesfile") {
        return Ok(Some(PathBuf::from(parse_path(&path))));
    }
    let home = std::env::var("HOME").ok();
    let Some(home) = home else {
        return Ok(None);
    };
    if let Ok(xdg) = std::env::var("XDG_CONFIG_HOME") {
        if !xdg.is_empty() {
            return Ok(Some(PathBuf::from(xdg).join("git/attributes")));
        }
    }
    Ok(Some(PathBuf::from(home).join(".config/git/attributes")))
}

fn read_worktree_gitattributes(path: &Path) -> Option<String> {
    let meta = std::fs::symlink_metadata(path).ok()?;
    if meta.file_type().is_symlink() {
        return None;
    }
    record_attr_read(path);
    std::fs::read_to_string(path).ok()
}

fn path_to_index_bytes(path: &Path) -> Vec<u8> {
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStrExt;
        path.as_os_str().as_bytes().to_vec()
    }
    #[cfg(not(unix))]
    {
        path.to_string_lossy().into_owned().into_bytes()
    }
}

/// Hook for [`crate::ignore`] to record pattern-file reads (memoization tests).
pub(crate) fn record_ignore_pattern_file_read(path: &Path) {
    record_ignore_read(path);
}

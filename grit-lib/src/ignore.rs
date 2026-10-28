//! Ignore and exclude matching for `check-ignore`.
//!
//! This module implements a focused subset of Git ignore behavior:
//! per-directory `.gitignore`, `.git/info/exclude`, and `core.excludesfile`
//! with "last matching pattern wins" precedence. With `core.ignorecase=true`, patterns match
//! without regard to ASCII case, as in Git.

use std::collections::HashMap;
use std::fmt;
use std::fs;
use std::path::{Component, Path, PathBuf};

use crate::config::parse_path;
use crate::error::{Error, Result};
use crate::index::{Index, MODE_GITLINK};
use crate::objects::ObjectKind;
use crate::repo::Repository;
use crate::wildmatch::{wildmatch, WM_CASEFOLD, WM_PATHNAME};

/// Where an ignore pattern was loaded from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IgnoreSource {
    /// A per-directory exclude file (typically `.gitignore`).
    PerDirectory(String),
    /// Patterns from `.git/info/exclude`.
    InfoExclude,
    /// Patterns from `core.excludesfile` (or the XDG default).
    CoreExcludesFile(String),
}

impl fmt::Display for IgnoreSource {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::PerDirectory(path) => f.write_str(path),
            Self::InfoExclude => f.write_str(".git/info/exclude"),
            Self::CoreExcludesFile(path) => f.write_str(path),
        }
    }
}

/// Metadata for a matching rule.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IgnoreMatch {
    /// Typed source of the matching pattern (see [`IgnoreSource`]).
    pub source: IgnoreSource,
    /// Line number in the source file (1-based).
    pub line_number: usize,
    /// Pattern text as written (excluding comments/blank lines).
    pub pattern_text: String,
    /// Whether this is a negated pattern (`!pattern`).
    pub negative: bool,
}

/// Result of checking one path against repository ignore rules.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IgnoreCheck {
    /// Repository-relative path that was queried.
    pub path: String,
    /// Whether the path is ignored (always `false` for tracked paths).
    pub ignored: bool,
    /// Last matching pattern metadata, when Git would attribute one (`check-ignore -v`).
    pub matched: Option<IgnoreMatch>,
}

/// Patterns parsed from one exclude file (such as a `.gitignore`), for matching single paths.
///
/// Unlike [`IgnoreMatcher`], a list is not tied to a repository: the caller decides which
/// files apply to a path and in what order, as tools that layer their own ignore handling on
/// top of Git's file format need to. Matching follows Git's `last_matching_pattern_from_list`
/// for one path: patterns without a slash match the path's last component, other patterns match
/// the whole path relative to the list's base directory, and directory-only patterns (`foo/`)
/// match only when the path is a directory. Parent directories are not consulted; callers that
/// walk a tree stop descending into ignored directories, as Git does.
///
/// # Examples
///
/// ```
/// use grit_lib::worktree::ignore::IgnorePatternList;
///
/// let list = IgnorePatternList::parse(b"*.log\n!keep.log\nbuild/\n", "");
/// assert!(list.is_ignored("debug.log", false, false).unwrap());
/// assert!(!list.is_ignored("keep.log", false, false).unwrap());
/// assert!(list.is_ignored("build", true, false).unwrap());
/// assert_eq!(list.is_ignored("build", false, false), None);
/// ```
#[derive(Debug, Clone, Default)]
pub struct IgnorePatternList {
    rules: Vec<IgnoreRule>,
}

/// The pattern that decided an [`IgnorePatternList::last_match`] query.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IgnorePatternMatch {
    /// Line number of the pattern in the parsed content (1-based).
    pub line_number: usize,
    /// Pattern text as written (without trailing whitespace).
    pub pattern_text: String,
    /// Whether this is a negated pattern (`!pattern`), which re-includes the path.
    pub negative: bool,
}

impl IgnorePatternList {
    /// Parse exclude-file `content` whose patterns are relative to `base_dir`.
    ///
    /// `base_dir` is the slash-separated directory holding the file, relative to the root
    /// paths are matched against (`""` for the root). Blank lines and comments are skipped,
    /// a UTF-8 byte order mark on the first line is dropped, and invalid UTF-8 is replaced
    /// as in [`String::from_utf8_lossy`].
    #[must_use]
    pub fn parse(content: &[u8], base_dir: &str) -> Self {
        let content = String::from_utf8_lossy(content);
        let base_dir = base_dir.trim_matches('/');
        let source = IgnoreSource::PerDirectory(if base_dir.is_empty() {
            ".gitignore".to_owned()
        } else {
            format!("{base_dir}/.gitignore")
        });
        let mut warnings = Vec::new();
        let rules =
            parse_gitignore_content(&content, source, base_dir, &mut warnings).unwrap_or_default();
        Self { rules }
    }

    /// Whether the list has no patterns.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.rules.is_empty()
    }

    /// Number of patterns in the list.
    #[must_use]
    pub fn len(&self) -> usize {
        self.rules.len()
    }

    /// The last pattern matching `path`, if any.
    ///
    /// `path` is slash-separated and relative to the root the list's base directory is
    /// relative to. Paths outside the base directory never match. With `ignore_case`,
    /// patterns compare without regard to ASCII case (`core.ignorecase`).
    #[must_use]
    pub fn last_match(
        &self,
        path: &str,
        is_dir: bool,
        ignore_case: bool,
    ) -> Option<IgnorePatternMatch> {
        self.rules
            .iter()
            .rev()
            .find(|rule| rule_matches(rule, path, is_dir, ignore_case))
            .map(|rule| IgnorePatternMatch {
                line_number: rule.line_number,
                pattern_text: rule.pattern_text.clone(),
                negative: rule.negative,
            })
    }

    /// Whether the last matching pattern ignores `path` (`Some(false)` when it re-includes
    /// it), or `None` when no pattern matches. See [`Self::last_match`].
    #[must_use]
    pub fn is_ignored(&self, path: &str, is_dir: bool, ignore_case: bool) -> Option<bool> {
        self.last_match(path, is_dir, ignore_case)
            .map(|m| !m.negative)
    }
}

#[derive(Debug, Clone)]
enum IgnorePatternKind {
    Wild,
    Literal(String),
    Prefix(String),
    Suffix(String),
}

#[derive(Debug, Clone)]
struct IgnoreRule {
    source: IgnoreSource,
    line_number: usize,
    pattern_text: String,
    negative: bool,
    directory_only: bool,
    anchored: bool,
    has_slash: bool,
    body: String,
    base_dir: String,
    match_kind: IgnorePatternKind,
}

/// Engine used to evaluate ignore patterns against repository-relative paths.
#[derive(Debug, Default, Clone)]
pub struct IgnoreMatcher {
    /// Patterns from `git ls-files -x` / `--exclude` (Git `EXC_CMDL`), evaluated first.
    cli_rules: Vec<IgnoreRule>,
    global_rules: Vec<IgnoreRule>,
    info_rules: Vec<IgnoreRule>,
    /// Patterns from `git ls-files -X` / `--exclude-from` (Git `EXC_FILE`, after global/info).
    exclude_from_rules: Vec<IgnoreRule>,
    gitignore_cache: HashMap<String, Vec<IgnoreRule>>,
    /// Per-directory exclude filename (`ls-files --exclude-per-directory`, default `.gitignore`).
    per_directory_name: Option<String>,
    /// Whether per-directory exclude files are read at all (Git `EXC_DIRS`).
    ///
    /// `from_repository` (standard excludes) and an explicit `--exclude-per-directory`/`--ignored`
    /// enable this; a matcher built only for `-x`/`-X` does not consult per-directory files.
    read_per_directory: bool,
    /// Warnings emitted while loading in-tree `.gitignore` (e.g. symlink paths).
    pub warnings: Vec<String>,
    /// Cached ancestor directory list for the last [`Self::ensure_per_dir_rules_loaded`] parent.
    per_dir_dirs_cache: Option<(String, Vec<String>)>,
    /// Per-directory ancestor exclude stacks reused across sibling checks under the same prefix.
    dir_exclude_stack_cache: HashMap<String, Vec<String>>,
    /// Fold ASCII case when comparing patterns and base directories (`core.ignorecase`).
    ignore_case: bool,
    /// Repository-relative directories entered during a depth-first walk (skip restat in EXC_DIRS).
    known_worktree_directories: std::collections::HashSet<String>,
}

impl IgnoreMatcher {
    /// Build a matcher from repository exclude sources.
    ///
    /// Honors `core.ignorecase`: when it is `true`, patterns match paths without regard to
    /// ASCII case (`Build/` ignores `build/`, `*.LOG` ignores `debug.log`), as Git does.
    ///
    /// # Parameters
    ///
    /// - `repo` - open repository.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Io`] if configured pattern files cannot be read, or a config error if
    /// the repository configuration cannot be loaded.
    pub fn from_repository(repo: &Repository) -> Result<Self> {
        let ignore_case = crate::crlf::config_ignorecase(repo.config()?.as_ref());
        Ok(Self {
            global_rules: load_global_excludes(repo)?,
            info_rules: load_info_excludes(repo)?,
            // Standard excludes include the per-directory `.gitignore` set (Git `EXC_DIRS`).
            read_per_directory: true,
            ignore_case,
            ..Self::default()
        })
    }

    /// Set whether patterns match case-insensitively (Git's `core.ignorecase`).
    ///
    /// [`Self::from_repository`] reads this from configuration; use this to override it, or to
    /// opt in on a matcher built with [`Default::default`]. Only ASCII letters are folded, like
    /// Git.
    ///
    /// # Parameters
    ///
    /// - `ignore_case` - `true` to fold ASCII case when matching.
    ///
    /// # Returns
    ///
    /// The matcher with the setting applied.
    #[must_use]
    pub fn with_ignore_case(mut self, ignore_case: bool) -> Self {
        self.ignore_case = ignore_case;
        self
    }

    /// Whether patterns match case-insensitively (see [`Self::with_ignore_case`]).
    #[must_use]
    pub fn ignore_case(&self) -> bool {
        self.ignore_case
    }

    /// Append patterns from `ls-files --exclude-from` / `-X` files (relative paths resolve from `cwd`).
    ///
    /// Matches Git's `EXC_FILE` lists loaded after `core.excludesfile` and `.git/info/exclude`.
    pub fn add_exclude_from_files(&mut self, paths: &[PathBuf], cwd: &Path) -> Result<()> {
        for path in paths {
            let resolved = if path.is_absolute() {
                path.clone()
            } else {
                cwd.join(path)
            };
            let display = path.display().to_string();
            let mut more = load_rules_from_file(
                &resolved,
                IgnoreSource::CoreExcludesFile(display),
                String::new(),
                false,
                &mut self.warnings,
            )?;
            self.exclude_from_rules.append(&mut more);
        }
        Ok(())
    }

    /// Override the per-directory exclude filename (`ls-files --exclude-per-directory=<file>`).
    ///
    /// When unset, per-directory excludes are read from `.gitignore` (Git's standard name).
    pub fn set_per_directory_name(&mut self, name: &str) {
        self.per_directory_name = Some(name.to_owned());
        self.read_per_directory = true;
        // The cache is keyed by directory only; clear it so a new filename is honored.
        self.gitignore_cache.clear();
        self.per_dir_dirs_cache = None;
        self.dir_exclude_stack_cache.clear();
    }

    /// Append patterns from `ls-files --exclude` / `-x` (Git command-line exclude list).
    pub fn add_cli_excludes(&mut self, patterns: &[String]) {
        for pat in patterns {
            if let Some(rule) = parse_rule_line(
                pat,
                1,
                IgnoreSource::PerDirectory("--exclude option".into()),
                "",
            ) {
                self.cli_rules.push(rule);
            }
        }
    }

    /// Take any warnings accumulated while loading ignore files (caller prints to stderr).
    #[must_use]
    pub fn take_warnings(&mut self) -> Vec<String> {
        std::mem::take(&mut self.warnings)
    }

    /// Whether `repo_rel_path` is ignored, without computing verbose `-v` match metadata.
    ///
    /// Prefer this on hot paths (for example untracked directory walks) that only need the
    /// boolean result.
    pub fn is_path_ignored(
        &mut self,
        repo: &Repository,
        index: Option<&Index>,
        repo_rel_path: &str,
        is_dir: bool,
    ) -> Result<bool> {
        if is_tracked(index, repo_rel_path) {
            return Ok(false);
        }
        let per_dir_dirs = self.ensure_per_dir_rules_loaded(repo, index, repo_rel_path)?;
        let per_dir_groups = self.cached_rule_slices_for_dirs(&per_dir_dirs);
        if !self.has_any_exclude_rules(&per_dir_groups) {
            return Ok(false);
        }
        if self
            .first_excluded_ancestor(repo_rel_path, &per_dir_groups)
            .is_some()
        {
            return Ok(true);
        }
        Ok(self
            .evaluate_against_groups(repo_rel_path, is_dir, &per_dir_groups)
            .0)
    }

    /// Check whether a repository-relative path is ignored.
    ///
    /// # Parameters
    ///
    /// - `repo` - repository handle.
    /// - `index` - optional index; when present, tracked entries are not ignored. The lookup is
    ///   exact-case even under `core.ignorecase`, like `git check-ignore`; work-tree walks that
    ///   must treat case variants of tracked paths as tracked check that before asking.
    /// - `repo_rel_path` - normalized repository-relative path with `/` separators.
    /// - `is_dir` - whether the queried path is a directory.
    ///
    /// # Returns
    ///
    /// Tuple `(ignored, match_info)` where `match_info` is the last matching
    /// pattern (including negated matches).
    ///
    /// # Errors
    ///
    /// Returns [`Error::Io`] when a relevant `.gitignore` cannot be read.
    pub fn check_path(
        &mut self,
        repo: &Repository,
        index: Option<&Index>,
        repo_rel_path: &str,
        is_dir: bool,
    ) -> Result<(bool, Option<IgnoreMatch>)> {
        if is_tracked(index, repo_rel_path) {
            return Ok((false, None));
        }

        // Git evaluates exclude pattern groups in priority order
        // (`last_matching_pattern_from_lists`, dir.c):
        //   1. EXC_CMDL  — command-line `-x`/`--exclude` patterns
        //   2. EXC_DIRS  — per-directory `.gitignore` (deepest directory wins)
        //   3. EXC_FILE  — `-X`/`--exclude-from`, then `core.excludesfile`, then `.git/info/exclude`
        // The FIRST group that yields a match decides the result; within a group, the last
        // matching pattern (last line) wins. This means a CLI `--exclude` overrides a per-directory
        // `!negation`, and a per-directory `.gitignore` overrides `core.excludesfile`.
        let per_dir_dirs = self.ensure_per_dir_rules_loaded(repo, index, repo_rel_path)?;
        let per_dir_groups = self.cached_rule_slices_for_dirs(&per_dir_dirs);

        // Git's `prep_exclude` (dir.c) descends into the path's ancestor directories before
        // matching the leaf. If an ancestor directory itself matches a non-negative exclude
        // pattern, the entire subtree is excluded and the walk stops: a later `!one/a.1`
        // negation can never re-include a file once its parent directory `one` is excluded
        // ("It is not possible to re-include a file if a parent directory of that file is
        // excluded", gitignore(5)). When checking ancestor `a/b`, only the per-directory
        // `.gitignore` files strictly above it are loaded yet, so we slice `per_dir_groups`
        // to the ancestor's own depth.
        if let Some(ancestor_match) = self.first_excluded_ancestor(repo_rel_path, &per_dir_groups) {
            return Ok((true, Some(ancestor_match)));
        }

        let (ignored, matched) =
            self.evaluate_against_groups(repo_rel_path, is_dir, &per_dir_groups);

        // For `check-ignore -v` attribution, the verbose refinement walks the full flattened set.
        let all_rules: Vec<&IgnoreRule> = self.flatten_groups(&per_dir_groups);
        let matched = refine_match_for_check_ignore_verbose(
            repo_rel_path,
            is_dir,
            ignored,
            matched,
            &all_rules,
            self.ignore_case,
        );

        Ok((ignored, matched))
    }

    /// Build the ordered group list (EXC_CMDL, EXC_DIRS deepest-first, EXC_FILE) for a given
    /// set of per-directory rule groups, then run Git's first-group-wins / last-pattern-wins
    /// evaluation against `path`.
    ///
    /// `per_dir_groups` is the root-first slice of `.gitignore` rule lists that should be
    /// considered for this query: callers pass the leaf's full slice for the path itself, or a
    /// shorter prefix slice when probing an ancestor directory (matching `prep_exclude`'s
    /// incremental loading order).
    fn evaluate_against_groups(
        &self,
        path: &str,
        is_dir: bool,
        per_dir_groups: &[&[IgnoreRule]],
    ) -> (bool, Option<IgnoreMatch>) {
        let groups = self.build_groups(per_dir_groups);
        for group in &groups {
            let mut group_match: Option<&&IgnoreRule> = None;
            for rule in group {
                if rule_matches(rule, path, is_dir, self.ignore_case) {
                    group_match = Some(rule);
                }
            }
            if let Some(rule) = group_match {
                return (
                    !rule.negative,
                    Some(IgnoreMatch {
                        source: rule.source.clone(),
                        line_number: rule.line_number,
                        pattern_text: rule.pattern_text.clone(),
                        negative: rule.negative,
                    }),
                );
            }
        }
        (false, None)
    }

    /// Flatten the ordered group list into a single rule slice (used for `check-ignore -v`
    /// attribution, which walks every loaded pattern).
    fn flatten_groups<'a>(&'a self, per_dir_groups: &[&'a [IgnoreRule]]) -> Vec<&'a IgnoreRule> {
        self.build_groups(per_dir_groups)
            .into_iter()
            .flatten()
            .collect()
    }

    /// Assemble the priority-ordered exclude groups: EXC_CMDL, then per-directory `.gitignore`
    /// (deepest directory first), then EXC_FILE (`--exclude-from`, global, info).
    fn build_groups<'a>(&'a self, per_dir_groups: &[&'a [IgnoreRule]]) -> Vec<Vec<&'a IgnoreRule>> {
        let cmdl_rules: Vec<&IgnoreRule> = self.cli_rules.iter().collect();
        let mut dir_groups: Vec<Vec<&IgnoreRule>> = Vec::new();
        for rules in per_dir_groups.iter().rev() {
            dir_groups.push(rules.iter().collect());
        }
        let file_rules: Vec<&IgnoreRule> = self
            .exclude_from_rules
            .iter()
            .chain(self.global_rules.iter())
            .chain(self.info_rules.iter())
            .collect();

        let mut groups: Vec<Vec<&IgnoreRule>> = Vec::new();
        groups.push(cmdl_rules);
        groups.extend(dir_groups);
        groups.push(file_rules);
        groups
    }

    /// Walk the ancestor directories of `repo_rel_path` from the top down. Returns the matching
    /// pattern of the first ancestor directory that is positively excluded (Git's `prep_exclude`
    /// abort), or `None` if no ancestor directory is excluded.
    ///
    /// `per_dir_groups` is the root-first list of per-directory rule lists for the leaf path;
    /// element `i` corresponds to the ancestor directory at depth `i`. When probing the ancestor
    /// directory at depth `d`, only groups `[0..d]` are loaded (the directory's own `.gitignore`
    /// has not been read yet), matching Git's incremental load order.
    fn first_excluded_ancestor(
        &self,
        repo_rel_path: &str,
        per_dir_groups: &[&[IgnoreRule]],
    ) -> Option<IgnoreMatch> {
        let parent = parent_dir(repo_rel_path);
        if parent.is_empty() {
            return None;
        }
        let mut cur = String::new();
        for (depth, segment) in parent.split('/').enumerate() {
            if !cur.is_empty() {
                cur.push('/');
            }
            cur.push_str(segment);
            // Patterns loaded before descending into this directory: per-directory groups for
            // strictly-shallower directories (root..=depth). `per_dir_groups[0]` is the root
            // `.gitignore`, so the ancestor at depth `depth` (0-based) sees groups `[0..=depth]`.
            let upper = (depth + 1).min(per_dir_groups.len());
            let slice = &per_dir_groups[..upper];
            let (ignored, matched) = self.evaluate_against_groups(&cur, true, slice);
            if ignored {
                return matched;
            }
        }
        None
    }

    /// Per-directory `.gitignore` rules for the ancestors of `repo_rel_path`, grouped by
    /// directory and ordered root-first (deepest directory last). The caller evaluates the
    /// groups deepest-first to honor Git's `EXC_DIRS` precedence (a nested `.gitignore` wins
    /// over an ancestor's, and over `EXC_FILE` sources).
    fn has_any_exclude_rules(&self, per_dir_groups: &[&[IgnoreRule]]) -> bool {
        !self.cli_rules.is_empty()
            || !self.exclude_from_rules.is_empty()
            || !self.global_rules.is_empty()
            || !self.info_rules.is_empty()
            || per_dir_groups.iter().any(|g| !g.is_empty())
    }

    /// Load per-directory exclude files for `dir_rel` when depth-first walking the work tree.
    ///
    /// Idempotent: repeated calls for the same directory reuse the in-memory cache.
    pub fn note_directory_entered(
        &mut self,
        repo: &Repository,
        index: Option<&Index>,
        dir_rel: &str,
    ) -> Result<()> {
        if !self.read_per_directory {
            return Ok(());
        }
        if self.gitignore_cache.contains_key(dir_rel) {
            return Ok(());
        }
        self.known_worktree_directories.insert(dir_rel.to_owned());
        let per_dir_name = self.per_directory_name.clone();
        let rules = load_gitignore_for_dir(
            repo,
            index,
            dir_rel,
            per_dir_name.as_deref(),
            &mut self.warnings,
        )?;
        self.gitignore_cache.insert(dir_rel.to_owned(), rules);
        Ok(())
    }

    fn ensure_per_dir_rules_loaded(
        &mut self,
        repo: &Repository,
        index: Option<&Index>,
        repo_rel_path: &str,
    ) -> Result<Vec<String>> {
        if !self.read_per_directory {
            return Ok(Vec::new());
        }
        let parent = parent_dir(repo_rel_path);
        let dirs = match self.per_dir_dirs_cache.as_ref() {
            Some((cached, cached_dirs)) if cached == parent => cached_dirs.clone(),
            _ => {
                if let Some(cached_dirs) = self.dir_exclude_stack_cache.get(parent) {
                    self.per_dir_dirs_cache = Some((parent.to_string(), cached_dirs.clone()));
                    cached_dirs.clone()
                } else {
                    let mut dirs = Vec::new();
                    dirs.push(String::new());
                    if !parent.is_empty() {
                        let mut cur = String::new();
                        for segment in parent.split('/') {
                            if !cur.is_empty() {
                                cur.push('/');
                            }
                            cur.push_str(segment);
                            if self.known_worktree_directories.contains(&cur) {
                                dirs.push(cur.clone());
                            } else if worktree_path_is_existing_file(repo, &cur) {
                                // Git does not treat a file path as a directory for EXC_DIRS
                                // loading (`tracked.txt/.gitignore` must not be probed).
                                break;
                            } else {
                                dirs.push(cur.clone());
                            }
                        }
                    }
                    self.per_dir_dirs_cache = Some((parent.to_string(), dirs.clone()));
                    self.dir_exclude_stack_cache
                        .insert(parent.to_string(), dirs.clone());
                    dirs
                }
            }
        };

        let per_dir_name = self.per_directory_name.clone();
        for dir in &dirs {
            if !self.gitignore_cache.contains_key(dir) {
                let rules = load_gitignore_for_dir(
                    repo,
                    index,
                    dir,
                    per_dir_name.as_deref(),
                    &mut self.warnings,
                )?;
                self.gitignore_cache.insert(dir.clone(), rules);
            }
        }
        Ok(dirs)
    }

    fn cached_rule_slices_for_dirs(&self, dirs: &[String]) -> Vec<&[IgnoreRule]> {
        dirs.iter()
            .map(|dir| {
                self.gitignore_cache
                    .get(dir)
                    .map_or(&[] as &[IgnoreRule], Vec::as_slice)
            })
            .collect()
    }
}

fn load_global_excludes(repo: &Repository) -> Result<Vec<IgnoreRule>> {
    let config = repo.config()?;
    let env = repo.environment();
    let (resolved, display_path) = if let Some(raw_path) = config.get("core.excludesfile") {
        let expanded = parse_path(env, &raw_path);
        let resolved = if Path::new(&expanded).is_absolute() {
            PathBuf::from(&expanded)
        } else if let Some(work_tree) = &repo.work_tree() {
            work_tree.join(&expanded)
        } else {
            repo.git_dir().join(&expanded)
        };
        (resolved, raw_path)
    } else if let Some(path) = env.default_xdg_config_git_file("ignore") {
        let display = path.display().to_string();
        (path, display)
    } else {
        return Ok(Vec::new());
    };

    let mut sink = Vec::new();
    load_rules_from_file(
        &resolved,
        IgnoreSource::CoreExcludesFile(display_path),
        String::new(),
        false,
        &mut sink,
    )
}

fn load_info_excludes(repo: &Repository) -> Result<Vec<IgnoreRule>> {
    let path = repo.git_dir().join("info/exclude");
    let mut sink = Vec::new();
    load_rules_from_file(
        &path,
        IgnoreSource::InfoExclude,
        String::new(),
        false,
        &mut sink,
    )
}

fn load_gitignore_for_dir(
    repo: &Repository,
    index: Option<&Index>,
    dir: &str,
    per_dir_name: Option<&str>,
    warnings: &mut Vec<String>,
) -> Result<Vec<IgnoreRule>> {
    let Some(work_tree) = &repo.work_tree() else {
        return Ok(Vec::new());
    };
    let file_name = per_dir_name.unwrap_or(".gitignore");
    let path = if dir.is_empty() {
        work_tree.join(file_name)
    } else {
        work_tree.join(dir).join(file_name)
    };
    let source_display = if dir.is_empty() {
        file_name.to_owned()
    } else {
        format!("{dir}/{file_name}")
    };
    let source = IgnoreSource::PerDirectory(source_display.clone());
    let rel_key = if dir.is_empty() {
        file_name.to_owned()
    } else {
        format!("{dir}/{file_name}")
    };

    // In-tree `.gitignore` must not be a symlink (Git follows symlinks for global/info excludes
    // only). Match Git's warning and skip the file (t0008).
    match read_in_tree_pattern_file(&path)? {
        InTreePatternFile::Symlink => {
            warnings.push(crate::diagnostics::warning_line(&format!(
                "unable to access '{source_display}': Too many levels of symbolic links"
            )));
            return Ok(Vec::new());
        }
        InTreePatternFile::Content(content) => {
            return parse_gitignore_content(&content, source, dir, warnings);
        }
        InTreePatternFile::Missing => {}
    }

    if let Some(ix) = index {
        if let Some(entry) = ix.get(rel_key.as_bytes(), 0) {
            if let Ok(obj) = repo.odb().read(&entry.oid) {
                if obj.kind == ObjectKind::Blob {
                    if let Ok(text) = std::str::from_utf8(&obj.data) {
                        return parse_gitignore_content(text, source, dir, warnings);
                    }
                }
            }
        }
    }

    Ok(Vec::new())
}

fn parse_gitignore_content(
    content: &str,
    source: IgnoreSource,
    base_dir: &str,
    _warnings: &mut Vec<String>,
) -> Result<Vec<IgnoreRule>> {
    let mut rules = Vec::new();
    // Split on LF only: like Git's `add_patterns_from_buffer`, a line loses at most one
    // trailing CR (in `parse_rule_line`), so `a\r\r` names the file `a\r`.
    for (idx, line) in content.split('\n').enumerate() {
        let line = if idx == 0 {
            line.strip_prefix('\u{feff}').unwrap_or(line)
        } else {
            line
        };
        if let Some(rule) = parse_rule_line(line, idx + 1, source.clone(), base_dir) {
            rules.push(rule);
        }
    }
    Ok(rules)
}

fn load_rules_from_file(
    path: &Path,
    source: IgnoreSource,
    base_dir: String,
    deny_symlink_gitignore: bool,
    warnings: &mut Vec<String>,
) -> Result<Vec<IgnoreRule>> {
    if deny_symlink_gitignore && path.exists() {
        if let Ok(meta) = fs::symlink_metadata(path) {
            if meta.file_type().is_symlink() {
                warnings.push(crate::diagnostics::warning_line(&format!(
                    "unable to access '{}': Too many levels of symbolic links",
                    source
                )));
                return Ok(Vec::new());
            }
        }
    }
    if path.exists() {
        crate::worktree_rules::record_ignore_pattern_file_read(path);
    }
    let Some(content) = read_optional_text(path)? else {
        return Ok(Vec::new());
    };
    parse_gitignore_content(&content, source, &base_dir, warnings)
}

/// Trims only *unescaped* trailing spaces, matching Git's `trim_trailing_spaces` in `dir.c`.
///
/// A backslash escapes the following byte; a run of spaces ending the line is removed only
/// when it is not part of an escape sequence (see t0008 "trailing whitespace is ignored").
fn trim_trailing_spaces_git(buf: &mut String) {
    let mut bytes = std::mem::take(buf).into_bytes();
    let mut last_space_start: Option<usize> = None;
    let mut i = 0usize;
    while i < bytes.len() {
        match bytes[i] {
            b' ' => {
                if last_space_start.is_none() {
                    last_space_start = Some(i);
                }
                i += 1;
            }
            b'\\' => {
                last_space_start = None;
                i += 1;
                if i < bytes.len() {
                    i += 1;
                } else {
                    *buf = String::from_utf8_lossy(&bytes).into_owned();
                    return;
                }
            }
            _ => {
                last_space_start = None;
                i += 1;
            }
        }
    }
    if let Some(start) = last_space_start {
        bytes.truncate(start);
    }
    *buf = String::from_utf8_lossy(&bytes).into_owned();
}

fn parse_rule_line(
    line: &str,
    line_number: usize,
    source: IgnoreSource,
    base_dir: &str,
) -> Option<IgnoreRule> {
    let mut raw_line = line.strip_suffix('\r').unwrap_or(line).to_owned();
    trim_trailing_spaces_git(&mut raw_line);
    if raw_line.is_empty() || raw_line.starts_with('#') {
        return None;
    }

    let pattern_text = raw_line.clone();
    let mut raw = raw_line;

    let mut negative = false;
    if let Some(rest) = raw.strip_prefix('!') {
        negative = true;
        raw = rest.to_owned();
    }
    if raw.is_empty() {
        return None;
    }

    let mut anchored = false;
    if let Some(rest) = raw.strip_prefix('/') {
        anchored = true;
        raw = rest.to_owned();
    }
    if raw.is_empty() {
        return None;
    }

    let mut directory_only = false;
    if let Some(rest) = raw.strip_suffix('/') {
        directory_only = true;
        raw = rest.to_owned();
    }
    if raw.is_empty() {
        return None;
    }

    let has_slash = raw.contains('/');
    let body = raw;
    let match_kind = classify_ignore_pattern(&body);
    Some(IgnoreRule {
        source,
        line_number,
        pattern_text,
        negative,
        directory_only,
        anchored,
        has_slash,
        body,
        base_dir: base_dir.to_owned(),
        match_kind,
    })
}

fn classify_ignore_pattern(body: &str) -> IgnorePatternKind {
    // Backslashes are wildmatch escapes (`foo\ bar` matches a literal space); never treat as
    // a plain literal string.
    if body.contains('\\') || body.contains(['?', '[']) {
        return IgnorePatternKind::Wild;
    }
    if !body.contains('*') {
        return IgnorePatternKind::Literal(body.to_owned());
    }
    if let Some(rest) = body.strip_prefix('*') {
        if !rest.contains('*') && !rest.contains('/') {
            return IgnorePatternKind::Suffix(rest.to_owned());
        }
    }
    if let Some(rest) = body.strip_suffix('*') {
        if !rest.contains('*') && !rest.contains('/') {
            return IgnorePatternKind::Prefix(rest.to_owned());
        }
    }
    IgnorePatternKind::Wild
}

fn base_dir_depth(base: &str) -> usize {
    if base.is_empty() {
        return 0;
    }
    base.split('/').count()
}

/// Git's `check-ignore -v` attributes coverage to a parent `…/` directory rule when a redundant
/// positive pattern exists in a nested `.gitignore` under an already-ignored directory.
fn refine_match_for_check_ignore_verbose(
    repo_rel_path: &str,
    is_dir: bool,
    ignored: bool,
    matched: Option<IgnoreMatch>,
    rules: &[&IgnoreRule],
    ignore_case: bool,
) -> Option<IgnoreMatch> {
    let m = matched?;
    if m.negative {
        return Some(m);
    }
    if !ignored {
        return Some(m);
    }
    let mut best: Option<&IgnoreRule> = None;
    for rule in rules {
        if !rule.directory_only {
            continue;
        }
        if !rule_matches(rule, repo_rel_path, is_dir, ignore_case) {
            continue;
        }
        match best {
            None => best = Some(rule),
            Some(b) if base_dir_depth(&rule.base_dir) < base_dir_depth(&b.base_dir) => {
                best = Some(rule);
            }
            Some(b)
                if base_dir_depth(&rule.base_dir) == base_dir_depth(&b.base_dir)
                    && rule.line_number < b.line_number =>
            {
                best = Some(rule);
            }
            _ => {}
        }
    }
    Some(
        best.map(|r| IgnoreMatch {
            source: r.source.clone(),
            line_number: r.line_number,
            pattern_text: r.pattern_text.clone(),
            negative: r.negative,
        })
        .unwrap_or(m),
    )
}

fn rule_matches(rule: &IgnoreRule, repo_rel_path: &str, is_dir: bool, ignore_case: bool) -> bool {
    if ignore_case {
        rule_matches_with::<true>(rule, repo_rel_path, is_dir)
    } else {
        rule_matches_with::<false>(rule, repo_rel_path, is_dir)
    }
}

/// [`rule_matches`] specialized on case folding so the default case-sensitive path carries
/// no per-comparison `core.ignorecase` checks.
///
/// Only the queried path itself is tested: slash-free patterns match its basename, patterns
/// with a slash match the whole path relative to the rule's directory. A path under an
/// excluded directory inherits that exclusion from [`IgnoreMatcher::first_excluded_ancestor`],
/// which probes each ancestor with the same test, so `tags` followed by `!tags/` leaves
/// `tags/a.txt` unignored while `tags` alone ignores it through its parent.
fn rule_matches_with<const FOLD: bool>(
    rule: &IgnoreRule,
    repo_rel_path: &str,
    is_dir: bool,
) -> bool {
    // Directory-only patterns (`foo/`) never match files, so `!pkg/` cannot re-include files
    // under `pkg/` that `*` still ignores (issue #1650).
    if rule.directory_only && !is_dir {
        return false;
    }
    let Some(rel_to_base) = strip_base(&rule.base_dir, repo_rel_path, FOLD) else {
        return false;
    };
    if rel_to_base.is_empty() {
        return false;
    }
    if rule.has_slash || rule.anchored {
        return rule_body_matches(rule, rel_to_base, true, FOLD);
    }
    let basename = rel_to_base.rsplit('/').next().unwrap_or(rel_to_base);
    rule_body_matches(rule, basename, false, FOLD)
}

fn is_tracked(index: Option<&Index>, repo_rel_path: &str) -> bool {
    let Some(index) = index else {
        return false;
    };
    // Trailing `/` is a directory query; Git does not apply the index tracked exemption
    // (`tracked.txt/` can match exclude rules even when `tracked.txt` is staged).
    if repo_rel_path.ends_with('/') {
        return false;
    }
    index.get(repo_rel_path.as_bytes(), 0).is_some()
}

/// True when `repo_rel_path` exists in the work tree as a regular file (not a directory).
fn worktree_path_is_existing_file(repo: &Repository, repo_rel_path: &str) -> bool {
    let Some(work_tree) = repo.work_tree() else {
        return false;
    };
    let path = work_tree.join(repo_rel_path);
    path.metadata().map(|m| m.is_file()).unwrap_or(false)
}

fn path_query_is_dir(repo: &Repository, repo_rel_path: &str) -> bool {
    if repo_rel_path.ends_with('/') {
        return true;
    }
    let Some(work_tree) = repo.work_tree() else {
        return false;
    };
    let path = work_tree.join(repo_rel_path);
    path.metadata().map(|m| m.is_dir()).unwrap_or(false)
}

impl Repository {
    /// Check whether paths are ignored, reusing one [`IgnoreMatcher`] for the whole batch.
    ///
    /// Tracked index entries are never reported as ignored. Each result includes verbose-style
    /// match metadata compatible with `git check-ignore -v`.
    ///
    /// # Parameters
    ///
    /// - `paths` - repository-relative paths with `/` separators (as returned by
    ///   [`normalize_repo_relative`]).
    /// - `index` - current index; tracked paths are treated as not ignored.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Io`] when loading exclude files fails.
    pub fn ignore_check(&self, paths: &[String], index: &Index) -> Result<Vec<IgnoreCheck>> {
        let mut matcher = IgnoreMatcher::from_repository(self)?;
        let mut out = Vec::with_capacity(paths.len());
        for path in paths {
            let is_dir = path_query_is_dir(self, path);
            let (ignored, matched) = matcher.check_path(self, Some(index), path, is_dir)?;
            out.push(IgnoreCheck {
                path: path.clone(),
                ignored,
                matched,
            });
        }
        Ok(out)
    }
}

/// Outcome of reading a per-directory pattern file that Git refuses to follow as a symlink.
enum InTreePatternFile {
    Missing,
    Symlink,
    Content(String),
}

/// Read `path` without following a final symlink, so a link (dangling, or to a FIFO or
/// device) is reported instead of opened. A missing file costs a single failed open on Unix.
fn read_in_tree_pattern_file(path: &Path) -> Result<InTreePatternFile> {
    #[cfg(unix)]
    let opened = {
        use std::os::unix::fs::OpenOptionsExt;
        fs::OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW)
            .open(path)
    };
    #[cfg(not(unix))]
    let opened = match fs::symlink_metadata(path) {
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            return Ok(InTreePatternFile::Missing);
        }
        Err(err) => return Err(Error::Io(err)),
        Ok(meta) if meta.file_type().is_symlink() => return Ok(InTreePatternFile::Symlink),
        Ok(_) => fs::File::open(path),
    };
    let mut file = match opened {
        Ok(file) => file,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            return Ok(InTreePatternFile::Missing);
        }
        Err(_) if fs::symlink_metadata(path).is_ok_and(|m| m.file_type().is_symlink()) => {
            return Ok(InTreePatternFile::Symlink);
        }
        Err(err) => return Err(Error::Io(err)),
    };
    let mut content = String::new();
    std::io::Read::read_to_string(&mut file, &mut content).map_err(Error::Io)?;
    Ok(InTreePatternFile::Content(content))
}

fn read_optional_text(path: &Path) -> Result<Option<String>> {
    match fs::read_to_string(path) {
        Ok(content) => Ok(Some(content)),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(err) => Err(Error::Io(err)),
    }
}

/// Path of `path` relative to the `.gitignore` directory `base`, or `None` when `path` is not
/// inside it. With `ignore_case`, the base compares without regard to ASCII case.
fn strip_base<'a>(base: &str, path: &'a str, ignore_case: bool) -> Option<&'a str> {
    if base.is_empty() {
        return Some(path);
    }
    if !starts_with_fold(path, base, ignore_case) {
        return None;
    }
    match path.as_bytes().get(base.len()) {
        None => Some(""),
        // `base.len()` is a char boundary: the byte there is ASCII `/`.
        Some(b'/') => path.get(base.len() + 1..),
        Some(_) => None,
    }
}

fn eq_fold(text: &str, pattern: &str, ignore_case: bool) -> bool {
    if ignore_case {
        text.eq_ignore_ascii_case(pattern)
    } else {
        text == pattern
    }
}

fn starts_with_fold(text: &str, prefix: &str, ignore_case: bool) -> bool {
    if !ignore_case {
        return text.starts_with(prefix);
    }
    text.as_bytes()
        .get(..prefix.len())
        .is_some_and(|head| head.eq_ignore_ascii_case(prefix.as_bytes()))
}

fn ends_with_fold(text: &str, suffix: &str, ignore_case: bool) -> bool {
    if !ignore_case {
        return text.ends_with(suffix);
    }
    text.len()
        .checked_sub(suffix.len())
        .and_then(|start| text.as_bytes().get(start..))
        .is_some_and(|tail| tail.eq_ignore_ascii_case(suffix.as_bytes()))
}

fn parent_dir(path: &str) -> &str {
    match path.rsplit_once('/') {
        Some((parent, _)) => parent,
        None => "",
    }
}

fn pattern_matches_kind(
    kind: &IgnorePatternKind,
    pattern: &str,
    text: &str,
    pathname: bool,
    ignore_case: bool,
) -> bool {
    match kind {
        IgnorePatternKind::Literal(lit) => eq_fold(text, lit, ignore_case),
        IgnorePatternKind::Prefix(prefix) => starts_with_fold(text, prefix, ignore_case),
        IgnorePatternKind::Suffix(suffix) => ends_with_fold(text, suffix, ignore_case),
        IgnorePatternKind::Wild => {
            let mut flags = if pathname { WM_PATHNAME } else { 0 };
            if ignore_case {
                flags |= WM_CASEFOLD;
            }
            wildmatch(pattern.as_bytes(), text.as_bytes(), flags)
        }
    }
}

fn rule_body_matches(rule: &IgnoreRule, text: &str, pathname: bool, ignore_case: bool) -> bool {
    pattern_matches_kind(&rule.match_kind, &rule.body, text, pathname, ignore_case)
}

/// One line from a sparse-checkout specification blob (`--filter=sparse:oid=…`).
#[derive(Debug, Clone)]
struct SparsePattern {
    negative: bool,
    directory_only: bool,
    anchored: bool,
    has_slash: bool,
    body: String,
}

impl SparsePattern {
    fn from_line(line: &str) -> Option<Self> {
        let mut raw_line = line.strip_suffix('\r').unwrap_or(line).to_owned();
        trim_trailing_spaces_git(&mut raw_line);
        if raw_line.is_empty() || raw_line.starts_with('#') {
            return None;
        }

        let mut raw = raw_line;

        let mut negative = false;
        if let Some(rest) = raw.strip_prefix('!') {
            negative = true;
            raw = rest.to_owned();
        }
        if raw.is_empty() {
            return None;
        }

        // Git `parse_path_pattern`: trailing `/` sets `PATTERN_FLAG_MUSTBEDIR` and shortens the
        // active pattern length without turning `/*` into an empty body (that would drop the rule).
        let mut directory_only = false;
        if raw.len() > 1 && raw.ends_with('/') {
            directory_only = true;
            raw.pop();
        }
        if raw.is_empty() {
            return None;
        }

        let mut anchored = false;
        if let Some(rest) = raw.strip_prefix('/') {
            anchored = true;
            raw = rest.to_owned();
        }
        // After `/*` → pop `/` we get `"/"` then strip leading `/` → empty. Git keeps this as the
        // root glob `*` (include all top-level names in non-cone sparse files).
        if raw.is_empty() && anchored && directory_only {
            raw = "*".to_owned();
            directory_only = false;
        } else if raw.is_empty() {
            return None;
        }

        let has_slash = raw.contains('/');
        Some(Self {
            negative,
            directory_only,
            anchored,
            has_slash,
            body: raw,
        })
    }
}

/// Whether `p` matches `pathname` for sparse-checkout evaluation.
///
/// `as_directory` mirrors Git's `dtype == DT_DIR` pass when walking parent paths:
/// patterns with a trailing `/` in the sparse file (`PATTERN_FLAG_MUSTBEDIR`) only
/// participate in those iterations.
fn sparse_pattern_matches(p: &SparsePattern, pathname: &str, as_directory: bool) -> bool {
    // Git never gates basename (NODIR) or anchored `/*` patterns on dtype: `match_basename` /
    // `match_pathname` run regardless of `DT_DIR` (dir.c). Earlier code rejected `nodir`
    // patterns on the parent-directory pass, which wrongly excluded `subsub/added` under
    // `/*` + `!sub` + `sub/added` (t1011 subtest 9). The `!/*/` directory-only rule still
    // shadows `/*` for nested dirs via reverse-order first-match (t7817/t1091 preserved).
    if p.directory_only && !as_directory {
        return false;
    }
    // On-disk `!/*/` parses as directory-only anchored `*`. For regular files we exclude nested
    // paths only (`dir/c`, not `a`). When walking parents (`as_directory`), Git still matches this
    // pattern against directory paths like `dir` so `dir/c` can be excluded (t7817).
    if p.directory_only && p.anchored && !p.has_slash && p.body == "*" {
        // Git `match_pathname` for MUSTBEDIR `/*` / `!/*/`: only top-level directory names
        // (no `/` in the pathname), including on the parent-directory pass — not every
        // `as_directory` path (dir.c / filter_sparse).
        let trimmed = pathname.trim_end_matches('/');
        return !trimmed.contains('/');
    }
    if !p.has_slash && !p.anchored {
        sparse_unanchored_basename_matches(&p.body, pathname)
    } else if p.anchored && !p.has_slash && p.body == "*" && !p.directory_only {
        // Sparse line `/*`: include only top-level paths (one segment). `WM_PATHNAME` keeps `*`
        // from matching `/`, but we match against the bare pathname; require no `/` so `dir/c` is
        // not included by `/*` alone (t7817 — excluded via `!/*/` on the parent directory pass).
        !pathname.contains('/')
    } else {
        wildmatch(p.body.as_bytes(), pathname.as_bytes(), WM_PATHNAME)
    }
}

fn sparse_pattern_matches_path(p: &SparsePattern, pathname: &str) -> bool {
    sparse_pattern_matches(p, pathname, false)
}

/// Parse sparse-checkout lines from a blob (same syntax as `info/sparse-checkout`).
#[must_use]
pub fn parse_sparse_patterns_from_blob(content: &str) -> Vec<String> {
    let mut patterns = Vec::new();
    for line in content.lines() {
        let t = line.trim_end_matches('\r');
        if t.is_empty() || t.starts_with('#') {
            continue;
        }
        patterns.push(t.to_owned());
    }
    patterns
}

fn sparse_list_last_match(
    pathname: &str,
    as_directory: bool,
    parsed: &[SparsePattern],
) -> Option<bool> {
    for p in parsed.iter().rev() {
        if sparse_pattern_matches(p, pathname, as_directory) {
            return Some(!p.negative);
        }
    }
    None
}

/// Non-cone sparse-checkout inclusion, matching Git's `path_in_sparse_checkout`.
///
/// Walks from the full path toward parents (as in `dir.c:path_in_sparse_checkout_1`):
/// each step uses last-match-wins over patterns; `UNDECIDED` falls back to the parent
/// directory until the decision is made or the path is rejected at the top level.
#[must_use]
pub fn path_in_sparse_checkout(path: &str, lines: &[String], work_tree: Option<&Path>) -> bool {
    ParsedSparsePatterns::parse(lines).includes(path, work_tree)
}

/// Non-cone sparse-checkout lines parsed once, for repeated [`path_in_sparse_checkout`] checks.
#[derive(Debug, Clone)]
pub(crate) struct ParsedSparsePatterns(Vec<SparsePattern>);

impl ParsedSparsePatterns {
    /// Parse sparse-checkout lines; blank lines and comments are dropped.
    pub(crate) fn parse(lines: &[String]) -> Self {
        Self(
            lines
                .iter()
                .filter_map(|l| SparsePattern::from_line(l))
                .collect(),
        )
    }

    /// Whether `path` is included; `work_tree` lets on-disk directories match directory-only rules.
    pub(crate) fn includes(&self, path: &str, work_tree: Option<&Path>) -> bool {
        if path.is_empty() || self.0.is_empty() {
            return true;
        }

        let mut end = path.len();
        let mut as_directory = false;

        loop {
            let pathname = &path[..end];
            let dtype_is_dir =
                work_tree.is_some_and(|wt| wt.join(pathname).is_dir()) || as_directory;

            match sparse_list_last_match(pathname, dtype_is_dir, &self.0) {
                Some(true) => return true,
                Some(false) => return false,
                None => {
                    let Some(slash) = path[..end].rfind('/') else {
                        // Top-level path with no matching rule: Git stops here (UNDECIDED → excluded),
                        // not at an empty pathname.
                        return false;
                    };
                    end = slash;
                    as_directory = true;
                }
            }
        }
    }
}

/// Last-match-wins sparse semantics for `rev-list --filter=sparse:oid=…` (non-cone).
///
/// Unanchored patterns without `/` use Git-style basename rules: `pat` matches `pat`,
/// `pat.ext`, and `pat/…` paths. Anchored patterns and patterns containing `/` use
/// pathname wildmatch with `WM_PATHNAME`.
///
/// Returns `None` if no pattern matched (`UNDECIDED`).
#[must_use]
pub fn path_matches_sparse_pattern_list(pathname: &str, lines: &[String]) -> Option<bool> {
    let parsed: Vec<SparsePattern> = lines
        .iter()
        .filter_map(|l| SparsePattern::from_line(l))
        .collect();
    if parsed.is_empty() {
        return None;
    }
    for p in parsed.iter().rev() {
        if sparse_pattern_matches_path(p, pathname) {
            return Some(!p.negative);
        }
    }
    None
}

fn sparse_unanchored_basename_matches(pat: &str, path: &str) -> bool {
    let basename = path.rsplit('/').next().unwrap_or(path);
    wildmatch(pat.as_bytes(), basename.as_bytes(), 0)
}

/// Returns the submodule path if `repo_rel_path` names something inside a gitlink entry.
#[must_use]
pub fn submodule_containing_path(repo_rel_path: &str, index: &Index) -> Option<String> {
    let bytes = repo_rel_path.as_bytes();
    if bytes.is_empty() {
        return None;
    }
    let mut best: Option<String> = None;
    let mut end = 0usize;
    while end < bytes.len() {
        let Some(slash) = bytes[end..].iter().position(|&b| b == b'/') else {
            break;
        };
        end += slash;
        if let Some(entry) = index.get(&bytes[..end], 0) {
            if entry.mode == MODE_GITLINK {
                best = Some(String::from_utf8_lossy(&bytes[..end]).into_owned());
            }
        }
        end += 1;
    }
    best
}

/// Convert a user-supplied path into a normalized repository-relative path.
///
/// # Parameters
///
/// - `repo` - repository handle.
/// - `cwd` - current working directory.
/// - `path` - user input path string.
///
/// # Errors
///
/// Returns [`Error::PathError`] if the path resolves outside the work tree.
pub fn normalize_repo_relative(repo: &Repository, cwd: &Path, path: &str) -> Result<String> {
    let Some(work_tree) = &repo.work_tree() else {
        return Err(Error::PathError(
            "this operation must be run in a work tree".to_owned(),
        ));
    };
    if path.starts_with(':') {
        return Ok(path.to_owned());
    }
    let query_as_dir = path.ends_with('/');
    let input = Path::new(path);
    let combined = if input.is_absolute() {
        input.to_path_buf()
    } else {
        cwd.join(input)
    };
    let normalized = normalize_path(&combined);
    // Repository opening already resolves the worktree root. Most queries share that
    // spelling, so avoid canonicalizing the same root again for every path in a batch.
    // Keep the filesystem-alias fallback for spellings such as macOS /tmp and /private/tmp.
    let mut rel = match normalized.strip_prefix(work_tree) {
        Ok(relative) => crate::git_path::path_to_git_slash(relative),
        Err(_) => {
            crate::git_path::strip_worktree_prefix(&normalized, work_tree).ok_or_else(|| {
                Error::PathError(format!("path '{path}' is outside repository work tree"))
            })?
        }
    };
    if query_as_dir && !rel.is_empty() && !rel.ends_with('/') {
        rel.push('/');
    }
    Ok(rel)
}

fn normalize_path(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                out.pop();
            }
            other => out.push(other.as_os_str()),
        }
    }
    out
}

#[cfg(test)]
mod normalization_tests {
    use super::*;

    #[test]
    fn bare_repository_queries_return_a_path_error() {
        let dir = tempfile::tempdir().unwrap();
        let repo = crate::repo::init_repository(
            dir.path(),
            true,
            "main",
            None,
            crate::RefStorageFormat::Files,
        )
        .unwrap();
        assert!(matches!(
            normalize_repo_relative(&repo, dir.path(), "missing/file"),
            Err(Error::PathError(_))
        ));
    }
}

#[cfg(test)]
mod sparse_checkout_tests {
    use super::*;

    #[test]
    fn non_cone_default_init_patterns() {
        let lines = vec!["/*".into(), "!/*/".into()];
        assert!(path_in_sparse_checkout("a", &lines, None));
        assert!(!path_in_sparse_checkout("folder1/a", &lines, None));
        let wt = std::env::temp_dir().join("grit-sparse-wt-test");
        let _ = std::fs::create_dir_all(wt.join("folder1"));
        let _ = std::fs::write(wt.join("a"), b"x");
        assert!(!path_in_sparse_checkout("folder1/a", &lines, Some(&wt)));
        assert!(!path_in_sparse_checkout("folder1", &lines, Some(&wt)));
    }

    #[test]
    fn unanchored_glob_matches_path_components() {
        let lines = vec!["/*".into(), "!/*/".into(), "*folder*".into()];
        assert!(path_in_sparse_checkout("folder1/a", &lines, None));
        assert!(path_in_sparse_checkout("folder2/a", &lines, None));
        assert!(!path_in_sparse_checkout("deep/a", &lines, None));
    }

    #[test]
    fn non_cone_src_nested_under_included_dir_with_negated_slash_slash() {
        let lines = vec!["/*".into(), "!/*/".into(), "/src/".into()];
        assert!(path_in_sparse_checkout("a.txt", &lines, None));
        assert!(path_in_sparse_checkout("src/z", &lines, None));
        assert!(path_in_sparse_checkout("src/deep/w", &lines, None));
        assert!(!path_in_sparse_checkout("d1/x", &lines, None));
        assert!(!path_in_sparse_checkout("d1/sub/y", &lines, None));
    }

    #[test]
    fn unanchored_star_with_negated_other_includes_other_slash_q() {
        let lines = vec!["*".into(), "!other".into()];
        assert!(path_in_sparse_checkout("other/q", &lines, None));
        assert!(!path_in_sparse_checkout("other", &lines, None));
    }
}

#[cfg(test)]
mod gitignore_glob_tests {
    use super::*;

    fn pathname_pattern_matches(pattern: &str, text: &str) -> bool {
        pattern_matches_kind(
            &classify_ignore_pattern(pattern),
            pattern,
            text,
            true,
            false,
        )
    }

    fn rule(line: &str, base_dir: &str) -> IgnoreRule {
        parse_rule_line(line, 1, IgnoreSource::InfoExclude, base_dir).unwrap()
    }

    #[test]
    fn lines_lose_at_most_one_trailing_cr() {
        let rules = |content: &str| {
            parse_gitignore_content(content, IgnoreSource::InfoExclude, "", &mut Vec::new())
                .unwrap()
        };
        let [crlf] = rules("a\r\n").try_into().unwrap();
        assert!(rule_matches(&crlf, "a", false, false));
        let [double_cr] = rules("a\r\r\n").try_into().unwrap();
        assert!(rule_matches(&double_cr, "a\r", false, false));
        assert!(!rule_matches(&double_cr, "a", false, false));
        let [leading_cr] = rules("\ra\n").try_into().unwrap();
        assert!(rule_matches(&leading_cr, "\ra", false, false));
        assert!(rules("a\nb").len() == 2);
    }

    #[test]
    fn every_pattern_kind_folds_ascii_case_only_with_ignore_case() {
        for (pattern, kind, path) in [
            ("README", "literal", "readme"),
            ("Target*", "prefix", "target-debug"),
            ("*.LOG", "suffix", "debug.log"),
            ("[Dd]EBUG?.txt", "wild", "debug1.TXT"),
        ] {
            let r = rule(pattern, "");
            assert!(!rule_matches(&r, path, false, false), "{kind}: {pattern}");
            assert!(rule_matches(&r, path, false, true), "{kind}: {pattern}");
            assert!(rule_matches(&r, &format!("sub/{path}"), false, true));
        }
        assert!(!rule_matches(&rule("*.LOG", ""), "x.lag", false, true));
        assert!(!rule_matches(&rule("Target*", ""), "tar", false, true));
        assert!(!rule_matches(&rule("*.LOG", ""), "LOG", false, true));
        // Git folds ASCII only; non-ASCII letters keep their case.
        assert!(!rule_matches(&rule("*É", ""), "é", false, true));
        assert!(!rule_matches(&rule("Über", ""), "über", false, true));
        assert!(rule_matches(&rule("Über", ""), "ÜBER", false, true));
    }

    #[test]
    fn directory_and_anchored_rules_fold_case() {
        let dir_only = rule("Build/", "");
        assert!(rule_matches(&dir_only, "build", true, true));
        assert!(rule_matches(&dir_only, "src/BUILD", true, true));
        assert!(!rule_matches(&dir_only, "build", true, false));
        assert!(!rule_matches(&dir_only, "build", false, true));
        let anchored = rule("/Docs/*.PDF", "");
        assert!(rule_matches(&anchored, "docs/manual.pdf", false, true));
        assert!(!rule_matches(&anchored, "docs/sub/manual.pdf", false, true));
        assert!(!rule_matches(&anchored, "docs/manual.pdf", false, false));
    }

    #[test]
    fn nested_gitignore_base_dir_folds_case() {
        let nested = rule("*.tmp", "Sub/Dir");
        assert!(rule_matches(&nested, "Sub/Dir/a.tmp", false, false));
        assert!(!rule_matches(&nested, "sub/dir/a.tmp", false, false));
        assert!(rule_matches(&nested, "sub/dir/a.tmp", false, true));
        assert!(!rule_matches(&nested, "sub/dirx/a.tmp", false, true));
        assert!(!rule_matches(&nested, "sub/di", false, true));
        assert_eq!(strip_base("Sub", "sub", true), Some(""));
        assert_eq!(strip_base("Sub", "SUB/x", true), Some("x"));
        assert_eq!(strip_base("Sub", "subway/x", true), None);
        assert_eq!(strip_base("Sub", "sub/x", false), None);
    }

    #[test]
    fn precompiled_literal_prefix_suffix_patterns() {
        assert!(matches!(
            classify_ignore_pattern("build"),
            IgnorePatternKind::Literal(_)
        ));
        assert!(matches!(
            classify_ignore_pattern("target*"),
            IgnorePatternKind::Prefix(_)
        ));
        assert!(matches!(
            classify_ignore_pattern("*.o"),
            IgnorePatternKind::Suffix(_)
        ));
        assert!(matches!(
            classify_ignore_pattern("a*b"),
            IgnorePatternKind::Wild
        ));
        assert!(matches!(
            classify_ignore_pattern(r"foo\ bar"),
            IgnorePatternKind::Wild
        ));
        assert!(matches!(
            classify_ignore_pattern("foo/*"),
            IgnorePatternKind::Wild
        ));
        assert!(!pathname_pattern_matches("foo/*", "foo/bar/baz"));
        assert!(rule_body_matches(
            &IgnoreRule {
                source: IgnoreSource::InfoExclude,
                line_number: 1,
                pattern_text: "*.log".into(),
                negative: false,
                directory_only: false,
                anchored: false,
                has_slash: false,
                body: "*.log".into(),
                base_dir: String::new(),
                match_kind: IgnorePatternKind::Suffix(".log".into()),
            },
            "app.log",
            false,
            false
        ));
    }

    #[test]
    fn dir_star_extension_matches_nested_path() {
        // Slash in pattern → WM_PATHNAME: `*` does not cross `/` (gitignore(5)).
        assert!(!pathname_pattern_matches("doc/*.pdf", "doc/sub/manual.pdf"));
        assert!(pathname_pattern_matches("doc/*.pdf", "doc/manual.pdf"));
        assert!(!pathname_pattern_matches("doc/*.pdf", "other/manual.pdf"));
        assert!(pathname_pattern_matches(
            "doc/**/*.pdf",
            "doc/sub/manual.pdf"
        ));
    }

    #[test]
    fn dir_star_extension_unexpanded_when_parent_has_glob() {
        assert!(!pathname_pattern_matches("*/foo/*.pdf", "a/foo/sub/x.pdf"));
    }

    #[test]
    fn nested_dir_star_extension() {
        assert!(!pathname_pattern_matches("foo/bar/*.c", "foo/bar/baz/x.c"));
    }
}

#[cfg(all(test, unix))]
mod per_directory_gitignore_tests {
    use super::*;
    use crate::repo::init_repository;
    use crate::RefStorageFormat;
    use std::os::unix::ffi::OsStringExt;
    use std::os::unix::fs::symlink;
    use tempfile::TempDir;

    fn ignored_with_warnings(repo: &Repository, path: &str) -> (bool, Vec<String>) {
        let mut matcher = IgnoreMatcher::from_repository(repo).unwrap();
        let ignored = matcher.is_path_ignored(repo, None, path, false).unwrap();
        (ignored, matcher.take_warnings())
    }

    #[test]
    fn symlinked_gitignore_is_skipped_with_warning_and_plain_one_applies() {
        let dir = TempDir::new().unwrap();
        let root = dir.path();
        let repo = init_repository(root, false, "main", None, RefStorageFormat::Files).unwrap();
        fs::create_dir_all(root.join("plain")).unwrap();
        fs::create_dir_all(root.join("to_file")).unwrap();
        fs::create_dir_all(root.join("to_dir")).unwrap();
        fs::create_dir_all(root.join("none")).unwrap();
        fs::write(root.join("plain/.gitignore"), "*.log\n").unwrap();
        fs::write(root.join("rules"), "*.log\n").unwrap();
        symlink(root.join("rules"), root.join("to_file/.gitignore")).unwrap();
        symlink(root.join("plain"), root.join("to_dir/.gitignore")).unwrap();
        fs::create_dir_all(root.join("dangling")).unwrap();
        symlink(root.join("no-such-rules"), root.join("dangling/.gitignore")).unwrap();
        fs::create_dir_all(root.join("to_fifo")).unwrap();
        let fifo = std::ffi::CString::new(root.join("fifo").into_os_string().into_vec()).unwrap();
        // SAFETY: `fifo` is a NUL-terminated path that outlives the call.
        assert_eq!(unsafe { libc::mkfifo(fifo.as_ptr(), 0o644) }, 0);
        symlink(root.join("fifo"), root.join("to_fifo/.gitignore")).unwrap();

        assert_eq!(ignored_with_warnings(&repo, "plain/a.log"), (true, vec![]));
        assert_eq!(ignored_with_warnings(&repo, "none/a.log"), (false, vec![]));
        let (done_tx, done_rx) = std::sync::mpsc::channel();
        let fifo_root = root.to_path_buf();
        std::thread::spawn(move || {
            let options = crate::environment::RepositoryOptions::empty().with_work_tree(&fifo_root);
            let repo = Repository::open(&fifo_root.join(".git"), &options).unwrap();
            let _ = done_tx.send(ignored_with_warnings(&repo, "to_fifo/a.log"));
        });
        let (ignored, warnings) = done_rx
            .recv_timeout(std::time::Duration::from_secs(5))
            .expect("a .gitignore symlink to a FIFO must be rejected without opening it");
        assert!(!ignored);
        assert_eq!(warnings.len(), 1, "{warnings:?}");
        for dir in ["to_file", "to_dir", "dangling"] {
            let (ignored, warnings) = ignored_with_warnings(&repo, &format!("{dir}/a.log"));
            assert!(!ignored, "{dir}: symlinked .gitignore must not apply");
            assert_eq!(warnings.len(), 1, "{dir}: {warnings:?}");
            assert!(warnings[0].contains(&format!("{dir}/.gitignore")));
        }
    }
}

#[cfg(test)]
mod ignore_case_matcher_tests {
    use super::*;
    use crate::repo::init_repository;
    use crate::RefStorageFormat;
    use tempfile::TempDir;

    fn issue_repo(ignore_case: bool) -> (TempDir, Repository) {
        let dir = TempDir::new().unwrap();
        let root = dir.path();
        init_repository(root, false, "main", None, RefStorageFormat::Files).unwrap();
        let mut config = fs::read_to_string(root.join(".git/config")).unwrap();
        config.push_str(&format!("[core]\n\tignorecase = {ignore_case}\n"));
        fs::write(root.join(".git/config"), config).unwrap();
        fs::write(root.join(".gitignore"), "Build/\n*.LOG\n!keep.log\n").unwrap();
        fs::create_dir_all(root.join("build")).unwrap();
        for file in ["build/x.o", "debug.log", "keep.log", "README"] {
            fs::write(root.join(file), "x\n").unwrap();
        }
        let options = crate::environment::RepositoryOptions::empty().with_work_tree(root);
        let repo = Repository::open(&root.join(".git"), &options).unwrap();
        (dir, repo)
    }

    fn ignored(matcher: &mut IgnoreMatcher, repo: &Repository, path: &str, is_dir: bool) -> bool {
        let fast = matcher.is_path_ignored(repo, None, path, is_dir).unwrap();
        let (verbose, _) = matcher.check_path(repo, None, path, is_dir).unwrap();
        assert_eq!(fast, verbose, "{path}");
        fast
    }

    #[test]
    fn from_repository_honors_core_ignorecase() {
        let (_dir, repo) = issue_repo(true);
        let mut matcher = IgnoreMatcher::from_repository(&repo).unwrap();
        assert!(matcher.ignore_case());
        assert!(ignored(&mut matcher, &repo, "build", true));
        assert!(ignored(&mut matcher, &repo, "build/x.o", false));
        assert!(ignored(&mut matcher, &repo, "debug.log", false));
        assert!(!ignored(&mut matcher, &repo, "keep.log", false));
        assert!(!ignored(&mut matcher, &repo, "KEEP.LOG", false));
        assert!(!ignored(&mut matcher, &repo, "README", false));
        let (_, matched) = matcher.check_path(&repo, None, "build/x.o", false).unwrap();
        assert_eq!(matched.unwrap().pattern_text, "Build/");
    }

    #[test]
    fn case_sensitive_by_default_and_overridable() {
        let (_dir, repo) = issue_repo(false);
        let mut matcher = IgnoreMatcher::from_repository(&repo).unwrap();
        assert!(!matcher.ignore_case());
        assert!(!ignored(&mut matcher, &repo, "build/x.o", false));
        assert!(!ignored(&mut matcher, &repo, "debug.log", false));
        assert!(ignored(&mut matcher, &repo, "Build/x.o", false));

        let mut folded = matcher.with_ignore_case(true);
        assert!(ignored(&mut folded, &repo, "build/x.o", false));
        assert!(ignored(&mut folded, &repo, "debug.log", false));
        assert!(!IgnoreMatcher::default().ignore_case());
    }
}

#[cfg(test)]
mod index_lookup_scaling_tests {
    use super::*;
    use crate::index::{index_path_cmp, Index, IndexEntry, MODE_REGULAR};
    use crate::objects::ObjectId;
    use crate::repo::init_repository;
    use crate::RefStorageFormat;
    use tempfile::TempDir;

    fn dummy_oid() -> ObjectId {
        ObjectId::from_bytes(&[0u8; 20]).unwrap()
    }

    fn make_entry(path: &str) -> IndexEntry {
        IndexEntry {
            ctime_sec: 0,
            ctime_nsec: 0,
            mtime_sec: 0,
            mtime_nsec: 0,
            dev: 0,
            ino: 0,
            mode: MODE_REGULAR,
            uid: 0,
            gid: 0,
            size: 0,
            oid: dummy_oid(),
            flags: path.len().min(0xFFF) as u16,
            flags_extended: None,
            path: path.as_bytes().to_vec(),
            base_index_pos: 0,
        }
    }

    #[test]
    fn tracked_check_path_index_lookups_sublinear_in_index_size() {
        const N: usize = 100_000;
        const M: usize = 1_000;
        let dir = TempDir::new().unwrap();
        let repo =
            init_repository(dir.path(), false, "main", None, RefStorageFormat::Files).unwrap();
        let mut index = Index::new();
        for i in 0..N {
            index.add_or_replace(make_entry(&format!("tracked/{i:06}.txt")));
        }
        let mut matcher = IgnoreMatcher::default();
        index_path_cmp::reset();
        for i in 0..M {
            let path = format!("untracked/{i:06}.txt");
            let (ignored, _) = matcher
                .check_path(&repo, Some(&index), &path, false)
                .unwrap();
            assert!(!ignored, "untracked path must not be ignored");
        }
        let cmps = index_path_cmp::take();
        let log_n = (N as f64).log2().ceil() as usize;
        let bound = 100 * M * log_n;
        assert!(
            cmps < bound,
            "index path comparisons {cmps} should stay under {bound} (N={N}, M={M})"
        );
    }
}

#[cfg(test)]
mod pattern_list_tests {
    use super::IgnorePatternList;

    fn ignored(content: &str, path: &str, is_dir: bool) -> bool {
        IgnorePatternList::parse(content.as_bytes(), "")
            .is_ignored(path, is_dir, false)
            .unwrap_or(false)
    }

    #[test]
    fn basename_patterns_match_last_component_only() {
        assert!(ignored("foo\n", "foo", false));
        assert!(ignored("foo\n", "dir/foo", false));
        assert!(!ignored("foo\n", "foo/bar", false));
        assert!(ignored("*.o\n", "a/b/c.o", false));
    }

    #[test]
    fn slash_patterns_match_whole_path() {
        assert!(ignored("/foo\n", "foo", false));
        assert!(!ignored("/foo\n", "dir/foo", false));
        assert!(ignored("dir/foo\n", "dir/foo", false));
        assert!(!ignored("dir/foo\n", "x/dir/foo", false));
        assert!(ignored("dir/*.o\n", "dir/a.o", false));
        assert!(!ignored("dir/*.o\n", "dir/sub/a.o", false));
        assert!(ignored("**/foo\n", "a/b/foo", false));
        assert!(ignored("a/**/b\n", "a/x/y/b", false));
    }

    #[test]
    fn directory_only_patterns_need_a_directory() {
        assert!(ignored("foo/\n", "foo", true));
        assert!(!ignored("foo/\n", "foo", false));
        assert!(!ignored("foo/\n", "foo/bar", false));
    }

    #[test]
    fn last_matching_pattern_wins() {
        let list = IgnorePatternList::parse(b"*.log\n!keep.log\n", "");
        assert_eq!(list.is_ignored("a.log", false, false), Some(true));
        assert_eq!(list.is_ignored("keep.log", false, false), Some(false));
        assert_eq!(list.is_ignored("a.txt", false, false), None);
        let m = list.last_match("keep.log", false, false).unwrap();
        assert_eq!((m.line_number, m.pattern_text.as_str()), (2, "!keep.log"));
    }

    #[test]
    fn comments_blank_lines_and_trailing_spaces() {
        let list = IgnorePatternList::parse(b"\xef\xbb\xbf# comment\n\nfoo  \nbar\\ \n\\#x\n", "");
        assert_eq!(list.len(), 3);
        assert!(ignored("foo  \n", "foo", false));
        assert!(list.is_ignored("bar ", false, false).unwrap());
        assert!(list.is_ignored("#x", false, false).unwrap());
    }

    #[test]
    fn base_directory_scopes_patterns() {
        let list = IgnorePatternList::parse(b"foo\n/bar\n", "sub");
        assert_eq!(list.is_ignored("sub/foo", false, false), Some(true));
        assert_eq!(list.is_ignored("sub/x/foo", false, false), Some(true));
        assert_eq!(list.is_ignored("sub/bar", false, false), Some(true));
        assert_eq!(list.is_ignored("sub/x/bar", false, false), None);
        assert_eq!(list.is_ignored("foo", false, false), None);
        assert_eq!(list.is_ignored("subfoo", false, false), None);
    }

    #[test]
    fn ignore_case() {
        let list = IgnorePatternList::parse(b"*.LOG\nBuild/\n", "");
        assert_eq!(list.is_ignored("debug.log", false, false), None);
        assert_eq!(list.is_ignored("debug.log", false, true), Some(true));
        assert_eq!(list.is_ignored("build", true, true), Some(true));
    }
}

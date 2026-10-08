//! Ignore and exclude matching for `check-ignore`.
//!
//! This module implements a focused subset of Git ignore behavior:
//! per-directory `.gitignore`, `.git/info/exclude`, and `core.excludesfile`
//! with "last matching pattern wins" precedence.

use std::collections::HashMap;
use std::fs;
use std::path::{Component, Path, PathBuf};

use crate::config::parse_path;
use crate::error::{Error, Result};
use crate::index::{Index, MODE_GITLINK};
use crate::objects::ObjectKind;
use crate::repo::Repository;
use crate::wildmatch::{wildmatch, WM_PATHNAME};

/// Metadata for a matching rule.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IgnoreMatch {
    /// The source file shown in verbose output.
    pub source_display: String,
    /// Line number in the source file (1-based).
    pub line_number: usize,
    /// Pattern text as written (excluding comments/blank lines).
    pub pattern_text: String,
    /// Whether this is a negated pattern (`!pattern`).
    pub negative: bool,
}

#[derive(Debug, Clone)]
struct IgnoreRule {
    source_display: String,
    line_number: usize,
    pattern_text: String,
    negative: bool,
    directory_only: bool,
    anchored: bool,
    has_slash: bool,
    body: String,
    base_dir: String,
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
}

impl IgnoreMatcher {
    /// Build a matcher from repository exclude sources.
    ///
    /// # Parameters
    ///
    /// - `repo` - open repository.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Io`] if configured pattern files cannot be read.
    pub fn from_repository(repo: &Repository) -> Result<Self> {
        Ok(Self {
            global_rules: load_global_excludes(repo)?,
            info_rules: load_info_excludes(repo)?,
            // Standard excludes include the per-directory `.gitignore` set (Git `EXC_DIRS`).
            read_per_directory: true,
            ..Self::default()
        })
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
            let mut more =
                load_rules_from_file(&resolved, display, String::new(), false, &mut self.warnings)?;
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
    }

    /// Append patterns from `ls-files --exclude` / `-x` (Git command-line exclude list).
    pub fn add_cli_excludes(&mut self, patterns: &[String]) {
        for pat in patterns {
            if let Some(rule) = parse_rule_line(pat, 1, "--exclude option", "") {
                self.cli_rules.push(rule);
            }
        }
    }

    /// Take any warnings accumulated while loading ignore files (caller prints to stderr).
    #[must_use]
    pub fn take_warnings(&mut self) -> Vec<String> {
        std::mem::take(&mut self.warnings)
    }

    /// Check whether a repository-relative path is ignored.
    ///
    /// # Parameters
    ///
    /// - `repo` - repository handle.
    /// - `index` - optional index; when present, tracked entries are not ignored.
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
        let per_dir_groups = self.rules_for_path_grouped(repo, index, repo_rel_path)?;

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
        per_dir_groups: &[Vec<IgnoreRule>],
    ) -> (bool, Option<IgnoreMatch>) {
        let groups = self.build_groups(per_dir_groups);
        for group in &groups {
            let mut group_match: Option<&&IgnoreRule> = None;
            for rule in group {
                if rule_matches(rule, path, is_dir) {
                    group_match = Some(rule);
                }
            }
            if let Some(rule) = group_match {
                return (
                    !rule.negative,
                    Some(IgnoreMatch {
                        source_display: rule.source_display.clone(),
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
    fn flatten_groups<'a>(&'a self, per_dir_groups: &'a [Vec<IgnoreRule>]) -> Vec<&'a IgnoreRule> {
        self.build_groups(per_dir_groups)
            .into_iter()
            .flatten()
            .collect()
    }

    /// Assemble the priority-ordered exclude groups: EXC_CMDL, then per-directory `.gitignore`
    /// (deepest directory first), then EXC_FILE (`--exclude-from`, global, info).
    fn build_groups<'a>(
        &'a self,
        per_dir_groups: &'a [Vec<IgnoreRule>],
    ) -> Vec<Vec<&'a IgnoreRule>> {
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
        per_dir_groups: &[Vec<IgnoreRule>],
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
    fn rules_for_path_grouped(
        &mut self,
        repo: &Repository,
        index: Option<&Index>,
        repo_rel_path: &str,
    ) -> Result<Vec<Vec<IgnoreRule>>> {
        if !self.read_per_directory {
            return Ok(Vec::new());
        }
        let parent = parent_dir(repo_rel_path);
        let mut dirs = Vec::new();
        dirs.push(String::new());
        if !parent.is_empty() {
            let mut cur = String::new();
            for segment in parent.split('/') {
                if !cur.is_empty() {
                    cur.push('/');
                }
                cur.push_str(segment);
                dirs.push(cur.clone());
            }
        }

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

        let mut groups: Vec<Vec<IgnoreRule>> = Vec::new();
        for dir in dirs {
            if let Some(rules) = self.gitignore_cache.get(&dir) {
                groups.push(rules.clone());
            } else {
                groups.push(Vec::new());
            }
        }
        Ok(groups)
    }
}

fn load_global_excludes(repo: &Repository) -> Result<Vec<IgnoreRule>> {
    let config = repo.config()?;
    let Some(raw_path) = config
        .get("core.excludesfile")
        .or_else(|| default_global_ignore_path(repo.environment()))
    else {
        return Ok(Vec::new());
    };

    let expanded = parse_path(&raw_path);
    let resolved = if Path::new(&expanded).is_absolute() {
        PathBuf::from(&expanded)
    } else if let Some(work_tree) = &repo.work_tree {
        work_tree.join(&expanded)
    } else {
        repo.git_dir.join(&expanded)
    };

    let mut sink = Vec::new();
    load_rules_from_file(&resolved, raw_path, String::new(), false, &mut sink)
}

fn default_global_ignore_path(env: &crate::environment::Environment) -> Option<String> {
    env.default_global_ignore_path()
}

fn load_info_excludes(repo: &Repository) -> Result<Vec<IgnoreRule>> {
    let path = repo.git_dir.join("info/exclude");
    let mut sink = Vec::new();
    load_rules_from_file(
        &path,
        ".git/info/exclude".to_owned(),
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
    let Some(work_tree) = &repo.work_tree else {
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
    let rel_key = if dir.is_empty() {
        file_name.to_owned()
    } else {
        format!("{dir}/{file_name}")
    };

    // In-tree `.gitignore` must not be a symlink (Git follows symlinks for global/info excludes
    // only). Match Git's warning and skip the file (t0008).
    if path.exists() {
        if let Ok(meta) = fs::symlink_metadata(&path) {
            if meta.file_type().is_symlink() {
                warnings.push(format!(
                    "warning: unable to access '{source_display}': Too many levels of symbolic links"
                ));
                return Ok(Vec::new());
            }
        }
    }

    if let Some(content) = read_optional_text(&path)? {
        return parse_gitignore_content(&content, &source_display, dir, warnings);
    }

    if let Some(ix) = index {
        if let Some(entry) = ix.entries.iter().find(|e| {
            e.stage() == 0
                && std::str::from_utf8(&e.path)
                    .map(|p| p == rel_key.as_str())
                    .unwrap_or(false)
        }) {
            if let Ok(obj) = repo.odb.read(&entry.oid) {
                if obj.kind == ObjectKind::Blob {
                    if let Ok(text) = std::str::from_utf8(&obj.data) {
                        return parse_gitignore_content(text, &source_display, dir, warnings);
                    }
                }
            }
        }
    }

    Ok(Vec::new())
}

fn parse_gitignore_content(
    content: &str,
    source_display: &str,
    base_dir: &str,
    _warnings: &mut Vec<String>,
) -> Result<Vec<IgnoreRule>> {
    let mut rules = Vec::new();
    for (idx, line) in content.lines().enumerate() {
        let line = if idx == 0 {
            line.strip_prefix('\u{feff}').unwrap_or(line)
        } else {
            line
        };
        if let Some(rule) = parse_rule_line(line, idx + 1, source_display, base_dir) {
            rules.push(rule);
        }
    }
    Ok(rules)
}

fn load_rules_from_file(
    path: &Path,
    source_display: String,
    base_dir: String,
    deny_symlink_gitignore: bool,
    warnings: &mut Vec<String>,
) -> Result<Vec<IgnoreRule>> {
    if deny_symlink_gitignore && path.exists() {
        if let Ok(meta) = fs::symlink_metadata(path) {
            if meta.file_type().is_symlink() {
                warnings.push(format!(
                    "warning: unable to access '{source_display}': Too many levels of symbolic links"
                ));
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
    parse_gitignore_content(&content, &source_display, &base_dir, warnings)
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
    source_display: &str,
    base_dir: &str,
) -> Option<IgnoreRule> {
    let mut raw_line = line.trim_end_matches('\r').to_owned();
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
    Some(IgnoreRule {
        source_display: source_display.to_owned(),
        line_number,
        pattern_text,
        negative,
        directory_only,
        anchored,
        has_slash,
        body,
        base_dir: base_dir.to_owned(),
    })
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
        if !rule_matches(rule, repo_rel_path, is_dir) {
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
            source_display: r.source_display.clone(),
            line_number: r.line_number,
            pattern_text: r.pattern_text.clone(),
            negative: r.negative,
        })
        .unwrap_or(m),
    )
}

fn rule_matches(rule: &IgnoreRule, repo_rel_path: &str, is_dir: bool) -> bool {
    // Negated directory-only patterns containing `**` (e.g. `!data/**/`) only apply to directory
    // paths, not to files inside those directories. Matching them against ancestor paths for
    // files would incorrectly negate `data/**` for every file (see t0008 "directories and ** matches").
    if rule.directory_only && rule.negative && rule.body.contains("**") && !is_dir {
        return false;
    }

    let Some(rel_to_base) = strip_base(&rule.base_dir, repo_rel_path) else {
        return false;
    };

    if rule.directory_only {
        if rule.has_slash || rule.anchored {
            for ancestor in ancestor_dirs(rel_to_base, is_dir) {
                if gitignore_path_glob_matches(&rule.body, &ancestor) {
                    return true;
                }
            }
            return false;
        }
        for ancestor in ancestor_dir_basenames(rel_to_base, is_dir) {
            if glob_matches(&rule.body, ancestor) {
                return true;
            }
        }
        return false;
    }

    if rule.has_slash || rule.anchored {
        return gitignore_path_glob_matches(&rule.body, rel_to_base);
    }

    path_component_names(rel_to_base)
        .iter()
        .any(|name| glob_matches(&rule.body, name))
}

fn is_tracked(index: Option<&Index>, repo_rel_path: &str) -> bool {
    let Some(index) = index else {
        return false;
    };
    index.entries.iter().any(|entry| {
        entry.stage() == 0
            && std::str::from_utf8(&entry.path)
                .map(|path| path == repo_rel_path)
                .unwrap_or(false)
    })
}

fn read_optional_text(path: &Path) -> Result<Option<String>> {
    match fs::read_to_string(path) {
        Ok(content) => Ok(Some(content)),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(err) => Err(Error::Io(err)),
    }
}

fn strip_base<'a>(base: &str, path: &'a str) -> Option<&'a str> {
    if base.is_empty() {
        return Some(path);
    }
    if path == base {
        return Some("");
    }
    let prefix = format!("{base}/");
    path.strip_prefix(&prefix)
}

fn parent_dir(path: &str) -> &str {
    match path.rsplit_once('/') {
        Some((parent, _)) => parent,
        None => "",
    }
}

fn path_component_names(path: &str) -> Vec<&str> {
    if path.is_empty() {
        return Vec::new();
    }
    path.split('/').collect()
}

fn ancestor_dirs(path: &str, is_dir: bool) -> Vec<String> {
    let mut out = Vec::new();
    if path.is_empty() {
        return out;
    }
    let parts: Vec<&str> = path.split('/').collect();
    let max = if is_dir {
        parts.len()
    } else {
        parts.len().saturating_sub(1)
    };
    for idx in 1..=max {
        out.push(parts[..idx].join("/"));
    }
    out
}

fn ancestor_dir_basenames(path: &str, is_dir: bool) -> Vec<&str> {
    let mut out = Vec::new();
    let parts: Vec<&str> = if path.is_empty() {
        Vec::new()
    } else {
        path.split('/').collect()
    };
    let max = if is_dir {
        parts.len()
    } else {
        parts.len().saturating_sub(1)
    };
    for item in parts.iter().take(max) {
        out.push(*item);
    }
    out
}

fn glob_matches(pattern: &str, text: &str) -> bool {
    wildmatch(pattern.as_bytes(), text.as_bytes(), WM_PATHNAME)
}

/// Like [`glob_matches`] for pathname-shaped ignore patterns.
///
/// Plain `wildmatch` with `WM_PATHNAME`: per gitignore(5), `*` never matches `/`, so
/// `dir/*.ext` matches only direct children of `dir/` (a previous extension that rewrote it
/// to `dir/**/*.ext` diverged from Git and made `add .` un-track nested files).
fn gitignore_path_glob_matches(pattern: &str, text: &str) -> bool {
    wildmatch(pattern.as_bytes(), text.as_bytes(), WM_PATHNAME)
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
        let mut raw_line = line.trim_end_matches('\r').to_owned();
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
        let trimmed = pathname.trim_end_matches('/');
        return trimmed.contains('/') || as_directory;
    }
    if !p.has_slash && !p.anchored {
        let pat = p.body.as_bytes();
        sparse_unanchored_basename_matches(&p.body, pathname)
            || wildmatch(pat, pathname.as_bytes(), WM_PATHNAME)
            || pathname
                .split('/')
                .any(|comp| wildmatch(pat, comp.as_bytes(), 0))
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
    if path.is_empty() {
        return true;
    }
    let parsed: Vec<SparsePattern> = lines
        .iter()
        .filter_map(|l| SparsePattern::from_line(l))
        .collect();
    if parsed.is_empty() {
        return true;
    }

    let mut end = path.len();
    let mut as_directory = false;

    loop {
        let pathname = &path[..end];
        let dtype_is_dir = work_tree.is_some_and(|wt| wt.join(pathname).is_dir()) || as_directory;

        match sparse_list_last_match(pathname, dtype_is_dir, &parsed) {
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
    if basename == pat {
        return true;
    }
    if let Some(rest) = basename.strip_prefix(pat) {
        return rest.starts_with('.') || rest.starts_with('/');
    }
    if path == pat {
        return true;
    }
    path.starts_with(&format!("{pat}/"))
}

/// Returns the submodule path if `repo_rel_path` names something inside a gitlink entry.
#[must_use]
pub fn submodule_containing_path(repo_rel_path: &str, index: &Index) -> Option<String> {
    let mut best: Option<&str> = None;
    for entry in &index.entries {
        if entry.stage() != 0 || entry.mode != MODE_GITLINK {
            continue;
        }
        let Ok(p) = std::str::from_utf8(&entry.path) else {
            continue;
        };
        if repo_rel_path.len() > p.len()
            && repo_rel_path.starts_with(p)
            && repo_rel_path.as_bytes().get(p.len()) == Some(&b'/')
            && best.is_none_or(|b| p.len() > b.len())
        {
            best = Some(p);
        }
    }
    best.map(std::string::ToString::to_string)
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
    let Some(work_tree) = &repo.work_tree else {
        return Err(Error::PathError(
            "this operation must be run in a work tree".to_owned(),
        ));
    };
    if path.starts_with(':') {
        return Ok(path.to_owned());
    }
    let input = Path::new(path);
    let combined = if input.is_absolute() {
        input.to_path_buf()
    } else {
        cwd.join(input)
    };
    let normalized = normalize_path(&combined);
    let rel = crate::git_path::strip_worktree_prefix(&normalized, work_tree).ok_or_else(|| {
        Error::PathError(format!("path '{path}' is outside repository work tree"))
    })?;
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
mod sparse_checkout_tests {
    use super::*;

    #[test]
    fn non_cone_default_init_patterns() {
        let lines = vec!["/*".into(), "!/*/".into()];
        assert!(path_in_sparse_checkout("a", &lines, None));
        assert!(!path_in_sparse_checkout("folder1/a", &lines, None));
        let wt = tempfile::tempdir().expect("tempdir").into_path().join("grit-sparse-wt-test");
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
}

#[cfg(test)]
mod gitignore_glob_tests {
    use super::*;

    #[test]
    fn dir_star_extension_matches_nested_path() {
        // Slash in pattern → WM_PATHNAME: `*` does not cross `/` (gitignore(5)).
        assert!(!gitignore_path_glob_matches(
            "doc/*.pdf",
            "doc/sub/manual.pdf"
        ));
        assert!(gitignore_path_glob_matches("doc/*.pdf", "doc/manual.pdf"));
        assert!(!gitignore_path_glob_matches(
            "doc/*.pdf",
            "other/manual.pdf"
        ));
        assert!(gitignore_path_glob_matches(
            "doc/**/*.pdf",
            "doc/sub/manual.pdf"
        ));
    }

    #[test]
    fn dir_star_extension_unexpanded_when_parent_has_glob() {
        assert!(!gitignore_path_glob_matches(
            "*/foo/*.pdf",
            "a/foo/sub/x.pdf"
        ));
    }

    #[test]
    fn nested_dir_star_extension() {
        assert!(!gitignore_path_glob_matches(
            "foo/bar/*.c",
            "foo/bar/baz/x.c"
        ));
    }
}

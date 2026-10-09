//! Sparse-checkout pattern parsing and path membership (cone and non-cone).
//!
//! Cone-mode parsing and matching follow Git's `add_pattern_to_hashsets` and
//! `path_matches_pattern_list` closely enough for `read-tree` and plumbing tests.

use std::collections::BTreeSet;

use crate::wildmatch::{wildmatch, WM_PATHNAME};

/// Parsed non-cone sparse-checkout patterns in file order (last match wins).
#[derive(Debug, Clone)]
pub struct NonConePatterns {
    lines: Vec<String>,
}

impl NonConePatterns {
    /// Build from already-trimmed pattern lines (non-cone mode).
    #[must_use]
    pub fn from_lines(lines: Vec<String>) -> Self {
        Self { lines }
    }

    /// Sparse-checkout pattern lines in file order (for Git-style inclusion checks).
    #[must_use]
    pub fn lines(&self) -> &[String] {
        &self.lines
    }

    /// Parse a sparse-checkout file into ordered patterns (non-cone mode).
    #[must_use]
    pub fn parse(content: &str) -> Self {
        let lines = content
            .lines()
            .map(str::trim)
            .filter(|l| !l.is_empty() && !l.starts_with('#'))
            .map(String::from)
            .collect();
        Self { lines }
    }

    /// Returns true if `path` is included after applying ordered negated patterns.
    #[must_use]
    pub fn path_included(&self, path: &str) -> bool {
        let mut included = false;
        for raw in &self.lines {
            let (negated, core) = match raw.strip_prefix('!') {
                Some(rest) => (true, rest),
                None => (false, raw.as_str()),
            };
            let core = core.trim();
            if core.is_empty() || core.starts_with('#') {
                continue;
            }
            if non_cone_line_matches(core, path) {
                included = !negated;
            }
        }
        included
    }
}

/// Mirror Git `dup_and_filter_pattern` (dir.c): drop escape backslashes (once each), then
/// if the result ends in `/*` truncate that trailing `/*`. Operates on a leading-`/` pattern.
fn dup_and_filter_pattern(pattern: &str) -> String {
    let bytes = pattern.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(bytes.len());
    let mut i = 0usize;
    while i < bytes.len() {
        if bytes[i] == b'\\' {
            i += 1;
            if i >= bytes.len() {
                break;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    if out.len() > 2 && out[out.len() - 1] == b'*' && out[out.len() - 2] == b'/' {
        out.truncate(out.len() - 2);
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn glob_special_unescaped(name: &[u8]) -> bool {
    let mut i = 0usize;
    while i < name.len() {
        if name[i] == b'\\' {
            i += 2;
            continue;
        }
        if matches!(name[i], b'*' | b'?' | b'[') {
            return true;
        }
        i += 1;
    }
    false
}

fn sparse_glob_match_star_crosses_slash(pattern: &[u8], text: &[u8]) -> bool {
    // For bracket classes / escapes, defer to full wildmatch with pathname disabled,
    // which keeps sparse non-cone semantics where `*` may span `/`.
    if pattern.contains(&b'[') || pattern.contains(&b'\\') {
        return wildmatch(pattern, text, 0);
    }
    let (mut pi, mut ti) = (0usize, 0usize);
    let (mut star_p, mut star_t) = (usize::MAX, 0usize);
    while ti < text.len() {
        if pi < pattern.len() && (pattern[pi] == b'?' || pattern[pi] == text[ti]) {
            pi += 1;
            ti += 1;
        } else if pi < pattern.len() && pattern[pi] == b'*' {
            star_p = pi;
            star_t = ti;
            pi += 1;
        } else if star_p != usize::MAX {
            pi = star_p + 1;
            star_t += 1;
            ti = star_t;
        } else {
            return false;
        }
    }
    while pi < pattern.len() && pattern[pi] == b'*' {
        pi += 1;
    }
    pi == pattern.len()
}

/// Same semantics as Git's plumbing for sparse-checkout file lines (`*` matches across `/`).
fn sparse_pattern_matches_git_non_cone(pattern: &str, path: &str) -> bool {
    let pat = pattern.trim();
    if pat.is_empty() {
        return false;
    }

    let anchored = pat.starts_with('/');
    let pat = pat.trim_start_matches('/');

    if let Some(dir) = pat.strip_suffix('/') {
        if anchored && dir == "*" {
            return path.contains('/');
        }
        if anchored {
            return path == dir || path.starts_with(&format!("{dir}/"));
        }
        return path == dir
            || path.starts_with(&format!("{dir}/"))
            || path.split('/').any(|component| component == dir);
    }

    if anchored {
        return sparse_glob_match_star_crosses_slash(pat.as_bytes(), path.as_bytes());
    }
    sparse_glob_match_star_crosses_slash(pat.as_bytes(), path.as_bytes())
        || path.rsplit('/').next().is_some_and(|base| {
            sparse_glob_match_star_crosses_slash(pat.as_bytes(), base.as_bytes())
        })
}

fn non_cone_line_matches(pattern: &str, path: &str) -> bool {
    sparse_pattern_matches_git_non_cone(pattern, path)
}

/// Cone-mode sparse state: keys use a leading `/` (Git's internal form).
#[derive(Debug, Clone, Default)]
pub struct ConePatterns {
    pub full_cone: bool,
    pub recursive_slash: BTreeSet<String>,
    pub parent_slash: BTreeSet<String>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum ConeMatch {
    Undecided,
    Matched,
    MatchedRecursive,
    NotMatched,
}

impl ConePatterns {
    /// Parse sparse-checkout lines in cone mode. On structural failure returns `None` and
    /// callers should fall back to non-cone matching (and may print `warnings`).
    #[must_use]
    pub fn try_parse_with_warnings(content: &str, warnings: &mut Vec<String>) -> Option<Self> {
        let lines: Vec<&str> = content
            .lines()
            .map(str::trim)
            .filter(|l| !l.is_empty() && !l.starts_with('#'))
            .collect();

        let mut full_cone = false;
        let mut recursive: BTreeSet<String> = BTreeSet::new();
        let mut parents: BTreeSet<String> = BTreeSet::new();

        for line in lines {
            let (negated, rest) = if let Some(r) = line.strip_prefix('!') {
                (true, r)
            } else {
                (false, line)
            };

            // Git `dir.c:add_pattern_to_hashsets`: negative root-all with directory flag clears
            // full_cone (`!/*` or expanded form `!/*/`); `/*` sets full_cone.
            if negated && (rest == "/*" || rest == "/*/") {
                full_cone = false;
                continue;
            }
            if !negated && rest == "/*" {
                full_cone = true;
                continue;
            }

            // Git `add_pattern_to_hashsets`: a MUSTBEDIR pattern whose stored form (trailing
            // dir-slash dropped) ends in `/*` is the "exclude immediate children" rule. For
            // `!/foo/bar/*/` the stored pattern is `!/foo/bar/*`; `dup_and_filter_pattern`
            // truncates the trailing `/*` to `/foo/bar`, which must already be recursive.
            // This also accepts multi-segment cone parents (e.g. `!/deep/deeper1/*/`) since
            // `/` is not a glob-special character.
            let stored = rest.strip_suffix('/').unwrap_or(rest);
            if stored.starts_with('/')
                && stored.len() > 2
                && stored.ends_with("/*")
                && !stored.ends_with("\\*")
            {
                if !negated {
                    warnings.push(crate::diagnostics::warning_line(&format!(
                        "unrecognized pattern: '{rest}'"
                    )));
                    warnings.push(crate::diagnostics::warning_line(
                        "disabling cone pattern matching",
                    ));
                    return None;
                }
                let key = dup_and_filter_pattern(stored);
                if !recursive.contains(&key) {
                    warnings.push(crate::diagnostics::warning_line(&format!(
                        "unrecognized negative pattern: '{rest}'"
                    )));
                    warnings.push(crate::diagnostics::warning_line(
                        "disabling cone pattern matching",
                    ));
                    return None;
                }
                recursive.remove(&key);
                parents.insert(key);
                continue;
            }

            if negated {
                warnings.push(crate::diagnostics::warning_line(&format!(
                    "unrecognized negative pattern: '{rest}'"
                )));
                warnings.push(crate::diagnostics::warning_line(
                    "disabling cone pattern matching",
                ));
                return None;
            }

            if rest == "/*" {
                continue;
            }

            if !rest.starts_with('/') {
                warnings.push(crate::diagnostics::warning_line(&format!(
                    "unrecognized pattern: '{rest}'"
                )));
                warnings.push(crate::diagnostics::warning_line(
                    "disabling cone pattern matching",
                ));
                return None;
            }
            if rest.contains("**") {
                warnings.push(crate::diagnostics::warning_line(&format!(
                    "unrecognized pattern: '{rest}'"
                )));
                warnings.push(crate::diagnostics::warning_line(
                    "disabling cone pattern matching",
                ));
                return None;
            }
            if rest.len() < 2 {
                warnings.push(crate::diagnostics::warning_line(&format!(
                    "unrecognized pattern: '{rest}'"
                )));
                warnings.push(crate::diagnostics::warning_line(
                    "disabling cone pattern matching",
                ));
                return None;
            }

            let must_be_dir = rest.ends_with('/');
            let body = rest[1..].trim_end_matches('/');
            if body.is_empty() {
                warnings.push(crate::diagnostics::warning_line(&format!(
                    "unrecognized pattern: '{rest}'"
                )));
                warnings.push(crate::diagnostics::warning_line(
                    "disabling cone pattern matching",
                ));
                return None;
            }
            if !must_be_dir {
                warnings.push(crate::diagnostics::warning_line(&format!(
                    "unrecognized pattern: '{rest}'"
                )));
                warnings.push(crate::diagnostics::warning_line(
                    "disabling cone pattern matching",
                ));
                return None;
            }
            if glob_special_unescaped(body.as_bytes()) {
                warnings.push(crate::diagnostics::warning_line(&format!(
                    "unrecognized pattern: '{rest}'"
                )));
                warnings.push(crate::diagnostics::warning_line(
                    "disabling cone pattern matching",
                ));
                return None;
            }

            // Git applies `dup_and_filter_pattern` before hashing: escape backslashes are
            // removed and a trailing `/*` is truncated. So `/foo/\*/` collapses to `/foo`,
            // which lets the duplicate-with-parent check fire (t1091 malformed cone patterns).
            let key = dup_and_filter_pattern(&format!("/{body}"));
            if parents.contains(&key) {
                warnings.push(crate::diagnostics::warning_line(&format!(
                    "your sparse-checkout file may have issues: pattern '{rest}' is repeated"
                )));
                warnings.push(crate::diagnostics::warning_line(
                    "disabling cone pattern matching",
                ));
                return None;
            }
            recursive.insert(key.clone());
            let key_body = key.trim_start_matches('/');
            let parts: Vec<&str> = key_body.split('/').collect();
            for i in 1..parts.len() {
                let prefix = parts[..i].join("/");
                parents.insert(format!("/{prefix}"));
            }
        }

        Some(ConePatterns {
            full_cone,
            recursive_slash: recursive,
            parent_slash: parents,
        })
    }

    #[must_use]
    pub fn try_parse(content: &str) -> Option<Self> {
        let mut w = Vec::new();
        Self::try_parse_with_warnings(content, &mut w)
    }

    fn recursive_contains_parent(path: &str, recursive: &BTreeSet<String>) -> bool {
        let mut buf = String::from("/");
        buf.push_str(path);
        let mut slash_pos = buf.rfind('/');
        while let Some(pos) = slash_pos {
            if pos == 0 {
                break;
            }
            buf.truncate(pos);
            if recursive.contains(&buf) {
                return true;
            }
            slash_pos = buf.rfind('/');
        }
        false
    }

    /// Git `path_matches_pattern_list` for cone mode (`pathname` has no leading slash).
    fn path_matches_pattern_list(&self, pathname: &str) -> ConeMatch {
        if self.full_cone {
            return ConeMatch::Matched;
        }

        let mut parent_pathname = String::with_capacity(pathname.len() + 2);
        parent_pathname.push('/');
        parent_pathname.push_str(pathname);

        let slash_pos = if parent_pathname.ends_with('/') {
            let sp = parent_pathname.len() - 1;
            parent_pathname.push('-');
            sp
        } else {
            parent_pathname.rfind('/').unwrap_or(0)
        };

        if self.recursive_slash.contains(&parent_pathname) {
            return ConeMatch::MatchedRecursive;
        }

        if slash_pos == 0 {
            return ConeMatch::Matched;
        }

        let parent_key = parent_pathname[..slash_pos].to_string();
        if self.parent_slash.contains(&parent_key) {
            return ConeMatch::Matched;
        }

        if Self::recursive_contains_parent(pathname, &self.recursive_slash) {
            return ConeMatch::MatchedRecursive;
        }

        ConeMatch::NotMatched
    }

    /// Whether `path` (repository-relative, no leading slash) is inside the cone.
    #[must_use]
    pub fn path_included(&self, path: &str) -> bool {
        if path.is_empty() {
            return true;
        }

        let bytes = path.as_bytes();
        let mut end = bytes.len();
        let mut match_result = ConeMatch::Undecided;

        while end > 0 && match_result == ConeMatch::Undecided {
            let slice = path.get(..end).unwrap_or("");
            match_result = self.path_matches_pattern_list(slice);

            let mut slash = end.saturating_sub(1);
            while slash > 0 && bytes[slash] != b'/' {
                slash -= 1;
            }
            end = if bytes.get(slash) == Some(&b'/') {
                slash
            } else {
                0
            };
        }

        matches!(
            match_result,
            ConeMatch::Matched | ConeMatch::MatchedRecursive
        )
    }
}

/// Load sparse-checkout file; returns `(cone_parse_ok, cone, non_cone)`.
#[must_use]
pub fn load_sparse_checkout(
    git_dir: &std::path::Path,
    cone_config: bool,
) -> (bool, Option<ConePatterns>, NonConePatterns) {
    let mut w = Vec::new();
    load_sparse_checkout_with_warnings(git_dir, cone_config, &mut w)
}

/// Like [`load_sparse_checkout`] but appends cone-parse warnings (for stderr).
pub fn load_sparse_checkout_with_warnings(
    git_dir: &std::path::Path,
    cone_config: bool,
    warnings: &mut Vec<String>,
) -> (bool, Option<ConePatterns>, NonConePatterns) {
    let path = git_dir.join("info").join("sparse-checkout");
    let Ok(content) = std::fs::read_to_string(&path) else {
        return (false, None, NonConePatterns { lines: Vec::new() });
    };
    let non_cone = NonConePatterns::parse(&content);
    if !cone_config {
        return (false, None, non_cone);
    }
    match ConePatterns::try_parse_with_warnings(&content, warnings) {
        Some(cone) => (true, Some(cone), non_cone),
        None => (false, None, non_cone),
    }
}

/// If `path` is included in the sparse checkout.
#[must_use]
pub fn path_in_sparse_checkout(
    path: &str,
    cone_config: bool,
    cone: Option<&ConePatterns>,
    non_cone: &NonConePatterns,
    work_tree: Option<&std::path::Path>,
) -> bool {
    if cone_config {
        if let Some(c) = cone {
            return c.path_included(path);
        }
    }
    crate::ignore::path_in_sparse_checkout(path, non_cone.lines(), work_tree)
}

/// Apply sparse-checkout rules to `index`: stage-0 entries get `skip-worktree` when excluded.
///
/// Matches Git's sparse-checkout application used after building a new index from a tree
/// (`read-tree`, branch checkout, fast-forward merge). When `core.sparseCheckout` is false or
/// the sparse-checkout file is missing, this is a no-op.
///
/// # Parameters
///
/// - `git_dir` — repository git directory (reads `config` and `info/sparse-checkout`).
/// - `index` — index to update in place; bumped to version 3 when any entry is marked skip-worktree.
/// - `skip_sparse_checkout` — when true (e.g. `read-tree --no-sparse-checkout`), do not set
///   `skip-worktree` bits even if sparse checkout is enabled.
pub fn apply_sparse_checkout_skip_worktree(
    git_dir: &std::path::Path,
    work_tree: Option<&std::path::Path>,
    index: &mut crate::index::Index,
    skip_sparse_checkout: bool,
) -> Vec<String> {
    if skip_sparse_checkout {
        return Vec::new();
    }

    let config = crate::config::ConfigSet::load(
        &crate::environment::Environment::empty(),
        Some(git_dir),
        true,
    )
    .unwrap_or_else(|_| crate::config::ConfigSet::new());
    let sparse_enabled = config
        .get_bool("core.sparsecheckout")
        .and_then(|r| r.ok())
        .unwrap_or(false);

    if !sparse_enabled {
        return Vec::new();
    }

    // Git default for core.sparseCheckoutCone is false (environment.c zero-init). When the key
    // is absent, treat the sparse file as non-cone so we do not try to cone-parse a legitimate
    // non-cone file (e.g. a bare `sub` pattern) and emit spurious "disabling cone pattern
    // matching" warnings on every index update (t1011 subtest 21).
    let cone_config = config
        .get_bool("core.sparsecheckoutcone")
        .and_then(|r| r.ok())
        .unwrap_or(false);

    let mut warnings = Vec::new();
    let (_cone_ok, _cone_loaded, non_cone) =
        load_sparse_checkout_with_warnings(git_dir, cone_config, &mut warnings);

    let sparse_path = git_dir.join("info").join("sparse-checkout");
    let file_content = std::fs::read_to_string(&sparse_path).unwrap_or_default();
    let sparse_lines = parse_sparse_checkout_file(&file_content);

    // Use silent cone parsing here: non-cone files like `sub` are normal and should not emit
    // "disabling cone pattern matching" on every index update (t1011 checkout noise).
    let cone_struct = if cone_config {
        ConePatterns::try_parse(&file_content)
    } else {
        None
    };
    let effective_cone = cone_config && cone_struct.is_some();

    // Git: an on-disk sparse-checkout file with no effective patterns (e.g. a file that only
    // contains blank lines) still enables sparse mode and excludes every path (`pl->nr == 0`
    // yields UNDECIDED → rejected at repo root in `path_in_sparse_checkout_1`).
    let sparse_file_exists = sparse_path.is_file();
    let exclude_all = sparse_file_exists && sparse_lines.is_empty();

    let mut any_skip = false;
    for entry in &mut index.entries {
        if entry.stage() != 0 {
            continue;
        }
        let path_str = String::from_utf8_lossy(&entry.path);
        let included = if exclude_all {
            false
        } else if effective_cone {
            path_in_sparse_checkout(
                path_str.as_ref(),
                true,
                cone_struct.as_ref(),
                &non_cone,
                work_tree,
            )
        } else {
            crate::ignore::path_in_sparse_checkout(path_str.as_ref(), non_cone.lines(), work_tree)
        };
        entry.set_skip_worktree(!included);
        if !included {
            any_skip = true;
        }
    }

    if any_skip && index.version < 3 {
        index.version = 3;
    }
    warnings
}

/// Longest common prefix of `path1` and `path2` that ends at a `/` (Git `max_common_dir_prefix`).
fn max_common_dir_prefix(path1: &str, path2: &str) -> usize {
    let b1 = path1.as_bytes();
    let b2 = path2.as_bytes();
    let mut common_prefix = 0usize;
    let mut i = 0usize;
    while i < b1.len() && i < b2.len() {
        if b1[i] != b2[i] {
            break;
        }
        if b1[i] == b'/' {
            common_prefix = i + 1;
        }
        i += 1;
    }
    common_prefix
}

struct PathFoundData {
    /// Cached path prefix that does not exist, always ending with `/` when non-empty.
    dir: String,
}

/// Whether `path` names an existing file or symlink (Git `path_found` in `sparse-index.c`).
fn path_found(path: &str, data: &mut PathFoundData) -> bool {
    let pb = path.as_bytes();
    let db = data.dir.as_bytes();
    if !db.is_empty() && pb.len() >= db.len() && pb[..db.len()] == *db {
        return false;
    }

    if std::fs::symlink_metadata(std::path::Path::new(path)).is_ok() {
        return true;
    }

    let common_prefix = max_common_dir_prefix(path, &data.dir);
    data.dir.truncate(common_prefix);

    loop {
        let rest = &path[data.dir.len()..];
        if let Some(rel_slash) = rest.find('/') {
            data.dir.push_str(&rest[..=rel_slash]);
            if std::fs::symlink_metadata(std::path::Path::new(&data.dir)).is_err() {
                return false;
            }
        } else {
            data.dir.push_str(rest);
            data.dir.push('/');
            break;
        }
    }
    false
}

/// Clear `skip-worktree` on index entries whose paths exist in the work tree when sparse checkout
/// is enabled, unless `sparse.expectFilesOutsideOfPatterns` is true.
///
/// Matches Git's `clear_skip_worktree_from_present_files` (`sparse-index.c`) for a full
/// (non-sparse-index) in-memory index.
pub fn clear_skip_worktree_from_present_files(
    git_dir: &std::path::Path,
    work_tree: &std::path::Path,
    index: &mut crate::index::Index,
    config: Option<&crate::config::ConfigSet>,
) {
    let config = config.cloned().unwrap_or_else(|| {
        crate::config::ConfigSet::load(
            &crate::environment::Environment::empty(),
            Some(git_dir),
            true,
        )
        .unwrap_or_default()
    });
    let sparse_enabled = config
        .get_bool("core.sparsecheckout")
        .and_then(|r| r.ok())
        .unwrap_or(false);
    if !sparse_enabled {
        return;
    }
    if config
        .get_bool("sparse.expectfilesoutsideofpatterns")
        .and_then(|r| r.ok())
        .unwrap_or(false)
    {
        return;
    }

    let mut found = PathFoundData { dir: String::new() };
    for entry in &mut index.entries {
        if entry.stage() != 0 || !entry.skip_worktree() {
            continue;
        }
        // With assume-unchanged (CE_VALID), Git keeps skip-worktree for `git grep` semantics
        // (t7817: present file + both bits still excluded from work-tree index grep).
        if entry.assume_unchanged() {
            continue;
        }
        let rel = String::from_utf8_lossy(&entry.path);
        let abs = work_tree.join(rel.as_ref());
        let abs_str = abs.to_string_lossy().into_owned();
        if path_found(&abs_str, &mut found) {
            entry.set_skip_worktree(false);
        }
    }
}

/// Mutable cone sparse state (Git `pattern_list` hashmaps) for building `sparse-checkout` files.
#[derive(Debug, Clone, Default)]
pub struct ConeWorkspace {
    pub recursive_slash: BTreeSet<String>,
    pub parent_slash: BTreeSet<String>,
}

impl ConeWorkspace {
    /// Build from parsed cone file content.
    #[must_use]
    pub fn from_cone_patterns(cp: &ConePatterns) -> Self {
        Self {
            recursive_slash: cp.recursive_slash.clone(),
            parent_slash: cp.parent_slash.clone(),
        }
    }

    /// Rebuild from a set of repository-relative directory paths (after pruning descendants).
    #[must_use]
    pub fn from_directory_list(dirs: &[String]) -> Self {
        let mut pruned: Vec<String> = dirs
            .iter()
            .map(|s| s.trim_start_matches('/').trim_end_matches('/').to_string())
            .filter(|s| !s.is_empty())
            .collect();
        pruned.sort();
        let mut kept: Vec<String> = Vec::new();
        for d in pruned {
            if kept
                .iter()
                .any(|p| d.starts_with(p) && d.as_bytes().get(p.len()) == Some(&b'/'))
            {
                continue;
            }
            kept.retain(|k| !(k.starts_with(&d) && k.as_bytes().get(d.len()) == Some(&b'/')));
            kept.push(d);
        }
        let mut ws = ConeWorkspace::default();
        for d in kept {
            ws.insert_directory(&d);
        }
        ws
    }

    /// Insert a repository-relative directory path (no leading slash).
    pub fn insert_directory(&mut self, rel: &str) {
        let rel = rel.trim_start_matches('/');
        let rel = rel.trim_end_matches('/');
        if rel.is_empty() {
            return;
        }
        let key = format!("/{rel}");
        if self.parent_slash.contains(&key) {
            return;
        }
        self.recursive_slash.insert(key.clone());
        let parts: Vec<&str> = rel.split('/').collect();
        for i in 1..parts.len() {
            let prefix = parts[..i].join("/");
            self.parent_slash.insert(format!("/{prefix}"));
        }
    }

    fn recursive_contains_parent(path_slash: &str, recursive: &BTreeSet<String>) -> bool {
        let mut buf = String::from(path_slash);
        let mut slash_pos = buf.rfind('/');
        while let Some(pos) = slash_pos {
            if pos == 0 {
                break;
            }
            buf.truncate(pos);
            if recursive.contains(&buf) {
                return true;
            }
            slash_pos = buf.rfind('/');
        }
        false
    }

    /// Serialize to `.git/info/sparse-checkout` cone format (includes `/*` and `!/*/` header).
    #[must_use]
    pub fn to_sparse_checkout_file(&self) -> String {
        let mut parent_only: Vec<&String> = self
            .parent_slash
            .iter()
            .filter(|p| {
                !self.recursive_slash.contains(*p)
                    && !Self::recursive_contains_parent(p, &self.recursive_slash)
            })
            .collect();
        parent_only.sort();

        let mut out = String::new();
        out.push_str("/*\n!/*/\n");

        for p in parent_only {
            let esc = escape_cone_path_component(p);
            out.push_str(&esc);
            out.push_str("/\n!");
            out.push_str(&esc);
            out.push_str("/*/\n");
        }

        let mut rec_only: Vec<&String> = self
            .recursive_slash
            .iter()
            .filter(|p| !Self::recursive_contains_parent(p, &self.recursive_slash))
            .collect();
        rec_only.sort();

        for p in rec_only {
            let esc = escape_cone_path_component(p);
            out.push_str(&esc);
            out.push_str("/\n");
        }
        out
    }

    /// Directory names for `git sparse-checkout list` in cone mode (no leading slash).
    #[must_use]
    pub fn list_cone_directories(&self) -> Vec<String> {
        let mut v: Vec<String> = self
            .recursive_slash
            .iter()
            .map(|s| s.trim_start_matches('/').to_string())
            .collect();
        v.sort();
        v
    }
}

fn escape_cone_path_component(path_with_leading_slash: &str) -> String {
    let mut out = String::new();
    for ch in path_with_leading_slash.chars() {
        if matches!(ch, '*' | '?' | '[' | '\\') {
            out.push('\\');
        }
        out.push(ch);
    }
    out
}

/// Read non-empty, non-comment lines from `.git/info/sparse-checkout`.
pub fn parse_sparse_checkout_file(content: &str) -> Vec<String> {
    content
        .lines()
        .map(|l| l.trim())
        .filter(|l| !l.is_empty() && !l.starts_with('#'))
        .map(String::from)
        .collect()
}

/// Returns true when the sparse-checkout file uses Git's expanded cone format
/// (starts with `/*` then `!/*/`).
pub fn sparse_checkout_lines_look_like_expanded_cone(lines: &[String]) -> bool {
    lines.len() >= 2 && lines[0] == "/*" && lines[1] == "!/*/"
}

/// Parent and recursive directory prefixes (no leading slash, no trailing slash) from an
/// expanded cone sparse-checkout file, matching Git `write_cone_to_file` layout.
fn parse_expanded_cone_parent_recursive(lines: &[String]) -> Option<(Vec<String>, Vec<String>)> {
    if !sparse_checkout_lines_look_like_expanded_cone(lines) {
        return None;
    }
    let mut parents = Vec::new();
    let mut recursive = Vec::new();
    let mut i = 2usize;
    while i + 1 < lines.len() {
        let a = &lines[i];
        let b = &lines[i + 1];
        if !a.starts_with('/') || !a.ends_with('/') || !b.starts_with('!') {
            break;
        }
        let inner_a = a.trim_start_matches('/').trim_end_matches('/');
        let expected_neg = format!("!/{inner_a}/*/");
        if b != &expected_neg {
            break;
        }
        // On-disk patterns escape glob/backslash chars; match against the literal directory name.
        parents.push(unescape_cone_pattern_path(inner_a));
        i += 2;
    }
    while i < lines.len() {
        let line = &lines[i];
        if line.starts_with('!') {
            return None;
        }
        if !line.starts_with('/') || !line.ends_with('/') {
            return None;
        }
        let body = line.trim_start_matches('/').trim_end_matches('/');
        if body.is_empty() {
            return None;
        }
        recursive.push(unescape_cone_pattern_path(body));
        i += 1;
    }
    Some((parents, recursive))
}

fn path_in_expanded_cone(path: &str, lines: &[String]) -> bool {
    let Some((parents, recursive)) = parse_expanded_cone_parent_recursive(lines) else {
        return false;
    };
    let raw = path.trim_start_matches('/');
    let is_directory_path = raw.ends_with('/');
    let path = raw.trim_end_matches('/');

    if !path.contains('/') {
        // Top-level: files are always in-cone (`/*`). Directories are in-cone only when
        // they lead into an expanded parent/recursive rule (Git uses dtype when matching).
        // Callers pass directories with a trailing slash (e.g. `a/`); files have none.
        if !is_directory_path {
            return true;
        }
        if parents.is_empty() && recursive.is_empty() {
            return false;
        }
        return parents.iter().any(|p| p == path)
            || recursive
                .iter()
                .any(|r| r == path || r.starts_with(&format!("{path}/")));
    }

    for r in &recursive {
        if path == *r || path.starts_with(&format!("{r}/")) {
            return true;
        }
    }

    for p in &parents {
        let p_slash = format!("{}/", p);
        if path == *p {
            return true;
        }
        if !path.starts_with(&p_slash) {
            continue;
        }
        let rest = &path[p_slash.len()..];
        let Some(slash_pos) = rest.find('/') else {
            // Immediate child `p/name`. Git's cone for a parent emits `/p/` plus `!/p/*/`:
            // a FILE directly in `p/` matches `/p/` (the `!/p/*/` exclusion only matches
            // sub-DIRECTORIES, never files), so `sub/d` is in-cone. A directory directly in
            // `p/` is excluded by `!/p/*/` unless a recursive rule re-includes it
            // (e.g. `sub/dir` under parent `sub`).
            if !is_directory_path {
                return true;
            }
            let combined = format!("{}/{}", p, rest);
            return recursive
                .iter()
                .any(|r| r == &combined || r.starts_with(&format!("{combined}/")));
        };
        let first = &rest[..slash_pos];
        let combined = format!("{}/{}", p, first);
        for r in &recursive {
            let under_r = path == *r || path.starts_with(&format!("{r}/"));
            let r_covers = r == &combined || r.starts_with(&format!("{combined}/"));
            if r_covers && under_r {
                return true;
            }
        }
    }

    false
}

/// Cone mode from config combined with on-disk pattern shape.
///
/// Git parses the sparse-checkout file in cone mode only when it matches the
/// expanded template (`/*`, `!/*/`, …). Raw lines like `a` are matched as
/// non-cone patterns even if `core.sparseCheckoutCone` is true.
#[must_use]
pub fn effective_cone_mode_for_sparse_file(cone_config: bool, lines: &[String]) -> bool {
    cone_config && sparse_checkout_lines_look_like_expanded_cone(lines)
}

/// Build the on-disk sparse-checkout contents for cone mode, matching
/// `write_cone_to_file` in Git's `builtin/sparse-checkout.c`.
///
/// `dirs` are worktree-relative directory paths as the user typed them (no
/// leading slash, `/` separators). Empty entries are ignored.
pub fn build_expanded_cone_sparse_checkout_lines(dirs: &[String]) -> Vec<String> {
    let mut recursive: BTreeSet<String> = BTreeSet::new();
    for d in dirs {
        let t = d.trim().trim_start_matches('/').trim_end_matches('/');
        if t.is_empty() {
            continue;
        }
        recursive.insert(format!("/{t}"));
    }

    let mut parents: BTreeSet<String> = BTreeSet::new();
    for r in &recursive {
        let mut cur = r.clone();
        while let Some(slash) = cur.rfind('/') {
            if slash == 0 {
                break;
            }
            cur.truncate(slash);
            parents.insert(cur.clone());
        }
    }

    let mut out = vec!["/*".to_owned(), "!/*/".to_owned()];

    for p in parents.iter() {
        if recursive.contains(p) {
            continue;
        }
        if recursive_set_has_strict_ancestor(&recursive, p) {
            continue;
        }
        let esc = escape_cone_pattern_path(p);
        out.push(format!("{esc}/"));
        out.push(format!("!{esc}/*/"));
    }

    for r in recursive.iter() {
        if recursive_set_has_strict_ancestor(&recursive, r) {
            continue;
        }
        let esc = escape_cone_pattern_path(r);
        out.push(format!("{esc}/"));
    }

    out
}

fn escape_cone_pattern_path(path_with_leading_slash: &str) -> String {
    // Git's `escaped_pattern` escapes backslashes, `[`, `*`, `?`, `#`; keep
    // tests (and normal paths) working with a minimal escape pass.
    let mut out = String::with_capacity(path_with_leading_slash.len() + 8);
    for ch in path_with_leading_slash.chars() {
        match ch {
            '\\' | '[' | '*' | '?' | '#' => {
                out.push('\\');
                out.push(ch);
            }
            _ => out.push(ch),
        }
    }
    out
}

/// Inverse of [`escape_cone_pattern_path`]: drop the escaping backslash before a glob/backslash
/// character so a cone pattern body becomes the plain directory name.
fn unescape_cone_pattern_path(escaped: &str) -> String {
    let mut out = String::with_capacity(escaped.len());
    let mut chars = escaped.chars().peekable();
    while let Some(ch) = chars.next() {
        if ch == '\\' {
            if let Some(&next) = chars.peek() {
                if matches!(next, '\\' | '[' | '*' | '?' | '#') {
                    out.push(next);
                    chars.next();
                    continue;
                }
            }
        }
        out.push(ch);
    }
    out
}

fn recursive_set_has_strict_ancestor(recursive: &BTreeSet<String>, path: &str) -> bool {
    let mut cur = path.to_string();
    while let Some(slash) = cur.rfind('/') {
        if slash == 0 {
            break;
        }
        cur.truncate(slash);
        if recursive.contains(&cur) {
            return true;
        }
    }
    false
}

/// User-facing cone directory names from an expanded on-disk file (`folder1`, not `/folder1/`).
///
/// Git `sparse-checkout list` prints one directory per line for each `/dir/` + `!/dir/*/` pair
/// written by `write_cone_to_file`, not the parent-only prefixes.
#[must_use]
pub fn parse_expanded_cone_user_directories(lines: &[String]) -> Vec<String> {
    if !sparse_checkout_lines_look_like_expanded_cone(lines) {
        return Vec::new();
    }
    let mut i = 2usize;
    let mut out = Vec::new();
    while i < lines.len() {
        let line = &lines[i];
        if line.starts_with('!') {
            i += 1;
            continue;
        }
        if !line.starts_with('/') || !line.ends_with('/') {
            i += 1;
            continue;
        }
        let body = line
            .trim_start_matches('/')
            .trim_end_matches('/')
            .to_string();
        let expected_neg = format!("!/{body}/*/");
        if i + 1 < lines.len() && lines[i + 1] == expected_neg {
            out.push(body);
            i += 2;
            continue;
        }
        i += 1;
    }
    out
}

/// Parse recursive directory paths from an expanded cone sparse-checkout file
/// (for merging on `sparse-checkout add`).
pub fn parse_expanded_cone_recursive_dirs(lines: &[String]) -> Vec<String> {
    if !sparse_checkout_lines_look_like_expanded_cone(lines) {
        return Vec::new();
    }
    let mut i = 2usize;
    let mut out = Vec::new();
    while i < lines.len() {
        let line = &lines[i];
        if line.starts_with('!') {
            i += 1;
            continue;
        }
        if !line.ends_with('/') || !line.starts_with('/') {
            i += 1;
            continue;
        }
        let trimmed = line.trim_end_matches('/');
        let body = trimmed.trim_start_matches('/');
        let esc = escape_cone_pattern_path(trimmed);
        let expected_neg = format!("!{esc}/*/");
        if i + 1 < lines.len() && lines[i + 1] == expected_neg {
            i += 2;
            continue;
        }
        // On-disk cone patterns escape glob/backslash characters (`escaped_pattern`); the
        // directory name itself (Git's recursive hashmap key) is the unescaped form.
        out.push(unescape_cone_pattern_path(body));
        i += 1;
    }
    out
}

/// Directory paths to merge with new inputs for `git sparse-checkout add` when cone mode is on.
///
/// Git loads the existing file with `core.sparseCheckoutCone` set, then checks
/// `existing.use_cone_patterns` after parsing. When the file has the expanded-cone header (`/*`,
/// `!/*/`) but non-cone body lines (e.g. a bare `dir` after `init --no-cone`), the file is not cone
/// mode and the merge uses literal pattern lines as directory names — not
/// [`parse_expanded_cone_recursive_dirs`], which would skip those lines and wrongly treat the file as
/// an empty cone list.
#[must_use]
pub fn cone_directory_inputs_for_add(content: &str) -> Vec<String> {
    let lines: Vec<String> = parse_sparse_checkout_file(content);
    if sparse_checkout_lines_look_like_expanded_cone(&lines) {
        let recursive = parse_expanded_cone_recursive_dirs(&lines);
        if !recursive.is_empty() {
            return recursive;
        }
        if lines.len() == 2 {
            return Vec::new();
        }
        // Header matches expanded cone but body lines are not in expanded form (e.g. bare `dir`
        // after `init --no-cone`). Merge uses those literals — do not strip with
        // `trim_start_matches('/')` on the whole file (would corrupt `/*`).
        return lines[2..]
            .iter()
            .map(|s| {
                s.trim()
                    .trim_start_matches('/')
                    .trim_end_matches('/')
                    .to_string()
            })
            .filter(|s| !s.is_empty())
            .collect();
    }
    if let Some(cp) = ConePatterns::try_parse(content) {
        return ConeWorkspace::from_cone_patterns(&cp).list_cone_directories();
    }
    lines
        .iter()
        .map(|s| {
            s.trim()
                .trim_start_matches('/')
                .trim_end_matches('/')
                .to_string()
        })
        .filter(|s| !s.is_empty())
        .collect()
}

/// Returns true when `path` is included in the sparse-checkout definition.
///
/// Implements parent-directory fallback like Git's `path_in_sparse_checkout`:
/// if the full path does not match, successively shorter prefixes (directory
/// parents) are tried until one matches or the path is exhausted.
///
/// `path` must use `/` separators and be relative to the repository root.
pub fn path_in_sparse_checkout_patterns(path: &str, patterns: &[String], cone_mode: bool) -> bool {
    if path.is_empty() || patterns.is_empty() {
        return true;
    }

    // Git's expanded cone file uses parent + recursive directory rules, not plain gitignore
    // wildmatch on each line (see `write_cone_to_file` / `path_matches_pattern_list`).
    if sparse_checkout_lines_look_like_expanded_cone(patterns) {
        return path_in_expanded_cone(path, patterns);
    }

    // Prefix-directory rules apply to **raw** cone patterns on disk (e.g. `sub`).
    let use_cone_prefix = cone_mode;

    let mut end = path.len();
    while end > 0 {
        if path_matches_sparse_patterns(&path[..end], patterns, use_cone_prefix) {
            return true;
        }
        let Some(slash) = path[..end].rfind('/') else {
            break;
        };
        end = slash;
    }
    false
}

/// Like [`path_in_sparse_checkout_patterns`], but only applies when `cone_enabled` is true.
///
/// When sparse-checkout is not in cone mode, Git treats every path as "in" for
/// this check (backward compatibility for file destinations).
pub fn path_in_cone_mode_sparse_checkout(
    path: &str,
    patterns: &[String],
    cone_enabled: bool,
) -> bool {
    if !cone_enabled || patterns.is_empty() {
        return true;
    }
    path_in_sparse_checkout_patterns(path, patterns, true)
}

/// Returns true when `path` is included, using the same rules as
/// `grit sparse-checkout` / `apply_sparse_patterns`.
pub fn path_matches_sparse_patterns(path: &str, patterns: &[String], cone_mode: bool) -> bool {
    let expanded_cone = sparse_checkout_lines_look_like_expanded_cone(patterns);
    if expanded_cone {
        return path_in_expanded_cone(path, patterns);
    }
    // Raw cone mode (`sparse-checkout set --cone sub` writing only `sub`): directory-prefix rules.
    // Expanded on-disk cone (`/*`, `!/*/`, `/sub/`, …): use full pattern matching like Git.
    let raw_cone_prefix = cone_mode && !expanded_cone;

    if raw_cone_prefix {
        if !path.contains('/') {
            return true;
        }

        for pattern in patterns {
            let prefix = pattern.trim_end_matches('/');
            if path.starts_with(prefix) && path.as_bytes().get(prefix.len()) == Some(&b'/') {
                return true;
            }
            if path == prefix {
                return true;
            }
        }
        return false;
    }

    let mut included = false;
    for raw_pattern in patterns {
        let pattern = raw_pattern.trim();
        if pattern.is_empty() || pattern.starts_with('#') {
            continue;
        }

        let (negated, core_pattern) = if let Some(rest) = pattern.strip_prefix('!') {
            (true, rest)
        } else {
            (false, pattern)
        };
        if core_pattern.is_empty() || core_pattern == "/" {
            continue;
        }

        let matches = if let Some(prefix_with_slash) = core_pattern.strip_suffix('/') {
            // Directory-only patterns: `/a/` or `a/`.
            let inner = prefix_with_slash.trim_start_matches('/');
            if inner.is_empty() {
                false
            } else if inner == "*" {
                // `/*/` and `!/*/` in expanded-cone files: match only nested paths (contain `/`),
                // not every top-level name (plain `wildmatch("*", …)` would match `sub2`, etc.).
                let trimmed = path.trim_end_matches('/');
                trimmed.contains('/')
            } else if inner.contains('*') || inner.contains('?') || inner.contains('[') {
                // e.g. `!/sub/*/` in expanded cone mode
                let pat = format!("{prefix_with_slash}/");
                let text = format!("/{path}/");
                wildmatch(pat.as_bytes(), text.as_bytes(), WM_PATHNAME)
            } else {
                path == inner || path.starts_with(&format!("{inner}/"))
            }
        } else if core_pattern.starts_with('/') {
            // Leading `/` anchors to repo root (same as gitignore / sparse-checkout).
            let text = format!("/{}", path.trim_start_matches('/'));
            wildmatch(core_pattern.as_bytes(), text.as_bytes(), WM_PATHNAME)
        } else {
            wildmatch(core_pattern.as_bytes(), path.as_bytes(), WM_PATHNAME)
        };

        if matches {
            included = !negated;
        }
    }

    included
}

#[cfg(test)]
mod path_in_expanded_cone_tests {
    use super::path_in_sparse_checkout_patterns;

    #[test]
    fn root_only_cone_includes_files_not_top_level_dirs() {
        let lines = vec!["/*".to_string(), "!/*/".to_string()];
        assert!(path_in_sparse_checkout_patterns("file.1.txt", &lines, true));
        assert!(!path_in_sparse_checkout_patterns("a/", &lines, true));
        assert!(!path_in_sparse_checkout_patterns("d/", &lines, true));
    }

    #[test]
    fn expanded_cone_with_d_includes_d_tree_not_sibling_a() {
        let lines = vec!["/*".to_string(), "!/*/".to_string(), "/d/".to_string()];
        assert!(path_in_sparse_checkout_patterns("file.1.txt", &lines, true));
        assert!(path_in_sparse_checkout_patterns("d/", &lines, true));
        assert!(!path_in_sparse_checkout_patterns("a/", &lines, true));
        assert!(path_in_sparse_checkout_patterns(
            "d/e/file.1.txt",
            &lines,
            true
        ));
    }
}

#[cfg(test)]
mod cone_directory_inputs_for_add_tests {
    use super::cone_directory_inputs_for_add;

    #[test]
    fn expanded_header_with_non_cone_body_preserves_literal_dir() {
        let content = "/*\n!/*/\ndir\n";
        assert_eq!(
            cone_directory_inputs_for_add(content),
            vec!["dir".to_string()]
        );
    }

    #[test]
    fn pure_expanded_cone_uses_recursive_dirs_only() {
        let content = "/*\n!/*/\n/sub/\n";
        assert_eq!(
            cone_directory_inputs_for_add(content),
            vec!["sub".to_string()]
        );
    }
}

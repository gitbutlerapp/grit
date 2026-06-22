//! Merge commit and combined (`--cc` / `-c`) diff helpers.
//!
//! These mirror the subset of Git's combine-diff output needed for porcelain
//! commands (`git show`, `git diff` during conflicts, `git diff-tree -c`).

use std::io::Write;
use std::path::Path;
use std::process::{Command, Stdio};

use similar::{ChangeTag, TextDiff};
use tempfile::NamedTempFile;

use crate::combined_diff_patch::{format_combined_diff_body, CombinedDiffWsOptions};
use crate::combined_tree_diff::CombinedParentSide;
use crate::config::{parse_bool, ConfigSet};
use crate::crlf::{get_file_attrs, load_gitattributes, DiffAttr, FileAttrs};
use crate::diff::{detect_renames, diff_trees, DiffStatus};
use crate::objects::{parse_commit, parse_tree, ObjectId, ObjectKind};
use crate::odb::Odb;
use crate::quote_path::format_diff_path_with_prefix;
use crate::textconv_cache::{read_textconv_cache, write_textconv_cache};

/// Paths that differ between the merge result tree and **every** parent tree.
#[must_use]
pub fn combined_diff_paths(odb: &Odb, commit_tree: &ObjectId, parents: &[ObjectId]) -> Vec<String> {
    if parents.len() < 2 {
        return Vec::new();
    }
    let mut per_parent: Vec<std::collections::HashSet<String>> = Vec::new();
    for p in parents {
        let Ok(po) = odb.read(p) else {
            continue;
        };
        let Ok(pc) = parse_commit(&po.data) else {
            continue;
        };
        let Ok(entries) = diff_trees(odb, Some(&pc.tree), Some(commit_tree), "") else {
            continue;
        };
        let paths: std::collections::HashSet<String> =
            entries.iter().map(|e| e.path().to_string_lossy().into_owned()).collect();
        per_parent.push(paths);
    }
    if per_parent.is_empty() {
        return Vec::new();
    }
    let mut common = per_parent[0].clone();
    for s in &per_parent[1..] {
        common = common.intersection(s).cloned().collect();
    }
    if common.is_empty() {
        return Vec::new();
    }
    let mut ordered = paths_in_tree_order(odb, commit_tree, "", &common);
    // Paths removed from the merge result are not present in `commit_tree`, so a merge-tree walk
    // alone would miss them. Git still lists them in combined diff when every parent changed
    // (`t4057-diff-combined-paths` merge + `git rm` amend).
    if ordered.len() < common.len() {
        let seen: std::collections::HashSet<String> = ordered.iter().cloned().collect();
        let mut rest: Vec<String> = common.difference(&seen).cloned().collect();
        rest.sort();
        ordered.extend(rest);
    }
    ordered
}

/// Per-parent blob paths for a combined merge path when rename detection is enabled.
///
/// Returns `None` when no special mapping is needed (each parent reads `merge_path`).
#[must_use]
pub fn combined_merge_parent_blob_paths(
    odb: &Odb,
    merge_path: &str,
    parent_trees: &[ObjectId],
    result_tree: &ObjectId,
    rename_threshold: u32,
) -> Option<Vec<String>> {
    if parent_trees.len() < 2 {
        return None;
    }
    let mut per_parent: Vec<String> = Vec::with_capacity(parent_trees.len());
    for t in parent_trees {
        if blob_oid_at_path(odb, t, merge_path).is_some() {
            per_parent.push(merge_path.to_string());
        } else {
            per_parent.push(String::new());
        }
    }
    if per_parent.iter().all(|p| !p.is_empty()) {
        return None;
    }
    let mut any_rename = false;
    for (i, t) in parent_trees.iter().enumerate() {
        if !per_parent[i].is_empty() {
            continue;
        }
        // Run the *full* per-parent diff (not restricted to `merge_path`): the rename source is a
        // path that was deleted relative to this parent, so it lives outside `merge_path`. Git's
        // `find_paths_generic` likewise runs a full rename-detecting diff against each parent and
        // intersects on the result path.
        let entries = diff_trees(odb, Some(t), Some(result_tree), "").ok()?;
        let with_rn = detect_renames(odb, None, entries, rename_threshold);
        let mut found: Option<String> = None;
        for e in with_rn {
            if e.status != DiffStatus::Renamed {
                continue;
            }
            let Some(new_p) = e.new_path.as_deref() else {
                continue;
            };
            if new_p != merge_path {
                continue;
            }
            // TODO(byte-paths): merge path interop stays lossy until rename sources migrate (Phase 4)
            let old_p = e.old_path.as_ref()?.to_string_lossy().into_owned();
            if blob_oid_at_path(odb, t, &old_p).is_some() {
                if found.is_some() {
                    return None;
                }
                found = Some(old_p);
            }
        }
        let p = found?;
        per_parent[i] = p;
        any_rename = true;
    }
    any_rename.then_some(per_parent)
}

/// Mode of the blob at `path` in `tree`, or `None` if missing / not a blob.
fn blob_mode_at_path(odb: &Odb, tree: &ObjectId, path: &str) -> Option<u32> {
    let mut current = *tree;
    let parts: Vec<&str> = path.split('/').collect();
    for (pi, part) in parts.iter().enumerate() {
        let obj = odb.read(&current).ok()?;
        let entries = crate::objects::parse_tree(&obj.data).ok()?;
        let found = entries
            .iter()
            .find(|e| std::str::from_utf8(&e.name).ok() == Some(*part))?;
        if pi + 1 == parts.len() {
            return Some(found.mode);
        }
        if found.mode != 0o040000 {
            return None;
        }
        current = found.oid;
    }
    None
}

/// Enrich a combined diff path with per-parent rename info (`-M`/`-C`).
///
/// For each parent that the merge-tree walk classified as `Added` (the result name does not exist
/// in that parent), run a full rename-detecting diff against that parent. If the result path is the
/// target of a rename, rewrite that parent side to `Renamed`, fill in the source blob's mode/OID,
/// and record the source name. This mirrors git's `find_paths_generic`, which intersects per-parent
/// rename-detecting diffs (producing `RR` with all source names under `--combined-all-paths`).
pub fn enrich_combined_path_renames(
    odb: &Odb,
    path: &mut crate::combined_tree_diff::CombinedDiffPath,
    parent_trees: &[ObjectId],
    result_tree: &ObjectId,
    rename_threshold: u32,
) {
    use crate::combined_tree_diff::CombinedParentStatus;
    if parent_trees.len() != path.parents.len() {
        return;
    }
    let Some(parent_paths) = combined_merge_parent_blob_paths(
        odb,
        &path.path,
        parent_trees,
        result_tree,
        rename_threshold,
    ) else {
        return;
    };
    for (i, side) in path.parents.iter_mut().enumerate() {
        if side.status != CombinedParentStatus::Added {
            continue;
        }
        let src = &parent_paths[i];
        if src.is_empty() || src == &path.path {
            continue;
        }
        let (Some(oid), Some(mode)) = (
            blob_oid_at_path(odb, &parent_trees[i], src),
            blob_mode_at_path(odb, &parent_trees[i], src),
        ) else {
            continue;
        };
        side.status = CombinedParentStatus::Renamed;
        side.oid = oid;
        side.mode = mode;
        side.rename_from = Some(src.clone());
    }
}

/// All blob paths in `tree_oid`, depth-first in Git tree entry order (for `diff` / `log`
/// `--rotate-to` / `--skip-to`).
#[must_use]
pub fn all_blob_paths_in_tree_order(odb: &Odb, tree_oid: &ObjectId) -> Vec<String> {
    all_blob_paths_dfs(odb, tree_oid, "")
}

fn all_blob_paths_dfs(odb: &Odb, tree_oid: &ObjectId, prefix: &str) -> Vec<String> {
    let Ok(obj) = odb.read(tree_oid) else {
        return Vec::new();
    };
    if obj.kind != ObjectKind::Tree {
        return Vec::new();
    }
    let Ok(entries) = parse_tree(&obj.data) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for e in entries {
        let name = String::from_utf8_lossy(&e.name);
        let path = if prefix.is_empty() {
            name.into_owned()
        } else {
            format!("{prefix}/{name}")
        };
        if e.mode == 0o040000 {
            out.extend(all_blob_paths_dfs(odb, &e.oid, &path));
        } else {
            out.push(path);
        }
    }
    out
}

/// List paths under `prefix` that appear in `want`, following merge-tree entry order (Git
/// `traverse_trees` order), not lexicographic sorting.
fn paths_in_tree_order(
    odb: &Odb,
    tree_oid: &ObjectId,
    prefix: &str,
    want: &std::collections::HashSet<String>,
) -> Vec<String> {
    let Ok(obj) = odb.read(tree_oid) else {
        return Vec::new();
    };
    if obj.kind != ObjectKind::Tree {
        return Vec::new();
    }
    let Ok(entries) = parse_tree(&obj.data) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for e in entries {
        let name = String::from_utf8_lossy(&e.name);
        let path = if prefix.is_empty() {
            name.into_owned()
        } else {
            format!("{prefix}/{name}")
        };
        if e.mode == 0o040000 {
            out.extend(paths_in_tree_order(odb, &e.oid, &path, want));
        } else if want.contains(&path) {
            out.push(path);
        }
    }
    out
}

/// Load attributes for `path` using root `.gitattributes` and `info/attributes`.
fn attrs_for_repo_path(git_dir: &Path, path: &str) -> FileAttrs {
    let work_tree = git_dir.parent().unwrap_or(git_dir);
    let rules = load_gitattributes(work_tree);
    let config = ConfigSet::load(Some(git_dir), true).unwrap_or_default();
    get_file_attrs(&rules, path, false, &config)
}

/// True if diff should treat this path as binary (NUL in blob or `-diff` / `diff=unset`).
#[must_use]
pub fn is_binary_for_diff(git_dir: &Path, path: &str, blob: &[u8]) -> bool {
    let fa = attrs_for_repo_path(git_dir, path);
    if matches!(fa.diff_attr, DiffAttr::Unset) {
        return true;
    }
    crate::crlf::is_binary(blob)
}

/// True when `diff.<driver>.binary` is set for this path's `diff=<driver>` attribute.
fn diff_driver_binary_config(config: &ConfigSet, driver: &str) -> bool {
    let key = format!("diff.{driver}.binary");
    config
        .get(&key)
        .is_some_and(|v| parse_bool(v.as_str()).unwrap_or(false))
}

/// Force `Binary files ... differ` when the path's diff driver sets `binary`, except for symlinks.
///
/// Matches Git's `diff_filespec_is_binary` driver flag: `diff.<name>.binary` applies to paths
/// using that driver, but symlink modes (`120000`) still emit textual symlink-target patches
/// (t4011).
#[must_use]
pub fn diff_forced_binary_by_driver(
    git_dir: &Path,
    config: &ConfigSet,
    path: &str,
    old_mode: &str,
    new_mode: &str,
) -> bool {
    let fa = attrs_for_repo_path(git_dir, path);
    let DiffAttr::Driver(driver) = fa.diff_attr else {
        return false;
    };
    if !diff_driver_binary_config(config, &driver) {
        return false;
    }
    if old_mode == "120000" || new_mode == "120000" {
        return false;
    }
    true
}

/// True when Git would wrap the textconv command with `sh -c 'cmd "$@"' -- ...`
/// (`prepare_shell_cmd` in Git's `run-command.c`).
fn textconv_cmd_needs_shell_wrapper(cmd_line: &str) -> bool {
    cmd_line.chars().any(|c| {
        matches!(
            c,
            '|' | '&'
                | ';'
                | '<'
                | '>'
                | '('
                | ')'
                | '$'
                | '`'
                | '\\'
                | '"'
                | '\''
                | ' '
                | '\t'
                | '\n'
                | '*'
                | '?'
                | '['
                | '#'
                | '~'
                | '='
                | '%'
        )
    })
}

/// Run `diff.<driver>.textconv` on `input`; returns raw stdout on success.
///
/// Matches Git's `run_textconv` / `prepare_shell_cmd`: by default the blob is written to a
/// temporary file and passed as an argument after `--`. Commands that contain shell
/// metacharacters (including spaces) use `sh -c 'pgm "$@"' -- pgm <tempfile>`. Config lines
/// ending with ` <` use stdin instead of a tempfile.
pub fn run_textconv_raw(
    command_cwd: &Path,
    config: &ConfigSet,
    driver: &str,
    input: &[u8],
) -> Option<Vec<u8>> {
    let mut cmd_line = config.get(&format!("diff.{driver}.textconv"))?;
    cmd_line = cmd_line.trim_end().to_string();
    let stdin_mode = if cmd_line.ends_with('<') {
        let t = cmd_line.trim_end_matches('<').trim_end();
        cmd_line = t.to_string();
        true
    } else {
        false
    };
    if stdin_mode {
        let mut child = Command::new("sh")
            .arg("-c")
            .arg(&cmd_line)
            .current_dir(command_cwd)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .ok()?;
        let mut stdin = child.stdin.take()?;
        stdin.write_all(input).ok()?;
        drop(stdin);
        let out = child.wait_with_output().ok()?;
        return if out.status.success() {
            Some(out.stdout)
        } else {
            None
        };
    }

    let mut tmp = NamedTempFile::new().ok()?;
    tmp.write_all(input).ok()?;
    tmp.flush().ok()?;
    let path = tmp.path().to_owned();

    let out = if textconv_cmd_needs_shell_wrapper(&cmd_line) {
        Command::new("sh")
            .current_dir(command_cwd)
            .arg("-c")
            .arg(format!("{} \"$@\"", cmd_line))
            .arg(&cmd_line)
            .arg(&path)
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .output()
            .ok()?
    } else {
        Command::new("sh")
            .current_dir(command_cwd)
            .arg(&cmd_line)
            .arg(&path)
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .output()
            .ok()?
    };

    if !out.status.success() {
        return None;
    }
    Some(out.stdout)
}

/// Run `diff.<driver>.textconv` feeding `input` on stdin; returns UTF-8 lossy text on success.
pub fn run_textconv(
    command_cwd: &Path,
    config: &ConfigSet,
    driver: &str,
    input: &[u8],
) -> Option<String> {
    run_textconv_raw(command_cwd, config, driver, input)
        .map(|b| String::from_utf8_lossy(&b).into_owned())
}

pub fn diff_textconv_cmd_line(config: &ConfigSet, driver: &str) -> Option<String> {
    let mut cmd_line = config.get(&format!("diff.{driver}.textconv"))?;
    cmd_line = cmd_line.trim_end().to_string();
    if cmd_line.ends_with('<') {
        let t = cmd_line.trim_end_matches('<').trim_end();
        cmd_line = t.to_string();
    }
    Some(cmd_line)
}

pub fn diff_cachetextconv_enabled(config: &ConfigSet, driver: &str) -> bool {
    config
        .get(&format!("diff.{driver}.cachetextconv"))
        .map(|v| matches!(v.to_ascii_lowercase().as_str(), "true" | "yes" | "1" | "on"))
        .unwrap_or(false)
}

/// Returns true when `path` has a `diff=<driver>` attribute and `diff.<driver>.textconv` is set.
///
/// When this holds, Git treats the path as textual for diff purposes (even if the blob contains
/// NUL), running textconv instead of emitting `Binary files differ`.
#[must_use]
pub fn diff_textconv_active(git_dir: &Path, config: &ConfigSet, path: &str) -> bool {
    let fa = attrs_for_repo_path(git_dir, path);
    let DiffAttr::Driver(ref driver) = fa.diff_attr else {
        return false;
    };
    diff_textconv_cmd_line(config, driver).is_some()
}

/// True when a path carries the `-diff` (or `diff=unset` / `binary`) attribute,
/// which forces Git to emit `Binary files ... differ` regardless of content.
#[must_use]
pub fn diff_attr_forces_binary(git_dir: &Path, path: &str) -> bool {
    matches!(
        attrs_for_repo_path(git_dir, path).diff_attr,
        DiffAttr::Unset
    )
}

/// True when a path carries a bare `diff` (set) attribute, which forces Git to
/// produce a textual diff even when the blob contains NUL bytes (t4020 #65).
#[must_use]
pub fn diff_attr_forces_text(git_dir: &Path, path: &str) -> bool {
    matches!(attrs_for_repo_path(git_dir, path).diff_attr, DiffAttr::Set)
}

/// Resolved external diff driver from a path's `diff=<name>` attribute.
///
/// Mirrors Git's `userdiff_find_by_path` → `drv->external`: when a path has a
/// `diff=<name>` attribute and `diff.<name>.command` is configured, that driver
/// takes precedence over `GIT_EXTERNAL_DIFF` / `diff.external`. The boolean is
/// `diff.<name>.trustExitCode`.
#[must_use]
pub fn diff_attr_external_driver(
    git_dir: &Path,
    config: &ConfigSet,
    path: &str,
) -> Option<(String, bool)> {
    let fa = attrs_for_repo_path(git_dir, path);
    let DiffAttr::Driver(ref driver) = fa.diff_attr else {
        return None;
    };
    let cmd = config.get(&format!("diff.{driver}.command"))?;
    if cmd.trim().is_empty() {
        return None;
    }
    let trust = config
        .get(&format!("diff.{driver}.trustExitCode"))
        .and_then(|v| parse_bool(v.as_str()).ok())
        .unwrap_or(false);
    Some((cmd, trust))
}

fn textconv_command_cwd(git_dir: &Path) -> std::path::PathBuf {
    git_dir.parent().unwrap_or(git_dir).to_path_buf()
}

fn blob_text_for_diff_inner(
    odb: Option<&Odb>,
    git_dir: &Path,
    config: &ConfigSet,
    path: &str,
    blob: &[u8],
    blob_oid: Option<&ObjectId>,
    use_textconv: bool,
) -> String {
    if !use_textconv {
        return String::from_utf8_lossy(blob).into_owned();
    }
    let fa = attrs_for_repo_path(git_dir, path);
    let DiffAttr::Driver(ref driver) = fa.diff_attr else {
        return String::from_utf8_lossy(blob).into_owned();
    };
    let Some(cmd_line) = diff_textconv_cmd_line(config, driver) else {
        return String::from_utf8_lossy(blob).into_owned();
    };
    let want_cache = diff_cachetextconv_enabled(config, driver);
    if want_cache {
        if let (Some(odb), Some(oid)) = (odb, blob_oid) {
            if let Some(bytes) = read_textconv_cache(odb, git_dir, driver, &cmd_line, oid) {
                return String::from_utf8_lossy(&bytes).into_owned();
            }
        }
    }
    let cwd = textconv_command_cwd(git_dir);
    let Some(t) = run_textconv(&cwd, config, driver, blob) else {
        return String::from_utf8_lossy(blob).into_owned();
    };
    if want_cache {
        if let (Some(odb), Some(oid)) = (odb, blob_oid) {
            write_textconv_cache(odb, git_dir, driver, &cmd_line, oid, t.as_bytes());
        }
    }
    t
}

/// Like [`blob_text_for_diff`], but uses `refs/notes/textconv/<driver>` when
/// `diff.<driver>.cachetextconv` is true and `blob_oid` is known.
#[must_use]
pub fn blob_text_for_diff_with_oid(
    odb: &Odb,
    git_dir: &Path,
    config: &ConfigSet,
    path: &str,
    blob: &[u8],
    blob_oid: &ObjectId,
    use_textconv: bool,
) -> String {
    blob_text_for_diff_inner(
        Some(odb),
        git_dir,
        config,
        path,
        blob,
        Some(blob_oid),
        use_textconv,
    )
}

/// Blob bytes after smudge/EOL conversion for `path`, using the same rules as checkout.
///
/// `index` is used to pick up `.gitattributes` from the index when the worktree file is
/// missing; pass `None` to use only on-disk `.gitattributes` under `work_tree`.
pub fn convert_blob_to_worktree_for_path(
    git_dir: &Path,
    work_tree: &Path,
    index: Option<&crate::index::Index>,
    odb: &Odb,
    path: &str,
    blob: &[u8],
    oid_hex: Option<&str>,
) -> std::io::Result<Vec<u8>> {
    let config = ConfigSet::load(Some(git_dir), true).unwrap_or_default();
    let conv = crate::crlf::ConversionConfig::from_config(&config);
    let rules = match index {
        Some(idx) => crate::crlf::load_gitattributes_for_checkout(work_tree, path, idx, odb),
        None => crate::crlf::load_gitattributes(work_tree),
    };
    let file_attrs = crate::crlf::get_file_attrs(&rules, path, false, &config);
    crate::crlf::convert_to_worktree_eager(blob, path, &conv, &file_attrs, oid_hex, None)
        .map_err(std::io::Error::other)
}

/// Prepare blob bytes for diff: optional textconv when `use_textconv` and `diff=<driver>`.
///
/// Does not read or write the textconv notes cache; use [`blob_text_for_diff_with_oid`] when the
/// blob OID is known (e.g. commit diffs with `cachetextconv`).
pub fn blob_text_for_diff(
    git_dir: &Path,
    config: &ConfigSet,
    path: &str,
    blob: &[u8],
    use_textconv: bool,
) -> String {
    blob_text_for_diff_inner(None, git_dir, config, path, blob, None, use_textconv)
}

/// `diff --git` against parent `p` for merge commit `-m` output.
#[allow(clippy::too_many_arguments)]
pub fn format_parent_patch(
    git_dir: &Path,
    config: &ConfigSet,
    odb: &Odb,
    path: &str,
    parent_tree: &ObjectId,
    result_tree: &ObjectId,
    abbrev: usize,
    context: usize,
    use_textconv: bool,
) -> Option<String> {
    let entries = diff_trees(odb, Some(parent_tree), Some(result_tree), "").ok()?;
    let entry = entries.iter().find(|e| e.path() == path)?;
    if entry.status == DiffStatus::Unmerged {
        return None;
    }

    let old_blob = read_blob(odb, &entry.old_oid);
    let new_blob = read_blob(odb, &entry.new_oid);
    let textconv_for_patch = use_textconv && diff_textconv_active(git_dir, config, path);
    let binary = !textconv_for_patch
        && (is_binary_for_diff(git_dir, path, &old_blob)
            || is_binary_for_diff(git_dir, path, &new_blob));

    let old_abbrev = abbrev_hex(&entry.old_oid, abbrev);
    let new_abbrev = abbrev_hex(&entry.new_oid, abbrev);

    let mut out = String::new();
    out.push_str(&format!("diff --git a/{path} b/{path}\n"));
    // Header lines depend on whether the file was added / deleted / mode-changed / modified,
    // matching git's `diff --git` body (added => `new file mode`, deleted => `deleted file mode`).
    let (old_disp, new_disp) = match entry.status {
        DiffStatus::Added => {
            out.push_str(&format!("new file mode {}\n", entry.new_mode));
            out.push_str(&format!("index {old_abbrev}..{new_abbrev}\n"));
            ("/dev/null".to_string(), format!("b/{path}"))
        }
        DiffStatus::Deleted => {
            out.push_str(&format!("deleted file mode {}\n", entry.old_mode));
            out.push_str(&format!("index {old_abbrev}..{new_abbrev}\n"));
            (format!("a/{path}"), "/dev/null".to_string())
        }
        _ => {
            if entry.old_mode != entry.new_mode {
                out.push_str(&format!("old mode {}\n", entry.old_mode));
                out.push_str(&format!("new mode {}\n", entry.new_mode));
                out.push_str(&format!("index {old_abbrev}..{new_abbrev}\n"));
            } else {
                out.push_str(&format!(
                    "index {old_abbrev}..{new_abbrev} {}\n",
                    entry.new_mode
                ));
            }
            (format!("a/{path}"), format!("b/{path}"))
        }
    };

    if binary {
        out.push_str(&format!("Binary files {old_disp} and {new_disp} differ\n"));
        return Some(out);
    }

    let old_t = if textconv_for_patch {
        blob_text_for_diff_with_oid(odb, git_dir, config, path, &old_blob, &entry.old_oid, true)
    } else {
        blob_text_for_diff(git_dir, config, path, &old_blob, use_textconv)
    };
    let new_t = if textconv_for_patch {
        blob_text_for_diff_with_oid(odb, git_dir, config, path, &new_blob, &entry.new_oid, true)
    } else {
        blob_text_for_diff(git_dir, config, path, &new_blob, use_textconv)
    };
    // Empty prefixes here because `old_disp`/`new_disp` already carry the `a/`/`b/` (or
    // `/dev/null`) decoration computed from the file status above.
    let patch = crate::diff::unified_diff_with_prefix(
        &old_t,
        &new_t,
        &old_disp,
        &new_disp,
        context,
        0,
        "",
        "",
        true,
        config.quote_path_fully(),
    );
    out.push_str(&patch);
    Some(out)
}

/// Combined diff header: `diff --combined` or `diff --cc`.
pub fn format_combined_binary_header(
    path: &str,
    parent_oids: &[ObjectId],
    result_oid: &ObjectId,
    abbrev: usize,
    use_cc_word: bool,
) -> String {
    format_combined_binary_header_n(path, parent_oids, result_oid, abbrev, use_cc_word)
}

/// `index` line for N-parent combined/binary diffs (`p1,p2,...pn..result`).
#[must_use]
pub fn format_combined_binary_header_n(
    path: &str,
    parent_oids: &[ObjectId],
    result_oid: &ObjectId,
    abbrev: usize,
    use_cc_word: bool,
) -> String {
    let idx: Vec<String> = parent_oids.iter().map(|o| abbrev_hex(o, abbrev)).collect();
    let res = abbrev_hex(result_oid, abbrev);
    let kind = if use_cc_word { "cc" } else { "combined" };
    format!(
        "diff --{kind} {path}\nindex {}..{res}\nBinary files differ\n",
        idx.join(",")
    )
}

/// Full combined diff for a binary path (two parents).
pub fn format_combined_binary(
    path: &str,
    parent_oids: &[ObjectId],
    result_oid: &ObjectId,
    abbrev: usize,
    use_cc_word: bool,
) -> String {
    format_combined_binary_header_n(path, parent_oids, result_oid, abbrev, use_cc_word)
}

fn push_combined_file_headers(
    out: &mut String,
    merge_path: &str,
    parent_paths: &[String],
    parent_sides: &[CombinedParentSide],
    combined_all_paths: bool,
    quote_path_fully: bool,
) {
    let a_prefix = "a/";
    let b_prefix = "b/";
    if combined_all_paths {
        for (i, p) in parent_paths.iter().enumerate() {
            // Show `/dev/null` only when this parent genuinely lacks the path (a true add). When a
            // per-parent rename was detected, `parent_paths[i]` holds the renamed-from path (which
            // the parent *does* have), so emit that name even though the merge-tree walk classified
            // the result side as "Added".
            let added_no_rename = parent_sides.get(i).is_some_and(|s| {
                s.status == crate::combined_tree_diff::CombinedParentStatus::Added
            }) && (p.is_empty() || p == merge_path);
            if added_no_rename {
                out.push_str("--- /dev/null\n");
            } else {
                let line = format_diff_path_with_prefix(a_prefix, p, quote_path_fully);
                out.push_str("--- ");
                out.push_str(&line);
                out.push('\n');
            }
        }
        let line = format_diff_path_with_prefix(b_prefix, merge_path, quote_path_fully);
        out.push_str("+++ ");
        out.push_str(&line);
        out.push('\n');
    } else {
        let la = format_diff_path_with_prefix(a_prefix, merge_path, quote_path_fully);
        let lb = format_diff_path_with_prefix(b_prefix, merge_path, quote_path_fully);
        out.push_str("--- ");
        out.push_str(&la);
        out.push('\n');
        out.push_str("+++ ");
        out.push_str(&lb);
        out.push('\n');
    }
}

/// Combined text diff with optional textconv (N parents, single merge path).
///
/// `parent_blob_paths` — when set, length must match `parent_trees`; each entry is the path
/// used to read that parent's blob (for `--combined-all-paths` rename cases). When `None`,
/// every parent uses `path`.
#[allow(clippy::too_many_arguments)]
pub fn format_combined_textconv_patch(
    git_dir: &Path,
    config: &ConfigSet,
    odb: &Odb,
    path: &str,
    parent_trees: &[ObjectId],
    result_tree: &ObjectId,
    abbrev: usize,
    context: usize,
    use_cc_word: bool,
    use_textconv: bool,
    ws: CombinedDiffWsOptions,
    combined_all_paths: bool,
    parent_blob_paths: Option<&[String]>,
    parent_sides: &[CombinedParentSide],
    quote_path_fully: bool,
) -> Option<String> {
    if parent_trees.len() < 2 {
        return None;
    }
    let parent_paths: Vec<&str> = if let Some(ps) = parent_blob_paths {
        if ps.len() != parent_trees.len() {
            return None;
        }
        ps.iter().map(|s| s.as_str()).collect()
    } else {
        vec![path; parent_trees.len()]
    };

    let mut parent_blobs = Vec::with_capacity(parent_trees.len());
    let mut parent_oids = Vec::with_capacity(parent_trees.len());
    for (i, t) in parent_trees.iter().enumerate() {
        let p = parent_paths[i];
        // A parent that lacks the path contributes /dev/null (empty content, zero OID): the path
        // was added relative to that parent. Git still emits the combined patch (`AA`/`new file`),
        // so do not bail out here.
        match blob_oid_at_path(odb, t, p) {
            Some(oid) => {
                parent_blobs.push(read_blob(odb, &oid));
                parent_oids.push(oid);
            }
            None => {
                parent_blobs.push(Vec::new());
                parent_oids.push(ObjectId::zero());
            }
        }
    }
    let result_blob = read_blob_at_path(odb, result_tree, path)?;
    let roid = blob_oid_at_path(odb, result_tree, path)?;

    let textconv_for_patch = use_textconv && diff_textconv_active(git_dir, config, path);
    if !textconv_for_patch
        && (parent_blobs
            .iter()
            .any(|b| is_binary_for_diff(git_dir, path, b))
            || is_binary_for_diff(git_dir, path, &result_blob))
    {
        return Some(format_combined_binary(
            path,
            &parent_oids,
            &roid,
            abbrev,
            use_cc_word,
        ));
    }

    let mut parent_texts = Vec::with_capacity(parent_trees.len());
    for (i, blob) in parent_blobs.iter().enumerate() {
        let p = parent_paths[i];
        let oid = &parent_oids[i];
        let t = if textconv_for_patch {
            blob_text_for_diff_with_oid(odb, git_dir, config, p, blob, oid, true)
        } else {
            blob_text_for_diff(git_dir, config, p, blob, use_textconv)
        };
        parent_texts.push(t);
    }
    let tr = if textconv_for_patch {
        blob_text_for_diff_with_oid(odb, git_dir, config, path, &result_blob, &roid, true)
    } else {
        blob_text_for_diff(git_dir, config, path, &result_blob, use_textconv)
    };

    let idx: Vec<String> = parent_oids.iter().map(|o| abbrev_hex(o, abbrev)).collect();
    let ra = abbrev_hex(&roid, abbrev);
    let kind = if use_cc_word { "cc" } else { "combined" };

    let header_paths: Vec<String> = if combined_all_paths {
        parent_paths.iter().map(|s| (*s).to_string()).collect()
    } else {
        Vec::new()
    };

    let mut out = String::new();
    out.push_str(&format!("diff --{kind} {path}\n"));
    out.push_str(&format!("index {}..{ra}\n", idx.join(",")));
    if combined_all_paths {
        push_combined_file_headers(
            &mut out,
            path,
            &header_paths,
            parent_sides,
            true,
            quote_path_fully,
        );
    } else {
        push_combined_file_headers(&mut out, path, &[], parent_sides, false, quote_path_fully);
    }
    out.push_str(&format_combined_diff_body(
        &parent_texts,
        &tr,
        context,
        use_cc_word,
        ws,
    ));
    Some(out)
}

/// Combined `diff --cc` for an unmerged **gitlink** path when stage blobs are absent from the ODB
/// (e.g. `t4027` synthetic `1ff…` / `2ff…` OIDs). Uses full hex in `Subproject commit` lines like Git.
#[must_use]
pub fn format_gitlink_unmerged_conflict_combined(
    path: &str,
    stage2_oid: &ObjectId,
    stage3_oid: &ObjectId,
    result_subproject_line: &str,
    abbrev: usize,
) -> String {
    let p1a = abbrev_hex(stage2_oid, abbrev);
    let p2a = abbrev_hex(stage3_oid, abbrev);
    let z = crate::diff::zero_oid();
    let za = abbrev_hex(&z, abbrev);

    let t_ours = format!("Subproject commit {}", stage2_oid.to_hex());
    let t_theirs = format!("Subproject commit {}", stage3_oid.to_hex());
    let tr = result_subproject_line.trim_end_matches('\n').to_owned();

    let mut out = String::new();
    out.push_str(&format!("diff --cc {path}\n"));
    out.push_str(&format!("index {p1a},{p2a}..{za}\n"));
    out.push_str(&format!("--- a/{path}\n"));
    out.push_str(&format!("+++ b/{path}\n"));
    out.push_str(&combined_hunk_two_parents(&t_ours, &t_theirs, &tr));
    out
}

/// `git diff` / `git diff --cc` during a conflict: worktree file with markers.
#[allow(clippy::too_many_arguments)]
pub fn format_worktree_conflict_combined(
    git_dir: &Path,
    config: &ConfigSet,
    odb: &Odb,
    path: &str,
    stage1_oid: &ObjectId,
    stage2_oid: &ObjectId,
    stage3_oid: &ObjectId,
    worktree_bytes: &[u8],
    abbrev: usize,
) -> String {
    let ours_blob = read_blob(odb, stage2_oid);
    let theirs_blob = read_blob(odb, stage3_oid);
    let _base_blob = read_blob(odb, stage1_oid);

    let use_conv = !worktree_bytes.contains(&0);
    let textconv_cache_path = diff_textconv_active(git_dir, config, path);
    let t_ours = if textconv_cache_path {
        blob_text_for_diff_with_oid(odb, git_dir, config, path, &ours_blob, stage2_oid, true)
    } else {
        blob_text_for_diff(git_dir, config, path, &ours_blob, use_conv)
    };
    let t_theirs = if textconv_cache_path {
        blob_text_for_diff_with_oid(odb, git_dir, config, path, &theirs_blob, stage3_oid, true)
    } else {
        blob_text_for_diff(git_dir, config, path, &theirs_blob, use_conv)
    };
    let wt_text = if textconv_cache_path || use_conv {
        blob_text_for_diff(git_dir, config, path, worktree_bytes, true)
    } else {
        String::from_utf8_lossy(worktree_bytes).into_owned()
    };
    let wt_for_conflict = wt_text.clone();

    let p1a = abbrev_hex(stage2_oid, abbrev);
    let p2a = abbrev_hex(stage3_oid, abbrev);
    let z = crate::diff::zero_oid();
    let za = abbrev_hex(&z, abbrev);

    let mut out = String::new();
    out.push_str(&format!("diff --cc {path}\n"));
    out.push_str(&format!("index {p1a},{p2a}..{za}\n"));
    out.push_str(&format!("--- a/{path}\n"));
    out.push_str(&format!("+++ b/{path}\n"));

    if wt_text.contains("<<<<<<<") && wt_text.contains(">>>>>>>") {
        out.push_str(&conflict_combined_body(&wt_for_conflict));
    } else {
        out.push_str(&format_combined_diff_body(
            &[t_ours, t_theirs],
            &wt_text,
            3,
            true,
            CombinedDiffWsOptions::default(),
        ));
    }
    out
}

/// Format the combined hunk for a worktree file that still contains conflict markers.
fn conflict_combined_body(wt: &str) -> String {
    let lines: Vec<&str> = wt.lines().collect();
    let mut body = String::new();
    let mut i = 0usize;
    while i < lines.len() {
        let line = lines[i];
        if line.starts_with("<<<<<<< ") {
            let mut hunk_new = 0u32;
            let mut ours_count = 0u32;
            let mut theirs_count = 0u32;
            body.push_str(&format!("++{line}\n"));
            hunk_new += 1;
            i += 1;
            while i < lines.len() && !lines[i].starts_with("=======") {
                body.push_str(&format!(" +{}\n", lines[i]));
                ours_count += 1;
                hunk_new += 1;
                i += 1;
            }
            if i < lines.len() && lines[i].starts_with("=======") {
                body.push_str("++=======\n");
                hunk_new += 1;
                i += 1;
            }
            while i < lines.len() && !lines[i].starts_with(">>>>>>>") {
                body.push_str(&format!("+ {}\n", lines[i]));
                theirs_count += 1;
                hunk_new += 1;
                i += 1;
            }
            if i < lines.len() {
                let closing = lines[i];
                body.push_str(&format!("++{closing}\n"));
                hunk_new += 1;
            }
            let header = format!(
                "@@@ -1,{} -1,{} +1,{} @@@\n",
                ours_count.max(1),
                theirs_count.max(1),
                hunk_new
            );
            return header + &body;
        }
        i += 1;
    }
    body
}

/// For each line of `result`, whether that line is absent from `parent` per a line-oriented diff.
#[allow(dead_code)] // Reserved for tighter `--cc` hunk alignment with Git's `dump_sline`.
fn result_line_differs_from_parent(parent: &str, result: &str) -> Vec<bool> {
    let lr: Vec<&str> = result.lines().collect();
    let mut out = vec![false; lr.len()];
    let diff = TextDiff::configure().diff_lines(parent, result);
    for change in diff.iter_all_changes() {
        match change.tag() {
            ChangeTag::Equal => {}
            ChangeTag::Delete => {}
            ChangeTag::Insert => {
                let range = change.value().lines().count();
                let Some(start) = change.new_index() else {
                    continue;
                };
                for i in 0..range {
                    if let Some(slot) = out.get_mut(start + i) {
                        *slot = true;
                    }
                }
            }
        }
    }
    out
}

/// Combined hunk body for two parents (Git `dump_sline` / `diff --cc` line prefixes).
///
/// Emits, like combine-diff.c `dump_sline`: first the per-parent deletion rows for any
/// parent line absent from the result (`-` in that parent's column, space in the other),
/// then the result rows with `+` in each column where the line differs from that parent.
fn combined_hunk_two_parents(a: &str, b: &str, result: &str) -> String {
    let la: Vec<&str> = a.lines().collect();
    let lb: Vec<&str> = b.lines().collect();
    let lr: Vec<&str> = result.lines().collect();

    let old_a = la.len().max(1) as u32;
    let old_b = lb.len().max(1) as u32;
    let new_c = lr.len().max(1) as u32;

    // Parent lines that do not survive into the result.
    let result_set: std::collections::HashSet<&&str> = lr.iter().collect();

    let mut body = String::new();
    // Per-parent deletion rows: parent0 (column 0), then parent1 (column 1).
    for line in &la {
        if !result_set.contains(line) {
            body.push_str(&format!("- {line}\n"));
        }
    }
    for line in &lb {
        if !result_set.contains(line) {
            body.push_str(&format!(" -{line}\n"));
        }
    }

    let d0 = result_line_differs_from_parent(a, result);
    let d1 = result_line_differs_from_parent(b, result);
    for (i, line) in lr.iter().enumerate() {
        let c0 = if d0.get(i).copied().unwrap_or(true) {
            '+'
        } else {
            ' '
        };
        let c1 = if d1.get(i).copied().unwrap_or(true) {
            '+'
        } else {
            ' '
        };
        body.push_str(&format!("{c0}{c1}{line}\n"));
    }

    format!("@@@ -1,{old_a} -1,{old_b} +1,{new_c} @@@\n{body}")
}

fn read_blob(odb: &Odb, oid: &ObjectId) -> Vec<u8> {
    if *oid == crate::diff::zero_oid() {
        return Vec::new();
    }
    odb.read(oid).map(|o| o.data).unwrap_or_default()
}

/// Read the blob at `path` in `tree`, or `None` if missing.
#[must_use]
pub fn read_blob_at_path(odb: &Odb, tree: &ObjectId, path: &str) -> Option<Vec<u8>> {
    let oid = blob_oid_at_path(odb, tree, path)?;
    Some(read_blob(odb, &oid))
}

/// OID of the blob at `path` in `tree`.
#[must_use]
pub fn blob_oid_at_path(odb: &Odb, tree: &ObjectId, path: &str) -> Option<ObjectId> {
    let mut current = *tree;
    let parts: Vec<&str> = path.split('/').collect();
    for (pi, part) in parts.iter().enumerate() {
        let obj = odb.read(&current).ok()?;
        let entries = crate::objects::parse_tree(&obj.data).ok()?;
        let found = entries
            .iter()
            .find(|e| std::str::from_utf8(&e.name).ok() == Some(*part))?;
        if pi + 1 == parts.len() {
            return Some(found.oid);
        }
        if found.mode != 0o040000 {
            return None;
        }
        current = found.oid;
    }
    None
}

fn abbrev_hex(oid: &ObjectId, abbrev: usize) -> String {
    let hex = oid.to_hex();
    let len = abbrev.min(hex.len());
    hex[..len].to_owned()
}

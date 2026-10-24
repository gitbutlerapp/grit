//! Merge commit and combined (`--cc` / `-c`) diff helpers.
//!
//! These mirror the subset of Git's combine-diff output needed for porcelain
//! commands (`git show`, `git diff` during conflicts, `git diff-tree -c`).

use std::ffi::OsString;
use std::io::Write;
use std::path::Path;

use tempfile::NamedTempFile;

use crate::combined_diff_patch::{format_combined_diff_body, CombinedDiffWsOptions};
use crate::combined_tree_diff::CombinedParentSide;
use crate::command_runner::{
    run_program, run_sh_dash_c, system_command_runner, CommandEnvironment, CommandSpec,
    CommandStdin, CommandStdio, ShellInvocation,
};
use crate::config::{parse_bool, ConfigSet};
use crate::crlf::{DiffAttr, FileAttrs};
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
            entries.iter().map(|e| e.path().to_string()).collect();
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
            let new_p = e.new_path.as_deref().unwrap_or("");
            if new_p != merge_path {
                continue;
            }
            let old_p = e.old_path.clone()?;
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
    let parsed = crate::crlf::load_gitattributes_parsed(work_tree);
    let config = ConfigSet::load(
        &crate::environment::Environment::empty(),
        Some(git_dir),
        true,
    )
    .unwrap_or_default();
    crate::crlf::file_attrs_from_gitattributes(&parsed.rules, &parsed.macros, path, false, &config)
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
    let runner = system_command_runner();
    if stdin_mode {
        let out = run_sh_dash_c(
            runner.as_ref(),
            &cmd_line,
            Some(command_cwd),
            CommandStdin::Pipe(input.to_vec()),
            CommandStdio::Null,
        )
        .ok()?;
        return if out.status.success {
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
        let spec = CommandSpec {
            program: "sh".into(),
            args: Vec::new(),
            shell: Some(ShellInvocation::DashC {
                script: format!("{} \"$@\"", cmd_line),
                argv0: Some(cmd_line.clone()),
                args: vec![OsString::from(path.to_string_lossy().as_ref())],
            }),
            cwd: Some(command_cwd.to_path_buf()),
            env: CommandEnvironment::inherit_process_only(),
            stdin: CommandStdin::Null,
            stdout: CommandStdio::Pipe,
            stderr: CommandStdio::Null,
        };
        runner.spawn(&spec).ok()?.wait_with_output().ok()?
    } else {
        run_program(
            runner.as_ref(),
            Path::new("sh"),
            &[
                OsString::from(&cmd_line),
                OsString::from(path.to_string_lossy().as_ref()),
            ],
            Some(command_cwd),
            CommandStdin::Null,
            CommandStdio::Null,
        )
        .ok()?
    };

    if !out.status.success {
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

/// `diff_textconv_cmd_line` function.
pub fn diff_textconv_cmd_line(config: &ConfigSet, driver: &str) -> Option<String> {
    let mut cmd_line = config.get(&format!("diff.{driver}.textconv"))?;
    cmd_line = cmd_line.trim_end().to_string();
    if cmd_line.ends_with('<') {
        let t = cmd_line.trim_end_matches('<').trim_end();
        cmd_line = t.to_string();
    }
    Some(cmd_line)
}

/// `diff_cachetextconv_enabled` function.
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
///
/// When a [`crate::repo::Repository`] handle is available, checkout uses the same smudge path
/// with destination-index attribute loading (see `WorktreeRules::from_checkout_prospective_index`).
#[allow(clippy::too_many_arguments)]
pub fn convert_blob_to_worktree_for_path(
    git_dir: &Path,
    work_tree: &Path,
    index: Option<&crate::index::Index>,
    odb: &Odb,
    path: &str,
    blob: &[u8],
    oid_hex: Option<&str>,
    filter_process: Option<&crate::filter_process::FilterProcessState>,
) -> std::io::Result<Vec<u8>> {
    let config = ConfigSet::load(
        &crate::environment::Environment::empty(),
        Some(git_dir),
        true,
    )
    .unwrap_or_default();
    let conv = crate::crlf::ConversionConfig::from_config(&config);
    let parsed = match index {
        Some(idx) => crate::crlf::load_gitattributes_for_checkout_parsed(work_tree, path, idx, odb),
        None => crate::crlf::load_gitattributes_parsed(work_tree),
    };
    let file_attrs = crate::crlf::file_attrs_from_gitattributes(
        &parsed.rules,
        &parsed.macros,
        path,
        false,
        &config,
    );
    crate::crlf::convert_to_worktree_eager(
        blob,
        path,
        &conv,
        &file_attrs,
        oid_hex,
        None,
        filter_process,
    )
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

#[cfg(test)]
mod line_coverage_tests {
    use super::*;
    use crate::combined_diff_patch::CombinedDiffWsOptions;
    use crate::combined_tree_diff::{CombinedDiffPath, CombinedParentSide, CombinedParentStatus};
    use crate::config::ConfigSet;
    use crate::objects::ObjectKind;
    use crate::odb::Odb;
    use tempfile::tempdir;

    fn sample_merge(odb: &Odb) -> (ObjectId, Vec<ObjectId>, ObjectId) {
        let b0 = odb.write(ObjectKind::Blob, b"base\n").expect("blob");
        let b1 = odb.write(ObjectKind::Blob, b"one\n").expect("blob");
        let b2 = odb.write(ObjectKind::Blob, b"two\n").expect("blob");
        let bm = odb.write(ObjectKind::Blob, b"merged\n").expect("blob");
        let tree = |entries: &[(&str, ObjectId)]| {
            let mut body = Vec::new();
            for (n, oid) in entries {
                body.extend_from_slice(format!("100644 {n}\0").as_bytes());
                body.extend_from_slice(oid.as_bytes());
            }
            odb.write(ObjectKind::Tree, &body).expect("tree")
        };
        let base_tree = tree(&[("f.txt", b0)]);
        let p1_tree = tree(&[("f.txt", b1), ("only-p1.txt", b0)]);
        let p2_tree = tree(&[("f.txt", b2), ("only-p2.txt", b0)]);
        let merge_tree = tree(&[("f.txt", bm), ("only-p1.txt", b1), ("only-p2.txt", b2)]);
        let commit = |tree: ObjectId, parents: &[ObjectId], msg: &str| {
            let mut body = format!("tree {tree}\n");
            for p in parents {
                body.push_str(&format!("parent {p}\n"));
            }
            body.push_str("author T <t@e.com> 1 +0000\ncommitter T <t@e.com> 1 +0000\n\n");
            body.push_str(msg);
            body.push('\n');
            odb.write(ObjectKind::Commit, body.as_bytes())
                .expect("commit")
        };
        let base = commit(base_tree, &[], "base");
        let p1 = commit(p1_tree, &[base], "p1");
        let p2 = commit(p2_tree, &[base], "p2");
        let merge = commit(merge_tree, &[p1, p2], "merge");
        (merge_tree, vec![p1, p2], merge)
    }

    #[test]
    fn combined_formatters_and_nested_paths() {
        let dir = tempdir().expect("tempdir");
        let odb = Odb::new(dir.path());
        let git_dir = dir.path().join(".git");
        std::fs::create_dir_all(&git_dir).expect("git");
        let (merge_tree, parents, _merge) = sample_merge(&odb);
        let cfg = ConfigSet::new();
        let paths = combined_diff_paths(&odb, &merge_tree, &parents);
        assert!(!paths.is_empty());
        let nested = all_blob_paths_in_tree_order(&odb, &merge_tree);
        assert!(nested.iter().any(|p| p.contains("only-p1")));
        let p1_tree = {
            let c = odb.read(&parents[0]).expect("p1");
            crate::objects::parse_commit(&c.data).expect("parse").tree
        };
        let p1_oid = blob_oid_at_path(&odb, &p1_tree, "f.txt").expect("p1 blob");
        let merge_oid = blob_oid_at_path(&odb, &merge_tree, "f.txt").expect("merge blob");
        let mut path = CombinedDiffPath {
            path: "f.txt".into(),
            merge_mode: 0o100644,
            merge_oid,
            parents: vec![
                CombinedParentSide {
                    status: CombinedParentStatus::Modified,
                    mode: 0o100644,
                    oid: p1_oid,
                    rename_from: None,
                },
                CombinedParentSide {
                    status: CombinedParentStatus::Modified,
                    mode: 0o100644,
                    oid: blob_oid_at_path(&odb, &merge_tree, "f.txt").expect("oid"),
                    rename_from: Some("old-f.txt".into()),
                },
            ],
        };
        enrich_combined_path_renames(&odb, &mut path, &parents, &merge_tree, 50);
        let _ = format_combined_binary_header_n("f.txt", &[p1_oid], &merge_oid, 7, false);
        let _ = format_combined_textconv_patch(
            &git_dir,
            &cfg,
            &odb,
            "f.txt",
            &parents,
            &merge_tree,
            7,
            3,
            false,
            false,
            CombinedDiffWsOptions::default(),
            false,
            Some(&["f.txt".to_owned()]),
            &path.parents,
            true,
        );
        let blob = read_blob_at_path(&odb, &merge_tree, "only-p2.txt").expect("blob");
        let _ = convert_blob_to_worktree_for_path(
            &git_dir,
            dir.path(),
            None,
            &odb,
            "only-p2.txt",
            &blob,
            None,
            None,
        );
        let z = crate::diff::zero_oid();
        let _ = format_worktree_conflict_combined(
            &git_dir, &cfg, &odb, "f.txt", &z, &p1_oid, &merge_oid, b"plain\n", 12,
        );
    }
}

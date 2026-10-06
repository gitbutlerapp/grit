//! `grit apply` — apply a unified diff/patch to the working tree or index.
//!
//! Modes:
//! - `grit apply <patch>` — apply patch to the working tree
//! - `grit apply --cached <patch>` — apply patch to the index only
//! - `grit apply --stat <patch>` — show diffstat without applying
//! - `grit apply --numstat <patch>` — show numstat without applying
//! - `grit apply --summary <patch>` — show summary without applying
//! - `grit apply --check <patch>` — check if patch applies cleanly
//! - `grit apply -R / --reverse` — reverse the patch
//! - `grit apply -p<n>` — strip leading path components (default 1)
//! - `grit apply --directory=<dir>` — prepend directory to paths
//! - Reads from stdin if no file argument given

use crate::explicit_exit::ExplicitExit;
use anyhow::{anyhow, bail, Context, Result};
use clap::Args as ClapArgs;
use grit_lib::apply::{inflate_binary_payload, parse_patch, FilePatch, Hunk, HunkLine};
use grit_lib::config::ConfigSet;
use grit_lib::crlf::{self, FileAttrs, MergeAttr};
use grit_lib::index::{
    Index, IndexEntry, MODE_EXECUTABLE, MODE_GITLINK, MODE_REGULAR, MODE_SYMLINK, MODE_TREE,
};
use grit_lib::merge_file::{merge, ConflictStyle, MergeFavor, MergeInput};
use grit_lib::objects::ObjectId;
use grit_lib::objects::ObjectKind;
use grit_lib::quote_path::quote_c_style;
use grit_lib::repo::Repository;
use grit_lib::rerere::{repo_rerere, rerere_enabled, RerereAutoupdate};
use grit_lib::rev_parse::resolve_revision_for_patch_old_blob;
use grit_lib::sparse_checkout::{
    effective_cone_mode_for_sparse_file, parse_sparse_checkout_file,
    path_in_sparse_checkout_patterns,
};
use grit_lib::ws::{self, WhitespaceGitAttr, WS_BLANK_AT_EOF, WS_DEFAULT_RULE, WS_INCOMPLETE_LINE};
use std::borrow::Cow;
use std::collections::HashMap;
use std::fs;
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

/// Arguments for `grit apply`.
#[derive(Debug, Default, ClapArgs)]
#[command(about = "Apply a patch to files and/or to the index")]
pub struct Args {
    /// Apply the patch to the index instead of the working tree.
    #[arg(long)]
    pub cached: bool,

    /// Show a diffstat of the patch without applying.
    #[arg(long)]
    pub stat: bool,

    /// Show machine-readable stat (additions/deletions per file).
    #[arg(long)]
    pub numstat: bool,

    /// Show a condensed summary of extended header information.
    #[arg(long)]
    pub summary: bool,

    /// Apply the patch even when combined with --stat/--summary output modes.
    #[arg(long = "apply")]
    pub apply: bool,

    /// Check if the patch applies cleanly without modifying anything.
    #[arg(long)]
    pub check: bool,

    /// Apply to both the working tree and the index.
    #[arg(long)]
    pub index: bool,

    /// Mark new files as intent-to-add when applying to the working tree.
    #[arg(short = 'N', long = "intent-to-add")]
    pub intent_to_add: bool,

    /// Apply the patch in reverse.
    #[arg(short = 'R', long = "reverse")]
    pub reverse: bool,

    /// Allow empty patches (or patch input with no diff hunks).
    #[arg(long = "allow-empty")]
    pub allow_empty: bool,

    /// Strip N leading path components from diff paths (default: 1).
    #[arg(short = 'p', default_value = "1")]
    pub strip: usize,

    /// Prepend directory to all file paths in the patch.
    #[arg(long = "directory", value_name = "DIR")]
    pub directory: Option<String>,

    /// Build a temporary index with preimage blobs from patch index lines.
    #[arg(long = "build-fake-ancestor", value_name = "FILE")]
    pub build_fake_ancestor: Option<PathBuf>,

    /// Ensure at least `<n>` lines of surrounding context match (git apply `-C<n>`).
    #[arg(short = 'C', value_name = "N")]
    pub context: Option<usize>,

    /// Recount hunk line counts (for corrupted patches).
    #[arg(long = "recount")]
    pub recount: bool,

    /// Apply with unidiff-zero context.
    #[arg(long = "unidiff-zero")]
    pub unidiff_zero: bool,

    /// Allow binary patches.
    #[arg(long = "allow-binary-replacement", alias = "binary")]
    pub allow_binary_replacement: bool,

    /// Verbose output.
    #[arg(short = 'v', long = "verbose")]
    pub verbose: bool,

    /// Leave rejected hunks in corresponding *.rej files.
    #[arg(long = "reject")]
    pub reject: bool,

    /// How to handle whitespace errors.
    #[arg(long = "whitespace", value_name = "ACTION", default_value = "warn")]
    pub whitespace: String,

    /// Ignore changes in whitespace when matching context lines.
    #[arg(long = "ignore-whitespace")]
    pub ignore_whitespace: bool,

    /// Ignore changes in amount of whitespace when matching context lines.
    #[arg(long = "ignore-space-change")]
    pub ignore_space_change: bool,

    /// Disable whitespace-ignoring context matching.
    #[arg(long = "no-ignore-whitespace")]
    pub no_ignore_whitespace: bool,

    /// Attempt a three-way merge if the patch does not apply cleanly.
    #[arg(long = "3way")]
    pub three_way: bool,

    /// With `--3way`, resolve conflicts using our version (`git apply --ours`).
    #[arg(long = "ours", requires = "three_way", conflicts_with_all = ["theirs", "union"])]
    pub ours: bool,

    /// With `--3way`, resolve conflicts using their version (`git apply --theirs`).
    #[arg(long = "theirs", requires = "three_way", conflicts_with_all = ["ours", "union"])]
    pub theirs: bool,

    /// With `--3way`, resolve conflicts using a union merge (`git apply --union`).
    #[arg(long = "union", requires = "three_way", conflicts_with_all = ["ours", "theirs"])]
    pub union: bool,

    /// Include context and removed lines in the output.
    #[arg(long = "include")]
    pub include: Option<String>,

    /// Exclude paths from the patch.
    #[arg(long = "exclude")]
    pub exclude: Option<String>,

    /// Do not trust the line counts in the hunk headers.
    #[arg(long = "inaccurate-eof")]
    pub inaccurate_eof: bool,

    /// Patch file(s). Reads from stdin if none given.
    #[arg(value_name = "PATCH")]
    pub patches: Vec<PathBuf>,
}

// ---------------------------------------------------------------------------
// Parsed patch types
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum WsCliMode {
    Warn,
    NoWarn,
    Error,
    Fix,
}

#[derive(Debug, Clone, Copy)]
struct ApplyWhitespaceMode {
    whitespace_fix: bool,
    ignore_space_change: bool,
    inaccurate_eof: bool,
    tab_width: usize,
    context: Option<usize>,
    ws_cli: WsCliMode,
    /// Git suppresses excess whitespace diagnostics after this many (0 = never squelch).
    ws_squelch: u32,
    /// `git apply --unidiff-zero`: do not require trailing context to pin the hunk end.
    unidiff_zero: bool,
}

fn config_ignore_space_change() -> bool {
    let Ok(repo) = Repository::discover(None) else {
        return false;
    };
    let config = grit_lib::config::ConfigSet::load(Some(&repo.git_dir), true)
        .unwrap_or_else(|_| grit_lib::config::ConfigSet::new());
    let Some(value) = config.get("apply.ignorewhitespace") else {
        return false;
    };
    matches!(
        value.to_ascii_lowercase().as_str(),
        "change" | "true" | "yes" | "on" | "1"
    )
}

fn config_whitespace_rule_bits() -> u32 {
    let Ok(repo) = Repository::discover(None) else {
        return WS_DEFAULT_RULE;
    };
    let config = grit_lib::config::ConfigSet::load(Some(&repo.git_dir), true)
        .unwrap_or_else(|_| grit_lib::config::ConfigSet::new());
    let Some(value) = config.get("core.whitespace") else {
        return WS_DEFAULT_RULE;
    };
    ws::parse_whitespace_rule(&value).unwrap_or(WS_DEFAULT_RULE)
}

fn config_tab_width() -> usize {
    ws::ws_tab_width(config_whitespace_rule_bits()).max(1)
}

fn config_whitespace_fix() -> bool {
    let Ok(repo) = Repository::discover(None) else {
        return false;
    };
    let config = grit_lib::config::ConfigSet::load(Some(&repo.git_dir), true)
        .unwrap_or_else(|_| grit_lib::config::ConfigSet::new());
    let Some(value) = config.get("apply.whitespace") else {
        return false;
    };
    // `apply.whitespace` mirrors `--whitespace`; `strip` is an alias for `fix` (t4119).
    value.eq_ignore_ascii_case("fix") || value.eq_ignore_ascii_case("strip")
}

fn whitespace_option_was_explicitly_set() -> bool {
    std::env::args().any(|arg| arg == "--whitespace" || arg.starts_with("--whitespace="))
}

fn resolve_apply_whitespace_mode(args: &Args) -> ApplyWhitespaceMode {
    let ws_cli = if args.whitespace.eq_ignore_ascii_case("nowarn") {
        WsCliMode::NoWarn
    } else if args.whitespace.eq_ignore_ascii_case("warn") {
        WsCliMode::Warn
    } else if args.whitespace.eq_ignore_ascii_case("error")
        || args.whitespace.eq_ignore_ascii_case("error-all")
    {
        WsCliMode::Error
    } else if args.whitespace.eq_ignore_ascii_case("fix")
        || args.whitespace.eq_ignore_ascii_case("strip")
    {
        WsCliMode::Fix
    } else {
        WsCliMode::Warn
    };

    // When `--whitespace` is omitted, Git defaults to `warn` but may still enable fix via
    // `apply.whitespace=fix`. When the user passes `--whitespace=...`, that choice wins
    // (t4125: `--whitespace=nowarn` must not strip added lines because config says fix).
    let whitespace_fix = if whitespace_option_was_explicitly_set() {
        matches!(ws_cli, WsCliMode::Fix)
    } else if matches!(ws_cli, WsCliMode::Warn) && config_whitespace_fix() {
        true
    } else {
        matches!(ws_cli, WsCliMode::Fix)
    };
    let ignore_space_change = if args.no_ignore_whitespace {
        false
    } else if args.ignore_whitespace || args.ignore_space_change {
        true
    } else {
        config_ignore_space_change()
    };
    let ws_squelch = if args.whitespace.eq_ignore_ascii_case("error-all") {
        0u32
    } else {
        5u32
    };

    ApplyWhitespaceMode {
        whitespace_fix,
        ignore_space_change,
        inaccurate_eof: args.inaccurate_eof,
        tab_width: config_tab_width(),
        context: args.context,
        ws_cli,
        ws_squelch,
        unidiff_zero: args.unidiff_zero,
    }
}

/// Outcome of a `--3way` attempt for one file (blob-level).
#[derive(Debug)]
struct ThreeWayBlobResult {
    merged_bytes: Vec<u8>,
    conflicted: bool,
    /// Stage blobs when `conflicted` (base, ours, theirs); unused entries are zero OID for add/add base.
    stages: [ObjectId; 3],
}

fn resolve_apply_conflict_style(repo: &Repository) -> ConflictStyle {
    let Ok(config) = ConfigSet::load(Some(&repo.git_dir), true) else {
        return ConflictStyle::Merge;
    };
    match config
        .get("merge.conflictstyle")
        .unwrap_or_default()
        .to_lowercase()
        .as_str()
    {
        "diff3" => ConflictStyle::Diff3,
        "zdiff3" => ConflictStyle::ZealousDiff3,
        _ => ConflictStyle::Merge,
    }
}

fn merge_favor_from_apply_args(args: &Args) -> MergeFavor {
    if args.union {
        MergeFavor::Union
    } else if args.theirs {
        MergeFavor::Theirs
    } else if args.ours {
        MergeFavor::Ours
    } else {
        MergeFavor::None
    }
}

fn effective_merge_favor_for_path(file_attrs: &FileAttrs, cli: MergeFavor) -> MergeFavor {
    if matches!(file_attrs.merge, MergeAttr::Driver(ref s) if s == "union") {
        MergeFavor::Union
    } else {
        cli
    }
}

fn conflict_marker_size_for_path(file_attrs: &FileAttrs) -> usize {
    file_attrs
        .conflict_marker_size
        .as_deref()
        .and_then(|s| s.parse::<usize>().ok())
        .filter(|&n| n > 0)
        .unwrap_or(7)
}

fn count_hunk_line_changes(fp: &FilePatch) -> (usize, usize) {
    let mut add = 0usize;
    let mut del = 0usize;
    for h in &fp.hunks {
        for line in &h.lines {
            match line {
                HunkLine::Add(_) => add += 1,
                HunkLine::Remove(_) => del += 1,
                _ => {}
            }
        }
    }
    (add, del)
}

/// Try Git's `apply --3way` in-core merge: base = patch preimage, theirs = patch postimage, ours = `our_bytes`.
#[expect(clippy::too_many_arguments)]
fn try_three_way_merge_blob(
    repo: &Repository,
    fp: &FilePatch,
    our_bytes: &[u8],
    ws_mode: ApplyWhitespaceMode,
    forward_apply: bool,
    patch_input_display: &str,
    favor: MergeFavor,
    style: ConflictStyle,
    file_attrs: &FileAttrs,
    direct_to_threeway: bool,
) -> Result<Option<ThreeWayBlobResult>> {
    if !forward_apply {
        return Ok(None);
    }
    if fp.is_deleted {
        return Ok(None);
    }
    if fp.involves_gitlink() {
        return Ok(None);
    }
    if fp.is_rename {
        let (a, d) = count_hunk_line_changes(fp);
        if a == 0 && d == 0 {
            return Ok(None);
        }
    }
    if fp.is_new && !direct_to_threeway {
        return Ok(None);
    }
    let (pre_bytes, preimage_oid): (Vec<u8>, Option<ObjectId>) = if fp.is_new {
        (Vec::new(), None)
    } else {
        let Some(prefix) = fp.old_oid.as_deref() else {
            return Ok(None);
        };
        let oid = resolve_revision_for_patch_old_blob(repo, prefix)?;
        let obj = repo.odb.read(&oid)?;
        (obj.data, Some(oid))
    };

    let post_bytes: Vec<u8> = if let Some(bin) = fp.binary_patch.as_ref() {
        inflate_binary_payload(&bin.forward_compressed)?
    } else if fp.hunks.is_empty() {
        pre_bytes.clone()
    } else {
        let pre_text = String::from_utf8_lossy(&pre_bytes);
        let post_text = apply_hunks(
            pre_text.as_ref(),
            fp,
            ws_mode,
            forward_apply,
            patch_input_display,
        )?;
        post_text.into_bytes()
    };

    if pre_bytes == our_bytes {
        let oid_base = preimage_oid.unwrap_or(ObjectId::zero());
        let oid_ours = repo.odb.write(ObjectKind::Blob, our_bytes)?;
        let oid_theirs = repo.odb.write(ObjectKind::Blob, &post_bytes)?;
        return Ok(Some(ThreeWayBlobResult {
            merged_bytes: post_bytes,
            conflicted: false,
            stages: [oid_base, oid_ours, oid_theirs],
        }));
    }
    if pre_bytes == post_bytes || our_bytes == post_bytes {
        let oid_base = preimage_oid.unwrap_or(ObjectId::zero());
        let oid_ours = repo.odb.write(ObjectKind::Blob, our_bytes)?;
        let oid_theirs = repo.odb.write(ObjectKind::Blob, &post_bytes)?;
        return Ok(Some(ThreeWayBlobResult {
            merged_bytes: our_bytes.to_vec(),
            conflicted: false,
            stages: [oid_base, oid_ours, oid_theirs],
        }));
    }

    let base_hex = preimage_oid.map(|o| o.to_hex()).unwrap_or_default();
    let abbrev_len = 7usize;
    let base_abbrev = if base_hex.is_empty() {
        String::new()
    } else if base_hex.len() > abbrev_len {
        base_hex[..abbrev_len].to_string()
    } else {
        base_hex.clone()
    };
    let label_base = match style {
        ConflictStyle::ZealousDiff3 if base_abbrev.chars().all(|c| c.is_ascii_hexdigit()) => {
            base_abbrev.clone()
        }
        ConflictStyle::Diff3 | ConflictStyle::ZealousDiff3 if !base_abbrev.is_empty() => {
            format!("{base_abbrev}:content")
        }
        _ => "base".to_string(),
    };

    let merged = merge(&MergeInput {
        base: &pre_bytes,
        ours: our_bytes,
        theirs: &post_bytes,
        label_ours: "ours",
        label_base: &label_base,
        label_theirs: "theirs",
        favor,
        style,
        marker_size: conflict_marker_size_for_path(file_attrs),
        diff_algorithm: None,
        ignore_all_space: false,
        ignore_space_change: false,
        ignore_space_at_eol: false,
        ignore_cr_at_eol: false,
    })
    .map_err(|e| anyhow::anyhow!("{e}"))?;

    let oid_base = preimage_oid.unwrap_or(ObjectId::zero());
    let oid_ours = repo.odb.write(ObjectKind::Blob, our_bytes)?;
    let oid_theirs = repo.odb.write(ObjectKind::Blob, &post_bytes)?;

    Ok(Some(ThreeWayBlobResult {
        merged_bytes: merged.content,
        conflicted: merged.conflicts > 0,
        stages: [oid_base, oid_ours, oid_theirs],
    }))
}

fn add_conflicted_index_stages(
    index: &mut Index,
    path_bytes: &[u8],
    mode: u32,
    stages: &[ObjectId; 3],
) {
    index.entries.retain(|e| e.path != path_bytes);
    for (stage_idx, oid) in stages.iter().enumerate() {
        if oid.is_zero() {
            continue;
        }
        let stage = (stage_idx + 1) as u8;
        let entry = IndexEntry {
            ctime_sec: 0,
            ctime_nsec: 0,
            mtime_sec: 0,
            mtime_nsec: 0,
            dev: 0,
            ino: 0,
            mode,
            uid: 0,
            gid: 0,
            size: 0,
            oid: *oid,
            flags: ((path_bytes.len().min(0xFFF)) as u16 & 0x0FFF) | ((stage as u16) << 12),
            flags_extended: None,
            path: path_bytes.to_vec(),
            base_index_pos: 0,
        };
        index.add_or_replace(entry);
    }
}

// ---------------------------------------------------------------------------
// Parsing
// ---------------------------------------------------------------------------

/// Infer `is_new` / `is_deleted` / `is_rename` for submodule diffs that only use mode lines and
/// `---`/`+++` paths (no `new file mode` / `deleted file mode` headers).
fn postprocess_gitlink_file_patches(patches: &mut [FilePatch]) {
    for fp in patches.iter_mut() {
        if !fp.involves_gitlink() {
            continue;
        }
        if fp.is_rename || fp.is_copy || fp.is_new || fp.is_deleted {
            continue;
        }
        let old_p = fp.old_path.as_deref();
        let new_p = fp.new_path.as_deref();
        let old_ok = old_p.is_some_and(|p| p != "/dev/null");
        let new_ok = new_p.is_some_and(|p| p != "/dev/null");
        match (old_ok, new_ok) {
            (true, false) => fp.is_deleted = true,
            (false, true) => fp.is_new = true,
            (true, true) => {
                if old_p != new_p {
                    fp.is_rename = true;
                }
            }
            (false, false) => {}
        }
    }
}

/// `git diff` represents replacing a tracked file with a submodule as two hunks: delete the blob,
/// then add the gitlink. Applying them separately removes the file then fails to create the empty
/// submodule directory (`t4137-apply-submodule`). Merge into one logical patch.
fn merge_adjacent_blob_to_gitlink_patches(patches: &mut Vec<FilePatch>) {
    let mut i = 0usize;
    while i + 1 < patches.len() {
        let del = &patches[i];
        let add = &patches[i + 1];
        let del_path = del
            .old_path
            .as_deref()
            .filter(|p| *p != "/dev/null")
            .or(del.effective_path());
        let add_path = add
            .new_path
            .as_deref()
            .filter(|p| *p != "/dev/null")
            .or(add.effective_path());
        let same_path = del_path.zip(add_path).is_some_and(|(a, b)| a == b);
        let del_blob = del.is_deleted && del.old_mode.as_deref() != Some("160000");
        let add_gitlink = add.is_new && add.new_mode.as_deref() == Some("160000");
        if same_path && del_blob && add_gitlink {
            let mut merged = del.clone();
            merged.new_path = add.new_path.clone();
            merged.saw_new_header = add.saw_new_header;
            merged.new_mode = Some("160000".to_string());
            merged.new_mode_line = add.new_mode_line;
            merged.is_deleted = false;
            merged.is_new = false;
            merged.new_oid = add.new_oid.clone();
            merged.is_toplevel_relative = del.is_toplevel_relative;
            merged.hunks.extend(add.hunks.clone());
            patches[i] = merged;
            patches.remove(i + 1);
            continue;
        }
        i += 1;
    }
}

fn subproject_commit_from_hunks(fp: &FilePatch) -> String {
    fp.hunks
        .iter()
        .flat_map(|h| h.lines.iter())
        .find_map(|l| {
            if let HunkLine::Add(s) = l {
                s.strip_prefix("Subproject commit ")
                    .map(|h| h.trim().to_string())
            } else {
                None
            }
        })
        .or_else(|| fp.new_oid.clone())
        .unwrap_or_else(|| "0000000000000000000000000000000000000000".to_string())
}

fn subproject_commit_oid_for_index(
    fp: &FilePatch,
    work_tree: Option<&Path>,
    target_adjusted: &str,
) -> Result<ObjectId> {
    let commit = subproject_commit_from_hunks(fp);
    if let Ok(oid) = ObjectId::from_hex(&commit) {
        return Ok(oid);
    }
    if let Some(wt) = work_tree {
        if let Some(head) = grit_lib::diff::read_submodule_head_oid(&wt.join(target_adjusted)) {
            if head.to_hex().starts_with(&commit) {
                return Ok(head);
            }
        }
    }
    ObjectId::from_hex(&commit).with_context(|| format!("invalid object id '{commit}'"))
}

// ---------------------------------------------------------------------------
// Strip / directory adjustment
// ---------------------------------------------------------------------------

/// Strip `n` leading path components.
fn strip_components(path: &str, n: usize) -> String {
    if n == 0 {
        return path.to_string();
    }
    let mut remaining = path;
    for _ in 0..n {
        if let Some(pos) = remaining.find('/') {
            remaining = &remaining[pos + 1..];
        } else {
            return remaining.to_string();
        }
    }
    remaining.to_string()
}

/// Strip `-p` prefix from a path (matches parsing-time strip, for validating headers).
fn path_after_strip(path: &str, strip: usize) -> String {
    if path == "/dev/null" {
        return path.to_string();
    }
    strip_components(path, strip)
}

/// Create compact rename path: "dir/{old => new}" or "old => new".
fn compact_rename_path(old: &str, new: &str) -> String {
    // Find common prefix
    let old_parts: Vec<&str> = old.split('/').collect();
    let new_parts: Vec<&str> = new.split('/').collect();
    let mut prefix_len = 0;
    for (a, b) in old_parts.iter().zip(new_parts.iter()) {
        if a == b {
            prefix_len += 1;
        } else {
            break;
        }
    }
    // Find common suffix
    let mut suffix_len = 0;
    let old_rev: Vec<&str> = old_parts.iter().rev().cloned().collect();
    let new_rev: Vec<&str> = new_parts.iter().rev().cloned().collect();
    for (a, b) in old_rev.iter().zip(new_rev.iter()) {
        if a == b && prefix_len + suffix_len < old_parts.len().min(new_parts.len()) {
            suffix_len += 1;
        } else {
            break;
        }
    }

    let prefix: String = old_parts[..prefix_len].join("/");
    let suffix: String = old_parts[old_parts.len() - suffix_len..].join("/");
    let old_mid: String = old_parts[prefix_len..old_parts.len() - suffix_len].join("/");
    let new_mid: String = new_parts[prefix_len..new_parts.len() - suffix_len].join("/");

    // If no common prefix or suffix, just use "old => new" without braces
    if prefix.is_empty() && suffix.is_empty() {
        return format!("{old_mid} => {new_mid}");
    }

    let mut result = String::new();
    if !prefix.is_empty() {
        result.push_str(&prefix);
        result.push('/');
    }
    result.push('{');
    result.push_str(&old_mid);
    result.push_str(" => ");
    result.push_str(&new_mid);
    result.push('}');
    if !suffix.is_empty() {
        result.push('/');
        result.push_str(&suffix);
    }
    result
}

/// Normalize `--directory` like Git (`strbuf_normalize_path` + trailing `/` when non-empty).
fn normalize_apply_directory(raw: &str) -> Result<String> {
    let normalized = grit_lib::git_path::normalize_path_copy(raw)
        .map_err(|_| anyhow::anyhow!("unable to normalize directory: '{raw}'"))?;
    if normalized.is_empty() {
        return Ok(String::new());
    }
    Ok(if normalized.ends_with('/') {
        normalized
    } else {
        format!("{normalized}/")
    })
}

/// Prepend normalized `--directory` to a path already adjusted for `-p` during parsing.
fn adjust_path(path: &str, directory: Option<&str>) -> String {
    if path == "/dev/null" {
        return path.to_string();
    }
    let Some(dir) = directory.filter(|d| !d.is_empty()) else {
        return path.to_string();
    };
    format!("{dir}{path}")
}

fn apply_work_tree_root() -> Option<PathBuf> {
    Repository::discover(None).ok().and_then(|r| r.work_tree)
}

/// Git `setup_git_directory` prefix: work-tree-relative path from CWD to repo root, with trailing `/`.
fn apply_setup_prefix() -> String {
    let Ok(repo) = Repository::discover(None) else {
        return String::new();
    };
    let Some(wt) = repo.work_tree else {
        return String::new();
    };
    let Ok(cwd) = std::env::current_dir() else {
        return String::new();
    };
    let Ok(rel) = cwd.strip_prefix(&wt) else {
        return String::new();
    };
    let s = rel.to_string_lossy();
    let trimmed = s.trim_end_matches(['/', '\\']);
    if trimmed.is_empty() {
        String::new()
    } else {
        format!("{trimmed}/")
    }
}

/// Absolute path for filesystem access. Git apply `chdir`s to the work tree root so patch paths
/// are always resolved from there, even when the user runs `git apply` from a subdirectory.
fn apply_fs_path(rel: &str, work_tree: Option<&Path>) -> PathBuf {
    let Some(wt) = work_tree else {
        return PathBuf::from(rel);
    };
    wt.join(rel)
}

fn symlink_prefix(
    path: &str,
    work_tree: Option<&Path>,
    symlink_overlay: &HashMap<String, bool>,
) -> Option<String> {
    let components: Vec<&str> = path.split('/').filter(|c| !c.is_empty()).collect();
    if components.len() <= 1 {
        return None;
    }

    let mut prefix = PathBuf::new();
    for component in &components[..components.len() - 1] {
        prefix.push(component);
        let prefix_str = prefix.to_string_lossy().into_owned();
        let full = apply_fs_path(&prefix_str, work_tree);
        let is_symlink = symlink_overlay
            .get(&prefix_str)
            .copied()
            .unwrap_or_else(|| {
                fs::symlink_metadata(&full)
                    .map(|meta| meta.file_type().is_symlink())
                    .unwrap_or(false)
            });
        if is_symlink {
            return Some(prefix_str);
        }
    }
    None
}

fn verify_patch_paths_not_beyond_symlink(patches: &[FilePatch], args: &Args) -> Result<()> {
    let work_tree = apply_work_tree_root();
    let work_tree_ref = work_tree.as_deref();
    let setup_prefix = apply_setup_prefix();
    let mut symlink_overlay: HashMap<String, bool> = HashMap::new();
    let mut replaced_directories_with_symlink: HashMap<String, bool> = HashMap::new();

    for fp in patches {
        if let Some(source) = fp.source_path() {
            let source_adjusted = adjust_path(source, args.directory.as_deref());
            let source_op = fp.worktree_rel_operational(&source_adjusted, &setup_prefix);
            if !source_op.is_empty() {
                if let Some(prefix) = symlink_prefix(&source_op, work_tree_ref, &symlink_overlay) {
                    let allow_delete_under_replaced_dir = fp.is_deleted
                        && source_op.starts_with(&format!("{prefix}/"))
                        && replaced_directories_with_symlink
                            .get(&prefix)
                            .copied()
                            .unwrap_or(false);
                    if !allow_delete_under_replaced_dir {
                        bail!("{source_op}: beyond a symbolic link");
                    }
                }
            }
        }
        if let Some(target) = fp.target_path() {
            let target_adjusted = adjust_path(target, args.directory.as_deref());
            let target_op = fp.worktree_rel_operational(&target_adjusted, &setup_prefix);
            if !target_op.is_empty() {
                if let Some(prefix) = symlink_prefix(&target_op, work_tree_ref, &symlink_overlay) {
                    let allow_delete_under_replaced_dir = fp.is_deleted
                        && target_op.starts_with(&format!("{prefix}/"))
                        && replaced_directories_with_symlink
                            .get(&prefix)
                            .copied()
                            .unwrap_or(false);
                    if !allow_delete_under_replaced_dir {
                        bail!("{target_op}: beyond a symbolic link");
                    }
                }
            }
        }

        let source_adjusted = fp
            .source_path()
            .map(|p| adjust_path(p, args.directory.as_deref()))
            .unwrap_or_default();
        let target_adjusted = fp
            .target_path()
            .map(|p| adjust_path(p, args.directory.as_deref()))
            .unwrap_or_default();
        let source_op = fp.worktree_rel_operational(&source_adjusted, &setup_prefix);
        let target_op = fp.worktree_rel_operational(&target_adjusted, &setup_prefix);
        let target_is_existing_dir =
            !target_op.is_empty() && apply_fs_path(&target_op, work_tree_ref).is_dir();

        if fp.is_deleted {
            if !source_op.is_empty() {
                symlink_overlay.insert(source_op, false);
            }
            continue;
        }

        if fp.is_rename && source_op != target_op && !source_op.is_empty() {
            symlink_overlay.insert(source_op, false);
        }

        if !target_op.is_empty() {
            if fp.new_mode.as_deref() == Some("120000") {
                if target_is_existing_dir {
                    replaced_directories_with_symlink.insert(target_op.clone(), true);
                }
                symlink_overlay.insert(target_op, true);
            } else if fp.new_mode.is_some() || fp.is_new {
                symlink_overlay.insert(target_op, false);
            }
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Reverse
// ---------------------------------------------------------------------------

/// Reverse a patch: swap old/new paths, swap +/- in hunks.
fn reverse_patches(patches: &mut [FilePatch]) {
    for fp in patches.iter_mut() {
        std::mem::swap(&mut fp.old_path, &mut fp.new_path);
        // Match Git `reverse_patches`: only swap mode headers when the forward patch had a new
        // mode or was a deletion (`t4129` reverse apply + mode-only diffs).
        if fp.new_mode.is_some() || fp.is_deleted {
            std::mem::swap(&mut fp.old_mode, &mut fp.new_mode);
            std::mem::swap(&mut fp.old_mode_line, &mut fp.new_mode_line);
        }
        std::mem::swap(&mut fp.old_oid, &mut fp.new_oid);
        std::mem::swap(&mut fp.is_new, &mut fp.is_deleted);
        if let Some(binary) = fp.binary_patch.as_mut() {
            std::mem::swap(
                &mut binary.forward_compressed,
                &mut binary.reverse_compressed,
            );
            std::mem::swap(
                &mut binary.forward_declared_size,
                &mut binary.reverse_declared_size,
            );
        }

        for hunk in &mut fp.hunks {
            std::mem::swap(&mut hunk.old_start, &mut hunk.new_start);
            std::mem::swap(&mut hunk.old_count, &mut hunk.new_count);
            let new_lines: Vec<HunkLine> = hunk
                .lines
                .drain(..)
                .map(|hl| match hl {
                    HunkLine::Add(s) => HunkLine::Remove(s),
                    HunkLine::Remove(s) => HunkLine::Add(s),
                    other => other,
                })
                .collect();
            hunk.lines = new_lines;
        }
    }

    // Apply reversed patchsets in reverse file order so that a later patch in
    // the forward direction is undone first. This matches Git's reverse-apply
    // behavior for multi-part patches touching the same path.
    patches.reverse();
}

/// When `diff --git` lists two different paths but `---` / `+++` agree on the
/// real file (same path after stripping `a/` / `b/`), treat the patch as a
/// normal same-file diff.
///
/// This matches Git's tolerance for a common mistake: `sed` with a single
/// substitution per line only rewrites the first `a/file` or `b/file` on the
/// `diff --git` line, leaving `a/target b/file` while both traditional headers
/// say `target` (see `t4124-apply-ws-rule.sh`).
fn assign_ws_rules(patches: &mut [FilePatch], args: &Args) {
    let cfg_rule = config_whitespace_rule_bits();
    let setup_prefix = apply_setup_prefix();
    let Ok(repo) = Repository::discover(None) else {
        for fp in patches {
            fp.ws_rule = cfg_rule;
        }
        return;
    };
    let config = ConfigSet::load(Some(&repo.git_dir), true).unwrap_or_else(|_| ConfigSet::new());
    let rules = repo
        .work_tree
        .as_ref()
        .map(|wt| crlf::load_gitattributes(wt))
        .unwrap_or_default();

    for fp in patches.iter_mut() {
        let path_for_attr = fp
            .target_path()
            .or_else(|| fp.effective_path())
            .unwrap_or("");
        let adjusted = adjust_path(path_for_attr, args.directory.as_deref());
        let operational = fp.worktree_rel_operational(&adjusted, &setup_prefix);
        let fa = crlf::get_file_attrs(&rules, &operational, false, &config);
        let attr = match fa.whitespace.as_deref() {
            None => WhitespaceGitAttr::Unspecified,
            Some("unset") => WhitespaceGitAttr::False,
            Some("set") => WhitespaceGitAttr::True,
            Some(s) => WhitespaceGitAttr::String(s.to_owned()),
        };
        fp.ws_rule = attr.merge_with_config(cfg_rule).unwrap_or(cfg_rule);

        let mode = fp.new_mode.as_deref().or(fp.old_mode.as_deref());
        if mode == Some("120000") {
            fp.ws_rule &= !WS_INCOMPLETE_LINE;
        }
    }
}

fn normalize_mismatched_diff_git_paths(patches: &mut [FilePatch], strip: usize) {
    fn strip_leading_ab(p: &str) -> &str {
        p.strip_prefix("a/")
            .or_else(|| p.strip_prefix("b/"))
            .unwrap_or(p)
    }

    for fp in patches.iter_mut() {
        let (Some(d_old_raw), Some(d_new_raw)) =
            (fp.diff_old_path.as_deref(), fp.diff_new_path.as_deref())
        else {
            continue;
        };
        let d_old = path_after_strip(d_old_raw, strip);
        let d_new = path_after_strip(d_new_raw, strip);
        if d_old == d_new {
            continue;
        }
        let (Some(o), Some(n)) = (fp.old_path.as_deref(), fp.new_path.as_deref()) else {
            continue;
        };
        if o == "/dev/null" || n == "/dev/null" {
            continue;
        }
        let ho = strip_leading_ab(o);
        let hn = strip_leading_ab(n);
        if ho == hn {
            fp.diff_old_path = Some(format!("a/{ho}"));
            fp.diff_new_path = Some(format!("b/{hn}"));
        }
    }
}

fn validate_patch_headers(patches: &[FilePatch], strip: usize) -> Result<()> {
    for fp in patches {
        if (fp.diff_old_path.is_some() || fp.diff_new_path.is_some())
            && !fp.hunks.is_empty()
            && (!fp.saw_old_header || !fp.saw_new_header)
        {
            bail!("patch lacks filename information");
        }

        if fp.saw_old_header {
            if let (Some(diff_old), Some(old)) =
                (fp.diff_old_path.as_deref(), fp.old_path.as_deref())
            {
                if old != "/dev/null" && path_after_strip(diff_old, strip) != old && diff_old != old
                {
                    bail!("inconsistent old filename");
                }
            }
        }

        if fp.saw_new_header {
            if let (Some(diff_new), Some(new)) =
                (fp.diff_new_path.as_deref(), fp.new_path.as_deref())
            {
                if new != "/dev/null" && path_after_strip(diff_new, strip) != new && diff_new != new
                {
                    bail!("inconsistent new filename");
                }
            }
        }
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// Applying hunks to content
// ---------------------------------------------------------------------------

/// Apply hunks to file content (a list of lines). Returns new content.
///
/// Find the best starting position for a hunk by scanning around the nominal
/// position. When `allow_unidiff_zero_fallback` is true, a `--unidiff-zero`
/// style fallback is used where all old-side lines (context/remove) may match
/// as an ordered subsequence (not necessarily contiguous) at a candidate
/// location.
fn find_hunk_start(
    old_lines: &[&str],
    hunk: &Hunk,
    nominal: usize,
    ws_mode: ApplyWhitespaceMode,
    allow_unidiff_zero_fallback: bool,
    fp: &FilePatch,
) -> usize {
    let adjusted_nominal = if hunk.old_count > 0 {
        let required_context = ws_mode
            .context
            .unwrap_or(hunk.old_count)
            .min(hunk.old_count);
        nominal.saturating_sub(hunk.old_count - required_context)
    } else {
        nominal
    };

    if hunk.old_count == 0 {
        return adjusted_nominal.min(old_lines.len());
    }

    if match_hunk_old_side_at(
        old_lines,
        hunk,
        adjusted_nominal,
        ws_mode,
        allow_unidiff_zero_fallback,
        fp,
    ) {
        return adjusted_nominal;
    }

    let max_scan = old_lines.len();
    for delta in 1..=max_scan {
        if adjusted_nominal >= delta
            && match_hunk_old_side_at(
                old_lines,
                hunk,
                adjusted_nominal - delta,
                ws_mode,
                allow_unidiff_zero_fallback,
                fp,
            )
        {
            return adjusted_nominal - delta;
        }
        if adjusted_nominal + delta <= old_lines.len()
            && match_hunk_old_side_at(
                old_lines,
                hunk,
                adjusted_nominal + delta,
                ws_mode,
                allow_unidiff_zero_fallback,
                fp,
            )
        {
            return adjusted_nominal + delta;
        }
    }

    adjusted_nominal.min(old_lines.len())
}

fn hunk_matches_at_position(
    old_lines: &[&str],
    hunk: &Hunk,
    start: usize,
    match_beginning: bool,
    match_end: bool,
    ws_mode: ApplyWhitespaceMode,
    fp: &FilePatch,
) -> bool {
    let old_n = old_side_line_count(hunk);
    if old_n == 0 {
        return true;
    }
    if start + old_n > old_lines.len() {
        return false;
    }
    if match_beginning && start != 0 {
        return false;
    }
    if match_end && start + old_n != old_lines.len() {
        return false;
    }
    hunk_old_side_matches_at(old_lines, hunk, start, ws_mode, fp)
}

/// Git `find_pos`: alternate scan from nominal line until a match or exhaustion.
fn find_pos_scan(
    old_lines: &[&str],
    hunk: &Hunk,
    ws_mode: ApplyWhitespaceMode,
    fp: &FilePatch,
    match_beginning: bool,
    match_end: bool,
    start_line: usize,
) -> Option<usize> {
    let old_n = old_side_line_count(hunk);
    let line = if match_beginning {
        0usize
    } else if match_end {
        old_lines.len().saturating_sub(old_n)
    } else {
        start_line.min(old_lines.len())
    };
    if line > old_lines.len() {
        return None;
    }

    let mut backwards_lno = line;
    let mut forwards_lno = line;
    let mut current_lno = line;
    let mut i = 0usize;
    loop {
        if hunk_matches_at_position(
            old_lines,
            hunk,
            current_lno,
            match_beginning,
            match_end,
            ws_mode,
            fp,
        ) {
            return Some(current_lno);
        }
        if backwards_lno == 0 && forwards_lno >= old_lines.len() {
            break;
        }
        if i & 1 == 1 {
            if backwards_lno == 0 {
                i += 1;
                continue;
            }
            backwards_lno -= 1;
            current_lno = backwards_lno;
        } else {
            if forwards_lno >= old_lines.len() {
                i += 1;
                continue;
            }
            forwards_lno += 1;
            current_lno = forwards_lno;
        }
        i += 1;
    }
    None
}

fn nominal_hunk_start(hunk: &Hunk, old_len: usize) -> Result<usize> {
    if hunk.old_count == 0 {
        let start = if hunk.old_start == 0 {
            0
        } else {
            hunk.old_start
        };
        if start > old_len {
            bail!("patch does not apply");
        }
        return Ok(start);
    }

    if hunk.old_start == 0 {
        bail!("patch does not apply");
    }
    let start = hunk.old_start - 1;
    Ok(start.min(old_len))
}

fn old_side_line_count(hunk: &Hunk) -> usize {
    hunk.lines
        .iter()
        .filter(|hl| matches!(hl, HunkLine::Context(_) | HunkLine::Remove(_)))
        .count()
}

/// Check whether hunk old-side lines match at the given candidate start.
fn match_hunk_old_side_at(
    old_lines: &[&str],
    hunk: &Hunk,
    start: usize,
    ws_mode: ApplyWhitespaceMode,
    allow_unidiff_zero_fallback: bool,
    fp: &FilePatch,
) -> bool {
    let old_side: Vec<&str> = hunk
        .lines
        .iter()
        .filter_map(|hl| match hl {
            HunkLine::Context(s) | HunkLine::Remove(s) => Some(s.as_str()),
            _ => None,
        })
        .collect();

    if old_side.is_empty() {
        return start <= old_lines.len();
    }

    if let Some(required) = ws_mode.context {
        let required = required.min(old_side.len());
        let max_leading_fuzz = old_side.len().saturating_sub(required);
        for leading_fuzz in 0..=max_leading_fuzz {
            let candidate_start = start + leading_fuzz;
            if let Some(sub) = hunk_slice_from_old_side_skip(hunk, leading_fuzz) {
                let slice_hunk = sub.as_hunk_ref();
                if candidate_start + sub.old_n <= old_lines.len()
                    && hunk_old_side_matches_at(
                        old_lines,
                        &slice_hunk,
                        candidate_start,
                        ws_mode,
                        fp,
                    )
                {
                    return true;
                }
            }
        }
    }

    if start + old_side.len() <= old_lines.len()
        && hunk_old_side_matches_at(old_lines, hunk, start, ws_mode, fp)
    {
        return true;
    }

    allow_unidiff_zero_fallback
        && is_subsequence_match_ws(
            &old_lines[start.min(old_lines.len())..],
            &old_side,
            ws_mode,
            fp,
        )
}

/// Hunk view that is `original` with the first `skip` old-side lines removed from the body.
#[derive(Debug, Clone)]
struct HunkSliceView {
    lines: Vec<HunkLine>,
    old_start: usize,
    new_start: usize,
    old_count: usize,
    new_count: usize,
    first_body_line: usize,
    old_n: usize,
}

fn hunk_slice_from_old_side_skip(hunk: &Hunk, skip: usize) -> Option<HunkSliceView> {
    if skip == 0 {
        return Some(HunkSliceView {
            lines: hunk.lines.clone(),
            old_start: hunk.old_start,
            new_start: hunk.new_start,
            old_count: hunk.old_count,
            new_count: hunk.new_count,
            first_body_line: hunk.first_body_line,
            old_n: old_side_line_count(hunk),
        });
    }
    let mut li = 0usize;
    let mut skipped = 0usize;
    let mut old_skipped = 0usize;
    let mut new_skipped = 0usize;
    while li < hunk.lines.len() && skipped < skip {
        match &hunk.lines[li] {
            HunkLine::Context(_) => {
                let has_nl = !matches!(hunk.lines.get(li + 1), Some(HunkLine::NoNewline));
                old_skipped += 1;
                new_skipped += 1;
                li += 1;
                if !has_nl {
                    li += 1;
                }
                skipped += 1;
            }
            HunkLine::Remove(_) => {
                let has_nl = !matches!(hunk.lines.get(li + 1), Some(HunkLine::NoNewline));
                old_skipped += 1;
                li += 1;
                if !has_nl {
                    li += 1;
                }
                skipped += 1;
            }
            HunkLine::Add(_) => {
                return None;
            }
            HunkLine::NoNewline => {
                li += 1;
            }
        }
    }
    if skipped != skip || li > hunk.lines.len() {
        return None;
    }
    let rest = hunk.lines[li..].to_vec();
    if rest.is_empty() {
        return None;
    }
    let old_n = old_side_line_count(&Hunk {
        old_start: 0,
        old_count: 0,
        new_start: 0,
        new_count: 0,
        first_body_line: 0,
        lines: rest.clone(),
    });
    Some(HunkSliceView {
        lines: rest,
        old_start: hunk.old_start.saturating_add(old_skipped),
        new_start: hunk.new_start.saturating_add(new_skipped),
        old_count: hunk.old_count.saturating_sub(old_skipped),
        new_count: hunk.new_count.saturating_sub(new_skipped),
        first_body_line: hunk.first_body_line,
        old_n,
    })
}

impl HunkSliceView {
    fn as_hunk_ref(&self) -> Hunk {
        Hunk {
            old_start: self.old_start,
            old_count: self.old_count,
            new_start: self.new_start,
            new_count: self.new_count,
            first_body_line: self.first_body_line,
            lines: self.lines.clone(),
        }
    }
}

/// Git `match_fragment` whitespace-fix check: `ws_fix_copy(preimage)==ws_fix_copy(target)` per line.
fn ws_fix_lines_match_patch_to_actual(
    patch_line: &str,
    actual_line: &str,
    has_nl: bool,
    ws_rule: u32,
) -> bool {
    let pre_body = patch_line_body_for_ws(patch_line, has_nl);
    let tgt_body = patch_line_body_for_ws(actual_line, has_nl);
    let (pre_fixed, _) = ws::ws_fix_copy_line(&pre_body, ws_rule);
    let (tgt_fixed, _) = ws::ws_fix_copy_line(&tgt_body, ws_rule);
    pre_fixed == tgt_fixed
}

fn patch_line_matches_actual(
    patch_line: &str,
    actual_line: &str,
    has_nl: bool,
    ws_mode: ApplyWhitespaceMode,
    fp: &FilePatch,
) -> bool {
    if ws_mode.whitespace_fix {
        ws_fix_lines_match_patch_to_actual(patch_line, actual_line, has_nl, fp.ws_rule)
    } else {
        lines_equal(patch_line, actual_line, ws_mode)
    }
}

fn hunk_old_side_matches_at(
    old_lines: &[&str],
    hunk: &Hunk,
    start: usize,
    ws_mode: ApplyWhitespaceMode,
    fp: &FilePatch,
) -> bool {
    hunk_old_side_matches_at_impl(old_lines, &hunk.lines, start, ws_mode, fp)
}

fn hunk_old_side_matches_at_impl(
    old_lines: &[&str],
    hunk_lines: &[HunkLine],
    start: usize,
    ws_mode: ApplyWhitespaceMode,
    fp: &FilePatch,
) -> bool {
    let mut old_idx = start;
    let mut li = 0usize;
    while li < hunk_lines.len() {
        match &hunk_lines[li] {
            HunkLine::Context(s) => {
                let has_nl = !matches!(hunk_lines.get(li + 1), Some(HunkLine::NoNewline));
                if old_idx >= old_lines.len() {
                    return false;
                }
                let ok = patch_line_matches_actual(s, old_lines[old_idx], has_nl, ws_mode, fp);
                if !ok {
                    return false;
                }
                old_idx += 1;
                li += 1;
                if !has_nl {
                    li += 1;
                }
            }
            HunkLine::Remove(s) => {
                let has_nl = !matches!(hunk_lines.get(li + 1), Some(HunkLine::NoNewline));
                if old_idx >= old_lines.len() {
                    return false;
                }
                let ok = patch_line_matches_actual(s, old_lines[old_idx], has_nl, ws_mode, fp);
                if !ok {
                    return false;
                }
                old_idx += 1;
                li += 1;
                if !has_nl {
                    li += 1;
                }
            }
            HunkLine::Add(_) => {
                let has_nl = !matches!(hunk_lines.get(li + 1), Some(HunkLine::NoNewline));
                li += 1;
                if !has_nl {
                    li += 1;
                }
            }
            HunkLine::NoNewline => {
                li += 1;
            }
        }
    }
    true
}

fn is_subsequence_match_ws(
    haystack: &[&str],
    needle: &[&str],
    ws_mode: ApplyWhitespaceMode,
    fp: &FilePatch,
) -> bool {
    if needle.is_empty() {
        return true;
    }

    let mut pos = 0usize;
    for expected in needle {
        while pos < haystack.len()
            && !line_matches_patch_to_actual(expected, haystack[pos], ws_mode, fp)
        {
            pos += 1;
        }
        if pos == haystack.len() {
            return false;
        }
        pos += 1;
    }

    true
}

fn line_matches_patch_to_actual(
    patch_line: &str,
    actual_line: &str,
    ws_mode: ApplyWhitespaceMode,
    fp: &FilePatch,
) -> bool {
    patch_line_matches_actual(patch_line, actual_line, true, ws_mode, fp)
}

/// Returns true if `needle` appears in `haystack` in order (not necessarily
/// contiguously), using whitespace-aware line comparison.
fn is_subsequence_match(haystack: &[&str], needle: &[&str], ws_mode: ApplyWhitespaceMode) -> bool {
    if needle.is_empty() {
        return true;
    }

    let mut pos = 0usize;
    for expected in needle {
        while pos < haystack.len() && !lines_equal(expected, haystack[pos], ws_mode) {
            pos += 1;
        }
        if pos == haystack.len() {
            return false;
        }
        pos += 1;
    }

    true
}

fn canonicalize_ws_line(line: &str) -> String {
    let mut end = line.len();
    while end > 0 {
        let b = line.as_bytes()[end - 1];
        if b == b' ' || b == b'\t' {
            end -= 1;
        } else {
            break;
        }
    }
    line[..end].to_string()
}

fn canonicalize_space_change_line(line: &str) -> String {
    let mut normalized = String::with_capacity(line.len());
    let mut in_space = false;
    for c in line.chars() {
        if c.is_whitespace() {
            if !in_space {
                normalized.push(' ');
                in_space = true;
            }
        } else {
            normalized.push(c);
            in_space = false;
        }
    }
    normalized.trim_end().to_owned()
}

fn expand_tabs_for_compare(line: &str, tab_width: usize) -> String {
    let mut out = String::with_capacity(line.len());
    let mut col = 0usize;
    for ch in line.chars() {
        match ch {
            '\t' => {
                let stop = tab_width.saturating_sub(col % tab_width).max(1);
                out.push_str(&" ".repeat(stop));
                col += stop;
            }
            _ => {
                out.push(ch);
                col += 1;
            }
        }
    }
    out
}

fn normalize_ws_fix_line(line: &str, tab_width: usize) -> String {
    let mut out = String::with_capacity(line.len());
    let mut in_indent = true;
    let mut col = 0usize;

    for ch in line.chars() {
        if in_indent {
            match ch {
                ' ' => {
                    out.push(' ');
                    col += 1;
                    continue;
                }
                '\t' => {
                    let stop = tab_width.saturating_sub(col % tab_width).max(1);
                    out.push_str(&" ".repeat(stop));
                    col += stop;
                    continue;
                }
                _ => in_indent = false,
            }
        }
        out.push(ch);
        col += 1;
    }

    canonicalize_ws_line(&out)
}

fn patch_line_body_for_ws(s: &str, has_newline: bool) -> String {
    if has_newline {
        format!("{s}\n")
    } else {
        s.to_string()
    }
}

fn record_ws_error(
    ws_mode: &ApplyWhitespaceMode,
    ws_errors: &mut u32,
    flags: u32,
    patch_file: &str,
    patch_linenr: usize,
    incomplete_line_body: Option<&str>,
) {
    if flags == 0 {
        return;
    }
    *ws_errors += 1;
    if ws_mode.ws_squelch > 0 && ws_mode.ws_squelch < *ws_errors {
        return;
    }
    if flags & WS_INCOMPLETE_LINE != 0 {
        if let Some(body) = incomplete_line_body {
            eprintln!("{patch_file}:{patch_linenr}: no newline at the end of file.\n{body}");
        }
    } else if flags & WS_BLANK_AT_EOF != 0 {
        eprintln!("{patch_file}:{patch_linenr}: new blank line at EOF.");
    } else {
        eprintln!(
            "{patch_file}:{patch_linenr}: {}.",
            ws::whitespace_error_string(flags)
        );
    }
}

/// Count leading and trailing context lines in a hunk body (Git `fragment->leading` / `trailing`).
fn hunk_leading_trailing_context(hunk: &Hunk) -> (usize, usize) {
    let mut leading = 0usize;
    let mut trailing = 0usize;
    let mut saw_change = false;
    let mut li = 0usize;
    while li < hunk.lines.len() {
        match &hunk.lines[li] {
            HunkLine::Context(_) => {
                if !saw_change {
                    leading += 1;
                }
                trailing += 1;
                li += 1;
                if matches!(hunk.lines.get(li), Some(HunkLine::NoNewline)) {
                    li += 1;
                }
            }
            HunkLine::Remove(_) | HunkLine::Add(_) => {
                saw_change = true;
                trailing = 0;
                li += 1;
                if matches!(hunk.lines.get(li), Some(HunkLine::NoNewline)) {
                    li += 1;
                }
            }
            HunkLine::NoNewline => {
                li += 1;
            }
        }
    }
    (leading, trailing)
}

/// Remove one leading context line (and optional `\ No newline`) from a hunk; adjust header counts.
fn pop_leading_context_line(hunk: &mut Hunk) -> bool {
    if !matches!(hunk.lines.first(), Some(HunkLine::Context(_))) {
        return false;
    }
    let mut end = 1usize;
    if matches!(hunk.lines.get(1), Some(HunkLine::NoNewline)) {
        end = 2;
    }
    hunk.lines.drain(0..end);
    hunk.old_count = hunk.old_count.saturating_sub(1);
    hunk.new_count = hunk.new_count.saturating_sub(1);
    hunk.old_start = hunk.old_start.saturating_add(1);
    hunk.new_start = hunk.new_start.saturating_add(1);
    true
}

/// Remove one trailing context line (and optional `\ No newline`) from a hunk; adjust header counts.
/// After a position is found, drop trailing context from the hunk while it still matches there.
/// This mirrors Git shrinking the preimage/postimage once a match is known: lines removed from
/// the hunk are not re-emitted (they stay as raw worktree bytes), which preserves trailing
/// whitespace beyond the "context horizon" (t4125).
fn minimize_trailing_context_at(
    hunk: &mut Hunk,
    old_lines: &[&str],
    start: usize,
    ws_mode: ApplyWhitespaceMode,
    fp: &FilePatch,
) {
    // Git may drop at most one trailing context line past the "horizon" while still matching
    // (t4125: only the final `h` line stays unfixed). Removing more would leave earlier trailing
    // context unprocessed while still satisfying a shortened preimage match.
    if hunk_leading_trailing_context(hunk).1 == 0 {
        return;
    }
    let mut trial = hunk.clone();
    if !pop_trailing_context_line(&mut trial) {
        return;
    }
    if hunk_old_side_matches_at(old_lines, &trial, start, ws_mode, fp) {
        *hunk = trial;
    }
}

fn pop_trailing_context_line(hunk: &mut Hunk) -> bool {
    if hunk.lines.is_empty() {
        return false;
    }
    let mut i = hunk.lines.len();
    if matches!(hunk.lines.last(), Some(HunkLine::NoNewline)) {
        i -= 1;
    }
    if i == 0 {
        return false;
    }
    i -= 1;
    if !matches!(hunk.lines.get(i), Some(HunkLine::Context(_))) {
        return false;
    }
    let end = if matches!(hunk.lines.get(i + 1), Some(HunkLine::NoNewline)) {
        i + 2
    } else {
        i + 1
    };
    hunk.lines.drain(i..end);
    hunk.old_count = hunk.old_count.saturating_sub(1);
    hunk.new_count = hunk.new_count.saturating_sub(1);
    true
}

fn lines_equal(expected: &str, actual: &str, ws_mode: ApplyWhitespaceMode) -> bool {
    if expected == actual {
        return true;
    }
    if ws_mode.whitespace_fix
        && expand_tabs_for_compare(expected, ws_mode.tab_width)
            == expand_tabs_for_compare(actual, ws_mode.tab_width)
    {
        return true;
    }
    if ws_mode.ignore_space_change
        && canonicalize_space_change_line(expected) == canonicalize_space_change_line(actual)
    {
        return true;
    }
    ws_mode.whitespace_fix
        && normalize_ws_fix_line(expected, ws_mode.tab_width)
            == normalize_ws_fix_line(actual, ws_mode.tab_width)
}

fn apply_hunks(
    old_content: &str,
    fp: &FilePatch,
    ws_mode: ApplyWhitespaceMode,
    forward: bool,
    patch_input_display: &str,
) -> Result<String> {
    let hunks = &fp.hunks;
    // Split into lines, keeping track of trailing newline
    let has_trailing_newline = old_content.is_empty() || old_content.ends_with('\n');
    let old_lines: Vec<&str> = if old_content.is_empty() {
        Vec::new()
    } else {
        old_content.lines().collect()
    };

    let mut result: Vec<String> = Vec::new();
    let mut old_idx: usize = 0; // 0-based index into old_lines
    let mut offset: isize = 0; // accumulated offset from previous hunks
    let mut ws_errors: u32 = 0;

    for hunk in hunks {
        let mut hunk_to_apply = hunk.clone();
        let p_ctx = ws_mode.context.unwrap_or(usize::MAX);

        // Fuzz-retry scaffold: the per-hunk match below is structured as a loop so reduced-context
        // retries can re-enter it, but every current path exits on the first iteration (the retry
        // re-entry is not yet wired). Pre-existing; kept as-is to avoid changing patch-application
        // behavior (covered by the t40xx apply suite).
        #[allow(clippy::never_loop)]
        loop {
            let required_context = ws_mode
                .context
                .unwrap_or(hunk_to_apply.old_count)
                .min(hunk_to_apply.old_count);
            let mut leading_context_fuzz_remaining = if ws_mode.context.is_some() {
                hunk_to_apply.old_count.saturating_sub(required_context)
            } else {
                0
            };
            let mut matched_old_side = false;

            let mut eof_new_blank = 0usize;
            let mut eof_found_line = 0usize;

            let nominal_start = nominal_hunk_start(&hunk_to_apply, old_lines.len())? as isize;
            let hunk_start = (nominal_start + offset).max(0) as usize;

            let (mut leading, mut trailing) = hunk_leading_trailing_context(&hunk_to_apply);
            let mut match_beginning = hunk_to_apply.old_start == 0
                || (hunk_to_apply.old_start == 1 && !ws_mode.unidiff_zero);
            let mut match_end =
                !ws_mode.unidiff_zero && trailing == 0 && hunk_to_apply.old_count > 0;

            let mut applied_pos: Option<usize> = None;
            loop {
                if !match_beginning && !match_end {
                    let adjusted_nominal = if hunk_to_apply.old_count > 0 {
                        let required_context = ws_mode
                            .context
                            .unwrap_or(hunk_to_apply.old_count)
                            .min(hunk_to_apply.old_count);
                        hunk_start.saturating_sub(hunk_to_apply.old_count - required_context)
                    } else {
                        hunk_start
                    };
                    if match_hunk_old_side_at(
                        old_lines.as_slice(),
                        &hunk_to_apply,
                        adjusted_nominal,
                        ws_mode,
                        ws_mode.context.is_some(),
                        fp,
                    ) {
                        applied_pos = Some(adjusted_nominal);
                    } else {
                        let max_scan = old_lines.len();
                        for delta in 1..=max_scan {
                            if adjusted_nominal >= delta
                                && match_hunk_old_side_at(
                                    old_lines.as_slice(),
                                    &hunk_to_apply,
                                    adjusted_nominal - delta,
                                    ws_mode,
                                    ws_mode.context.is_some(),
                                    fp,
                                )
                            {
                                applied_pos = Some(adjusted_nominal - delta);
                                break;
                            }
                            if adjusted_nominal + delta <= old_lines.len()
                                && match_hunk_old_side_at(
                                    old_lines.as_slice(),
                                    &hunk_to_apply,
                                    adjusted_nominal + delta,
                                    ws_mode,
                                    ws_mode.context.is_some(),
                                    fp,
                                )
                            {
                                applied_pos = Some(adjusted_nominal + delta);
                                break;
                            }
                        }
                    }
                }

                if applied_pos.is_none() {
                    applied_pos = find_pos_scan(
                        old_lines.as_slice(),
                        &hunk_to_apply,
                        ws_mode,
                        fp,
                        match_beginning,
                        match_end,
                        hunk_start,
                    );
                }

                if applied_pos.is_some() {
                    break;
                }

                if leading <= p_ctx && trailing <= p_ctx {
                    break;
                }

                if match_beginning || match_end {
                    match_beginning = false;
                    match_end = false;
                    continue;
                }

                let mut did_reduce = false;
                if leading >= trailing {
                    did_reduce |= pop_leading_context_line(&mut hunk_to_apply);
                }
                let (l_after_lead, t_after_lead) = hunk_leading_trailing_context(&hunk_to_apply);
                if t_after_lead > l_after_lead {
                    did_reduce |= pop_trailing_context_line(&mut hunk_to_apply);
                }
                if !did_reduce {
                    break;
                }
                (leading, trailing) = hunk_leading_trailing_context(&hunk_to_apply);
                match_end = !ws_mode.unidiff_zero && trailing == 0 && hunk_to_apply.old_count > 0;
            }

            let Some(actual_start) = applied_pos else {
                bail!("patch does not apply");
            };

            if ws_mode.whitespace_fix {
                minimize_trailing_context_at(
                    &mut hunk_to_apply,
                    old_lines.as_slice(),
                    actual_start,
                    ws_mode,
                    fp,
                );
            }

            while old_idx < actual_start && old_idx < old_lines.len() {
                result.push(old_lines[old_idx].to_string());
                old_idx += 1;
            }

            if actual_start != hunk_start {
                offset += actual_start as isize - hunk_start as isize;
            }

            let apply_outcome = (|| -> Result<()> {
                let mut li = 0usize;
                let mut cur_patch_line = hunk_to_apply.first_body_line;
                let mut pending_incomplete_body: Option<String> = None;
                while li < hunk_to_apply.lines.len() {
                    match &hunk_to_apply.lines[li] {
                        HunkLine::Context(s) => {
                            pending_incomplete_body = None;
                            let has_nl = !matches!(
                                hunk_to_apply.lines.get(li + 1),
                                Some(HunkLine::NoNewline)
                            );
                            let body = patch_line_body_for_ws(s, has_nl);
                            let plen = if has_nl {
                                body.len().saturating_sub(1)
                            } else {
                                body.len()
                            };
                            let is_blank_context = (fp.ws_rule & WS_BLANK_AT_EOF) != 0
                                && (plen == 0 || ws::ws_blank_line(&body[..plen]));
                            if !is_blank_context {
                                eof_new_blank = 0;
                            }
                            if ws_mode.whitespace_fix {
                                let flags = ws::ws_check(&body, fp.ws_rule);
                                record_ws_error(
                                    &ws_mode,
                                    &mut ws_errors,
                                    flags,
                                    patch_input_display,
                                    cur_patch_line,
                                    None,
                                );
                            }
                            if old_idx >= old_lines.len() {
                                bail!(
                                    "context mismatch at line {}: expected {:?}, got EOF",
                                    old_idx + 1,
                                    s
                                );
                            }
                            let actual_line = old_lines[old_idx];
                            if !patch_line_matches_actual(s, actual_line, has_nl, ws_mode, fp) {
                                if ws_mode.context.is_some()
                                    && !matched_old_side
                                    && leading_context_fuzz_remaining > 0
                                {
                                    result.push(actual_line.to_string());
                                    old_idx += 1;
                                    leading_context_fuzz_remaining -= 1;
                                    cur_patch_line += 1;
                                    li += 1;
                                    if !has_nl {
                                        cur_patch_line += 1;
                                        li += 1;
                                    }
                                    continue;
                                }
                                bail!(
                                    "context mismatch at line {}: expected {:?}, got {:?}",
                                    old_idx + 1,
                                    s,
                                    actual_line
                                );
                            }
                            old_idx += 1;
                            let out_line = if ws_mode.whitespace_fix {
                                let body_in = patch_line_body_for_ws(actual_line, has_nl);
                                let (fixed, _) = ws::ws_fix_copy_line(&body_in, fp.ws_rule);
                                fixed.trim_end_matches('\n').to_string()
                            } else {
                                actual_line.to_string()
                            };
                            result.push(out_line);
                            matched_old_side = true;
                            cur_patch_line += 1;
                            li += 1;
                            if !has_nl {
                                cur_patch_line += 1;
                                li += 1;
                            }
                        }
                        HunkLine::Remove(s) => {
                            eof_new_blank = 0;
                            let has_nl = !matches!(
                                hunk_to_apply.lines.get(li + 1),
                                Some(HunkLine::NoNewline)
                            );
                            let check_side = if forward {
                                false
                            } else {
                                ws_mode.ws_cli != WsCliMode::NoWarn
                            };
                            if check_side {
                                let body = patch_line_body_for_ws(s, has_nl);
                                let flags = ws::ws_check(&body, fp.ws_rule);
                                if !has_nl && (flags & WS_INCOMPLETE_LINE) != 0 {
                                    pending_incomplete_body = Some(s.clone());
                                    let rest = flags & !WS_INCOMPLETE_LINE;
                                    record_ws_error(
                                        &ws_mode,
                                        &mut ws_errors,
                                        rest,
                                        patch_input_display,
                                        cur_patch_line,
                                        None,
                                    );
                                } else {
                                    pending_incomplete_body = None;
                                    record_ws_error(
                                        &ws_mode,
                                        &mut ws_errors,
                                        flags,
                                        patch_input_display,
                                        cur_patch_line,
                                        None,
                                    );
                                }
                            } else if !has_nl {
                                pending_incomplete_body = None;
                            }
                            if old_idx >= old_lines.len() {
                                bail!(
                                    "remove mismatch at line {}: expected {:?}, got EOF",
                                    old_idx + 1,
                                    s
                                );
                            }
                            if !patch_line_matches_actual(
                                s,
                                old_lines[old_idx],
                                has_nl,
                                ws_mode,
                                fp,
                            ) {
                                bail!(
                                    "remove mismatch at line {}: expected {:?}, got {:?}",
                                    old_idx + 1,
                                    s,
                                    old_lines[old_idx]
                                );
                            }
                            old_idx += 1;
                            matched_old_side = true;
                            if !has_nl {
                                if let Some(body) = pending_incomplete_body.take() {
                                    if ws_mode.ws_cli != WsCliMode::NoWarn {
                                        record_ws_error(
                                            &ws_mode,
                                            &mut ws_errors,
                                            WS_INCOMPLETE_LINE,
                                            patch_input_display,
                                            cur_patch_line + 1,
                                            Some(body.as_str()),
                                        );
                                    }
                                }
                                cur_patch_line += 2;
                                li += 2;
                            } else {
                                cur_patch_line += 1;
                                li += 1;
                            }
                        }
                        HunkLine::Add(s) => {
                            let has_nl = !matches!(
                                hunk_to_apply.lines.get(li + 1),
                                Some(HunkLine::NoNewline)
                            );
                            let body = patch_line_body_for_ws(s, has_nl);
                            let plen = if has_nl {
                                body.len().saturating_sub(1)
                            } else {
                                body.len()
                            };
                            let added_blank = (fp.ws_rule & WS_BLANK_AT_EOF) != 0
                                && ws::ws_blank_line(&body[..plen]);
                            if added_blank {
                                if eof_new_blank == 0 {
                                    eof_found_line = cur_patch_line;
                                }
                                eof_new_blank += 1;
                            } else {
                                eof_new_blank = 0;
                            }
                            let check_side = if forward {
                                ws_mode.ws_cli != WsCliMode::NoWarn
                            } else {
                                false
                            };
                            if check_side {
                                let flags = ws::ws_check(&body, fp.ws_rule);
                                if !has_nl && (flags & WS_INCOMPLETE_LINE) != 0 {
                                    pending_incomplete_body = Some(s.clone());
                                    let rest = flags & !WS_INCOMPLETE_LINE;
                                    record_ws_error(
                                        &ws_mode,
                                        &mut ws_errors,
                                        rest,
                                        patch_input_display,
                                        cur_patch_line,
                                        None,
                                    );
                                } else {
                                    pending_incomplete_body = None;
                                    record_ws_error(
                                        &ws_mode,
                                        &mut ws_errors,
                                        flags,
                                        patch_input_display,
                                        cur_patch_line,
                                        None,
                                    );
                                }
                            } else if !has_nl {
                                pending_incomplete_body = None;
                            }
                            let out_line = if ws_mode.whitespace_fix {
                                let body_in = patch_line_body_for_ws(s, has_nl);
                                let (fixed, _) = ws::ws_fix_copy_line(&body_in, fp.ws_rule);
                                fixed.trim_end_matches('\n').to_string()
                            } else {
                                s.clone()
                            };
                            result.push(out_line);
                            matched_old_side = true;
                            if !has_nl {
                                if let Some(body) = pending_incomplete_body.take() {
                                    if ws_mode.ws_cli != WsCliMode::NoWarn {
                                        record_ws_error(
                                            &ws_mode,
                                            &mut ws_errors,
                                            WS_INCOMPLETE_LINE,
                                            patch_input_display,
                                            cur_patch_line + 1,
                                            Some(body.as_str()),
                                        );
                                    }
                                }
                                cur_patch_line += 2;
                                li += 2;
                            } else {
                                cur_patch_line += 1;
                                li += 1;
                            }
                        }
                        HunkLine::NoNewline => {
                            cur_patch_line += 1;
                            li += 1;
                        }
                    }
                }

                if old_idx == old_lines.len()
                    && eof_new_blank > 0
                    && (fp.ws_rule & WS_BLANK_AT_EOF) != 0
                    && ws_mode.ws_cli != WsCliMode::NoWarn
                {
                    record_ws_error(
                        &ws_mode,
                        &mut ws_errors,
                        WS_BLANK_AT_EOF,
                        patch_input_display,
                        eof_found_line,
                        None,
                    );
                    if ws_mode.whitespace_fix {
                        for _ in 0..eof_new_blank {
                            if result.last().is_some_and(|l| ws::ws_blank_line(l)) {
                                result.pop();
                            }
                        }
                    }
                }
                Ok(())
            })();
            apply_outcome?;
            break;
        }
    }

    // Copy remaining lines after the last hunk
    while old_idx < old_lines.len() {
        result.push(old_lines[old_idx].to_string());
        old_idx += 1;
    }

    // Reconstruct with newlines
    if result.is_empty() {
        return Ok(String::new());
    }

    // Check if the last hunk has a no-newline marker for the new side.
    // The new side ends without newline if the last line that contributes
    // to the new side (Add or Context) is immediately followed by NoNewline.
    let ends_no_newline = hunks.last().is_some_and(|h| {
        let mut last_was_new_side = false; // true if last meaningful line goes to new side
        let mut saw_no_newline = false;
        for hl in &h.lines {
            match hl {
                HunkLine::Add(_) | HunkLine::Context(_) => {
                    last_was_new_side = true;
                    saw_no_newline = false;
                }
                HunkLine::NoNewline if last_was_new_side => {
                    saw_no_newline = true;
                }
                HunkLine::Remove(_) => {
                    last_was_new_side = false;
                    saw_no_newline = false;
                }
                _ => {}
            }
        }
        saw_no_newline
    });

    let mut out = result.join("\n");
    let force_no_trailing_newline =
        ws_mode.inaccurate_eof || (ws_mode.ignore_space_change && !has_trailing_newline);
    if !ends_no_newline && !force_no_trailing_newline && (has_trailing_newline || !hunks.is_empty())
    {
        out.push('\n');
    }

    if ws_errors > 0 && matches!(ws_mode.ws_cli, WsCliMode::Error) {
        bail!("{} line adds whitespace errors.", ws_errors);
    }

    Ok(out)
}

/// Apply hunks while collecting failed hunks for `--reject` mode.
///
/// Returns `(new_content, rejected_hunks)`.
fn apply_hunks_with_reject(
    old_content: &str,
    fp: &FilePatch,
    ws_mode: ApplyWhitespaceMode,
    forward: bool,
    patch_input_display: &str,
) -> Result<(String, Vec<Hunk>)> {
    let hunks = &fp.hunks;
    let has_trailing_newline = old_content.is_empty() || old_content.ends_with('\n');
    let mut lines: Vec<String> = if old_content.is_empty() {
        Vec::new()
    } else {
        old_content
            .lines()
            .map(std::string::ToString::to_string)
            .collect()
    };

    let mut offset: isize = 0;
    let mut rejected: Vec<Hunk> = Vec::new();
    let mut ws_errors: u32 = 0;

    for hunk in hunks {
        let mut eof_new_blank = 0usize;
        let mut eof_found_line = 0usize;

        let nominal_start = match nominal_hunk_start(hunk, lines.len()) {
            Ok(start) => start as isize,
            Err(_) => {
                rejected.push(hunk.clone());
                continue;
            }
        };
        let hunk_start = (nominal_start + offset).max(0) as usize;

        let old_lines: Vec<&str> = lines.iter().map(String::as_str).collect();
        let actual_start = find_hunk_start(
            &old_lines,
            hunk,
            hunk_start.min(old_lines.len()),
            ws_mode,
            ws_mode.context.is_some(),
            fp,
        );

        let mut idx = actual_start;
        let mut replacement: Vec<String> = Vec::new();
        let mut failed = false;

        let mut li = 0usize;
        let mut cur_patch_line = hunk.first_body_line;
        let mut pending_incomplete_body: Option<String> = None;
        while li < hunk.lines.len() {
            match &hunk.lines[li] {
                HunkLine::Context(s) => {
                    pending_incomplete_body = None;
                    let has_nl = !matches!(hunk.lines.get(li + 1), Some(HunkLine::NoNewline));
                    let body = patch_line_body_for_ws(s, has_nl);
                    let plen = if has_nl {
                        body.len().saturating_sub(1)
                    } else {
                        body.len()
                    };
                    let is_blank_context = (fp.ws_rule & WS_BLANK_AT_EOF) != 0
                        && (plen == 0 || ws::ws_blank_line(&body[..plen]));
                    if !is_blank_context {
                        eof_new_blank = 0;
                    }
                    if ws_mode.whitespace_fix {
                        let flags = ws::ws_check(&body, fp.ws_rule);
                        record_ws_error(
                            &ws_mode,
                            &mut ws_errors,
                            flags,
                            patch_input_display,
                            cur_patch_line,
                            None,
                        );
                    }
                    if idx >= lines.len()
                        || !patch_line_matches_actual(s, &lines[idx], has_nl, ws_mode, fp)
                    {
                        failed = true;
                        break;
                    }
                    let out_line = if ws_mode.whitespace_fix {
                        let body_in = patch_line_body_for_ws(&lines[idx], has_nl);
                        let (fixed, _) = ws::ws_fix_copy_line(&body_in, fp.ws_rule);
                        fixed.trim_end_matches('\n').to_string()
                    } else {
                        lines[idx].clone()
                    };
                    idx += 1;
                    replacement.push(out_line);
                    cur_patch_line += 1;
                    li += 1;
                    if !has_nl {
                        cur_patch_line += 1;
                        li += 1;
                    }
                }
                HunkLine::Remove(s) => {
                    eof_new_blank = 0;
                    let has_nl = !matches!(hunk.lines.get(li + 1), Some(HunkLine::NoNewline));
                    let check_side = if forward {
                        false
                    } else {
                        ws_mode.ws_cli != WsCliMode::NoWarn
                    };
                    if check_side {
                        let body = patch_line_body_for_ws(s, has_nl);
                        let flags = ws::ws_check(&body, fp.ws_rule);
                        if !has_nl && (flags & WS_INCOMPLETE_LINE) != 0 {
                            pending_incomplete_body = Some(s.clone());
                            let rest = flags & !WS_INCOMPLETE_LINE;
                            record_ws_error(
                                &ws_mode,
                                &mut ws_errors,
                                rest,
                                patch_input_display,
                                cur_patch_line,
                                None,
                            );
                        } else {
                            pending_incomplete_body = None;
                            record_ws_error(
                                &ws_mode,
                                &mut ws_errors,
                                flags,
                                patch_input_display,
                                cur_patch_line,
                                None,
                            );
                        }
                    } else if !has_nl {
                        pending_incomplete_body = None;
                    }
                    if idx >= lines.len()
                        || !patch_line_matches_actual(s, &lines[idx], has_nl, ws_mode, fp)
                    {
                        failed = true;
                        break;
                    }
                    idx += 1;
                    if !has_nl {
                        if let Some(body) = pending_incomplete_body.take() {
                            if ws_mode.ws_cli != WsCliMode::NoWarn {
                                record_ws_error(
                                    &ws_mode,
                                    &mut ws_errors,
                                    WS_INCOMPLETE_LINE,
                                    patch_input_display,
                                    cur_patch_line + 1,
                                    Some(body.as_str()),
                                );
                            }
                        }
                        cur_patch_line += 2;
                        li += 2;
                    } else {
                        cur_patch_line += 1;
                        li += 1;
                    }
                }
                HunkLine::Add(s) => {
                    let has_nl = !matches!(hunk.lines.get(li + 1), Some(HunkLine::NoNewline));
                    let body = patch_line_body_for_ws(s, has_nl);
                    let plen = if has_nl {
                        body.len().saturating_sub(1)
                    } else {
                        body.len()
                    };
                    let added_blank =
                        (fp.ws_rule & WS_BLANK_AT_EOF) != 0 && ws::ws_blank_line(&body[..plen]);
                    if added_blank {
                        if eof_new_blank == 0 {
                            eof_found_line = cur_patch_line;
                        }
                        eof_new_blank += 1;
                    } else {
                        eof_new_blank = 0;
                    }
                    let check_side = if forward {
                        ws_mode.ws_cli != WsCliMode::NoWarn
                    } else {
                        false
                    };
                    if check_side {
                        let flags = ws::ws_check(&body, fp.ws_rule);
                        if !has_nl && (flags & WS_INCOMPLETE_LINE) != 0 {
                            pending_incomplete_body = Some(s.clone());
                            let rest = flags & !WS_INCOMPLETE_LINE;
                            record_ws_error(
                                &ws_mode,
                                &mut ws_errors,
                                rest,
                                patch_input_display,
                                cur_patch_line,
                                None,
                            );
                        } else {
                            pending_incomplete_body = None;
                            record_ws_error(
                                &ws_mode,
                                &mut ws_errors,
                                flags,
                                patch_input_display,
                                cur_patch_line,
                                None,
                            );
                        }
                    } else if !has_nl {
                        pending_incomplete_body = None;
                    }
                    let out_line = if ws_mode.whitespace_fix {
                        let body_in = patch_line_body_for_ws(s, has_nl);
                        let (fixed, _) = ws::ws_fix_copy_line(&body_in, fp.ws_rule);
                        fixed.trim_end_matches('\n').to_string()
                    } else {
                        s.clone()
                    };
                    replacement.push(out_line);
                    if !has_nl {
                        if let Some(body) = pending_incomplete_body.take() {
                            if ws_mode.ws_cli != WsCliMode::NoWarn {
                                record_ws_error(
                                    &ws_mode,
                                    &mut ws_errors,
                                    WS_INCOMPLETE_LINE,
                                    patch_input_display,
                                    cur_patch_line + 1,
                                    Some(body.as_str()),
                                );
                            }
                        }
                        cur_patch_line += 2;
                        li += 2;
                    } else {
                        cur_patch_line += 1;
                        li += 1;
                    }
                }
                HunkLine::NoNewline => {
                    cur_patch_line += 1;
                    li += 1;
                }
            }
        }

        if failed {
            rejected.push(hunk.clone());
            continue;
        }

        if idx == lines.len()
            && eof_new_blank > 0
            && (fp.ws_rule & WS_BLANK_AT_EOF) != 0
            && ws_mode.ws_cli != WsCliMode::NoWarn
        {
            record_ws_error(
                &ws_mode,
                &mut ws_errors,
                WS_BLANK_AT_EOF,
                patch_input_display,
                eof_found_line,
                None,
            );
            if ws_mode.whitespace_fix {
                for _ in 0..eof_new_blank {
                    if replacement.last().is_some_and(|l| ws::ws_blank_line(l)) {
                        replacement.pop();
                    }
                }
            }
        }

        let removed_count = idx.saturating_sub(actual_start);
        lines.splice(actual_start..idx, replacement.iter().cloned());
        offset += replacement.len() as isize - removed_count as isize;
    }

    if ws_errors > 0 && matches!(ws_mode.ws_cli, WsCliMode::Error) {
        bail!("{} line adds whitespace errors.", ws_errors);
    }

    if lines.is_empty() {
        return Ok((String::new(), rejected));
    }

    let mut out = lines.join("\n");
    if has_trailing_newline && !ws_mode.inaccurate_eof {
        out.push('\n');
    }
    Ok((out, rejected))
}

fn render_hunk_line(line: &HunkLine) -> String {
    match line {
        HunkLine::Context(s) => format!(" {s}"),
        HunkLine::Add(s) => format!("+{s}"),
        HunkLine::Remove(s) => format!("-{s}"),
        HunkLine::NoNewline => "\\ No newline at end of file".to_string(),
    }
}

fn write_reject_file(path: &Path, patch: &FilePatch, rejected_hunks: &[Hunk]) -> Result<()> {
    if rejected_hunks.is_empty() {
        return Ok(());
    }

    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() && !parent.exists() {
            fs::create_dir_all(parent)?;
        }
    }

    let old_hdr = patch.old_path.as_deref().unwrap_or("/dev/null");
    let new_hdr = patch.new_path.as_deref().unwrap_or("/dev/null");

    let mut out = String::new();
    out.push_str(&format!("--- {old_hdr}\n"));
    out.push_str(&format!("+++ {new_hdr}\n"));

    for hunk in rejected_hunks {
        out.push_str(&format!(
            "@@ -{},{} +{},{} @@\n",
            hunk.old_start, hunk.old_count, hunk.new_start, hunk.new_count
        ));
        for line in &hunk.lines {
            out.push_str(&render_hunk_line(line));
            out.push('\n');
        }
    }

    fs::write(path, out.as_bytes()).with_context(|| format!("failed to write {}", path.display()))
}

// ---------------------------------------------------------------------------
// Stat / numstat / summary output
// ---------------------------------------------------------------------------

fn apply_quote_path_fully() -> bool {
    Repository::discover(None)
        .ok()
        .and_then(|r| ConfigSet::load(Some(&r.git_dir), true).ok())
        .map(|c| c.quote_path_fully())
        .unwrap_or(true)
}

fn show_stat(patches: &[FilePatch], directory: Option<&str>, quote_fully: bool) {
    let stdout = io::stdout();
    let mut out = stdout.lock();

    let mut total_add = 0usize;
    let mut total_del = 0usize;
    let mut max_path_len = 0usize;
    let mut entries: Vec<(String, usize, usize, bool)> = Vec::new();

    for fp in patches {
        let path = fp
            .effective_path()
            .map(|p| adjust_path(p, directory))
            .unwrap_or_else(|| "(unknown)".to_string());
        let display = quote_c_style(&path, quote_fully);
        let binary = fp.is_binary;
        let (add, del) = if binary {
            (0, 0)
        } else {
            count_hunk_changes(&fp.hunks)
        };
        if display.len() > max_path_len {
            max_path_len = display.len();
        }
        total_add += add;
        total_del += del;
        entries.push((display, add, del, binary));
    }

    for (path, add, del, binary) in &entries {
        if *binary {
            // Git: binary changes show "Bin" instead of line counts.
            let _ = writeln!(out, " {path:<width$} |  Bin", width = max_path_len);
            continue;
        }
        let total = add + del;
        let bar = make_stat_bar(*add, *del, 40);
        let _ = writeln!(
            out,
            " {path:<width$} | {total:>4} {bar}",
            width = max_path_len
        );
    }
    let n = entries.len();
    let _ = writeln!(
        out,
        " {n} file{s} changed, {total_add} insertion{si}(+), {total_del} deletion{sd}(-)",
        s = if n == 1 { "" } else { "s" },
        si = if total_add == 1 { "" } else { "s" },
        sd = if total_del == 1 { "" } else { "s" },
    );
}

fn show_numstat(patches: &[FilePatch], directory: Option<&str>, quote_fully: bool) {
    let stdout = io::stdout();
    let mut out = stdout.lock();

    for fp in patches {
        let path = fp
            .effective_path()
            .map(|p| adjust_path(p, directory))
            .unwrap_or_else(|| "(unknown)".to_string());
        let display = quote_c_style(&path, quote_fully);
        if fp.is_binary {
            // Git: binary changes show "-" for both counts.
            let _ = writeln!(out, "-\t-\t{display}");
            continue;
        }
        let (add, del) = count_hunk_changes(&fp.hunks);
        let _ = writeln!(out, "{add}\t{del}\t{display}");
    }
}

fn show_summary(patches: &[FilePatch], directory: Option<&str>) {
    let stdout = io::stdout();
    let mut out = stdout.lock();

    for fp in patches {
        let path = fp
            .effective_path()
            .map(|p| adjust_path(p, directory))
            .unwrap_or_else(|| "(unknown)".to_string());

        if fp.is_new {
            let mode = fp.new_mode.as_deref().unwrap_or("100644");
            let _ = writeln!(out, " create mode {mode} {path}");
        } else if fp.is_deleted {
            let mode = fp.old_mode.as_deref().unwrap_or("100644");
            let _ = writeln!(out, " delete mode {mode} {path}");
        } else if fp.is_rename || fp.is_copy {
            let old = fp
                .old_path
                .as_deref()
                .map(|p| adjust_path(p, directory))
                .unwrap_or_else(|| "(unknown)".to_string());
            let kind = if fp.is_copy { "copy" } else { "rename" };
            let pct = fp.similarity_index.unwrap_or(100);
            let compact = compact_rename_path(&old, &path);
            let _ = writeln!(out, " {kind} {compact} ({pct}%)");
        } else if let Some(pct) = fp.dissimilarity_index {
            let _ = writeln!(out, " rewrite {path} ({pct}%)");
        } else if fp.old_mode.is_some() && fp.new_mode.is_some() && fp.old_mode != fp.new_mode {
            let _ = writeln!(
                out,
                " mode change {} => {} {path}",
                fp.old_mode.as_deref().unwrap_or(""),
                fp.new_mode.as_deref().unwrap_or("")
            );
        }
    }
}

fn count_hunk_changes(hunks: &[Hunk]) -> (usize, usize) {
    let mut add = 0;
    let mut del = 0;
    for hunk in hunks {
        for hl in &hunk.lines {
            match hl {
                HunkLine::Add(_) => add += 1,
                HunkLine::Remove(_) => del += 1,
                _ => {}
            }
        }
    }
    (add, del)
}

fn make_stat_bar(add: usize, del: usize, max_width: usize) -> String {
    let total = add + del;
    if total == 0 {
        return String::new();
    }
    let width = total.min(max_width);
    let plus_width = if total <= max_width {
        add
    } else {
        (add as f64 / total as f64 * max_width as f64).round() as usize
    };
    let minus_width = width - plus_width;
    format!("{}{}", "+".repeat(plus_width), "-".repeat(minus_width))
}

// ---------------------------------------------------------------------------
// Main run
// ---------------------------------------------------------------------------

pub fn run(mut args: Args) -> Result<()> {
    if let Some(dir) = args.directory.take() {
        let norm = normalize_apply_directory(&dir)?;
        args.directory = if norm.is_empty() { None } else { Some(norm) };
    }

    // Validate repository format if operating on the index or doing a check that requires it
    if args.cached || args.index || args.check {
        if let Some(git_dir) = crate::commands::config::resolve_git_dir_pub() {
            if let Err(e) = grit_lib::repo::validate_repo_format(&git_dir) {
                bail!("{}", e);
            }
        } else if std::path::Path::new(".git").is_dir() {
            if let Err(e) = grit_lib::repo::validate_repo_format(std::path::Path::new(".git")) {
                bail!("{}", e);
            }
        }
    }

    // Read patch input
    let input = if args.patches.is_empty() {
        let mut buf = String::new();
        io::stdin()
            .read_to_string(&mut buf)
            .context("failed to read patch from stdin")?;
        buf
    } else {
        let mut buf = String::new();
        for path in &args.patches {
            let content = if path.as_os_str() == "-" {
                let mut s = String::new();
                io::stdin()
                    .read_to_string(&mut s)
                    .context("failed to read patch from stdin")?;
                s
            } else {
                fs::read_to_string(path)
                    .with_context(|| format!("cannot read {}", path.display()))?
            };
            buf.push_str(&content);
            if !content.ends_with('\n') {
                buf.push('\n');
            }
        }
        buf
    };

    let patch_input_display = if args.patches.len() == 1 && args.patches[0].as_os_str() != "-" {
        args.patches[0].to_string_lossy().into_owned()
    } else {
        "<stdin>".to_string()
    };

    let setup_prefix = apply_setup_prefix();
    let mut patches = parse_patch(
        &input,
        args.strip,
        &patch_input_display,
        args.recount,
        (!setup_prefix.is_empty()).then_some(setup_prefix.as_str()),
    )?;
    normalize_mismatched_diff_git_paths(&mut patches, args.strip);
    postprocess_gitlink_file_patches(&mut patches);
    merge_adjacent_blob_to_gitlink_patches(&mut patches);
    validate_patch_headers(&patches, args.strip)?;

    if args.reverse {
        reverse_patches(&mut patches);
    }
    assign_ws_rules(&mut patches, &args);
    if patches.is_empty() && !args.allow_empty {
        bail!("No valid patches in input");
    }

    validate_and_canonicalize_patch_modes(&mut patches, &patch_input_display)?;

    // Info-only modes unless explicitly overridden by --apply.
    let info_only = (args.stat || args.numstat || args.summary) && !args.apply;
    let quote_fully = if args.stat || args.numstat {
        apply_quote_path_fully()
    } else {
        true
    };
    if args.stat {
        show_stat(&patches, args.directory.as_deref(), quote_fully);
    }
    if args.numstat {
        show_numstat(&patches, args.directory.as_deref(), quote_fully);
    }
    if args.summary {
        show_summary(&patches, args.directory.as_deref());
    }
    if info_only {
        return Ok(());
    }

    if let Some(path) = &args.build_fake_ancestor {
        build_fake_ancestor_file(&patches, &args, path)?;
        return Ok(());
    }
    verify_patch_paths_not_beyond_symlink(&patches, &args)?;
    let ws_mode = resolve_apply_whitespace_mode(&args);

    if args.three_way && args.reject {
        bail!("options '--reject' and '--3way' cannot be used together");
    }
    if args.three_way && Repository::discover(None).is_err() {
        bail!("'--3way' outside a repository");
    }
    let pre_expanded_sparse_paths = sparse_index_patch_paths(&patches, &args);
    maybe_emit_apply_sparse_index_trace(&patches, &args);
    prepare_patch_modes_for_apply(&mut patches, &args)?;

    // For --cached, we need a repository and index.
    // For working tree apply, we may or may not be in a repo.
    if args.cached {
        apply_to_index(&patches, &args, ws_mode, &patch_input_display)?;
    } else {
        if args.check {
            check_patches(&patches, &args, ws_mode, &patch_input_display)?;
            if !args.apply {
                return Ok(());
            }
        }

        if args.index {
            if let Ok(repo) = Repository::discover(None) {
                if let Some(wt) = repo.work_tree.as_deref() {
                    let index = repo.load_index().unwrap_or_else(|_| Index::new());
                    for fp in &patches {
                        verify_worktree_matches_index_for_patch(fp, &args)?;
                        if fp.involves_gitlink() {
                            verify_worktree_gitlink_patch(fp, &args, wt, &index)?;
                        }
                    }
                }
            } else {
                for fp in &patches {
                    verify_worktree_matches_index_for_patch(fp, &args)?;
                }
            }
            apply_to_worktree(
                &patches,
                &args,
                ws_mode,
                &patch_input_display,
                &pre_expanded_sparse_paths,
            )?;
            apply_to_index(&patches, &args, ws_mode, &patch_input_display)?;
            ensure_gitlink_placeholder_dirs(&patches, &args)?;
        } else {
            apply_to_worktree(&patches, &args, ws_mode, &patch_input_display, &[])?;
            if args.intent_to_add {
                apply_intent_to_add_entries(&patches, &args)?;
            }
        }
    }

    if args.three_way && !args.cached {
        if let Ok(repo) = Repository::discover(None) {
            let config = ConfigSet::load(Some(&repo.git_dir), true).unwrap_or_default();
            if rerere_enabled(&config, &repo.git_dir) {
                let _ = repo_rerere(&repo, RerereAutoupdate::FromConfig);
            }
        }
    }

    Ok(())
}

/// Build a temporary index file containing original blob versions referenced by the patch.
///
/// This implements `git apply --build-fake-ancestor=<file>` behavior.
fn build_fake_ancestor_file(patches: &[FilePatch], args: &Args, out_path: &Path) -> Result<()> {
    let repo = Repository::discover(None).context("not a git repository")?;
    let current_index = repo.load_index().unwrap_or_else(|_| Index::new());
    let mut fake = Index::new();

    for fp in patches {
        let Some(raw_old_path) = fp.old_path.as_deref() else {
            continue;
        };
        if raw_old_path == "/dev/null" {
            continue;
        }
        let adjusted = adjust_path(raw_old_path, args.directory.as_deref());
        if adjusted.is_empty() {
            continue;
        }

        let resolved = if let Some(old_oid) = fp.old_oid.as_deref() {
            let oid = resolve_revision_for_patch_old_blob(&repo, old_oid)
                .with_context(|| format!("resolving old blob id `{old_oid}` for `{adjusted}`"))?;
            let mode = fp.old_mode.as_deref().map(parse_mode).unwrap_or(0o100644);
            Some((mode, oid))
        } else {
            current_index
                .get(adjusted.as_bytes(), 0)
                .map(|entry| (entry.mode, entry.oid))
        };

        let Some((mode, oid)) = resolved else {
            continue;
        };

        let entry = IndexEntry {
            ctime_sec: 0,
            ctime_nsec: 0,
            mtime_sec: 0,
            mtime_nsec: 0,
            dev: 0,
            ino: 0,
            mode,
            uid: 0,
            gid: 0,
            size: 0,
            oid,
            flags: ((adjusted.len().min(0xFFF)) as u16) & 0x0FFF,
            flags_extended: None,
            path: adjusted.into_bytes(),
            base_index_pos: 0,
        };
        fake.add_or_replace(entry);
    }

    if let Some(parent) = out_path.parent() {
        if !parent.as_os_str().is_empty() && !parent.exists() {
            fs::create_dir_all(parent)
                .with_context(|| format!("failed to create {}", parent.display()))?;
        }
    }
    fake.write(out_path)
        .with_context(|| format!("writing {}", out_path.display()))?;
    Ok(())
}

/// Repository + index state for CRLF-aware apply (clean/smudge around unified diffs).
struct ApplyCrlfContext {
    repo: Repository,
    work_tree: PathBuf,
    index: Index,
    config: ConfigSet,
    conv: crlf::ConversionConfig,
}

impl ApplyCrlfContext {
    fn load() -> Option<Self> {
        let repo = Repository::discover(None).ok()?;
        let work_tree = repo.work_tree.clone()?;
        let index = repo.load_index().unwrap_or_else(|_| Index::new());
        let config = ConfigSet::load(Some(&repo.git_dir), true).unwrap_or_default();
        let conv = crlf::ConversionConfig::from_config(&config);
        Some(Self {
            repo,
            work_tree,
            index,
            config,
            conv,
        })
    }

    /// Bytes that hash to the same blob OID as `git add` / the index (clean direction).
    fn blob_for_index_hash(&self, path: &Path, rel_path: &str, mode: u32) -> Result<Vec<u8>> {
        if mode == grit_lib::index::MODE_SYMLINK {
            return read_symlink_target_bytes(path);
        }
        let raw = fs::read(path).with_context(|| format!("failed to read {}", path.display()))?;
        let rules = crlf::load_gitattributes_for_checkout(
            &self.work_tree,
            rel_path,
            &self.index,
            &self.repo.odb,
        );
        let file_attrs = crlf::get_file_attrs(&rules, rel_path, false, &self.config);
        crlf::convert_to_git(&raw, rel_path, &self.conv, &file_attrs)
            .map_err(|e| anyhow::anyhow!("{e}"))
    }

    /// Normalized LF text for unified-diff hunk matching (same pipeline as index blobs).
    fn normalized_text(&self, path: &Path, rel_path: &str) -> Result<String> {
        let meta = fs::symlink_metadata(path)?;
        if meta.file_type().is_symlink() {
            let b = read_symlink_target_bytes(path)?;
            return Ok(String::from_utf8_lossy(&b).into_owned());
        }
        let raw = fs::read(path).with_context(|| format!("failed to read {}", path.display()))?;
        let rules = crlf::load_gitattributes_for_checkout(
            &self.work_tree,
            rel_path,
            &self.index,
            &self.repo.odb,
        );
        let file_attrs = crlf::get_file_attrs(&rules, rel_path, false, &self.config);
        let normalized = crlf::convert_to_git(&raw, rel_path, &self.conv, &file_attrs)
            .map_err(|e| anyhow::anyhow!("{e}"))?;
        Ok(String::from_utf8_lossy(&normalized).into_owned())
    }
}

fn file_attrs_for_apply_path(ctx: &ApplyCrlfContext, rel_path: &str) -> FileAttrs {
    let rules =
        crlf::load_gitattributes_for_checkout(&ctx.work_tree, rel_path, &ctx.index, &ctx.repo.odb);
    crlf::get_file_attrs(&rules, rel_path, false, &ctx.config)
}

fn file_attrs_for_index_apply(repo: &Repository, index: &Index, rel_path: &str) -> FileAttrs {
    let Ok(config) = ConfigSet::load(Some(&repo.git_dir), true) else {
        return FileAttrs::default();
    };
    let Some(wt) = repo.work_tree.as_ref() else {
        return FileAttrs::default();
    };
    let rules = crlf::load_gitattributes_for_checkout(wt, rel_path, index, &repo.odb);
    crlf::get_file_attrs(&rules, rel_path, false, &config)
}

/// Submodule/gitlink patches: ensure the work tree state matches what `--index` apply expects.
fn verify_worktree_gitlink_patch(
    fp: &FilePatch,
    args: &Args,
    work_tree: &Path,
    index: &Index,
) -> Result<()> {
    if fp.is_deleted && fp.old_mode.as_deref() == Some("160000") {
        return Ok(());
    }
    let blob_to_gitlink = fp.new_mode.as_deref() == Some("160000")
        && fp.old_mode.as_deref() != Some("160000")
        && !fp.is_deleted;
    if (fp.is_new && fp.new_mode.as_deref() == Some("160000")) || blob_to_gitlink {
        let Some(target) = fp.target_path() else {
            return Ok(());
        };
        let adjusted = adjust_path(target, args.directory.as_deref());
        let path = work_tree.join(&adjusted);
        if path.exists() && path.is_file() {
            // Replacing a tracked regular file (or symlink) with a submodule: the work tree must
            // still match the index entry for that path (`replace_sub1_with_file` → submodule).
            let Some(entry) = index.get(adjusted.as_bytes(), 0) else {
                bail!("{}: already exists", adjusted);
            };
            if entry.mode == grit_lib::index::MODE_GITLINK {
                bail!("{}: already exists", adjusted);
            }
            let wt_oid = if let Some(ctx) = ApplyCrlfContext::load() {
                let bytes = ctx.blob_for_index_hash(&path, &adjusted, entry.mode)?;
                grit_lib::odb::Odb::hash_object_data(ObjectKind::Blob, &bytes)
            } else if entry.mode == grit_lib::index::MODE_SYMLINK {
                let b = read_symlink_target_bytes(&path)?;
                grit_lib::odb::Odb::hash_object_data(ObjectKind::Blob, &b)
            } else {
                let raw = fs::read(&path)
                    .with_context(|| format!("failed to read {}", path.display()))?;
                grit_lib::odb::Odb::hash_object_data(ObjectKind::Blob, &raw)
            };
            if wt_oid != entry.oid {
                bail!("{adjusted}: does not match index");
            }
            return Ok(());
        }
        if path.exists() && !path.is_dir() {
            bail!("{}: already exists", adjusted);
        }
        return Ok(());
    }
    if fp.old_mode.as_deref() == Some("160000") && !fp.is_deleted {
        let Some(source) = fp.source_path() else {
            return Ok(());
        };
        let adjusted = adjust_path(source, args.directory.as_deref());
        let path = work_tree.join(&adjusted);
        if !path.exists() {
            bail!("{}: does not exist", adjusted);
        }
        if !path.is_dir() {
            bail!("{}: does not match index", adjusted);
        }
    }
    Ok(())
}

/// After `--index` apply, nested file patches may have removed an empty gitlink placeholder
/// directory via `remove_empty_dirs_up`; recreate empty dirs for new/changed gitlinks.
fn ensure_gitlink_placeholder_dirs(patches: &[FilePatch], args: &Args) -> Result<()> {
    let work_tree = apply_work_tree_root();
    let work_tree_ref = work_tree.as_deref();
    for fp in patches {
        let target_is_gitlink = fp.new_mode.as_deref() == Some("160000") && !fp.is_deleted;
        let need_empty_submodule_dir =
            target_is_gitlink && (fp.is_new || fp.old_mode.as_deref() != Some("160000"));
        if !need_empty_submodule_dir {
            continue;
        }
        let Some(path_str) = fp.target_path() else {
            continue;
        };
        let adjusted = adjust_path(path_str, args.directory.as_deref());
        let path = apply_fs_path(&adjusted, work_tree_ref);
        if path.is_dir() {
            continue;
        }
        if path.is_file() || path.is_symlink() {
            fs::remove_file(&path).with_context(|| {
                format!("failed to remove file at submodule path {}", path.display())
            })?;
        }
        if !path.exists() {
            fs::create_dir_all(&path)
                .with_context(|| format!("failed to create {}", path.display()))?;
        }
    }
    Ok(())
}

/// Verify the working tree matches the index for paths touched by one patch (`git apply --index`).
///
/// Git checks each patch in sequence; unrelated dirty files must not abort the whole apply
/// (`t4108-apply-threeway` dirty working tree case).
fn verify_worktree_matches_index_for_patch(fp: &FilePatch, args: &Args) -> Result<()> {
    let Some(ctx) = ApplyCrlfContext::load() else {
        return Ok(());
    };

    if fp.is_new {
        if let Some(target) = fp.target_path() {
            let adjusted = adjust_path(target, args.directory.as_deref());
            if let Some(entry) = ctx.index.get(adjusted.as_bytes(), 0) {
                let path = ctx.work_tree.join(&adjusted);
                if !path.exists() {
                    bail!("{adjusted}: does not match index");
                }
                let wt_content = ctx.blob_for_index_hash(&path, &adjusted, entry.mode)?;
                let wt_oid = grit_lib::odb::Odb::hash_object_data(ObjectKind::Blob, &wt_content);
                if wt_oid != entry.oid {
                    bail!("{adjusted}: does not match index");
                }
            }
        }
        return Ok(());
    }
    // Skip submodule-only patches; file→submodule transitions keep a blob preimage on disk.
    if fp.old_mode.as_deref() == Some("160000")
        || (fp.new_mode.as_deref() == Some("160000") && fp.old_mode.as_deref() == Some("160000"))
    {
        return Ok(());
    }
    let path_str = fp
        .source_path()
        .ok_or_else(|| anyhow::anyhow!("patch has no file path"))?;
    let adjusted = adjust_path(path_str, args.directory.as_deref());
    let path = ctx.work_tree.join(&adjusted);

    if !path.exists() {
        return Ok(());
    }

    if let Some(entry) = ctx.index.get(adjusted.as_bytes(), 0) {
        let wt_content = ctx.blob_for_index_hash(&path, &adjusted, entry.mode)?;
        let wt_oid = grit_lib::odb::Odb::hash_object_data(ObjectKind::Blob, &wt_content);
        if wt_oid != entry.oid {
            bail!("{adjusted}: does not match index");
        }
        if let Some(ref expected_oid) = fp.old_oid {
            let index_hex = entry.oid.to_hex();
            if !index_hex.starts_with(expected_oid.as_str()) {
                bail!("{adjusted}: does not match index");
            }
        }
    }

    Ok(())
}

fn read_symlink_target_bytes(path: &Path) -> Result<Vec<u8>> {
    let target = fs::read_link(path)
        .with_context(|| format!("failed to read symlink target {}", path.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStrExt;
        Ok(target.as_os_str().as_bytes().to_vec())
    }
    #[cfg(not(unix))]
    {
        Ok(target.to_string_lossy().into_owned().into_bytes())
    }
}

fn read_worktree_blob_as_text(path: &Path) -> io::Result<String> {
    let metadata = fs::symlink_metadata(path)?;
    if metadata.file_type().is_symlink() {
        let target =
            read_symlink_target_bytes(path).map_err(|err| io::Error::other(err.to_string()))?;
        return Ok(String::from_utf8_lossy(&target).into_owned());
    }
    let bytes = fs::read(path)?;
    Ok(String::from_utf8_lossy(&bytes).into_owned())
}

/// Remove empty directories upward from path.
fn remove_empty_dirs_up(dir: &Path) {
    let cwd = std::env::current_dir().ok();
    let mut current = dir.to_path_buf();
    while current.as_os_str() != "." && !current.as_os_str().is_empty() {
        if cwd
            .as_ref()
            .is_some_and(|cwd| cwd == &current || cwd.starts_with(&current))
        {
            break;
        }
        if fs::remove_dir(&current).is_err() {
            break;
        }
        match current.parent() {
            Some(parent) => current = parent.to_path_buf(),
            None => break,
        }
    }
}

fn remove_path_for_replacement(path: &Path) -> Result<()> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(meta) => meta,
        Err(err)
            if matches!(
                err.kind(),
                std::io::ErrorKind::NotFound | std::io::ErrorKind::NotADirectory
            ) =>
        {
            return Ok(());
        }
        Err(err) => return Err(err).with_context(|| format!("failed to stat {}", path.display())),
    };
    if metadata.file_type().is_dir() && !metadata.file_type().is_symlink() {
        fs::remove_dir_all(path)
            .with_context(|| format!("failed to remove directory {}", path.display()))?;
    } else {
        fs::remove_file(path).with_context(|| format!("failed to remove {}", path.display()))?;
    }
    Ok(())
}

fn target_is_inside_source(target: &Path, source: &Path) -> bool {
    target
        .strip_prefix(source)
        .map(|suffix| !suffix.as_os_str().is_empty())
        .unwrap_or(false)
}

fn write_worktree_path(
    path: &Path,
    content: &str,
    mode: Option<&str>,
    source_exec_bit: Option<bool>,
    crlf_ctx: Option<&ApplyCrlfContext>,
    rel_path: &str,
) -> Result<()> {
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() && !parent.exists() {
            fs::create_dir_all(parent)?;
        }
    }

    let raw_on_disk_had_crlf = path.is_file()
        && fs::read(path)
            .ok()
            .is_some_and(|raw| raw.windows(2).any(|w| w == *b"\r\n"));

    if mode == Some("120000") {
        remove_path_for_replacement(path)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::symlink;
            symlink(content, path)
                .with_context(|| format!("failed to create symlink {}", path.display()))?;
            return Ok(());
        }
        #[cfg(not(unix))]
        {
            fs::write(path, content.as_bytes())
                .with_context(|| format!("failed to write {}", path.display()))?;
            return Ok(());
        }
    }

    if path.is_dir() {
        fs::remove_dir_all(path)
            .with_context(|| format!("failed to remove directory {}", path.display()))?;
    }

    let bytes: Cow<'_, [u8]> = if let Some(ctx) = crlf_ctx {
        let rules = crlf::load_gitattributes_for_checkout(
            &ctx.work_tree,
            rel_path,
            &ctx.index,
            &ctx.repo.odb,
        );
        let file_attrs = crlf::get_file_attrs(&rules, rel_path, false, &ctx.config);
        let mut out = crlf::convert_to_worktree_eager(
            content.as_bytes(),
            rel_path,
            &ctx.conv,
            &file_attrs,
            None,
            None,
        )
        .map_err(|e| anyhow::anyhow!("{e}"))?;
        if raw_on_disk_had_crlf
            && !crlf::should_convert_to_crlf(&ctx.conv, &file_attrs, content.as_bytes())
        {
            out = crlf::lf_to_crlf(&out);
        }
        Cow::Owned(out)
    } else {
        Cow::Borrowed(content.as_bytes())
    };
    fs::write(path, &bytes).with_context(|| format!("failed to write {}", path.display()))?;

    #[cfg(unix)]
    {
        if let Some(mode_str) = mode {
            if mode_str != "120000" {
                chmod_worktree_regular(path, parse_mode(mode_str))?;
            }
        } else if let Some(executable) = source_exec_bit {
            chmod_worktree_regular(
                path,
                if executable {
                    MODE_EXECUTABLE
                } else {
                    MODE_REGULAR
                },
            )?;
        }
    }

    Ok(())
}

#[cfg(unix)]
fn process_umask_bits() -> u32 {
    static UMASK_CACHE: OnceLock<u32> = OnceLock::new();
    *UMASK_CACHE.get_or_init(|| {
        #[cfg(target_os = "linux")]
        {
            if let Ok(status) = fs::read_to_string("/proc/self/status") {
                for line in status.lines() {
                    let line = line.trim_start();
                    if let Some(rest) = line.strip_prefix("Umask:") {
                        let tok = rest.trim();
                        if let Ok(v) = u32::from_str_radix(tok, 8) {
                            return v;
                        }
                    }
                }
            }
        }
        0o022
    })
}

/// Worktree permission bits for a regular file from index mode and current umask (Git checkout).
#[cfg(unix)]
fn worktree_mode_from_index_mode(index_mode: u32) -> u32 {
    let base = if index_mode == MODE_EXECUTABLE {
        0o777
    } else {
        0o666
    };
    base & !process_umask_bits()
}

#[cfg(unix)]
fn chmod_worktree_regular(path: &Path, index_mode: u32) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let perm_bits = if matches!(index_mode, MODE_REGULAR | MODE_EXECUTABLE) {
        worktree_mode_from_index_mode(index_mode)
    } else {
        index_mode & 0o777
    };
    fs::set_permissions(path, fs::Permissions::from_mode(perm_bits))?;
    Ok(())
}

fn verify_old_oid_matches_content(expected_oid: &str, content: &str) -> Result<()> {
    verify_old_oid_matches_bytes(expected_oid, content.as_bytes())
}

fn verify_old_oid_matches_bytes(expected_oid: &str, content: &[u8]) -> Result<()> {
    let actual_oid = grit_lib::odb::Odb::hash_object_data(ObjectKind::Blob, content);
    let actual_hex = actual_oid.to_hex();
    if !actual_hex.starts_with(expected_oid) {
        bail!("patch does not apply");
    }
    Ok(())
}

fn record_path_existence(
    path: &str,
    work_tree: Option<&Path>,
    current: &mut HashMap<String, bool>,
    initial: &mut HashMap<String, bool>,
) {
    if current.contains_key(path) {
        return;
    }
    let exists = apply_fs_path(path, work_tree).exists();
    current.insert(path.to_string(), exists);
    initial.insert(path.to_string(), exists);
}

/// Return true when a patch can apply to a missing preimage as empty content.
///
/// This matches hunks whose old side starts from line 0 with count 0 and
/// contains only additions.
fn can_apply_with_empty_preimage(fp: &FilePatch) -> bool {
    if fp.hunks.is_empty() {
        return false;
    }

    fp.hunks.iter().all(|hunk| {
        hunk.old_start == 0
            && hunk.old_count == 0
            && hunk
                .lines
                .iter()
                .all(|line| matches!(line, HunkLine::Add(_) | HunkLine::NoNewline))
    })
}

/// Preflight worktree patch ordering/path availability before writing.
///
/// This catches invalid sequences (e.g. later patches reading a path that was
/// moved away by an earlier rename) and prevents partially-applied worktree
/// state when such sequences are detected.
fn precheck_worktree_patch_sequence(
    patches: &[FilePatch],
    args: &Args,
    pre_expanded_sparse_paths: &[String],
) -> Result<()> {
    let work_tree = apply_work_tree_root();
    let work_tree_ref = work_tree.as_deref();
    let setup_prefix = apply_setup_prefix();
    let repo = Repository::discover(None).ok();
    let mut current_exists: HashMap<String, bool> = HashMap::new();
    let mut initial_exists: HashMap<String, bool> = HashMap::new();

    for fp in patches {
        if let Some(source) = fp.source_path() {
            let adjusted = adjust_path(source, args.directory.as_deref());
            let op = fp.worktree_rel_operational(&adjusted, &setup_prefix);
            record_path_existence(&op, work_tree_ref, &mut current_exists, &mut initial_exists);
        }
        if let Some(target) = fp.target_path() {
            let adjusted = adjust_path(target, args.directory.as_deref());
            let op = fp.worktree_rel_operational(&adjusted, &setup_prefix);
            record_path_existence(&op, work_tree_ref, &mut current_exists, &mut initial_exists);
        }
    }

    for fp in patches {
        let source_adjusted = fp
            .source_path()
            .map(|p| adjust_path(p, args.directory.as_deref()))
            .unwrap_or_default();
        let target_adjusted = fp
            .target_path()
            .map(|p| adjust_path(p, args.directory.as_deref()))
            .unwrap_or_default();
        let source_op = fp.worktree_rel_operational(&source_adjusted, &setup_prefix);
        let target_op = fp.worktree_rel_operational(&target_adjusted, &setup_prefix);

        let source_exists_now = current_exists
            .get(&source_op)
            .copied()
            .unwrap_or_else(|| apply_fs_path(&source_op, work_tree_ref).exists());
        let source_existed_initially = initial_exists
            .get(&source_op)
            .copied()
            .unwrap_or(source_exists_now);

        if fp.is_deleted {
            if !source_exists_now {
                if source_existed_initially {
                    continue;
                }
                bail!(
                    "failed to read {}: No such file or directory (os error 2)",
                    source_op
                );
            }
            set_path_and_descendants_state(&mut current_exists, &source_op, false);
            if source_op != target_op {
                set_path_and_descendants_state(&mut current_exists, &target_op, false);
            }
            continue;
        }

        if fp.is_new {
            let target_exists_now = current_exists
                .get(&target_op)
                .copied()
                .unwrap_or_else(|| apply_fs_path(&target_op, work_tree_ref).exists());
            if target_exists_now {
                if apply_fs_path(&target_op, work_tree_ref).is_dir() {
                    set_path_and_descendants_state(&mut current_exists, &target_op, false);
                } else if !args.three_way {
                    // With `--3way`, Git may emit multiple add/add hunks for the same path
                    // (`t4108-apply-threeway`); the real check happens during apply.
                    bail!("{target_op}: already exists");
                }
            }
            current_exists.insert(target_op.clone(), true);
            continue;
        }

        if !source_exists_now {
            let can_use_initial_snapshot =
                source_op != target_op && (fp.is_copy || fp.is_rename) && source_existed_initially;
            let source_was_sparse = args.index
                && (path_matches_sparse_patch_paths(&source_op, pre_expanded_sparse_paths)
                    || repo
                        .as_ref()
                        .is_some_and(|repo| path_outside_sparse_checkout(repo, &source_op)));
            if source_was_sparse {
                continue;
            }
            if !can_use_initial_snapshot && !can_apply_with_empty_preimage(fp) {
                bail!(
                    "failed to read {}: No such file or directory (os error 2)",
                    source_op
                );
            }
        }

        if fp.is_rename && source_op != target_op {
            set_path_and_descendants_state(&mut current_exists, &source_op, false);
        }
        current_exists.insert(target_op, true);
    }

    Ok(())
}

fn set_path_and_descendants_state(
    current_exists: &mut HashMap<String, bool>,
    path: &str,
    exists: bool,
) {
    current_exists.insert(path.to_string(), exists);
    let prefix = format!("{path}/");
    let affected: Vec<String> = current_exists
        .keys()
        .filter(|key| key.starts_with(&prefix))
        .cloned()
        .collect();
    for key in affected {
        current_exists.insert(key, exists);
    }
}

fn apply_to_worktree(
    patches: &[FilePatch],
    args: &Args,
    ws_mode: ApplyWhitespaceMode,
    patch_input_display: &str,
    pre_expanded_sparse_paths: &[String],
) -> Result<()> {
    let work_tree = apply_work_tree_root();
    let work_tree_ref = work_tree.as_deref();
    let setup_prefix = apply_setup_prefix();
    let raw_index = Repository::discover(None)
        .ok()
        .and_then(|repo| load_raw_index_for_sparse_detection(&repo).ok());
    let crlf_ctx = ApplyCrlfContext::load();
    let mut had_rejects = false;
    let mut apply_nonzero = false;
    let three_way_allowed = crlf_ctx.is_some();
    let conflict_style = crlf_ctx
        .as_ref()
        .map(|c| resolve_apply_conflict_style(&c.repo))
        .unwrap_or(ConflictStyle::Merge);
    let merge_favor_cli = merge_favor_from_apply_args(args);
    // Snapshot source-side file contents used by cross-path rename/copy patches
    // so later modifications/removals do not affect subsequent patch sections.
    let mut source_snapshots: HashMap<String, String> = HashMap::new();
    for fp in patches {
        let Some(source) = fp.source_path() else {
            continue;
        };
        let Some(target) = fp.target_path() else {
            continue;
        };
        let source_adjusted = adjust_path(source, args.directory.as_deref());
        let target_adjusted = adjust_path(target, args.directory.as_deref());
        let source_op = fp.worktree_rel_operational(&source_adjusted, &setup_prefix);
        let target_op = fp.worktree_rel_operational(&target_adjusted, &setup_prefix);
        if source_op == target_op || source_snapshots.contains_key(&source_op) {
            continue;
        }
        let snap_path = apply_fs_path(&source_op, work_tree_ref);
        let snap_ok = match &crlf_ctx {
            Some(ctx) => ctx.normalized_text(&snap_path, &source_op),
            None => read_worktree_blob_as_text(&snap_path).map_err(|e| anyhow::anyhow!("{e}")),
        };
        if let Ok(content) = snap_ok {
            source_snapshots.insert(source_op, content);
        }
    }
    precheck_worktree_patch_sequence(patches, args, pre_expanded_sparse_paths)?;

    for fp in patches {
        let path_str = fp
            .target_path()
            .ok_or_else(|| anyhow::anyhow!("patch has no file path"))?;
        let path_adjusted = adjust_path(path_str, args.directory.as_deref());
        let path_operational = fp.worktree_rel_operational(&path_adjusted, &setup_prefix);
        let path = apply_fs_path(&path_operational, work_tree_ref);

        if fp.is_deleted {
            // Delete the file (or directory for submodules)
            if path.is_dir() {
                fs::remove_dir_all(&path)
                    .with_context(|| format!("failed to remove directory {}", path.display()))?;
            } else if path.exists() {
                fs::remove_file(&path)
                    .with_context(|| format!("failed to remove {}", path.display()))?;
            }
            // Clean up empty parent directories
            if let Some(parent) = path.parent() {
                remove_empty_dirs_up(parent);
            }
            continue;
        }

        if fp.old_mode.as_deref() == Some("160000") && fp.new_mode.as_deref() == Some("160000") {
            continue;
        }

        if fp.new_mode.as_deref() == Some("160000")
            && fp.old_mode.as_deref() != Some("160000")
            && !fp.is_new
        {
            if path.is_file() || path.is_symlink() {
                fs::remove_file(&path).with_context(|| {
                    format!(
                        "failed to remove file before submodule dir {}",
                        path.display()
                    )
                })?;
            }
            fs::create_dir_all(&path)
                .with_context(|| format!("failed to create {}", path.display()))?;
            continue;
        }

        if fp.is_new {
            // Submodule: create directory
            if fp.new_mode.as_deref() == Some("160000") {
                if path.is_file() || path.is_symlink() {
                    fs::remove_file(&path).with_context(|| {
                        format!(
                            "failed to remove file before submodule dir {}",
                            path.display()
                        )
                    })?;
                }
                fs::create_dir_all(&path)?;
                continue;
            }
            let path_exists = path.exists();
            let add_add_threeway =
                args.three_way && path_exists && fp.old_path.as_deref() == Some("/dev/null");
            if add_add_threeway {
                let Some(ctx) = crlf_ctx.as_ref() else {
                    bail!("not a git repository");
                };
                let repo = &ctx.repo;
                let mode_for_read = ctx
                    .index
                    .get(path_adjusted.as_bytes(), 0)
                    .map(|e| e.mode)
                    .unwrap_or_else(|| parse_mode(fp.new_mode.as_deref().unwrap_or("100644")));
                let our_bytes = if path.is_file() || path.is_symlink() {
                    ctx.blob_for_index_hash(&path, &path_adjusted, mode_for_read)?
                } else {
                    bail!("cannot read the current contents of '{}'", path_str);
                };
                let file_attrs = file_attrs_for_apply_path(ctx, &path_adjusted);
                let favor = effective_merge_favor_for_path(&file_attrs, merge_favor_cli);
                if let Some(tw) = try_three_way_merge_blob(
                    repo,
                    fp,
                    &our_bytes,
                    ws_mode,
                    !args.reverse,
                    patch_input_display,
                    favor,
                    conflict_style,
                    &file_attrs,
                    true,
                )? {
                    if tw.conflicted {
                        apply_nonzero = true;
                    }
                    let merged_text = String::from_utf8_lossy(&tw.merged_bytes).into_owned();
                    write_worktree_path(
                        &path,
                        &merged_text,
                        fp.new_mode.as_deref(),
                        None,
                        crlf_ctx.as_ref(),
                        &path_adjusted,
                    )?;
                    continue;
                }
            }
            let content = apply_hunks("", fp, ws_mode, !args.reverse, patch_input_display)
                .with_context(|| {
                    format!("failed to apply hunks for new file {}", path.display())
                })?;
            write_worktree_path(
                &path,
                &content,
                fp.new_mode.as_deref(),
                None,
                crlf_ctx.as_ref(),
                &path_operational,
            )?;
            continue;
        }

        // Modify existing file — read preimage from source side (important
        // for rename/copy patches where target may not exist yet).
        let source_adjusted = fp
            .source_path()
            .map(|p| adjust_path(p, args.directory.as_deref()))
            .unwrap_or_else(|| path_adjusted.clone());
        let source_operational = fp.worktree_rel_operational(&source_adjusted, &setup_prefix);
        let read_path = apply_fs_path(&source_operational, work_tree_ref);
        let source_contains_target =
            fp.is_rename && read_path != path && target_is_inside_source(&path, &read_path);
        let source_was_sparse =
            path_is_sparse_in_indexes(
                crlf_ctx.as_ref().map(|ctx| &ctx.index),
                raw_index.as_ref(),
                &source_operational,
            ) || path_matches_sparse_patch_paths(&source_operational, pre_expanded_sparse_paths)
                || crlf_ctx.as_ref().is_some_and(|ctx| {
                    path_outside_sparse_checkout(&ctx.repo, &source_operational)
                });
        if args.index && !read_path.exists() && source_was_sparse {
            continue;
        }

        if fp.binary_patch.is_some() {
            let old_bytes: Vec<u8> = if source_adjusted != path_adjusted {
                if !read_path.exists() && can_apply_with_empty_preimage(fp) {
                    Vec::new()
                } else if read_path.symlink_metadata()?.file_type().is_symlink() {
                    read_symlink_target_bytes(&read_path)?
                } else {
                    fs::read(&read_path)
                        .with_context(|| format!("failed to read {}", read_path.display()))?
                }
            } else if !read_path.exists() && can_apply_with_empty_preimage(fp) {
                Vec::new()
            } else if read_path.symlink_metadata()?.file_type().is_symlink() {
                read_symlink_target_bytes(&read_path)?
            } else {
                fs::read(&read_path)
                    .with_context(|| format!("failed to read {}", read_path.display()))?
            };
            if !args.reject {
                if let Some(expected_oid) = fp.old_oid.as_deref() {
                    if !ws_mode.whitespace_fix && !args.three_way {
                        verify_old_oid_matches_bytes(expected_oid, &old_bytes)?;
                    }
                }
            }
            #[cfg(unix)]
            let source_exec_bit = if source_adjusted != path_adjusted {
                use std::os::unix::fs::PermissionsExt;
                fs::metadata(&read_path)
                    .ok()
                    .map(|meta| meta.permissions().mode() & 0o111 != 0)
            } else {
                None
            };
            let binary_patch = fp
                .binary_patch
                .as_ref()
                .ok_or_else(|| anyhow!("internal error: missing binary patch payload"))?;
            let new_bytes = inflate_binary_payload(&binary_patch.forward_compressed)?;
            if let Some(expected_new) = fp.new_oid.as_deref() {
                let actual = grit_lib::odb::Odb::hash_object_data(ObjectKind::Blob, &new_bytes);
                if !actual.to_hex().starts_with(expected_new) {
                    bail!("patch does not apply");
                }
            }
            if source_contains_target {
                remove_path_for_replacement(&read_path)?;
            }
            if let Some(parent) = path.parent() {
                if !parent.as_os_str().is_empty() && !parent.exists() {
                    fs::create_dir_all(parent)?;
                }
            }
            fs::write(&path, &new_bytes)
                .with_context(|| format!("failed to write {}", path.display()))?;

            #[cfg(unix)]
            {
                if let Some(mode) = fp.new_mode.as_deref() {
                    chmod_worktree_regular(&path, parse_mode(mode))?;
                } else if let Some(executable) = source_exec_bit {
                    chmod_worktree_regular(
                        &path,
                        if executable {
                            MODE_EXECUTABLE
                        } else {
                            MODE_REGULAR
                        },
                    )?;
                }
            }

            if fp.is_rename && read_path != path && !source_contains_target {
                remove_path_for_replacement(&read_path)?;
                if let Some(parent) = read_path.parent() {
                    remove_empty_dirs_up(parent);
                }
            }
            continue;
        }

        let load_old_content_from_disk = || -> Result<String> {
            match &crlf_ctx {
                Some(ctx) => {
                    if !read_path.exists() && can_apply_with_empty_preimage(fp) {
                        Ok(String::new())
                    } else {
                        ctx.normalized_text(&read_path, &source_operational)
                    }
                }
                None => match read_worktree_blob_as_text(&read_path) {
                    Ok(content) => Ok(content),
                    Err(err)
                        if err.kind() == std::io::ErrorKind::NotFound
                            && can_apply_with_empty_preimage(fp) =>
                    {
                        Ok(String::new())
                    }
                    Err(err) => {
                        Err(err).with_context(|| format!("failed to read {}", read_path.display()))
                    }
                },
            }
        };
        let old_content = if source_operational != path_operational {
            if let Some(snapshot) = source_snapshots.get(&source_operational) {
                snapshot.clone()
            } else {
                load_old_content_from_disk()?
            }
        } else {
            load_old_content_from_disk()?
        };
        // With `--whitespace=fix`, a prior apply can normalize trailing whitespace so the
        // worktree no longer hashes to the patch's `index` line preimage (see t4125). Git
        // matches hunks against file content instead; skip strict OID checks in that mode.
        if !args.reject {
            if let Some(expected_oid) = fp.old_oid.as_deref() {
                if !ws_mode.whitespace_fix && !args.three_way {
                    verify_old_oid_matches_content(expected_oid, &old_content)?;
                }
            }
        }
        #[cfg(unix)]
        let source_exec_bit = if source_operational != path_operational {
            use std::os::unix::fs::PermissionsExt;
            fs::metadata(&read_path)
                .ok()
                .map(|meta| meta.permissions().mode() & 0o111 != 0)
        } else {
            None
        };

        if fp.hunks.is_empty() {
            if source_operational != path_operational {
                if source_contains_target {
                    remove_path_for_replacement(&read_path)?;
                }
                write_worktree_path(
                    &path,
                    &old_content,
                    fp.new_mode.as_deref(),
                    source_exec_bit,
                    crlf_ctx.as_ref(),
                    &path_operational,
                )?;
                if fp.is_rename && read_path != path && !source_contains_target {
                    remove_path_for_replacement(&read_path)?;
                    if let Some(parent) = read_path.parent() {
                        remove_empty_dirs_up(parent);
                    }
                }
                continue;
            }

            #[cfg(unix)]
            if let Some(mode) = fp.new_mode.as_deref() {
                chmod_worktree_regular(&path, parse_mode(mode))?;
            }
            continue;
        }

        let (new_content, rejected_hunks) = if args.reject {
            apply_hunks_with_reject(
                &old_content,
                fp,
                ws_mode,
                !args.reverse,
                patch_input_display,
            )
            .with_context(|| format!("failed to apply patch to {}", path.display()))?
        } else if args.three_way && three_way_allowed {
            let ctx = crlf_ctx.as_ref().context("not a git repository")?;
            let repo = &ctx.repo;
            let file_attrs = file_attrs_for_apply_path(ctx, &path_adjusted);
            let favor = effective_merge_favor_for_path(&file_attrs, merge_favor_cli);
            if let Some(tw) = try_three_way_merge_blob(
                repo,
                fp,
                old_content.as_bytes(),
                ws_mode,
                !args.reverse,
                patch_input_display,
                favor,
                conflict_style,
                &file_attrs,
                false,
            )
            .with_context(|| format!("failed to apply patch to {}", path.display()))?
            {
                if tw.conflicted {
                    apply_nonzero = true;
                }
                (
                    String::from_utf8_lossy(&tw.merged_bytes).into_owned(),
                    Vec::new(),
                )
            } else {
                let content = apply_hunks(
                    &old_content,
                    fp,
                    ws_mode,
                    !args.reverse,
                    patch_input_display,
                )
                .with_context(|| format!("failed to apply patch to {}", path.display()))?;
                (content, Vec::new())
            }
        } else {
            let content = apply_hunks(
                &old_content,
                fp,
                ws_mode,
                !args.reverse,
                patch_input_display,
            )
            .with_context(|| format!("failed to apply patch to {}", path.display()))?;
            (content, Vec::new())
        };
        if source_contains_target {
            remove_path_for_replacement(&read_path)?;
        }
        write_worktree_path(
            &path,
            &new_content,
            fp.new_mode.as_deref(),
            source_exec_bit,
            crlf_ctx.as_ref(),
            &path_operational,
        )?;

        if !rejected_hunks.is_empty() {
            had_rejects = true;
            let reject_path = apply_fs_path(&format!("{path_operational}.rej"), work_tree_ref);
            write_reject_file(&reject_path, fp, &rejected_hunks)?;
        }

        if fp.is_rename && read_path != path && !source_contains_target {
            remove_path_for_replacement(&read_path)?;
            if let Some(parent) = read_path.parent() {
                remove_empty_dirs_up(parent);
            }
        }
    }

    if had_rejects {
        bail!("patch failed");
    }

    // With `--index`, the index is updated in a second phase (`apply_to_index`). Git still writes
    // unmerged entries even when the work tree contains conflict markers; do not exit early.
    if apply_nonzero && !args.index {
        return Err(anyhow::Error::new(ExplicitExit {
            code: 1,
            message: String::new(),
        }));
    }

    Ok(())
}

/// Apply patches to the index only (--cached).
fn apply_to_index(
    patches: &[FilePatch],
    args: &Args,
    ws_mode: ApplyWhitespaceMode,
    patch_input_display: &str,
) -> Result<()> {
    let repo = Repository::discover(None).context("not a git repository")?;
    let conflict_style = resolve_apply_conflict_style(&repo);
    let merge_favor_cli = merge_favor_from_apply_args(args);
    let mut apply_nonzero = false;
    let mut index = match repo.load_index() {
        Ok(idx) => idx,
        Err(_) => Index::new(),
    };
    let original_index = index.clone();
    // CWD prefix for subdir apply
    let cwd_prefix = if let Some(ref wt) = repo.work_tree {
        if let Ok(cwd) = std::env::current_dir() {
            if let Ok(rel) = cwd.strip_prefix(wt) {
                let s = rel.to_string_lossy().to_string();
                if s.is_empty() {
                    String::new()
                } else {
                    format!("{s}/")
                }
            } else {
                String::new()
            }
        } else {
            String::new()
        }
    } else {
        String::new()
    };

    for fp in patches {
        let target_path_str = fp
            .target_path()
            .ok_or_else(|| anyhow::anyhow!("patch has no file path"))?;
        let target_raw = adjust_path(target_path_str, args.directory.as_deref());
        let target_adjusted = format!("{cwd_prefix}{target_raw}");
        let source_adjusted = fp
            .source_path()
            .map(|p| adjust_path(p, args.directory.as_deref()))
            .map(|raw| format!("{cwd_prefix}{raw}"))
            .unwrap_or_else(|| target_adjusted.clone());

        if fp.is_deleted {
            index.remove(source_adjusted.as_bytes());
            continue;
        }

        if let Some(binary_patch) = fp.binary_patch.as_ref() {
            let source_index = if source_adjusted != target_adjusted {
                &original_index
            } else {
                &index
            };
            let Some(old_ent) = source_index.get(source_adjusted.as_bytes(), 0) else {
                bail!("{source_adjusted} not found in index");
            };
            if let Some(expected_oid) = fp.old_oid.as_deref() {
                if !ws_mode.whitespace_fix && !args.three_way {
                    let index_hex = old_ent.oid.to_hex();
                    if !index_hex.starts_with(expected_oid) {
                        bail!("patch does not apply");
                    }
                }
            }
            let new_bytes = inflate_binary_payload(&binary_patch.forward_compressed)?;
            if let Some(expected_new) = fp.new_oid.as_deref() {
                let actual = grit_lib::odb::Odb::hash_object_data(ObjectKind::Blob, &new_bytes);
                if !actual.to_hex().starts_with(expected_new) {
                    bail!("patch does not apply");
                }
            }
            let new_oid = repo.odb.write(ObjectKind::Blob, &new_bytes)?;

            let mode = if let Some(m) = fp.new_mode.as_deref() {
                parse_mode(m)
            } else if source_adjusted != target_adjusted {
                original_index
                    .get(source_adjusted.as_bytes(), 0)
                    .map(|entry| entry.mode)
                    .unwrap_or(0o100644)
            } else if let Some(entry) = index.get(source_adjusted.as_bytes(), 0) {
                entry.mode
            } else {
                0o100644
            };

            let entry = grit_lib::index::IndexEntry {
                ctime_sec: 0,
                ctime_nsec: 0,
                mtime_sec: 0,
                mtime_nsec: 0,
                dev: 0,
                ino: 0,
                mode,
                uid: 0,
                gid: 0,
                size: new_bytes.len() as u32,
                oid: new_oid,
                flags: ((target_adjusted.len().min(0xFFF)) as u16) & 0x0FFF,
                flags_extended: None,
                path: target_adjusted.clone().into_bytes(),
                base_index_pos: 0,
            };
            if fp.is_rename && source_adjusted != target_adjusted {
                index.remove(source_adjusted.as_bytes());
            }
            index.add_or_replace(entry);
            continue;
        }

        if fp.new_mode.as_deref() == Some("160000") && !fp.is_deleted {
            if fp.is_new {
                let oid = subproject_commit_oid_for_index(
                    fp,
                    repo.work_tree.as_deref(),
                    &target_adjusted,
                )?;
                let mode = grit_lib::index::MODE_GITLINK;
                let entry = grit_lib::index::IndexEntry {
                    ctime_sec: 0,
                    ctime_nsec: 0,
                    mtime_sec: 0,
                    mtime_nsec: 0,
                    dev: 0,
                    ino: 0,
                    mode,
                    uid: 0,
                    gid: 0,
                    size: 0,
                    oid,
                    flags: ((target_adjusted.len().min(0xFFF)) as u16) & 0x0FFF,
                    flags_extended: None,
                    path: target_adjusted.into_bytes(),
                    base_index_pos: 0,
                };
                index.add_or_replace(entry);
                continue;
            }

            let source_index = if source_adjusted != target_adjusted {
                &original_index
            } else {
                &index
            };
            if fp.old_mode.as_deref() == Some("160000") {
                let Some(old_ent) = source_index.get(source_adjusted.as_bytes(), 0) else {
                    bail!("{source_adjusted} not found in index");
                };
                if old_ent.mode != grit_lib::index::MODE_GITLINK {
                    bail!("{source_adjusted} not found in index");
                }
                if let Some(expected_oid) = fp.old_oid.as_deref() {
                    let index_hex = old_ent.oid.to_hex();
                    if !index_hex.starts_with(expected_oid) {
                        bail!("patch does not apply");
                    }
                }
            } else {
                let Some(entry) = source_index.get(source_adjusted.as_bytes(), 0) else {
                    bail!("{source_adjusted} not found in index");
                };
                if entry.mode == grit_lib::index::MODE_GITLINK {
                    bail!("{source_adjusted} not found in index");
                }
                let obj = repo.odb.read(&entry.oid)?;
                let old_content = String::from_utf8_lossy(&obj.data).into_owned();
                if let Some(expected_oid) = fp.old_oid.as_deref() {
                    verify_old_oid_matches_content(expected_oid, &old_content)?;
                }
            }

            let oid =
                subproject_commit_oid_for_index(fp, repo.work_tree.as_deref(), &target_adjusted)?;
            let mode = grit_lib::index::MODE_GITLINK;
            let entry = grit_lib::index::IndexEntry {
                ctime_sec: 0,
                ctime_nsec: 0,
                mtime_sec: 0,
                mtime_nsec: 0,
                dev: 0,
                ino: 0,
                mode,
                uid: 0,
                gid: 0,
                size: 0,
                oid,
                flags: ((target_adjusted.len().min(0xFFF)) as u16) & 0x0FFF,
                flags_extended: None,
                path: target_adjusted.clone().into_bytes(),
                base_index_pos: 0,
            };
            if fp.is_rename && source_adjusted != target_adjusted {
                index.remove(source_adjusted.as_bytes());
            }
            index.add_or_replace(entry);
            continue;
        }

        let path_bytes = target_adjusted.as_bytes();
        // Index preimage: for new-file patches, read the target path when present (add/add --3way).
        let old_content = if fp.is_new {
            if let Some(entry) = index.get(path_bytes, 0) {
                let obj = repo.odb.read(&entry.oid)?;
                String::from_utf8_lossy(&obj.data).into_owned()
            } else {
                String::new()
            }
        } else {
            let source_index = if source_adjusted != target_adjusted {
                &original_index
            } else {
                &index
            };
            if let Some(entry) = source_index.get(source_adjusted.as_bytes(), 0) {
                let obj = repo.odb.read(&entry.oid)?;
                String::from_utf8_lossy(&obj.data).into_owned()
            } else if can_apply_with_empty_preimage(fp) {
                String::new()
            } else {
                bail!("{source_adjusted} not found in index");
            }
        };
        if !fp.is_new {
            if let Some(expected_oid) = fp.old_oid.as_deref() {
                if !ws_mode.whitespace_fix && !args.three_way {
                    verify_old_oid_matches_content(expected_oid, &old_content)?;
                }
            }
        }

        let file_attrs = file_attrs_for_index_apply(&repo, &index, &target_raw);
        let favor = effective_merge_favor_for_path(&file_attrs, merge_favor_cli);
        let direct_to_threeway = fp.is_new && !old_content.is_empty();

        let (new_content, threeway_out) = if fp.hunks.is_empty() {
            (old_content.clone(), None)
        } else if args.three_way {
            if let Some(tw) = try_three_way_merge_blob(
                &repo,
                fp,
                old_content.as_bytes(),
                ws_mode,
                !args.reverse,
                patch_input_display,
                favor,
                conflict_style,
                &file_attrs,
                direct_to_threeway,
            )
            .with_context(|| format!("failed to apply patch to {target_adjusted}"))?
            {
                if tw.conflicted {
                    apply_nonzero = true;
                }
                (
                    String::from_utf8_lossy(&tw.merged_bytes).into_owned(),
                    Some(tw),
                )
            } else {
                let s = apply_hunks(
                    &old_content,
                    fp,
                    ws_mode,
                    !args.reverse,
                    patch_input_display,
                )
                .with_context(|| format!("failed to apply patch to {target_adjusted}"))?;
                (s, None)
            }
        } else {
            let s = apply_hunks(
                &old_content,
                fp,
                ws_mode,
                !args.reverse,
                patch_input_display,
            )
            .with_context(|| format!("failed to apply patch to {target_adjusted}"))?;
            (s, None)
        };

        // Determine mode
        let mode = if let Some(m) = fp.new_mode.as_deref() {
            parse_mode(m)
        } else if source_adjusted != target_adjusted {
            original_index
                .get(source_adjusted.as_bytes(), 0)
                .map(|entry| entry.mode)
                .unwrap_or(0o100644)
        } else if let Some(entry) = index.get(source_adjusted.as_bytes(), 0) {
            entry.mode
        } else {
            0o100644
        };

        if let Some(tw) = threeway_out.as_ref() {
            if tw.conflicted {
                add_conflicted_index_stages(&mut index, path_bytes, mode, &tw.stages);
                if fp.is_rename && source_adjusted != target_adjusted {
                    index.remove(source_adjusted.as_bytes());
                }
                continue;
            }
        }

        // Write new blob to ODB
        let new_oid = repo.odb.write(ObjectKind::Blob, new_content.as_bytes())?;

        // Update index entry
        let size = new_content.len() as u32;
        let entry = grit_lib::index::IndexEntry {
            ctime_sec: 0,
            ctime_nsec: 0,
            mtime_sec: 0,
            mtime_nsec: 0,
            dev: 0,
            ino: 0,
            mode,
            uid: 0,
            gid: 0,
            size,
            oid: new_oid,
            flags: ((target_adjusted.len().min(0xFFF)) as u16) & 0x0FFF,
            flags_extended: None,
            path: target_adjusted.clone().into_bytes(),
            base_index_pos: 0,
        };

        if fp.is_rename && source_adjusted != target_adjusted {
            index.remove(source_adjusted.as_bytes());
        }
        index.add_or_replace(entry);
    }

    repo.write_index(&mut index)?;
    if apply_nonzero {
        if args.three_way {
            let config = ConfigSet::load(Some(&repo.git_dir), true).unwrap_or_default();
            if rerere_enabled(&config, &repo.git_dir) {
                let _ = repo_rerere(&repo, RerereAutoupdate::FromConfig);
            }
        }
        return Err(anyhow::Error::new(ExplicitExit {
            code: 1,
            message: String::new(),
        }));
    }
    Ok(())
}

/// Mark patch-created paths as intent-to-add entries in the index.
///
/// This implements `git apply -N/--intent-to-add` for worktree applies.
/// The option is ignored outside a repository and when `--index`/`--cached`
/// modes are active.
fn apply_intent_to_add_entries(patches: &[FilePatch], args: &Args) -> Result<()> {
    if args.cached || args.index {
        return Ok(());
    }

    let repo = match Repository::discover(None) {
        Ok(repo) => repo,
        Err(_) => return Ok(()),
    };
    let mut index = match repo.load_index() {
        Ok(idx) => idx,
        Err(_) => Index::new(),
    };

    let cwd_prefix = if let Some(ref wt) = repo.work_tree {
        if let Ok(cwd) = std::env::current_dir() {
            if let Ok(rel) = cwd.strip_prefix(wt) {
                let s = rel.to_string_lossy().to_string();
                if s.is_empty() {
                    String::new()
                } else {
                    format!("{s}/")
                }
            } else {
                String::new()
            }
        } else {
            String::new()
        }
    } else {
        String::new()
    };

    for fp in patches {
        if !fp.is_new {
            continue;
        }
        let Some(target_path) = fp.target_path() else {
            continue;
        };
        let target_raw = adjust_path(target_path, args.directory.as_deref());
        let target_adjusted = format!("{cwd_prefix}{target_raw}");
        if target_adjusted.is_empty() {
            continue;
        }

        let mode = fp.new_mode.as_deref().map(parse_mode).unwrap_or(0o100644);
        let mut entry = IndexEntry {
            ctime_sec: 0,
            ctime_nsec: 0,
            mtime_sec: 0,
            mtime_nsec: 0,
            dev: 0,
            ino: 0,
            mode,
            uid: 0,
            gid: 0,
            size: 0,
            oid: grit_lib::diff::empty_blob_oid(),
            flags: ((target_adjusted.len().min(0xFFF)) as u16) & 0x0FFF,
            flags_extended: None,
            path: target_adjusted.into_bytes(),
            base_index_pos: 0,
        };
        entry.set_intent_to_add(true);
        index.add_or_replace(entry);
    }

    repo.write_index(&mut index)?;
    Ok(())
}

/// Check if patches apply cleanly without modifying anything.
fn check_patches(
    patches: &[FilePatch],
    args: &Args,
    ws_mode: ApplyWhitespaceMode,
    patch_input_display: &str,
) -> Result<()> {
    let work_tree = apply_work_tree_root();
    let work_tree_ref = work_tree.as_deref();
    let setup_prefix = apply_setup_prefix();
    let crlf_ctx = ApplyCrlfContext::load();
    for fp in patches {
        let path_str = fp
            .effective_path()
            .ok_or_else(|| anyhow::anyhow!("patch has no file path"))?;
        let path_adjusted = adjust_path(path_str, args.directory.as_deref());
        let path_operational = fp.worktree_rel_operational(&path_adjusted, &setup_prefix);
        let path = apply_fs_path(&path_operational, work_tree_ref);

        if fp.is_deleted {
            if !path.exists() {
                bail!("{}: does not exist", path.display());
            }
            continue;
        }

        if fp.is_new {
            if path.exists() {
                bail!("{}: already exists", path.display());
            }
            // Verify hunks apply to empty content
            apply_hunks("", fp, ws_mode, !args.reverse, patch_input_display)?;
            continue;
        }

        let source_adjusted = fp
            .source_path()
            .map(|p| adjust_path(p, args.directory.as_deref()))
            .unwrap_or_else(|| path_adjusted.clone());
        let source_operational = fp.worktree_rel_operational(&source_adjusted, &setup_prefix);
        let read_path = apply_fs_path(&source_operational, work_tree_ref);
        let old_content = match &crlf_ctx {
            Some(ctx) => {
                if !read_path.exists() && can_apply_with_empty_preimage(fp) {
                    String::new()
                } else {
                    ctx.normalized_text(&read_path, &source_operational)?
                }
            }
            None => match fs::read_to_string(&read_path) {
                Ok(content) => content,
                Err(err)
                    if err.kind() == std::io::ErrorKind::NotFound
                        && can_apply_with_empty_preimage(fp) =>
                {
                    String::new()
                }
                Err(err) => {
                    return Err(err)
                        .with_context(|| format!("failed to read {}", read_path.display()))
                }
            },
        };
        if let Some(expected_oid) = fp.old_oid.as_deref() {
            if !ws_mode.whitespace_fix {
                verify_old_oid_matches_content(expected_oid, &old_content)?;
            }
        }
        apply_hunks(
            &old_content,
            fp,
            ws_mode,
            !args.reverse,
            patch_input_display,
        )?;
    }

    Ok(())
}

const S_IFMT: u32 = 0o170000;
const S_IFREG: u32 = 0o100000;
const S_IFLNK: u32 = 0o120000;
const S_IFDIR: u32 = 0o040000;
const S_IFGITLINK: u32 = 0o160000;

/// Git `ce_permissions`: only the owner execute bit matters for regular files.
fn ce_permissions(mode: u32) -> u32 {
    if mode & 0o100 != 0 {
        0o755
    } else {
        0o644
    }
}

/// Git `create_ce_mode` for stat modes.
fn create_ce_mode(mode: u32) -> u32 {
    match mode & S_IFMT {
        S_IFLNK => MODE_SYMLINK,
        S_IFDIR => S_IFDIR,
        S_IFGITLINK => MODE_GITLINK,
        S_IFREG => S_IFREG | ce_permissions(mode),
        0 => S_IFREG | ce_permissions(mode),
        _ => MODE_REGULAR,
    }
}

/// Canonicalize a parsed tree mode (Git `canon_mode`).
fn canon_mode(mode: u32) -> u32 {
    match mode & S_IFMT {
        S_IFREG => S_IFREG | ce_permissions(mode),
        S_IFLNK => MODE_SYMLINK,
        S_IFDIR => S_IFDIR,
        S_IFGITLINK => MODE_GITLINK,
        _ => MODE_REGULAR,
    }
}

/// Git `ce_mode_from_stat` (matches `read-cache.h`).
fn ce_mode_from_stat(ce: Option<u32>, st_mode: u32, trust_file_mode: bool) -> u32 {
    let is_reg = st_mode & S_IFMT == S_IFREG;
    if !trust_file_mode && is_reg {
        if let Some(ce_m) = ce {
            if ce_m & S_IFMT == S_IFREG {
                return ce_m;
            }
        }
        return create_ce_mode(0o666);
    }
    create_ce_mode(st_mode)
}

/// Parse a mode token from a patch header (Git `parse_mode_line`); returns canonical index mode.
fn parse_mode_token(line: &str, patch_input_display: &str, line_no: usize) -> Result<u32> {
    let line = line.trim_end_matches('\r').trim_end();
    let mut end = 0usize;
    for (i, ch) in line.char_indices() {
        if !matches!(ch, '0'..='7') {
            end = i;
            break;
        }
    }
    if end == 0 && !line.is_empty() && matches!(line.as_bytes()[0], b'0'..=b'7') {
        end = line.len();
    }
    if end == 0 {
        if patch_input_display.is_empty() || patch_input_display == "<stdin>" {
            bail!("invalid mode on line {line_no}: {line}");
        }
        bail!("invalid mode at {patch_input_display}:{line_no}: {line}");
    }
    let rest = &line[end..];
    if !rest.is_empty() && !rest.starts_with(|c: char| c.is_ascii_whitespace()) {
        if patch_input_display.is_empty() || patch_input_display == "<stdin>" {
            bail!("invalid mode on line {line_no}: {line}");
        }
        bail!("invalid mode at {patch_input_display}:{line_no}: {line}");
    }
    let oct_str = &line[..end];
    let raw = u32::from_str_radix(oct_str, 8).map_err(|_| {
        if patch_input_display.is_empty() || patch_input_display == "<stdin>" {
            anyhow::anyhow!("invalid mode on line {line_no}: {line}")
        } else {
            anyhow::anyhow!("invalid mode at {patch_input_display}:{line_no}: {line}")
        }
    })?;
    Ok(canon_mode(raw))
}

fn validate_and_canonicalize_patch_modes(
    patches: &mut [FilePatch],
    patch_input_display: &str,
) -> Result<()> {
    for fp in patches.iter_mut() {
        if let Some(ref s) = fp.old_mode {
            let line = fp.old_mode_line.unwrap_or(0);
            let canon = parse_mode_token(s, patch_input_display, line.max(1))?;
            fp.old_mode = Some(format!("{canon:o}"));
        }
        if let Some(ref s) = fp.new_mode {
            let line = fp.new_mode_line.unwrap_or(0);
            let canon = parse_mode_token(s, patch_input_display, line.max(1))?;
            fp.new_mode = Some(format!("{canon:o}"));
        }
    }
    Ok(())
}

/// Parse canonical mode string (already validated) to `u32`.
fn parse_mode(s: &str) -> u32 {
    u32::from_str_radix(s.trim(), 8).unwrap_or(MODE_REGULAR)
}

fn preimage_mode_mismatch_warn(path: &str, st_mode: u32, expected: u32) {
    eprintln!(
        "warning: {} has type {:o}, expected {:o}",
        path, st_mode, expected
    );
}

/// Git `check_preimage`: compute `st_mode` for comparing against `patch->old_mode`.
fn preimage_st_mode_for_apply(
    trust_file_mode: bool,
    cached: bool,
    check_index: bool,
    ce_mode: Option<u32>,
    st_stat: Option<u32>,
    patch_old_declared: Option<u32>,
    old_path_display: &str,
) -> Result<u32> {
    if cached {
        return ce_mode
            .ok_or_else(|| anyhow::anyhow!("{old_path_display}: does not exist in index"));
    }
    if check_index {
        let ce = ce_mode
            .ok_or_else(|| anyhow::anyhow!("{old_path_display}: does not exist in index"))?;
        if ce == MODE_GITLINK {
            return Ok(ce);
        }
        let st = st_stat.ok_or_else(|| anyhow::anyhow!("failed to stat {}", old_path_display))?;
        let is_reg = st & S_IFMT == S_IFREG;
        if trust_file_mode || !is_reg {
            return Ok(ce_mode_from_stat(Some(ce), st, trust_file_mode));
        }
        return Ok(ce);
    }
    let st = st_stat.ok_or_else(|| anyhow::anyhow!("failed to stat {}", old_path_display))?;
    let is_reg = st & S_IFMT == S_IFREG;
    if trust_file_mode || !is_reg {
        return Ok(ce_mode_from_stat(ce_mode, st, trust_file_mode));
    }
    if let Some(ce) = ce_mode {
        return Ok(ce);
    }
    Ok(patch_old_declared.unwrap_or(MODE_REGULAR))
}

fn maybe_emit_apply_sparse_index_trace(patches: &[FilePatch], args: &Args) {
    if !(args.index || args.cached) {
        return;
    }
    let Ok(repo) = Repository::discover(None) else {
        return;
    };
    let Ok(index) = load_raw_index_for_sparse_detection(&repo) else {
        return;
    };
    if !patches
        .iter()
        .any(|fp| patch_touches_sparse_dir(fp, args, &index))
    {
        return;
    }
    if let Ok(trace2_event) = std::env::var("GIT_TRACE2_EVENT") {
        if !trace2_event.trim().is_empty() {
            let _ = crate::trace2_region_json(&trace2_event, "index", "ensure_full_index");
        }
    }
}

fn load_raw_index_for_sparse_detection(repo: &Repository) -> Result<Index> {
    let index_path = repo.index_path_for_env()?;
    Index::load(&index_path).map_err(Into::into)
}

fn sparse_index_patch_paths(patches: &[FilePatch], args: &Args) -> Vec<String> {
    if !(args.index || args.cached) {
        return Vec::new();
    }
    let Ok(repo) = Repository::discover(None) else {
        return Vec::new();
    };
    let Ok(index) = load_raw_index_for_sparse_detection(&repo) else {
        return Vec::new();
    };
    let setup_prefix = apply_setup_prefix();
    let mut sparse_paths = Vec::new();
    for fp in patches {
        for path in fp.source_path().into_iter().chain(fp.target_path()) {
            if path == "/dev/null" {
                continue;
            }
            let adjusted = adjust_path(path, args.directory.as_deref());
            let operational = fp.worktree_rel_operational(&adjusted, &setup_prefix);
            if index_path_is_sparse(&index, &operational) {
                sparse_paths.push(operational);
            }
        }
    }
    sparse_paths
}

fn patch_touches_sparse_dir(fp: &FilePatch, args: &Args, index: &Index) -> bool {
    fp.source_path()
        .into_iter()
        .chain(fp.target_path())
        .filter(|path| *path != "/dev/null")
        .map(|path| adjust_path(path, args.directory.as_deref()))
        .any(|path| index_path_is_under_sparse_dir(index, &path))
}

fn index_path_is_under_sparse_dir(index: &Index, path: &str) -> bool {
    let path = path.trim_end_matches('/');
    let candidates = [
        path,
        path.strip_prefix("a/").unwrap_or(path),
        path.strip_prefix("b/").unwrap_or(path),
    ];
    index
        .entries
        .iter()
        .filter(|entry| entry.stage() == 0 && entry.mode == MODE_TREE)
        .filter_map(|entry| std::str::from_utf8(&entry.path).ok())
        .map(|prefix| prefix.trim_end_matches('/'))
        .any(|prefix| {
            let prefix_slash = format!("{prefix}/");
            candidates
                .iter()
                .any(|candidate| *candidate == prefix || candidate.starts_with(&prefix_slash))
        })
}

fn index_path_is_sparse(index: &Index, path: &str) -> bool {
    let path = path.trim_end_matches('/');
    let candidates = [
        path,
        path.strip_prefix("a/").unwrap_or(path),
        path.strip_prefix("b/").unwrap_or(path),
    ];
    candidates.iter().any(|candidate| {
        index
            .get(candidate.as_bytes(), 0)
            .is_some_and(|entry| entry.skip_worktree())
            || index_path_is_under_sparse_dir(index, candidate)
    })
}

fn path_is_sparse_in_indexes(index: Option<&Index>, raw_index: Option<&Index>, path: &str) -> bool {
    index.is_some_and(|ix| index_path_is_sparse(ix, path))
        || raw_index.is_some_and(|ix| index_path_is_sparse(ix, path))
}

fn path_matches_sparse_patch_paths(path: &str, sparse_paths: &[String]) -> bool {
    sparse_paths
        .iter()
        .any(|sparse_path| path_variants_match(path, sparse_path))
}

fn path_variants_match(left: &str, right: &str) -> bool {
    let left = left.trim_end_matches('/');
    let right = right.trim_end_matches('/');
    let left_variants = [
        left,
        left.strip_prefix("a/").unwrap_or(left),
        left.strip_prefix("b/").unwrap_or(left),
    ];
    let right_variants = [
        right,
        right.strip_prefix("a/").unwrap_or(right),
        right.strip_prefix("b/").unwrap_or(right),
    ];
    left_variants
        .iter()
        .any(|left| right_variants.iter().any(|right| left == right))
}

fn path_outside_sparse_checkout(repo: &Repository, path: &str) -> bool {
    let config = ConfigSet::load(Some(&repo.git_dir), true).unwrap_or_default();
    let sparse_enabled = config
        .get("core.sparseCheckout")
        .map(|v| v.eq_ignore_ascii_case("true"))
        .unwrap_or(false);
    if !sparse_enabled {
        return false;
    }
    let sparse_file = repo.git_dir.join("info").join("sparse-checkout");
    let Ok(contents) = fs::read_to_string(sparse_file) else {
        return false;
    };
    let patterns = parse_sparse_checkout_file(&contents);
    if patterns.is_empty() {
        return false;
    }
    let cone_config = config
        .get("core.sparseCheckoutCone")
        .and_then(|v| v.parse::<bool>().ok())
        .unwrap_or(true);
    let path = path.trim_end_matches('/');
    let included = if effective_cone_mode_for_sparse_file(cone_config, &patterns) {
        path_in_sparse_checkout_patterns(path, &patterns, true)
    } else {
        grit_lib::ignore::path_in_sparse_checkout(path, &patterns, repo.work_tree.as_deref())
    };
    !included
}

/// Reconcile patch extended modes with index/stat like Git `check_preimage`.
fn reconcile_filepatch_preimage_modes(
    fp: &mut FilePatch,
    trust_file_mode: bool,
    cached: bool,
    check_index: bool,
    ce_mode: Option<u32>,
    st_stat: Option<u32>,
    old_path_display: &str,
) -> Result<()> {
    if fp.is_new {
        return Ok(());
    }
    let old_name = fp
        .old_path
        .as_deref()
        .filter(|p| *p != "/dev/null")
        .unwrap_or(old_path_display);

    let patch_old = fp.old_mode.as_ref().map(|s| parse_mode(s));

    let st_mode = preimage_st_mode_for_apply(
        trust_file_mode,
        cached,
        check_index,
        ce_mode,
        st_stat,
        patch_old,
        old_path_display,
    )?;

    if fp.old_mode.is_none() {
        fp.old_mode = Some(format!("{st_mode:o}"));
    }
    let effective_old = patch_old.unwrap_or(st_mode);
    if (st_mode ^ effective_old) & S_IFMT != 0 {
        bail!("{old_name}: wrong type");
    }
    if st_mode != effective_old {
        preimage_mode_mismatch_warn(old_name, st_mode, effective_old);
    }
    if fp.new_mode.is_none() && !fp.is_deleted {
        fp.new_mode = Some(format!("{st_mode:o}"));
    }
    Ok(())
}

/// Fill in missing `old_mode` / `new_mode` and emit mode mismatch warnings (Git `check_preimage`).
fn prepare_patch_modes_for_apply(patches: &mut [FilePatch], args: &Args) -> Result<()> {
    let repo_ok = Repository::discover(None).ok();
    let raw_index = repo_ok
        .as_ref()
        .and_then(|r| load_raw_index_for_sparse_detection(r).ok());
    let index = repo_ok.as_ref().and_then(|r| r.load_index().ok());
    let work_tree = repo_ok.as_ref().and_then(|r| r.work_tree.clone());
    let work_tree_ref = work_tree.as_deref();
    let setup_prefix = apply_setup_prefix();
    let trust_file_mode = repo_ok
        .as_ref()
        .map(|r| {
            ConfigSet::load(Some(&r.git_dir), true)
                .unwrap_or_default()
                .get_bool("core.fileMode")
                .and_then(|v| v.ok())
                .unwrap_or(true)
        })
        .unwrap_or(true);

    for fp in patches.iter_mut() {
        if fp.is_new {
            continue;
        }
        let Some(source) = fp.source_path() else {
            continue;
        };
        if source == "/dev/null" {
            continue;
        }
        let adjusted = adjust_path(source, args.directory.as_deref());
        let operational = fp.worktree_rel_operational(&adjusted, &setup_prefix);
        let ce_entry = index
            .as_ref()
            .and_then(|ix| ix.get(operational.as_bytes(), 0));
        let ce_mode = ce_entry.map(|e| e.mode);

        if args.cached {
            reconcile_filepatch_preimage_modes(
                fp,
                trust_file_mode,
                true,
                false,
                ce_mode,
                None,
                &adjusted,
            )?;
            continue;
        }

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let path = apply_fs_path(&operational, work_tree_ref);
            let st_stat = match fs::symlink_metadata(&path) {
                Ok(meta) => Some(meta.permissions().mode()),
                Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                    if (args.index
                        && path_is_sparse_in_indexes(
                            index.as_ref(),
                            raw_index.as_ref(),
                            &operational,
                        ))
                        || can_apply_with_empty_preimage(fp)
                    {
                        None
                    } else {
                        return Err(err).with_context(|| format!("failed to stat {adjusted}"));
                    }
                }
                Err(err) => {
                    return Err(err).with_context(|| format!("failed to stat {adjusted}"));
                }
            };
            let Some(st) = st_stat else {
                continue;
            };
            reconcile_filepatch_preimage_modes(
                fp,
                trust_file_mode,
                false,
                args.index,
                ce_mode,
                Some(st),
                &adjusted,
            )?;
        }
        #[cfg(not(unix))]
        {
            let _ = (ce_mode, trust_file_mode);
            bail!("file mode handling is only supported on Unix");
        }
    }

    Ok(())
}

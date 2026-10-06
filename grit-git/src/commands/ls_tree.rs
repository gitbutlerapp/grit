//! `grit ls-tree` — list the contents of a tree object.

#![allow(clippy::collapsible_match)]
use anyhow::{bail, Context, Result};
use clap::Args as ClapArgs;
use std::io::{self, Write};
use std::path::Path;

use grit_lib::config::ConfigSet;
use grit_lib::crlf::AttrRule;
use grit_lib::objects::{parse_commit, parse_tag, parse_tree, ObjectId, ObjectKind, TreeEntry};
use grit_lib::pathspec::matches_pathspec_set_for_object_ls_tree;
use grit_lib::refs::resolve_ref;
use grit_lib::repo::Repository;
use grit_lib::rev_parse::abbreviate_object_id;

/// Default maximum tree recursion depth when `core.maxtreedepth` is unset.
const DEFAULT_MAX_TREE_DEPTH: usize = 2048;
/// Canonical empty tree object ID (SHA-1).
const EMPTY_TREE_OID: &str = "4b825dc642cb6eb9a060e54bf8d69288fbee4904";

/// Arguments for `grit ls-tree`.
#[derive(Debug, ClapArgs)]
#[command(args_override_self = true)]
pub struct Args {
    /// Show only trees (not blobs).
    #[arg(short = 'd')]
    pub only_trees: bool,

    /// Recurse into sub-trees.
    #[arg(short = 'r')]
    pub recursive: bool,

    /// Show trees even when recursing.
    #[arg(short = 't')]
    pub show_trees: bool,

    /// Show object size (long format).
    #[arg(short = 'l', long)]
    pub long: bool,

    /// Show only names.
    #[arg(long = "name-only")]
    pub name_only: bool,

    /// Show only names (same as --name-only).
    #[arg(long = "name-status")]
    pub name_status: bool,

    /// Show only object names (hashes).
    #[arg(long = "object-only")]
    pub object_only: bool,

    /// \0 line termination on output.
    #[arg(short = 'z')]
    pub null_terminated: bool,

    /// Abbreviate OIDs (Git: `--abbrev` or `--abbrev=<n>`; bare `--abbrev` defaults to 7).
    #[arg(
        long,
        value_name = "N",
        default_missing_value = "7",
        num_args = 0..=1,
        require_equals = true
    )]
    pub abbrev: Option<String>,

    /// Format string for output.
    #[arg(long)]
    pub format: Option<String>,

    /// Show full path names (even when called from a subdirectory).
    #[arg(
        long = "full-name",
        action = clap::ArgAction::SetTrue,
        overrides_with = "no_full_name"
    )]
    pub full_name: bool,

    /// Show relative path names (default; counterpart to --full-name).
    #[arg(
        long = "no-full-name",
        action = clap::ArgAction::SetTrue,
        overrides_with = "full_name"
    )]
    pub no_full_name: bool,

    /// Do not limit the listing to the current working tree.
    #[arg(long = "full-tree")]
    pub full_tree: bool,

    /// The tree-ish to list (defaults to `HEAD` when omitted).
    #[arg(default_value = "HEAD")]
    pub tree_ish: String,

    /// Paths to restrict listing.
    #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
    pub paths: Vec<String>,
}

fn ls_tree_abbrev_len(args: &Args) -> usize {
    let Some(raw) = args.abbrev.as_deref() else {
        return 40;
    };
    if raw.is_empty() {
        return 7;
    }
    let n: usize = raw.parse().unwrap_or(7);
    n.clamp(4, 40)
}

fn file_type_mask(mode: u32) -> u32 {
    mode & 0o170000
}

/// True for blob-like entries (`git ls-tree -d` hides these, but keeps trees and submodules).
fn is_blob_like_tree_entry(mode: u32) -> bool {
    matches!(
        file_type_mask(mode),
        0o100000 | 0o120000 // regular / symlink
    )
}

/// After `core.maxtreedepth` checks, match Git: `-d` together with `-r` implies `-t`.
fn apply_ls_tree_implications(args: &mut Args) {
    if args.only_trees && args.recursive {
        args.show_trees = true;
    }
}

/// Git rejects pathspecs that normalize outside the work tree when `--full-tree` is used.
///
/// With `--full-tree`, Git parses pathspecs with a `NULL` prefix (see `builtin/ls-tree.c`), so
/// normalization behaves like `normalize_path_copy` on the raw string — e.g. `../` fails even
/// though the process cwd is inside a subdirectory.
fn ensure_full_tree_pathspecs_in_repo(repo: &Repository, paths: &[String]) -> Result<()> {
    if paths.is_empty() {
        return Ok(());
    }
    let Some(wt) = repo.work_tree.as_ref() else {
        return Ok(());
    };
    let wt_hint = wt.canonicalize().unwrap_or_else(|_| wt.to_path_buf());
    let wt_display = wt_hint.display().to_string();

    for spec in paths {
        if spec.starts_with('/') {
            bail!("fatal: '{spec}': absolute pathspec outside working tree");
        }
        let normalized = git_normalize_pathspec_no_prefix(spec).map_err(|_| {
            anyhow::anyhow!("fatal: {spec}: '{spec}' is outside repository at '{wt_display}'")
        })?;
        let resolved = if normalized.is_empty() {
            wt_hint.clone()
        } else {
            wt_hint.join(Path::new(&normalized))
        };
        let resolved = resolved.canonicalize().unwrap_or(resolved);
        if !(resolved == wt_hint || resolved.starts_with(&wt_hint)) {
            bail!("fatal: {spec}: '{spec}' is outside repository at '{wt_display}'");
        }
    }
    Ok(())
}

/// Match Git's `normalize_path_copy` / `prefix_path_gently` behavior for a relative pathspec when
/// the pathspec prefix is empty: leading `..` components that would strip past the root fail.
/// Repo-root-relative path of the current directory within the work tree, using `/` separators.
/// `None` if the process cwd is outside the work tree (Git then lists from the repository root).
fn cwd_relative_to_work_tree(repo: &Repository) -> Result<Option<String>> {
    let Some(wt) = repo.work_tree.as_ref() else {
        return Ok(None);
    };
    let cwd = std::env::current_dir().context("resolving cwd")?;
    let Ok(rel) = cwd.strip_prefix(wt) else {
        return Ok(None);
    };
    if rel.as_os_str().is_empty() {
        return Ok(Some(String::new()));
    }
    let mut parts = Vec::new();
    for comp in rel.components() {
        match comp {
            std::path::Component::Normal(s) => parts.push(s.to_string_lossy().into_owned()),
            std::path::Component::CurDir => {}
            std::path::Component::ParentDir => {
                if parts.pop().is_none() {
                    return Ok(None);
                }
            }
            _ => {}
        }
    }
    Ok(Some(parts.join("/")))
}

/// Walk from `tree_oid` following directory components in `cwd_rel`.
///
/// Returns `Ok(None)` when any path component is missing from the tree (Git prints nothing), or
/// when a component exists but is not a tree.
fn descend_tree_to_path(
    repo: &Repository,
    mut tree_oid: ObjectId,
    mut path_prefix: String,
    cwd_rel: &str,
) -> Result<Option<(ObjectId, String)>> {
    if cwd_rel.is_empty() {
        return Ok(Some((tree_oid, path_prefix)));
    }
    for part in cwd_rel.split('/').filter(|p| !p.is_empty()) {
        let obj = repo
            .odb
            .read(&tree_oid)
            .context("reading tree while narrowing ls-tree")?;
        let entries = parse_tree(&obj.data)?;
        let found: Option<&TreeEntry> = entries.iter().find(|e| {
            let name = String::from_utf8_lossy(&e.name);
            name == part && file_type_mask(e.mode) == 0o040000
        });
        let Some(entry) = found else {
            return Ok(None);
        };
        if path_prefix.is_empty() {
            path_prefix = part.to_string();
        } else {
            path_prefix = format!("{path_prefix}/{part}");
        }
        tree_oid = entry.oid;
    }
    Ok(Some((tree_oid, path_prefix)))
}

fn git_normalize_pathspec_no_prefix(spec: &str) -> Result<String> {
    let mut stack: Vec<&str> = Vec::new();
    for part in spec.split('/') {
        if part.is_empty() || part == "." {
            continue;
        }
        if part == ".." {
            if stack.pop().is_none() {
                bail!("pathspec escapes repository root");
            }
        } else {
            stack.push(part);
        }
    }
    Ok(stack.join("/"))
}

fn git_normalize_pathspec_preserve_trailing_slash(spec: &str) -> Result<String> {
    let has_trailing_slash = spec.ends_with('/');
    let mut normalized = git_normalize_pathspec_no_prefix(spec)?;
    if has_trailing_slash && !normalized.is_empty() {
        normalized.push('/');
    }
    Ok(normalized)
}

fn only_trees_pathspec_wants_descent(spec: &str, full_name: &str) -> bool {
    let Ok(normalized) = git_normalize_pathspec_preserve_trailing_slash(spec) else {
        return false;
    };
    let dir_prefix = format!("{full_name}/");
    normalized == dir_prefix
        || (normalized.starts_with(&dir_prefix) && normalized.len() > dir_prefix.len())
}

/// Git-canonical match string for a (non-magic) pathspec: collapses `.`/`..`/empty path
/// components and runs of `/`, preserving at most one trailing slash (mirrors
/// `normalize_path_copy` as used by `prefix_path_gently`). Magic pathspecs (`:(...)`, `:!`, ...)
/// are left untouched so their parsing in `pathspec.rs` still applies.
fn ls_tree_spec_match_string(spec: &str) -> String {
    if spec.starts_with(':') {
        return spec.to_string();
    }
    git_normalize_pathspec_preserve_trailing_slash(spec).unwrap_or_else(|_| spec.to_string())
}

/// Mirror Git's `show_recursive` (`builtin/ls-tree.c`) using only the pathspec list — the `-r`
/// short-circuit is applied separately by the caller. `base_with_slash` is the directory prefix
/// being walked (empty at the tree root, otherwise ending in `/`); `name` is the entry's own name.
///
/// Returns true when some pathspec reaches strictly *below* this entry, i.e. Git would descend
/// into it rather than show it as a leaf.
fn ls_tree_show_recursive(specs: &[String], base_with_slash: &str, name: &str) -> bool {
    let base = base_with_slash.as_bytes();
    let baselen = base.len();
    let name_b = name.as_bytes();
    let len = name_b.len();
    for raw in specs {
        let spec = ls_tree_spec_match_string(raw);
        let spec_b = spec.as_bytes();
        // strncmp(base, spec, baselen): spec must agree with the base prefix.
        if spec_b.len() < baselen || spec_b[..baselen] != base[..] {
            continue;
        }
        let rest = &spec_b[baselen..];
        // The spec (relative to base) must be strictly longer than the entry name to descend.
        if rest.len() <= len {
            continue;
        }
        // The character after the matched name must be a path separator.
        if rest[len] != b'/' {
            continue;
        }
        if rest[..len] != name_b[..] {
            continue;
        }
        return true;
    }
    false
}

/// Run `grit ls-tree`.
pub fn run(mut args: Args) -> Result<()> {
    // Validate incompatible display-mode options
    {
        let mut display_modes: Vec<&str> = Vec::new();
        if args.long {
            display_modes.push("--long");
        }
        if args.name_only {
            display_modes.push("--name-only");
        }
        if args.name_status {
            display_modes.push("--name-status");
        }
        if args.object_only {
            display_modes.push("--object-only");
        }
        if display_modes.len() > 1 {
            eprintln!(
                "error: {} and {} cannot be used together",
                display_modes[0], display_modes[1]
            );
            std::process::exit(129);
        }
        if args.format.is_some() && !display_modes.is_empty() {
            eprintln!(
                "error: {} and --format cannot be used together",
                display_modes[0]
            );
            std::process::exit(129);
        }
    }

    // --name-status is an alias for --name-only
    if args.name_status {
        args.name_only = true;
    }

    let repo = Repository::discover(None).context("not a git repository")?;
    let config = ConfigSet::load(Some(&repo.git_dir), true)?;
    let max_tree_depth = resolve_max_tree_depth(&config)?;
    let quote_fully = config.quote_path_fully();
    let attr_rules = if let Some(ref wt) = repo.work_tree {
        grit_lib::crlf::load_gitattributes(wt)
    } else {
        Vec::new()
    };

    apply_ls_tree_implications(&mut args);

    if args.full_tree && !args.paths.is_empty() {
        ensure_full_tree_pathspecs_in_repo(&repo, &args.paths)?;
    }

    // Resolve pathspecs relative to cwd within the work tree, then express
    // them as repo-root-relative paths so the tree walk can match correctly.
    // Resolve cwd-relative pathspecs to repo-root-relative paths (join cwd prefix inside the work
    // tree, then normalize `..` — see t3102 `../a[a]` from a subdirectory).
    if !args.paths.is_empty() && !args.full_tree {
        if let Some(ref wt) = repo.work_tree {
            let cwd = std::env::current_dir().context("resolving cwd")?;
            let prefix = cwd.strip_prefix(wt).unwrap_or(std::path::Path::new(""));
            if !prefix.as_os_str().is_empty() {
                let mut resolved = Vec::with_capacity(args.paths.len());
                for p in &args.paths {
                    let combined = prefix.join(p);
                    let mut norm = Vec::new();
                    for comp in combined.components() {
                        match comp {
                            std::path::Component::ParentDir => {
                                norm.pop();
                            }
                            std::path::Component::CurDir => {}
                            other => norm.push(other.as_os_str().to_string_lossy().into_owned()),
                        }
                    }
                    resolved.push(norm.join("/"));
                }
                args.paths = resolved;
            }
        }
    }

    // Canonicalize each (non-magic) pathspec the way Git's `prefix_path_gently` does: collapse
    // `.`/`..`/empty components and runs of `/`, keeping at most one trailing slash. This makes
    // `path3/2.txt///` behave like `path3/2.txt/` (directory-only) and `path3//23.txt` like
    // `path3/23.txt`.
    for p in args.paths.iter_mut() {
        *p = ls_tree_spec_match_string(p);
    }

    // `dir/../` from a subdirectory normalizes to `""`, which Git treats as "match everything".
    if args.paths.iter().any(|p| p.is_empty()) {
        args.paths.clear();
    }

    let oid = resolve_tree_ish(&repo, &args.tree_ish)?;
    let obj = repo.odb.read(&oid)?;

    // Peel tags to their target, then commits to their tree.
    let mut current_oid = oid;
    let mut obj = obj;
    loop {
        match obj.kind {
            ObjectKind::Tag => {
                let tag = parse_tag(&obj.data).context("parsing tag")?;
                current_oid = tag.object;
                obj = repo.odb.read(&current_oid).context("reading tag target")?;
            }
            ObjectKind::Commit => {
                let commit = parse_commit(&obj.data).context("parsing commit")?;
                current_oid = commit.tree;
                obj = repo.odb.read(&current_oid).context("reading tree")?;
            }
            ObjectKind::Tree => break,
            _ => bail!("'{}' is not a tree object", args.tree_ish),
        }
    }
    let mut tree_oid = current_oid;
    let mut tree_data = obj.data;

    let stdout = io::stdout();
    let mut out = stdout.lock();
    let term = if args.null_terminated { b'\0' } else { b'\n' };

    // Match Git: at work tree root, paths are repo-root-relative; in a subdirectory, default is
    // cwd-relative unless `--full-name` / `--full-tree`. `--no-full-name` only matters in a subdir.
    let at_repo_root = repo.work_tree.as_ref().is_none_or(|wt| {
        std::env::current_dir()
            .ok()
            .is_none_or(|cwd| match cwd.strip_prefix(wt) {
                Ok(p) => p.as_os_str().is_empty(),
                Err(_) => true,
            })
    });
    // --full-tree and --full-name are separate concerns:
    //   --full-tree  affects scope (skip cwd narrowing, show entire tree)
    //   --full-name  affects display only (repo-root relative paths)
    let scope_full_tree = args.full_tree || at_repo_root;
    let display_full_paths = args.full_name || scope_full_tree;

    // cwd_rel: needed for tree narrowing (scope) regardless of --full-name.
    // Only --full-tree skips cwd context entirely.
    // Display layer uses display_full_paths (above) to decide path formatting,
    // so cwd_rel being Some here when --full-name is set won't affect output.
    let cwd_rel = if args.full_tree || at_repo_root {
        None
    } else {
        cwd_relative_to_work_tree(&repo)?
    };

    // Git narrows the listing to the tree object for the current directory (see `read_tree` +
    // pathspec prefix). Pathspec arguments disable narrowing: `read_tree` matches against the full
    // tree so patterns like `../a[a]` from a subdirectory still resolve (t3102).
    let mut narrowed_prefix = String::new();
    if !scope_full_tree && args.paths.is_empty() {
        if let Some(ref cwd_rel) = cwd_rel {
            if !cwd_rel.is_empty() {
                let Some((oid, pfx)) =
                    descend_tree_to_path(&repo, tree_oid, String::new(), cwd_rel)?
                else {
                    return Ok(());
                };
                tree_oid = oid;
                tree_data = repo
                    .odb
                    .read(&tree_oid)
                    .context("reading narrowed tree for ls-tree")?
                    .data;
                narrowed_prefix = pfx;
            }
        }
    }

    // Prefix for paths under `list_tree` (repo-root-relative), and cwd-relative display adjustment.
    let (list_prefix, cwd_prefix) = if display_full_paths {
        // --full-tree: narrowed_prefix is empty, list from root with full paths.
        // --full-name: narrowed_prefix is set, use it as list prefix for repo-root display.
        (narrowed_prefix.clone(), None)
    } else if !narrowed_prefix.is_empty() {
        (narrowed_prefix.clone(), Some(narrowed_prefix))
    } else if let Some(ref rel) = cwd_rel {
        if !rel.is_empty() {
            (String::new(), Some(rel.clone()))
        } else {
            (String::new(), None)
        }
    } else {
        (String::new(), None)
    };

    list_tree(
        &repo,
        &tree_data,
        &list_prefix,
        0,
        max_tree_depth,
        &args,
        &mut out,
        term,
        cwd_prefix.as_deref(),
        quote_fully,
        &attr_rules,
    )?;

    Ok(())
}

/// Make a repo-root-relative path display-relative to the cwd prefix.
/// If path is under cwd_prefix, strip the prefix. Otherwise prepend ../.
fn make_cwd_relative(path: &str, cwd_prefix: Option<&str>) -> String {
    let prefix = match cwd_prefix {
        Some(p) if !p.is_empty() => p,
        _ => return path.to_string(),
    };
    let prefix_slash = format!("{}/", prefix);
    if path.starts_with(&prefix_slash) {
        // Path is inside our cwd: strip the prefix
        path[prefix_slash.len()..].to_string()
    } else {
        // Path is outside our cwd: prepend ../
        let depth = prefix.split('/').count();
        let mut result = String::new();
        for _ in 0..depth {
            result.push_str("../");
        }
        result.push_str(path);
        result
    }
}

#[allow(clippy::too_many_arguments)]
fn list_tree(
    repo: &Repository,
    data: &[u8],
    prefix: &str,
    depth: usize,
    max_tree_depth: usize,
    args: &Args,
    out: &mut impl Write,
    term: u8,
    cwd_prefix: Option<&str>,
    quote_fully: bool,
    attr_rules: &[AttrRule],
) -> Result<()> {
    if depth > max_tree_depth {
        bail!(
            "tree depth {} exceeds core.maxtreedepth {}",
            depth,
            max_tree_depth
        );
    }

    let entries = parse_tree(data)?;

    for entry in &entries {
        let name = String::from_utf8_lossy(&entry.name);
        let full_name = if prefix.is_empty() {
            name.to_string()
        } else {
            format!("{prefix}/{name}")
        };

        let is_tree = entry.mode == 0o040000;
        let is_submodule = file_type_mask(entry.mode) == 0o160000;

        // `base` is the directory prefix as Git's tree walk sees it: empty at the tree root,
        // otherwise ending in `/`. Used by `ls_tree_show_recursive` (mirrors `show_recursive`).
        let base_with_slash = if prefix.is_empty() {
            String::new()
        } else {
            format!("{prefix}/")
        };

        // Whether some pathspec reaches strictly below this entry (Git's `show_recursive`, sans
        // the `-r` short-circuit). For `-d` the comparison is dir-prefix based (`-d` may pair with
        // a deeper pathspec; see `only_trees_pathspec_wants_descent`).
        let pathspec_descends = is_tree
            && !args.paths.is_empty()
            && if args.only_trees {
                args.paths
                    .iter()
                    .any(|p| only_trees_pathspec_wants_descent(p, &full_name))
            } else {
                ls_tree_show_recursive(&args.paths, &base_with_slash, &name)
            };

        // Apply path filter (Git pathspec semantics, including `:(exclude)` and literal `[` paths).
        // An entry is "interesting" (Git's `tree_entry_interesting`) when it matches a pathspec as
        // a leaf, or when it is a tree some pathspec descends into.
        if !args.paths.is_empty() {
            let matches = matches_pathspec_set_for_object_ls_tree(
                &args.paths,
                &full_name,
                entry.mode,
                attr_rules,
            );
            if !matches && !pathspec_descends {
                continue;
            }
        }

        // Git's `show_tree_fmt`: descend into a tree when `-r` is set, or when a pathspec reaches
        // below it. A tree that is itself the leaf match (no deeper pathspec) is shown, not walked.
        let recurse = is_tree && !is_submodule && (args.recursive || pathspec_descends);

        if recurse {
            // Git `show_tree_common`: when a tree is recursed into, the intermediate node is
            // emitted only when `LS_SHOW_TREES` is set. That flag comes from `-t` (`show_trees`)
            // or is auto-set for `-d -r` together (see the `only_trees && recursive` step above).
            // Plain `-d` with a descending pathspec (e.g. `dir/`) must NOT print `dir` itself —
            // only entries strictly below it — so don't gate on `only_trees` here.
            if args.show_trees {
                let display_name = make_cwd_relative(&full_name, cwd_prefix);
                print_entry(repo, entry, &display_name, args, out, term, quote_fully)?;
            }
            let sub_obj = repo.odb.read(&entry.oid)?;
            list_tree(
                repo,
                &sub_obj.data,
                &full_name,
                depth + 1,
                max_tree_depth,
                args,
                out,
                term,
                cwd_prefix,
                quote_fully,
                attr_rules,
            )?;
            continue;
        }

        if args.only_trees && is_blob_like_tree_entry(entry.mode) {
            continue;
        }

        let display_name = make_cwd_relative(&full_name, cwd_prefix);
        print_entry(repo, entry, &display_name, args, out, term, quote_fully)?;
    }
    Ok(())
}

fn print_entry(
    repo: &Repository,
    entry: &grit_lib::objects::TreeEntry,
    name: &str,
    args: &Args,
    out: &mut impl Write,
    term: u8,
    quote_fully: bool,
) -> Result<()> {
    let kind_str = match file_type_mask(entry.mode) {
        0o160000 => "commit",
        0o040000 => "tree",
        _ => "blob",
    };

    if let Some(fmt) = &args.format {
        let line = expand_ls_tree_format(repo, fmt, entry, name, kind_str, args, quote_fully)?;
        write!(out, "{line}")?;
    } else if args.name_only {
        if args.null_terminated {
            write!(out, "{name}")?;
        } else {
            write!(
                out,
                "{}",
                grit_lib::quote_path::quote_c_style(name, quote_fully)
            )?;
        }
    } else if args.object_only {
        write!(out, "{}", ls_tree_object_name(repo, entry, args)?)?;
    } else if args.long {
        let size_str = if kind_str == "blob" {
            match repo.odb.read(&entry.oid) {
                Ok(obj) => format!("{:>7}", obj.data.len()),
                Err(_) => "      -".to_string(),
            }
        } else {
            "      -".to_string()
        };
        let path_disp = grit_lib::quote_path::quote_path_for_tree_listing(name, quote_fully);
        write!(
            out,
            "{:06o} {kind_str} {} {size_str}\t{path_disp}",
            entry.mode,
            ls_tree_object_name(repo, entry, args)?
        )?;
    } else {
        let path_disp = grit_lib::quote_path::quote_path_for_tree_listing(name, quote_fully);
        write!(
            out,
            "{:06o} {kind_str} {}\t{path_disp}",
            entry.mode,
            ls_tree_object_name(repo, entry, args)?
        )?;
    }
    out.write_all(&[term])?;
    Ok(())
}

/// Object name for `ls-tree` output: full hex unless `--abbrev`, then a unique prefix (Git:
/// `repo_find_unique_abbrev`).
fn ls_tree_object_name(repo: &Repository, entry: &TreeEntry, args: &Args) -> Result<String> {
    let min_len = ls_tree_abbrev_len(args);
    if min_len >= 40 {
        return Ok(entry.oid.to_hex());
    }
    abbreviate_object_id(repo, entry.oid, min_len).map_err(|e| anyhow::anyhow!("{e}"))
}

fn ls_tree_object_size_field(
    repo: &Repository,
    entry: &TreeEntry,
    kind_str: &str,
    padded: bool,
) -> String {
    if kind_str != "blob" {
        return if padded {
            format!("{:>7}", "-")
        } else {
            "-".to_string()
        };
    }
    match repo.odb.read(&entry.oid) {
        Ok(obj) => {
            if padded {
                format!("{:>7}", obj.data.len())
            } else {
                format!("{}", obj.data.len())
            }
        }
        Err(_) => {
            if padded {
                "      -".to_string()
            } else {
                "-".to_string()
            }
        }
    }
}

/// Expand `git ls-tree --format` placeholders (see `builtin/ls-tree.c` `show_tree_fmt`).
fn expand_ls_tree_format(
    repo: &Repository,
    fmt: &str,
    entry: &TreeEntry,
    display_path: &str,
    kind_str: &str,
    args: &Args,
    quote_fully: bool,
) -> Result<String> {
    let mut out = String::new();
    let bs = fmt.as_bytes();
    let mut i = 0usize;
    while i < bs.len() {
        if bs[i] != b'%' {
            let start = i;
            while i < bs.len() && bs[i] != b'%' {
                i += 1;
            }
            out.push_str(&fmt[start..i]);
            continue;
        }
        i += 1;
        if i >= bs.len() {
            break;
        }
        match bs[i] {
            b'%' => {
                out.push('%');
                i += 1;
            }
            b'n' => {
                out.push('\n');
                i += 1;
            }
            b'x' | b'X' => {
                i += 1;
                let Some(b1) = bs.get(i).copied() else {
                    bail!("bad ls-tree format: incomplete %x escape");
                };
                let Some(b2) = bs.get(i + 1).copied() else {
                    bail!("bad ls-tree format: incomplete %x escape");
                };
                let h1 = (b1 as char).to_digit(16);
                let h2 = (b2 as char).to_digit(16);
                let Some((d1, d2)) = h1.zip(h2) else {
                    bail!("bad ls-tree format: invalid %x escape");
                };
                out.push(char::from_u32(d1 * 16 + d2).unwrap_or('\u{FFFD}'));
                i += 2;
            }
            b'(' => {
                let tail = &fmt[i..];
                let consumed = if tail.starts_with("(objectsize:padded)") {
                    out.push_str(&ls_tree_object_size_field(repo, entry, kind_str, true));
                    "(objectsize:padded)".len()
                } else if tail.starts_with("(objectsize)") {
                    out.push_str(&ls_tree_object_size_field(repo, entry, kind_str, false));
                    "(objectsize)".len()
                } else if tail.starts_with("(objectmode)") {
                    out.push_str(&format!("{:06o}", entry.mode));
                    "(objectmode)".len()
                } else if tail.starts_with("(objecttype)") {
                    out.push_str(kind_str);
                    "(objecttype)".len()
                } else if tail.starts_with("(objectname)") {
                    out.push_str(&ls_tree_object_name(repo, entry, args)?);
                    "(objectname)".len()
                } else if tail.starts_with("(path)") {
                    out.push_str(&quote_path_for_ls_tree_format(
                        display_path,
                        args.null_terminated,
                        quote_fully,
                    ));
                    "(path)".len()
                } else {
                    let end = tail.find(')').map(|p| p + 1).unwrap_or(tail.len());
                    bail!("bad ls-tree format: %{}", &tail[..end]);
                };
                i += consumed;
            }
            _ => bail!("bad ls-tree format: unknown % escape"),
        }
    }
    Ok(out)
}

/// `%(path)` uses C-style quoting like `quote_c_style` when not `-z` (see `show_tree_fmt`).
fn quote_path_for_ls_tree_format(path: &str, null_terminated: bool, quote_fully: bool) -> String {
    if null_terminated {
        path.to_string()
    } else {
        grit_lib::quote_path::quote_c_style(path, quote_fully)
    }
}

fn resolve_tree_ish(repo: &Repository, s: &str) -> Result<ObjectId> {
    if s == EMPTY_TREE_OID {
        return Ok(ObjectId::from_hex(EMPTY_TREE_OID)?);
    }
    // First try the full revision syntax (handles ^, ~, :path, etc.)
    if let Ok(oid) = grit_lib::rev_parse::resolve_revision(repo, s) {
        return Ok(oid);
    }
    // Fallback: try as raw OID
    if let Ok(oid) = s.parse::<ObjectId>() {
        return Ok(oid);
    }
    if let Ok(oid) = resolve_ref(&repo.git_dir, s) {
        return Ok(oid);
    }
    let as_branch = format!("refs/heads/{s}");
    if let Ok(oid) = resolve_ref(&repo.git_dir, &as_branch) {
        return Ok(oid);
    }
    let as_tag = format!("refs/tags/{s}");
    if let Ok(oid) = resolve_ref(&repo.git_dir, &as_tag) {
        return Ok(oid);
    }
    bail!("not a valid tree-ish: '{s}'")
}

fn resolve_max_tree_depth(config: &ConfigSet) -> Result<usize> {
    let depth = if let Some(raw) = config.get("core.maxtreedepth") {
        raw.parse::<usize>()
            .map_err(|_| anyhow::anyhow!("invalid core.maxtreedepth: '{raw}'"))?
    } else {
        DEFAULT_MAX_TREE_DEPTH
    };
    Ok(depth)
}

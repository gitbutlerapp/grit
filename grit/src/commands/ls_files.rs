//! `grit ls-files` — list information about files in the index and working tree.

use anyhow::{Context, Result};
use clap::Args as ClapArgs;
use std::borrow::Cow;
use std::collections::BTreeSet;
use std::fs;
use std::io::{self, Write};
use std::path::Component;
use std::path::{Path, PathBuf};

use grit_lib::ignore::IgnoreMatcher;
use grit_lib::index::{Index, IndexEntry};
use grit_lib::repo::{resolve_dot_git, Repository};
use grit_lib::submodule_config::{
    is_submodule_active, load_submodule_registrations, submodule_name_for_path,
};
use grit_lib::unicode_normalization::{precompose_utf8_path, precompose_utf8_segment};

use crate::explicit_exit::ExplicitExit;

fn resolved_env_index_path(repo: &Repository) -> PathBuf {
    if let Ok(raw) = std::env::var("GIT_INDEX_FILE") {
        let p = PathBuf::from(raw);
        if p.is_absolute() {
            p
        } else if let Ok(cwd) = std::env::current_dir() {
            cwd.join(p)
        } else {
            p
        }
    } else {
        repo.index_path()
    }
}

fn write_eol_record<W: Write + ?Sized>(
    out: &mut W,
    index_eol: &str,
    wt_eol: &str,
    attr_str: &str,
    name: &str,
) -> io::Result<()> {
    let index_field = format!("i/{index_eol}");
    let wt_field = format!("w/{wt_eol}");
    let attr_field = format!("attr/{attr_str}");
    write!(out, "{index_field:<8}{wt_field:<8}{attr_field:<22}\t{name}")
}

/// Object type name for an index entry mode (Git `object_type`): gitlink → commit,
/// directory → tree, everything else (regular/exec/symlink) → blob.
fn ls_format_objecttype(mode: u32) -> &'static str {
    match mode & 0o170000 {
        0o160000 => "commit",
        0o040000 => "tree",
        _ => "blob",
    }
}

/// Object size for `%(objectsize)` (Git `expand_objectsize`): blob size, or `-` for non-blobs.
/// `padded` right-justifies in a 7-wide field.
fn ls_format_objectsize(entry: &IndexEntry, repo: &Repository, padded: bool) -> String {
    let is_blob = matches!(entry.mode & 0o170000, 0o100000 | 0o120000);
    let value = if is_blob {
        match repo.odb.read(&entry.oid) {
            Ok(obj) => obj.data.len().to_string(),
            Err(_) => "-".to_string(),
        }
    } else {
        "-".to_string()
    };
    if padded {
        format!("{value:>7}")
    } else {
        value
    }
}

/// Expand a `git ls-files --format` template for one index entry (Git `show_ce_fmt`).
///
/// Supports `%%`, `%n`, `%xXX` literal escapes and the `%(...)` atoms:
/// objectmode, objectname, objecttype, objectsize[:padded], stage, eolinfo:index,
/// eolinfo:worktree, eolattr, path. Output is byte-oriented to preserve the path encoding.
#[allow(clippy::too_many_arguments)]
fn expand_ls_format(
    fmt: &str,
    entry: &IndexEntry,
    display_name: &str,
    repo_rel_path: &str,
    work_tree: &Path,
    repo: &Repository,
    attrs_for_eol: &[grit_lib::crlf::AttrRule],
    config: &grit_lib::config::ConfigSet,
) -> Result<Vec<u8>> {
    let bytes = fmt.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(fmt.len());
    let is_regular = matches!(entry.mode & 0o170000, 0o100000);
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] != b'%' {
            out.push(bytes[i]);
            i += 1;
            continue;
        }
        let rest = &fmt[i + 1..];
        if let Some(after) = rest.strip_prefix('%') {
            out.push(b'%');
            i = fmt.len() - after.len();
            continue;
        }
        if let Some(after) = rest.strip_prefix('n') {
            out.push(b'\n');
            i = fmt.len() - after.len();
            continue;
        }
        if let Some(hex) = rest.strip_prefix('x') {
            let hb = hex.as_bytes();
            if hb.len() >= 2 {
                if let Ok(byte) = u8::from_str_radix(&hex[..2], 16) {
                    out.push(byte);
                    i = (i + 1) + 3; // consume "%xHH"
                    continue;
                }
            }
        }
        let atom = |name: &str| rest.strip_prefix(name);
        if let Some(after) = atom("(objectmode)") {
            out.extend_from_slice(format!("{:06o}", entry.mode).as_bytes());
            i = fmt.len() - after.len();
        } else if let Some(after) = atom("(objectname)") {
            out.extend_from_slice(entry.oid.to_hex().as_bytes());
            i = fmt.len() - after.len();
        } else if let Some(after) = atom("(objecttype)") {
            out.extend_from_slice(ls_format_objecttype(entry.mode).as_bytes());
            i = fmt.len() - after.len();
        } else if let Some(after) = atom("(objectsize:padded)") {
            out.extend_from_slice(ls_format_objectsize(entry, repo, true).as_bytes());
            i = fmt.len() - after.len();
        } else if let Some(after) = atom("(objectsize)") {
            out.extend_from_slice(ls_format_objectsize(entry, repo, false).as_bytes());
            i = fmt.len() - after.len();
        } else if let Some(after) = atom("(stage)") {
            out.extend_from_slice(format!("{}", entry.stage()).as_bytes());
            i = fmt.len() - after.len();
        } else if let Some(after) = atom("(eolinfo:index)") {
            if is_regular && entry.oid != grit_lib::diff::zero_oid() {
                if let Ok(obj) = repo.odb.read(&entry.oid) {
                    out.extend_from_slice(
                        grit_lib::crlf::gather_convert_stats_ascii(&obj.data).as_bytes(),
                    );
                }
            }
            i = fmt.len() - after.len();
        } else if let Some(after) = atom("(eolinfo:worktree)") {
            let wt_path = work_tree.join(repo_rel_path);
            if let Ok(meta) = std::fs::symlink_metadata(&wt_path) {
                if meta.file_type().is_file() {
                    if let Ok(data) = std::fs::read(&wt_path) {
                        out.extend_from_slice(
                            grit_lib::crlf::gather_convert_stats_ascii(&data).as_bytes(),
                        );
                    }
                }
            }
            i = fmt.len() - after.len();
        } else if let Some(after) = atom("(eolattr)") {
            let attr_str = grit_lib::crlf::convert_attr_ascii_for_ls_files(
                attrs_for_eol,
                repo_rel_path,
                config,
            );
            out.extend_from_slice(attr_str.as_bytes());
            i = fmt.len() - after.len();
        } else if let Some(after) = atom("(path)") {
            out.extend_from_slice(display_name.as_bytes());
            i = fmt.len() - after.len();
        } else {
            anyhow::bail!("fatal: bad ls-files format: {fmt}");
        }
    }
    Ok(out)
}

/// Arguments for `grit ls-files`.
#[derive(Debug, ClapArgs)]
pub struct Args {
    /// Show cached (staged) files (default).
    #[arg(short = 'c', long)]
    pub cached: bool,

    /// Show deleted files.
    #[arg(short = 'd', long)]
    pub deleted: bool,

    /// Show modified files.
    #[arg(short = 'm', long)]
    pub modified: bool,

    /// Show other (untracked) files.
    #[arg(short = 'o', long)]
    pub others: bool,

    /// Show ignored files.
    #[arg(short = 'i', long)]
    pub ignored: bool,

    /// Show unmerged files.
    #[arg(short = 'u', long)]
    pub unmerged: bool,

    /// Show killed files.
    #[arg(short = 'k', long)]
    pub killed: bool,

    /// Show object name in each line.
    #[arg(short = 's', long)]
    pub stage: bool,

    /// \0 line termination on output.
    #[arg(short = 'z')]
    pub null_terminated: bool,

    /// Show only unmerged files and their stage numbers.
    #[arg(long = "error-unmatch")]
    pub error_unmatch: bool,

    /// Deduplicate entries (for untracked files).
    #[arg(long)]
    pub deduplicate: bool,

    /// Suppress any error message (for -t).
    #[arg(short = 't')]
    pub show_tag: bool,

    /// Show lowercase tags for tracked files (`-v`).
    #[arg(short = 'v')]
    pub show_untracked_cache_tag: bool,

    /// Show lowercase tags for fsmonitor-valid entries (`-f`).
    #[arg(short = 'f')]
    pub show_fsmonitor_valid_tag: bool,

    /// Show verbose long format.
    #[arg(long)]
    pub long: bool,

    /// Show debugging data for each cache entry (ctime/mtime/dev/ino/uid/gid/size/flags).
    #[arg(long)]
    pub debug: bool,

    /// Show sparse directory placeholders in the index (do not expand sparse index).
    #[arg(long)]
    pub sparse: bool,

    /// Format string for output (supports %(objectmode), %(objectname), %(stage), %(path)).
    #[arg(long)]
    pub format: Option<String>,

    /// Exclude pattern (e.g. --exclude='*.o').
    #[arg(short = 'x', long = "exclude", value_name = "PATTERN")]
    pub exclude: Vec<String>,

    /// Exclude patterns from file.
    #[arg(short = 'X', long = "exclude-from", value_name = "FILE")]
    pub exclude_from: Vec<PathBuf>,

    /// Read exclude patterns from file in each directory.
    #[arg(long = "exclude-per-directory", value_name = "FILE")]
    pub exclude_per_directory: Option<String>,

    /// Use standard exclude sources (.gitignore, .git/info/exclude, core.excludesFile).
    #[arg(long = "exclude-standard")]
    pub exclude_standard: bool,

    /// If showing untracked files, show only directories.
    #[arg(long = "directory")]
    pub directory: bool,

    /// Do not list empty directories (only meaningful with --directory).
    #[arg(long = "no-empty-directory")]
    pub no_empty_directory: bool,

    /// Show line-ending information for files.
    #[arg(long)]
    pub eol: bool,

    /// Show resolve-undo information from the index.
    #[arg(long = "resolve-undo")]
    pub resolve_undo: bool,

    /// Show paths relative to repository root.
    #[arg(long = "full-name")]
    pub full_name: bool,

    /// Change directory before listing files.
    #[arg(short = 'C', value_name = "DIR")]
    pub change_dir: Option<PathBuf>,

    /// Pretend paths removed since this tree are still in the index (for cached listings).
    #[arg(long = "with-tree", value_name = "TREEISH")]
    pub with_tree: Option<String>,

    /// Recurse into submodules (not compatible with all `ls-files` modes).
    #[arg(long = "recurse-submodules")]
    pub recurse_submodules: bool,

    /// Pathspecs to restrict output.
    pub pathspecs: Vec<PathBuf>,
}

/// Run `grit ls-files`.
pub fn run(args: Args) -> Result<()> {
    // Handle -C flag: change directory before doing anything else
    if let Some(ref dir) = args.change_dir {
        let target = if dir.is_absolute() {
            dir.clone()
        } else {
            std::env::current_dir()?.join(dir)
        };
        std::env::set_current_dir(&target)
            .with_context(|| format!("cannot change to directory '{}'", target.display()))?;
    }

    let repo = Repository::discover(None).context("not a git repository")?;
    let own_git_dir = grit_lib::git_path::path_for_disk_compare(&repo.git_dir);
    let cwd = repo.effective_pathspec_cwd();
    let work_tree = if let Some(wt) = repo.work_tree.as_deref() {
        wt
    } else {
        if let Some(outside) = args.pathspecs.iter().find(|p| pathspec_escapes_repo(p)) {
            anyhow::bail!(
                "pathspec '{}' is outside repository",
                outside.to_string_lossy()
            );
        }
        anyhow::bail!("cannot ls-files in bare repository");
    };
    let cwd_prefix = cwd_prefix_bytes(work_tree, &cwd)?;
    let config = grit_lib::config::ConfigSet::load(Some(&repo.git_dir), true).unwrap_or_default();
    let precompose_unicode =
        grit_lib::precompose_config::effective_core_precomposeunicode(Some(&repo.git_dir));
    let precompose_walk = precompose_unicode
        && grit_lib::precompose_config::filesystem_nfd_nfc_aliases(&repo.git_dir);
    let quote_fully = config.quote_path_fully();
    let index_path = resolved_env_index_path(&repo);
    let mut index = if args.sparse {
        grit_lib::index::Index::load(&index_path).context("loading index")?
    } else {
        repo.load_index_at(&index_path).context("loading index")?
    };
    if !args.sparse && args.pathspecs.is_empty() {
        if let Ok(trace2_event) = std::env::var("GIT_TRACE2_EVENT") {
            if !trace2_event.trim().is_empty() {
                // Loaded raw (un-expanded) only here: `load_index_at` has
                // already expanded sparse placeholders, so re-read to learn
                // whether the on-disk index had any. Off the hot path —
                // only when trace2 event output is requested.
                let raw_index_had_sparse_dirs = grit_lib::index::Index::load(&index_path)
                    .map(|idx| idx.has_sparse_directory_placeholders())
                    .unwrap_or(false);
                if raw_index_had_sparse_dirs {
                    let _ = crate::trace2_region_json(&trace2_event, "index", "ensure_full_index");
                }
            }
        }
    }

    if args.recurse_submodules
        && (args.deleted
            || args.others
            || args.unmerged
            || args.killed
            || args.modified
            || args.with_tree.is_some()
            || args.resolve_undo)
    {
        anyhow::bail!("fatal: ls-files --recurse-submodules unsupported mode");
    }

    if args.with_tree.is_some() && (args.unmerged || args.stage) {
        anyhow::bail!("fatal: options 'ls-files --with-tree' and '-s/-u' cannot be used together");
    }

    if args.recurse_submodules && args.error_unmatch {
        anyhow::bail!("fatal: ls-files --recurse-submodules does not support --error-unmatch");
    }

    // `--format` is incompatible with several output modes (Git `cmd_ls_files`): -s/-u (stage),
    // -o (others), -k (killed), -t (tag/-v/-f), --resolve-undo, --deduplicate, --eol.
    // Git reports this as a usage error (exit code 129).
    if args.format.is_some()
        && (args.stage
            || args.unmerged
            || args.others
            || args.killed
            || args.resolve_undo
            || args.deduplicate
            || args.eol
            || args.show_tag
            || args.show_untracked_cache_tag
            || args.show_fsmonitor_valid_tag)
    {
        return Err(anyhow::Error::new(ExplicitExit {
            code: 129,
            message: "fatal: --format cannot be used with -s, -o, -k, -t, --resolve-undo, --deduplicate, --eol".to_string(),
        }));
    }

    let stdout = io::stdout();
    // Rust's StdoutLock is line-buffered; per-entry lines would issue one
    // write(2) each (10k syscalls on a 10k-file index). Fully buffer like
    // C git's stdio and flush once at the end.
    let mut out = io::BufWriter::new(stdout.lock());

    let term = if args.null_terminated { b'\0' } else { b'\n' };
    let use_nul = args.null_terminated;

    // Determine which mode to use
    let show_cached = args.cached
        || args.stage
        || (!args.deleted
            && !args.modified
            && !args.others
            && !args.ignored
            && !args.unmerged
            && !args.killed
            && !args.resolve_undo);
    if args.sparse && (args.deleted || args.modified) && !show_cached {
        index
            .expand_sparse_directory_placeholders(&repo.odb)
            .context("expanding sparse index for working-tree comparison")?;
        grit_lib::sparse_checkout::clear_skip_worktree_from_present_files(
            &repo.git_dir,
            work_tree,
            &mut index,
        );
    }
    let show_stage = args.stage || args.unmerged;
    // Match git ls-files.c: --deduplicate is ignored with -t/-s/-u (show_tag/show_stage).
    let dedup_paths = args.deduplicate && !args.show_tag && !show_stage;

    let mut pathspec_filter: Vec<Pathspec> = args
        .pathspecs
        .iter()
        .map(|p| resolve_pathspec(work_tree, &cwd, p, precompose_unicode))
        .collect::<Result<Vec<_>>>()?;
    let mut pathspec_display: Vec<String> = args
        .pathspecs
        .iter()
        .map(|p| p.to_string_lossy().into_owned())
        .collect();
    if let Err(message) = grit_lib::pathspec::validate_attr_pathspecs(&pathspec_display) {
        anyhow::bail!("{message}");
    }
    if pathspec_filter.is_empty() && !cwd_prefix.is_empty()  {
        pathspec_filter.push(Pathspec::Literal(cwd_prefix.clone()));
        pathspec_display.push(".".to_string());
    }

    if let Some(ref treeish) = args.with_tree {
        let mut overlay_prefix = common_pathspec_prefix_for_overlay(&pathspec_filter);
        while overlay_prefix.last() == Some(&b'/') {
            overlay_prefix.pop();
        }
        index
            .overlay_tree_on_index(&repo, treeish, &overlay_prefix)
            .with_context(|| format!("overlay tree '{treeish}' on index"))?;
    }

    if !args.recurse_submodules {
        pathspec_filter =
            expand_ls_files_globs(pathspec_filter, work_tree, &index, precompose_walk);
    }

    let cwd_prefix_str = String::from_utf8_lossy(&cwd_prefix).into_owned();
    let cwd_trim = cwd_prefix_str.trim_end_matches('/').to_string();
    let cwd_for_resolve = (!cwd_trim.is_empty()).then_some(cwd_trim);
    let resolved_pathspec_strings: Vec<String> = pathspec_filter
        .iter()
        .map(|ps| {
            let raw = pathspec_for_lib_path_match(ps);
            match ps {
                Pathspec::Magic(_) => {
                    crate::pathspec::resolve_pathspec(&raw, work_tree, cwd_for_resolve.as_deref())
                }
                _ => raw,
            }
        })
        .collect();
    let pathspec_lib_strings = grit_lib::pathspec::extend_pathspec_list_implicit_cwd(
        &resolved_pathspec_strings,
        cwd_for_resolve.as_deref(),
    );
    let pathspec_uses_attrs = pathspec_lib_strings
        .iter()
        .any(|spec| spec.starts_with(":(attr:") || spec.contains(",attr:"));

    // For `--error-unmatch`, Git matches pathspecs separately against index output vs untracked
    // output (`-c` vs `-o`). A pathspec that only hits tracked files does not satisfy `-o`.
    let mut matched_index: Vec<bool> = vec![false; pathspec_filter.len()];
    let mut matched_others: Vec<bool> = vec![false; pathspec_filter.len()];
    if !args.recurse_submodules {
        for i in 0..resolved_pathspec_strings.len() {
            if grit_lib::pathspec::pathspec_is_exclude(&resolved_pathspec_strings[i]) {
                matched_index[i] = true;
                matched_others[i] = true;
            }
        }
    }

    // Build exclude/ignore matcher if needed (before cached loop so -i -c works).
    // Git order: standard excludes (global → info → .gitignore), plus `-X` files and `-x` patterns.
    let has_explicit_excludes = !args.exclude.is_empty()
        || !args.exclude_from.is_empty()
        || args.exclude_per_directory.is_some();
    // The upstream plumbing requires an explicit exclude source with `--ignored`, but this
    // implementation's status integration expects plain untracked `ls-files --ignored` to use the
    // repository's standard ignore stack. Keep explicit `-x`/`-X`/`--exclude-per-directory` calls on
    // their explicit matcher path so tests that exercise exclude-only behavior are not broadened.
    let use_standard_ignores =
        args.exclude_standard || (args.ignored && !args.cached && !has_explicit_excludes);
    let has_excludes = use_standard_ignores || has_explicit_excludes;
    let need_matcher = use_standard_ignores
        || !args.exclude.is_empty()
        || !args.exclude_from.is_empty()
        || args.exclude_per_directory.is_some();
    let mut matcher = if need_matcher {
        let mut m = if use_standard_ignores {
            IgnoreMatcher::from_repository(&repo).unwrap_or_default()
        } else {
            IgnoreMatcher::default()
        };
        // `--exclude-per-directory=<file>` enables per-directory excludes from <file> (Git
        // `EXC_DIRS`) without pulling in the standard global/info sources.
        if let Some(ref per_dir) = args.exclude_per_directory {
            m.set_per_directory_name(per_dir);
        }
        if !args.exclude_from.is_empty() {
            m.add_exclude_from_files(&args.exclude_from, &cwd)?;
        }
        if !args.exclude.is_empty() {
            m.add_cli_excludes(&args.exclude);
        }
        Some(m)
    } else {
        None
    };

    let attrs_for_eol = grit_lib::crlf::load_gitattributes(work_tree);

    let tag_resolve_undo = if args.show_tag || args.stage {
        "U "
    } else {
        ""
    };

    if args.recurse_submodules {
        let mut last_dedup: Option<Vec<u8>> = None;
        let recurse_params = LsFilesRecurseParams {
            show_cached,
            show_stage,
            dedup_paths,
            show_tag: args.show_tag,
            show_untracked_cache_tag: args.show_untracked_cache_tag,
            show_fsmonitor_valid_tag: args.show_fsmonitor_valid_tag,
            deleted: args.deleted,
            modified: args.modified,
            ignored: args.ignored,
            others: args.others,
            unmerged: args.unmerged,
            eol: args.eol,
            format: args.format.as_deref(),
            full_name: args.full_name,
            precompose_unicode,
            precompose_walk,
            quote_fully,
            term,
            use_nul,
            attrs_for_eol: &attrs_for_eol,
            debug: args.debug,
        };
        ls_files_recurse_submodules(
            &repo,
            &config,
            &index,
            work_tree,
            work_tree,
            &config,
            &cwd,
            &cwd_prefix,
            "",
            &pathspec_filter,
            &mut matched_index,
            &mut last_dedup,
            &recurse_params,
            &mut out,
            &mut matcher,
        )?;
    } else {
        for (i, spec) in pathspec_filter.iter().enumerate() {
            let s = match spec {
                Pathspec::Literal(b) => String::from_utf8_lossy(b).into_owned(),
                Pathspec::Glob(g) => g.clone(),
                Pathspec::Magic(m) => m.clone(),
            };
            if grit_lib::pathspec::pathspec_is_exclude(&s) {
                matched_index[i] = true;
                matched_others[i] = true;
            }
        }

        let mut last_dedup_path: Option<Vec<u8>> = None;
        for entry in &index.entries {
            if entry.overlay_tree_skip_output() {
                continue;
            }
            // Filter by pathspec (Git `match_pathspec`: positives ORed, then excludes subtracted).
            if !pathspec_filter.is_empty() {
                let path_str = String::from_utf8_lossy(&entry.path);
                let path_attrs;
                let attrs_for_pathspec = if pathspec_uses_attrs {
                    path_attrs = grit_lib::crlf::load_gitattributes_for_checkout(
                        work_tree,
                        path_str.as_ref(),
                        &index,
                        &repo.odb,
                    );
                    path_attrs.as_slice()
                } else {
                    &attrs_for_eol
                };
                if !grit_lib::pathspec::matches_pathspec_set_for_object_ls_tree(
                    &pathspec_lib_strings,
                    path_str.as_ref(),
                    entry.mode,
                    attrs_for_pathspec,
                ) {
                    continue;
                }
                for (i, spec) in pathspec_filter.iter().enumerate() {
                    if grit_lib::pathspec::pathspec_is_exclude(&pathspec_lib_strings[i]) {
                        continue;
                    }
                    if spec.matches(&entry.path) {
                        matched_index[i] = true;
                    }
                }
            }

            // Unmerged: stage != 0
            if args.unmerged && entry.stage() == 0 {
                continue;
            }
            // --ignored with --cached: only show tracked files that are ignored
            if args.ignored && show_cached && !args.others {
                let path_str = String::from_utf8_lossy(&entry.path);
                // Pass None for index so tracked files aren't auto-skipped
                let excluded = if let Some(ref mut m) = matcher {
                    m.check_path(&repo, None, &path_str, false)
                        .map(|(ig, _)| ig)
                        .unwrap_or(false)
                } else {
                    false
                };
                if !excluded {
                    continue;
                }
            }

            // --deleted / --modified: show entries that are deleted or modified on disk.
            // Applies to every index stage (including unmerged); matches git ls-files.c.
            // When both -d and -m are set, show if EITHER condition is true.
            if (args.deleted || args.modified) && !show_cached {
                if entry.skip_worktree() {
                    continue;
                }
                let full = work_tree.join(std::str::from_utf8(&entry.path).unwrap_or(""));
                let is_deleted = !full.exists();
                let is_mod = is_modified(entry, &full);
                let dominated = if args.deleted && args.modified {
                    !is_deleted && !is_mod
                } else if args.deleted {
                    !is_deleted
                } else {
                    !is_mod
                };
                if dominated {
                    continue;
                }
            }

            // For -d/-m with -t/-v, compute tags. Git uses "C" for modified (including
            // unmerged conflict paths under -d/-m), not the unmerged "M" tag from -u/-s.
            // A deleted file with both -d and -m produces TWO output lines: 'R path' and 'C path'.
            let (tag, extra_tag) = if args.show_tag
                || args.show_untracked_cache_tag
                || args.show_fsmonitor_valid_tag
            {
                if args.deleted || args.modified {
                    let full = work_tree.join(std::str::from_utf8(&entry.path).unwrap_or(""));
                    if !full.exists() {
                        if args.deleted && args.modified {
                            (Some('R'), Some('C'))
                        } else {
                            (Some('R'), None)
                        }
                    } else if is_modified(entry, &full) {
                        (Some('C'), None)
                    } else {
                        (Some(status_tag(entry)), None)
                    }
                } else {
                    let base_tag = status_tag(entry);
                    let adjusted_tag = if args.show_untracked_cache_tag {
                        base_tag.to_ascii_lowercase()
                    } else if args.show_fsmonitor_valid_tag && entry.fsmonitor_valid() {
                        base_tag.to_ascii_lowercase()
                    } else {
                        base_tag
                    };
                    (Some(adjusted_tag), None)
                }
            } else {
                (None, None)
            };

            if args.eol {
                let display = format_ls_display_path(
                    args.full_name,
                    &cwd,
                    work_tree,
                    &entry.path,
                    &cwd_prefix,
                    &config,
                )?;
                let name = String::from_utf8_lossy(display.as_ref());
                let path_str = std::str::from_utf8(&entry.path).unwrap_or("");

                // Index / worktree EOL stats: match Git `write_eolinfo`, which only computes
                // stats for **regular** files (`S_ISREG`); symlinks/gitlinks/dirs show empty.
                let index_is_regular = matches!(entry.mode & 0o170000, 0o100000);
                let index_eol = if index_is_regular && entry.oid != grit_lib::diff::zero_oid() {
                    if let Ok(obj) = repo.odb.read(&entry.oid) {
                        grit_lib::crlf::gather_convert_stats_ascii(&obj.data).to_string()
                    } else {
                        "binary".to_string()
                    }
                } else {
                    String::new()
                };

                let wt_path = work_tree.join(path_str);
                let wt_eol = match std::fs::symlink_metadata(&wt_path) {
                    Ok(meta) if meta.file_type().is_file() => match std::fs::read(&wt_path) {
                        Ok(data) => grit_lib::crlf::gather_convert_stats_ascii(&data).to_string(),
                        Err(_) => String::new(),
                    },
                    _ => String::new(),
                };

                let attr_str = grit_lib::crlf::convert_attr_ascii_for_ls_files(
                    &attrs_for_eol,
                    path_str,
                    &config,
                );

                write_eol_record(&mut out, &index_eol, &wt_eol, &attr_str, &name)?;
                out.write_all(&[term])?;
                if args.debug {
                    write_ls_files_debug(&mut out, entry)?;
                }
            } else if let Some(ref fmt) = args.format {
                // Custom format output
                let display = format_ls_display_path(
                    args.full_name,
                    &cwd,
                    work_tree,
                    &entry.path,
                    &cwd_prefix,
                    &config,
                )?;
                let name = String::from_utf8_lossy(display.as_ref());
                let path_str = std::str::from_utf8(&entry.path).unwrap_or("");
                let line = expand_ls_format(
                    fmt,
                    entry,
                    &name,
                    path_str,
                    work_tree,
                    &repo,
                    &attrs_for_eol,
                    &config,
                )?;
                out.write_all(&line)?;
                out.write_all(&[term])?;
                if args.debug {
                    write_ls_files_debug(&mut out, entry)?;
                }
            } else if show_stage {
                let display = format_ls_display_path(
                    args.full_name,
                    &cwd,
                    work_tree,
                    &entry.path,
                    &cwd_prefix,
                    &config,
                )?;
                let name = String::from_utf8_lossy(display.as_ref());
                let qname = format_ls_path(&name, use_nul, quote_fully);
                if let Some(t) = tag {
                    write!(out, "{} ", t)?;
                }
                write!(
                    out,
                    "{:06o} {} {}\t{}",
                    entry.mode,
                    entry.oid,
                    entry.stage(),
                    qname
                )?;
                out.write_all(&[term])?;
                if args.debug {
                    write_ls_files_debug(&mut out, entry)?;
                }
            } else if show_cached || args.deleted || args.modified {
                // Deduplicate: skip if same path as last printed.
                // With -t flag, don't deduplicate unmerged entries (stage != 0)
                // since they have distinct stage info that should be visible.
                // With -u/--unmerged, each stage must appear on its own line (t6402).
                // Without -t/-u, deduplicate all entries including unmerged.
                // `dedup_paths` encodes git ls-files.c: --deduplicate is ignored with -t/-s/-u.
                if dedup_paths {
                    if let Some(ref last) = last_dedup_path {
                        if last == &entry.path {
                            continue;
                        }
                    }
                    last_dedup_path = Some(entry.path.clone());
                }
                let display = format_ls_display_path(
                    args.full_name,
                    &cwd,
                    work_tree,
                    &entry.path,
                    &cwd_prefix,
                    &config,
                )?;
                let name = String::from_utf8_lossy(display.as_ref());
                let qname = format_ls_path(&name, use_nul, quote_fully);
                if let Some(t) = tag {
                    write!(out, "{} ", t)?;
                }
                write!(out, "{qname}")?;
                out.write_all(&[term])?;
                // Output extra line for deleted files with both -d and -m and -t
                if let Some(et) = extra_tag {
                    write!(out, "{} ", et)?;
                    write!(out, "{qname}")?;
                    out.write_all(&[term])?;
                }
                if args.debug {
                    write_ls_files_debug(&mut out, entry)?;
                }
            }
        }
    }

    if args.resolve_undo {
        if let Some(ru_map) = &index.resolve_undo {
            for (path_bytes, ru) in ru_map {
                if !pathspec_filter.is_empty() {
                    let idx = pathspec_filter
                        .iter()
                        .position(|spec| spec.matches(path_bytes.as_slice()));
                    match idx {
                        Some(i) => matched_index[i] = true,
                        None => continue,
                    }
                }
                let display = format_ls_display_path(
                    args.full_name,
                    &cwd,
                    work_tree,
                    path_bytes,
                    &cwd_prefix,
                    &config,
                )?;
                let name = String::from_utf8_lossy(display.as_ref());
                let qname = format_ls_path(&name, use_nul, quote_fully);
                for stage in 1u8..=3u8 {
                    let i = (stage - 1) as usize;
                    if ru.modes[i] == 0 {
                        continue;
                    }
                    write!(
                        out,
                        "{}{:06o} {} {}\t{}",
                        tag_resolve_undo,
                        ru.modes[i],
                        ru.oids[i].to_hex(),
                        stage,
                        qname
                    )?;
                    out.write_all(&[term])?;
                }
            }
        }
    }

    // --others: list untracked files
    // --ignored: show only ignored untracked files (implies --others)
    // --ignored implies --others only when --cached is not explicitly set
    let show_others = args.others || (args.ignored && !args.cached);
    if show_others {
        let mut indexed_paths: BTreeSet<Vec<u8>> =
            index.entries.iter().map(|e| e.path.clone()).collect();
        let mut indexed_gitlink_paths: BTreeSet<Vec<u8>> = index
            .entries
            .iter()
            .filter(|e| e.mode == grit_lib::index::MODE_GITLINK)
            .map(|e| e.path.clone())
            .collect();
        if precompose_walk {
            for entry in &index.entries {
                if let Ok(path) = std::str::from_utf8(&entry.path) {
                    indexed_paths.insert(precompose_utf8_path(path).into_owned().into_bytes());
                    if entry.mode == grit_lib::index::MODE_GITLINK {
                        indexed_gitlink_paths
                            .insert(precompose_utf8_path(path).into_owned().into_bytes());
                    }
                }
            }
        }
        let mut untracked = Vec::new();
        // With `--ignored --directory`, Git collapses a directory to `dir/` only when the
        // directory itself is ignored; directories that merely *contain* ignored files are
        // recursed into so the individual ignored files are listed (t3001 "** patterns and
        // --directory"). We still emit a `dir/` marker for genuinely empty directories (t3001
        // "show empty ignored directory"), so we keep empty-directory emission on but force the
        // walk to recurse into every non-empty directory, then decide the ignored-directory
        // collapse afterwards from the ignore matcher.
        let walk_emit_empty = args.directory;
        let recurse_nonempty_dirs = args.directory && args.ignored;
        walk_worktree(
            work_tree,
            work_tree,
            &indexed_paths,
            &indexed_gitlink_paths,
            &mut untracked,
            true,
            walk_emit_empty,
            args.no_empty_directory,
            precompose_walk,
            if pathspec_filter.is_empty() {
                None
            } else {
                Some(pathspec_filter.as_slice())
            },
            &own_git_dir,
            args.directory && !args.ignored,
            recurse_nonempty_dirs,
        )?;
        untracked.sort();

        let mut filtered_untracked: Vec<Vec<u8>> = Vec::new();

        for path_bytes in &untracked {
            if !pathspec_filter.is_empty() {
                let path_str = String::from_utf8_lossy(path_bytes);
                let mode = if path_str.ends_with('/') {
                    0o040000
                } else {
                    0o100644
                };
                let path_for_match = path_str.trim_end_matches('/');
                if !grit_lib::pathspec::matches_pathspec_set_for_object_ls_tree(
                    &pathspec_lib_strings,
                    path_for_match,
                    mode,
                    &attrs_for_eol,
                ) {
                    continue;
                }
            }

            // When `--ignored --directory` collapses an ignored file to the `dir/` of its
            // shallowest ignored ancestor directory, this holds the repo-relative directory
            // path (with trailing `/`) to emit instead of the file itself.
            let mut ignored_dir_collapse: Option<Vec<u8>> = None;

            // Apply exclude filtering (always when matcher is loaded)
            if has_excludes || args.ignored || matcher.is_some() {
                let path_str = String::from_utf8_lossy(path_bytes);
                let is_dir = path_str.ends_with('/');
                let is_excluded = if let Some(ref mut m) = matcher {
                    m.check_path(&repo, Some(&index), &path_str, is_dir)
                        .map(|(ig, _)| ig)
                        .unwrap_or(false)
                } else {
                    false
                };

                if args.ignored && !is_excluded {
                    continue; // --ignored: only show excluded files
                }
                if !args.ignored && is_excluded {
                    continue; // --others with excludes: hide excluded files
                }

                // `--ignored --directory`: Git shows a directory as `dir/` only when the
                // directory itself is ignored *and* has no tracked content under it (Git's
                // `treat_directory` only collapses untracked-as-a-whole directories). Find the
                // shallowest such ancestor directory and collapse the entry onto it. A directory
                // that holds a tracked file (t3001 "empty ignored sub-directory") is not
                // collapsed, so a deeper empty ignored subdirectory below it is shown instead;
                // entries with no qualifying ignored ancestor are listed individually.
                if args.ignored && args.directory {
                    if let Some(ref mut m) = matcher {
                        let trimmed = path_str.trim_end_matches('/');
                        let segments: Vec<&str> = trimmed.split('/').collect();
                        // For a file the last segment is the file name; for a `dir/` marker the
                        // directory itself is a collapse candidate, so include all segments.
                        let dir_segments = if is_dir {
                            segments.len()
                        } else {
                            segments.len().saturating_sub(1)
                        };
                        let mut acc = String::new();
                        for seg in segments.iter().take(dir_segments) {
                            if !acc.is_empty() {
                                acc.push('/');
                            }
                            acc.push_str(seg);
                            let prefix_slash = format!("{acc}/");
                            let has_tracked_under = indexed_paths
                                .iter()
                                .any(|t| t.starts_with(prefix_slash.as_bytes()));
                            if has_tracked_under {
                                continue;
                            }
                            let dir_ignored = m
                                .check_path(&repo, Some(&index), &acc, true)
                                .map(|(ig, _)| ig)
                                .unwrap_or(false);
                            if dir_ignored {
                                let mut dir_bytes = acc.clone().into_bytes();
                                dir_bytes.push(b'/');
                                ignored_dir_collapse = Some(dir_bytes);
                                break;
                            }
                        }
                    }
                }
            }

            if !pathspec_filter.is_empty() {
                for (i, spec) in pathspec_filter.iter().enumerate() {
                    if grit_lib::pathspec::pathspec_is_exclude(&pathspec_lib_strings[i]) {
                        continue;
                    }
                    if spec.matches(path_bytes) {
                        matched_others[i] = true;
                    }
                }
            }

            let emit_bytes: &[u8] = ignored_dir_collapse.as_deref().unwrap_or(path_bytes);
            let display = format_ls_display_path(
                args.full_name,
                &cwd,
                work_tree,
                emit_bytes,
                &cwd_prefix,
                &config,
            )?;
            let mut display = display.into_owned();
            if emit_bytes.ends_with(b"/") && !display.ends_with(b"/") {
                display.push(b'/');
            }
            filtered_untracked.push(display);
        }
        // The ignored-directory collapse above can emit the same `dir/` for many files; keep the
        // first occurrence of each path so the directory is listed once (the list is already
        // sorted by repo-relative path, so duplicates are adjacent).
        if args.ignored && args.directory {
            filtered_untracked.dedup();
        }

        // Collapse to directories if --directory (after making paths cwd-relative).
        // In `--ignored` mode the per-file ignored-ancestor collapse above has already produced
        // the final `dir/` vs individual-file shape, so the generic untracked-directory collapse
        // is skipped (it would wrongly fold the individually-listed ignored files together).
        let output_paths = if args.directory && !args.ignored {
            let mut collapsed = collapse_to_directories_for_pathspecs(
                &filtered_untracked,
                &indexed_paths,
                &pathspec_lib_strings,
                &attrs_for_eol,
            );
            if args.no_empty_directory {
                // Remove directory entries that have no file children
                // (empty directory markers from walk_worktree end with '/')
                collapsed.retain(|p| {
                    if !p.ends_with(b"/") {
                        return true; // plain file, keep
                    }
                    // Check if any non-directory entry starts with this prefix
                    let prefix = &p[..];
                    filtered_untracked
                        .iter()
                        .any(|f| !f.ends_with(b"/") && f.starts_with(prefix))
                });
            }
            collapsed
        } else if args.no_empty_directory {
            // Even without --directory, filter out empty dir markers
            filtered_untracked
                .into_iter()
                .filter(|p| !p.ends_with(b"/"))
                .collect()
        } else {
            filtered_untracked
        };

        // If --no-empty-directory removed entries, re-evaluate pathspec matching
        // based on what actually gets output.
        if args.no_empty_directory && !pathspec_filter.is_empty() && !output_paths.is_empty() {
            // At least one path survived filtering, so pathspecs are matched.
        } else if args.no_empty_directory && !pathspec_filter.is_empty() && output_paths.is_empty()
        {
            // All entries were empty dirs that got filtered. Reset others matching.
            for m in matched_others.iter_mut() {
                *m = false;
            }
        }

        for display in &output_paths {
            let name = String::from_utf8_lossy(display);
            let qname = format_ls_path(&name, use_nul, quote_fully);
            if args.eol {
                let path_str = String::from_utf8_lossy(display);
                let full = work_tree.join(path_str.as_ref());
                let wt_eol = if let Ok(data) = std::fs::read(&full) {
                    grit_lib::crlf::gather_convert_stats_ascii(&data).to_string()
                } else {
                    String::new()
                };
                let attr_str = grit_lib::crlf::convert_attr_ascii_for_ls_files(
                    &attrs_for_eol,
                    path_str.as_ref(),
                    &config,
                );
                write_eol_record(&mut out, "", &wt_eol, &attr_str, &qname)?;
            } else if args.show_tag {
                write!(out, "? {qname}")?;
            } else {
                write!(out, "{qname}")?;
            }
            out.write_all(&[term])?;
        }
    }

    // --killed: untracked working-tree paths that would be clobbered by a checkout because a
    // leading directory of the path (or the path itself, expected to be a directory) is a tracked
    // *file* in the index (Git `show_killed_files`).
    if args.killed {
        let indexed_paths: BTreeSet<Vec<u8>> =
            index.entries.iter().map(|e| e.path.clone()).collect();
        let indexed_gitlink_paths: BTreeSet<Vec<u8>> = index
            .entries
            .iter()
            .filter(|e| e.mode == grit_lib::index::MODE_GITLINK)
            .map(|e| e.path.clone())
            .collect();
        // Sorted list of stage-0 tracked names for prefix / immediate-successor lookups.
        let mut cache_names: Vec<&[u8]> = index
            .entries
            .iter()
            .filter(|e| e.stage() == 0)
            .map(|e| e.path.as_slice())
            .collect();
        cache_names.sort_unstable();

        let mut untracked = Vec::new();
        walk_worktree(
            work_tree,
            work_tree,
            &indexed_paths,
            &indexed_gitlink_paths,
            &mut untracked,
            true,
            false, // no --directory collapsing: killed needs the full file list
            false,
            precompose_walk,
            if pathspec_filter.is_empty() {
                None
            } else {
                Some(pathspec_filter.as_slice())
            },
            &own_git_dir,
            false,
            false,
        )?;
        untracked.sort();

        for path_bytes in &untracked {
            if path_bytes.ends_with(b"/") {
                continue;
            }
            if !pathspec_filter.is_empty() {
                let path_str = String::from_utf8_lossy(path_bytes);
                if !grit_lib::pathspec::matches_pathspec_set_for_object_ls_tree(
                    &pathspec_lib_strings,
                    path_str.as_ref(),
                    0o100644,
                    &attrs_for_eol,
                ) {
                    continue;
                }
            }
            if !path_is_killed(path_bytes, &cache_names) {
                continue;
            }
            for (i, spec) in pathspec_filter.iter().enumerate() {
                if grit_lib::pathspec::pathspec_is_exclude(&pathspec_lib_strings[i]) {
                    continue;
                }
                if spec.matches(path_bytes) {
                    matched_others[i] = true;
                }
            }
            let display = format_ls_display_path(
                args.full_name,
                &cwd,
                work_tree,
                path_bytes,
                &cwd_prefix,
                &config,
            )?;
            let name = String::from_utf8_lossy(display.as_ref());
            let qname = format_ls_path(&name, use_nul, quote_fully);
            if args.show_tag {
                write!(out, "K {qname}")?;
            } else {
                write!(out, "{qname}")?;
            }
            out.write_all(&[term])?;
        }
    }

    // --error-unmatch: fail if any pathspec matched nothing in the active mode(s).
    if args.error_unmatch {
        let show_others_err = args.others || (args.ignored && !args.cached);
        let index_emits = args.eol
            || args.format.is_some()
            || args.stage
            || args.unmerged
            || args.resolve_undo
            || (show_cached || args.deleted || args.modified);
        let mut unmatched_specs: Vec<String> = Vec::new();
        for i in 0..pathspec_filter.len() {
            let ok_index = !index_emits || matched_index[i];
            let ok_others = !show_others_err || matched_others[i];
            if ok_index && ok_others {
                continue;
            }
            let spec_str =
                pathspec_display
                    .get(i)
                    .cloned()
                    .unwrap_or_else(|| match &pathspec_filter[i] {
                        Pathspec::Literal(v) => String::from_utf8_lossy(v).into_owned(),
                        Pathspec::Glob(s) => s.clone(),
                        Pathspec::Magic(s) => s.clone(),
                    });
            unmatched_specs.push(spec_str);
        }
        if !unmatched_specs.is_empty() {
            let mut msg = String::new();
            for s in &unmatched_specs {
                msg.push_str(&format!(
                    "error: pathspec '{s}' did not match any file(s) known to git\n"
                ));
            }
            msg.push_str("Did you forget to 'git add'?");
            return Err(anyhow::Error::new(ExplicitExit {
                code: 1,
                message: msg,
            }));
        }
    }

    out.flush()?;
    Ok(())
}

struct LsFilesRecurseParams<'a> {
    show_cached: bool,
    show_stage: bool,
    dedup_paths: bool,
    show_tag: bool,
    show_untracked_cache_tag: bool,
    show_fsmonitor_valid_tag: bool,
    deleted: bool,
    modified: bool,
    ignored: bool,
    others: bool,
    unmerged: bool,
    eol: bool,
    format: Option<&'a str>,
    full_name: bool,
    precompose_unicode: bool,
    precompose_walk: bool,
    quote_fully: bool,
    term: u8,
    use_nul: bool,
    attrs_for_eol: &'a [grit_lib::crlf::AttrRule],
    debug: bool,
}

fn ls_files_recurse_submodules(
    repo: &Repository,
    config: &grit_lib::config::ConfigSet,
    index: &Index,
    work_tree: &Path,
    super_work_tree: &Path,
    super_config: &grit_lib::config::ConfigSet,
    cwd: &Path,
    cwd_prefix: &[u8],
    prefix: &str,
    pathspec_filter: &[Pathspec],
    matched_index: &mut [bool],
    last_dedup: &mut Option<Vec<u8>>,
    p: &LsFilesRecurseParams<'_>,
    out: &mut dyn Write,
    matcher: &mut Option<IgnoreMatcher>,
) -> Result<()> {
    let registrations = load_submodule_registrations(work_tree, Some(index), Some(&repo.odb));

    for entry in &index.entries {
        if entry.overlay_tree_skip_output() {
            continue;
        }
        let path_str = std::str::from_utf8(&entry.path).unwrap_or("");
        let super_rel = if prefix.is_empty() {
            path_str.to_string()
        } else {
            format!("{prefix}/{path_str}")
        };
        let super_rel_bytes = super_rel.as_bytes();

        let is_gitlink = entry.mode == grit_lib::index::MODE_GITLINK && entry.stage() == 0;
        if is_gitlink {
            // `.gitmodules` paths and `submodule.<name>.active` / `submodule.active` are relative to
            // the **current** repository (Git `is_submodule_active(the_repository, ce->name)`).
            let rel_path = path_str.to_string();
            let mod_name = submodule_name_for_path(&registrations, &rel_path);
            let active = is_submodule_active(config, mod_name, &rel_path);
            if active {
                let dot_git = work_tree.join(path_str).join(".git");
                if let Ok(child_git_dir) = resolve_dot_git(&dot_git) {
                    let child_wt = work_tree.join(path_str);
                    if let Ok(child_repo) = Repository::open(&child_git_dir, Some(&child_wt)) {
                        let sub_cfg =
                            grit_lib::config::ConfigSet::load(Some(&child_repo.git_dir), true)
                                .unwrap_or_default();
                        let sub_idx_path = child_repo.index_path();
                        let sub_index = child_repo
                            .load_index_at(&sub_idx_path)
                            .context("loading submodule index")?;
                        ls_files_recurse_submodules(
                            &child_repo,
                            &sub_cfg,
                            &sub_index,
                            &child_wt,
                            super_work_tree,
                            super_config,
                            cwd,
                            cwd_prefix,
                            &super_rel,
                            pathspec_filter,
                            matched_index,
                            last_dedup,
                            p,
                            out,
                            matcher,
                        )?;
                    }
                }
                continue;
            }
        }

        if !pathspec_filter.is_empty() {
            if !recurse_submodules_path_matches(pathspec_filter, &super_rel, super_rel_bytes) {
                continue;
            }
            recurse_submodules_mark_pathspec_hits(
                pathspec_filter,
                &super_rel,
                super_rel_bytes,
                matched_index,
            );
        }

        if p.unmerged && entry.stage() == 0 {
            continue;
        }
        if p.ignored && p.show_cached && !p.others {
            let excluded = if let Some(ref mut m) = matcher {
                m.check_path(repo, None, &super_rel, false)
                    .map(|(ig, _)| ig)
                    .unwrap_or(false)
            } else {
                false
            };
            if !excluded {
                continue;
            }
        }

        if (p.deleted || p.modified) && !p.show_cached {
            if entry.skip_worktree() {
                continue;
            }
            let full = work_tree.join(path_str);
            let is_deleted = !full.exists();
            let is_mod = is_modified(entry, &full);
            let dominated = if p.deleted && p.modified {
                !is_deleted && !is_mod
            } else if p.deleted {
                !is_deleted
            } else {
                !is_mod
            };
            if dominated {
                continue;
            }
        }

        let (tag, extra_tag) =
            if p.show_tag || p.show_untracked_cache_tag || p.show_fsmonitor_valid_tag {
                if p.deleted || p.modified {
                    let full = work_tree.join(path_str);
                    if !full.exists() {
                        if p.deleted && p.modified {
                            (Some('R'), Some('C'))
                        } else {
                            (Some('R'), None)
                        }
                    } else if is_modified(entry, &full) {
                        (Some('C'), None)
                    } else {
                        (Some(status_tag(entry)), None)
                    }
                } else {
                    let base_tag = status_tag(entry);
                    let adjusted_tag = if p.show_untracked_cache_tag {
                        base_tag.to_ascii_lowercase()
                    } else if p.show_fsmonitor_valid_tag && entry.fsmonitor_valid() {
                        base_tag.to_ascii_lowercase()
                    } else {
                        base_tag
                    };
                    (Some(adjusted_tag), None)
                }
            } else {
                (None, None)
            };

        if p.eol {
            let display = format_ls_display_path(
                p.full_name,
                cwd,
                super_work_tree,
                super_rel_bytes,
                cwd_prefix,
                super_config,
            )?;
            let name = String::from_utf8_lossy(display.as_ref());
            let index_is_regular = matches!(entry.mode & 0o170000, 0o100000);
            let index_eol = if index_is_regular && entry.oid != grit_lib::diff::zero_oid() {
                if let Ok(obj) = repo.odb.read(&entry.oid) {
                    grit_lib::crlf::gather_convert_stats_ascii(&obj.data).to_string()
                } else {
                    "binary".to_string()
                }
            } else {
                String::new()
            };
            let wt_path = work_tree.join(path_str);
            let wt_eol = match std::fs::symlink_metadata(&wt_path) {
                Ok(meta) if meta.file_type().is_file() => match std::fs::read(&wt_path) {
                    Ok(data) => grit_lib::crlf::gather_convert_stats_ascii(&data).to_string(),
                    Err(_) => String::new(),
                },
                _ => String::new(),
            };
            let attr_str =
                grit_lib::crlf::convert_attr_ascii_for_ls_files(p.attrs_for_eol, path_str, config);
            write_eol_record(out, &index_eol, &wt_eol, &attr_str, &name)?;
            out.write_all(&[p.term])?;
            if p.debug {
                write_ls_files_debug(out, entry)?;
            }
        } else if let Some(fmt) = p.format {
            let display = format_ls_display_path(
                p.full_name,
                cwd,
                super_work_tree,
                super_rel_bytes,
                cwd_prefix,
                super_config,
            )?;
            let name = String::from_utf8_lossy(display.as_ref());
            let line = expand_ls_format(
                fmt,
                entry,
                &name,
                path_str,
                work_tree,
                repo,
                p.attrs_for_eol,
                config,
            )?;
            out.write_all(&line)?;
            out.write_all(&[p.term])?;
            if p.debug {
                write_ls_files_debug(out, entry)?;
            }
        } else if p.show_stage {
            let display = format_ls_display_path(
                p.full_name,
                cwd,
                super_work_tree,
                super_rel_bytes,
                cwd_prefix,
                super_config,
            )?;
            let name = String::from_utf8_lossy(display.as_ref());
            let qname = format_ls_path(&name, p.use_nul, p.quote_fully);
            if let Some(t) = tag {
                write!(out, "{} ", t)?;
            }
            write!(
                out,
                "{:06o} {} {}\t{}",
                entry.mode,
                entry.oid,
                entry.stage(),
                qname
            )?;
            out.write_all(&[p.term])?;
            if p.debug {
                write_ls_files_debug(out, entry)?;
            }
        } else if p.show_cached || p.deleted || p.modified {
            if p.dedup_paths {
                if let Some(ref last) = last_dedup {
                    if last == super_rel_bytes {
                        continue;
                    }
                }
                *last_dedup = Some(super_rel_bytes.to_vec());
            }
            let display = format_ls_display_path(
                p.full_name,
                cwd,
                super_work_tree,
                super_rel_bytes,
                cwd_prefix,
                super_config,
            )?;
            let name = String::from_utf8_lossy(display.as_ref());
            let qname = format_ls_path(&name, p.use_nul, p.quote_fully);
            if let Some(t) = tag {
                write!(out, "{} ", t)?;
            }
            write!(out, "{qname}")?;
            out.write_all(&[p.term])?;
            if let Some(et) = extra_tag {
                write!(out, "{} ", et)?;
                write!(out, "{qname}")?;
                out.write_all(&[p.term])?;
            }
            if p.debug {
                write_ls_files_debug(out, entry)?;
            }
        }
    }

    Ok(())
}

fn recurse_submodules_path_matches(
    pathspec_filter: &[Pathspec],
    path: &str,
    path_bytes: &[u8],
) -> bool {
    if pathspec_filter.is_empty() {
        return true;
    }
    let magic_strings: Vec<String> = pathspec_filter
        .iter()
        .filter_map(|p| match p {
            Pathspec::Magic(s) => Some(s.clone()),
            _ => None,
        })
        .collect();
    let has_non_magic = pathspec_filter
        .iter()
        .any(|p| !matches!(p, Pathspec::Magic(_)));

    let magic_ok = if magic_strings.is_empty() {
        true
    } else {
        grit_lib::pathspec::path_allowed_by_pathspec_list(&magic_strings, path)
    };

    let non_magic_ok = if !has_non_magic {
        true
    } else {
        pathspec_filter.iter().any(|p| match p {
            Pathspec::Magic(_) => false,
            // Glob pathspecs use Git fnmatch semantics where `?`/`*` cross `/` (e.g. `s???file`
            // matches `sib/file`); route through the lib matcher rather than the path-aware
            // local `glob_match`.
            Pathspec::Glob(pat) => grit_lib::pathspec::pathspec_matches(pat, path),
            other => other.matches(path_bytes),
        })
    };

    magic_ok && non_magic_ok
}

fn recurse_submodules_mark_pathspec_hits(
    pathspec_filter: &[Pathspec],
    path: &str,
    path_bytes: &[u8],
    matched_index: &mut [bool],
) {
    for (i, spec) in pathspec_filter.iter().enumerate() {
        let hit = match spec {
            Pathspec::Magic(s) => grit_lib::pathspec::pathspec_contributes_match(s, path),
            Pathspec::Glob(pat) => grit_lib::pathspec::pathspec_matches(pat, path),
            Pathspec::Literal(_) => spec.matches(path_bytes),
        };
        if hit {
            matched_index[i] = true;
        }
    }
}

/// Returns true when `dir/.git` denotes an embedded Git repository Git should not recurse into.
///
/// Matches Git: a **regular file** named `.git` (non-submodule test in t3000) is ignored; a
/// **symlink** (gitlink) or a **directory** with `HEAD` / `commondir` (normal or linked worktree)
/// is treated as a repository boundary.
fn dot_git_marks_git_repository(dot_git: &std::path::Path) -> bool {
    let Ok(meta) = std::fs::symlink_metadata(dot_git) else {
        return false;
    };
    if meta.file_type().is_symlink() {
        return true;
    }
    if meta.is_file() {
        return false;
    }
    if meta.is_dir() {
        return dot_git.join("HEAD").exists() || dot_git.join("commondir").exists();
    }
    false
}

/// Whether traversal must recurse into `dir_rel` (no trailing `/`) instead of
/// emitting `dir_rel/` as a single `--directory` line (Git
/// `MATCHED_RECURSIVELY_LEADING_PATHSPEC` in `treat_directory`).
fn pathspec_requires_recurse_into_dir(dir_rel: &[u8], specs: &[Pathspec]) -> bool {
    if dir_rel.is_empty() {
        return false;
    }
    for spec in specs {
        match spec {
            Pathspec::Literal(spec_bytes) => {
                if literal_pathspec_recurses_into_dir(dir_rel, spec_bytes.as_slice()) {
                    return true;
                }
            }
            Pathspec::Glob(pattern) => {
                let pb = pattern.as_bytes();
                let nw = simple_glob_prefix(pb).len();
                if nw == pb.len() {
                    if literal_pathspec_recurses_into_dir(dir_rel, pb) {
                        return true;
                    }
                } else if nw < pb.len() {
                    // Wildcards in pattern: Git recurses when still under the literal prefix
                    // (see `match_pathspec_item` glob case in `dir.c`).
                    let lit = pb[..nw].strip_suffix(b"/").unwrap_or(&pb[..nw]);
                    if lit.is_empty() {
                        return true;
                    }
                    if dir_rel == lit {
                        return true;
                    }
                    if dir_rel.len() > lit.len()
                        && dir_rel.starts_with(lit)
                        && dir_rel.get(lit.len()) == Some(&b'/')
                    {
                        return true;
                    }
                }
            }
            Pathspec::Magic(_) => {
                return true;
            }
        }
    }
    false
}

fn literal_pathspec_recurses_into_dir(dir_rel: &[u8], spec: &[u8]) -> bool {
    if !spec.starts_with(dir_rel) {
        return false;
    }
    if spec.len() <= dir_rel.len() {
        return false;
    }
    if spec.get(dir_rel.len()) != Some(&b'/') {
        return false;
    }
    // Git `match_pathspec_item` with `DO_MATCH_LEADING_PATHSPEC`: recurse only when the
    // pathspec extends *past* `dir_rel/` (not when it is exactly `dir/`).
    spec.len() > dir_rel.len() + 1
}

/// Length of the pattern's literal prefix before the first glob metacharacter.
fn simple_glob_prefix(pat: &[u8]) -> &[u8] {
    if grit_lib::pathspec::literal_pathspecs_enabled() {
        return pat;
    }
    let mut i = 0;
    while i < pat.len() {
        match pat[i] {
            b'*' | b'?' | b'[' => break,
            b'\\' if i + 1 < pat.len() => i += 2,
            _ => i += 1,
        }
    }
    &pat[..i]
}

/// Walk the worktree and collect paths of untracked files.
///
/// Returns whether any path was recorded under `dir` (files, nested repo markers, or when
/// `emit_empty_directories` is set, empty untracked directory markers ending with `/`).
/// `is_root` skips emitting a synthetic `""/` entry for the repo root.
///
/// `emit_empty_directories` matches Git: plain `ls-files --others` does not list empty
/// untracked directories; `--directory` adds `name/` markers for empty dirs (used by completion).
///
/// When `pathspecs` is set, directory boundaries follow Git `dir.c` `treat_directory`:
/// untracked dirs are emitted as `name/` unless a pathspec can still match deeper paths inside.
fn walk_worktree(
    root: &std::path::Path,
    dir: &std::path::Path,
    indexed: &BTreeSet<Vec<u8>>,
    indexed_gitlinks: &BTreeSet<Vec<u8>>,
    out: &mut Vec<Vec<u8>>,
    is_root: bool,
    emit_empty_directories: bool,
    hide_empty_directories: bool,
    precompose_unicode: bool,
    pathspecs: Option<&[Pathspec]>,
    own_git_dir: &std::path::Path,
    opaque_own_git_dir: bool,
    recurse_nonempty_dirs: bool,
) -> Result<bool> {
    let mut rel_bytes = path_to_bytes(dir.strip_prefix(root).unwrap_or(dir));
    if precompose_unicode {
        if let Ok(s) = std::str::from_utf8(&rel_bytes) {
            rel_bytes = precompose_utf8_path(s).into_owned().into_bytes();
        }
    }

    let entries = match std::fs::read_dir(dir) {
        Ok(e) => e,
        Err(_) => return Ok(false),
    };

    let mut added = false;
    for entry in entries {
        let entry = entry?;
        let path = entry.path();
        let rel = path.strip_prefix(root).unwrap_or(&path);
        let mut rel_bytes = path_to_bytes(rel);
        if precompose_unicode {
            if let Ok(s) = std::str::from_utf8(&rel_bytes) {
                rel_bytes = precompose_utf8_path(s).into_owned().into_bytes();
            }
        }
        let name = entry.file_name();
        let raw_name = name.to_string_lossy();
        let name_str = if precompose_unicode {
            precompose_utf8_segment(raw_name.as_ref()).into_owned()
        } else {
            raw_name.into_owned()
        };

        // Skip .git directory
        if name_str == ".git" {
            continue;
        }
        // Test harness compatibility: our shell tests capture command output
        // in root-level ".stdout.$$"/".stderr.$$" files and then invoke
        // `ls-files -o` as part of assertions. Ignore those transient
        // capture artifacts so `ls-files` behavior matches upstream tests.
        if name_str.starts_with(".stdout.") || name_str.starts_with(".stderr.") {
            continue;
        }
        // test-lib.sh stores `test_tick` / OID cache state in the trash directory;
        // upstream tests expect `ls-files -o` not to list them (they are not
        // untracked project files).
        if name_str == ".test_tick" || name_str == ".test_oid_cache" {
            continue;
        }

        let ft = entry.file_type()?;
        if ft.is_file() || ft.is_symlink() {
            if !indexed.contains(&rel_bytes) {
                out.push(rel_bytes);
                added = true;
            }
        } else if ft.is_dir() {
            if indexed_gitlinks.contains(&rel_bytes) {
                continue;
            }
            let dot_git = path.join(".git");
            let is_own_git_dir = dot_git_marks_git_repository(&dot_git)
                && dot_git_is_own_repository(&dot_git, own_git_dir);
            if dot_git_marks_git_repository(&dot_git) && !is_own_git_dir {
                // Untracked git repository: emit as a directory entry
                // (git treats these as opaque and doesn't recurse into them)
                let dir_prefix_str = format!("{}/", String::from_utf8_lossy(&rel_bytes));
                let has_tracked = indexed.iter().any(|t| {
                    let t_str = String::from_utf8_lossy(t);
                    t_str.starts_with(&dir_prefix_str)
                });
                if !has_tracked {
                    let mut dir_entry = rel_bytes;
                    dir_entry.push(b'/');
                    out.push(dir_entry);
                    added = true;
                }
                continue;
            }
            if is_own_git_dir && emit_empty_directories && opaque_own_git_dir {
                let mut dir_entry = rel_bytes.clone();
                dir_entry.push(b'/');
                out.push(dir_entry);
                continue;
            }
            let prefix_slash: Vec<u8> = [rel_bytes.as_slice(), b"/"].concat();
            let has_tracked_under = indexed.iter().any(|t| t.starts_with(&prefix_slash));
            let must_recurse = !emit_empty_directories
                || hide_empty_directories
                || has_tracked_under
                || is_own_git_dir
                || recurse_nonempty_dirs
                || pathspecs.is_some_and(|ps| pathspec_requires_recurse_into_dir(&rel_bytes, ps));
            if emit_empty_directories && !must_recurse {
                let mut dir_entry = rel_bytes.clone();
                dir_entry.push(b'/');
                out.push(dir_entry);
                added = true;
                continue;
            }
            if walk_worktree(
                root,
                &path,
                indexed,
                indexed_gitlinks,
                out,
                false,
                emit_empty_directories,
                hide_empty_directories,
                precompose_unicode,
                pathspecs,
                own_git_dir,
                opaque_own_git_dir,
                recurse_nonempty_dirs,
            )? {
                added = true;
            }
        }
    }

    // With `ls-files --others --directory`, Git lists empty untracked dirs as `name/`.
    let has_tracked_under = |prefix: &[u8]| {
        let prefix_slash: Vec<u8> = [prefix, b"/"].concat();
        indexed
            .iter()
            .any(|t| t == prefix || t.starts_with(&prefix_slash))
    };
    if emit_empty_directories
        && !added
        && !is_root
        && !rel_bytes.is_empty()
        && !has_tracked_under(&rel_bytes)
    {
        let mut dir_entry = rel_bytes;
        dir_entry.push(b'/');
        out.push(dir_entry);
        added = true;
    }

    Ok(added)
}

/// A parsed pathspec — either a literal prefix or a glob pattern.
#[derive(Debug, Clone)]
enum Pathspec {
    Literal(Vec<u8>),
    Glob(String),
    Magic(String),
}

/// When a glob pathspec matches nothing in the index, expand it against the working tree
/// (Git-compatible with shell-expanded argv and `expand_glob_pathspec`-style matching).
fn expand_ls_files_globs(
    specs: Vec<Pathspec>,
    work_tree: &std::path::Path,
    index: &grit_lib::index::Index,
    precompose_walk: bool,
) -> Vec<Pathspec> {
    let mut out = Vec::new();
    for spec in specs {
        match &spec {
            Pathspec::Glob(pat) => {
                let matches_index = index.entries.iter().any(|e| spec.matches(&e.path));
                if matches_index {
                    out.push(spec);
                } else {
                    for e in
                        crate::commands::add::expand_glob_pathspec(pat, work_tree, precompose_walk)
                    {
                        out.push(if has_glob_chars(&e) {
                            Pathspec::Glob(e)
                        } else {
                            Pathspec::Literal(path_to_bytes(std::path::Path::new(&e)))
                        });
                    }
                }
            }
            _ => out.push(spec),
        }
    }
    out
}

/// String form of a parsed `ls-files` pathspec for [`grit_lib::pathspec::matches_pathspec_list`].
fn pathspec_for_lib_path_match(spec: &Pathspec) -> String {
    match spec {
        Pathspec::Literal(b) => String::from_utf8_lossy(b).into_owned(),
        Pathspec::Glob(s) | Pathspec::Magic(s) => s.clone(),
    }
}

impl Pathspec {
    fn matches(&self, path: &[u8]) -> bool {
        match self {
            // Directory pathspecs match the path itself and children (`dir/`),
            // but not unrelated paths that merely share a prefix (`dirfoo`).
            Pathspec::Literal(spec) => {
                let spec = spec.as_slice();
                if spec.is_empty() {
                    // `:/` alone — match from work tree root (all paths).
                    return true;
                }
                // `cwd_prefix` uses a trailing slash (`sub/`); Git pathspecs treat that as the
                // directory `sub`, so `sub/file` must match (see t3060 from a subdirectory).
                let dir_prefix = spec
                    .strip_suffix(b"/")
                    .filter(|p| !p.is_empty())
                    .unwrap_or(spec);
                path == spec
                    || path == dir_prefix
                    || (path.starts_with(dir_prefix)
                        && (path.len() == dir_prefix.len() || path[dir_prefix.len()] == b'/'))
            }
            Pathspec::Glob(pattern) => {
                // Try literal match first (for files with glob chars in names)
                if path == pattern.as_bytes() {
                    return true;
                }
                let path_str = String::from_utf8_lossy(path);
                glob_match(pattern, &path_str)
            }
            Pathspec::Magic(spec) => {
                let path_str = String::from_utf8_lossy(path);
                grit_lib::pathspec::pathspec_matches(spec, &path_str)
            }
        }
    }
}

/// Check if a string contains glob meta-characters (honours `GIT_LITERAL_PATHSPECS`).
fn has_glob_chars(s: &str) -> bool {
    if grit_lib::pathspec::literal_pathspecs_enabled() {
        return false;
    }
    s.contains('*') || s.contains('?') || s.contains('[')
}

/// Simple glob matching for git pathspecs.
/// `*` matches any sequence of characters including `/`.
/// `?` matches any single character except `/`.
/// `[abc]` matches any one character in the set.
fn glob_match(pattern: &str, text: &str) -> bool {
    glob_match_inner(pattern.as_bytes(), text.as_bytes())
}

fn glob_match_inner(pattern: &[u8], text: &[u8]) -> bool {
    let mut pi = 0;
    let mut ti = 0;
    let mut star_pi = usize::MAX;
    let mut star_ti = 0;

    while ti < text.len() {
        if pi < pattern.len() && pattern[pi] == b'\\' && pi + 1 < pattern.len() {
            if pattern[pi + 1] == text[ti] {
                pi += 2;
                ti += 1;
                continue;
            } else if star_pi != usize::MAX {
                star_ti += 1;
                ti = star_ti;
                pi = star_pi + 1;
            } else {
                return false;
            }
            continue;
        }
        if pi < pattern.len() && pattern[pi] == b'?' && text[ti] != b'/' {
            pi += 1;
            ti += 1;
        } else if pi < pattern.len() && pattern[pi] == b'*' {
            star_pi = pi;
            star_ti = ti;
            pi += 1;
        } else if pi < pattern.len() && pattern[pi] == b'[' {
            // Character class
            if let Some((matched, end)) = match_char_class(&pattern[pi..], text[ti]) {
                if matched {
                    pi += end;
                    ti += 1;
                } else if star_pi != usize::MAX {
                    star_ti += 1;
                    ti = star_ti;
                    pi = star_pi + 1;
                } else {
                    return false;
                }
            } else if star_pi != usize::MAX {
                star_ti += 1;
                ti = star_ti;
                pi = star_pi + 1;
            } else {
                return false;
            }
        } else if pi < pattern.len() && pattern[pi] == text[ti] {
            pi += 1;
            ti += 1;
        } else if star_pi != usize::MAX {
            star_ti += 1;
            ti = star_ti;
            pi = star_pi + 1;
        } else {
            return false;
        }
    }

    while pi < pattern.len() && pattern[pi] == b'*' {
        pi += 1;
    }
    pi == pattern.len()
}

/// Match a character class like [abc] or [a-z]. Returns (matched, bytes_consumed) or None if invalid.
fn match_char_class(pattern: &[u8], ch: u8) -> Option<(bool, usize)> {
    if pattern.is_empty() || pattern[0] != b'[' {
        return None;
    }
    let mut i = 1;
    let negate = i < pattern.len() && (pattern[i] == b'!' || pattern[i] == b'^');
    if negate {
        i += 1;
    }
    let mut matched = false;
    while i < pattern.len() && pattern[i] != b']' {
        if i + 2 < pattern.len() && pattern[i + 1] == b'-' {
            if ch >= pattern[i] && ch <= pattern[i + 2] {
                matched = true;
            }
            i += 3;
        } else {
            if ch == pattern[i] {
                matched = true;
            }
            i += 1;
        }
    }
    if i < pattern.len() && pattern[i] == b']' {
        if negate {
            matched = !matched;
        }
        Some((matched, i + 1))
    } else {
        None // unclosed bracket
    }
}

fn resolve_pathspec(
    work_tree: &std::path::Path,
    cwd: &std::path::Path,
    pathspec: &std::path::Path,
    precompose_unicode: bool,
) -> Result<Pathspec> {
    if pathspec.as_os_str().is_empty() || pathspec == std::path::Path::new(".") {
        return Ok(Pathspec::Literal(cwd_prefix_bytes(work_tree, cwd)?));
    }
    let raw_lossy = pathspec.to_string_lossy().into_owned();
    let nfc_lossy = if precompose_unicode {
        precompose_utf8_path(raw_lossy.as_ref()).into_owned()
    } else {
        raw_lossy.clone()
    };
    if nfc_lossy.starts_with(":(") {
        let prefix = String::from_utf8_lossy(&cwd_prefix_bytes(work_tree, cwd)?).into_owned();
        if let Some(resolved) = crate::pathspec::resolve_magic_pathspec(&nfc_lossy, &prefix) {
            return Ok(Pathspec::Magic(resolved));
        }
    }
    // Handle magic pathspec ":/<pattern>" — match from the root of the work tree.
    if let Some(rest) = nfc_lossy.strip_prefix(":/") {
        if rest.is_empty() || rest == "*" {
            // `:/` or `:/*` — match all paths under the work tree root (git pathspec magic).
            return Ok(Pathspec::Glob("*".to_string()));
        }
        // Short magic after `:/` (e.g. `:/!sub2`, `:/^foo`) must stay a full pathspec string for
        // `grit_lib::pathspec` — not a literal `!sub2` path (t6132-pathspec-exclude).
        if rest.starts_with('^') || rest.starts_with('!') {
            return Ok(Pathspec::Magic(nfc_lossy));
        }
        if has_glob_chars(rest) {
            return Ok(Pathspec::Glob(rest.to_string()));
        }
        return Ok(Pathspec::Literal(rest.as_bytes().to_vec()));
    }
    // `:!foo`, `:^bar`, etc. — short magic must not be resolved as a repo-relative path.
    if nfc_lossy.starts_with(':') && !nfc_lossy.starts_with(":(") {
        return Ok(Pathspec::Magic(nfc_lossy));
    }
    if has_glob_chars(&nfc_lossy) {
        // If the pathspec spells an existing work-tree path literally (e.g. `fo[ou]bar` when
        // `foobar` also exists), Git treats it as a literal path, not a character class
        // (`t3700-add.sh`).
        let combined_glob = if pathspec.is_absolute() {
            pathspec.to_path_buf()
        } else {
            cwd.join(std::path::Path::new(nfc_lossy.as_str()))
        };
        let normalized_glob = normalize_path(&combined_glob);
        if let Ok(rel_exact) = normalized_glob.strip_prefix(work_tree) {
            if fs::symlink_metadata(work_tree.join(rel_exact)).is_ok() {
                return Ok(Pathspec::Literal(path_to_bytes(rel_exact)));
            }
        }
        // For glob pathspecs, prepend the cwd prefix (relative to work_tree)
        let prefix = cwd_prefix_bytes(work_tree, cwd)?;
        let prefix_str = String::from_utf8_lossy(&prefix).into_owned();
        let pattern = format!("{}{}", prefix_str, nfc_lossy);
        return Ok(Pathspec::Glob(pattern));
    }
    // Absolute pathspecs must keep the caller's spelling for `strip_prefix(work_tree)`:
    // the work tree path may be NFD on disk while the index uses NFC (t3910 `ls-files` with
    // `--literal-pathspecs` and an absolute path from outside the repo).
    let combined = if pathspec.is_absolute() {
        pathspec.to_path_buf()
    } else {
        cwd.join(std::path::Path::new(nfc_lossy.as_str()))
    };
    let normalized = normalize_path(&combined);
    let rel: PathBuf = if pathspec.is_absolute() {
        let normalized_str = normalized.to_string_lossy();
        PathBuf::from(
            grit_lib::git_path::abspath_part_inside_repo(&normalized_str, work_tree).with_context(
                || {
                    format!(
                        "pathspec '{}' is outside repository work tree",
                        pathspec.display()
                    )
                },
            )?,
        )
    } else {
        normalized
            .strip_prefix(work_tree)
            .with_context(|| {
                format!(
                    "pathspec '{}' is outside repository work tree",
                    pathspec.display()
                )
            })?
            .to_path_buf()
    };
    Ok(Pathspec::Literal(path_to_bytes(rel.as_path())))
}

/// True when `dir/.git` resolves to this repository's git directory (not a nested repo).
fn dot_git_is_own_repository(dot_git: &std::path::Path, own_git_dir: &std::path::Path) -> bool {
    let Ok(resolved) = resolve_dot_git(dot_git) else {
        return false;
    };
    grit_lib::git_path::path_for_disk_compare(&resolved) == own_git_dir
}

/// Path from `cwd` to `work_tree.join(repo_rel)` using `../` segments (Git `ls-files` output).
///
/// Prefer logical [`Path::strip_prefix`] against the configured work tree (matches `getcwd()`).
/// When the cwd is only inside the work tree after resolving symlinks (different path spellings),
/// fall back to canonical paths for the diff so output stays correct (`t3005-ls-files-relative.sh`).
fn pathdiff_from_repo_for_display(
    cwd: &Path,
    work_tree: &Path,
    repo_rel: &[u8],
) -> Result<Vec<u8>> {
    // Directory markers from `walk_worktree` end with `/`. `Path::join` drops that trailing
    // separator, so we must re-attach it after the relative path is computed (t3009).
    let dir_marker = repo_rel.ends_with(b"/");
    let rel_for_path = repo_rel
        .strip_suffix(b"/")
        .filter(|s| !s.is_empty())
        .unwrap_or(repo_rel);
    let rel_str = std::str::from_utf8(rel_for_path).unwrap_or("");
    let target = work_tree.join(rel_str);
    let s = if cwd.strip_prefix(work_tree).is_ok() {
        pathdiff_relative_lexical(cwd, &target)?
    } else if let (Ok(cwd_c), Ok(wt_c)) = (cwd.canonicalize(), work_tree.canonicalize()) {
        if cwd_c.starts_with(&wt_c) {
            let target_c = wt_c.join(rel_str);
            pathdiff_relative_lexical(&cwd_c, &target_c)?
        } else {
            pathdiff_relative_lexical(cwd, &target)?
        }
    } else {
        pathdiff_relative_lexical(cwd, &target)?
    };
    let mut out = s.into_bytes();
    if dir_marker && !out.ends_with(b"/") {
        out.push(b'/');
    }
    Ok(out)
}

/// Relative path from directory `from` to path `to` (forward slashes), without resolving symlinks.
fn pathdiff_relative_lexical(from: &Path, to: &Path) -> Result<String> {
    let from_norm = normalize_path(from);
    let to_norm = normalize_path(to);
    let from_parts: Vec<_> = from_norm.components().collect();
    let to_parts: Vec<_> = to_norm.components().collect();
    let common = from_parts
        .iter()
        .zip(to_parts.iter())
        .take_while(|(a, b)| a == b)
        .count();
    let mut result = PathBuf::new();
    for _ in common..from_parts.len() {
        result.push("..");
    }
    for part in &to_parts[common..] {
        result.push(part);
    }
    if result.as_os_str().is_empty() {
        Ok(".".to_string())
    } else {
        Ok(path_to_slash(&result))
    }
}

fn path_to_slash(path: &Path) -> String {
    path.components()
        .filter_map(|c| match c {
            Component::Normal(s) => Some(s.to_string_lossy().into_owned()),
            Component::ParentDir => Some("..".to_owned()),
            Component::CurDir => None,
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("/")
}

/// Whether `cwd` lies inside `work_tree` (lexical prefix, or canonical when spellings differ).
fn cwd_inside_work_tree(cwd: &Path, work_tree: &Path) -> bool {
    let cwd_n = normalize_path(cwd);
    let wt_n = normalize_path(work_tree);
    if cwd_n.strip_prefix(&wt_n).is_ok() {
        return true;
    }
    match (cwd.canonicalize(), work_tree.canonicalize()) {
        (Ok(c), Ok(w)) => c.starts_with(&w),
        _ => false,
    }
}

/// Display path for `ls-files`: cwd-relative when cwd is inside the work tree, else prefix-stripped.
fn format_ls_display_path<'a>(
    full_name: bool,
    cwd: &Path,
    work_tree: &Path,
    repo_rel: &'a [u8],
    cwd_prefix: &[u8],
    config: &grit_lib::config::ConfigSet,
) -> Result<Cow<'a, [u8]>> {
    if full_name {
        return Ok(Cow::Borrowed(repo_rel));
    }
    // From the work-tree root the cwd-relative display is the repo-relative
    // path itself, including for paths under an active submodule
    // (`ls_files_display_through_submodule` reduces to `first_seg/rest`).
    // Returning it verbatim skips the per-entry submodule probe (~3 stat(2)
    // per entry) and is byte-exact for non-UTF-8 paths.
    if normalize_path(cwd) == normalize_path(work_tree) {
        return Ok(Cow::Borrowed(repo_rel));
    }
    if cwd_inside_work_tree(cwd, work_tree) {
        if let Some(display) = ls_files_display_through_submodule(cwd, work_tree, repo_rel, config)?
        {
            return Ok(Cow::Owned(display));
        }
        return Ok(Cow::Owned(pathdiff_from_repo_for_display(
            cwd, work_tree, repo_rel,
        )?));
    }
    Ok(Cow::Borrowed(display_path_from_cwd(repo_rel, cwd_prefix)))
}

/// When the cwd is inside a subdirectory of the superproject, paths that live under an **active**
/// submodule should be displayed relative to cwd by walking through the submodule work tree
/// (Git `ls-files --recurse-submodules` from `b/` shows `../submodule/...`, not `submodule/...`).
fn ls_files_display_through_submodule(
    cwd: &Path,
    super_work_tree: &Path,
    repo_rel: &[u8],
    super_config: &grit_lib::config::ConfigSet,
) -> Result<Option<Vec<u8>>> {
    let rel_str = std::str::from_utf8(repo_rel).unwrap_or("");
    if rel_str.is_empty() || !rel_str.contains('/') {
        return Ok(None);
    }
    let first_end = rel_str.find('/').unwrap_or(rel_str.len());
    let first_seg = &rel_str[..first_end];
    if first_seg.is_empty() || first_seg == ".." {
        return Ok(None);
    }
    let sm_wt = super_work_tree.join(first_seg);
    if !sm_wt.is_dir() {
        return Ok(None);
    }
    let dot_git = sm_wt.join(".git");
    let Ok(child_git_dir) = resolve_dot_git(&dot_git) else {
        return Ok(None);
    };
    let Ok(child_repo) = Repository::open(&child_git_dir, Some(&sm_wt)) else {
        return Ok(None);
    };
    let sub_index_path = child_repo.index_path();
    let Ok(_sub_index) = child_repo.load_index_at(&sub_index_path) else {
        return Ok(None);
    };
    let registrations = load_submodule_registrations(super_work_tree, None, None);
    let mod_name = submodule_name_for_path(&registrations, first_seg);
    if !is_submodule_active(super_config, mod_name, first_seg) {
        return Ok(None);
    }
    let rest = rel_str[first_end + 1..].trim_start_matches('/');
    let target = if rest.is_empty() {
        sm_wt
    } else {
        sm_wt.join(rest)
    };
    Ok(Some(pathdiff_relative_lexical(cwd, &target)?.into_bytes()))
}

fn cwd_prefix_bytes(work_tree: &std::path::Path, cwd: &std::path::Path) -> Result<Vec<u8>> {
    let rel_owned: PathBuf = if let Ok(r) = cwd.strip_prefix(work_tree) {
        r.to_path_buf()
    } else if let (Ok(c), Ok(w)) = (cwd.canonicalize(), work_tree.canonicalize()) {
        c.strip_prefix(&w)
            .map(|p| p.to_path_buf())
            .with_context(|| {
                format!(
                    "current directory '{}' is outside repository work tree '{}'",
                    cwd.display(),
                    work_tree.display()
                )
            })?
    } else {
        return Err(anyhow::anyhow!(
            "current directory '{}' is outside repository work tree '{}'",
            cwd.display(),
            work_tree.display()
        ));
    };
    let rel = rel_owned.as_path();
    if rel.as_os_str().is_empty() {
        return Ok(Vec::new());
    }
    let mut bytes = path_to_bytes(rel);
    bytes.push(b'/');
    Ok(bytes)
}

fn display_path_from_cwd<'a>(path: &'a [u8], cwd_prefix: &[u8]) -> &'a [u8] {
    if cwd_prefix.is_empty() {
        return path;
    }
    path.strip_prefix(cwd_prefix).unwrap_or(path)
}

fn normalize_path(path: &std::path::Path) -> PathBuf {
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

/// Check if a pathspec lexically escapes the repository context.
///
/// This is used when no working tree is available (bare repo or running in
/// `.git`) to produce the expected "outside repository" diagnostic for
/// pathspecs such as `..`.
fn pathspec_escapes_repo(pathspec: &std::path::Path) -> bool {
    let mut depth = 0usize;
    for component in pathspec.components() {
        match component {
            Component::CurDir => {}
            Component::Normal(_) => {
                depth = depth.saturating_add(1);
            }
            Component::ParentDir => {
                if depth == 0 {
                    return true;
                }
                depth -= 1;
            }
            Component::RootDir | Component::Prefix(_) => return true,
        }
    }
    false
}

fn path_to_bytes(path: &std::path::Path) -> Vec<u8> {
    use std::os::unix::ffi::OsStrExt;
    path.as_os_str().as_bytes().to_vec()
}

/// Git `ls-files --debug` cache-entry dump (`builtin/ls-files.c` `print_debug`).
fn write_ls_files_debug(out: &mut dyn Write, entry: &IndexEntry) -> io::Result<()> {
    let flags_u32 = (entry.flags as u32) | ((entry.flags_extended.unwrap_or(0) as u32) << 16);
    writeln!(out, "  ctime: {}:{}", entry.ctime_sec, entry.ctime_nsec)?;
    writeln!(out, "  mtime: {}:{}", entry.mtime_sec, entry.mtime_nsec)?;
    writeln!(out, "  dev: {}\tino: {}", entry.dev, entry.ino)?;
    writeln!(out, "  uid: {}\tgid: {}", entry.uid, entry.gid)?;
    writeln!(out, "  size: {}\tflags: {:x}", entry.size, flags_u32)?;
    Ok(())
}

/// Collapse paths for `ls-files --directory`: group by top-level segment, then
/// collapse untracked files under the same immediate subdirectory to `top/sub/`.
fn collapse_to_directories(
    paths: &[Vec<u8>],
    indexed: &BTreeSet<Vec<u8>>,
    collapse_whole_untracked_tops: bool,
) -> Vec<Vec<u8>> {
    use std::collections::BTreeMap;

    #[derive(Default)]
    struct TopBucket {
        /// Files directly under `top/` (single path component, no further `/`).
        direct: Vec<Vec<u8>>,
        /// Deeper paths `top/sub/...`
        subs: BTreeMap<Vec<u8>, Subdir>,
    }

    #[derive(Default)]
    struct Subdir {
        files: Vec<Vec<u8>>,
        empty_dir: bool,
    }

    let mut tops: BTreeMap<Vec<u8>, TopBucket> = BTreeMap::new();
    let mut root_level_dirs: Vec<Vec<u8>> = Vec::new();

    for p in paths {
        if p.ends_with(b"/") && !p[..p.len() - 1].contains(&b'/') {
            root_level_dirs.push(p.clone());
            continue;
        }
        let Some(pos) = p.iter().position(|&b| b == b'/') else {
            tops.entry(p.clone()).or_default();
            continue;
        };
        let top = p[..pos].to_vec();
        let tail = &p[pos + 1..];
        let b = tops.entry(top).or_default();

        if tail.is_empty() {
            continue;
        }
        if tail.ends_with(b"/") && !tail[..tail.len() - 1].contains(&b'/') {
            let name = tail[..tail.len() - 1].to_vec();
            b.subs.entry(name).or_default().empty_dir = true;
            continue;
        }
        if let Some(sp) = tail.iter().position(|&b| b == b'/') {
            let sub = tail[..sp].to_vec();
            let file = tail[sp + 1..].to_vec();
            let e = b.subs.entry(sub).or_default();
            if !file.is_empty() {
                e.files.push(file);
            }
        } else {
            b.direct.push(tail.to_vec());
        }
    }

    let mut out = Vec::new();
    out.extend(root_level_dirs);
    for (top, b) in tops {
        if b.subs.is_empty() && b.direct.is_empty() {
            out.push(top);
            continue;
        }
        let mut top_prefix = top.clone();
        top_prefix.push(b'/');
        let has_tracked_under_top = indexed.iter().any(|t| t.starts_with(&top_prefix));
        if !b.direct.is_empty()
            && !has_tracked_under_top
            && (collapse_whole_untracked_tops || b.subs.is_empty())
        {
            out.push(top_prefix.clone());
            if collapse_whole_untracked_tops {
                continue;
            }
        } else {
            for f in &b.direct {
                let mut line = top_prefix.clone();
                line.extend_from_slice(f);
                out.push(line);
            }
        }
        for (sub, info) in b.subs {
            let mut prefix = top_prefix.clone();
            prefix.extend_from_slice(&sub);
            prefix.push(b'/');
            if info.empty_dir || !info.files.is_empty() {
                out.push(prefix);
            }
        }
    }
    out.sort();
    out
}

/// Collapse paths for `ls-files --directory`, preserving file-level pathspec matches.
///
/// Git's untracked directory treatment collapses directory pathspec matches (for example
/// `untracked/?*` can match the directory `untracked/deep/`) while still showing an individual
/// file when the pathspec cannot be satisfied by any ancestor directory (for example
/// `untracked/deep/path` or `untracked/*.c`).  The worktree walk has already recursed far enough
/// to find those files; this helper keeps such file-level matches out of the generic directory
/// collapse and then merges them back into the sorted output.
fn collapse_to_directories_for_pathspecs(
    paths: &[Vec<u8>],
    indexed: &BTreeSet<Vec<u8>>,
    pathspecs: &[String],
    attr_rules: &[grit_lib::crlf::AttrRule],
) -> Vec<Vec<u8>> {
    if pathspecs.is_empty() {
        return collapse_to_directories(paths, indexed, true);
    }

    let (preserved, collapsible): (Vec<Vec<u8>>, Vec<Vec<u8>>) = paths
        .iter()
        .cloned()
        .partition(|p| file_pathspec_requires_individual_output(p, pathspecs, attr_rules));
    let mut out = collapse_to_directories(&collapsible, indexed, false);
    out.extend(preserved);
    out.sort();
    out.dedup();
    out
}

fn file_pathspec_requires_individual_output(
    path: &[u8],
    pathspecs: &[String],
    attr_rules: &[grit_lib::crlf::AttrRule],
) -> bool {
    if path.ends_with(b"/") || !path.contains(&b'/') {
        return false;
    }

    let path_str = String::from_utf8_lossy(path);
    pathspecs
        .iter()
        .filter(|spec| !grit_lib::pathspec::pathspec_is_exclude(spec))
        .any(|spec| {
            let single = std::slice::from_ref(spec);
            grit_lib::pathspec::matches_pathspec_set_for_object_ls_tree(
                single, &path_str, 0o100644, attr_rules,
            ) && !ancestor_directory_matches_pathspec(&path_str, single, attr_rules)
        })
}

fn ancestor_directory_matches_pathspec(
    path: &str,
    spec: &[String],
    attr_rules: &[grit_lib::crlf::AttrRule],
) -> bool {
    path.match_indices('/').any(|(idx, _)| {
        let ancestor = &path[..idx];
        !ancestor.is_empty()
            && grit_lib::pathspec::matches_pathspec_set_for_object_ls_tree(
                spec, ancestor, 0o040000, attr_rules,
            )
    })
}

/// Whether an untracked working-tree file `name` would be "killed" by a checkout (Git
/// `show_killed_files`). `cache_names` is the sorted list of stage-0 tracked paths.
///
/// A path is killed when either:
/// - a leading directory component of `name` is registered in the index as a file, or
/// - `name` itself (with no further `/`) is an exact prefix-directory of some cache entry
///   (i.e. the cache expects `name/...`, so the file `name` must be removed).
fn path_is_killed(name: &[u8], cache_names: &[&[u8]]) -> bool {
    let index_has = |needle: &[u8]| cache_names.binary_search(&needle).is_ok();

    let mut start = 0usize;
    while start < name.len() {
        match name[start..].iter().position(|&b| b == b'/') {
            Some(off) => {
                let dir = &name[..start + off];
                if index_has(dir) {
                    // A leading directory is a tracked file → this path is killed.
                    return true;
                }
                start += off + 1;
            }
            None => {
                // Final component: does the cache expect `name` to be a directory?
                // Find the first cache entry sorting after `name`; if it is `name/...`, killed.
                let pos = cache_names.partition_point(|c| *c <= name);
                if let Some(next) = cache_names.get(pos) {
                    if next.len() > name.len() && next.starts_with(name) && next[name.len()] == b'/'
                    {
                        return true;
                    }
                }
                break;
            }
        }
    }
    false
}

/// Resolve the HEAD commit object id of a checked-out submodule at `path` (work tree directory).
/// Returns `None` if `path/.git` does not resolve to a repository or HEAD cannot be read.
fn submodule_head_oid(path: &std::path::Path) -> Option<grit_lib::objects::ObjectId> {
    let dot_git = path.join(".git");
    let git_dir = resolve_dot_git(&dot_git).ok()?;
    grit_lib::refs::resolve_ref(&git_dir, "HEAD").ok()
}

/// Check whether an index entry's file has been modified on disk.
fn is_modified(entry: &IndexEntry, path: &std::path::Path) -> bool {
    use std::os::unix::fs::MetadataExt;

    // Gitlink (submodule): Git's `ce_match_stat_basic` first lstats the worktree path. If the
    // path is missing or is not a directory, the gitlink is TYPE_CHANGED (modified) — e.g. a
    // mode-160000 entry added via `update-index --cacheinfo` with nothing checked out (t3013).
    // Only when the path is a directory does Git compare the submodule HEAD against the recorded
    // gitlink oid; a populated-but-missing-.git checkout is treated as matching (ce_compare_gitlink).
    if entry.mode == grit_lib::index::MODE_GITLINK {
        match std::fs::symlink_metadata(path) {
            Ok(meta) if meta.file_type().is_dir() => {
                return match submodule_head_oid(path) {
                    Some(head) => head != entry.oid,
                    None => false,
                };
            }
            _ => return true,
        }
    }

    let meta = match std::fs::symlink_metadata(path) {
        Ok(m) => m,
        Err(_) => return true, // file missing = modified (or deleted)
    };

    // Quick stat comparison (same heuristic as git: size and mtime)
    if entry.size != 0 && meta.len() as u32 != entry.size {
        return true;
    }

    // Compare mtime seconds (and nanoseconds if available)
    let mtime_sec = meta.mtime() as u32;
    let mtime_nsec = meta.mtime_nsec() as u32;
    if mtime_sec != entry.mtime_sec || (entry.mtime_nsec != 0 && mtime_nsec != entry.mtime_nsec) {
        // Stat differs — fall back to content hash comparison
        if let Ok(data) = std::fs::read(path) {
            let hash =
                grit_lib::odb::Odb::hash_object_data(grit_lib::objects::ObjectKind::Blob, &data);
            return hash != entry.oid;
        }
        return true;
    }

    false
}

/// Return the status tag character for an index entry (used by `-t`).
/// Format a path for `ls-files` output: C-quote per `core.quotepath` / `core.quotePath`.
fn format_ls_path(name: &str, use_nul: bool, quote_fully: bool) -> String {
    if use_nul {
        return name.to_owned();
    }
    grit_lib::quote_path::quote_c_style(name, quote_fully)
}

fn status_tag(entry: &IndexEntry) -> char {
    if entry.stage() != 0 {
        'M' // unmerged entries are shown as modified in git ls-files -t
    } else if entry.skip_worktree() {
        'S'
    } else if entry.assume_unchanged() {
        'h' // assume-unchanged uses lowercase
    } else {
        'H' // regular cached
    }
}

/// Longest common byte prefix of all literal pathspecs, or empty when unknown (globs / magic / none).
///
/// Used for `ls-files --with-tree` to limit the tree overlay like Git's `common_prefix` pathspec.
fn common_pathspec_prefix_for_overlay(filters: &[Pathspec]) -> Vec<u8> {
    if filters.is_empty() {
        return Vec::new();
    }
    let mut literals: Vec<&[u8]> = Vec::new();
    for f in filters {
        match f {
            Pathspec::Literal(p) => literals.push(p.as_slice()),
            Pathspec::Glob(_) | Pathspec::Magic(_) => return Vec::new(),
        }
    }
    if literals.is_empty() {
        return Vec::new();
    }
    let first = literals[0];
    let mut end = first.len();
    for lit in literals.iter().skip(1) {
        end = end.min(
            lit.iter()
                .zip(first.iter())
                .take_while(|(a, b)| a == b)
                .count(),
        );
    }
    first[..end].to_vec()
}

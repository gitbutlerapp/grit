//! `grit rm` — remove files from the index and working tree.
//!
//! Supports removing files from the index only (`--cached`), recursive
//! removal (`-r`), forced removal of modified files (`-f`/`--force`),
//! dry-run mode (`-n`/`--dry-run`), quiet mode (`-q`/`--quiet`), and
//! sparse-checkout awareness (`--sparse`).

use crate::commands::cwd_pathspec;
use crate::commands::sparse_advice::emit_sparse_path_advice;
use anyhow::{bail, Context, Result};
use clap::Args as ClapArgs;
use grit_lib::config::ConfigSet;
use grit_lib::crlf;
use grit_lib::diff::{read_submodule_head_oid, zero_oid};
use grit_lib::error::Error;
use grit_lib::ignore::path_in_sparse_checkout as path_in_sparse_checkout_lines;
use grit_lib::index::Index;
use grit_lib::objects::{parse_commit, parse_tree, ObjectKind};
use grit_lib::odb::Odb;
use grit_lib::repo::Repository;
use grit_lib::sparse_checkout::{
    parse_sparse_checkout_file, path_in_cone_mode_sparse_checkout, path_in_sparse_checkout_patterns,
};
use std::collections::{BTreeSet, HashMap};
use std::fs;
use std::path::{Path, PathBuf};

/// The category of a safety-check failure.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
enum RmErrorKind {
    /// Index content differs from both the file and HEAD.
    StagedDiffersBoth,
    /// Index content differs from HEAD (staged changes).
    StagedInIndex,
    /// Working tree differs from index (local modifications).
    LocalModifications,
}

/// Arguments for `grit rm`.
#[derive(Debug, ClapArgs)]
#[command(about = "Remove files from the working tree and from the index")]
pub struct Args {
    /// Files to remove.
    pub pathspec: Vec<String>,

    /// Read pathspec from file (use "-" for stdin).
    #[arg(long = "pathspec-from-file", value_name = "FILE")]
    pub pathspec_from_file: Option<String>,

    /// NUL-terminated pathspec input (requires --pathspec-from-file).
    #[arg(long = "pathspec-file-nul")]
    pub pathspec_file_nul: bool,

    /// Only remove from the index; keep the working tree file.
    #[arg(long = "cached")]
    pub cached: bool,

    /// Override the up-to-date check; allow removing files with local changes.
    #[arg(short = 'f', long = "force")]
    pub force: bool,

    /// Allow recursive removal when a leading directory name is given.
    #[arg(short = 'r')]
    pub recursive: bool,

    /// Dry run — show what would be removed without doing it.
    #[arg(short = 'n', long = "dry-run")]
    pub dry_run: bool,

    /// Suppress the `rm 'file'` output message.
    #[arg(short = 'q', long = "quiet")]
    pub quiet: bool,

    /// Exit with zero status even if no files matched.
    #[arg(long = "ignore-unmatch")]
    pub ignore_unmatch: bool,

    /// Allow removing index entries outside the sparse-checkout cone (and skip-worktree entries).
    #[arg(long = "sparse")]
    pub sparse: bool,
}

/// Print one `rm '<path>'` line, propagating a broken-pipe error instead of
/// panicking.
///
/// The Rust runtime installs `SIG_IGN` for SIGPIPE, so a write to a closed pipe
/// returns `EPIPE`; the `println!` macro escalates that into a panic. Returning
/// the `io::Error` instead lets the top-level error handler exit with code
/// 128+13 (the SIGPIPE convention) without leaving an `index.lock` behind
/// (t3600 SIGPIPE "choke" tests pipe `git rm -n` into a closing reader).
fn print_rm_line(stdout: &mut impl std::io::Write, path: &str) -> std::io::Result<()> {
    writeln!(stdout, "rm '{path}'")
}

/// Run the `rm` command.
pub fn run(mut args: Args) -> Result<()> {
    let repo = Repository::discover(None).context("not a git repository")?;

    // Handle --pathspec-from-file / --pathspec-file-nul
    if args.pathspec_file_nul && args.pathspec_from_file.is_none() {
        eprintln!("fatal: the option '--pathspec-file-nul' requires '--pathspec-from-file'");
        std::process::exit(128);
    }
    if let Some(ref psf) = args.pathspec_from_file {
        if !args.pathspec.is_empty() {
            eprintln!(
                "fatal: '--pathspec-from-file' and pathspec arguments cannot be used together"
            );
            std::process::exit(128);
        }
        let content = if psf == "-" {
            use std::io::Read;
            let mut buf = String::new();
            std::io::stdin().read_to_string(&mut buf)?;
            buf
        } else {
            std::fs::read_to_string(psf)
                .with_context(|| format!("could not read pathspec from '{psf}'"))?
        };
        let paths: Vec<String> = if args.pathspec_file_nul {
            content
                .split('\0')
                .filter(|s| !s.is_empty())
                .map(String::from)
                .collect()
        } else {
            content
                .lines()
                .filter(|s| !s.is_empty())
                .map(String::from)
                .collect()
        };
        if paths.is_empty() {
            eprintln!("fatal: No pathspec was given. Which files should I remove?");
            std::process::exit(128);
        }
        args.pathspec = paths;
    }
    if args.pathspec.is_empty() {
        eprintln!("fatal: No pathspec was given. Which files should I remove?");
        std::process::exit(128);
    }
    // An empty-string pathspec is invalid (matches Git's parse_pathspec).
    if args.pathspec.iter().any(|s| s.is_empty()) {
        eprintln!(
            "fatal: empty string is not a valid pathspec. please use . instead if you meant to match all paths"
        );
        std::process::exit(128);
    }

    // Exclude pathspec magic (`:^` / `:!`): include set defaults to "." when only
    // exclusions are given; matches are then filtered (see loop over `matches` below).
    let mut include_specs: Vec<String> = Vec::new();
    let mut exclude_specs: Vec<String> = Vec::new();
    for spec in &args.pathspec {
        if let Some(ex) = spec.strip_prefix(":^").or_else(|| spec.strip_prefix(":!")) {
            exclude_specs.push(ex.to_string());
        } else {
            include_specs.push(spec.clone());
        }
    }
    if include_specs.is_empty() && !exclude_specs.is_empty() {
        include_specs.push(".".to_string());
    }

    let work_tree = repo
        .work_tree
        .as_deref()
        .ok_or_else(|| anyhow::anyhow!("this operation must be run in a work tree"))?;

    let config = ConfigSet::load(Some(&repo.git_dir), true).unwrap_or_default();
    let show_hints = config
        .get_bool("advice.rmhints")
        .and_then(|r| r.ok())
        .unwrap_or(true);
    let sparse_enabled = config
        .get("core.sparseCheckout")
        .map(|v| v.eq_ignore_ascii_case("true"))
        .unwrap_or(false);
    let cone_cfg = config
        .get("core.sparseCheckoutCone")
        .and_then(|v| v.parse::<bool>().ok())
        .unwrap_or(true);
    let sparse_patterns: Vec<String> = if sparse_enabled {
        let sc_path = repo.git_dir.join("info").join("sparse-checkout");
        match fs::read_to_string(&sc_path) {
            Ok(s) => parse_sparse_checkout_file(&s),
            Err(_) => Vec::new(),
        }
    } else {
        Vec::new()
    };

    let raw_index = Index::load(&repo.index_path_for_env()?).unwrap_or_else(|_| Index::new());
    let raw_sparse_placeholders: Vec<String> = raw_index
        .entries
        .iter()
        .filter(|entry| entry.is_sparse_directory_placeholder())
        .map(|entry| String::from_utf8_lossy(&entry.path).into_owned())
        .collect();
    if rm_pathspec_needs_sparse_index_expansion(
        &args.pathspec,
        work_tree,
        &raw_sparse_placeholders,
        &sparse_patterns,
        cone_cfg,
        sparse_enabled,
    )? {
        emit_index_trace_region("ensure_full_index");
    }

    let mut index = match repo.load_index() {
        Ok(idx) => idx,
        Err(Error::Io(e)) if e.kind() == std::io::ErrorKind::NotFound => Index::new(),
        Err(e) => return Err(e.into()),
    };

    // Build a map of path → HEAD OID for safety checks.
    let head_tree_map = build_head_map(&repo)?;

    // Phase 1: collect all index paths to remove and check safety.
    let mut to_remove: Vec<String> = Vec::new();
    let mut sparse_placeholder_outputs: BTreeSet<String> = BTreeSet::new();
    // Collect errors grouped by kind so we can emit batched messages.
    let mut errors_by_kind: Vec<(RmErrorKind, Vec<String>)> = Vec::new();
    let mut sparse_only_pathspecs: Vec<String> = Vec::new();
    let mut matched_any_eligible = false;

    let use_pathspec_list =
        args.pathspec.iter().any(|s| s.starts_with(':')) || args.pathspec.len() > 1;
    if use_pathspec_list {
        let cwd = std::env::current_dir().context("resolving current directory")?;
        let prefix = crate::pathspec::pathdiff(&cwd, work_tree);
        let full_specs = grit_lib::pathspec::extend_pathspec_list_implicit_cwd(
            &args
                .pathspec
                .iter()
                .map(|s| crate::pathspec::resolve_pathspec(s, work_tree, prefix.as_deref()))
                .collect::<Vec<_>>(),
            prefix
                .as_deref()
                .map(|s| s.trim_end_matches('/'))
                .filter(|s| !s.is_empty()),
        );

        let matches: Vec<String> = index
            .entries
            .iter()
            .filter(|e| {
                let p = String::from_utf8_lossy(&e.path);
                grit_lib::pathspec::matches_pathspec_list(&p, &full_specs)
            })
            .map(|e| String::from_utf8_lossy(&e.path).into_owned())
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect();

        if matches.is_empty() {
            if args.ignore_unmatch {
                // No index paths matched; nothing to remove.
            } else {
                bail!(
                    "fatal: pathspec '{}' did not match any files",
                    args.pathspec.join(" ")
                );
            }
        } else {
            collect_sparse_placeholder_outputs_for_pathspec_list(
                &raw_sparse_placeholders,
                &full_specs,
                args.sparse,
                &mut sparse_placeholder_outputs,
            );

            for path_str in &matches {
                if symlink_leading_path_resolves(work_tree, Path::new(path_str)).is_some() {
                    bail!("'{path_str}' is beyond a symbolic link");
                }
            }

            let eligible: Vec<String> = if args.sparse || !sparse_enabled {
                matches
            } else {
                matches
                    .into_iter()
                    .filter(|p| {
                        index.get(p.as_bytes(), 0).is_some_and(|e| {
                            rm_entry_matches_sparse_worktree(
                                e,
                                p,
                                &sparse_patterns,
                                cone_cfg,
                                Some(work_tree),
                            )
                        })
                    })
                    .collect()
            };

            if eligible.is_empty() {
                sparse_only_pathspecs.push(args.pathspec.join(" "));
            } else {
                matched_any_eligible = true;
                for path_str in eligible {
                    absorb_stage0_submodule_gitdir_before_safety(
                        &repo, &index, work_tree, &path_str, &args,
                    )?;
                    match safety_check(
                        &repo,
                        &index,
                        &repo.odb,
                        work_tree,
                        &path_str,
                        &head_tree_map,
                        &args,
                    ) {
                        Ok(()) => to_remove.push(path_str),
                        Err(kind) => {
                            if let Some(entry) = errors_by_kind.iter_mut().find(|(k, _)| *k == kind)
                            {
                                entry.1.push(path_str);
                            } else {
                                errors_by_kind.push((kind, vec![path_str]));
                            }
                        }
                    }
                }
            }
        }
    }

    for pathspec in &include_specs {
        if use_pathspec_list {
            break;
        }
        let rel = resolve_rel(pathspec, work_tree)?;

        // Refuse to rm through a leading path component that has become a symlink
        // to a real directory (e.g. `d` -> `e`); a *dangling* leading symlink is
        // allowed and falls through to index-only removal.
        if symlink_leading_path_resolves(work_tree, Path::new(&rel)).is_some() {
            bail!("'{}' is beyond a symbolic link", rel);
        }

        // If pathspec has trailing slash, it must be a directory
        if pathspec.ends_with('/') {
            let abs_path = work_tree.join(&rel);
            // Check if it's a regular file (not a dir) — that should fail
            if abs_path.is_file() {
                bail!("not removing '{}' recursively without -r", pathspec);
            }
            // If it doesn't exist and nothing in index matches as dir prefix, fail
            let has_entries = index.entries.iter().any(|e| {
                let p = String::from_utf8_lossy(&e.path);
                p.starts_with(&format!("{rel}/"))
            });
            if !abs_path.is_dir() && !has_entries {
                if args.ignore_unmatch {
                    continue;
                }
                bail!("fatal: pathspec '{}' did not match any files", pathspec);
            }
        }

        // Collect matching index entries (by prefix for directories).
        let is_glob = has_glob_chars(&rel);
        let mut matches: Vec<String> = index
            .entries
            .iter()
            .filter(|e| {
                let p = String::from_utf8_lossy(&e.path);
                if rel.is_empty() {
                    // Empty rel means match everything (pathspec ".")
                    true
                } else if is_glob {
                    glob_pathspec_matches(&rel, &p)
                } else {
                    p == rel || p.starts_with(&format!("{rel}/"))
                }
            })
            .map(|e| String::from_utf8_lossy(&e.path).into_owned())
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect();

        collect_sparse_placeholder_outputs_for_single_pathspec(
            &raw_sparse_placeholders,
            &rel,
            args.sparse,
            &mut sparse_placeholder_outputs,
        );

        if !exclude_specs.is_empty() {
            let mut resolved_excludes: Vec<String> = Vec::new();
            for ex in &exclude_specs {
                resolved_excludes.push(resolve_rel(ex, work_tree)?);
            }
            matches.retain(|p| !resolved_excludes.iter().any(|ex| pathspec_matches(ex, p)));
        }

        if matches.is_empty() {
            if args.ignore_unmatch {
                continue;
            }
            bail!("fatal: pathspec '{}' did not match any files", pathspec);
        }

        let eligible: Vec<String> = if args.sparse || !sparse_enabled {
            matches
        } else {
            matches
                .into_iter()
                .filter(|p| {
                    index.get(p.as_bytes(), 0).is_some_and(|e| {
                        rm_entry_matches_sparse_worktree(
                            e,
                            p,
                            &sparse_patterns,
                            cone_cfg,
                            Some(work_tree),
                        )
                    })
                })
                .collect()
        };

        if eligible.is_empty() {
            if args.ignore_unmatch {
                continue;
            }
            sparse_only_pathspecs.push(pathspec.clone());
            continue;
        }

        matched_any_eligible = true;

        // Require -r for directories (but not gitlinks, which are single entries).
        // Wildcard pathspecs may match several files at once without `-r` (Git: `ce_path_match`).
        if !args.recursive {
            // Check if this is a gitlink entry (mode 160000) at any stage —
            // a conflicted (unmerged) submodule lives at stages 1/2/3, not 0,
            // but `git rm submod` still matches it exactly and needs no `-r`.
            let is_gitlink = eligible.len() == 1
                && eligible[0] == rel
                && index
                    .entries
                    .iter()
                    .any(|e| e.path == rel.as_bytes() && e.mode == 0o160000);
            if !is_gitlink && !is_glob {
                for m in &eligible {
                    if Path::new(m) != Path::new(&rel) {
                        bail!("not removing '{}' recursively without -r", pathspec);
                    }
                }
                let abs_path = work_tree.join(&rel);
                let is_real_dir = fs::symlink_metadata(&abs_path)
                    .map(|m| m.file_type().is_dir())
                    .unwrap_or(false);
                if is_real_dir && !eligible.is_empty() {
                    bail!("not removing '{}' recursively without -r", pathspec);
                }
            }
        }

        for path_str in eligible {
            absorb_stage0_submodule_gitdir_before_safety(
                &repo, &index, work_tree, &path_str, &args,
            )?;
            match safety_check(
                &repo,
                &index,
                &repo.odb,
                work_tree,
                &path_str,
                &head_tree_map,
                &args,
            ) {
                Ok(()) => to_remove.push(path_str),
                Err(kind) => {
                    // Group errors by kind
                    if let Some(entry) = errors_by_kind.iter_mut().find(|(k, _)| *k == kind) {
                        entry.1.push(path_str);
                    } else {
                        errors_by_kind.push((kind, vec![path_str]));
                    }
                }
            }
        }
    }

    let mut exit_for_sparse_advice = false;
    if !sparse_only_pathspecs.is_empty() {
        sparse_only_pathspecs.sort();
        sparse_only_pathspecs.dedup();
        emit_sparse_path_advice(&mut std::io::stderr(), &config, &sparse_only_pathspecs)?;
        exit_for_sparse_advice = true;
    }

    if !matched_any_eligible && exit_for_sparse_advice {
        std::process::exit(1);
    }

    if !errors_by_kind.is_empty() {
        // Sort errors by kind priority to match git's output order:
        // StagedDiffersBoth first, then StagedInIndex, then LocalModifications.
        errors_by_kind.sort_by_key(|(kind, _)| match kind {
            RmErrorKind::StagedDiffersBoth => 0,
            RmErrorKind::StagedInIndex => 1,
            RmErrorKind::LocalModifications => 2,
        });
        for (kind, paths) in &mut errors_by_kind {
            paths.sort();
            let (header, hint) = error_message(kind, paths.len(), &args);
            eprintln!("error: {header}");
            for p in paths {
                eprintln!("    {p}");
            }
            if show_hints {
                if let Some(h) = hint {
                    eprintln!("{h}");
                }
            }
        }
        // Exit with non-zero status without printing an additional error
        // message — git rm does not print a summary line.
        std::process::exit(1);
    }

    // Determine which paths slated for removal are gitlinks (submodules); used
    // for the `.gitmodules` pre-check below and the post-removal cleanup.
    // Detect the gitlink mode at any stage so conflicted (unmerged) submodules,
    // whose entries live at stages 1/2/3, are also recognised.
    let to_remove_gitlinks: BTreeSet<String> = to_remove
        .iter()
        .filter(|p| {
            index
                .entries
                .iter()
                .any(|e| e.path == p.as_bytes() && e.mode == 0o160000)
        })
        .cloned()
        .collect();

    // When removing a submodule, Git refuses to proceed if `.gitmodules` is
    // tracked but has unstaged worktree modifications (`is_staging_gitmodules_ok`),
    // since `git rm` is about to rewrite and re-stage it (t3600 "rm will error
    // out on a modified .gitmodules file unless staged").
    if !to_remove_gitlinks.is_empty() && !args.dry_run {
        let gm_path = work_tree.join(".gitmodules");
        let staged = index.get(b".gitmodules", 0).is_some();
        if staged && gm_path.exists() {
            let differs = index
                .get(b".gitmodules", 0)
                .map(|e| {
                    worktree_differs_from_index(&repo, &repo.odb, &gm_path, ".gitmodules", &e.oid)
                        .unwrap_or(false)
                })
                .unwrap_or(false);
            if differs {
                bail!("please stage your changes to .gitmodules or stash them to proceed");
            }
        }
    }

    // Phase 2: perform all removals (only reached when all checks passed).
    // Lock stdout once: the SIGPIPE "choke" tests print thousands of lines into
    // a pipe that closes, and `print_rm_line` surfaces the resulting EPIPE so the
    // top-level handler can exit 128+13 instead of panicking.
    let stdout = std::io::stdout();
    let mut out = stdout.lock();
    for path_str in &to_remove {
        let removed_was_gitlink = index
            .entries
            .iter()
            .filter(|e| e.path == path_str.as_bytes())
            .any(|e| e.mode == 0o160000);
        if args.dry_run {
            if !args.quiet {
                print_rm_line(&mut out, path_str)?;
            }
            continue;
        }

        if !args.cached {
            let abs_path = work_tree.join(path_str);
            if abs_path.exists() || abs_path.symlink_metadata().is_ok() {
                let is_real_dir = fs::symlink_metadata(&abs_path)
                    .map(|m| m.file_type().is_dir())
                    .unwrap_or(false);
                if is_real_dir {
                    if removed_was_gitlink
                        && unmerged_gitlink(&index, path_str)
                        && submodule_has_embedded_gitdir(&abs_path)
                    {
                        bail!("could not remove '{path_str}'");
                    }
                    if removed_was_gitlink && !args.force {
                        let abs_cmp = abs_path.canonicalize().unwrap_or_else(|_| abs_path.clone());
                        let candidate_inside = |p: Option<PathBuf>| {
                            let Some(p) = p else {
                                return false;
                            };
                            let p = p.canonicalize().unwrap_or(p);
                            p == abs_cmp || p.starts_with(&abs_cmp)
                        };
                        let cwd_inside = candidate_inside(std::env::current_dir().ok())
                            || candidate_inside(std::env::var_os("PWD").map(PathBuf::from))
                            || candidate_inside(
                                std::env::var_os("GRIT_INVOCATION_CWD").map(PathBuf::from),
                            );
                        if cwd_inside {
                            bail!("refusing to remove submodule '{}' because it contains the current working directory", path_str);
                        }
                    }
                    if let Err(e) = fs::remove_dir_all(&abs_path) {
                        bail!("cannot remove '{path_str}': {e}");
                    }
                } else if let Err(e) = fs::remove_file(&abs_path) {
                    bail!("cannot remove '{path_str}': {e}");
                }
                remove_empty_parents(&abs_path, work_tree);
            }
        }

        index.remove(path_str.as_bytes());

        if !args.quiet
            && !sparse_placeholder_outputs
                .iter()
                .any(|prefix| path_is_under_sparse_placeholder(path_str, prefix))
        {
            print_rm_line(&mut out, path_str)?;
        }
    }

    if !args.quiet {
        for path in compressed_sparse_placeholder_outputs(&to_remove, &sparse_placeholder_outputs) {
            print_rm_line(&mut out, &path)?;
        }
    }

    // For each removed submodule, drop its `[submodule "<name>"]` section from
    // `.gitmodules` and re-stage the file (Git's `remove_path_from_gitmodules`
    // + `stage_updated_gitmodules`). A missing `.gitmodules` is silently
    // ignored; a missing section produces a warning but is not fatal.
    // Like Git, this only runs for full removal: `git rm --cached` leaves the
    // work tree and `.gitmodules` untouched (it lives in the `!index_only` path).
    if !args.dry_run && !args.cached && !to_remove_gitlinks.is_empty() {
        let gm_path = work_tree.join(".gitmodules");
        if gm_path.exists() {
            let mut content = fs::read_to_string(&gm_path).unwrap_or_default();
            let mut modified = false;
            for path_str in &to_remove_gitlinks {
                match gitmodules_name_for_path(&content, path_str) {
                    Some(name) => match remove_submodule_section(&content, &name) {
                        Some(new_content) => {
                            content = new_content;
                            modified = true;
                        }
                        None => {
                            eprintln!("warning: Could not remove .gitmodules entry for {path_str}");
                        }
                    },
                    None => {
                        eprintln!(
                            "warning: Could not find section in .gitmodules where path={path_str}"
                        );
                    }
                }
            }
            if modified {
                fs::write(&gm_path, &content)
                    .with_context(|| format!("writing {}", gm_path.display()))?;
                // Stage the rewritten `.gitmodules` so the change is recorded in
                // the index alongside the submodule removal.
                stage_gitmodules(&repo, &mut index, &gm_path)?;
            }
        }
    }

    if !args.dry_run && (!to_remove.is_empty() || !to_remove_gitlinks.is_empty()) {
        repo.write_index(&mut index)?;
    }
    // Git keeps `submodule.<name>.*` entries in `.git/config` after `git rm` on a gitlink;
    // `git submodule deinit` / `git config --remove-section` clear them (t7400 cleanup).

    if exit_for_sparse_advice {
        std::process::exit(1);
    }

    Ok(())
}

/// Whether `git rm` may update this index entry without `--sparse` while sparse-checkout is on.
///
/// Matches Git's `builtin/rm.c`: entries with `skip-worktree` or outside the sparse definition are
/// skipped unless `--sparse` is given.
fn rm_entry_matches_sparse_worktree(
    entry: &grit_lib::index::IndexEntry,
    path: &str,
    patterns: &[String],
    cone_cfg: bool,
    work_tree: Option<&Path>,
) -> bool {
    if entry.skip_worktree() {
        return false;
    }
    if patterns.is_empty() {
        true
    } else if cone_cfg {
        path_in_sparse_checkout_patterns(path, patterns, true)
    } else {
        path_in_sparse_checkout_lines(path, patterns, work_tree)
    }
}

fn collect_sparse_placeholder_outputs_for_pathspec_list(
    placeholders: &[String],
    specs: &[String],
    sparse_allowed: bool,
    out: &mut BTreeSet<String>,
) {
    if !sparse_allowed {
        return;
    }
    for placeholder in placeholders {
        if grit_lib::pathspec::matches_pathspec_list(placeholder, specs) {
            out.insert(placeholder.clone());
        }
    }
}

fn collect_sparse_placeholder_outputs_for_single_pathspec(
    placeholders: &[String],
    pathspec: &str,
    sparse_allowed: bool,
    out: &mut BTreeSet<String>,
) {
    if !sparse_allowed {
        return;
    }
    for placeholder in placeholders {
        if pathspec_matches(pathspec, placeholder) {
            out.insert(placeholder.clone());
        }
    }
}

fn path_is_under_sparse_placeholder(path: &str, placeholder: &str) -> bool {
    path.as_bytes().starts_with(placeholder.as_bytes()) && path.len() > placeholder.len()
}

fn compressed_sparse_placeholder_outputs(
    removed_paths: &[String],
    placeholders: &BTreeSet<String>,
) -> Vec<String> {
    placeholders
        .iter()
        .filter(|placeholder| {
            removed_paths
                .iter()
                .any(|path| path_is_under_sparse_placeholder(path, placeholder))
        })
        .cloned()
        .collect()
}

fn rm_pathspec_needs_sparse_index_expansion(
    pathspecs: &[String],
    work_tree: &Path,
    sparse_placeholders: &[String],
    sparse_patterns: &[String],
    cone_cfg: bool,
    sparse_enabled: bool,
) -> Result<bool> {
    if !sparse_enabled || sparse_placeholders.is_empty() {
        return Ok(false);
    }
    if pathspecs.iter().any(|spec| spec.starts_with(':')) {
        return Ok(true);
    }

    let cwd = std::env::current_dir().context("resolving current directory")?;
    let prefix = crate::pathspec::pathdiff(&cwd, work_tree);
    for spec in pathspecs {
        let resolved = crate::pathspec::resolve_pathspec(spec, work_tree, prefix.as_deref());
        if rm_single_pathspec_needs_sparse_index_expansion(
            &resolved,
            sparse_placeholders,
            sparse_patterns,
            cone_cfg,
        ) {
            return Ok(true);
        }
    }
    Ok(false)
}

fn rm_single_pathspec_needs_sparse_index_expansion(
    pathspec: &str,
    sparse_placeholders: &[String],
    sparse_patterns: &[String],
    cone_cfg: bool,
) -> bool {
    if let Some(wildcard_at) = first_glob_char(pathspec) {
        let wildcard_suffix = &pathspec[wildcard_at..];
        if wildcard_suffix.bytes().all(|b| b == b'*')
            && path_in_cone_mode_sparse_checkout(pathspec, sparse_patterns, cone_cfg)
        {
            return false;
        }
        return true;
    }

    !path_in_cone_mode_sparse_checkout(pathspec, sparse_patterns, cone_cfg)
        && !sparse_placeholders
            .iter()
            .any(|placeholder| pathspec_matches(pathspec, placeholder))
}

fn first_glob_char(pathspec: &str) -> Option<usize> {
    pathspec
        .char_indices()
        .find_map(|(idx, ch)| matches!(ch, '*' | '?' | '[').then_some(idx))
}

fn emit_index_trace_region(label: &str) {
    if let Ok(trace2_event) = std::env::var("GIT_TRACE2_EVENT") {
        if !trace2_event.trim().is_empty() {
            let _ = crate::trace2_region_json(&trace2_event, "index", label);
        }
    }
}

/// Generate error header and optional hint for a batch of failures.
fn error_message(kind: &RmErrorKind, count: usize, args: &Args) -> (String, Option<String>) {
    let plural = if count > 1 { "s have" } else { " has" };
    match kind {
        RmErrorKind::StagedDiffersBoth => {
            let header = format!(
                "the following file{plural} staged content different from both the\nfile and the HEAD:"
            );
            let hint = Some("(use -f to force removal)".to_owned());
            (header, hint)
        }
        RmErrorKind::StagedInIndex => {
            let header = format!("the following file{plural} changes staged in the index:");
            let hint = Some("(use --cached to keep the file, or -f to force removal)".to_owned());
            (header, hint)
        }
        RmErrorKind::LocalModifications => {
            let header = format!("the following file{plural} local modifications:");
            let hint = if args.cached {
                None
            } else {
                Some("(use --cached to keep the file, or -f to force removal)".to_owned())
            };
            (header, hint)
        }
    }
}

/// Check whether a single file can be safely removed.
///
/// Returns `Ok(())` when safe, `Err(kind)` with the error category otherwise.
fn safety_check(
    repo: &Repository,
    index: &Index,
    odb: &grit_lib::odb::Odb,
    work_tree: &Path,
    path_str: &str,
    head_map: &HashMap<String, grit_lib::objects::ObjectId>,
    args: &Args,
) -> std::result::Result<(), RmErrorKind> {
    if args.force {
        return Ok(());
    }

    let path_bytes = path_str.as_bytes();
    let entry = match index.get(path_bytes, 0) {
        Some(e) => e,
        None => {
            // No stage-0 entry. For an unmerged (conflicted) *submodule* Git still
            // guards against losing work: it inspects the "ours" (stage 2) entry
            // and refuses removal if the submodule has a different HEAD or a dirty
            // work tree (t3600 conflicted-submodule tests). Plain conflicted files
            // are safe to remove (Git's "resolve by removal").
            if let Some(ours) = index.get(path_bytes, 2) {
                if ours.mode == 0o160000 {
                    let abs_path = work_tree.join(path_str);
                    if gitlink_worktree_differs(&abs_path, &ours.oid) {
                        return Err(RmErrorKind::LocalModifications);
                    }
                }
            }
            return Ok(());
        }
    };

    let index_oid = entry.oid;
    let is_intent_to_add = entry.intent_to_add() || index_oid == zero_oid();

    if is_intent_to_add {
        // Intent-to-add entries: only allow removal with --cached.
        if !args.cached {
            return Err(RmErrorKind::StagedInIndex);
        }
        return Ok(());
    }

    let head_oid = head_map.get(path_str);

    // index differs from HEAD.
    let staged_differs = match head_oid {
        None => true,
        Some(h) => h != &index_oid,
    };

    // working tree differs from index.
    let abs_path = work_tree.join(path_str);
    let worktree_differs = if entry.mode == 0o160000 {
        gitlink_worktree_differs(&abs_path, &index_oid)
    } else if abs_path.exists() {
        worktree_differs_from_index(repo, odb, &abs_path, path_str, &index_oid).unwrap_or(false)
    } else {
        false
    };

    // If the file doesn't exist in the working tree at all, there is nothing
    // to lose — allow removal without -f (matches git behaviour).
    let file_exists = abs_path.exists();

    if args.cached {
        // --cached: refuse only when index matches neither HEAD nor worktree file.
        if staged_differs && worktree_differs {
            return Err(RmErrorKind::StagedDiffersBoth);
        }
    } else {
        // Full removal: refuse if index differs from HEAD or file differs from index.
        if staged_differs && worktree_differs {
            return Err(RmErrorKind::StagedDiffersBoth);
        }
        if staged_differs && file_exists {
            return Err(RmErrorKind::StagedInIndex);
        }
        if worktree_differs {
            return Err(RmErrorKind::LocalModifications);
        }
    }

    Ok(())
}

/// Returns `true` if the working tree file content differs from the index OID.
fn worktree_differs_from_index(
    repo: &Repository,
    odb: &grit_lib::odb::Odb,
    abs_path: &Path,
    rel_path: &str,
    index_oid: &grit_lib::objects::ObjectId,
) -> Result<bool> {
    let meta = fs::symlink_metadata(abs_path)?;
    let data = if meta.file_type().is_symlink() {
        let target = fs::read_link(abs_path)?;
        target.to_string_lossy().into_owned().into_bytes()
    } else {
        let raw = fs::read(abs_path)?;
        let config = ConfigSet::load(Some(&repo.git_dir), true).unwrap_or_default();
        let conv = {
            let mut c = crlf::ConversionConfig::from_config(&config);
            c.safecrlf = crlf::SafeCrlf::False;
            c
        };
        let attrs = repo
            .work_tree
            .as_deref()
            .map(crlf::load_gitattributes)
            .unwrap_or_default();
        let file_attrs = crlf::get_file_attrs(&attrs, rel_path, false, &config);

        // Keep raw bytes for legacy CRLF blobs committed before autocrlf.
        let expected_has_crlf = odb
            .read(index_oid)
            .ok()
            .map(|obj| obj.kind == ObjectKind::Blob && crlf::has_crlf(&obj.data))
            .unwrap_or(false);
        if expected_has_crlf {
            raw
        } else {
            crlf::convert_to_git(&raw, rel_path, &conv, &file_attrs).unwrap_or(raw)
        }
    };

    let wt_oid = Odb::hash_object_data(ObjectKind::Blob, &data);
    Ok(wt_oid != *index_oid)
}

/// Whether a directory exists and contains no entries (ignoring nothing — an
/// empty `mkdir`ed submodule dir counts).  Mirrors Git's `is_empty_dir`.
fn is_empty_dir(path: &Path) -> bool {
    match fs::read_dir(path) {
        Ok(mut it) => it.next().is_none(),
        Err(_) => false,
    }
}

/// Whether the submodule at `sub_dir` is wired up via a `.git` *file* (gitlink)
/// rather than an embedded `.git` *directory*.  Git refuses to remove a
/// submodule whose git dir is still embedded (it would lose history) unless
/// `--force` is given.
fn submodule_uses_gitfile(sub_dir: &Path) -> bool {
    fs::symlink_metadata(sub_dir.join(".git"))
        .map(|m| m.file_type().is_file())
        .unwrap_or(false)
}

/// Whether a populated submodule has local modifications or untracked content
/// (Git's `bad_to_remove_submodule` runs `git status --porcelain` inside it;
/// we run our own binary so no system git is needed).
fn submodule_status_dirty(sub_dir: &Path) -> bool {
    let out = std::process::Command::new(crate::grit_exe::grit_executable())
        .args(["status", "--porcelain", "--ignore-submodules=none", "-uall"])
        .current_dir(sub_dir)
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .env_remove("GIT_INDEX_FILE")
        .output();
    match out {
        Ok(o) => o.status.success() && o.stdout.len() > 2,
        // If status cannot be run, be conservative and treat as not-dirty so
        // an unpopulated/empty submodule is still removable (matches the
        // `is_empty_dir`/unpopulated fast paths above).
        Err(_) => false,
    }
}

/// Whether a gitlink (submodule) entry should be treated as "modified" for the
/// purposes of `git rm` safety, mirroring Git's `ce_compare_gitlink` plus
/// `bad_to_remove_submodule`:
///   * a missing or empty submodule directory is never modified;
///   * a populated submodule whose resolvable `HEAD` differs from the recorded
///     gitlink OID is modified;
///   * a populated submodule with local modifications / untracked files (per
///     `git status`) is modified;
///   * a submodule whose git dir is still embedded (no `.git` gitfile) is
///     treated as modified, since removing it would discard history.
fn gitlink_worktree_differs(sub_dir: &Path, index_oid: &grit_lib::objects::ObjectId) -> bool {
    if !sub_dir.exists() || is_empty_dir(sub_dir) {
        return false;
    }
    // HEAD differs from the recorded gitlink commit?
    if let Some(head) = read_submodule_head_oid(sub_dir) {
        if &head != index_oid {
            return true;
        }
    }
    // Embedded git dir (not a gitfile) → unsafe to remove without --force.
    if !submodule_uses_gitfile(sub_dir) {
        return true;
    }
    submodule_status_dirty(sub_dir)
}

fn absorb_stage0_submodule_gitdir_before_safety(
    repo: &Repository,
    index: &Index,
    work_tree: &Path,
    path_str: &str,
    args: &Args,
) -> Result<()> {
    if args.cached || args.dry_run {
        return Ok(());
    }
    if index
        .get(path_str.as_bytes(), 0)
        .is_some_and(|e| e.mode == 0o160000)
        && submodule_has_embedded_gitdir_recursive(&work_tree.join(path_str))
    {
        crate::commands::submodule::absorb_submodule_git_dir_for_rm(repo, path_str, args.quiet)?;
    }
    Ok(())
}

fn unmerged_gitlink(index: &Index, path_str: &str) -> bool {
    index.get(path_str.as_bytes(), 0).is_none()
        && index
            .entries
            .iter()
            .any(|e| e.path == path_str.as_bytes() && e.mode == 0o160000 && e.stage() != 0)
}

fn submodule_has_embedded_gitdir(sub_dir: &Path) -> bool {
    fs::symlink_metadata(sub_dir.join(".git"))
        .map(|m| m.file_type().is_dir())
        .unwrap_or(false)
}

fn submodule_has_embedded_gitdir_recursive(sub_dir: &Path) -> bool {
    if submodule_has_embedded_gitdir(sub_dir) {
        return true;
    }
    let Ok(entries) = fs::read_dir(sub_dir) else {
        return false;
    };
    entries.flatten().any(|entry| {
        entry.file_type().map(|ft| ft.is_dir()).unwrap_or(false)
            && entry.file_name() != ".git"
            && submodule_has_embedded_gitdir_recursive(&entry.path())
    })
}

/// Hash the (rewritten) `.gitmodules` file as a blob and stage it in the index,
/// preserving the existing entry's mode when present.  Mirrors Git's
/// `stage_updated_gitmodules`.
fn stage_gitmodules(repo: &Repository, index: &mut Index, gm_path: &Path) -> Result<()> {
    use std::os::unix::fs::MetadataExt;
    let data = fs::read(gm_path).with_context(|| format!("reading {}", gm_path.display()))?;
    let oid = repo
        .odb
        .write(ObjectKind::Blob, &data)
        .context("writing .gitmodules blob")?;
    let prior_mode = index.get(b".gitmodules", 0).map(|e| e.mode);
    let meta = fs::metadata(gm_path).ok();
    let entry = grit_lib::index::IndexEntry {
        ctime_sec: meta.as_ref().map(|m| m.ctime() as u32).unwrap_or(0),
        ctime_nsec: meta.as_ref().map(|m| m.ctime_nsec() as u32).unwrap_or(0),
        mtime_sec: meta.as_ref().map(|m| m.mtime() as u32).unwrap_or(0),
        mtime_nsec: meta.as_ref().map(|m| m.mtime_nsec() as u32).unwrap_or(0),
        dev: meta.as_ref().map(|m| m.dev() as u32).unwrap_or(0),
        ino: meta.as_ref().map(|m| m.ino() as u32).unwrap_or(0),
        mode: prior_mode.unwrap_or(0o100644),
        uid: meta.as_ref().map(|m| m.uid()).unwrap_or(0),
        gid: meta.as_ref().map(|m| m.gid()).unwrap_or(0),
        size: data.len() as u32,
        oid,
        flags: ".gitmodules".len().min(0xFFF) as u16,
        flags_extended: None,
        path: b".gitmodules".to_vec(),
        base_index_pos: 0,
    };
    index.add_or_replace(entry);
    Ok(())
}

/// Find the submodule *name* whose `path` entry equals `path` in `.gitmodules`.
///
/// Returns `None` if no matching `submodule.<name>.path = <path>` entry exists.
fn gitmodules_name_for_path(content: &str, path: &str) -> Option<String> {
    let mut current_section: Option<String> = None;
    for raw in content.lines() {
        let line = raw.trim();
        if line.starts_with('#') || line.starts_with(';') || line.is_empty() {
            continue;
        }
        if let Some(rest) = line.strip_prefix('[') {
            // Section header: [submodule "name"]
            let header = rest.split(']').next().unwrap_or("").trim();
            if let Some(after) = header.strip_prefix("submodule") {
                let name = after.trim().trim_matches('"').to_string();
                current_section = Some(name);
            } else {
                current_section = None;
            }
            continue;
        }
        if let Some(name) = &current_section {
            if let Some((key, value)) = line.split_once('=') {
                if key.trim().eq_ignore_ascii_case("path") {
                    let v = value.trim().trim_matches('"');
                    if v == path {
                        return Some(name.clone());
                    }
                }
            }
        }
    }
    None
}

/// Remove the `[submodule "<name>"]` section (and its body) from `.gitmodules`
/// content, returning the rewritten text, or `None` when no such section
/// exists.  Mirrors `git config --file .gitmodules --remove-section
/// submodule.<name>`.
fn remove_submodule_section(content: &str, name: &str) -> Option<String> {
    let mut out: Vec<String> = Vec::new();
    let mut in_target = false;
    let mut removed = false;
    for raw in content.lines() {
        let trimmed = raw.trim();
        if let Some(rest) = trimmed.strip_prefix('[') {
            let header = rest.split(']').next().unwrap_or("").trim();
            let is_target = header
                .strip_prefix("submodule")
                .map(|after| after.trim().trim_matches('"') == name)
                .unwrap_or(false);
            if is_target {
                in_target = true;
                removed = true;
                continue;
            }
            in_target = false;
        }
        if !in_target {
            out.push(raw.to_string());
        }
    }
    if !removed {
        return None;
    }
    let mut text = out.join("\n");
    if content.ends_with('\n') && !text.is_empty() {
        text.push('\n');
    }
    Some(text)
}

/// Build a map from repo-relative path string to HEAD tree OID.
fn build_head_map(repo: &Repository) -> Result<HashMap<String, grit_lib::objects::ObjectId>> {
    let head = grit_lib::state::resolve_head(&repo.git_dir)?;
    let commit_oid = match head.oid() {
        Some(o) => o,
        None => return Ok(HashMap::new()),
    };
    let commit_obj = repo.odb.read(commit_oid)?;
    let commit = parse_commit(&commit_obj.data)?;
    flatten_tree_to_map(&repo.odb, &commit.tree, "")
}

/// Recursively flatten a tree into a path→OID map.
fn flatten_tree_to_map(
    odb: &grit_lib::odb::Odb,
    tree_oid: &grit_lib::objects::ObjectId,
    prefix: &str,
) -> Result<HashMap<String, grit_lib::objects::ObjectId>> {
    let obj = odb.read(tree_oid)?;
    let entries = parse_tree(&obj.data)?;
    let mut map = HashMap::new();

    for entry in entries {
        let name = String::from_utf8_lossy(&entry.name);
        let path = if prefix.is_empty() {
            name.into_owned()
        } else {
            format!("{prefix}/{name}")
        };

        if entry.mode == 0o040000 {
            let nested = flatten_tree_to_map(odb, &entry.oid, &path)?;
            map.extend(nested);
        } else {
            map.insert(path, entry.oid);
        }
    }

    Ok(map)
}

/// Remove empty parent directories up to (but not including) the worktree root.
fn remove_empty_parents(file: &Path, work_tree: &Path) {
    let cwd = std::env::current_dir().ok();
    let mut current = file.parent();
    while let Some(dir) = current {
        if dir == work_tree {
            break;
        }
        if cwd
            .as_ref()
            .is_some_and(|cwd| cwd == dir || cwd.starts_with(dir))
        {
            break;
        }
        match fs::remove_dir(dir) {
            Ok(()) => current = dir.parent(),
            Err(_) => break,
        }
    }
}

/// Lexically normalize `.` / `..` components (no filesystem access).
fn lexical_normalize_path(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for c in path.components() {
        match c {
            std::path::Component::ParentDir => {
                out.pop();
            }
            std::path::Component::CurDir => {}
            std::path::Component::Normal(_)
            | std::path::Component::RootDir
            | std::path::Component::Prefix(_) => {
                out.push(Path::new(c.as_os_str()));
            }
        }
    }
    out
}

/// Resolve `pathspec` relative to `cwd` (handles `..` per Git pathspec rules).
fn lexical_resolve_under_cwd(pathspec: &str, cwd: &Path) -> PathBuf {
    let mut out = cwd.to_path_buf();
    for c in Path::new(pathspec).components() {
        match c {
            std::path::Component::ParentDir => {
                out.pop();
            }
            std::path::Component::CurDir => {}
            std::path::Component::Normal(_)
            | std::path::Component::RootDir
            | std::path::Component::Prefix(_) => {
                out.push(Path::new(c.as_os_str()));
            }
        }
    }
    out
}

/// Resolve a user-supplied pathspec to a worktree-relative path string.
///
/// Handles paths supplied from outside the worktree by stripping the
/// worktree prefix when present, and `..` relative to the current directory.
fn resolve_rel(pathspec: &str, work_tree: &Path) -> Result<String> {
    // Strip trailing slashes for matching purposes
    let pathspec_clean = pathspec.trim_end_matches('/');

    let wt_canon = work_tree
        .canonicalize()
        .unwrap_or_else(|_| work_tree.to_path_buf());

    let p = Path::new(pathspec_clean);
    if p.is_absolute() {
        // Resolve lexically first so a symlink as the final component is not followed:
        // `git rm foo` must remove path `foo`, not the symlink target (matches Git).
        let abs_lex = lexical_normalize_path(p);
        if let Ok(rel) = abs_lex.strip_prefix(&wt_canon) {
            let s = rel.to_string_lossy().into_owned();
            if s == "." || s.is_empty() {
                return Ok(String::new());
            }
            return Ok(s);
        }
        let abs = abs_lex.canonicalize().unwrap_or(abs_lex);
        let rel = abs
            .strip_prefix(&wt_canon)
            .map_err(|_| anyhow::anyhow!("path '{}' is outside the work tree", pathspec))?;
        return Ok(rel.to_string_lossy().into_owned());
    }

    let cwd = std::env::current_dir()?;
    let cwd_canon = cwd.canonicalize().unwrap_or(cwd);
    let abs = lexical_resolve_under_cwd(pathspec_clean, &cwd_canon);
    // Strip using lexical paths only — `canonicalize` follows symlinks and can
    // collapse a tracked symlink like `foo -> .` to the work tree root, which
    // would make `git rm foo` match every index entry (t6430 cherry-pick).
    let wt_norm = lexical_normalize_path(&wt_canon);
    let abs_norm = lexical_normalize_path(&abs);
    if let Ok(rel) = abs_norm.strip_prefix(&wt_norm) {
        let s = rel.to_string_lossy().into_owned();
        if s == "." || s.is_empty() {
            return Ok(String::new());
        }
        return Ok(s);
    }

    // Pathspec relative to worktree root (e.g. when cwd is not under the repo).
    let from_root = lexical_normalize_path(&wt_canon.join(pathspec_clean));
    if let Ok(rel) = from_root.strip_prefix(&wt_norm) {
        let s = rel.to_string_lossy().into_owned();
        if s == "." || s.is_empty() {
            return Ok(String::new());
        }
        return Ok(s);
    }

    if pathspec_clean == "." {
        return Ok(String::new());
    }
    if cwd_pathspec::has_parent_pathspec_component(pathspec_clean) {
        bail!("pathspec '{}' resolved outside the work tree", pathspec);
    }
    Ok(pathspec_clean.to_owned())
}

/// Walk the parent components of `rel_path` (relative to `work_tree`) and
/// return `Some(prefix)` if any of them is a symbolic link whose target
/// currently resolves (a *non-dangling* symlink).
///
/// `git rm d/f` must refuse to operate through a leading symlink that points
/// at a real directory, since the path the index entry names no longer maps to
/// a regular file under the work tree (t3600 "rm across a symlinked leading
/// path"). A *dangling* leading symlink is allowed: the work-tree file has
/// effectively vanished, so the entry is simply dropped from the index
/// (t3600 "rm of d/f when d has become a dangling symlink").
fn symlink_leading_path_resolves(work_tree: &Path, rel_path: &Path) -> Option<std::path::PathBuf> {
    let mut accumulated = std::path::PathBuf::new();
    let components: Vec<_> = rel_path.components().collect();
    for component in components.iter().take(components.len().saturating_sub(1)) {
        accumulated.push(component);
        let abs = work_tree.join(&accumulated);
        if let Ok(meta) = fs::symlink_metadata(&abs) {
            if meta.file_type().is_symlink() {
                // Only a symlink whose target exists blocks removal; a dangling
                // symlink leaves the named work-tree path unreachable, so the
                // index entry can be removed safely.
                if abs.metadata().is_ok() {
                    return Some(accumulated);
                }
            }
        }
    }
    None
}

fn has_glob_chars(s: &str) -> bool {
    s.contains('*') || s.contains('?') || s.contains('[')
}

fn glob_matches(pattern: &str, path: &str) -> bool {
    glob_matches_inner(pattern.as_bytes(), path.as_bytes())
}

fn glob_matches_inner(pattern: &[u8], path: &[u8]) -> bool {
    // Git pathspec matching uses `wildmatch(pattern, string, 0)` (no WM_PATHNAME) for plain
    // pathspecs, so `*`, `?`, and bracket classes all match `/` too. Thus `folder1/*`
    // matches `folder1/0/0/0`, not just `folder1/a` (t1092 rm pathspec outside cone).
    let mut pi = 0;
    let mut si = 0;
    let mut star_pi = usize::MAX;
    let mut star_si = 0;

    while si < path.len() {
        if pi < pattern.len() && pattern[pi] == b'?' {
            pi += 1;
            si += 1;
        } else if pi < pattern.len() && pattern[pi] == b'*' {
            if pi + 1 < pattern.len() && pattern[pi + 1] == b'*' {
                let rest = &pattern[pi + 2..];
                let rest = if !rest.is_empty() && rest[0] == b'/' {
                    &rest[1..]
                } else {
                    rest
                };
                for i in si..=path.len() {
                    if glob_matches_inner(rest, &path[i..]) {
                        return true;
                    }
                }
                return false;
            }
            star_pi = pi;
            star_si = si;
            pi += 1;
        } else if pi < pattern.len() && pattern[pi] == b'[' {
            pi += 1;
            let negate = pi < pattern.len() && (pattern[pi] == b'!' || pattern[pi] == b'^');
            if negate {
                pi += 1;
            }
            let mut found = false;
            let ch = path[si];
            while pi < pattern.len() && pattern[pi] != b']' {
                if pi + 2 < pattern.len() && pattern[pi + 1] == b'-' {
                    if ch >= pattern[pi] && ch <= pattern[pi + 2] {
                        found = true;
                    }
                    pi += 3;
                } else {
                    if ch == pattern[pi] {
                        found = true;
                    }
                    pi += 1;
                }
            }
            if pi < pattern.len() {
                pi += 1;
            }
            if found == negate {
                if star_pi != usize::MAX {
                    pi = star_pi + 1;
                    star_si += 1;
                    si = star_si;
                } else {
                    return false;
                }
            } else {
                si += 1;
            }
        } else if pi < pattern.len() && pattern[pi] == path[si] {
            pi += 1;
            si += 1;
        } else if star_pi != usize::MAX {
            pi = star_pi + 1;
            star_si += 1;
            si = star_si;
        } else {
            return false;
        }
    }

    while pi < pattern.len() && pattern[pi] == b'*' {
        pi += 1;
    }
    pi == pattern.len()
}

fn glob_pathspec_matches(pattern: &str, path: &str) -> bool {
    if glob_matches(pattern, path) {
        return true;
    }
    // For directory-like pathspecs (e.g. "*" or "dir*"), Git also matches
    // top-level path components and then applies recursion with -r.
    if let Some((first, _)) = path.split_once('/') {
        glob_matches(pattern, first)
    } else {
        false
    }
}

fn pathspec_matches(spec: &str, path: &str) -> bool {
    if spec.is_empty() {
        return true;
    }
    if has_glob_chars(spec) {
        return glob_pathspec_matches(spec, path);
    }
    path == spec || path.starts_with(&format!("{spec}/"))
}

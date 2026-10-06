//! `grit sparse-checkout` — manage sparse checkout patterns.
//!
//! Patterns live in `.git/info/sparse-checkout`. `core.sparseCheckout` and
//! `core.sparseCheckoutCone` are stored in `config.worktree` when present,
//! matching Git's worktree-local sparse settings.

use anyhow::{bail, Context, Result};
use clap::{Args as ClapArgs, Subcommand};
use grit_lib::config::{ConfigFile, ConfigScope};
use grit_lib::error::Error as GritError;
use grit_lib::ignore::path_in_sparse_checkout as path_in_sparse_checkout_lines;
use grit_lib::index::{MODE_GITLINK, MODE_TREE};
use grit_lib::objects::parse_commit;
use grit_lib::repo::Repository;
use grit_lib::sparse_checkout::{
    build_expanded_cone_sparse_checkout_lines, cone_directory_inputs_for_add,
    effective_cone_mode_for_sparse_file, load_sparse_checkout_with_warnings,
    parse_expanded_cone_recursive_dirs, path_in_sparse_checkout, ConePatterns, ConeWorkspace,
    NonConePatterns,
};
use grit_lib::state::resolve_head;
use std::collections::{BTreeSet, HashSet};
use std::fs;
use std::io::{self, BufRead, Write};
use std::path::{Path, PathBuf};

/// Arguments for `grit sparse-checkout`.
#[derive(Debug, ClapArgs)]
#[command(about = "Manage sparse checkout patterns")]
pub struct Args {
    #[command(subcommand)]
    pub subcommand: SparseCheckoutSubcommand,
}

#[derive(Debug, Subcommand)]
pub enum SparseCheckoutSubcommand {
    Init(InitArgs),
    Set(SetArgs),
    Add(AddArgs),
    Reapply(ReapplyArgs),
    List,
    Disable,
    CheckRules(CheckRulesArgs),
    Clean(CleanArgs),
}

#[derive(Debug, ClapArgs)]
pub struct InitArgs {
    #[arg(long)]
    pub cone: bool,
    #[arg(long = "no-cone")]
    pub no_cone: bool,
    #[arg(long)]
    pub sparse_index: bool,
    #[arg(long = "no-sparse-index")]
    pub no_sparse_index: bool,
}

#[derive(Debug, ClapArgs)]
pub struct SetArgs {
    #[arg(long)]
    pub cone: bool,
    #[arg(long = "no-cone")]
    pub no_cone: bool,
    #[arg(long)]
    pub sparse_index: bool,
    #[arg(long = "no-sparse-index")]
    pub no_sparse_index: bool,
    #[arg(long = "skip-checks")]
    pub skip_checks: bool,
    #[arg(long)]
    pub stdin: bool,
    #[arg(long = "end-of-options", hide = true)]
    pub end_of_options: bool,
    pub patterns: Vec<String>,
}

#[derive(Debug, ClapArgs)]
pub struct AddArgs {
    #[arg(long = "skip-checks")]
    pub skip_checks: bool,
    #[arg(long)]
    pub stdin: bool,
    #[arg(long = "end-of-options", hide = true)]
    pub end_of_options: bool,
    pub patterns: Vec<String>,
}

#[derive(Debug, ClapArgs)]
pub struct ReapplyArgs {
    #[arg(long)]
    pub cone: bool,
    #[arg(long = "no-cone")]
    pub no_cone: bool,
    #[arg(long)]
    pub sparse_index: bool,
    #[arg(long = "no-sparse-index")]
    pub no_sparse_index: bool,
}

#[derive(Debug, ClapArgs)]
pub struct CheckRulesArgs {
    #[arg(short = 'z')]
    pub nul: bool,
    #[arg(long)]
    pub cone: bool,
    #[arg(long = "no-cone")]
    pub no_cone: bool,
    #[arg(long = "rules-file", value_name = "FILE")]
    pub rules_file: Option<PathBuf>,
}

#[derive(Debug, ClapArgs)]
pub struct CleanArgs {
    #[arg(short = 'n', long = "dry-run")]
    pub dry_run: bool,
    #[arg(short = 'f', long)]
    pub force: bool,
    #[arg(short = 'v', long)]
    pub verbose: bool,
}

/// After `git clone --sparse`: enable cone sparse-checkout and optionally shrink the tree.
/// Copy `.git/info/sparse-checkout` from the main repo into a linked worktree admin dir.
pub(crate) fn copy_sparse_checkout_to_admin(source_git_dir: &Path, admin_dir: &Path) -> Result<()> {
    let src = source_git_dir.join("info").join("sparse-checkout");
    if !src.exists() {
        return Ok(());
    }
    let dst_dir = admin_dir.join("info");
    fs::create_dir_all(&dst_dir)?;
    fs::copy(&src, dst_dir.join("sparse-checkout"))?;
    Ok(())
}

/// Copy `.git/config.worktree` into a linked worktree admin dir (Git stores sparse-checkout toggles
/// there so each worktree can differ).
pub(crate) fn copy_worktree_config_to_admin(source_git_dir: &Path, admin_dir: &Path) -> Result<()> {
    let src = source_git_dir.join("config.worktree");
    if !src.exists() {
        return Ok(());
    }
    fs::copy(&src, admin_dir.join("config.worktree"))
        .context("copying config.worktree to linked worktree")?;
    Ok(())
}

pub(crate) fn finalize_sparse_clone(repo: &Repository, apply_to_index: bool) -> Result<()> {
    if apply_to_index {
        crate::commands::clone::ensure_index_from_head_if_missing(repo)?;
    }
    grit_lib::repo::init_worktree_config(&repo.git_dir)?;
    set_sparse_config(repo, true)?;
    set_cone_config(repo, true)?;
    let ws = ConeWorkspace::default();
    write_sparse_file(repo, &ws.to_sparse_checkout_file())?;
    if apply_to_index {
        let patterns = read_sparse_patterns(repo)?;
        apply_sparse_patterns(repo, &patterns, true)?;
    }
    Ok(())
}

/// Run `grit sparse-checkout`.
pub fn run(args: Args) -> Result<()> {
    let repo = Repository::discover(None).context("not a git repository")?;

    match args.subcommand {
        SparseCheckoutSubcommand::Init(a) => cmd_init(&repo, &a),
        SparseCheckoutSubcommand::Set(a) => cmd_set(&repo, &a),
        SparseCheckoutSubcommand::Add(a) => cmd_add(&repo, &a),
        SparseCheckoutSubcommand::Reapply(a) => cmd_reapply(&repo, &a),
        SparseCheckoutSubcommand::List => cmd_list(&repo),
        SparseCheckoutSubcommand::Disable => cmd_disable(&repo),
        SparseCheckoutSubcommand::CheckRules(a) => cmd_check_rules(&repo, &a),
        SparseCheckoutSubcommand::Clean(a) => cmd_clean(&repo, &a),
    }
}

fn tri_bool(cone: bool, no_cone: bool) -> Result<Option<bool>> {
    match (cone, no_cone) {
        (true, true) => bail!("cannot combine --cone and --no-cone"),
        (true, false) => Ok(Some(true)),
        (false, true) => Ok(Some(false)),
        (false, false) => Ok(None),
    }
}

fn tri_bool_sparse(sparse: bool, no_sparse: bool) -> Result<Option<bool>> {
    match (sparse, no_sparse) {
        (true, true) => bail!("cannot combine --sparse-index and --no-sparse-index"),
        (true, false) => Ok(Some(true)),
        (false, true) => Ok(Some(false)),
        (false, false) => Ok(None),
    }
}

fn worktree_config_path(repo: &Repository) -> PathBuf {
    repo.git_dir.join("config.worktree")
}

fn load_merged_config(repo: &Repository) -> grit_lib::config::ConfigSet {
    grit_lib::config::ConfigSet::load(Some(&repo.git_dir), true).unwrap_or_default()
}

fn sparse_checkout_path(repo: &Repository) -> PathBuf {
    repo.git_dir.join("info").join("sparse-checkout")
}

fn acquire_sparse_lock(repo: &Repository) -> Result<std::fs::File> {
    let lock_path = repo.git_dir.join("info").join("sparse-checkout.lock");
    if let Some(parent) = lock_path.parent() {
        fs::create_dir_all(parent)?;
    }
    match fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&lock_path)
    {
        Ok(f) => Ok(f),
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
            bail!("Unable to create '{}': File exists.", lock_path.display());
        }
        Err(e) => Err(e).context("sparse-checkout lock")?,
    }
}

fn release_sparse_lock(repo: &Repository) {
    let _ = fs::remove_file(repo.git_dir.join("info").join("sparse-checkout.lock"));
}

fn set_sparse_config(repo: &Repository, enable: bool) -> Result<()> {
    grit_lib::repo::init_worktree_config(&repo.git_dir)?;
    let path = worktree_config_path(repo);
    let content = fs::read_to_string(&path).unwrap_or_default();
    let mut cfg = ConfigFile::parse(&path, &content, ConfigScope::Worktree)?;
    cfg.set("core.sparseCheckout", if enable { "true" } else { "false" })?;
    cfg.write()?;
    Ok(())
}

fn set_cone_config(repo: &Repository, cone: bool) -> Result<()> {
    grit_lib::repo::init_worktree_config(&repo.git_dir)?;
    let path = worktree_config_path(repo);
    let content = fs::read_to_string(&path).unwrap_or_default();
    let mut cfg = ConfigFile::parse(&path, &content, ConfigScope::Worktree)?;
    cfg.set(
        "core.sparseCheckoutCone",
        if cone { "true" } else { "false" },
    )?;
    cfg.write()?;
    Ok(())
}

fn set_sparse_index_config(repo: &Repository, enable: bool) -> Result<()> {
    grit_lib::repo::init_worktree_config(&repo.git_dir)?;
    let path = worktree_config_path(repo);
    let content = fs::read_to_string(&path).unwrap_or_default();
    let mut cfg = ConfigFile::parse(&path, &content, ConfigScope::Worktree)?;
    cfg.set("index.sparse", if enable { "true" } else { "false" })?;
    cfg.write()?;
    Ok(())
}

fn read_sparse_file_content(repo: &Repository) -> String {
    let p = sparse_checkout_path(repo);
    fs::read_to_string(&p).unwrap_or_default()
}

fn write_sparse_file(repo: &Repository, content: &str) -> Result<()> {
    let sc_path = sparse_checkout_path(repo);
    if let Some(parent) = sc_path.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(&sc_path, content).context("writing sparse-checkout file")?;
    Ok(())
}

/// Initialize sparse-checkout after `clone --sparse` (matches `git clone --sparse`).
///
/// Writes `/*` and `!/*/` patterns, enables `core.sparseCheckout` and cone mode.
/// When `apply_worktree` is true, updates the index and working tree (normal clone).
pub(crate) fn init_clone_sparse_checkout(repo: &Repository, apply_worktree: bool) -> Result<()> {
    set_sparse_config(repo, true)?;
    set_cone_config(repo, true)?;

    let sc_path = sparse_checkout_path(repo);
    if let Some(parent) = sc_path.parent() {
        fs::create_dir_all(parent).context("creating info directory")?;
    }

    let patterns = vec!["/*".to_string(), "!/*/".to_string()];
    let content: String = patterns.iter().map(|p| format!("{p}\n")).collect();
    fs::write(&sc_path, &content).context("writing sparse-checkout file")?;
    if apply_worktree {
        apply_sparse_patterns(repo, &patterns, true)?;
    }
    Ok(())
}

fn head_tree_oid(repo: &Repository) -> Result<Option<grit_lib::objects::ObjectId>> {
    let head = resolve_head(&repo.git_dir).context("reading HEAD")?;
    let Some(commit_oid) = head.oid() else {
        return Ok(None);
    };
    let obj = repo.odb.read(commit_oid).context("reading HEAD commit")?;
    let commit = parse_commit(&obj.data).context("parsing HEAD commit")?;
    Ok(Some(commit.tree))
}

fn cmd_init(repo: &Repository, args: &InitArgs) -> Result<()> {
    let _work_tree = repo
        .work_tree
        .as_deref()
        .ok_or_else(|| anyhow::anyhow!("this operation must be run in a work tree"))?;

    let cone_opt = tri_bool(args.cone, args.no_cone)?;
    let sparse_idx_opt = tri_bool_sparse(args.sparse_index, args.no_sparse_index)?;

    let config = load_merged_config(repo);
    let was_sparse = config
        .get("core.sparseCheckout")
        .map(|v| v == "true")
        .unwrap_or(false);
    let prev_cone = config
        .get("core.sparseCheckoutCone")
        .and_then(|v| v.parse::<bool>().ok())
        .unwrap_or(true);

    // When sparse was off (e.g. after `sparse-checkout disable`), honor the saved
    // `core.sparseCheckoutCone` value instead of defaulting to cone — matches Git and
    // t7817 (non-cone superproject must stay non-cone across disable/init).
    let cone = match cone_opt {
        Some(c) => c,
        None if was_sparse => prev_cone,
        None => prev_cone,
    };

    set_sparse_config(repo, true)?;
    set_cone_config(repo, cone)?;
    if let Some(enable) = sparse_idx_opt {
        set_sparse_index_config(repo, enable)?;
    }

    let sc_path = sparse_checkout_path(repo);
    if let Some(parent) = sc_path.parent() {
        fs::create_dir_all(parent)?;
    }

    if sc_path.exists() {
        let patterns = read_sparse_patterns(repo)?;
        warn_sparse_apply_side_effects(repo, &patterns, cone, true)?;
        apply_sparse_patterns(repo, &patterns, cone)?;
        return Ok(());
    }

    // When the sparse-checkout file was removed (e.g. `sparse-checkout disable`),
    // Git recreates the default `/*` + `!/*/` pair before applying (see
    // `sparse_checkout_init` in sparse-checkout.c). A lone `/*` would leave every
    // top-level directory included, so `!b` in t7817 would never take effect.
    if head_tree_oid(repo)?.is_none() {
        write_sparse_file(repo, "/*\n!/*/\n")?;
        return Ok(());
    }

    if cone {
        let ws = ConeWorkspace::default();
        write_sparse_file(repo, &ws.to_sparse_checkout_file())?;
    } else {
        write_sparse_file(repo, "/*\n!/*/\n")?;
    }
    let patterns = read_sparse_patterns(repo)?;
    warn_sparse_apply_side_effects(repo, &patterns, cone, true)?;
    apply_sparse_patterns(repo, &patterns, cone)?;
    Ok(())
}

fn cmd_set(repo: &Repository, args: &SetArgs) -> Result<()> {
    let _wt = repo
        .work_tree
        .as_deref()
        .ok_or_else(|| anyhow::anyhow!("this operation must be run in a work tree"))?;

    let cone_opt = tri_bool(args.cone, args.no_cone)?;
    let sparse_idx_opt = tri_bool_sparse(args.sparse_index, args.no_sparse_index)?;

    let config = load_merged_config(repo);
    let prev_cone = config
        .get("core.sparseCheckoutCone")
        .and_then(|v| v.parse::<bool>().ok())
        .unwrap_or(true);
    let mut cone = cone_opt.unwrap_or(prev_cone);

    set_sparse_config(repo, true)?;
    if let Some(enable) = sparse_idx_opt {
        set_sparse_index_config(repo, enable)?;
    }

    let _lock = acquire_sparse_lock(repo)?;
    let result = (|| {
        if args.stdin {
            let stdin = io::stdin();
            let mut stdin = stdin.lock();
            let mut lines = read_stdin_lines(&mut stdin)?;
            if cone
                && lines.iter().any(|l| {
                    let t = l.trim();
                    !t.is_empty() && !t.starts_with('#') && t.starts_with('!')
                })
            {
                cone = false;
                set_cone_config(repo, false)?;
            } else {
                set_cone_config(repo, cone)?;
            }
            if cone {
                let mut dirs = Vec::new();
                for line in lines {
                    let p = normalize_cone_input_line(&line)?;
                    if !args.skip_checks {
                        validate_cone_patterns(repo, std::slice::from_ref(&p))?;
                    }
                    dirs.push(p);
                }
                let ws = ConeWorkspace::from_directory_list(&dirs);
                let body = ws.to_sparse_checkout_file();
                write_sparse_file(repo, &body)?;
                let patterns = read_sparse_patterns(repo)?;
                crate::commands::promisor_hydrate::hydrate_sparse_patterns_after_sparse_checkout_update(
                    repo, &patterns, true,
                )?;
                warn_sparse_apply_side_effects(repo, &patterns, true, true)?;
                apply_sparse_patterns(repo, &patterns, true)?;
            } else {
                if lines.is_empty() {
                    lines = vec!["/*".to_string(), "!/*/".to_string()];
                }
                let body: String = lines.iter().map(|l| format!("{l}\n")).collect();
                write_sparse_file(repo, &body)?;
                let patterns = read_sparse_patterns(repo)?;
                crate::commands::promisor_hydrate::hydrate_sparse_patterns_after_sparse_checkout_update(
                    repo, &patterns, false,
                )?;
                warn_sparse_apply_side_effects(repo, &patterns, false, true)?;
                apply_sparse_patterns(repo, &patterns, false)?;
            }
            Ok(())
        } else {
            if cone && args.patterns.iter().any(|p| p.starts_with('!')) {
                cone = false;
                set_cone_config(repo, false)?;
            } else {
                set_cone_config(repo, cone)?;
            }
            let mut pats = args.patterns.clone();
            sanitize_set_paths(
                repo,
                worktree_prefix(repo)?,
                cone,
                args.skip_checks,
                &mut pats,
            )?;
            if !cone && pats.is_empty() {
                pats = vec!["/*".to_string(), "!/*/".to_string()];
            }
            let mut file_only_cone = false;
            if cone {
                if !args.skip_checks {
                    validate_cone_patterns(repo, &pats)?;
                }
                file_only_cone = cone_patterns_are_all_tracked_files(repo, &pats)?;
                let effective_cone_dirs = !file_only_cone;
                if file_only_cone {
                    set_cone_config(repo, false)?;
                }
                let lines: Vec<String> = if effective_cone_dirs {
                    build_expanded_cone_sparse_checkout_lines(&pats)
                } else {
                    pats.clone()
                };
                let body: String = lines.iter().map(|l| format!("{l}\n")).collect();
                write_sparse_file(repo, &body)?;
                if file_only_cone {
                    set_cone_config(repo, true)?;
                }
            } else {
                let body: String = pats.iter().map(|p| format!("{p}\n")).collect();
                write_sparse_file(repo, &body)?;
            }
            let patterns = read_sparse_patterns(repo)?;
            let apply_cone = cone && !file_only_cone;
            crate::commands::promisor_hydrate::hydrate_sparse_patterns_after_sparse_checkout_update(
                repo, &patterns, apply_cone,
            )?;
            warn_sparse_apply_side_effects(repo, &patterns, apply_cone, true)?;
            apply_sparse_patterns(repo, &patterns, apply_cone)?;
            Ok(())
        }
    })();
    release_sparse_lock(repo);
    result
}

fn cmd_add(repo: &Repository, args: &AddArgs) -> Result<()> {
    let _wt = repo
        .work_tree
        .as_deref()
        .ok_or_else(|| anyhow::anyhow!("this operation must be run in a work tree"))?;

    let config = load_merged_config(repo);
    let sparse_enabled = config
        .get("core.sparseCheckout")
        .map(|v| v == "true")
        .unwrap_or(false);
    if !sparse_enabled {
        bail!("no sparse-checkout to add to");
    }
    let cone = config
        .get("core.sparseCheckoutCone")
        .and_then(|v| v.parse::<bool>().ok())
        .unwrap_or(true);

    if !args.stdin && args.patterns.is_empty() {
        bail!("specify directories to add");
    }

    let _lock = acquire_sparse_lock(repo)?;
    let result = (|| {
        if cone {
            // Sanitize first (Git `sanitize_paths` runs before `modify_pattern_list`): a tracked
            // regular file argument like `.gitignore` must die "is not a directory" even when the
            // existing on-disk file is not cone-formatted (t1091 test 59).
            let inputs = if args.stdin {
                let stdin = io::stdin();
                let mut stdin = stdin.lock();
                read_stdin_lines(&mut stdin)?
            } else {
                let mut p = args.patterns.clone();
                sanitize_add_paths(repo, worktree_prefix(repo)?, args.skip_checks, &mut p)?;
                p
            };
            let content = read_sparse_file_content(repo);
            if ConePatterns::try_parse(&content).is_none() {
                bail!("existing sparse-checkout patterns do not use cone mode");
            }
            let mut dirs = cone_directory_inputs_for_add(&content);
            for line in inputs {
                let p = normalize_cone_input_line(&line)?;
                if !args.skip_checks {
                    validate_cone_patterns(repo, std::slice::from_ref(&p))?;
                }
                dirs.push(p);
            }
            dirs.sort();
            dirs.dedup();
            let lines = build_expanded_cone_sparse_checkout_lines(&dirs);
            let body: String = lines.iter().map(|l| format!("{l}\n")).collect();
            write_sparse_file(repo, &body)?;
        } else {
            let mut patterns = read_sparse_patterns(repo)?;
            let extra = if args.stdin {
                let stdin = io::stdin();
                let mut stdin = stdin.lock();
                read_stdin_lines(&mut stdin)?
            } else {
                let mut p = args.patterns.clone();
                sanitize_add_paths(repo, worktree_prefix(repo)?, args.skip_checks, &mut p)?;
                p
            };
            for pat in extra {
                patterns.push(pat);
            }
            let body: String = patterns.iter().map(|p| format!("{p}\n")).collect();
            write_sparse_file(repo, &body)?;
        }
        let patterns = read_sparse_patterns(repo)?;
        crate::commands::promisor_hydrate::hydrate_sparse_patterns_after_sparse_checkout_update(
            repo, &patterns, cone,
        )?;
        warn_sparse_apply_side_effects(repo, &patterns, cone, true)?;
        apply_sparse_patterns(repo, &patterns, cone)?;
        Ok(())
    })();
    release_sparse_lock(repo);
    result
}

fn cmd_reapply(repo: &Repository, args: &ReapplyArgs) -> Result<()> {
    let _wt = repo
        .work_tree
        .as_deref()
        .ok_or_else(|| anyhow::anyhow!("this operation must be run in a work tree"))?;

    let config = load_merged_config(repo);
    let sparse_enabled = config
        .get("core.sparseCheckout")
        .map(|v| v == "true")
        .unwrap_or(false);
    if !sparse_enabled {
        bail!("must be in a sparse-checkout to reapply sparsity patterns");
    }

    let cone_opt = tri_bool(args.cone, args.no_cone)?;
    let sparse_idx_opt = tri_bool_sparse(args.sparse_index, args.no_sparse_index)?;

    if let Some(cone) = cone_opt {
        set_cone_config(repo, cone)?;
    }
    if let Some(enable) = sparse_idx_opt {
        set_sparse_index_config(repo, enable)?;
    }

    let config = load_merged_config(repo);
    let cone = config
        .get("core.sparseCheckoutCone")
        .and_then(|v| v.parse::<bool>().ok())
        .unwrap_or(true);
    let patterns = read_sparse_patterns(repo)?;
    crate::commands::promisor_hydrate::hydrate_sparse_patterns_after_sparse_checkout_update(
        repo, &patterns, cone,
    )?;
    warn_sparse_apply_side_effects(repo, &patterns, cone, true)?;
    apply_sparse_patterns(repo, &patterns, cone)?;
    Ok(())
}

fn cmd_list(repo: &Repository) -> Result<()> {
    let _wt = repo
        .work_tree
        .as_deref()
        .ok_or_else(|| anyhow::anyhow!("this operation must be run in a work tree"))?;

    let config = load_merged_config(repo);
    let sparse_enabled = config
        .get("core.sparseCheckout")
        .map(|v| v == "true")
        .unwrap_or(false);
    if !sparse_enabled {
        bail!("this worktree is not sparse");
    }

    let sc_path = sparse_checkout_path(repo);
    if !sc_path.exists() {
        eprintln!("warning: this worktree is not sparse (sparse-checkout file may not exist)");
        return Ok(());
    }

    let content = match fs::read_to_string(&sc_path) {
        Ok(c) => c,
        Err(_) => {
            eprintln!("warning: this worktree is not sparse (sparse-checkout file may not exist)");
            return Ok(());
        }
    };
    let cone_cfg = config
        .get("core.sparseCheckoutCone")
        .and_then(|v| v.parse::<bool>().ok())
        .unwrap_or(true);

    let lines: Vec<String> = content
        .lines()
        .map(|l| l.trim().to_string())
        .filter(|l| !l.is_empty() && !l.starts_with('#'))
        .collect();

    let stdout = io::stdout();
    let mut out = stdout.lock();

    if cone_cfg {
        // Match Git `sparse-checkout list`: parse the file in cone mode, printing any
        // structural warnings (repeated / unrecognized patterns). When cone parsing fails,
        // Git disables cone matching and lists the raw pattern lines verbatim.
        let mut warnings = Vec::new();
        let parsed = ConePatterns::try_parse_with_warnings(&content, &mut warnings);
        for w in &warnings {
            eprintln!("{w}");
        }
        match parsed {
            Some(cp) => {
                // List the leaf (recursive) cone directories the user added — Git uses the
                // recursive hashset (the `/dir/` lines with no trailing `!/dir/*/`).
                let mut dirs = parse_expanded_cone_recursive_dirs(&lines);
                if dirs.is_empty() {
                    dirs = ConeWorkspace::from_cone_patterns(&cp).list_cone_directories();
                }
                dirs.sort();
                dirs.dedup();
                // Git `sparse-checkout list` C-quotes directory names that contain bytes needing
                // escaping (backslash, control chars, …) so output round-trips with `ls-tree`.
                let quote_fully = config
                    .get("core.quotePath")
                    .map(|v| v != "false")
                    .unwrap_or(true);
                for d in dirs {
                    writeln!(
                        out,
                        "{}",
                        grit_lib::quote_path::quote_c_style(&d, quote_fully)
                    )?;
                }
            }
            None => {
                for line in &lines {
                    writeln!(out, "{line}")?;
                }
            }
        }
        return Ok(());
    }

    for line in &lines {
        writeln!(out, "{line}")?;
    }
    Ok(())
}

fn cmd_disable(repo: &Repository) -> Result<()> {
    let _work_tree = repo
        .work_tree
        .as_deref()
        .ok_or_else(|| anyhow::anyhow!("this operation must be run in a work tree"))?;

    // Match Git `sparse_checkout_disable`: build an in-memory `/*` pattern list to expand the
    // work tree to a full checkout, but do **not** overwrite `info/sparse-checkout` with only
    // `/*` — that would erase stored rules like `!b` and break a later `sparse-checkout init`
    // (t7817).
    set_sparse_config(repo, true)?;
    set_sparse_index_config(repo, false)?;

    let patterns = vec!["/*".to_string()];
    warn_sparse_apply_side_effects(repo, &patterns, false, false)?;
    apply_full_checkout(repo)?;

    // Git `set_config(MODE_NO_PATTERNS)`: record the sparse toggles as **false** in the worktree
    // config (not unset). A later `sparse-checkout init` reads the recorded cone mode so a
    // non-cone repo stays non-cone (t7817), and `index.sparse=false` is asserted directly (t1091).
    set_sparse_config(repo, false)?;
    set_cone_config(repo, false)?;
    set_sparse_index_config(repo, false)?;
    // Remove any stale keys recorded in the local (non-worktree) config layer so the worktree
    // values win and `git config <key>` (default scope) reports them as absent (t1091 test 19).
    clear_sparse_keys_local_layer(repo)?;
    Ok(())
}

fn clear_sparse_keys_local_layer(repo: &Repository) -> Result<()> {
    let path = repo.git_dir.join("config");
    if path.exists() {
        let content = fs::read_to_string(&path).unwrap_or_default();
        let mut cfg = ConfigFile::parse(&path, &content, ConfigScope::Local)?;
        let _ = cfg.unset("core.sparseCheckout");
        let _ = cfg.unset("core.sparseCheckoutCone");
        let _ = cfg.unset("index.sparse");
        cfg.write()?;
    }
    Ok(())
}

fn cmd_check_rules(repo: &Repository, args: &CheckRulesArgs) -> Result<()> {
    let cone_opt = tri_bool(args.cone, args.no_cone)?;
    let config = load_merged_config(repo);
    let mut cone = config
        .get("core.sparseCheckoutCone")
        .and_then(|v| v.parse::<bool>().ok())
        .unwrap_or(true);
    if let Some(c) = cone_opt {
        cone = c;
    }
    if args.rules_file.is_some() && cone_opt.is_none() {
        cone = true;
    }

    let (cone_pat, non_cone, effective_cone) = if let Some(ref rf) = args.rules_file {
        let text = fs::read_to_string(rf).with_context(|| rf.display().to_string())?;
        if cone {
            let mut dirs = Vec::new();
            for line in text.lines() {
                let t = line.trim();
                if t.is_empty() || t.starts_with('#') {
                    continue;
                }
                let p = normalize_cone_input_line(t)?;
                dirs.push(p);
            }
            let ws = ConeWorkspace::from_directory_list(&dirs);
            let file_body = ws.to_sparse_checkout_file();
            let mut w = Vec::new();
            let cp = ConePatterns::try_parse_with_warnings(&file_body, &mut w);
            let ec = cp.is_some();
            (cp, NonConePatterns::parse(&file_body), ec)
        } else {
            (None, NonConePatterns::parse(&text), false)
        }
    } else {
        let mut w = Vec::new();
        let (_ok, cp, nc) = load_sparse_checkout_with_warnings(&repo.git_dir, cone, &mut w);
        let sparse_content = read_sparse_file_content(repo);
        let ec = cone && ConePatterns::try_parse(&sparse_content).is_some();
        (cp, nc, ec)
    };

    let stdin = io::stdin();
    let mut stdin = stdin.lock();
    let mut line = Vec::new();
    let stdout = io::stdout();
    let mut out = stdout.lock();

    if args.nul {
        loop {
            line.clear();
            let n = stdin.read_until(0, &mut line)?;
            if n == 0 {
                break;
            }
            if line.last() == Some(&0) {
                line.pop();
            }
            let path = String::from_utf8_lossy(&line);
            let path = path.as_ref();
            if path_in_sparse_checkout(
                path,
                effective_cone,
                cone_pat.as_ref(),
                &non_cone,
                repo.work_tree.as_deref(),
            ) {
                out.write_all(line.as_slice())?;
                out.write_all(&[0])?;
            }
        }
    } else {
        loop {
            line.clear();
            let n = stdin.read_until(b'\n', &mut line)?;
            if n == 0 {
                break;
            }
            while line.last() == Some(&b'\n') || line.last() == Some(&b'\r') {
                line.pop();
            }
            let raw = String::from_utf8_lossy(&line);
            // Git `check_rules`: C-unquote a `"`-quoted input line before matching, then re-quote
            // the matched path on output via `write_name_quoted` (t1091 check-rules quoting).
            let path = if raw.starts_with('"') {
                unquote_c_style(raw.as_ref())?
            } else {
                raw.into_owned()
            };
            if path_in_sparse_checkout(
                &path,
                effective_cone,
                cone_pat.as_ref(),
                &non_cone,
                repo.work_tree.as_deref(),
            ) {
                let quote_fully = config
                    .get("core.quotePath")
                    .map(|v| v != "false")
                    .unwrap_or(true);
                writeln!(
                    out,
                    "{}",
                    grit_lib::quote_path::quote_c_style(&path, quote_fully)
                )?;
            }
        }
    }
    Ok(())
}

/// Sparse directories `convert_to_sparse` would collapse: every stage-0 path under the directory
/// is skip-worktree and the directory is entirely outside the cone. Unlike the sparse-*index*
/// collapse this does not require entries to match HEAD (a staged out-of-cone edit still counts).
/// Returns the shallowest such directories (so `folder2`, not `folder2/sub`).
fn removable_sparse_directories(
    index: &grit_lib::index::Index,
    patterns: &[String],
) -> Vec<String> {
    let mut dir_state: BTreeSet<String> = BTreeSet::new();
    let mut not_all_skip: HashSet<String> = HashSet::new();
    for entry in &index.entries {
        if entry.stage() != 0 {
            continue;
        }
        let path = String::from_utf8_lossy(&entry.path);
        let skip = entry.skip_worktree();
        for (i, &b) in path.as_bytes().iter().enumerate() {
            if b == b'/' {
                let dir = path[..i].to_string();
                if !skip {
                    not_all_skip.insert(dir.clone());
                }
                dir_state.insert(dir);
            }
        }
    }
    let out_of_cone_dir = |dir: &str| -> bool {
        let with_slash = format!("{}/", dir.trim_end_matches('/'));
        !grit_lib::sparse_checkout::path_matches_sparse_patterns(&with_slash, patterns, true)
    };
    let mut removable: Vec<String> = dir_state
        .into_iter()
        .filter(|dir| !not_all_skip.contains(dir) && out_of_cone_dir(dir))
        .collect();
    removable.sort();
    let mut shallow: Vec<String> = Vec::new();
    for dir in removable {
        if shallow
            .iter()
            .any(|p| dir == *p || dir.starts_with(&format!("{p}/")))
        {
            continue;
        }
        shallow.push(dir);
    }
    shallow
}

/// Whether a sparse directory holds a *non-ignored* untracked file on disk (recursively). Git's
/// `clean_tracked_sparse_directories` uses `fill_directory` (which excludes gitignored paths), so a
/// directory whose only contents are gitignored files is still removable.
fn dir_has_unignored_file(
    repo: &Repository,
    matcher: &mut grit_lib::ignore::IgnoreMatcher,
    index: Option<&grit_lib::index::Index>,
    work_tree: &Path,
    dir: &Path,
) -> bool {
    let Ok(rd) = fs::read_dir(dir) else {
        return false;
    };
    for ent in rd.flatten() {
        let Ok(meta) = ent.metadata() else { continue };
        let path = ent.path();
        let rel = normalize_rel_path(path.strip_prefix(work_tree).unwrap_or(&path));
        let ignored = matcher
            .check_path(repo, index, &rel, meta.is_dir())
            .map(|(i, _)| i)
            .unwrap_or(false);
        if ignored {
            // An ignored directory (and everything under it) is removable; an ignored file too.
            continue;
        }
        if meta.is_dir() {
            if dir_has_unignored_file(repo, matcher, index, work_tree, &path) {
                return true;
            }
        } else {
            return true;
        }
    }
    false
}

/// Git `clean_tracked_sparse_directories`: after applying sparse patterns, remove the now
/// out-of-cone sparse directories that hold only tracked/ignored content, and warn about those
/// that still contain non-ignored untracked files (t1091 'cone mode clears ignored subdirectories'
/// / 'clean with sparse file states').
fn clean_tracked_sparse_directories(
    repo: &Repository,
    work_tree: &Path,
    index: &grit_lib::index::Index,
    patterns: &[String],
) {
    let mut matcher = match grit_lib::ignore::IgnoreMatcher::from_repository(repo) {
        Ok(m) => m,
        Err(_) => return,
    };
    for name in removable_sparse_directories(index, patterns) {
        let full = work_tree.join(&name);
        if !full.is_dir() {
            continue;
        }
        if dir_has_unignored_file(repo, &mut matcher, Some(index), work_tree, &full) {
            eprintln!(
                "warning: directory '{name}/' contains untracked files, but is not in the sparse-checkout cone"
            );
        } else {
            let _ = fs::remove_dir_all(&full);
        }
    }
}

fn cmd_clean(repo: &Repository, args: &CleanArgs) -> Result<()> {
    let work_tree = repo
        .work_tree
        .as_deref()
        .ok_or_else(|| anyhow::anyhow!("this operation must be run in a work tree"))?;

    let config = load_merged_config(repo);
    let sparse_enabled = config
        .get("core.sparseCheckout")
        .map(|v| v == "true")
        .unwrap_or(false);
    if !sparse_enabled {
        bail!("must be in a sparse-checkout to clean directories");
    }
    let cone = config
        .get("core.sparseCheckoutCone")
        .and_then(|v| v.parse::<bool>().ok())
        .unwrap_or(true);
    if !cone {
        bail!("must be in a cone-mode sparse-checkout to clean directories");
    }

    let require_force = config
        .get("clean.requireForce")
        .map(|v| v == "true")
        .unwrap_or(true);
    if require_force && !args.force && !args.dry_run {
        bail!("for safety, refusing to clean without one of --force or --dry-run");
    }

    let index_path = repo.index_path();
    let mut index = repo.load_index_at(&index_path).context("reading index")?;

    // Git `sparse_checkout_clean`: convert the index to a sparse index in memory and remove the
    // sparse-directory placeholders. Unmerged entries block the conversion, mirroring
    // `convert_to_sparse` failing (t1091 'operations with merge conflicts').
    if index.entries.iter().any(|e| e.stage() != 0) {
        bail!("failed to convert index to a sparse index; resolve merge conflicts and try again");
    }
    if index.has_sparse_directory_placeholders() {
        index
            .expand_sparse_directory_placeholders(&repo.odb)
            .context("expanding sparse index before clean")?;
    }
    let patterns = read_sparse_patterns(repo)?;
    let placeholders = removable_sparse_directories(&index, &patterns);

    let msg_remove = "Removing ";
    let msg_would = "Would remove ";
    let msg = if args.dry_run { msg_would } else { msg_remove };

    for name in placeholders {
        let full = work_tree.join(&name);
        if !full.is_dir() {
            continue;
        }
        if args.verbose {
            for rel in list_files_under_dir(&full, work_tree)? {
                writeln!(io::stdout(), "{msg}{rel}")?;
            }
        } else {
            writeln!(io::stdout(), "{msg}{name}/")?;
        }
        if !args.dry_run {
            let _ = fs::remove_dir_all(&full);
        }
    }

    Ok(())
}

fn worktree_prefix(repo: &Repository) -> Result<String> {
    let wt = repo
        .work_tree
        .as_deref()
        .ok_or_else(|| anyhow::anyhow!("bare"))?;
    let cwd = std::env::current_dir()?;
    let wt = wt.canonicalize().unwrap_or_else(|_| wt.to_path_buf());
    let cwd = cwd.canonicalize().unwrap_or(cwd);
    if let Ok(rest) = cwd.strip_prefix(&wt) {
        let s = rest.to_string_lossy().replace('\\', "/");
        if s.is_empty() {
            return Ok(String::new());
        }
        Ok(format!("{}/", s.trim_end_matches('/')))
    } else {
        Ok(String::new())
    }
}

fn sanitize_set_paths(
    repo: &Repository,
    prefix: String,
    cone: bool,
    skip_checks: bool,
    args: &mut [String],
) -> Result<()> {
    if args.is_empty() {
        return Ok(());
    }
    // Mirrors Git `sanitize_paths` (builtin/sparse-checkout.c). Prefix-prepend only happens in
    // cone mode (args are not pathspecs).
    if !prefix.is_empty() && cone {
        let work_tree = repo
            .work_tree
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("this operation must be run in a work tree"))?;
        for a in args.iter_mut() {
            if let Some(p) = grit_lib::git_path::prefix_path_gently(&prefix, a, work_tree) {
                *a = p;
            }
        }
    }
    if skip_checks {
        return Ok(());
    }
    if !prefix.is_empty() && !cone {
        bail!("please run from the toplevel directory in non-cone mode");
    }
    if cone {
        for a in args.iter() {
            if a.starts_with('/') {
                bail!("specify directories rather than patterns (no leading slash)");
            }
            if a.starts_with('!') {
                bail!("specify directories rather than patterns.  If your directory starts with a '!', pass --skip-checks");
            }
            if a.contains('*') || a.contains('?') || a.contains('[') {
                bail!("specify directories rather than patterns.  If your directory really has any of '*?[]\\' in it, pass --skip-checks");
            }
        }
    }
    // Index file-check: a tracked, non-sparse-dir entry is a regular file. Cone mode rejects it;
    // non-cone mode warns about the missing leading slash (Git `sanitize_paths`).
    check_tracked_file_args(repo, cone, args)?;
    Ok(())
}

fn sanitize_add_paths(
    repo: &Repository,
    prefix: String,
    skip_checks: bool,
    args: &mut [String],
) -> Result<()> {
    let config = load_merged_config(repo);
    let cone = config
        .get("core.sparseCheckoutCone")
        .and_then(|v| v.parse::<bool>().ok())
        .unwrap_or(true);
    sanitize_set_paths(repo, prefix, cone, skip_checks, args)
}

fn normalize_cone_input_line(line: &str) -> Result<String> {
    let mut s = line.trim().to_string();
    if s.starts_with('"') {
        s = unquote_c_style(&s)?;
    }
    s = s.trim_end_matches('/').to_string();
    let normalized = grit_lib::git_path::normalize_path_copy(&s)
        .map_err(|_| anyhow::anyhow!("could not normalize path {s}"))?;
    Ok(normalized.trim_start_matches('/').to_string())
}

fn unquote_c_style(s: &str) -> Result<String> {
    if !s.starts_with('"') || !s.ends_with('"') || s.len() < 2 {
        return Ok(s.to_string());
    }
    let inner = &s[1..s.len() - 1];
    let mut out = String::new();
    let mut chars = inner.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\\' {
            let Some(n) = chars.next() else {
                bail!("invalid escape");
            };
            match n {
                'n' => out.push('\n'),
                't' => out.push('\t'),
                '"' => out.push('"'),
                '\\' => out.push('\\'),
                _ => out.push(n),
            }
        } else {
            out.push(c);
        }
    }
    Ok(out)
}

fn read_stdin_lines<R: BufRead>(r: &mut R) -> Result<Vec<String>> {
    let mut v = Vec::new();
    for line in r.lines() {
        v.push(line?);
    }
    Ok(v)
}

fn cone_patterns_are_all_tracked_files(repo: &Repository, patterns: &[String]) -> Result<bool> {
    if patterns.is_empty() {
        return Ok(false);
    }
    let index_path = repo.index_path();
    let index =
        grit_lib::index::Index::load(&index_path).context("reading index for cone heuristics")?;
    for pat in patterns {
        let p = pat.trim().trim_start_matches('/').trim_end_matches('/');
        if p.is_empty() || p.contains('/') {
            return Ok(false);
        }
        let Some(ce) = index.get(p.as_bytes(), 0) else {
            return Ok(false);
        };
        if ce.is_sparse_directory_placeholder() || ce.mode == MODE_TREE {
            return Ok(false);
        }
    }
    Ok(true)
}

fn validate_cone_patterns(repo: &Repository, patterns: &[String]) -> Result<()> {
    check_tracked_file_args(repo, true, patterns)
}

/// Git `sanitize_paths` index file-check: for each arg that is a tracked, non-sparse-dir index
/// entry (i.e. a regular file), cone mode dies "is not a directory" while non-cone mode warns
/// to use a leading slash for a single file (t1091).
fn check_tracked_file_args(repo: &Repository, cone: bool, patterns: &[String]) -> Result<()> {
    let index_path = repo.index_path();
    let index =
        grit_lib::index::Index::load(&index_path).context("reading index for validation")?;
    for pat in patterns {
        let p = pat.trim_end_matches('/');
        if p.is_empty() {
            continue;
        }
        if let Some(ce) = index.get(p.as_bytes(), 0) {
            if ce.is_sparse_directory_placeholder() {
                continue;
            }
            if cone {
                // A tracked, non-tree index entry is a regular file. Git's cone-mode set/add
                // rejects files; pass --skip-checks to treat it as a directory anyway.
                bail!(
                    "'{}' is not a directory; to treat it as a directory anyway, rerun with --skip-checks",
                    p
                );
            }
            eprintln!(
                "warning: pass a leading slash before paths such as '{p}' if you want a single file (see NON-CONE PROBLEMS in the git-sparse-checkout manual)."
            );
        }
        // No exact index entry: allowed (matches Git `sanitize_paths` / `index_name_pos`).
    }
    Ok(())
}

fn read_sparse_patterns(repo: &Repository) -> Result<Vec<String>> {
    let sc_path = sparse_checkout_path(repo);
    if !sc_path.exists() {
        return Ok(Vec::new());
    }
    let content = fs::read_to_string(&sc_path).context("reading sparse-checkout file")?;
    Ok(content
        .lines()
        .map(|l| l.trim())
        .filter(|l| !l.is_empty() && !l.starts_with('#'))
        .map(String::from)
        .collect())
}

/// Re-run sparse-checkout pattern application after commands that rebuild the index
/// (e.g. `git reset --hard`), matching Git's behaviour of preserving sparsity.
pub(crate) fn reapply_sparse_checkout_if_configured(repo: &Repository) -> Result<()> {
    let config = grit_lib::config::ConfigSet::load(Some(&repo.git_dir), true).unwrap_or_default();
    let sparse_enabled = config
        .get("core.sparseCheckout")
        .map(|v| v == "true")
        .unwrap_or(false);
    if !sparse_enabled {
        return Ok(());
    }
    let sc_path = sparse_checkout_path(repo);
    if !sc_path.exists() {
        return Ok(());
    }
    let content = fs::read_to_string(&sc_path).context("reading sparse-checkout file")?;
    let lines: Vec<String> = content
        .lines()
        .map(|l| l.trim())
        .filter(|l| !l.is_empty() && !l.starts_with('#'))
        .map(String::from)
        .collect();
    if lines.is_empty() {
        return Ok(());
    }
    let cone_cfg = config
        .get("core.sparseCheckoutCone")
        .and_then(|v| v.parse::<bool>().ok())
        .unwrap_or(true);
    apply_sparse_patterns(repo, &lines, cone_cfg)
}

fn path_included_for_sparse_apply(
    path: &str,
    patterns: &[String],
    cone_mode: bool,
    _file_content: &str,
    cone_struct: Option<&ConePatterns>,
    non_cone: &NonConePatterns,
    work_tree: Option<&Path>,
) -> bool {
    let effective_cone =
        effective_cone_mode_for_sparse_file(cone_mode, patterns) && cone_struct.is_some();
    if effective_cone {
        path_in_sparse_checkout(path, true, cone_struct, non_cone, work_tree)
    } else {
        path_in_sparse_checkout_lines(path, patterns, work_tree)
    }
}

/// Excluded paths Git leaves on disk when applying sparse patterns: unmerged entries and
/// not-up-to-date entries (on-disk content differs from the indexed OID). These are reported as
/// warnings and must NOT be removed / marked skip-worktree (`unpack-trees` `verify_uptodate` /
/// t1091 tests 27, 31, 32).
struct SparseSideEffects {
    unmerged: BTreeSet<String>,
    not_uptodate: BTreeSet<String>,
}

fn compute_sparse_side_effect_paths(
    repo: &Repository,
    index: &grit_lib::index::Index,
    patterns: &[String],
    cone_mode: bool,
) -> SparseSideEffects {
    let work_tree = repo.work_tree.as_deref();
    let file_content = read_sparse_file_content(repo);
    let cone_struct = if effective_cone_mode_for_sparse_file(cone_mode, patterns) {
        ConePatterns::try_parse(&file_content)
    } else {
        None
    };
    let non_cone = NonConePatterns::from_lines(patterns.to_vec());

    let mut unmerged = BTreeSet::new();
    for entry in &index.entries {
        if entry.stage() != 0 {
            unmerged.insert(String::from_utf8_lossy(&entry.path).into_owned());
        }
    }

    let mut not_uptodate = BTreeSet::new();
    if let Some(work_tree) = work_tree {
        for entry in &index.entries {
            if entry.stage() != 0 || entry.mode == MODE_TREE {
                continue;
            }
            let path_str = String::from_utf8_lossy(&entry.path);
            // A path being excluded by the new patterns is what matters; the current skip-worktree
            // bit is irrelevant (a second sparse op over an already-excluded dirty file must still
            // warn, t1091 test 31).
            if path_included_for_sparse_apply(
                path_str.as_ref(),
                patterns,
                cone_mode,
                &file_content,
                cone_struct.as_ref(),
                &non_cone,
                Some(work_tree),
            ) {
                continue;
            }
            let full = work_tree.join(path_str.as_ref());
            let Ok(meta) = fs::symlink_metadata(&full) else {
                continue;
            };
            if !meta.is_file() && !meta.file_type().is_symlink() {
                continue;
            }
            let differs = match (repo.odb.read(&entry.oid), fs::read(&full)) {
                (Ok(obj), Ok(disk)) => obj.data != disk,
                _ => true,
            };
            if differs {
                not_uptodate.insert(path_str.into_owned());
            }
        }
    }

    SparseSideEffects {
        unmerged,
        not_uptodate,
    }
}

/// Warn about worktree paths Git leaves when applying sparse patterns (`unpack-trees` / t1091).
fn warn_sparse_apply_side_effects(
    repo: &Repository,
    patterns: &[String],
    cone_mode: bool,
    warn_not_uptodate: bool,
) -> Result<()> {
    if repo.work_tree.is_none() || !repo.index_path().exists() {
        return Ok(());
    }
    let index = repo
        .load_index()
        .context("reading index for sparse warnings")?;
    let effects = compute_sparse_side_effect_paths(repo, &index, patterns, cone_mode);

    if !effects.unmerged.is_empty() {
        eprintln!(
            "warning: The following paths are unmerged and were left despite sparse patterns:"
        );
        for path in &effects.unmerged {
            eprintln!("{path}");
        }
    }

    if warn_not_uptodate && !effects.not_uptodate.is_empty() {
        eprintln!(
            "warning: The following paths are not up to date and were left despite sparse patterns:"
        );
        for path in &effects.not_uptodate {
            eprintln!("{path}");
        }
    }
    Ok(())
}

fn apply_sparse_patterns(repo: &Repository, patterns: &[String], cone_mode: bool) -> Result<()> {
    apply_sparse_patterns_inner(repo, patterns, cone_mode, false)
}

/// Expand the whole worktree to a full checkout (Git `sparse-checkout disable`): clear every
/// skip-worktree bit, materialize all blobs, and never re-collapse the index.
fn apply_full_checkout(repo: &Repository) -> Result<()> {
    apply_sparse_patterns_inner(repo, &["/*".to_string()], false, true)
}

fn apply_sparse_patterns_inner(
    repo: &Repository,
    patterns: &[String],
    cone_mode: bool,
    force_include_all: bool,
) -> Result<()> {
    let work_tree = repo
        .work_tree
        .as_deref()
        .ok_or_else(|| anyhow::anyhow!("bare repository cannot use sparse checkout"))?;
    let config = load_merged_config(repo);
    let sparse_index_enabled = config
        .get("index.sparse")
        .map(|v| v == "true")
        .unwrap_or(false);

    let index_path = repo.index_path();
    // `git clone --no-checkout` leaves no index until the first real checkout. Sparse-checkout
    // may still update `info/sparse-checkout` and config; Git does not create `.git/index` until
    // checkout (t1091).
    if !index_path.exists() {
        return Ok(());
    }
    let mut index = repo.load_index_at(&index_path).context("reading index")?;
    if index.entries.is_empty() {
        crate::commands::clone::ensure_index_from_head_if_missing(repo)?;
        index = repo
            .load_index_at(&index_path)
            .context("reading index after building from HEAD")?;
    }

    if index.version < 3 {
        index.version = 3;
    }

    // A sparse index collapses out-of-cone directories into MODE_TREE placeholders. To re-apply
    // patterns per file (clear skip-worktree, materialize blobs) we must first expand those
    // placeholders to a full index, otherwise widening the cone (e.g. `disable`) never writes the
    // collapsed directories' files to disk (t1091 'sparse-checkout disable').
    if index.has_sparse_directory_placeholders() {
        index
            .expand_sparse_directory_placeholders(&repo.odb)
            .context("expanding sparse index before applying patterns")?;
    }

    let file_content = read_sparse_file_content(repo);
    let expanded_cone_shape = effective_cone_mode_for_sparse_file(cone_mode, patterns);
    let cone_struct = if expanded_cone_shape {
        ConePatterns::try_parse(&file_content)
    } else {
        None
    };
    let effective_cone = expanded_cone_shape && cone_struct.is_some();
    let non_cone = NonConePatterns::from_lines(patterns.to_vec());

    // Git's `unpack-trees` refuses to drop not-up-to-date / unmerged paths (`verify_uptodate`).
    // Keep those excluded files on disk (do not clear/remove) so sparsifying never destroys local
    // edits — only warn (t1091 tests 27, 31, 32). A full checkout includes everything anyway.
    let protected: HashSet<String> = if force_include_all {
        HashSet::new()
    } else {
        let effects = compute_sparse_side_effect_paths(repo, &index, patterns, cone_mode);
        effects
            .not_uptodate
            .into_iter()
            .chain(effects.unmerged)
            .collect()
    };

    for entry in &mut index.entries {
        if entry.mode == MODE_TREE {
            continue;
        }
        let path_str = String::from_utf8_lossy(&entry.path).to_string();
        // Non-cone mode must use Git's `path_in_sparse_checkout` (parent walk + last-match),
        // not `NonConePatterns::path_included` (sequential toggles). See t3602-rm-sparse-checkout.
        let matches = if force_include_all {
            true
        } else if effective_cone {
            path_in_sparse_checkout(
                &path_str,
                true,
                cone_struct.as_ref(),
                &non_cone,
                Some(work_tree),
            )
        } else {
            path_in_sparse_checkout_lines(&path_str, patterns, Some(work_tree))
        };

        if matches {
            if entry.skip_worktree() {
                entry.set_skip_worktree(false);
                let full_path = work_tree.join(&path_str);
                if !full_path.exists() {
                    if let Some(parent) = full_path.parent() {
                        let _ = fs::create_dir_all(parent);
                    }
                    let blob_data = match repo.odb.read(&entry.oid) {
                        Ok(obj) => Some(obj.data),
                        Err(GritError::ObjectNotFound(_)) => {
                            if crate::commands::promisor_hydrate::try_lazy_fetch_promisor_object(
                                repo, entry.oid,
                            )
                            .is_ok()
                            {
                                repo.odb.read(&entry.oid).ok().map(|o| o.data)
                            } else {
                                None
                            }
                        }
                        Err(_) => None,
                    };
                    if let Some(data) = blob_data {
                        let _ = fs::write(&full_path, &data);
                    }
                }
                // Refresh the cached stat data so subsequent stat-only comparisons
                // (diff-files, status) see the materialized file as up to date rather
                // than reporting a spurious modification. Mirrors checkout's stat-fill.
                if let Ok(meta) = fs::symlink_metadata(&full_path) {
                    use std::os::unix::fs::MetadataExt as _;
                    entry.ctime_sec = meta.ctime() as u32;
                    entry.ctime_nsec = meta.ctime_nsec() as u32;
                    entry.mtime_sec = meta.mtime() as u32;
                    entry.mtime_nsec = meta.mtime_nsec() as u32;
                    entry.dev = meta.dev() as u32;
                    entry.ino = meta.ino() as u32;
                    entry.size = meta.size() as u32;
                }
            }
        } else if protected.contains(&path_str) {
            // Not-up-to-date or unmerged: leave the file on disk and keep it in the worktree (do
            // not set skip-worktree) so the local edit/conflict is preserved (t1091 27/31/32).
        } else {
            entry.set_skip_worktree(true);
            let full_path = work_tree.join(&path_str);
            if fs::symlink_metadata(&full_path).is_ok() {
                let _ = fs::remove_file(&full_path);
                if let Some(parent) = full_path.parent() {
                    remove_empty_dirs_up_to(parent, work_tree);
                }
            }
        }
    }

    // Git `clean_tracked_sparse_directories` (runs after `update_working_directory` for
    // set/reapply): remove the now out-of-cone sparse directories with no files, warn about those
    // still holding untracked files. Skip for a full checkout (everything is in-cone) and in cone
    // mode only (matches Git). Computed from the pre-collapse per-file skip-worktree state.
    if !force_include_all && effective_cone {
        clean_tracked_sparse_directories(repo, work_tree, &index, patterns);
    }

    // In partial clones (`grit-promisor-missing` lists blobs not yet local), sparse
    // directory collapse would expand excluded subtrees into the index and pull blob
    // OIDs into scope — breaking `rev-list --missing=print` expectations (t5620).
    let promisor_marker = repo.git_dir.join("grit-promisor-missing");
    let skip_collapse = fs::read_to_string(&promisor_marker)
        .map(|s| {
            s.lines()
                .any(|l| l.len() == 40 && l.chars().all(|c| c.is_ascii_hexdigit()))
        })
        .unwrap_or(false);

    if force_include_all {
        // A full checkout (`disable`) must never re-collapse the index into sparse placeholders.
        index.sparse_directories = false;
    } else if !skip_collapse {
        if let Some(tree_oid) = head_tree_oid(repo)? {
            index.try_collapse_sparse_directories(
                &repo.odb,
                &tree_oid,
                patterns,
                effective_cone,
                sparse_index_enabled,
            )?;
        } else {
            index.sparse_directories = false;
        }
    } else {
        index.sparse_directories = false;
    }

    repo.write_index_at(&index_path, &mut index)
        .context("writing index")?;

    // Prune tracked sparse directories that fell out of the cone, mirroring Git's
    // `clean_tracked_sparse_directories` (builtin/sparse-checkout.c). The helper runs ONLY in
    // cone mode (it returns early when not cone, like Git's `!use_cone_patterns` guard) and
    // never deletes untracked files: a directory containing untracked content is preserved.
    // A full checkout (`disable`) includes everything, so there is nothing to prune (t7012).
    if !force_include_all {
        let indexed_paths: HashSet<String> = index
            .entries
            .iter()
            .map(|e| String::from_utf8_lossy(&e.path).into_owned())
            .collect();
        let gitlink_paths: HashSet<String> = index
            .entries
            .iter()
            .filter(|e| e.stage() == 0 && e.mode == MODE_GITLINK)
            .map(|e| String::from_utf8_lossy(&e.path).into_owned())
            .collect();
        remove_untracked_outside_sparse(
            work_tree,
            work_tree,
            &indexed_paths,
            &gitlink_paths,
            effective_cone,
            cone_struct.as_ref(),
            &non_cone,
        )?;
    }

    // Submodule work trees keep their own `info/sparse-checkout`. After the superproject applies
    // sparsity we skip cleaning inside gitlink dirs (so we do not delete `sub/B/b`), so re-run the
    // submodule's sparse rules so paths like `sub/A` are pruned (t7817).
    for entry in &index.entries {
        if entry.stage() != 0 || entry.mode != MODE_GITLINK {
            continue;
        }
        let rel = String::from_utf8_lossy(&entry.path);
        let included = if effective_cone {
            path_in_sparse_checkout(
                rel.as_ref(),
                true,
                cone_struct.as_ref(),
                &non_cone,
                Some(work_tree),
            )
        } else {
            path_in_sparse_checkout_lines(rel.as_ref(), patterns, Some(work_tree))
        };
        if !included {
            continue;
        }
        let sub_wt = work_tree.join(rel.as_ref());
        if let Ok(sub_repo) = open_gitlink_worktree_repo(&sub_wt) {
            let _ = reapply_sparse_checkout_if_configured(&sub_repo);
        }
    }

    Ok(())
}

fn open_gitlink_worktree_repo(sub_work_tree: &Path) -> Result<Repository> {
    let git_path = sub_work_tree.join(".git");
    if !git_path.try_exists().context("stat submodule .git")? {
        bail!("missing .git in {}", sub_work_tree.display());
    }
    if git_path.is_dir() {
        Repository::open(&git_path, Some(sub_work_tree)).context("open submodule repository")
    } else {
        let content =
            fs::read_to_string(&git_path).with_context(|| git_path.display().to_string())?;
        let gitdir = content
            .trim()
            .strip_prefix("gitdir: ")
            .ok_or_else(|| anyhow::anyhow!("invalid gitdir file {}", git_path.display()))?;
        let gitdir_path = if Path::new(gitdir).is_absolute() {
            PathBuf::from(gitdir)
        } else {
            sub_work_tree.join(gitdir)
        };
        let gitdir_path = gitdir_path
            .canonicalize()
            .with_context(|| format!("resolve gitdir {}", gitdir_path.display()))?;
        Repository::open(&gitdir_path, Some(sub_work_tree)).context("open submodule repository")
    }
}

/// Convert a worktree-relative path to a `/`-separated string. On Windows the native separator is
/// `\`, so it is rewritten to `/`; on Unix a `\` is a *literal* filename character and must be
/// preserved (t1091 escaped-characters: a directory literally named `zbad\dir`).
fn normalize_rel_path(p: &Path) -> String {
    let s = p.to_string_lossy();
    if std::path::MAIN_SEPARATOR == '\\' {
        s.replace('\\', "/")
    } else {
        s.into_owned()
    }
}

/// Remove whole tracked directory subtrees that have fallen out of the sparse
/// cone, mirroring Git's `clean_tracked_sparse_directories`
/// (git/builtin/sparse-checkout.c).
///
/// Git only does this in cone mode (it returns early when
/// `!use_cone_patterns`), and it never deletes individual untracked files. It
/// considers each tracked sparse directory that exists on disk; if the
/// directory contains any untracked-or-ignored files it warns
/// ("contains untracked files") and leaves the directory in place, otherwise it
/// removes the whole subtree. Top-level untracked/ignored files (e.g. `file.o`,
/// `obj/`) are always preserved.
fn remove_untracked_outside_sparse(
    work_tree: &Path,
    current: &Path,
    indexed_paths: &HashSet<String>,
    gitlink_paths: &HashSet<String>,
    effective_cone: bool,
    cone_struct: Option<&ConePatterns>,
    non_cone: &NonConePatterns,
) -> Result<()> {
    // Non-cone mode: Git cannot safely delete directories outside the cone, so
    // it cleans nothing here. Matches the early return in
    // clean_tracked_sparse_directories.
    if !effective_cone {
        return Ok(());
    }

    // Directories that hold tracked content. A directory is "tracked" when some
    // index entry lives inside it.
    let mut tracked_dirs: HashSet<String> = HashSet::new();
    for p in indexed_paths {
        let mut rest = p.as_str();
        while let Some(idx) = rest.rfind('/') {
            let dir = &rest[..idx];
            if !tracked_dirs.insert(dir.to_string()) {
                break;
            }
            rest = dir;
        }
    }

    clean_tracked_sparse_dirs(
        work_tree,
        current,
        indexed_paths,
        gitlink_paths,
        &tracked_dirs,
        cone_struct,
        non_cone,
    )
}

fn clean_tracked_sparse_dirs(
    work_tree: &Path,
    current: &Path,
    indexed_paths: &HashSet<String>,
    gitlink_paths: &HashSet<String>,
    tracked_dirs: &HashSet<String>,
    cone_struct: Option<&ConePatterns>,
    non_cone: &NonConePatterns,
) -> Result<()> {
    let Ok(read_dir) = fs::read_dir(current) else {
        return Ok(());
    };
    for ent in read_dir {
        let ent = ent.context("reading work tree directory")?;
        let path = ent.path();
        let rel = normalize_rel_path(path.strip_prefix(work_tree).unwrap_or(&path));
        // Skip the main repo's `.git` and every nested `.git` (e.g. `sub/.git` for submodules).
        if rel == ".git"
            || rel.starts_with(".git/")
            || rel.ends_with("/.git")
            || rel.contains("/.git/")
        {
            continue;
        }
        let meta = fs::symlink_metadata(&path).context("stat work tree path")?;
        if !meta.is_dir() {
            // Git never deletes loose untracked/ignored files here.
            continue;
        }
        if gitlink_paths.contains(&rel) {
            continue;
        }

        let included = path_in_sparse_checkout(&rel, true, cone_struct, non_cone, Some(work_tree));
        if included {
            // Still in the cone: descend to find deeper out-of-cone tracked dirs.
            clean_tracked_sparse_dirs(
                work_tree,
                &path,
                indexed_paths,
                gitlink_paths,
                tracked_dirs,
                cone_struct,
                non_cone,
            )?;
            continue;
        }
        // Out of cone. Only remove a directory that holds tracked content (a
        // tracked sparse directory). Purely untracked/ignored directories at the
        // top level (e.g. `obj/`) are preserved, matching Git. Untracked files
        // (and symlinks) outside the cone are NEVER removed here (only
        // `sparse-checkout clean` does) — deleting them would destroy user data
        // and breaks tests staging helper files outside the cone (t3602/t3705).
        if !tracked_dirs.contains(&rel) {
            continue;
        }

        if dir_has_untracked(&path, work_tree, indexed_paths)? {
            eprintln!(
                "warning: directory '{rel}/' contains untracked files, but is not in the sparse-checkout cone"
            );
            continue;
        }

        // No untracked files: safe to remove the whole subtree.
        let _ = fs::remove_dir_all(&path);
        if let Some(parent) = path.parent() {
            remove_empty_dirs_up_to(parent, work_tree);
        }
    }
    Ok(())
}

/// Whether `dir` contains any file on disk that is not a tracked index entry
/// (mirrors Git's `fill_directory` with `DIR_SHOW_IGNORED_TOO`: both untracked
/// and ignored files count).
fn dir_has_untracked(
    dir: &Path,
    work_tree: &Path,
    indexed_paths: &HashSet<String>,
) -> Result<bool> {
    let Ok(read_dir) = fs::read_dir(dir) else {
        return Ok(false);
    };
    for ent in read_dir {
        let ent = ent.context("reading work tree directory")?;
        let path = ent.path();
        let rel = path
            .strip_prefix(work_tree)
            .unwrap_or(&path)
            .to_string_lossy()
            .replace('\\', "/");
        if rel == ".git"
            || rel.starts_with(".git/")
            || rel.ends_with("/.git")
            || rel.contains("/.git/")
        {
            continue;
        }
        let meta = fs::symlink_metadata(&path).context("stat work tree path")?;
        if meta.is_dir() {
            if dir_has_untracked(&path, work_tree, indexed_paths)? {
                return Ok(true);
            }
        } else if !indexed_paths.contains(&rel) {
            return Ok(true);
        }
    }
    Ok(false)
}

/// Whether `path` is included in the sparse checkout for the given patterns.
///
/// Used by `grit backfill --sparse` and promisor hydrate to mirror Git's path-walk sparse filtering.
pub(crate) fn path_matches_sparse_patterns(
    path: &str,
    patterns: &[String],
    cone_mode: bool,
) -> bool {
    if cone_mode {
        return grit_lib::sparse_checkout::path_matches_sparse_patterns(path, patterns, cone_mode);
    }
    path_in_sparse_checkout_lines(path, patterns, None)
}

fn list_files_under_dir(dir: &Path, work_tree: &Path) -> Result<Vec<String>> {
    let mut out = Vec::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        for ent in fs::read_dir(&d).with_context(|| d.display().to_string())? {
            let ent = ent?;
            let p = ent.path();
            let meta = ent.metadata()?;
            if meta.is_dir() {
                stack.push(p);
            } else if meta.is_file() {
                let rel = p.strip_prefix(work_tree).unwrap_or(&p);
                out.push(rel.to_string_lossy().replace('\\', "/"));
            }
        }
    }
    out.sort();
    Ok(out)
}

fn remove_empty_dirs_up_to(dir: &Path, stop: &Path) {
    let mut current = dir.to_path_buf();
    while current != stop {
        if let Ok(mut entries) = fs::read_dir(&current) {
            if entries.next().is_some() {
                break;
            }
            let _ = fs::remove_dir(&current);
        } else {
            break;
        }
        match current.parent() {
            Some(p) => current = p.to_path_buf(),
            None => break,
        }
    }
}

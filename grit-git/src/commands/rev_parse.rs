//! `grit rev-parse` - pick out and massage revision parameters.

use crate::grit_exe;
use anyhow::{bail, Context, Result};
use clap::Args as ClapArgs;
use grit_lib::error::Error as LibError;
use grit_lib::git_date::approx::approxidate_careful;
use grit_lib::merge_base;
use grit_lib::ref_exclusions::{git_namespace_prefix, strip_git_namespace, RefExclusions};
use grit_lib::refs;
use grit_lib::repo::Repository;
use grit_lib::rev_parse::{
    abbreviate_object_id, abbreviate_ref_name, ambiguous_object_hint_lines, discover_optional,
    expand_parent_shorthand_rev_parse_lines, is_inside_git_dir, is_inside_work_tree,
    list_all_abbrev_matches, parse_peel_suffix, peel_to_commit_for_merge_base, resolve_revision,
    resolve_revision_for_range_end, resolve_revision_without_index_dwim, show_prefix,
    spec_has_parent_shorthand_suffix, split_double_dot_range, split_triple_dot_range,
    superproject_work_tree_from_nested_git_modules, symbolic_full_name, to_relative_path,
};
use std::borrow::Cow;
use std::env;
use std::path::{Path, PathBuf};
use std::process::Command;

/// Strip a single leading uninteresting `^` (Git revision machinery).
///
/// Returns `(true, rest)` when `spec` is `^<rev>` with a non-empty rest that does not start with
/// `^` (so `^^foo` is not treated as negated). Matches `git rev-parse` for plumbing output.
fn strip_leading_uninteresting_caret(spec: &str) -> (bool, &str) {
    let Some(rest) = spec.strip_prefix('^') else {
        return (false, spec);
    };
    if rest.is_empty() || rest.starts_with('^') {
        return (false, spec);
    }
    (true, rest)
}

/// Arguments for `grit rev-parse`.
#[derive(Debug, ClapArgs)]
pub struct Args {
    /// Raw command arguments forwarded by the CLI parser.
    #[arg(value_name = "ARG", num_args = 0.., allow_hyphen_values = true, trailing_var_arg = true)]
    pub args: Vec<String>,
}

fn realpath_forgiving(path: &Path) -> PathBuf {
    path.canonicalize().unwrap_or_else(|_| path.to_path_buf())
}

fn is_linked_worktree_git_dir(git_dir: &Path) -> bool {
    git_dir
        .components()
        .any(|c| c.as_os_str() == std::ffi::OsStr::new("worktrees"))
}

/// Compute the gitdir string the way Git's `setup_git_directory` would leave
/// `the_repository->gitdir` for the current `cwd`. Git keeps the gitdir relative
/// to the working directory when it was discovered by walking up to a literal
/// `.git` directory (yielding `.git`, `../.git`, …), but keeps it absolute when
/// the `.git` is a gitfile pointing elsewhere (separate gitdir / linked
/// worktree) or `GIT_DIR` named an explicit path.
fn setup_relative_git_dir(git_dir: &Path, cwd: &Path) -> PathBuf {
    if let Ok(gd) = std::env::var("GIT_DIR") {
        if !gd.is_empty() {
            return PathBuf::from(gd);
        }
    }
    let git_a = realpath_forgiving(git_dir);
    let cwd_a = realpath_forgiving(cwd);
    // At the gitdir itself (typical for a bare repo): Git reports ".".
    if git_a == cwd_a {
        return PathBuf::from(".");
    }
    // A gitfile (`.git` is a file) keeps the absolute target. Linked worktree
    // admin dirs live under `.../worktrees/<name>` and are likewise absolute.
    if cwd.join(".git").is_file() || is_linked_worktree_git_dir(&git_a) {
        return git_a;
    }
    // `.git` discovered as a directory in cwd or an ancestor: keep it relative.
    if git_a.file_name() == Some(std::ffi::OsStr::new(".git")) {
        if let Some(parent) = git_a.parent() {
            let parent_a = realpath_forgiving(parent);
            if cwd_a == parent_a || cwd_a.starts_with(&parent_a) {
                return PathBuf::from(to_relative_path(&git_a, &cwd_a));
            }
        }
    }
    git_a
}

/// Mirror Git's `get_common_dir_noenv`: if the gitdir has a `commondir` file the
/// common dir is the canonical absolute path it points at, otherwise it is the
/// gitdir itself (in the cwd-relative form Git's setup would have produced).
fn computed_common_dir(git_dir: &Path, cwd: &Path) -> PathBuf {
    if let Ok(common) = std::env::var("GIT_COMMON_DIR") {
        if !common.is_empty() {
            return PathBuf::from(common);
        }
    }
    if let Some(common) = refs::common_dir(git_dir) {
        return common;
    }
    setup_relative_git_dir(git_dir, cwd)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum PathDefaultMode {
    /// Pass through `path` without canonicalization (e.g. literal `.git`).
    Unmodified,
    /// Realpath / canonical absolute (default for `--git-dir` inside a work tree).
    Canonical,
    /// `relative_path(realpath(path), realpath(cwd))` (Git `DEFAULT_RELATIVE`).
    RelativeToCwd,
    /// Git `DEFAULT_RELATIVE_IF_SHARED` (`--git-path` default).
    RelativeIfShared,
}

fn print_rev_parse_path(
    path: &Path,
    cwd: &Path,
    cli_prefix: Option<&Path>,
    path_format_absolute: Option<bool>,
    default_mode: PathDefaultMode,
) {
    let path_abs = realpath_forgiving(path);
    let cwd_abs = realpath_forgiving(cwd);
    match path_format_absolute {
        Some(true) => {
            println!("{}", path_abs.display());
        }
        Some(false) => {
            let base = cli_prefix
                .map(realpath_forgiving)
                .unwrap_or_else(|| cwd_abs.clone());
            println!("{}", to_relative_path(&path_abs, &base));
        }
        None => match default_mode {
            PathDefaultMode::Unmodified => {
                println!("{}", path.display());
            }
            PathDefaultMode::Canonical => {
                println!("{}", path_abs.display());
            }
            PathDefaultMode::RelativeToCwd => {
                println!("{}", to_relative_path(&path_abs, &cwd_abs));
            }
            PathDefaultMode::RelativeIfShared => {
                let prefix = cli_prefix.filter(|p| !p.as_os_str().is_empty());
                let base_abs = match prefix {
                    Some(base) => realpath_forgiving(base),
                    None => cwd_abs.clone(),
                };
                if paths_share_root(&path_abs, &base_abs) {
                    // Git's `DEFAULT_RELATIVE_IF_SHARED` uses `relative_path()`,
                    // which copies the path suffix verbatim. Use the faithful
                    // port so internal `//` runs (e.g. `--git-path
                    // info//sparse-checkout`) survive instead of being collapsed
                    // by component-based normalization.
                    let path_s = path_abs.to_string_lossy();
                    let base_s = base_abs.to_string_lossy();
                    let mut sb = String::new();
                    match grit_lib::git_path::relative_path(&path_s, &base_s, &mut sb) {
                        Some(rel) => println!("{rel}"),
                        None => println!("{}", to_relative_path(&path_abs, &base_abs)),
                    }
                } else {
                    println!("{}", path_abs.display());
                }
            }
        },
    }
}

/// Faithful port of Git's `print_path(path, prefix, format, DEFAULT_RELATIVE_IF_SHARED)`
/// as used by `--git-common-dir`.
///
/// * `--path-format=absolute` (`Some(true)`) → realpath, absolute.
/// * `--path-format=relative` (`Some(false)`) → relative to `--prefix` or cwd
///   after canonicalizing both ends.
/// * default (`None`) → `relative_path(path, prefix)`, where `prefix` is NULL
///   unless an explicit `--prefix` was given. A NULL prefix prints `path`
///   verbatim; a sharing prefix yields a relative path.
fn print_common_dir_path(path: &Path, cli_prefix: Option<&Path>, fmt: Option<bool>) {
    match fmt {
        Some(true) => {
            println!("{}", realpath_forgiving(path).display());
        }
        Some(false) => {
            let cwd = std::env::current_dir().unwrap_or_default();
            let base = cli_prefix
                .map(realpath_forgiving)
                .unwrap_or_else(|| realpath_forgiving(&cwd));
            println!("{}", to_relative_path(&realpath_forgiving(path), &base));
        }
        None => match cli_prefix {
            // Git passes the command-line prefix straight through; with no
            // `--prefix` it is NULL and `relative_path` returns the path as-is.
            None => println!("{}", path.display()),
            Some(prefix) => {
                let path_abs = realpath_forgiving(path);
                let prefix_abs = realpath_forgiving(prefix);
                let path_s = path_abs.to_string_lossy();
                let prefix_s = prefix_abs.to_string_lossy();
                let mut sb = String::new();
                match grit_lib::git_path::relative_path(&path_s, &prefix_s, &mut sb) {
                    Some(rel) => println!("{rel}"),
                    None => println!("{}", path.display()),
                }
            }
        },
    }
}

/// True when `a` and `b` are under the same filesystem root (Git `have_same_root`).
fn paths_share_root(a: &Path, b: &Path) -> bool {
    use std::path::Component;
    let mut ac = a.components().filter(|c| !matches!(c, Component::CurDir));
    let mut bc = b.components().filter(|c| !matches!(c, Component::CurDir));
    match (ac.next(), bc.next()) {
        (Some(Component::RootDir), Some(Component::RootDir)) => true,
        (Some(Component::Prefix(pa)), Some(Component::Prefix(pb))) => pa == pb,
        (Some(Component::Normal(a0)), Some(Component::Normal(b0))) => a0 == b0,
        _ => false,
    }
}

fn read_extensions_refstorage(git_dir: &Path) -> String {
    let config_path = git_dir.join("config");
    let Ok(content) = std::fs::read_to_string(&config_path) else {
        return "files".to_string();
    };
    let mut in_ext = false;
    let mut found = String::from("files");
    for line in content.lines() {
        let t = line.trim();
        if t.starts_with('[') {
            in_ext = t.eq_ignore_ascii_case("[extensions]");
            continue;
        }
        if in_ext {
            if let Some((k, v)) = t.split_once('=') {
                if k.trim().eq_ignore_ascii_case("refstorage") {
                    found = v.trim().to_owned();
                }
            }
        }
    }
    found
}

fn ref_storage_format_is_valid(raw: &str) -> bool {
    let lower = raw.to_ascii_lowercase();
    let name = lower
        .split_once(':')
        .map(|(a, _)| a)
        .unwrap_or(lower.as_str());
    matches!(name, "files" | "reftable")
}

fn path_relative_to_base(base: &Path, target: &Path) -> Option<String> {
    let base_a = realpath_forgiving(base);
    let target_a = realpath_forgiving(target);
    let base_c: Vec<_> = base_a.components().collect();
    let target_c: Vec<_> = target_a.components().collect();
    let mut common = 0usize;
    let max = base_c.len().min(target_c.len());
    while common < max && base_c[common] == target_c[common] {
        common += 1;
    }
    if common < base_c.len() {
        return None;
    }
    let rest: Vec<_> = target_c
        .iter()
        .skip(common)
        .map(|c| c.as_os_str().to_string_lossy().into_owned())
        .collect();
    Some(if rest.is_empty() {
        ".".to_owned()
    } else {
        rest.join("/")
    })
}

fn superproject_working_tree_via_ls_files(repo: &Repository, cwd: &Path) -> Option<PathBuf> {
    let work_tree = repo.work_tree.as_ref()?;
    let rel_s = path_relative_to_base(work_tree, cwd)?;
    let spec = if rel_s.is_empty() {
        "."
    } else {
        rel_s.as_str()
    };
    let mut cmd = Command::new(grit_exe::grit_executable());
    grit_exe::strip_trace2_env(&mut cmd);
    cmd.current_dir(work_tree)
        .args(["ls-files", "-z", "--stage", "--full-name", "--", spec])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null());
    let output = cmd.output().ok()?;
    if output.status.code() == Some(128) || !output.status.success() {
        return None;
    }
    let data = output.stdout;
    if !data.starts_with(b"160000 ") {
        return None;
    }
    let tab = data.iter().position(|&b| b == b'\t')?;
    let path_bytes = &data[tab + 1..data.len().saturating_sub(1)];
    let super_sub = std::str::from_utf8(path_bytes).ok()?.trim_end_matches('\0');
    let cwd_s = cwd.to_string_lossy();
    if super_sub.len() > cwd_s.len() {
        return None;
    }
    if !cwd_s.ends_with(super_sub) {
        return None;
    }
    let trim = cwd_s.len() - super_sub.len();
    let mut super_wt = cwd_s[..trim].to_string();
    while super_wt.ends_with('/') {
        super_wt.pop();
    }
    Some(PathBuf::from(super_wt))
}

/// Run `rev-parse` with argv as passed after the subcommand (preserves `--` for path separation).
///
/// Clap strips `--` from positional lists; `git rev-parse` relies on it, so the main binary
/// bypasses clap for this command and forwards raw args here.
pub fn run_with_raw_args(rest: &[String]) -> Result<()> {
    crate::commands::upstream_synopsis_help::try_print_upstream_help_and_exit("rev-parse", rest);
    run(Args {
        args: rest.to_vec(),
    })
}

/// Run `grit rev-parse`.
pub fn run(args: Args) -> Result<()> {
    // Handle --parseopt mode: parse option spec from stdin, emit parsed args
    if args.args.first().map(|s| s.as_str()) == Some("--parseopt") {
        return run_parseopt(&args.args[1..]);
    }

    let cwd = env::current_dir().context("failed to read current directory")?;

    // Global modifier flags (these modify behavior but don't produce output themselves)
    let mut verify = false;
    let mut quiet = false;
    let mut sq_quote = false;
    let mut short_len: Option<usize> = None;
    let mut show_symbolic_full_name = false;
    let mut show_symbolic_asis = false;
    let mut abbrev_ref = false;
    let mut prefix: Option<String> = None;
    let mut default_rev: Option<String> = None;
    let mut revs_only = false;
    let mut no_revs = false;
    let mut no_flags = false;
    let mut sq_output = false;
    let mut path_format_absolute: Option<bool> = None;
    let mut cli_prefix_path: Option<PathBuf> = None;

    // Collect ordered actions for sequential output
    // Each action captures the flag state at time of parsing
    #[derive(Debug)]
    enum Action {
        ShowIsInsideWorkTree,
        ShowIsInsideGitDir,
        ShowIsBare,
        ShowIsShallow,
        ShowToplevel(Option<bool>),
        ShowPrefix,
        ShowCdup,
        ShowGitDir(Option<bool>),
        ShowSharedIndexPath,
        ShowGitCommonDir(Option<bool>),
        ShowAbsoluteGitDir,
        ShowSuperprojectWorkingTree(Option<bool>),
        ShowRefFormat,
        ShowObjectFormat(String),
        GitPath(Option<bool>, String),
        BisectRefs(bool),
        MaxAge(String),
        MinAge(String),
        All,
        Branches(Option<String>),
        Tags(Option<String>),
        Remotes(Option<String>),
        Glob(String),
        Exclude(String),
        ExcludeHidden(String),
        LocalEnvVars,
        ResolveGitDir(String),
        Revision(String, bool, bool, bool), // (rev_spec, symbolic_full_name, symbolic_asis, strict_before_first_dd)
        ForcedPath(String),
        PathSeparator,
        Literal(String),
        Disambiguate(String),
    }

    let mut actions: Vec<Action> = Vec::new();
    let mut end_of_options = false;
    let mut saw_path_separator = false;
    let first_path_sep_dd = args.args.iter().position(|a| a == "--");

    // First pass: parse all arguments and build ordered action list
    let mut i = 0usize;
    while i < args.args.len() {
        let arg = &args.args[i];
        if !end_of_options && arg == "--" {
            end_of_options = true;
            saw_path_separator = true;
            actions.push(Action::PathSeparator);
            i += 1;
            continue;
        }
        if end_of_options {
            if arg == "--" {
                saw_path_separator = true;
                actions.push(Action::PathSeparator);
                i += 1;
                continue;
            }
            if saw_path_separator {
                actions.push(Action::ForcedPath(arg.clone()));
            } else {
                let strict = first_path_sep_dd.is_some_and(|dd| i < dd);
                actions.push(Action::Revision(
                    arg.clone(),
                    show_symbolic_full_name,
                    show_symbolic_asis,
                    strict,
                ));
            }
            i += 1;
            continue;
        }
        if !end_of_options && arg.starts_with('-') {
            if arg == "--path-format=absolute" {
                path_format_absolute = Some(true);
                i += 1;
                continue;
            } else if arg == "--path-format=relative" {
                path_format_absolute = Some(false);
                i += 1;
                continue;
            } else if arg == "--path-format" {
                i += 1;
                let val = args
                    .args
                    .get(i)
                    .ok_or_else(|| anyhow::anyhow!("--path-format requires an argument"))?;
                match val.as_str() {
                    "absolute" => path_format_absolute = Some(true),
                    "relative" => path_format_absolute = Some(false),
                    other => bail!("unknown argument to --path-format: {other}"),
                }
                i += 1;
                continue;
            } else if arg == "--verify" {
                verify = true;
            } else if arg == "--quiet" || arg == "-q" {
                quiet = true;
            } else if arg == "--is-inside-work-tree" {
                actions.push(Action::ShowIsInsideWorkTree);
            } else if arg == "--is-inside-git-dir" {
                actions.push(Action::ShowIsInsideGitDir);
            } else if arg == "--is-shallow-repository" {
                actions.push(Action::ShowIsShallow);
            } else if arg == "--is-bare-repository" {
                actions.push(Action::ShowIsBare);
            } else if arg == "--show-toplevel" {
                actions.push(Action::ShowToplevel(path_format_absolute));
            } else if arg == "--show-superproject-working-tree" {
                actions.push(Action::ShowSuperprojectWorkingTree(path_format_absolute));
            } else if arg == "--show-prefix" {
                actions.push(Action::ShowPrefix);
            } else if arg == "--show-cdup" {
                actions.push(Action::ShowCdup);
            } else if arg == "--symbolic-full-name" {
                show_symbolic_full_name = true;
            } else if arg == "--symbolic" {
                show_symbolic_asis = true;
            } else if arg == "--abbrev-ref" {
                abbrev_ref = true;
            } else if arg == "--git-dir" {
                actions.push(Action::ShowGitDir(path_format_absolute));
            } else if arg == "--shared-index-path" {
                actions.push(Action::ShowSharedIndexPath);
            } else if arg == "--git-common-dir" {
                actions.push(Action::ShowGitCommonDir(path_format_absolute));
            } else if arg == "--absolute-git-dir" {
                actions.push(Action::ShowAbsoluteGitDir);
            } else if arg == "--bisect" {
                actions.push(Action::BisectRefs(show_symbolic_full_name));
            } else if let Some(date) = arg.strip_prefix("--since=") {
                actions.push(Action::MaxAge(date.to_owned()));
            } else if let Some(date) = arg.strip_prefix("--after=") {
                actions.push(Action::MaxAge(date.to_owned()));
            } else if let Some(date) = arg.strip_prefix("--before=") {
                actions.push(Action::MinAge(date.to_owned()));
            } else if let Some(date) = arg.strip_prefix("--until=") {
                actions.push(Action::MinAge(date.to_owned()));
            } else if arg == "--git-path" {
                i += 1;
                let path_arg = args
                    .args
                    .get(i)
                    .ok_or_else(|| anyhow::anyhow!("--git-path requires an argument"))?;
                actions.push(Action::GitPath(path_format_absolute, path_arg.clone()));
            } else if arg == "--prefix" {
                i += 1;
                let value = args
                    .args
                    .get(i)
                    .ok_or_else(|| anyhow::anyhow!("--prefix requires an argument"))?;
                prefix = Some(value.clone());
                cli_prefix_path = Some(cwd.join(value));
            } else if let Some(value) = arg.strip_prefix("--prefix=") {
                prefix = Some(value.to_owned());
                cli_prefix_path = Some(cwd.join(value));
            } else if let Some(value) = arg.strip_prefix("--short=") {
                short_len = Some(parse_short_len(value)?);
            } else if arg == "--short" {
                // Default short length will be resolved later from core.abbrev
                short_len = Some(0);
            } else if arg == "--default" {
                i += 1;
                let value = args
                    .args
                    .get(i)
                    .ok_or_else(|| anyhow::anyhow!("--default requires an argument"))?;
                default_rev = Some(value.clone());
            } else if let Some(value) = arg.strip_prefix("--default=") {
                default_rev = Some(value.to_owned());
            } else if arg == "--end-of-options" {
                end_of_options = true;
                actions.push(Action::Literal("--end-of-options".to_owned()));
            } else if arg == "--branches" {
                actions.push(Action::Branches(None));
            } else if let Some(pattern) = arg.strip_prefix("--branches=") {
                actions.push(Action::Branches(Some(pattern.to_owned())));
            } else if arg == "--tags" {
                actions.push(Action::Tags(None));
            } else if let Some(pattern) = arg.strip_prefix("--tags=") {
                actions.push(Action::Tags(Some(pattern.to_owned())));
            } else if let Some(pattern) = arg.strip_prefix("--glob=") {
                actions.push(Action::Glob(normalize_glob_pattern(pattern)));
            } else if arg == "--glob" {
                i += 1;
                if let Some(pattern) = args.args.get(i) {
                    actions.push(Action::Glob(normalize_glob_pattern(pattern)));
                }
            } else if arg == "--remotes" {
                actions.push(Action::Remotes(None));
            } else if let Some(pattern) = arg.strip_prefix("--remotes=") {
                actions.push(Action::Remotes(Some(pattern.to_owned())));
            } else if arg == "--all" {
                actions.push(Action::All);
            } else if let Some(pattern) = arg.strip_prefix("--exclude=") {
                actions.push(Action::Exclude(pattern.to_owned()));
            } else if arg == "--exclude" {
                i += 1;
                if let Some(pattern) = args.args.get(i) {
                    actions.push(Action::Exclude(pattern.to_owned()));
                }
            } else if arg.starts_with("--exclude-hidden=") {
                actions.push(Action::ExcludeHidden(
                    arg.trim_start_matches("--exclude-hidden=").to_owned(),
                ));
            } else if arg == "--show-ref-format" {
                actions.push(Action::ShowRefFormat);
            } else if let Some(mode) = arg.strip_prefix("--show-object-format=") {
                actions.push(Action::ShowObjectFormat(mode.to_owned()));
            } else if arg == "--show-object-format" {
                actions.push(Action::ShowObjectFormat("storage".to_owned()));
            } else if arg == "--sq-quote" {
                sq_quote = true;
            } else if arg == "--sq" {
                sq_output = true;
            } else if let Some(pfx) = arg.strip_prefix("--disambiguate=") {
                actions.push(Action::Disambiguate(pfx.to_owned()));
            } else if arg == "--disambiguate" {
                i += 1;
                let pfx = args
                    .args
                    .get(i)
                    .ok_or_else(|| anyhow::anyhow!("--disambiguate requires a prefix argument"))?;
                actions.push(Action::Disambiguate(pfx.clone()));
            } else if arg == "--local-env-vars" {
                actions.push(Action::LocalEnvVars);
            } else if arg == "--resolve-git-dir" {
                i += 1;
                let path_arg = args
                    .args
                    .get(i)
                    .ok_or_else(|| anyhow::anyhow!("--resolve-git-dir requires an argument"))?;
                actions.push(Action::ResolveGitDir(path_arg.clone()));
            } else if arg == "--revs-only" {
                revs_only = true;
            } else if arg == "--no-revs" {
                no_revs = true;
            } else if arg == "--no-flags" {
                no_flags = true;
            } else if no_flags {
                // In --no-flags mode, silently skip unknown flags
            } else if no_revs {
                // In --no-revs mode, output unknown flags as non-rev output
                println!("{arg}");
            } else {
                bail!("unsupported option: {arg}");
            }
            i += 1;
            continue;
        }
        if saw_path_separator {
            actions.push(Action::ForcedPath(arg.clone()));
        } else {
            let strict = first_path_sep_dd.is_some_and(|dd| i < dd);
            actions.push(Action::Revision(
                arg.clone(),
                show_symbolic_full_name,
                show_symbolic_asis,
                strict,
            ));
        }
        i += 1;
    }

    // --sq-quote: shell-quote all non-flag args and exit
    if sq_quote {
        let mut out = String::new();
        for action in &actions {
            if let Action::Revision(rev, _, _, _) = action {
                if !out.is_empty() {
                    out.push(' ');
                }
                out.push_str(&sq_quote_str(rev));
            }
        }
        println!("{out}");
        return Ok(());
    }

    // --verify mode: exactly one revision, output its OID
    if verify {
        let revisions: Vec<&str> = actions
            .iter()
            .filter_map(|a| match a {
                Action::Revision(r, _, _, _) => Some(r.as_str()),
                _ => None,
            })
            .collect();
        let mut rev_list = revisions;
        if rev_list.is_empty() {
            if let Some(default_name) = default_rev.as_deref() {
                rev_list = vec![default_name];
            }
        }
        if rev_list.len() != 1 {
            return fail_verify(quiet, false);
        }
        let repo = discover_optional(None)?;
        let Some(current) = repo.as_ref() else {
            return fail_verify(quiet, false);
        };
        let mut spec = rev_list[0];
        let mut negated = false;
        if let (true, rest) = strip_leading_uninteresting_caret(spec) {
            if split_double_dot_range(rest).is_some()
                || split_triple_dot_range(rest).is_some()
                || spec_has_parent_shorthand_suffix(rest)
            {
                return fail_verify(quiet, false);
            }
            negated = true;
            spec = rest;
        }
        // Git's `try_difference()` only treats `A..B` / `A...B` as a range when *both*
        // endpoints resolve. If either side fails (e.g. a ref name that merely contains
        // `..`/`...`, such as the broken ref names exercised by t1430), git restores the
        // string and resolves the whole spec as a single revision. Resolving the endpoints
        // here with `.ok()` lets us fall through to the single-spec path on failure rather
        // than reporting an "ambiguous argument" error for the left endpoint.
        if let Some((left, right)) = split_double_dot_range(spec) {
            let endpoints = (|| {
                let left_oid = if left.is_empty() {
                    resolve_revision_for_range_end(current, "HEAD")
                } else {
                    resolve_revision_for_range_end(current, left)
                }
                .ok()?;
                let right_oid = if right.is_empty() {
                    resolve_revision_for_range_end(current, "HEAD")
                } else {
                    resolve_revision_for_range_end(current, right)
                }
                .ok()?;
                Some((left_oid, right_oid))
            })();
            if let Some((left_oid, right_oid)) = endpoints {
                if let Some(mut len) = short_len {
                    if len == 0 {
                        use grit_lib::config::ConfigSet;
                        let config = ConfigSet::load(Some(&current.git_dir), false)
                            .unwrap_or_else(|_| ConfigSet::new());
                        len = config
                            .get("core.abbrev")
                            .and_then(|v| v.parse::<usize>().ok())
                            .unwrap_or(7);
                    }
                    println!("{}", abbreviate_object_id(current, left_oid, len)?);
                    println!("^{}", abbreviate_object_id(current, right_oid, len)?);
                } else {
                    println!("{left_oid}");
                    println!("^{right_oid}");
                }
                return Ok(());
            }
        }
        if let Some((left, right)) = split_triple_dot_range(spec) {
            let tips = (|| {
                let left_tip = if left.is_empty() {
                    resolve_revision_for_range_end(current, "HEAD")
                } else {
                    resolve_revision_for_range_end(current, left)
                }
                .ok()?;
                let right_tip = if right.is_empty() {
                    resolve_revision_for_range_end(current, "HEAD")
                } else {
                    resolve_revision_for_range_end(current, right)
                }
                .ok()?;
                Some((left_tip, right_tip))
            })();
            if let Some((left_tip, right_tip)) = tips {
                let left_commit = peel_to_commit_for_merge_base(current, left_tip)?;
                let right_commit = peel_to_commit_for_merge_base(current, right_tip)?;
                let bases =
                    merge_base::merge_bases_first_vs_rest(current, left_commit, &[right_commit])?;
                let Some(mb) = bases.into_iter().next() else {
                    return fail_verify(quiet, false);
                };
                if let Some(mut len) = short_len {
                    if len == 0 {
                        use grit_lib::config::ConfigSet;
                        let config = ConfigSet::load(Some(&current.git_dir), false)
                            .unwrap_or_else(|_| ConfigSet::new());
                        len = config
                            .get("core.abbrev")
                            .and_then(|v| v.parse::<usize>().ok())
                            .unwrap_or(7);
                    }
                    println!("{}", abbreviate_object_id(current, left_tip, len)?);
                    println!("{}", abbreviate_object_id(current, right_tip, len)?);
                    println!("^{}", abbreviate_object_id(current, mb, len)?);
                } else {
                    println!("{left_tip}");
                    println!("{right_tip}");
                    println!("^{mb}");
                }
                return Ok(());
            }
        }
        // We only reach here for a `..`/`...` spec when at least one range endpoint failed
        // to resolve (matching git's `try_difference`, which then treats the whole string as
        // a single revision). Git refuses to resolve a ref whose name is itself invalid, so a
        // dotted name like `refs/heads/broken...symref` yields "Needed a single revision"
        // rather than following the broken-named symref. Mirror that: reject a dotted spec
        // that is not a valid ref name before attempting whole-spec resolution.
        if (split_double_dot_range(spec).is_some() || split_triple_dot_range(spec).is_some())
            && grit_lib::check_ref_format::check_refname_format(
                spec,
                &grit_lib::check_ref_format::RefNameOptions::default(),
            )
            .is_err()
        {
            return fail_verify(quiet, false);
        }
        let oid = if matches!(spec, "REVERT_HEAD" | "CHERRY_PICK_HEAD") {
            let path = current.git_dir.join(spec);
            let raw = match std::fs::read_to_string(&path) {
                Ok(s) => s,
                Err(_) => return fail_verify(quiet, false),
            };
            let line = raw.lines().next().unwrap_or("").trim();
            match line.parse::<grit_lib::objects::ObjectId>() {
                Ok(o) => o,
                Err(_) => return fail_verify(quiet, false),
            }
        } else {
            match grit_lib::rev_parse::resolve_revision_for_verify(current, spec) {
                Ok(oid) => oid,
                Err(e) => {
                    if quiet && is_self_referential_symbolic_ref(current, spec) {
                        return Ok(());
                    }
                    return fail_verify_resolve(quiet, &e, Some(current));
                }
            }
        };
        if let Some(mut len) = short_len {
            if len == 0 {
                use grit_lib::config::ConfigSet;
                let config = ConfigSet::load(Some(&current.git_dir), false)
                    .unwrap_or_else(|_| ConfigSet::new());
                len = config
                    .get("core.abbrev")
                    .and_then(|v| v.parse::<usize>().ok())
                    .unwrap_or(7);
            }
            if negated {
                println!("^{}", abbreviate_object_id(current, oid, len)?);
            } else {
                println!("{}", abbreviate_object_id(current, oid, len)?);
            }
        } else if negated {
            println!("^{oid}");
        } else {
            println!("{oid}");
        }
        return Ok(());
    }

    // Apply --default: if no Revision actions exist, inject the default
    if let Some(ref def) = default_rev {
        let has_revision = actions
            .iter()
            .any(|a| matches!(a, Action::Revision(_, _, _, _)));
        if !has_revision {
            actions.push(Action::Revision(
                def.clone(),
                show_symbolic_full_name,
                show_symbolic_asis,
                false,
            ));
        }
    }

    // Check if we have any actions at all
    let has_output_actions = actions.iter().any(|a| !matches!(a, Action::PathSeparator));
    if !has_output_actions {
        // Match git behavior: plain `git rev-parse` still requires repository
        // setup and should fail for invalid/missing gitdir state.
        let _ = Repository::discover(None)?;
        return Ok(());
    }

    let repo = discover_optional(None)?;

    // Resolve default --short length from core.abbrev config if not explicitly given
    if short_len == Some(0) {
        let default_abbrev = if let Some(ref r) = repo {
            use grit_lib::config::ConfigSet;
            let config =
                ConfigSet::load(Some(&r.git_dir), false).unwrap_or_else(|_| ConfigSet::new());
            config
                .get("core.abbrev")
                .and_then(|v| v.parse::<usize>().ok())
                .unwrap_or(7)
        } else {
            7
        };
        short_len = Some(default_abbrev);
    }

    // Process actions in order
    let mut saw_path_sep_output = false;
    let mut exclude_patterns: Vec<String> = Vec::new();
    let mut ref_exclusions = RefExclusions::default();
    let _ = sq_output; // --sq accepted but output quoting deferred to callers
    let mut seen_ambiguous_revision = false;
    let mut deferred_fatal_stderr: Option<String> = None;
    for action in &actions {
        match action {
            Action::Literal(s) => {
                println!("{s}");
            }
            Action::Disambiguate(pfx) => {
                let Some(current) = repo.as_ref() else {
                    bail!("not a git repository (or any of the parent directories)");
                };
                let mut oids = list_all_abbrev_matches(current, pfx)?;
                oids.sort_by_key(|o| o.to_hex());
                oids.dedup();
                for oid in oids {
                    println!("{}", oid.to_hex());
                }
            }
            Action::ShowIsInsideWorkTree => {
                let inside = repo
                    .as_ref()
                    .map(|current| is_inside_work_tree(current, &cwd))
                    .unwrap_or(false);
                println!("{}", if inside { "true" } else { "false" });
            }
            Action::ShowIsInsideGitDir => {
                let inside = repo
                    .as_ref()
                    .map(|current| is_inside_git_dir(current, &cwd))
                    .unwrap_or(false);
                println!("{}", if inside { "true" } else { "false" });
            }
            Action::ShowIsShallow => {
                let is_shallow = repo
                    .as_ref()
                    .map(|r| r.git_dir.join("shallow").exists())
                    .unwrap_or(false);
                println!("{}", if is_shallow { "true" } else { "false" });
            }
            Action::ShowIsBare => {
                let bare = repo
                    .as_ref()
                    .map(|current| current.is_bare())
                    .unwrap_or(false);
                println!("{}", if bare { "true" } else { "false" });
            }
            Action::ShowToplevel(fmt) => {
                let Some(current) = repo.as_ref() else {
                    bail!("not a git repository (or any of the parent directories)");
                };
                let Some(work_tree) = &current.work_tree else {
                    bail!("this operation must be run in a work tree");
                };
                match fmt {
                    Some(true) => {
                        println!("{}", realpath_forgiving(work_tree).display());
                    }
                    Some(false) => {
                        let wt_a = realpath_forgiving(work_tree);
                        let cwd_a = realpath_forgiving(&cwd);
                        if wt_a == cwd_a {
                            println!("./");
                        } else {
                            print_rev_parse_path(
                                work_tree,
                                &cwd,
                                cli_prefix_path.as_deref(),
                                Some(false),
                                PathDefaultMode::Canonical,
                            );
                        }
                    }
                    None => {
                        println!("{}", work_tree.display());
                    }
                }
            }
            Action::ShowPrefix => {
                let Some(current) = repo.as_ref() else {
                    eprintln!("error: not a git repository (or any of the parent directories)");
                    std::process::exit(128);
                };
                println!("{}", show_prefix(current, &cwd));
            }
            Action::ShowCdup => {
                let Some(current) = repo.as_ref() else {
                    eprintln!("error: not a git repository (or any of the parent directories)");
                    std::process::exit(128);
                };
                let pfx = show_prefix(current, &cwd);
                if pfx.is_empty() {
                    println!();
                } else {
                    let depth = pfx.trim_end_matches('/').matches('/').count() + 1;
                    let cdup: String = "../".repeat(depth);
                    println!("{cdup}");
                }
            }
            Action::ShowGitDir(fmt) => {
                let Some(current) = repo.as_ref() else {
                    bail!("not a git repository (or any of the parent directories)");
                };
                if let Ok(gd) = std::env::var("GIT_DIR") {
                    let gd_path = PathBuf::from(gd);
                    print_rev_parse_path(
                        &gd_path,
                        &cwd,
                        cli_prefix_path.as_deref(),
                        *fmt,
                        PathDefaultMode::Unmodified,
                    );
                } else {
                    let git_dir = current.git_dir.as_path();
                    let cwd_a = realpath_forgiving(&cwd);
                    let git_a = realpath_forgiving(git_dir);
                    if cwd_a == git_a {
                        match fmt {
                            Some(true) => println!("{}", git_a.display()),
                            Some(false) => println!("."),
                            None => println!("."),
                        }
                    } else if cwd_a.starts_with(&git_a) {
                        print_rev_parse_path(
                            git_dir,
                            &cwd,
                            cli_prefix_path.as_deref(),
                            *fmt,
                            PathDefaultMode::Canonical,
                        );
                    } else if current.work_tree.as_ref().is_some_and(|wt| {
                        cwd_a == realpath_forgiving(wt) && !is_linked_worktree_git_dir(git_dir)
                    }) {
                        if cwd.join(".git").is_file() {
                            print_rev_parse_path(
                                git_dir,
                                &cwd,
                                cli_prefix_path.as_deref(),
                                *fmt,
                                PathDefaultMode::Canonical,
                            );
                            continue;
                        }
                        match fmt {
                            Some(true) => {
                                println!("{}", git_a.display());
                            }
                            Some(false) => {
                                print_rev_parse_path(
                                    Path::new(".git"),
                                    &cwd,
                                    cli_prefix_path.as_deref(),
                                    Some(false),
                                    PathDefaultMode::Unmodified,
                                );
                            }
                            None => {
                                println!(".git");
                            }
                        }
                    } else {
                        print_rev_parse_path(
                            git_dir,
                            &cwd,
                            cli_prefix_path.as_deref(),
                            *fmt,
                            PathDefaultMode::Canonical,
                        );
                    }
                }
            }
            Action::ShowSharedIndexPath => {
                let Some(current) = repo.as_ref() else {
                    bail!("not a git repository (or any of the parent directories)");
                };
                let index_path = std::env::var("GIT_INDEX_FILE")
                    .ok()
                    .filter(|s| !s.is_empty())
                    .map(|raw| {
                        let p = std::path::PathBuf::from(raw);
                        if p.is_absolute() {
                            p
                        } else {
                            cwd.join(p)
                        }
                    })
                    .unwrap_or_else(|| current.index_path());
                let data = std::fs::read(&index_path).context("Could not read the index")?;
                let idx =
                    grit_lib::index::Index::parse(&data).context("Could not read the index")?;
                if let Some(base) = idx.split_index_base_oid() {
                    let shared = current
                        .git_dir
                        .join(format!("sharedindex.{}", base.to_hex()));
                    println!("{}", to_relative_path(shared.as_path(), &cwd));
                }
            }
            Action::ShowGitCommonDir(fmt) => {
                let Some(current) = repo.as_ref() else {
                    bail!("not a git repository (or any of the parent directories)");
                };
                // Git: `print_path(repo_get_common_dir(), prefix, format,
                // DEFAULT_RELATIVE_IF_SHARED)`. The common dir is reported in the
                // same form Git's setup left it in (cwd-relative for an in-tree
                // `.git`, absolute for a gitfile / linked worktree), and the
                // RELATIVE_IF_SHARED default prints it verbatim unless an
                // explicit `--prefix` was given that shares a filesystem root.
                let common_git_dir = computed_common_dir(&current.git_dir, &cwd);
                print_common_dir_path(&common_git_dir, cli_prefix_path.as_deref(), *fmt);
            }
            Action::ShowAbsoluteGitDir => {
                let Some(current) = repo.as_ref() else {
                    bail!("not a git repository (or any of the parent directories)");
                };
                let gd_path = if let Ok(gd) = std::env::var("GIT_DIR") {
                    let p = PathBuf::from(gd);
                    if p.is_relative() {
                        cwd.join(p)
                    } else {
                        p
                    }
                } else {
                    current.git_dir.clone()
                };
                println!("{}", realpath_forgiving(&gd_path).display());
            }
            Action::ShowSuperprojectWorkingTree(fmt) => {
                let Some(current) = repo.as_ref() else {
                    bail!("not a git repository (or any of the parent directories)");
                };
                if !is_inside_work_tree(current, &cwd) {
                    continue;
                }
                let super_wt = superproject_working_tree_via_ls_files(current, &cwd)
                    .or_else(|| superproject_work_tree_from_nested_git_modules(&current.git_dir));
                if let Some(super_wt) = super_wt {
                    print_rev_parse_path(
                        &super_wt,
                        &cwd,
                        cli_prefix_path.as_deref(),
                        *fmt,
                        PathDefaultMode::Unmodified,
                    );
                }
            }
            Action::ShowRefFormat => {
                let Some(current) = repo.as_ref() else {
                    bail!("not a git repository (or any of the parent directories)");
                };
                let raw = read_extensions_refstorage(&current.git_dir);
                if !ref_storage_format_is_valid(&raw) {
                    bail!(
                        "error: invalid value for 'extensions.refstorage': '{}'",
                        raw
                    );
                }
                let format = raw.to_ascii_lowercase();
                let name = format
                    .split_once(':')
                    .map(|(a, _)| a.to_string())
                    .unwrap_or(format);
                println!("{name}");
            }
            Action::ShowObjectFormat(mode) => {
                let Some(current) = repo.as_ref() else {
                    bail!("not a git repository (or any of the parent directories)");
                };
                let (storage_fmt, compat_fmt) = read_object_format_from_config(&current.git_dir);
                match mode.as_str() {
                    "storage" | "input" | "output" => println!("{storage_fmt}"),
                    "compat" => {
                        if let Some(c) = compat_fmt {
                            println!("{c}");
                        } else {
                            println!();
                        }
                    }
                    other => {
                        bail!("unknown mode for --show-object-format: {other}");
                    }
                }
            }
            Action::BisectRefs(symbolic_full_name) => {
                let Some(current) = repo.as_ref() else {
                    bail!("not a git repository (or any of the parent directories)");
                };
                // Mirror git rev-parse --bisect: iterate refs under the
                // `refs/bisect/bad` prefix (printed as "good" revs) and the
                // `refs/bisect/good` prefix (printed as anti-revs, "^"). Git
                // uses a plain string-prefix match and emits refs in
                // sorted-by-name order, so `refs/bisect/b` and `refs/bisect/go`
                // are excluded (they do not start with the full prefixes).
                let all = grit_lib::refs::list_refs(&current.git_dir, "refs/bisect/")
                    .context("failed to list bisect refs")?;

                // Collect (name, oid) for each prefix, sorted by ref name to
                // match git's ref-store iteration order.
                let collect_sorted = |prefix: &str| {
                    let mut v: Vec<_> = all.iter().filter(|(r, _)| r.starts_with(prefix)).collect();
                    v.sort_by(|(a, _), (b, _)| a.cmp(b));
                    v
                };

                // With --symbolic-full-name git prints the full ref name (via
                // dwim resolution); otherwise it prints the object id.
                for (name, oid) in collect_sorted("refs/bisect/bad") {
                    if *symbolic_full_name {
                        println!("{name}");
                    } else {
                        println!("{oid}");
                    }
                }
                for (name, oid) in collect_sorted("refs/bisect/good") {
                    if *symbolic_full_name {
                        println!("^{name}");
                    } else {
                        println!("^{oid}");
                    }
                }
            }
            Action::MaxAge(date) => {
                let mut err = 0;
                let ts = approxidate_careful(date, Some(&mut err));
                println!("--max-age={ts}");
            }
            Action::MinAge(date) => {
                let mut err = 0;
                let ts = approxidate_careful(date, Some(&mut err));
                println!("--min-age={ts}");
            }
            Action::GitPath(fmt, path_arg) => {
                if let Some(current) = repo.as_ref() {
                    // Use original path_arg for output, normalized for matching
                    let path_arg_for_match = {
                        let mut s = path_arg.clone();
                        while s.contains("//") {
                            s = s.replace("//", "/");
                        }
                        s = s.trim_start_matches('/').to_owned();
                        s
                    };
                    let path_arg_out = path_arg; // original for output
                    let path_arg = &path_arg_for_match; // normalized for matching

                    // GIT_COMMON_DIR: paths classified as "common" by Git's
                    // `update_common_dir`/`common_list` (port in
                    // `grit_lib::git_path::is_common_git_path`) are resolved
                    // against the common dir; everything else stays under the
                    // per-worktree git dir. Git uses the env var spelling
                    // verbatim (see setup.c `get_common_dir`) and replaces the
                    // git-dir prefix, so a relative `GIT_COMMON_DIR=bar` yields a
                    // relative `bar/<component>` result (printed as-is by
                    // relative-if-shared since it shares no root with cwd).
                    if let Ok(common_dir) = std::env::var("GIT_COMMON_DIR") {
                        // env-var overrides (grafts/index/objects) still win over
                        // the common-dir replacement, matching `adjust_git_path`.
                        let has_env_override = (path_arg == "info/grafts"
                            && std::env::var("GIT_GRAFT_FILE").is_ok())
                            || (path_arg == "index" && std::env::var("GIT_INDEX_FILE").is_ok())
                            || ((path_arg == "objects" || path_arg.starts_with("objects/"))
                                && std::env::var("GIT_OBJECT_DIRECTORY").is_ok());
                        if !has_env_override
                            && grit_lib::git_path::is_common_git_path(path_arg.as_str())
                        {
                            // Append the original component verbatim (preserving
                            // internal `//`), mirroring `repo_git_pathv` +
                            // `replace_dir`.
                            let component =
                                grit_lib::git_path::git_path_relative_component(path_arg_out);
                            let sep = if common_dir.ends_with('/') { "" } else { "/" };
                            println!("{common_dir}{sep}{component}");
                            continue;
                        }
                    }
                    // Check env var overrides
                    let env_override = if path_arg == "info/grafts" {
                        std::env::var("GIT_GRAFT_FILE").ok()
                    } else if path_arg == "index" {
                        std::env::var("GIT_INDEX_FILE").ok()
                    } else if path_arg == "objects" {
                        std::env::var("GIT_OBJECT_DIRECTORY").ok()
                    } else if let Some(remainder) = path_arg.strip_prefix("objects/") {
                        if let Ok(obj_dir) = std::env::var("GIT_OBJECT_DIRECTORY") {
                            Some(format!("{obj_dir}/{remainder}"))
                        } else {
                            None
                        }
                    } else {
                        None
                    };
                    if let Some(env_val) = env_override {
                        println!("{env_val}");
                        continue;
                    }
                    let resolved = if path_arg == "hooks" || path_arg.starts_with("hooks/") {
                        let config =
                            grit_lib::config::ConfigSet::load(Some(&current.git_dir), true)?;
                        if let Some(hooks_path) = config.get("core.hooksPath") {
                            let hooks_dir = std::path::Path::new(&hooks_path);
                            if path_arg == "hooks" {
                                hooks_dir.to_path_buf()
                            } else {
                                let remainder = &path_arg["hooks/".len()..];
                                hooks_dir.join(remainder)
                            }
                        } else {
                            current.git_dir.join(path_arg)
                        }
                    } else {
                        // Some paths are stored in the common dir (shared across worktrees)
                        let common_paths = [
                            "objects",
                            "refs",
                            "packed-refs",
                            "info",
                            "config",
                            "ORIG_HEAD",
                            "FETCH_HEAD",
                            "logs",
                            "shallow",
                        ];
                        let use_common = common_paths
                            .iter()
                            .any(|p| path_arg == *p || path_arg.starts_with(&format!("{}/", p)))
                            // Linked worktrees keep sparse-checkout under their admin dir
                            // (`.git/worktrees/<name>/info/sparse-checkout`), not in commondir.
                            && !path_arg.starts_with("info/sparse-checkout");
                        // Git's `repo_git_pathv` appends the caller-supplied
                        // component verbatim (only `cleanup_path` runs over it),
                        // so internal `//` runs are preserved in the output even
                        // though path classification above uses the normalized
                        // form. Join the original component, not `path_arg`.
                        let original_component =
                            grit_lib::git_path::git_path_relative_component(path_arg_out);
                        if use_common {
                            let common = refs::common_dir(&current.git_dir)
                                .unwrap_or_else(|| current.git_dir.clone());
                            if common != current.git_dir {
                                let p = common.join(path_arg);
                                println!("{}", p.canonicalize().unwrap_or(p).display());
                                continue;
                            }
                            common.join(path_arg)
                        } else {
                            current.git_dir.join(original_component)
                        }
                    };
                    // For a real linked worktree (git_dir under `.../worktrees/<name>`),
                    // worktree-local paths (those NOT classified as common) resolve to
                    // the admin dir and print as a canonical absolute path.
                    let is_worktree_local =
                        !grit_lib::git_path::is_common_git_path(path_arg.as_str());
                    if is_worktree_local
                        && current
                            .git_dir
                            .components()
                            .any(|c| c.as_os_str() == std::ffi::OsStr::new("worktrees"))
                    {
                        println!("{}", resolved.canonicalize().unwrap_or(resolved).display());
                        continue;
                    }
                    print_rev_parse_path(
                        &resolved,
                        &cwd,
                        cli_prefix_path.as_deref(),
                        *fmt,
                        PathDefaultMode::RelativeIfShared,
                    );
                } else {
                    bail!("not a git repository");
                }
            }
            Action::Exclude(pattern) => {
                exclude_patterns.push(pattern.clone());
                ref_exclusions.add_excluded_ref(pattern.clone());
            }
            Action::ExcludeHidden(section) => {
                if ref_exclusions.hidden_configured {
                    eprintln!("fatal: --exclude-hidden= passed more than once");
                    std::process::exit(128);
                }
                match section.as_str() {
                    "fetch" | "receive" | "uploadpack" => {
                        let Some(current) = repo.as_ref() else {
                            bail!("not a git repository");
                        };
                        let config =
                            grit_lib::config::ConfigSet::load(Some(&current.git_dir), true)?;
                        ref_exclusions.load_hidden_refs_from_config(&config, section);
                    }
                    _ => {
                        eprintln!("fatal: unsupported section for hidden refs: {section}");
                        std::process::exit(128);
                    }
                }
            }
            Action::All => {
                if let Some(current) = repo.as_ref() {
                    let matching = if ref_exclusions.hidden_configured {
                        grit_lib::refs::list_refs_physical(&current.git_dir, "refs/")
                    } else {
                        grit_lib::refs::list_refs(&current.git_dir, "refs/")
                    }
                    .context("failed to list refs")?;
                    let namespace_prefix = git_namespace_prefix();
                    for (refname, oid) in &matching {
                        if !ref_exclusions
                            .ref_excluded(strip_git_namespace(refname, &namespace_prefix), refname)
                        {
                            println!("{oid}");
                        }
                    }
                    exclude_patterns.clear();
                    ref_exclusions.clear();
                }
            }
            Action::Branches(pattern) => {
                if let Some(current) = repo.as_ref() {
                    reject_rev_parse_exclude_hidden_with("--branches", &ref_exclusions)?;
                    let matching = if let Some(pat) = pattern {
                        let full = normalize_ref_pattern("refs/heads/", pat);
                        grit_lib::refs::list_refs_glob(&current.git_dir, &full)
                            .context("failed to list branch refs")?
                    } else {
                        grit_lib::refs::list_refs(&current.git_dir, "refs/heads/")
                            .context("failed to list branch refs")?
                    };
                    for (refname, oid) in &matching {
                        let short = refname.strip_prefix("refs/heads/").unwrap_or(refname);
                        if !is_excluded(short, &exclude_patterns) {
                            println!("{oid}");
                        }
                    }
                    exclude_patterns.clear();
                    ref_exclusions.clear();
                }
            }
            Action::Tags(pattern) => {
                if let Some(current) = repo.as_ref() {
                    reject_rev_parse_exclude_hidden_with("--tags", &ref_exclusions)?;
                    let matching = if let Some(pat) = pattern {
                        let full = normalize_ref_pattern("refs/tags/", pat);
                        grit_lib::refs::list_refs_glob(&current.git_dir, &full)
                            .context("failed to list tag refs")?
                    } else {
                        grit_lib::refs::list_refs(&current.git_dir, "refs/tags/")
                            .context("failed to list tag refs")?
                    };
                    for (refname, oid) in &matching {
                        let short = refname.strip_prefix("refs/tags/").unwrap_or(refname);
                        if !is_excluded(short, &exclude_patterns) {
                            println!("{oid}");
                        }
                    }
                    exclude_patterns.clear();
                    ref_exclusions.clear();
                }
            }
            Action::Remotes(pattern) => {
                if let Some(current) = repo.as_ref() {
                    reject_rev_parse_exclude_hidden_with("--remotes", &ref_exclusions)?;
                    let matching = if let Some(pat) = pattern {
                        let full = normalize_ref_pattern("refs/remotes/", pat);
                        grit_lib::refs::list_refs_glob(&current.git_dir, &full)
                            .context("failed to list remote refs")?
                    } else {
                        grit_lib::refs::list_refs(&current.git_dir, "refs/remotes/")
                            .context("failed to list remote refs")?
                    };
                    for (refname, oid) in &matching {
                        let short = refname.strip_prefix("refs/remotes/").unwrap_or(refname);
                        if !is_excluded(short, &exclude_patterns) {
                            println!("{oid}");
                        }
                    }
                    exclude_patterns.clear();
                    ref_exclusions.clear();
                }
            }
            Action::Glob(full) => {
                if let Some(current) = repo.as_ref() {
                    let matching = if ref_exclusions.hidden_configured {
                        list_refs_glob_physical_rev_parse(current, full)
                    } else {
                        Ok(grit_lib::refs::list_refs_glob(&current.git_dir, full)?)
                    }
                    .context("failed to list refs")?;
                    let namespace_prefix = git_namespace_prefix();
                    for (refname, oid) in &matching {
                        if !ref_exclusions
                            .ref_excluded(strip_git_namespace(refname, &namespace_prefix), refname)
                        {
                            println!("{oid}");
                        }
                    }
                    exclude_patterns.clear();
                    ref_exclusions.clear();
                }
            }
            Action::LocalEnvVars => {
                for var in &[
                    "GIT_DIR",
                    "GIT_WORK_TREE",
                    "GIT_OBJECT_DIRECTORY",
                    "GIT_ALTERNATE_OBJECT_DIRECTORIES",
                    "GIT_INDEX_FILE",
                    "GIT_GRAFT_FILE",
                    "GIT_COMMON_DIR",
                ] {
                    println!("{var}");
                }
            }
            Action::ResolveGitDir(path_arg) => {
                let p = std::path::Path::new(path_arg);
                if p.is_dir() && p.join("HEAD").exists() {
                    let resolved = p.canonicalize().unwrap_or_else(|_| p.to_path_buf());
                    println!("{}", resolved.display());
                } else if p.is_file() {
                    let content = std::fs::read_to_string(p)
                        .with_context(|| format!("cannot read '{}'", p.display()))?;
                    let mut found = false;
                    for line in content.lines() {
                        if let Some(rest) = line.strip_prefix("gitdir:") {
                            let rel = rest.trim();
                            let git_dir = if std::path::Path::new(rel).is_absolute() {
                                std::path::PathBuf::from(rel)
                            } else {
                                p.parent().unwrap_or(std::path::Path::new(".")).join(rel)
                            };
                            let resolved = git_dir.canonicalize().unwrap_or(git_dir);
                            println!("{}", resolved.display());
                            found = true;
                            break;
                        }
                    }
                    if !found {
                        bail!("not a gitdir: {path_arg}");
                    }
                } else {
                    bail!("not a valid directory: {path_arg}");
                }
            }
            Action::Revision(
                rev,
                rev_symbolic_full_name,
                rev_symbolic_asis,
                strict_before_first_dd,
            ) => {
                let Some(current) = repo.as_ref() else {
                    if quiet {
                        std::process::exit(1);
                    }
                    bail!("not a git repository (or any of the parent directories)");
                };
                if seen_ambiguous_revision {
                    println!("{rev}");
                    if rev.contains(':') && !rev.starts_with(':') {
                        deferred_fatal_stderr = Some(format!(
                            "fatal: {rev}: no such path in the working tree.\n\
Use 'git <command> -- <path>...' to specify paths that do not exist locally."
                        ));
                    }
                    continue;
                }
                let use_symbolic_asis = *rev_symbolic_asis;

                if abbrev_ref {
                    // --abbrev-ref: resolve to symbolic name and abbreviate
                    if let Some(full) = symbolic_full_name(current, rev) {
                        println!("{}", abbreviate_ref_name(&full));
                        continue;
                    }
                    // Fall through to try resolving as OID and printing as-is
                }

                if *rev_symbolic_full_name {
                    if let Some(full) = symbolic_full_name(current, rev) {
                        println!("{full}");
                        continue;
                    }
                }

                let rewritten = rewrite_tree_path_spec(rev, prefix.as_deref());
                let (negated, work) = strip_leading_uninteresting_caret(&rewritten);
                if negated
                    && (split_double_dot_range(work).is_some()
                        || split_triple_dot_range(work).is_some())
                {
                    eprintln!(
                        "fatal: ambiguous argument '{rewritten}': unknown revision or path not in the working tree.\n\
Use '--' to separate paths from revisions, like this:\n\
'git <command> [<revision>...] -- [<file>...]'"
                    );
                    println!("{rewritten}");
                    std::process::exit(128);
                }
                if let Some((left, right)) = split_triple_dot_range(work) {
                    if no_revs {
                        continue;
                    }
                    let left_tip = if left.is_empty() {
                        resolve_revision_for_range_end(current, "HEAD")?
                    } else {
                        resolve_revision_for_range_end(current, left)?
                    };
                    let right_tip = if right.is_empty() {
                        resolve_revision_for_range_end(current, "HEAD")?
                    } else {
                        resolve_revision_for_range_end(current, right)?
                    };
                    let left_commit = peel_to_commit_for_merge_base(current, left_tip)?;
                    let right_commit = peel_to_commit_for_merge_base(current, right_tip)?;
                    let bases = merge_base::merge_bases_first_vs_rest(
                        current,
                        left_commit,
                        &[right_commit],
                    )?;
                    let Some(mb) = bases.into_iter().next() else {
                        bail!("no merge base for '{work}'");
                    };
                    if use_symbolic_asis && short_len.is_none() {
                        let left_out = if left.is_empty() {
                            "HEAD".to_owned()
                        } else {
                            left.to_owned()
                        };
                        let right_out = if right.is_empty() {
                            "HEAD".to_owned()
                        } else {
                            right.to_owned()
                        };
                        println!("{right_out}");
                        println!("{left_out}");
                        println!("^{mb}");
                    } else if let Some(len) = short_len {
                        println!("{}", abbreviate_object_id(current, left_tip, len)?);
                        println!("{}", abbreviate_object_id(current, right_tip, len)?);
                        println!("^{}", abbreviate_object_id(current, mb, len)?);
                    } else {
                        println!("{left_tip}");
                        println!("{right_tip}");
                        println!("^{mb}");
                    }
                    continue;
                }
                if let Some((left, right)) = split_double_dot_range(work) {
                    if no_revs {
                        continue;
                    }
                    let left_oid = if left.is_empty() {
                        resolve_revision_for_range_end(current, "HEAD")?
                    } else {
                        resolve_revision_for_range_end(current, left)?
                    };
                    let right_oid = if right.is_empty() {
                        resolve_revision_for_range_end(current, "HEAD")?
                    } else {
                        resolve_revision_for_range_end(current, right)?
                    };
                    if left.is_empty() && right.is_empty() {
                        println!("..");
                    } else if use_symbolic_asis && short_len.is_none() {
                        let left_out = if left.is_empty() {
                            "HEAD".to_owned()
                        } else {
                            left.to_owned()
                        };
                        let right_out = if right.is_empty() {
                            "HEAD".to_owned()
                        } else {
                            right.to_owned()
                        };
                        println!("{right_out}");
                        println!("^{left_out}");
                    } else if let Some(len) = short_len {
                        println!("{}", abbreviate_object_id(current, right_oid, len)?);
                        println!("^{}", abbreviate_object_id(current, left_oid, len)?);
                    } else {
                        println!("{right_oid}");
                        println!("^{left_oid}");
                    }
                    continue;
                }
                if let Some(lines) = expand_parent_shorthand_rev_parse_lines(
                    current,
                    work,
                    use_symbolic_asis,
                    short_len,
                )? {
                    if no_revs {
                        continue;
                    }
                    for line in lines {
                        if negated {
                            println!("^{line}");
                        } else {
                            println!("{line}");
                        }
                    }
                    continue;
                }
                if looks_like_shell_glob(work) {
                    if no_revs {
                        continue;
                    }
                    match resolve_revision_without_index_dwim(current, work) {
                        Ok(oid) => {
                            if let Some(len) = short_len {
                                if negated {
                                    println!("^{}", abbreviate_object_id(current, oid, len)?);
                                } else {
                                    println!("{}", abbreviate_object_id(current, oid, len)?);
                                }
                            } else if negated {
                                println!("^{oid}");
                            } else {
                                println!("{oid}");
                            }
                        }
                        Err(_) => {
                            if revs_only {
                                continue;
                            }
                            if negated {
                                println!("^{work}");
                            } else {
                                println!("{rewritten}");
                            }
                        }
                    }
                    continue;
                }
                match resolve_revision_without_index_dwim(current, work) {
                    Ok(oid) => {
                        if no_revs {
                            // --no-revs: skip resolved revisions
                            continue;
                        }
                        if use_symbolic_asis && short_len.is_none() {
                            if negated {
                                println!("^{work}");
                            } else {
                                println!("{rewritten}");
                            }
                        } else if let Some(len) = short_len {
                            if negated {
                                println!("^{}", abbreviate_object_id(current, oid, len)?);
                            } else {
                                println!("{}", abbreviate_object_id(current, oid, len)?);
                            }
                        } else if negated {
                            println!("^{oid}");
                        } else {
                            println!("{oid}");
                        }
                    }
                    Err(e) => {
                        if revs_only {
                            // --revs-only: silently skip unresolvable args
                            continue;
                        }
                        let msg = e.to_string();
                        if *strict_before_first_dd && !rev.contains(':') {
                            match &e {
                                LibError::Message(_) | LibError::ObjectNotFound(_) => {
                                    bail!("fatal: bad revision '{rev}'");
                                }
                                _ if msg.contains("ambiguous argument") => {
                                    bail!("fatal: bad revision '{rev}'");
                                }
                                _ => {}
                            }
                        }
                        if matches!(&e, LibError::Message(m) if m.contains("ambiguous argument")) {
                            // Git's `rev-parse` with `--prefix`: after failed `repo_get_oid`,
                            // `verify_filename` uses `prefix_filename(prefix, arg)` + `lstat`.
                            // If that path exists, emit the prefixed path instead of dying
                            // (t1513 disambiguate path / file+refs with prefix).
                            if let Some(ref pfx) = prefix {
                                if prefixed_path_exists_on_disk(Some(pfx.as_str()), rev) {
                                    if short_len.is_some() {
                                        return fail_verify(quiet, false);
                                    }
                                    println!("{}", apply_prefix_for_forced_path(pfx, rev));
                                    continue;
                                }
                            }
                            // With `--short`, match Git: no stdout for the failed rev; exit via
                            // fail_verify after other actions (t9903 bare/orphan prompt).
                            if short_len.is_some() {
                                return fail_verify(quiet, false);
                            }
                            println!("{rev}");
                            seen_ambiguous_revision = true;
                            deferred_fatal_stderr = Some(msg);
                            continue;
                        }
                        let amb_prefix = parse_ambiguous_short_oid(&msg);
                        if let Some(ref pfx) = amb_prefix {
                            print_ambiguous_short_oid_error(current, rev, pfx)?;
                        }
                        if matches!(&e, LibError::Message(_) | LibError::InvalidRef(_)) {
                            return Err(e.into());
                        }
                        if msg.contains("ambiguous") {
                            return Err(anyhow::anyhow!("{msg}"));
                        }
                        // With `--short`, Git does not echo the unresolved spec to stdout; it fails
                        // with "Needed a single revision" (t9903 `__git_ps1` + bare/orphan repos).
                        if short_len.is_some() {
                            return fail_verify(quiet, false);
                        }
                        if no_revs || amb_prefix.is_some() {
                            if let Some(path_prefix) = prefix.as_deref() {
                                println!("{}", apply_prefix_for_forced_path(path_prefix, rev));
                            } else {
                                println!("{rev}");
                            }
                        } else {
                            bail!("fatal: bad revision '{rev}'");
                        }
                    }
                }
            }
            Action::PathSeparator => {
                println!("--");
                saw_path_sep_output = true;
            }
            Action::ForcedPath(path) => {
                if !saw_path_sep_output {
                    println!("--");
                    saw_path_sep_output = true;
                }
                if let Some(path_prefix) = prefix.as_deref() {
                    println!("{}", apply_prefix_for_forced_path(path_prefix, path));
                } else {
                    println!("{path}");
                }
            }
        }
    }
    if let Some(msg) = deferred_fatal_stderr {
        eprintln!("{msg}");
        std::process::exit(128);
    }
    Ok(())
}

fn parse_short_len(raw: &str) -> Result<usize> {
    let parsed = raw
        .parse::<usize>()
        .map_err(|_| anyhow::anyhow!("invalid --short length: {raw}"))?;
    Ok(parsed.clamp(4, 40))
}

fn fail_verify(quiet: bool, is_reflog_selector: bool) -> Result<()> {
    if quiet {
        std::process::exit(1);
    }
    if is_reflog_selector {
        // Match git behavior for invalid reflog selectors when not quiet.
        bail!("log for '<ref>' has no entries")
    } else {
        bail!("Needed a single revision")
    }
}

fn is_self_referential_symbolic_ref(repo: &Repository, spec: &str) -> bool {
    let candidates = if spec.starts_with("refs/") {
        vec![spec.to_owned()]
    } else {
        vec![format!("refs/heads/{spec}"), spec.to_owned()]
    };
    candidates.into_iter().any(|candidate| {
        matches!(
            grit_lib::refs::read_symbolic_ref(&repo.git_dir, &candidate),
            Ok(Some(target)) if target == candidate
        )
    })
}

fn fail_verify_resolve(
    quiet: bool,
    err: &LibError,
    repo: Option<&grit_lib::repo::Repository>,
) -> Result<()> {
    if quiet {
        std::process::exit(1);
    }
    let msg = err.to_string();
    if msg.contains("only has") && msg.contains("entries") {
        bail!("{msg}");
    }
    if let (LibError::ObjectNotFound(spec), Some(r)) = (err, repo) {
        if spec.contains("-g") && spec.matches('-').count() >= 2 {
            if let Ok(oid) = resolve_revision(r, spec) {
                println!("{oid}");
                return Ok(());
            }
        }
    }
    if matches!(err, LibError::InvalidRef(_)) {
        bail!("{msg}");
    }
    if matches!(err, LibError::Message(_)) && !msg.contains("ambiguous argument") {
        bail!("{msg}");
    }
    fail_verify(quiet, false)
}

fn apply_prefix_for_forced_path(prefix: &str, path: &str) -> String {
    if prefix.is_empty() {
        return path.to_owned();
    }
    format!("{prefix}{path}")
}

/// Git `abspath.c` `prefix_filename`: concatenate `pfx` and `arg` unless `arg` is absolute.
fn prefix_filename_for_stat<'a>(pfx: Option<&'a str>, arg: &'a str) -> Cow<'a, str> {
    let Some(p) = pfx else {
        return Cow::Borrowed(arg);
    };
    if p.is_empty() {
        return Cow::Borrowed(arg);
    }
    if Path::new(arg).is_absolute() {
        return Cow::Borrowed(arg);
    }
    Cow::Owned(format!("{p}{arg}"))
}

fn prefixed_path_exists_on_disk(pfx: Option<&str>, arg: &str) -> bool {
    let path = prefix_filename_for_stat(pfx, arg);
    Path::new(path.as_ref()).exists()
}

fn rewrite_tree_path_spec(spec: &str, prefix: Option<&str>) -> String {
    let Some((treeish, raw_path)) = spec.split_once(':') else {
        return spec.to_owned();
    };
    if treeish.is_empty() || raw_path.is_empty() {
        return spec.to_owned();
    }
    if !raw_path.starts_with("./") && !raw_path.starts_with("../") {
        return spec.to_owned();
    }
    // Without `--prefix`, `./` and `../` are resolved by the library relative to cwd; do not
    // normalize here (stripping `./` would wrongly turn `HEAD:./file` into `HEAD:file`).
    let Some(prefix) = prefix else {
        return spec.to_owned();
    };

    let mut joined = String::new();
    joined.push_str(prefix);
    joined.push_str(raw_path);
    let normalized = normalize_slash_path(&joined);
    format!("{treeish}:{normalized}")
}

fn parse_ambiguous_short_oid(message: &str) -> Option<String> {
    let trimmed = message.trim();
    if let Some(rest) = trimmed.strip_prefix("invalid ref: short object ID ") {
        return rest
            .strip_suffix(" is ambiguous")
            .map(std::borrow::ToOwned::to_owned);
    }
    if let Some(rest) = trimmed.strip_prefix("short object ID ") {
        return rest
            .strip_suffix(" is ambiguous")
            .map(std::borrow::ToOwned::to_owned);
    }
    None
}

fn print_ambiguous_short_oid_error(
    repo: &grit_lib::repo::Repository,
    rev: &str,
    short_prefix: &str,
) -> Result<()> {
    let candidates = list_all_abbrev_matches(repo, short_prefix)?;
    if candidates.is_empty() {
        return Err(anyhow::anyhow!(
            "invalid ref: short object ID {} is ambiguous",
            short_prefix
        ));
    }

    let mut typed_count = 0usize;
    let mut bad_oids: Vec<String> = Vec::new();
    for oid in &candidates {
        let oid_hex = oid.to_hex();
        match repo.odb.read(oid) {
            Ok(_) => typed_count += 1,
            Err(_) => bad_oids.push(oid_hex),
        }
    }

    eprintln!("error: short object ID {} is ambiguous", short_prefix);

    if typed_count == 0 {
        eprintln!("fatal: invalid object type");
        std::process::exit(128);
    }

    if !bad_oids.is_empty() {
        for oid_hex in &bad_oids {
            eprintln!("error: inflate: data stream error (incorrect header check)");
            eprintln!("error: unable to unpack {} header", oid_hex);
            eprintln!("error: inflate: data stream error (incorrect header check)");
            eprintln!("error: unable to unpack {} header", oid_hex);
        }
    }

    let peel_filter = parse_peel_suffix(rev).1;
    eprintln!("hint: The candidates are:");
    for line in ambiguous_object_hint_lines(repo, short_prefix, peel_filter)? {
        eprintln!("{line}");
    }

    eprintln!(
        "fatal: ambiguous argument '{}': unknown revision or path not in the working tree.",
        rev
    );
    eprintln!("Use '--' to separate paths from revisions, like this:");
    eprintln!("'git <command> [<revision>...] -- [<file>...]'");
    std::process::exit(128);
}

/// Shell-quote a string using single quotes, matching git's sq_quote_buf.
fn sq_quote_str(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('\'');
    for ch in s.chars() {
        if ch == '\'' {
            out.push_str("'\\''");
        } else {
            out.push(ch);
        }
    }
    out.push('\'');
    out
}

fn looks_like_shell_glob(spec: &str) -> bool {
    let mut it = spec.chars();
    while let Some(c) = it.next() {
        if c == '\\' {
            let _ = it.next();
            continue;
        }
        if matches!(c, '*' | '?' | '[') {
            return true;
        }
    }
    false
}

fn normalize_slash_path(path: &str) -> String {
    let mut parts = Vec::new();
    for component in path.split('/') {
        match component {
            "" | "." => {}
            ".." => {
                parts.pop();
            }
            other => parts.push(other),
        }
    }
    parts.join("/")
}

/// Run `rev-parse --parseopt` mode.
///
/// Reads an option specification from stdin, then parses the arguments
/// that follow `--` against that spec and outputs normalized options.
fn run_parseopt(extra_args: &[String]) -> Result<()> {
    super::rev_parse_parseopt::run_parseopt(extra_args)
}

/// Shell-escape a string for single-quote context.
/// Read `extensions.objectformat` and `extensions.compatobjectformat` from `config`.
///
/// Returns `(storage, compat)` where `storage` defaults to `sha1` when unset, matching Git.
fn read_object_format_from_config(git_dir: &std::path::Path) -> (String, Option<String>) {
    let config_path = git_dir.join("config");
    let Ok(content) = std::fs::read_to_string(&config_path) else {
        return ("sha1".to_owned(), None);
    };
    let mut in_extensions = false;
    let mut object_format: Option<String> = None;
    let mut compat: Option<String> = None;
    for line in content.lines() {
        let t = line.trim();
        if t.starts_with('[') {
            in_extensions = t.eq_ignore_ascii_case("[extensions]");
            continue;
        }
        if !in_extensions {
            continue;
        }
        let Some((k, v)) = t.split_once('=') else {
            continue;
        };
        let key = k.trim();
        let val = v.trim().to_lowercase();
        if key.eq_ignore_ascii_case("objectformat") {
            object_format = Some(val);
        } else if key.eq_ignore_ascii_case("compatobjectformat") {
            compat = Some(val);
        }
    }
    (object_format.unwrap_or_else(|| "sha1".to_owned()), compat)
}

fn shell_escape(s: &str) -> String {
    s.replace('\'', "'\\''")
}

/// Check whether a ref name matches any of the exclude patterns.
fn is_excluded(refname: &str, patterns: &[String]) -> bool {
    for pat in patterns {
        let full_pat = if pat.contains('*') || pat.contains('?') || pat.contains('[') {
            pat.clone()
        } else {
            // Treat non-glob patterns as exact ref suffixes
            pat.clone()
        };
        // Try matching as a glob pattern against the full refname
        if grit_lib::refs::ref_matches_glob(refname, &full_pat) {
            return true;
        }
    }
    false
}

fn reject_rev_parse_exclude_hidden_with(option: &str, exclusions: &RefExclusions) -> Result<()> {
    if exclusions.hidden_configured {
        bail!("options '--exclude-hidden' and '{option}' cannot be used together");
    }
    Ok(())
}

fn list_refs_glob_physical_rev_parse(
    repo: &Repository,
    pattern: &str,
) -> Result<Vec<(String, grit_lib::objects::ObjectId)>> {
    Ok(grit_lib::refs::list_refs_physical(&repo.git_dir, "refs/")?
        .into_iter()
        .filter(|(refname, _)| grit_lib::refs::ref_matches_glob(refname, pattern))
        .collect())
}

/// Normalize a --glob pattern: prepend refs/ if needed, append /* if no glob chars.
fn normalize_glob_pattern(pattern: &str) -> String {
    let full = if pattern.starts_with("refs/") {
        pattern.to_owned()
    } else {
        format!("refs/{pattern}")
    };
    ensure_glob_suffix(&full)
}

/// Normalize a ref-category pattern (for --branches=, --tags=, --remotes=).
/// The `prefix` is e.g. `refs/heads/`, and `pattern` is the user-supplied
/// portion. If the pattern has no glob characters, append `/*` so it matches
/// everything under that prefix path.
fn normalize_ref_pattern(prefix: &str, pattern: &str) -> String {
    let full = format!("{prefix}{pattern}");
    ensure_glob_suffix(&full)
}

/// If the given pattern has no glob characters, treat it as a prefix and
/// append `/*` (or just `*` if it ends with `/`).
fn ensure_glob_suffix(pattern: &str) -> String {
    if pattern.contains('*') || pattern.contains('?') || pattern.contains('[') {
        return pattern.to_owned();
    }
    if pattern.ends_with('/') {
        format!("{pattern}*")
    } else {
        format!("{pattern}/*")
    }
}

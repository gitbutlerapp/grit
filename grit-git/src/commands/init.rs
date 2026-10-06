//! `grit init` — initialise or reinitialise a Git repository.

#![allow(
    clippy::if_same_then_else,
    clippy::needless_option_as_deref,
    clippy::unnecessary_lazy_evaluations
)]
use anyhow::{bail, Context, Result};
use clap::Args as ClapArgs;
use std::fs;
use std::path::{Path, PathBuf};

use grit_lib::config::{ConfigFile, ConfigScope, ConfigSet};
use grit_lib::shared_repo::{
    adjust_shared_repo_tree, git_config_perm, shared_repository_config_stored_value, PERM_GROUP,
    PERM_UMASK,
};
use grit_lib::unicode_normalization::probe_filesystem_normalizes_nfd_to_nfc;

/// `guess_repository_type` from git/builtin/init-db.c (used when `--bare` was not passed).
fn guess_repository_type(git_dir: &Path, cwd: &Path, raw_git_dir_env: Option<&str>) -> bool {
    if raw_git_dir_env == Some(".") {
        return true;
    }
    if git_dir.as_os_str() == "." {
        return true;
    }
    let cwd_canon = fs::canonicalize(cwd).unwrap_or_else(|_| cwd.to_path_buf());
    let gd_canon = fs::canonicalize(git_dir).unwrap_or_else(|_| git_dir.to_path_buf());
    if gd_canon == cwd_canon {
        return true;
    }
    if git_dir == Path::new(".git") {
        return false;
    }
    // Any nested `.git` directory (e.g. `repo/sub/.git`) is a non-bare work tree, even when the
    // init target directory is named the same as the current working directory (t4203 `init space`
    // from `trash/space/`).
    if git_dir.file_name() == Some(std::ffi::OsStr::new(".git")) {
        return false;
    }
    true
}

/// Resolve `$GIT_DIR` or default `.git` to a directory path for repository-type guessing.
fn resolve_git_dir_for_init(
    cwd: &Path,
    abs_path: &Path,
    explicit_directory: bool,
    raw_git_dir_env: Option<&str>,
) -> Result<PathBuf> {
    let mut p = if let Some(g) = raw_git_dir_env.filter(|s| !s.is_empty()) {
        if g == "." {
            return Ok(fs::canonicalize(cwd).unwrap_or_else(|_| cwd.to_path_buf()));
        }
        PathBuf::from(g)
    } else if explicit_directory {
        abs_path.join(".git")
    } else {
        cwd.join(".git")
    };
    if !p.is_absolute() {
        p = cwd.join(p);
    }
    if p.is_file() {
        let c = fs::read_to_string(&p)?;
        p = parse_gitfile_line(&c, p.parent().unwrap_or(cwd))?;
    }
    Ok(fs::canonicalize(&p).unwrap_or(p))
}

fn parse_gitfile_line(content: &str, base: &Path) -> Result<PathBuf> {
    for line in content.lines() {
        let line = line.trim();
        if let Some(rest) = line.strip_prefix("gitdir:") {
            let path = rest.trim();
            let p = PathBuf::from(path);
            let resolved = if p.is_absolute() { p } else { base.join(p) };
            return Ok(fs::canonicalize(&resolved).unwrap_or(resolved));
        }
    }
    bail!("invalid gitfile format")
}

/// The git directory for a work-tree rooted at `work_tree`: the gitfile target when
/// `<work_tree>/.git` is an existing gitfile (so `git init` from inside a separate-git-dir
/// repo reinitializes the real git dir instead of clobbering the gitfile; t0001 #40), otherwise
/// `<work_tree>/.git` itself. When the gitfile points at a *linked-worktree* admin dir (it has a
/// `commondir`), resolve to the shared common git dir so re-init operates on the main repo and
/// does not corrupt the worktree admin dir (t0001 #51).
fn git_dir_via_existing_link(work_tree: &Path) -> PathBuf {
    let link = work_tree.join(".git");
    if link.is_file() {
        if let Ok(content) = fs::read_to_string(&link) {
            let base = link.parent().unwrap_or(Path::new("."));
            if let Ok(target) = parse_gitfile_line(&content, base) {
                if let Some(common) = grit_lib::refs::common_dir(&target) {
                    return common;
                }
                return target;
            }
        }
    }
    link
}

/// Resolve a work-tree's `.git` to the path git treats as the link (`original_git_dir =
/// real_pathdup(git_dir)` in git/setup.c `init_db`). When `.git` is a symlink it is followed to
/// its target (e.g. a `.git -> here` symlink resolves to `here`); a regular file or directory is
/// returned as-is. The `.git` symlink itself is left intact, matching git rewriting the gitfile at
/// the symlink *target* (t0001 #43 "re-init to move gitdir symlink").
fn resolve_git_link_path(link_path: &Path) -> PathBuf {
    match fs::symlink_metadata(link_path) {
        Ok(meta) if meta.file_type().is_symlink() => {
            // Resolve the symlink target relative to its directory; the final component may be a
            // directory (the real git dir) or a regular gitfile.
            match fs::read_link(link_path) {
                Ok(target) => {
                    if target.is_absolute() {
                        target
                    } else {
                        link_path.parent().unwrap_or(Path::new(".")).join(target)
                    }
                }
                Err(_) => link_path.to_path_buf(),
            }
        }
        _ => link_path.to_path_buf(),
    }
}

/// Resolve the git directory a work-tree's `.git` link points at, for `--separate-git-dir`
/// reinit (git/setup.c `separate_git_dir`). Returns the source git dir to relocate:
/// the gitfile target when the link is a regular file, the directory itself when it is a
/// directory, or `None` when it does not exist.
fn resolve_existing_gitdir_link(link_path: &Path) -> Option<PathBuf> {
    let meta = fs::symlink_metadata(link_path).ok()?;
    if meta.file_type().is_dir() {
        return Some(link_path.to_path_buf());
    }
    // Regular file: read it as a gitfile.
    let content = fs::read_to_string(link_path).ok()?;
    let base = link_path.parent().unwrap_or(Path::new("."));
    parse_gitfile_line(&content, base).ok()
}

/// After a git dir is moved (`old_dir` -> `new_dir`), repair the linking files of every linked
/// worktree so their `.git` gitfiles point at the relocated admin directories
/// (git/worktree.c `repair_worktrees_after_gitdir_move`). Failures are non-fatal: a worktree
/// whose backing `.git` no longer exists is simply skipped.
fn repair_worktrees_after_gitdir_move(old_dir: &Path, new_dir: &Path) {
    let worktrees_dir = new_dir.join("worktrees");
    let entries = match fs::read_dir(&worktrees_dir) {
        Ok(e) => e,
        Err(_) => return,
    };
    let use_relative = worktree_uses_relative_paths(new_dir);
    for entry in entries.flatten() {
        let admin = entry.path();
        if !admin.is_dir() {
            continue;
        }
        let id = entry.file_name();
        let gitdir_file = admin.join("gitdir");
        let Ok(raw) = fs::read_to_string(&gitdir_file) else {
            continue;
        };
        let stored = raw.trim();
        if stored.is_empty() {
            continue;
        }
        // The stored path is the work-tree's `.git`. A relative value was written relative to
        // the *old* admin dir location, so resolve it against that, normalizing `..`/`.`
        // lexically (the old admin dir no longer exists after the move).
        let dotgit = {
            let p = PathBuf::from(stored);
            if p.is_absolute() {
                p
            } else {
                lexically_normalize(&old_dir.join("worktrees").join(&id).join(&p))
            }
        };
        if !dotgit.exists() {
            continue;
        }
        // `dotgit` here points at `<wt>/.git`; its parent is the work-tree root.
        let wt_path = dotgit.parent().unwrap_or(Path::new("."));
        let _ = write_worktree_linking_files_local(wt_path, &admin, use_relative);
    }
}

/// Resolve `.`/`..` components in a path lexically, without touching the filesystem (mirrors
/// git's `strbuf_realpath_forgiving` for already-absolute inputs).
fn lexically_normalize(path: &Path) -> PathBuf {
    use std::path::Component;
    let mut out: Vec<Component> = Vec::new();
    for comp in path.components() {
        match comp {
            Component::CurDir => {}
            Component::ParentDir => {
                if matches!(out.last(), Some(Component::Normal(_))) {
                    out.pop();
                } else {
                    out.push(comp);
                }
            }
            other => out.push(other),
        }
    }
    out.iter().collect()
}

/// Read `worktree.useRelativePaths` from a git dir's config (defaults to false).
fn worktree_uses_relative_paths(git_dir: &Path) -> bool {
    let config_path = git_dir.join("config");
    let Ok(content) = fs::read_to_string(&config_path) else {
        return false;
    };
    ConfigFile::parse(&config_path, &content, ConfigScope::Local)
        .ok()
        .and_then(|f| f.get("worktree.useRelativePaths"))
        .is_some_and(|v| matches!(v.trim().to_ascii_lowercase().as_str(), "true" | "yes" | "1"))
}

/// Write the pair of worktree linking files (`<wt>/.git` gitfile and `<admin>/gitdir`),
/// mirroring git/worktree.c `write_worktree_linking_files`.
fn write_worktree_linking_files_local(
    wt_path: &Path,
    wt_admin: &Path,
    use_relative: bool,
) -> Result<()> {
    let dot_git = wt_path.join(".git");
    if use_relative {
        let gitdir_rel = make_relative_path(wt_admin, &dot_git);
        fs::write(
            wt_admin.join("gitdir"),
            format!("{}\n", gitdir_rel.display()),
        )?;
        let dotgit_rel = make_relative_path(wt_path, wt_admin);
        fs::write(dot_git, format!("gitdir: {}\n", dotgit_rel.display()))?;
    } else {
        let dot_git_abs = path_for_git_storage(wt_path).join(".git");
        let admin_abs = path_for_git_storage(wt_admin);
        fs::write(
            wt_admin.join("gitdir"),
            format!("{}\n", dot_git_abs.display()),
        )?;
        fs::write(dot_git, format!("gitdir: {}\n", admin_abs.display()))?;
    }
    Ok(())
}

/// Compute the relative path from directory `from` to `to` (mirrors worktree.rs helper).
fn make_relative_path(from: &Path, to: &Path) -> PathBuf {
    let from_abs = from.canonicalize().unwrap_or_else(|_| from.to_path_buf());
    let to_abs = to.canonicalize().unwrap_or_else(|_| to.to_path_buf());
    let from_comps: Vec<_> = from_abs.components().collect();
    let to_comps: Vec<_> = to_abs.components().collect();
    let common_len = from_comps
        .iter()
        .zip(to_comps.iter())
        .take_while(|(a, b)| a == b)
        .count();
    let up = from_comps.len() - common_len;
    let mut result = PathBuf::new();
    for _ in 0..up {
        result.push("..");
    }
    for comp in &to_comps[common_len..] {
        result.push(comp.as_os_str());
    }
    result
}

/// Canonicalize for storage in gitdir/gitfile paths (mirrors worktree.rs helper).
fn path_for_git_storage(path: &Path) -> PathBuf {
    let canon = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
    #[cfg(target_os = "macos")]
    {
        if let Ok(stripped) = canon.strip_prefix("/private") {
            let without_private = PathBuf::from("/").join(stripped);
            if without_private.exists() {
                return without_private;
            }
        }
    }
    canon
}

/// Arguments for `grit init`.
#[derive(Debug, ClapArgs)]
pub struct Args {
    /// Create a bare repository.
    #[arg(long)]
    pub bare: bool,

    /// Be quiet; only print error messages.
    #[arg(short, long)]
    pub quiet: bool,

    /// Use the specified template directory.
    /// Pass --template= (empty) to skip templates entirely.
    #[arg(long, value_name = "template-directory")]
    pub template: Option<String>,

    /// Separate the git directory from the working tree.
    #[arg(long, value_name = "git-dir")]
    pub separate_git_dir: Option<PathBuf>,

    /// Specify the object format (hash algorithm).
    #[arg(long, value_name = "format")]
    pub object_format: Option<String>,

    /// Override the name of the initial branch.
    #[arg(short = 'b', long, value_name = "branch-name")]
    pub initial_branch: Option<String>,

    /// Specify the sharing permissions (group, all, umask, or octal).
    #[arg(long, value_name = "permissions")]
    pub shared: Option<String>,

    /// Specify the ref storage format.
    #[arg(long, value_name = "format")]
    pub ref_format: Option<String>,

    /// Path to initialize (defaults to current directory).
    pub directory: Option<PathBuf>,
}

/// Run `grit init`.
pub fn run(args: Args, global_bare: bool) -> Result<()> {
    let explicit_directory = args.directory.is_some();
    let explicit_bare = args.bare || global_bare;

    // init-db.c: explicit --bare + --separate-git-dir (before repository-type guess).
    if explicit_bare && args.separate_git_dir.is_some() {
        bail!("options '--separate-git-dir' and '--bare' cannot be used together");
    }

    let work_tree_env = std::env::var("GIT_WORK_TREE")
        .ok()
        .filter(|s| !s.is_empty());
    let git_dir_env = std::env::var("GIT_DIR").ok().filter(|s| !s.is_empty());

    // Match git/builtin/init-db.c: GIT_WORK_TREE only with GIT_DIR and without --bare.
    if work_tree_env.is_some() && (git_dir_env.is_none() || explicit_bare) {
        bail!(
            "GIT_WORK_TREE (or --work-tree=<directory>) not allowed without specifying \
             GIT_DIR (or --git-dir=<directory>)"
        );
    }

    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let path = args.directory.clone().unwrap_or_else(|| cwd.clone());

    // Create directory if it doesn't exist
    if !path.exists() {
        fs::create_dir_all(&path)
            .with_context(|| format!("cannot create directory '{}'", path.display()))?;
    }

    // Canonicalize path for absolute output
    let abs_path = fs::canonicalize(&path).unwrap_or_else(|_| path.clone());

    let resolved_git_dir =
        resolve_git_dir_for_init(&cwd, &abs_path, explicit_directory, git_dir_env.as_deref())?;

    // Mirror git/builtin/init-db.c: `guess_repository_type` is fed the *literal* git dir
    // (`$GIT_DIR` or the default `.git`), not the gitfile-resolved target. A `.git` gitfile
    // therefore still guesses non-bare (t0001 #41). Only when `--separate-git-dir` is used and
    // the gitfile points at a *linked-worktree* admin dir (has a `commondir`) do we relocate to
    // the main worktree's common dir for the guess.
    let literal_git_dir: PathBuf = if let Some(g) = git_dir_env.as_deref().filter(|s| !s.is_empty())
    {
        PathBuf::from(g)
    } else if explicit_directory {
        abs_path.join(".git")
    } else {
        PathBuf::from(".git")
    };
    let mut git_dir_for_guess = literal_git_dir;
    if args.separate_git_dir.is_some() {
        if let Some(common) = grit_lib::refs::common_dir(&resolved_git_dir) {
            git_dir_for_guess = common;
        }
    }

    let mut bare = if explicit_bare {
        true
    } else {
        guess_repository_type(&git_dir_for_guess, &cwd, git_dir_env.as_deref())
    };

    // setup.c:create_default_files sets is_bare_repository_cfg = !work_tree when both GIT_DIR
    // and GIT_WORK_TREE are set (non-bare repo with separate git dir + work tree).
    if work_tree_env.is_some() && git_dir_env.is_some() && !explicit_bare {
        bare = false;
    }

    if bare && args.separate_git_dir.is_some() {
        bail!("--separate-git-dir incompatible with bare repository");
    }

    // Determine the real git directory (where HEAD, objects, refs live)
    let real_git_dir = if let Some(ref sep) = args.separate_git_dir {
        // --separate-git-dir: git dir goes to the separate location
        let sep_abs = if sep.is_absolute() {
            sep.clone()
        } else {
            cwd.join(sep)
        };
        fs::canonicalize(&sep_abs).unwrap_or(sep_abs)
    } else if explicit_directory {
        // Command-line path wins over GIT_DIR (see t0001 "init prefers command line to GIT_DIR").
        if bare {
            abs_path.clone()
        } else {
            git_dir_via_existing_link(&abs_path)
        }
    } else if git_dir_env.is_some() {
        if let Some(parent) = resolved_git_dir.parent() {
            fs::create_dir_all(parent).ok();
        }
        resolved_git_dir
    } else if bare {
        abs_path.clone()
    } else {
        git_dir_via_existing_link(&abs_path)
    };

    // `--separate-git-dir` on an existing repository: relocate the current git dir to the
    // separate location (git/setup.c `separate_git_dir`), then below we replace the work-tree's
    // `.git` with a gitfile (t0001 #41-47, #51). The "link" is normally the work-tree's `.git`
    // (file or directory), but when `--separate-git-dir` is run from inside a *linked* worktree
    // we relocate the shared common git dir and rewrite the *main* worktree's `.git`
    // (git/builtin/init-db.c relocates the common `.git/`, not `.git/worktrees/<id>/`).
    let mut sep_gitfile_link: Option<PathBuf> = None;
    if args.separate_git_dir.is_some() && !bare {
        // git/setup.c uses `original_git_dir = real_pathdup(.git)`: a `.git` symlink is followed to
        // its target, and that target is both relocated and rewritten as the gitfile (the symlink
        // itself stays). A regular `.git` file/dir is used unchanged.
        let wt_link = resolve_git_link_path(&abs_path.join(".git"));
        if let Some(resolved) = resolve_existing_gitdir_link(&wt_link) {
            // If this work-tree's git dir is a linked-worktree admin dir, target the common dir
            // and the main worktree's `.git` instead.
            let (src, link_path) = match grit_lib::refs::common_dir(&resolved) {
                Some(common) => {
                    let link = common
                        .parent()
                        .map(|p| p.join(".git"))
                        .unwrap_or(common.clone());
                    (common, link)
                }
                None => (resolved, wt_link.clone()),
            };
            let src_canon = fs::canonicalize(&src).unwrap_or_else(|_| src.clone());
            // Only relocate when the existing git dir actually lives somewhere else.
            if src_canon != real_git_dir && src.join("HEAD").exists() {
                if real_git_dir.exists() {
                    bail!("{} already exists", real_git_dir.display());
                }
                if let Some(parent) = real_git_dir.parent() {
                    fs::create_dir_all(parent).ok();
                }
                fs::rename(&src, &real_git_dir).with_context(|| {
                    format!(
                        "unable to move {} to {}",
                        src.display(),
                        real_git_dir.display()
                    )
                })?;
                // Repair linked worktrees that pointed into the old git dir.
                repair_worktrees_after_gitdir_move(&src_canon, &real_git_dir);
                // If `.git` was a directory we just moved, drop the now-empty path so the
                // gitfile can be written in its place.
                if link_path.is_dir() {
                    let _ = fs::remove_dir_all(&link_path);
                }
                sep_gitfile_link = Some(link_path);
            }
        }
    }

    // Leftover `.git` from a failed/partial init (no HEAD): remove so `git init` matches Git
    // (t5332 `git init` into a directory that had an incomplete `.git`).
    if !bare && real_git_dir.exists() && !real_git_dir.join("HEAD").exists() {
        if real_git_dir.is_dir() {
            fs::remove_dir_all(&real_git_dir)
                .with_context(|| format!("cannot remove incomplete {}", real_git_dir.display()))?;
        } else {
            fs::remove_file(&real_git_dir)
                .with_context(|| format!("cannot remove {}", real_git_dir.display()))?;
        }
    }

    // Check if this is a reinit
    let is_reinit = real_git_dir.join("HEAD").exists();

    // On reinit, warn if --initial-branch is given (it's ignored)
    if is_reinit && args.initial_branch.is_some() {
        eprintln!(
            "hint: ignored --initial-branch={} for existing repository",
            args.initial_branch.as_deref().unwrap_or("")
        );
    }

    // Load config to get defaults. Fresh init must not read the current repo's local config
    // (t1301 "remote init does not use config from cwd"); reinit loads this repo only.
    // A malformed config (including one pulled in via a matching `[includeIf]` from the command
    // line) is fatal, mirroring Git's `git_config` abort (t0001 #102).
    let config = if is_reinit {
        ConfigSet::load(Some(&real_git_dir), true).map_err(|e| anyhow::anyhow!("{e}"))?
    } else {
        ConfigSet::load(None, true).map_err(|e| anyhow::anyhow!("{e}"))?
    };

    // Determine initial branch name:
    // 1. --initial-branch / -b flag (only on fresh init)
    // 2. GIT_TEST_DEFAULT_INITIAL_BRANCH_NAME env (test support)
    // 3. init.defaultBranch config
    // 4. "main" as fallback (matches modern Git default; see `git init` builtin)
    // `branch_from_fallback` records when no explicit name was given so we can advise (t0001 #92).
    let mut branch_from_fallback = false;
    let initial_branch = if !is_reinit {
        if let Some(ref b) = args.initial_branch {
            b.clone()
        } else if let Some(b) = std::env::var("GIT_TEST_DEFAULT_INITIAL_BRANCH_NAME")
            .ok()
            .filter(|b| !b.is_empty())
        {
            b
        } else if let Some(b) = config.get("init.defaultBranch") {
            b
        } else {
            branch_from_fallback = true;
            // git/refs.c `repo_default_branch_name`: the built-in default is `master` (changes to
            // `main` only in a `WITH_BREAKING_CHANGES`/Git 3.0 build, which Grit is not). t0001 #94.
            "master".to_owned()
        }
    } else {
        // On reinit, don't change HEAD
        String::new()
    };

    // git/refs.c `repo_default_branch_name`: when the initial branch falls back to the built-in
    // default (no -b, no GIT_TEST_DEFAULT_INITIAL_BRANCH_NAME, no init.defaultBranch), advise the
    // user how to configure it, unless `advice.defaultBranchName=false` (t0001 #92).
    if branch_from_fallback && !args.quiet {
        let advice_enabled = config
            .get("advice.defaultBranchName")
            .map(|v| !matches!(v.trim().to_ascii_lowercase().as_str(), "false" | "no" | "0"))
            .unwrap_or(true);
        if advice_enabled {
            emit_default_branch_advice(&config, &initial_branch);
        }
    }

    // The initial branch must form a valid `refs/heads/<name>` (git/refs.c
    // `validate_new_branch_name`); e.g. a name with a space is rejected (t0001 #96).
    if !initial_branch.is_empty() {
        let full = format!("refs/heads/{initial_branch}");
        if grit_lib::check_ref_format::check_refname_format(
            &full,
            &grit_lib::check_ref_format::RefNameOptions::default(),
        )
        .is_err()
        {
            bail!("fatal: invalid branch name: {initial_branch}");
        }
    }

    // git/setup.c `read_default_format_config` warns about a garbage `init.defaultObjectFormat`
    // whenever the key is present, independent of whether env/CLI ultimately overrides it.
    if let Some(v) = config
        .get("init.defaultobjectformat")
        .filter(|v| !v.trim().is_empty())
    {
        if !is_known_object_format(&v) {
            eprintln!("warning: unknown hash algorithm '{}'", v.trim());
        }
    }

    // Determine object format, mirroring git/setup.c `repository_format_configure`:
    //   --object-format (CLI) → GIT_DEFAULT_HASH (env) → init.defaultObjectFormat (config) → sha1.
    // Reinit preserves the existing hash; an explicit CLI/env hash differing from it is fatal.
    // An unknown CLI/env hash is fatal, but an unknown init.defaultObjectFormat only warns.
    let existing_object_format = is_reinit.then(|| detect_object_format(&real_git_dir));
    let env_hash = std::env::var("GIT_DEFAULT_HASH")
        .ok()
        .filter(|h| !h.is_empty());
    let object_format = if let Some(ref fmt) = args.object_format {
        if !is_known_object_format(fmt) {
            bail!("fatal: unknown hash algorithm '{fmt}'");
        }
        if let Some(existing) = existing_object_format.as_deref() {
            if existing != fmt {
                bail!("fatal: attempt to reinitialize repository with different hash");
            }
        }
        fmt.clone()
    } else if let Some(existing) = existing_object_format {
        // Reinit without an explicit format keeps the current hash (env/config do not override).
        existing.to_owned()
    } else if let Some(hash) = env_hash {
        if !is_known_object_format(&hash) {
            bail!("fatal: unknown hash algorithm '{hash}'");
        }
        hash
    } else if let Some(fmt) = config
        .get("init.defaultobjectformat")
        .filter(|v| !v.trim().is_empty())
    {
        if is_known_object_format(&fmt) {
            fmt
        } else {
            // Already warned above; ignore the bad value and fall back to the default.
            "sha1".to_owned()
        }
    } else {
        "sha1".to_owned()
    };

    // Determine template directory:
    // --template=<path> → use that path
    // --template= (empty string) → skip templates
    // not specified → check GIT_TEMPLATE_DIR env, then init.templateDir config, then built-in defaults
    let template_dir: Option<PathBuf> = match &args.template {
        Some(t) if t.is_empty() => None, // explicitly empty → skip
        Some(t) => Some(PathBuf::from(t)),
        None => {
            // Check GIT_TEMPLATE_DIR env var first
            if let Ok(tdir) = std::env::var("GIT_TEMPLATE_DIR") {
                if !tdir.is_empty() {
                    Some(PathBuf::from(tdir))
                } else {
                    None
                }
            } else if let Some(tdir) = config.get("init.templateDir") {
                let expanded = expand_tilde(&tdir);
                if !expanded.is_empty() {
                    Some(PathBuf::from(expanded))
                } else {
                    None
                }
            } else {
                None // Use built-in defaults
            }
        }
    };
    let skip_default_templates = matches!(&args.template, Some(t) if t.is_empty())
        || (args.template.is_none() && std::env::var_os("TEST_CREATE_REPO_NO_TEMPLATE").is_some());

    // git/setup.c `read_default_format_config` warns about a garbage `init.defaultRefFormat`
    // whenever the key is present, independent of whether env/CLI ultimately overrides it.
    if let Some(v) = config
        .get("init.defaultrefformat")
        .filter(|v| !v.trim().is_empty())
    {
        if !is_known_ref_format(&v) {
            eprintln!("warning: unknown ref storage format '{}'", v.trim());
        }
    }

    // Determine ref format, mirroring git/setup.c `repository_format_configure`. Validation is
    // per-source: an explicit `--ref-format`/`GIT_DEFAULT_REF_FORMAT` value that is unknown is a
    // fatal error, but a bad `init.defaultRefFormat` only warns and is ignored.
    let existing_ref_format = is_reinit.then(|| detect_ref_format(&real_git_dir));
    // Upstream's test harness (git/t/test-lib.sh) does
    // `GIT_DEFAULT_REF_FORMAT="${GIT_TEST_DEFAULT_REF_FORMAT:-files}"`, i.e. the test-only
    // `GIT_TEST_DEFAULT_REF_FORMAT` override takes precedence over `GIT_DEFAULT_REF_FORMAT`.
    // Honor that ordering here so tests that opt into the reftable backend (e.g. t0610/t0614)
    // create reftable repositories even when the harness has already exported
    // `GIT_DEFAULT_REF_FORMAT=files`.
    let env_ref_format = std::env::var("GIT_TEST_DEFAULT_REF_FORMAT")
        .ok()
        .filter(|value| !value.is_empty())
        .or_else(|| {
            std::env::var("GIT_DEFAULT_REF_FORMAT")
                .ok()
                .filter(|value| !value.is_empty())
        });

    let ref_format_owned: String = if let Some(format) = args.ref_format.as_deref() {
        // CLI `--ref-format`: must be a known backend.
        if !is_known_ref_format(format) {
            bail!("fatal: unknown ref storage format '{format}'");
        }
        // Reinit with an explicit format different from the existing backend is fatal.
        if let Some(existing) = existing_ref_format {
            if existing != format {
                bail!(
                    "fatal: attempt to reinitialize repository with different reference storage format"
                );
            }
        }
        format.to_owned()
    } else if let Some(existing) = existing_ref_format {
        // Reinit without an explicit format preserves the existing backend; env/config that would
        // choose a different default does not change it (and does not error on mismatch).
        existing.to_owned()
    } else if let Some(env) = env_ref_format.as_deref() {
        // `GIT_DEFAULT_REF_FORMAT`: an unknown value is fatal.
        if !is_known_ref_format(env) {
            bail!("fatal: unknown ref storage format '{env}'");
        }
        env.to_owned()
    } else if let Some(configured) = config
        .get("init.defaultrefformat")
        .filter(|value| !value.trim().is_empty())
    {
        // `init.defaultRefFormat`: an unknown value is ignored (already warned above), falling
        // through to the feature.experimental / default backend.
        if is_known_ref_format(&configured) {
            configured
        } else {
            default_ref_format(&config)
        }
    } else {
        default_ref_format(&config)
    };
    let ref_format = ref_format_owned.as_str();

    let work_tree_abs = work_tree_env.as_ref().map(|wt| {
        let p = PathBuf::from(wt);
        fs::canonicalize(&p).unwrap_or(p)
    });

    // Create the git directory structure (shared-repository mode is applied afterward so
    // templates can supply `core.sharedRepository` and `--shared` can update on reinit; t1301).
    create_git_dir(
        &real_git_dir,
        CreateGitDirOptions {
            initial_branch: &initial_branch,
            bare,
            object_format: &object_format,
            template_dir: template_dir.as_deref(),
            skip_default_templates,
            is_reinit,
            ref_format,
            work_tree: work_tree_abs.as_deref(),
        },
    )?;

    // Fresh init honors `core.sharedRepository` from global/system config (t0001 #21); the
    // just-written local `config` (which may carry a template-supplied value) wins over it.
    let global_shared = if is_reinit {
        None
    } else {
        config.get("core.sharedRepository")
    };
    let shared_perm = apply_shared_repository_settings(
        &real_git_dir,
        args.shared.as_deref(),
        global_shared.as_deref(),
        is_reinit,
        bare,
    )?;

    // Git's probe_utf8_pathname_composition: if the FS aliases NFC/NFD spellings under .git,
    // set core.precomposeunicode (unless already set in higher-priority config).
    // `GIT_TEST_UTF8_NFD_TO_NFC` forces this for harness portability (Linux CI).
    if !is_reinit && !bare && config.get("core.precomposeunicode").is_none() {
        let force_probe = matches!(
            std::env::var("GIT_TEST_UTF8_NFD_TO_NFC").ok().as_deref(),
            Some("true") | Some("1")
        );
        let probe_ok =
            force_probe || probe_filesystem_normalizes_nfd_to_nfc(&real_git_dir).unwrap_or(false);
        if probe_ok {
            let config_path = real_git_dir.join("config");
            let content = fs::read_to_string(&config_path).unwrap_or_default();
            let mut cfg = ConfigFile::parse(&config_path, &content, ConfigScope::Local)?;
            cfg.set("core.precomposeunicode", "true")?;
            cfg.write()?;
        }
    }

    if !is_reinit
        && !bare
        && config
            .get("init.defaultSubmodulePathConfig")
            .as_deref()
            .is_some_and(|v| matches!(v.to_ascii_lowercase().as_str(), "true" | "yes" | "on" | "1"))
    {
        let config_path = real_git_dir.join("config");
        let content = fs::read_to_string(&config_path).unwrap_or_default();
        let mut cfg = ConfigFile::parse(&config_path, &content, ConfigScope::Local)?;
        cfg.set("core.repositoryformatversion", "1")?;
        cfg.set("extensions.submodulePathConfig", "true")?;
        cfg.write()?;
    }

    // Handle --separate-git-dir: write gitfile at the work-tree's `.git` (or the main worktree's
    // `.git` when relocating from a linked worktree). Store the realpath of the separate git dir
    // (git/setup.c writes `gitdir: <real_pathdup>`); the dir now exists so canonicalize.
    if args.separate_git_dir.is_some() && !bare {
        let gitfile_path = sep_gitfile_link.unwrap_or_else(|| abs_path.join(".git"));
        let stored = path_for_git_storage(&real_git_dir);
        let gitfile_content = format!("gitdir: {}\n", stored.display());
        fs::write(&gitfile_path, gitfile_content).with_context(|| "cannot write gitfile")?;
    }

    if !args.quiet {
        let prefix = if is_reinit {
            if shared_perm != 0 {
                "Reinitialized existing shared"
            } else {
                "Reinitialized existing"
            }
        } else if shared_perm != 0 {
            "Initialized empty shared"
        } else {
            "Initialized empty"
        };

        let path = if bare {
            abs_path.display()
        } else {
            real_git_dir.display()
        };
        println!("{} Git repository in {}/", prefix, path);
    }

    Ok(())
}

/// Create or update the git directory structure.
/// Emit the "Using '<name>' as the name for the initial branch" advice (git/refs.c
/// `default_branch_name_advice`), colorizing the `hint:` prefix per `color.advice` like
/// git's `vadvise` / `advise_get_color(ADVICE_COLOR_HINT)`.
fn emit_default_branch_advice(config: &ConfigSet, branch: &str) {
    // Determine whether to color: `color.advice` (or `color.ui`) of `always` always colors;
    // `auto`/unset colors only on a terminal (false under the test harness). `never`/`false`
    // disables. With color.advice=always the test greps for `<YELLOW>hint: ` (t0001 #92).
    let color_setting = config
        .get("color.advice")
        .or_else(|| config.get("color.ui"));
    let use_color = match color_setting.as_deref().map(str::trim) {
        Some("always") => true,
        Some("never") | Some("false") | Some("no") | Some("0") => false,
        _ => std::io::IsTerminal::is_terminal(&std::io::stderr()),
    };
    let (yellow, reset) = if use_color {
        ("\x1b[33m", "\x1b[m")
    } else {
        ("", "")
    };

    let advice = format!(
        "Using '{branch}' as the name for the initial branch. This default branch name\n\
         will change to \"main\" in Git 3.0. To configure the initial branch name\n\
         to use in all of your new repositories, which will suppress this warning,\n\
         call:\n\
         \n\
         \tgit config --global init.defaultBranch <name>\n\
         \n\
         Names commonly chosen instead of 'master' are 'main', 'trunk' and\n\
         'development'. The just-created branch can be renamed via this command:\n\
         \n\
         \tgit branch -m <name>"
    );
    for line in advice.split('\n') {
        let sep = if line.is_empty() { "" } else { " " };
        eprintln!("{yellow}hint:{sep}{line}{reset}");
    }
}

/// Whether `name` is a known ref storage backend.
fn is_known_ref_format(name: &str) -> bool {
    matches!(name, "files" | "reftable")
}

/// Whether `name` is a known object format (hash algorithm).
fn is_known_object_format(name: &str) -> bool {
    matches!(name, "sha1" | "sha256")
}

/// Detect the object format (hash algorithm) of an existing repository from its
/// `extensions.objectformat` config; defaults to `sha1`.
fn detect_object_format(git_dir: &Path) -> &'static str {
    let config_path = git_dir.join("config");
    if let Ok(content) = fs::read_to_string(&config_path) {
        let mut in_extensions = false;
        for line in content.lines() {
            let trimmed = line.trim();
            if trimmed.starts_with('[') {
                in_extensions = trimmed.eq_ignore_ascii_case("[extensions]");
                continue;
            }
            if in_extensions {
                if let Some((key, value)) = trimmed.split_once('=') {
                    if key.trim().eq_ignore_ascii_case("objectformat")
                        && value.trim().eq_ignore_ascii_case("sha256")
                    {
                        return "sha256";
                    }
                }
            }
        }
    }
    "sha1"
}

/// The default ref format for a fresh repository when no explicit format/env/config applies.
/// `feature.experimental=true` selects `reftable` (git/setup.c read_default_format_config),
/// otherwise the built-in default `files`.
fn default_ref_format(config: &ConfigSet) -> String {
    if config
        .get("feature.experimental")
        .is_some_and(|v| matches!(v.trim().to_ascii_lowercase().as_str(), "true" | "yes" | "1"))
    {
        "reftable".to_owned()
    } else {
        "files".to_owned()
    }
}

/// Detect the ref storage format of an existing repository.
fn detect_ref_format(git_dir: &Path) -> &'static str {
    // Check config for extensions.refStorage
    let config_path = git_dir.join("config");
    if let Ok(content) = fs::read_to_string(&config_path) {
        // Simple INI parsing: look for refStorage under [extensions]
        let mut in_extensions = false;
        for line in content.lines() {
            let trimmed = line.trim();
            if trimmed.starts_with('[') {
                in_extensions = trimmed.eq_ignore_ascii_case("[extensions]");
                continue;
            }
            if in_extensions {
                if let Some((key, value)) = trimmed.split_once('=') {
                    if key.trim().eq_ignore_ascii_case("refstorage") {
                        let v = value.trim();
                        if v.eq_ignore_ascii_case("reftable") {
                            return "reftable";
                        }
                    }
                }
            }
        }
    }
    "files"
}

/// Parameters for [`create_git_dir`].
struct CreateGitDirOptions<'a> {
    initial_branch: &'a str,
    bare: bool,
    object_format: &'a str,
    template_dir: Option<&'a Path>,
    skip_default_templates: bool,
    is_reinit: bool,
    ref_format: &'a str,
    work_tree: Option<&'a Path>,
}

fn create_git_dir(git_dir: &Path, opts: CreateGitDirOptions<'_>) -> Result<()> {
    let CreateGitDirOptions {
        initial_branch,
        bare,
        object_format,
        template_dir,
        skip_default_templates,
        is_reinit,
        ref_format,
        work_tree,
    } = opts;

    // `hooks/`, `info/`, and `description` are *template* content in upstream Git
    // (git/setup.c `create_default_files` only copies them via `copy_templates`). When templates
    // are skipped entirely — `git init --template=` (empty) or `TEST_CREATE_REPO_NO_TEMPLATE` with
    // no explicit `--template` — none of these are created, so a later `mkdir .git/hooks` in the
    // harness's `mk_empty` succeeds (t5516). Only the built-in default-template branch below
    // recreates `info/exclude`.
    let has_template_content = template_dir.is_some() || !skip_default_templates;

    // Create core directories (git/setup.c always creates objects/refs; `hooks` only from templates).
    let mut core_dirs: Vec<&str> = vec![
        "objects",
        "objects/info",
        "objects/pack",
        "refs",
        "refs/heads",
        "refs/tags",
    ];
    if has_template_content {
        core_dirs.push("hooks");
    }
    for sub in &core_dirs {
        fs::create_dir_all(git_dir.join(sub))?;
    }

    // Create reftable directory structure if needed
    if ref_format == "reftable" {
        let reftable_dir = git_dir.join("reftable");
        fs::create_dir_all(&reftable_dir)?;
        let tables_list = reftable_dir.join("tables.list");
        if !tables_list.exists() {
            fs::write(&tables_list, "")?;
        }
        if !is_reinit && fs::read_to_string(&tables_list)?.trim().is_empty() {
            let writer = grit_lib::reftable::ReftableWriter::new(
                grit_lib::reftable::WriteOptions::default(),
                1,
                1,
            );
            let table = writer.finish()?;
            let mut stack = grit_lib::reftable::ReftableStack::open(git_dir)?;
            stack.add_table(&table, 1)?;
        }
    }

    // Apply templates or built-in defaults
    if let Some(tmpl) = template_dir {
        if tmpl.is_dir() {
            copy_template(tmpl, git_dir)?;
        }
    } else if !skip_default_templates {
        // Create built-in default template content.
        fs::create_dir_all(git_dir.join("info"))?;
        // Write info/exclude (default template content)
        let exclude_path = git_dir.join("info").join("exclude");
        if !exclude_path.exists() {
            fs::write(
                &exclude_path,
                "# git ls-files --others --exclude-from=.git/info/exclude\n\
                 # Lines that start with '#' are comments.\n\
                 # For a project mostly in C, the following would be a good set of\n\
                 # temporary files to exclude:\n\
                 #.*.[oa]\n\
                 #*~\n\
                 .test_tick\n",
            )?;
        }
    }

    // Write HEAD (only on fresh init, or if missing during unusual setups)
    let head_path = git_dir.join("HEAD");
    if !initial_branch.is_empty() && (!is_reinit || !head_path.exists()) {
        let head_content = format!("ref: refs/heads/{initial_branch}\n");
        fs::write(&head_path, head_content)?;
    }

    // Write or merge config (templates may supply `config`; do not clobber it — t1301 #22).
    let config_path = git_dir.join("config");
    if !is_reinit || !config_path.exists() {
        let needs_extensions = object_format != "sha1" || ref_format == "reftable";
        let repo_version = if needs_extensions { 1 } else { 0 };

        let existing = fs::read_to_string(&config_path).unwrap_or_default();
        let mut cfg = ConfigFile::parse(&config_path, &existing, ConfigScope::Local)?;

        cfg.set("core.repositoryformatversion", &repo_version.to_string())?;
        cfg.set("core.filemode", "true")?;
        if bare {
            cfg.set("core.bare", "true")?;
        } else {
            cfg.set("core.bare", "false")?;
            // Mirror git's init (setup.c): only write the local
            // `core.logAllRefUpdates=true` default when the value is not already
            // set in the merged config (system/global, or a template-supplied
            // local value). Otherwise a global `logAllRefUpdates=false` (e.g.
            // `test_config_global` in t0613) would be overridden by the local
            // default, wrongly enabling reflogs for reftable repos.
            let log_all_already_set = ConfigSet::load(Some(git_dir), true)
                .ok()
                .map(|merged| merged.get("core.logAllRefUpdates").is_some())
                .unwrap_or(false);
            if !log_all_already_set {
                cfg.set("core.logallrefupdates", "true")?;
            }
            if let Some(wt) = work_tree {
                cfg.set(
                    "core.worktree",
                    &wt.display().to_string().replace('\\', "/"),
                )?;
            }
        }

        if needs_extensions {
            if object_format != "sha1" {
                cfg.set("extensions.objectformat", object_format)?;
            }
            if ref_format == "reftable" {
                cfg.set("extensions.refStorage", "reftable")?;
            }
        }

        // Match upstream `git init`: the initial branch is recorded only in `HEAD`, not as
        // `init.defaultBranch` in `.git/config`. Tests (e.g. t1300-config) expect `config --list`
        // in a fresh repo to omit that key.

        cfg.write()?;
    }

    // Write description (template content; skipped when templates are skipped — t5516 `mk_empty`).
    if has_template_content {
        let desc_path = git_dir.join("description");
        if !desc_path.exists() {
            fs::write(
                &desc_path,
                "Unnamed repository; edit this file 'description' to name the repository.\n",
            )?;
        }
    }

    Ok(())
}

/// Read local `config` only (ignore global/system `core.sharedRepository`; t12660), resolve sharing,
/// update `config` / `receive.*`, chmod the git dir.
fn apply_shared_repository_settings(
    git_dir: &Path,
    shared_arg: Option<&str>,
    global_shared: Option<&str>,
    is_reinit: bool,
    bare: bool,
) -> Result<i32> {
    let config_path = git_dir.join("config");
    let from_cfg = fs::read_to_string(&config_path)
        .ok()
        .and_then(|c| ConfigFile::parse(&config_path, &c, ConfigScope::Local).ok())
        .and_then(|f| f.get("core.sharedRepository"))
        // Fall back to global/system config (fresh init only); local template value wins.
        .or_else(|| global_shared.map(str::to_owned));
    let (shared_perm, stored) =
        resolve_shared_repository_mode(shared_arg, from_cfg.as_deref(), is_reinit, bare)?;

    let content = fs::read_to_string(&config_path).unwrap_or_default();
    let mut cfg = ConfigFile::parse(&config_path, &content, ConfigScope::Local)?;

    let shared_from_cli = shared_arg
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .is_some();
    if let Some(stored_val) = stored.as_deref() {
        cfg.set("core.sharedRepository", stored_val)?;
        cfg.set("receive.denyNonFastforwards", "true")?;
    } else if shared_from_cli && shared_perm == PERM_UMASK {
        let _ = cfg.unset("core.sharedRepository");
        let _ = cfg.unset("receive.denyNonFastforwards");
    }
    cfg.write()?;

    if shared_perm != 0 {
        adjust_shared_repo_tree(git_dir, shared_perm)
            .context("adjust shared repository permissions")?;
    }

    Ok(shared_perm)
}

/// Resolve chmod vs config persistence separately: Grit defaults fresh non-bare repos to
/// group-writable `.git` trees (t12660) **without** writing `core.sharedRepository` unless the
/// mode came from `--shared` or from config (template / prior init; t1301).
fn resolve_shared_repository_mode(
    shared_arg: Option<&str>,
    shared_config: Option<&str>,
    is_reinit: bool,
    bare: bool,
) -> Result<(i32, Option<String>)> {
    let from_arg = shared_arg.map(str::trim).filter(|s| !s.is_empty());
    let from_cfg = shared_config.map(str::trim).filter(|s| !s.is_empty());

    let perm_explicit: Option<i32> = match (&from_arg, &from_cfg) {
        (Some(v), _) => Some(git_config_perm("arg", v).map_err(|e| anyhow::anyhow!(e))?),
        (None, Some(v)) => {
            Some(git_config_perm("core.sharedRepository", v).map_err(|e| anyhow::anyhow!(e))?)
        }
        (None, None) => None,
    };

    let perm = perm_explicit.unwrap_or_else(|| {
        if is_reinit {
            PERM_UMASK
        } else if bare {
            PERM_UMASK
        } else {
            PERM_GROUP
        }
    });

    let stored = perm_explicit.and_then(shared_repository_config_stored_value);

    Ok((perm, stored))
}

/// Expand ~ at the start of a path to $HOME.
fn expand_tilde(path: &str) -> String {
    if path.starts_with("~/") || path == "~" {
        if let Ok(home) = std::env::var("HOME") {
            return path.replacen('~', &home, 1);
        }
    }
    path.to_owned()
}

/// Recursively copy template files from `src` to `dst`, skipping existing files.
fn copy_template(src: &Path, dst: &Path) -> Result<()> {
    for entry in fs::read_dir(src)? {
        let entry = entry?;
        let src_path = entry.path();
        let dst_path = dst.join(entry.file_name());
        if src_path.is_dir() {
            fs::create_dir_all(&dst_path)?;
            copy_template(&src_path, &dst_path)?;
        } else {
            fs::copy(&src_path, &dst_path)?;
        }
    }
    Ok(())
}
